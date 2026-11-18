// Bugle macOS shim.
//
// Thin Objective-C layer over CoreGraphics, ScreenCaptureKit, Vision, AppKit
// and AVFoundation. Everything crosses the boundary as plain C structs and
// malloc'd buffers; Rust owns the policy, this file owns the frameworks.

#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Vision/Vision.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <AVFoundation/AVFoundation.h>
#import <ApplicationServices/ApplicationServices.h>
#include "shim.h"
#include <sys/wait.h>

// ---------------------------------------------------------------------------
// Errors / memory
// ---------------------------------------------------------------------------

// The last error is per thread: the engine calls into the shim from many
// threads at once, and each caller reads the error on the thread that made the
// failing call. A plain malloc'd C string (no ARC object) so nothing is shared.
static _Thread_local char *t_last_error = NULL;

static void clear_error(void) {
    free(t_last_error);
    t_last_error = NULL;
}

static BOOL has_error(void) { return t_last_error != NULL; }

static void set_error(NSString *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
    const char *u = s.UTF8String;
    free(t_last_error);
    t_last_error = strdup(u ? u : "unknown error");
}

/// Hands the calling thread's last error to the caller (free with
/// `bugle_free`) and clears it. NULL when there is none.
char *bugle_last_error(void) {
    char *e = t_last_error;
    t_last_error = NULL;
    return e;
}

void bugle_free(void *p) { free(p); }

void bugle_image_free(bugle_image_t *img) {
    if (img && img->data) {
        free(img->data);
        img->data = NULL;
        img->width = img->height = 0;
    }
}

// ---------------------------------------------------------------------------
// Image conversion
// ---------------------------------------------------------------------------

// Largest image side accepted from or handed to the frameworks, in pixels.
#define BUGLE_MAX_PX 16384

/// Byte length of a w x h RGBA8 image, or 0 when the size is not positive or
/// does not fit (each side must fit in int32_t; the product must fit size_t).
static size_t rgba_len(int64_t w, int64_t h) {
    if (w <= 0 || h <= 0 || w > INT32_MAX || h > INT32_MAX) return 0;
    if ((uint64_t)w > SIZE_MAX / 4 / (uint64_t)h) return 0;
    return (size_t)w * (size_t)h * 4;
}

static bugle_image_t cg_to_rgba(CGImageRef img) {
    bugle_image_t out = {0};
    if (!img) return out;
    size_t w = CGImageGetWidth(img), h = CGImageGetHeight(img);
    if (w > INT32_MAX || h > INT32_MAX || rgba_len((int64_t)w, (int64_t)h) == 0) {
        set_error(@"image of %zux%zu pixels is too large", w, h);
        return out;
    }
    uint8_t *buf = calloc(w * h * 4, 1);
    if (!buf) { set_error(@"out of memory for a %zux%zu image", w, h); return out; }
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef ctx = CGBitmapContextCreate(buf, w, h, 8, w * 4, cs,
                                             kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
    CGColorSpaceRelease(cs);
    if (!ctx) { free(buf); set_error(@"could not create a %zux%zu bitmap context", w, h); return out; }
    CGContextSetBlendMode(ctx, kCGBlendModeCopy);
    CGContextDrawImage(ctx, CGRectMake(0, 0, w, h), img);
    CGContextRelease(ctx);
    // Un-premultiply so Rust sees straight alpha (window shadows are translucent).
    for (size_t i = 0; i < w * h; i++) {
        uint8_t a = buf[i * 4 + 3];
        if (a != 0 && a != 255) {
            for (int k = 0; k < 3; k++) {
                unsigned v = (buf[i * 4 + k] * 255u + a / 2) / a;
                buf[i * 4 + k] = (uint8_t)(v > 255 ? 255 : v);
            }
        }
    }
    out.data = buf;
    out.width = (int32_t)w;
    out.height = (int32_t)h;
    return out;
}

/// Wraps a caller-supplied RGBA8 buffer. NULL when the pointer is NULL, the
/// size is not positive, or `len` is shorter than `w * h * 4`.
static CGImageRef cg_from_rgba(const uint8_t *rgba, size_t len, int32_t w, int32_t h) {
    size_t need = rgba_len(w, h);
    if (!rgba || need == 0 || len < need || need > (size_t)LONG_MAX) return NULL;
    CFDataRef d = CFDataCreate(NULL, rgba, (CFIndex)need);
    if (!d) return NULL;
    CGDataProviderRef p = CGDataProviderCreateWithCFData(d);
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGImageRef img = CGImageCreate(w, h, 8, 32, (size_t)w * 4, cs,
                                   kCGImageAlphaLast | kCGBitmapByteOrder32Big, p, NULL, false,
                                   kCGRenderingIntentDefault);
    CGColorSpaceRelease(cs);
    CGDataProviderRelease(p);
    CFRelease(d);
    return img;
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

int32_t bugle_screen_access_preflight(void) { return CGPreflightScreenCaptureAccess() ? 1 : 0; }
int32_t bugle_screen_access_request(void) { return CGRequestScreenCaptureAccess() ? 1 : 0; }

// Called from arbitrary threads. AXIsProcessTrustedWithOptions is a plain
// C call into the accessibility server and needs no main-thread hop.
int32_t bugle_accessibility_trusted(int32_t prompt) {
    NSDictionary *opts = @{(__bridge NSString *)kAXTrustedCheckOptionPrompt : @(prompt != 0)};
    return AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)opts) ? 1 : 0;
}

// ---------------------------------------------------------------------------
// Displays & windows
// ---------------------------------------------------------------------------

int32_t bugle_list_displays(bugle_display_t *out, int32_t max) {
    CGDirectDisplayID ids[32];
    uint32_t n = 0;
    if (CGGetActiveDisplayList(32, ids, &n) != kCGErrorSuccess) return -1;
    int32_t count = 0;
    for (uint32_t i = 0; i < n && count < max; i++) {
        CGRect b = CGDisplayBounds(ids[i]);
        size_t px = CGDisplayPixelsWide(ids[i]);
        bugle_display_t d;
        d.id = ids[i];
        d.x = b.origin.x; d.y = b.origin.y; d.w = b.size.width; d.h = b.size.height;
        d.scale = b.size.width > 0 ? (double)px / b.size.width : 1.0;
        d.is_main = CGDisplayIsMain(ids[i]) ? 1 : 0;
        out[count++] = d;
    }
    return count;
}

/// Copies `s` as NUL-terminated UTF-8, truncating at a character boundary so a
/// multi-byte sequence is never cut in half.
static void copy_str(char *dst, size_t cap, NSString *s) {
    if (cap == 0) return;
    const char *src = [s isKindOfClass:[NSString class]] ? s.UTF8String : NULL;
    if (!src) { dst[0] = 0; return; }
    size_t n = strlen(src);
    if (n > cap - 1) {
        n = cap - 1;
        // Back off to the start of the sequence that straddles the limit.
        while (n > 0 && ((unsigned char)src[n] & 0xC0) == 0x80) n--;
    }
    memcpy(dst, src, n);
    dst[n] = 0;
}

/// Writes up to `max` windows to `out` and returns the total number of
/// windows found (which may exceed `max`; the caller grows and retries).
/// `out` may be NULL when `max` is 0.
int32_t bugle_list_windows(bugle_window_t *out, int32_t max, int32_t on_screen_only) {
    CGWindowListOption opt = kCGWindowListExcludeDesktopElements;
    if (on_screen_only) opt |= kCGWindowListOptionOnScreenOnly;
    CFArrayRef list = CGWindowListCopyWindowInfo(opt, kCGNullWindowID);
    if (!list) return -1;
    int32_t count = 0;
    NSArray *arr = CFBridgingRelease(list);
    if (!out || max < 0) max = 0;
    for (NSDictionary *w in arr) {
        if (count == INT32_MAX) break;
        if (count >= max) { count++; continue; }
        bugle_window_t win;
        memset(&win, 0, sizeof win);
        win.id = [w[(__bridge NSString *)kCGWindowNumber] unsignedIntValue];
        win.pid = [w[(__bridge NSString *)kCGWindowOwnerPID] intValue];
        win.layer = [w[(__bridge NSString *)kCGWindowLayer] intValue];
        win.on_screen = [w[(__bridge NSString *)kCGWindowIsOnscreen] boolValue] ? 1 : 0;
        NSNumber *alpha = w[(__bridge NSString *)kCGWindowAlpha];
        win.alpha = alpha ? alpha.doubleValue : 1.0;
        CGRect b = CGRectZero;
        CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)w[(__bridge NSString *)kCGWindowBounds], &b);
        win.x = b.origin.x; win.y = b.origin.y; win.w = b.size.width; win.h = b.size.height;
        copy_str(win.owner, sizeof win.owner, w[(__bridge NSString *)kCGWindowOwnerName]);
        copy_str(win.title, sizeof win.title, w[(__bridge NSString *)kCGWindowName]);
        out[count++] = win;
    }
    return count;
}

// ---------------------------------------------------------------------------
// Capture — ScreenCaptureKit first, CoreGraphics fallback
// ---------------------------------------------------------------------------

// `@available` would pull `___isPlatformVersionAtLeast` from compiler-rt, which
// Rust's linker invocation does not provide. A class lookup is enough: the
// ScreenCaptureKit classes are weak-linked because the deployment target is
// older than their introduction.
static BOOL sck_available(void) {
    static int cached = -1;
    if (cached < 0) cached = (NSClassFromString(@"SCScreenshotManager") != nil) ? 1 : 0;
    return cached == 1;
}

static SCShareableContent *sck_content(void) {
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block SCShareableContent *content = nil;
    __block NSError *err = nil;
    // On timeout the handler may still run later; its writes go to the
    // heap-held __block storage and are released by ARC with it, so a late
    // delivery leaks nothing and nobody reads it.
    [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                                              onScreenWindowsOnly:YES
                                                completionHandler:^(SCShareableContent *c, NSError *e) {
        content = c;
        err = e;
        dispatch_semaphore_signal(sem);
    }];
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) != 0) {
        set_error(@"ScreenCaptureKit: timed out enumerating shareable content");
        return nil;
    }
    if (err) set_error(@"ScreenCaptureKit: %@", err.localizedDescription);
    return content;
}

static CGImageRef sck_screenshot(SCContentFilter *filter, CGSize pixelSize, BOOL hasSource, CGRect sourceRect,
                                 BOOL showCursor, BOOL ignoreShadow) {
    if (!isfinite(pixelSize.width) || !isfinite(pixelSize.height) || pixelSize.width < 0.5 ||
        pixelSize.height < 0.5 || pixelSize.width > BUGLE_MAX_PX || pixelSize.height > BUGLE_MAX_PX) {
        set_error(@"capture size %.0fx%.0f px is outside 1...%d", pixelSize.width, pixelSize.height, BUGLE_MAX_PX);
        return NULL;
    }
    SCStreamConfiguration *cfg = [[SCStreamConfiguration alloc] init];
    cfg.width = (size_t)llround(pixelSize.width);
    cfg.height = (size_t)llround(pixelSize.height);
    cfg.showsCursor = showCursor;
    cfg.pixelFormat = kCVPixelFormatType_32BGRA;
    cfg.captureResolution = SCCaptureResolutionBest;
    cfg.ignoreShadowsSingleWindow = ignoreShadow;
    cfg.ignoreShadowsDisplay = YES;
    cfg.shouldBeOpaque = NO;
    if (hasSource) cfg.sourceRect = sourceRect;
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    // The completion handler can fire after we gave up waiting. `abandoned`
    // (guarded by `guard`) tells it not to retain an image nobody will release.
    NSObject *guard = [[NSObject alloc] init];
    __block BOOL abandoned = NO;
    __block CGImageRef out = NULL;
    __block NSError *err = nil;
    [SCScreenshotManager captureImageWithFilter:filter
                                  configuration:cfg
                              completionHandler:^(CGImageRef img, NSError *e) {
        @synchronized (guard) {
            if (!abandoned) {
                if (img) out = CGImageRetain(img);
                err = e;
            }
        }
        dispatch_semaphore_signal(sem);
    }];
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) != 0) {
        CGImageRef late = NULL;
        @synchronized (guard) {
            abandoned = YES;
            late = out;  // non-NULL only if the handler ran just before this
            out = NULL;
        }
        if (late) return late;
        set_error(@"ScreenCaptureKit: screenshot timed out");
        return NULL;
    }
    if (err) set_error(@"ScreenCaptureKit: %@", err.localizedDescription);
    return out;
}

static CGDirectDisplayID display_for_rect(CGRect r) {
    CGDirectDisplayID ids[32];
    uint32_t n = 0;
    CGGetActiveDisplayList(32, ids, &n);
    CGPoint c = CGPointMake(CGRectGetMidX(r), CGRectGetMidY(r));
    CGDirectDisplayID best = CGMainDisplayID();
    double bestArea = -1;
    for (uint32_t i = 0; i < n; i++) {
        CGRect b = CGDisplayBounds(ids[i]);
        if (CGRectContainsPoint(b, c)) return ids[i];
        CGRect ix = CGRectIntersection(b, r);
        double area = CGRectIsNull(ix) ? 0 : ix.size.width * ix.size.height;
        if (area > bestArea) { bestArea = area; best = ids[i]; }
    }
    return best;
}

bugle_image_t bugle_capture_display(uint32_t display_id, int32_t show_cursor) {
    @autoreleasepool {
        clear_error();
        if (display_id == 0) display_id = CGMainDisplayID();
        CGImageRef img = NULL;
        if (sck_available()) {
            SCShareableContent *content = sck_content();
            for (SCDisplay *d in content.displays) {
                if (d.displayID == display_id) {
                    SCContentFilter *f = [[SCContentFilter alloc] initWithDisplay:d excludingWindows:@[]];
                    CGSize px = CGSizeMake(d.width * f.pointPixelScale, d.height * f.pointPixelScale);
                    img = sck_screenshot(f, px, NO, CGRectZero, show_cursor != 0, YES);
                    break;
                }
            }
        }
        if (!img) img = CGDisplayCreateImage(display_id);
        if (!img) { if (!has_error()) set_error(@"display %u could not be captured", display_id); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

bugle_image_t bugle_capture_area(double x, double y, double w, double h, int32_t show_cursor) {
    @autoreleasepool {
        clear_error();
        // Reject NaN/infinite or absurd rects before they reach llround() and
        // SCStreamConfiguration (llround(NaN) is undefined).
        if (!isfinite(x) || !isfinite(y) || !isfinite(w) || !isfinite(h) || w <= 0 || h <= 0 ||
            w > BUGLE_MAX_PX || h > BUGLE_MAX_PX) {
            set_error(@"invalid capture area %g,%g %gx%g (sides must be finite and in (0, %d] points)", x, y, w, h,
                      BUGLE_MAX_PX);
            return (bugle_image_t){0};
        }
        CGRect rect = CGRectMake(x, y, w, h);
        CGImageRef img = NULL;
        CGDirectDisplayID did = display_for_rect(rect);
        CGRect db = CGDisplayBounds(did);
        if (sck_available()) {
            SCShareableContent *content = sck_content();
            for (SCDisplay *d in content.displays) {
                if (d.displayID == did) {
                    SCContentFilter *f = [[SCContentFilter alloc] initWithDisplay:d excludingWindows:@[]];
                    double s = f.pointPixelScale;
                    CGRect local = CGRectMake(x - db.origin.x, y - db.origin.y, w, h);
                    img = sck_screenshot(f, CGSizeMake(w * s, h * s), YES, local, show_cursor != 0, YES);
                    break;
                }
            }
        }
        if (!img) {
            img = CGWindowListCreateImage(rect, kCGWindowListOptionOnScreenOnly, kCGNullWindowID,
                                          kCGWindowImageBestResolution);
        }
        if (!img) { if (!has_error()) set_error(@"area could not be captured"); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

bugle_image_t bugle_capture_window(uint32_t window_id, int32_t shadow, int32_t show_cursor) {
    @autoreleasepool {
        clear_error();
        CGImageRef img = NULL;
        if (shadow) {
            // ScreenCaptureKit clips single-window captures to the window
            // bounds; the classic CG path is the one that keeps the shadow.
            img = CGWindowListCreateImage(CGRectNull, kCGWindowListOptionIncludingWindow, window_id,
                                          kCGWindowImageBestResolution);
        }
        if (!img) {
            if (sck_available()) {
                SCShareableContent *content = sck_content();
                for (SCWindow *win in content.windows) {
                    if (win.windowID == window_id) {
                        SCContentFilter *f = [[SCContentFilter alloc] initWithDesktopIndependentWindow:win];
                        double s = f.pointPixelScale;
                        CGRect cr = f.contentRect;
                        img = sck_screenshot(f, CGSizeMake(cr.size.width * s, cr.size.height * s), NO, CGRectZero,
                                             show_cursor != 0, shadow == 0);
                        break;
                    }
                }
            }
        }
        if (!img) {
            CGWindowImageOption opt = kCGWindowImageBestResolution;
            if (!shadow) opt |= kCGWindowImageBoundsIgnoreFraming;
            img = CGWindowListCreateImage(CGRectNull, kCGWindowListOptionIncludingWindow, window_id, opt);
        }
        if (!img) { if (!has_error()) set_error(@"window %u could not be captured", window_id); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

// ---------------------------------------------------------------------------
// OCR — VNRecognizeTextRequest (accurate,
// language correction, configurable languages).
// ---------------------------------------------------------------------------

int32_t bugle_ocr(const uint8_t *rgba, size_t len, int32_t w, int32_t h, const char *langs_csv, int32_t accurate,
                  int32_t lang_correction, bugle_ocr_line_t **out_lines, int32_t *out_count) {
    @autoreleasepool {
        clear_error();
        if (!out_lines || !out_count) { set_error(@"OCR: NULL output pointer"); return -1; }
        *out_lines = NULL;
        *out_count = 0;
        CGImageRef img = cg_from_rgba(rgba, len, w, h);
        if (!img) { set_error(@"OCR: invalid image (%dx%d, %zu bytes)", w, h, len); return -1; }
        VNRecognizeTextRequest *req = [[VNRecognizeTextRequest alloc] init];
        req.recognitionLevel = accurate ? VNRequestTextRecognitionLevelAccurate : VNRequestTextRecognitionLevelFast;
        req.usesLanguageCorrection = lang_correction != 0;
        if (langs_csv && strlen(langs_csv) > 0) {
            NSArray *langs = [[NSString stringWithUTF8String:langs_csv] componentsSeparatedByString:@","];
            req.recognitionLanguages = langs;
        } else if ([req respondsToSelector:@selector(setAutomaticallyDetectsLanguage:)]) {
            req.automaticallyDetectsLanguage = YES;
        }
        VNImageRequestHandler *handler = [[VNImageRequestHandler alloc] initWithCGImage:img options:@{}];
        NSError *err = nil;
        BOOL ok = [handler performRequests:@[req] error:&err];
        CGImageRelease(img);
        if (!ok) { set_error(@"OCR: %@", err.localizedDescription); return -1; }
        NSArray *obs = req.results;
        if (obs.count >= INT32_MAX) { set_error(@"OCR: too many results"); return -1; }
        bugle_ocr_line_t *lines = calloc(obs.count + 1, sizeof(bugle_ocr_line_t));
        if (!lines) { set_error(@"OCR: out of memory"); return -1; }
        int32_t n = 0;
        for (VNRecognizedTextObservation *o in obs) {
            VNRecognizedText *top = [o topCandidates:1].firstObject;
            if (!top) continue;
            CGRect bb = o.boundingBox;  // normalized, bottom-left origin
            const char *utf8 = top.string.UTF8String;
            char *text = strdup(utf8 ? utf8 : "");
            if (!text) {
                bugle_ocr_free(lines, n);
                set_error(@"OCR: out of memory");
                return -1;
            }
            lines[n].text = text;
            lines[n].x = bb.origin.x * w;
            lines[n].y = (1.0 - bb.origin.y - bb.size.height) * h;
            lines[n].w = bb.size.width * w;
            lines[n].h = bb.size.height * h;
            lines[n].confidence = top.confidence;
            n++;
        }
        *out_lines = lines;
        *out_count = n;
        return 0;
    }
}

void bugle_ocr_free(bugle_ocr_line_t *lines, int32_t count) {
    if (!lines) return;
    for (int32_t i = 0; i < count; i++) free(lines[i].text);
    free(lines);
}

// ---------------------------------------------------------------------------
// Frame registration for scrolling capture
// (VNTranslationalImageRegistrationRequest driven by a sequence handler).
// Returns the translation that maps the floating image onto the reference.
// ---------------------------------------------------------------------------

int32_t bugle_register_translation(const uint8_t *ref_rgba, size_t ref_len, int32_t rw, int32_t rh,
                                   const uint8_t *flt_rgba, size_t flt_len, int32_t fw, int32_t fh, double *dx,
                                   double *dy) {
    @autoreleasepool {
        clear_error();
        if (!dx || !dy) { set_error(@"registration: NULL output pointer"); return -1; }
        CGImageRef ref = cg_from_rgba(ref_rgba, ref_len, rw, rh);
        CGImageRef flt = cg_from_rgba(flt_rgba, flt_len, fw, fh);
        if (!ref || !flt) {
            if (ref) CGImageRelease(ref);
            if (flt) CGImageRelease(flt);
            set_error(@"registration: invalid image");
            return -1;
        }
        VNTranslationalImageRegistrationRequest *req =
            [[VNTranslationalImageRegistrationRequest alloc] initWithTargetedCGImage:flt options:@{}];
        VNSequenceRequestHandler *seq = [[VNSequenceRequestHandler alloc] init];
        NSError *err = nil;
        BOOL ok = [seq performRequests:@[req] onCGImage:ref error:&err];
        CGImageRelease(ref);
        CGImageRelease(flt);
        if (!ok) { set_error(@"registration: %@", err.localizedDescription); return -1; }
        VNImageTranslationAlignmentObservation *o = req.results.firstObject;
        if (!o) { set_error(@"registration: no observation"); return -1; }
        CGAffineTransform t = o.alignmentTransform;
        *dx = t.tx;
        *dy = t.ty;
        return 0;
    }
}

// ---------------------------------------------------------------------------
// Input & clipboard
// ---------------------------------------------------------------------------

int32_t bugle_post_scroll(double x, double y, int32_t dx_lines, int32_t dy_lines) {
    CGEventRef ev = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitLine, 2, dy_lines, dx_lines);
    if (!ev) return -1;
    CGEventSetLocation(ev, CGPointMake(x, y));
    CGEventPost(kCGHIDEventTap, ev);
    CFRelease(ev);
    return 0;
}

void bugle_cursor_position(double *x, double *y) {
    CGEventRef ev = CGEventCreate(NULL);
    CGPoint p = CGEventGetLocation(ev);
    if (ev) CFRelease(ev);
    *x = p.x;
    *y = p.y;
}

int32_t bugle_mouse_buttons(void) {
    // Reads the session's button state; needs no permission and posts nothing.
    int32_t m = 0;
    if (CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) m |= 1;
    if (CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonRight)) m |= 2;
    if (CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonCenter)) m |= 4;
    return m;
}

double bugle_host_time_now(void) { return CMTimeGetSeconds(CMClockGetTime(CMClockGetHostTimeClock())); }

int32_t bugle_window_frame(uint32_t window_id, double *x, double *y, double *w, double *h) {
    @autoreleasepool {
        CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window_id);
        if (!list) return -1;
        int32_t rc = -1;
        if (CFArrayGetCount(list) > 0) {
            NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(list, 0);
            CGRect r;
            if ([info[(id)kCGWindowNumber] unsignedIntValue] == window_id &&
                CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)info[(id)kCGWindowBounds], &r)) {
                *x = r.origin.x; *y = r.origin.y; *w = r.size.width; *h = r.size.height;
                rc = 0;
            }
        }
        CFRelease(list);
        return rc;
    }
}

int32_t bugle_child_exited(int32_t pid) {
    // WNOWAIT leaves the child to be reaped by its owner.
    siginfo_t info;
    memset(&info, 0, sizeof info);
    if (waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG | WNOWAIT) != 0) return -1;
    return info.si_pid == pid ? 1 : 0;
}

// Clipboard, sound and workspace calls below run on whichever thread the
// engine uses (often a blocking-pool worker). NSPasteboard, NSSound playback
// and -[NSWorkspace openURL:] are safe off the main thread in practice, and a
// hop to the main queue would deadlock hosts whose main thread is not running
// a run loop (the CLI), so they are deliberately called in place.

int32_t bugle_clipboard_write_image(const uint8_t *png, size_t len) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        [pb clearContents];
        NSData *d = [NSData dataWithBytes:png length:len];
        return [pb setData:d forType:NSPasteboardTypePNG] ? 0 : -1;
    }
}

int32_t bugle_clipboard_write_text(const char *text) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        [pb clearContents];
        return [pb setString:[NSString stringWithUTF8String:text] forType:NSPasteboardTypeString] ? 0 : -1;
    }
}

int32_t bugle_clipboard_write_file(const char *path) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        [pb clearContents];
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        return [pb writeObjects:@[url]] ? 0 : -1;
    }
}

// ---------------------------------------------------------------------------
// Interactive area selection overlay (crosshair over a dimmed screen)
// ---------------------------------------------------------------------------
//
// Two entry points share one implementation. `bugle_select_rect_blocking`
// spins `[NSApp run]` until the drag ends, for the CLI which has no run loop
// of its own. `bugle_select_rect_begin` returns at once and reports through a
// C callback, for the menu-bar app whose run loop is already going.

static void stop_app(void) {
    [NSApp stop:nil];
    // -stop only takes effect once an event is processed; post a no-op one.
    NSEvent *e = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                    location:NSZeroPoint
                               modifierFlags:0
                                   timestamp:0
                                windowNumber:0
                                     context:nil
                                     subtype:0
                                       data1:0
                                       data2:0];
    [NSApp postEvent:e atStart:YES];
}

/// `result`: 0 = cancelled, 1 = `rect` selected, 2 = Space asked for the
/// window picker instead.
typedef void (^BugleSelectCompletion)(int32_t result, CGRect rect);

void bugle_draw_overlay_hint(NSRect bounds, NSString *text) {
    if (text.length == 0) return;
    NSDictionary *attrs = @{
        NSFontAttributeName : [NSFont systemFontOfSize:13 weight:NSFontWeightMedium],
        NSForegroundColorAttributeName : [NSColor whiteColor]
    };
    NSSize sz = [text sizeWithAttributes:attrs];
    NSRect r = NSMakeRect(NSMidX(bounds) - (sz.width + 24) / 2, NSMinY(bounds) + 48, sz.width + 24, sz.height + 12);
    [[NSColor colorWithWhite:0 alpha:0.66] setFill];
    [[NSBezierPath bezierPathWithRoundedRect:r xRadius:8 yRadius:8] fill];
    [text drawAtPoint:NSMakePoint(r.origin.x + 12, r.origin.y + 6) withAttributes:attrs];
}

@class BugleSelectSession;

@interface BugleSelectView : NSView
@property(nonatomic) NSPoint start;
@property(nonatomic) NSRect sel;
@property(nonatomic) BOOL dragging;
@property(nonatomic, weak) BugleSelectSession *session;
@end

@interface BugleSelectSession : NSObject
@property(nonatomic, strong) NSMutableArray<NSWindow *> *windows;
@property(nonatomic, copy) BugleSelectCompletion completion;
@property(nonatomic) BOOL finished;
@property(nonatomic) BOOL allowSwitch;
@property(nonatomic, strong) NSTimer *timer;
- (void)finishWithRect:(CGRect)rect result:(int32_t)result;
- (void)armTimer:(double)seconds;
@end

static BugleSelectSession *g_select_session = nil;

@implementation BugleSelectView
- (BOOL)acceptsFirstResponder { return YES; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (void)resetCursorRects { [self addCursorRect:self.bounds cursor:[NSCursor crosshairCursor]]; }
- (void)drawRect:(NSRect)dirty {
    [[NSColor colorWithWhite:0 alpha:0.28] setFill];
    NSRectFill(self.bounds);
    if (self.session.allowSwitch && !self.dragging) {
        bugle_draw_overlay_hint(self.bounds, @"Drag an area  ·  Space: pick a window  ·  Esc: cancel");
    }
    if (self.dragging && self.sel.size.width > 0 && self.sel.size.height > 0) {
        [[NSColor clearColor] setFill];
        NSRectFillUsingOperation(self.sel, NSCompositingOperationCopy);
        [[NSColor whiteColor] setStroke];
        NSBezierPath *p = [NSBezierPath bezierPathWithRect:NSInsetRect(self.sel, 0.5, 0.5)];
        p.lineWidth = 1.0;
        [p stroke];
        NSString *label = [NSString stringWithFormat:@"%.0f × %.0f", self.sel.size.width, self.sel.size.height];
        NSDictionary *attrs = @{
            NSFontAttributeName : [NSFont monospacedDigitSystemFontOfSize:12 weight:NSFontWeightMedium],
            NSForegroundColorAttributeName : [NSColor whiteColor]
        };
        NSSize sz = [label sizeWithAttributes:attrs];
        NSRect lr = NSMakeRect(NSMinX(self.sel), NSMinY(self.sel) - sz.height - 10, sz.width + 12, sz.height + 6);
        if (lr.origin.y < 0) lr.origin.y = NSMinY(self.sel) + 6;
        [[NSColor colorWithWhite:0 alpha:0.75] setFill];
        [[NSBezierPath bezierPathWithRoundedRect:lr xRadius:4 yRadius:4] fill];
        [label drawAtPoint:NSMakePoint(lr.origin.x + 6, lr.origin.y + 3) withAttributes:attrs];
    }
}
- (void)mouseDown:(NSEvent *)e {
    self.start = [self convertPoint:e.locationInWindow fromView:nil];
    self.sel = NSMakeRect(self.start.x, self.start.y, 0, 0);
    self.dragging = YES;
    [self setNeedsDisplay:YES];
}
- (void)mouseDragged:(NSEvent *)e {
    NSPoint p = [self convertPoint:e.locationInWindow fromView:nil];
    CGFloat x0 = MIN(p.x, self.start.x), y0 = MIN(p.y, self.start.y);
    self.sel = NSMakeRect(floor(x0), floor(y0), fabs(p.x - self.start.x), fabs(p.y - self.start.y));
    [self setNeedsDisplay:YES];
}
- (void)mouseUp:(NSEvent *)e {
    if (!self.dragging) return;
    self.dragging = NO;
    if (self.sel.size.width >= 2 && self.sel.size.height >= 2) {
        NSRect inWin = self.sel;
        NSRect screenRect = [self.window convertRectToScreen:inWin];
        // Cocoa bottom-left → CoreGraphics top-left global coordinates.
        CGFloat primaryH = [NSScreen screens].firstObject.frame.size.height;
        CGRect r = CGRectMake(screenRect.origin.x, primaryH - screenRect.origin.y - screenRect.size.height,
                              screenRect.size.width, screenRect.size.height);
        [self.session finishWithRect:r result:1];
    }
    [self setNeedsDisplay:YES];
}
- (void)keyDown:(NSEvent *)e {
    if (e.keyCode == 53) { // Esc
        [self.session finishWithRect:CGRectZero result:0];
    } else if (e.keyCode == 49 && !e.isARepeat) { // Space
        bugle_select_rect_switch();
    }
}
@end

@interface BugleOverlayWindow : NSWindow
@end
@implementation BugleOverlayWindow
- (BOOL)canBecomeKeyWindow { return YES; }
- (BOOL)canBecomeMainWindow { return YES; }
@end

@implementation BugleSelectSession
/// Cancel after `seconds`, but never in the middle of a drag: a drag in
/// progress pushes the timeout back a few seconds at a time.
- (void)armTimer:(double)seconds {
    [self.timer invalidate];
    __weak BugleSelectSession *weak = self;
    self.timer = [NSTimer timerWithTimeInterval:seconds
                                        repeats:NO
                                          block:^(NSTimer *timer) {
                                              BugleSelectSession *s = weak;
                                              if (!s || s.finished) return;
                                              for (NSWindow *win in s.windows) {
                                                  if ([win.contentView isKindOfClass:[BugleSelectView class]] &&
                                                      ((BugleSelectView *)win.contentView).dragging) {
                                                      [s armTimer:3.0];
                                                      return;
                                                  }
                                              }
                                              [s finishWithRect:CGRectZero result:0];
                                          }];
    [[NSRunLoop mainRunLoop] addTimer:self.timer forMode:NSRunLoopCommonModes];
}
- (void)finishWithRect:(CGRect)rect result:(int32_t)result {
    if (self.finished) return;
    self.finished = YES;
    [self.timer invalidate];
    self.timer = nil;
    [NSCursor pop];
    for (NSWindow *win in self.windows) [win orderOut:nil];
    [self.windows removeAllObjects];
    BugleSelectCompletion done = self.completion;
    self.completion = nil;
    if (g_select_session == self) g_select_session = nil;
    // Give keyboard focus back to whatever the user was working in, unless
    // the window picker opens next (it needs Bugle active for Esc, and a
    // resign-active would cancel it).
    if (result != 2) [NSApp deactivate];
    if (done) done(result, rect);
}
@end

/// Show the overlay on every screen. Returns NO when a selection is already
/// in progress (the completion is then never called).
static BOOL select_begin(BugleSelectCompletion completion, BOOL allowSwitch, double timeoutSeconds) {
    if (g_select_session || bugle_pick_active()) return NO;
    NSApplication *app = [NSApplication sharedApplication];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    BugleSelectSession *session = [[BugleSelectSession alloc] init];
    session.windows = [NSMutableArray array];
    session.completion = completion;
    session.allowSwitch = allowSwitch;
    for (NSScreen *screen in [NSScreen screens]) {
        BugleOverlayWindow *win = [[BugleOverlayWindow alloc] initWithContentRect:screen.frame
                                                                        styleMask:NSWindowStyleMaskBorderless
                                                                          backing:NSBackingStoreBuffered
                                                                            defer:NO
                                                                           screen:screen];
        win.releasedWhenClosed = NO;
        win.level = NSScreenSaverWindowLevel;
        win.backgroundColor = [NSColor clearColor];
        win.opaque = NO;
        win.hasShadow = NO;
        win.ignoresMouseEvents = NO;
        win.acceptsMouseMovedEvents = YES;
        win.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary;
        BugleSelectView *v = [[BugleSelectView alloc] initWithFrame:win.contentView.bounds];
        v.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        v.session = session;
        win.contentView = v;
        [win makeFirstResponder:v];
        [win makeKeyAndOrderFront:nil];
        [session.windows addObject:win];
    }
    if (timeoutSeconds > 0) [session armTimer:timeoutSeconds];
    g_select_session = session;
    [app activateIgnoringOtherApps:YES];
    [[NSCursor crosshairCursor] push];
    return YES;
}

int32_t bugle_select_rect_blocking(double *x, double *y, double *w, double *h) {
    @autoreleasepool {
        __block BOOL ok = NO;
        __block CGRect r = CGRectZero;
        if (!select_begin(
                ^(int32_t k, CGRect rect) {
                    ok = k == 1;
                    r = rect;
                    stop_app();
                },
                NO, 0)) {
            return 0;
        }
        [NSApp run];
        if (!ok) return 0;
        *x = r.origin.x;
        *y = r.origin.y;
        *w = r.size.width;
        *h = r.size.height;
        return 1;
    }
}

int32_t bugle_select_rect_begin_ex(bugle_select_callback_t cb, void *ctx, int32_t allow_switch, double timeout_seconds) {
    @autoreleasepool {
        BOOL started = select_begin(
            ^(int32_t result, CGRect r) {
                cb(ctx, result, r.origin.x, r.origin.y, r.size.width, r.size.height);
            },
            allow_switch != 0, timeout_seconds);
        return started ? 1 : 0;
    }
}

int32_t bugle_select_rect_begin(bugle_select_callback_t cb, void *ctx) {
    return bugle_select_rect_begin_ex(cb, ctx, 0, 0);
}

int32_t bugle_select_rect_active(void) { return g_select_session ? 1 : 0; }

void bugle_editor_window_closed(void) {
    NSWindow *next = bugle_markup_editor_front_window();
    if (!next) next = bugle_video_editor_window();
    if (next) {
        [next makeKeyAndOrderFront:nil];
    } else {
        [NSApp deactivate];
    }
}

int32_t bugle_key_window_title(char *buf, size_t cap) {
    if (!buf || cap == 0) return 0;
    __block int32_t found = 0;
    void (^read)(void) = ^{
        @autoreleasepool {
            NSWindow *w = NSApp.keyWindow;
            if (!w || !NSApp.isActive) return;
            found = 1;
            const char *t = w.title.UTF8String ?: "";
            strlcpy(buf, t, cap);
        }
    };
    if ([NSThread isMainThread]) read();
    else dispatch_sync(dispatch_get_main_queue(), read);
    return found;
}

void bugle_app_deactivate(void) {
    @autoreleasepool {
        [NSApp deactivate];
    }
}

// For a process without a running AppKit loop (the CLI): make it an
// accessory app (no Dock icon) that has finished launching, so overlays can
// become key and receive keys, then pump events in short slices from the
// caller's own loop. `[NSApp run]` cannot be used there: the host needs to
// look at its own state (a Rust channel) between events.
void bugle_app_bootstrap(void) {
    @autoreleasepool {
        static BOOL launched = NO;
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        if (!launched) {
            launched = YES;
            [app finishLaunching];
        }
    }
}

void bugle_run_loop_once(double seconds) {
    @autoreleasepool {
        NSDate *until = [NSDate dateWithTimeIntervalSinceNow:seconds > 0 ? seconds : 0];
        // Dispatching events also runs the main run loop in the default
        // mode, which drains the main dispatch queue and fires the overlay
        // timers.
        NSEvent *e = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:until inMode:NSDefaultRunLoopMode dequeue:YES];
        if (e) [NSApp sendEvent:e];
        [NSApp updateWindows];
    }
}

int32_t bugle_is_main_thread(void) { return [NSThread isMainThread] ? 1 : 0; }

/// What Space does: hand over to the window picker, when the host allowed it
/// and no drag is in progress.
void bugle_select_rect_switch(void) {
    @autoreleasepool {
        BugleSelectSession *s = g_select_session;
        if (!s || !s.allowSwitch) return;
        for (NSWindow *win in s.windows) {
            if ([win.contentView isKindOfClass:[BugleSelectView class]] && ((BugleSelectView *)win.contentView).dragging) return;
        }
        [s finishWithRect:CGRectZero result:2];
    }
}

/// Dismiss an in-progress selection as cancelled (the callback fires with ok=0).
void bugle_select_rect_cancel(void) {
    @autoreleasepool {
        [g_select_session finishWithRect:CGRectZero result:0];
    }
}

// ---------------------------------------------------------------------------
// Pin to screen. Esc or double-click closes; drag anywhere to move; scroll
// to resize. `bugle_pin_image_show` returns at once (the window lives until
// closed); `bugle_pin_image_blocking` waits for it, for the CLI.
// ---------------------------------------------------------------------------

static NSMutableArray<NSWindow *> *g_pins = nil;

@interface BuglePinView : NSImageView
@property(nonatomic, copy) void (^onClose)(void);
- (void)closePin;
@end
@implementation BuglePinView
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (BOOL)acceptsFirstResponder { return YES; }
- (void)closePin {
    NSWindow *win = self.window;
    void (^done)(void) = self.onClose;
    self.onClose = nil;
    [win orderOut:nil];
    [g_pins removeObject:win];
    if (done) done();
}
- (void)mouseDown:(NSEvent *)e {
    if (e.clickCount == 2) { [self closePin]; return; }
    [self.window performWindowDragWithEvent:e];
}
- (void)keyDown:(NSEvent *)e {
    if (e.keyCode == 53 || (e.keyCode == 13 && (e.modifierFlags & NSEventModifierFlagCommand))) [self closePin];
}
- (void)scrollWheel:(NSEvent *)e {
    NSRect f = self.window.frame;
    CGFloat factor = 1.0 + (e.scrollingDeltaY * 0.01);
    factor = MAX(0.8, MIN(1.25, factor));
    CGFloat nw = MAX(80, f.size.width * factor);
    CGFloat nh = f.size.height * (nw / f.size.width);
    NSRect nf = NSMakeRect(f.origin.x, f.origin.y + (f.size.height - nh), nw, nh);
    [self.window setFrame:nf display:YES animate:NO];
}
@end

static BuglePinView *pin_show(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity) {
    NSApplication *app = [NSApplication sharedApplication];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    NSImage *img = [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]];
    if (!img) return nil;
    if (!g_pins) g_pins = [NSMutableArray array];
    CGFloat primaryH = [NSScreen screens].firstObject.frame.size.height;
    if (w <= 0 || h <= 0) {
        NSSize s = img.size;
        w = s.width;
        h = s.height;
    }
    if (x < 0 || y < 0) {
        NSRect vf = [NSScreen mainScreen].visibleFrame;
        x = NSMaxX(vf) - w - 24;
        y = primaryH - NSMaxY(vf) + 24;
    }
    NSRect frame = NSMakeRect(x, primaryH - y - h, w, h);
    BugleOverlayWindow *win = [[BugleOverlayWindow alloc] initWithContentRect:frame
                                                                    styleMask:NSWindowStyleMaskBorderless
                                                                      backing:NSBackingStoreBuffered
                                                                        defer:NO];
    win.releasedWhenClosed = NO;
    win.level = NSFloatingWindowLevel;
    win.hasShadow = YES;
    win.opaque = NO;
    win.backgroundColor = [NSColor clearColor];
    win.alphaValue = opacity > 0 ? opacity : 1.0;
    win.movableByWindowBackground = YES;
    win.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary;
    BuglePinView *iv = [[BuglePinView alloc] initWithFrame:win.contentView.bounds];
    iv.image = img;
    iv.imageScaling = NSImageScaleProportionallyUpOrDown;
    iv.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    iv.wantsLayer = YES;
    iv.layer.cornerRadius = 8;
    iv.layer.masksToBounds = YES;
    win.contentView = iv;
    [win makeFirstResponder:iv];
    [g_pins addObject:win];
    [win makeKeyAndOrderFront:nil];
    [app activateIgnoringOtherApps:YES];
    return iv;
}

void bugle_pin_image_blocking(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity) {
    @autoreleasepool {
        BuglePinView *iv = pin_show(png, len, x, y, w, h, opacity);
        if (!iv) return;
        iv.onClose = ^{ stop_app(); };
        [NSApp run];
    }
}

int32_t bugle_pin_image_show(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity) {
    @autoreleasepool {
        return pin_show(png, len, x, y, w, h, opacity) ? 1 : 0;
    }
}

int32_t bugle_pin_close_all(void) {
    @autoreleasepool {
        NSArray<NSWindow *> *open = [g_pins copy];
        for (NSWindow *win in open) {
            if ([win.contentView isKindOfClass:[BuglePinView class]]) [(BuglePinView *)win.contentView closePin];
        }
        return (int32_t)open.count;
    }
}

// ---------------------------------------------------------------------------
// Toast: a small non-activating panel in the corner of the main screen that
// shows a thumbnail and two lines of text, then fades out. Clicking it opens
// the file. Used by the menu-bar app as its "capture done" notification;
// unbundled binaries cannot post user notifications.
// ---------------------------------------------------------------------------

static NSPanel *g_toast = nil;
static NSTimer *g_toast_timer = nil;

@interface BugleToastView : NSVisualEffectView
@property(nonatomic, copy) NSString *openPath;
@end
@implementation BugleToastView
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (void)mouseDown:(NSEvent *)e {
    if (self.openPath.length) {
        [[NSWorkspace sharedWorkspace] openURL:[NSURL fileURLWithPath:self.openPath]];
    }
    [g_toast_timer invalidate];
    g_toast_timer = nil;
    [g_toast orderOut:nil];
    g_toast = nil;
}
@end

static void toast_dismiss(void) {
    NSPanel *p = g_toast;
    g_toast = nil;
    g_toast_timer = nil;
    if (!p) return;
    [NSAnimationContext runAnimationGroup:^(NSAnimationContext *ctx) {
        ctx.duration = 0.25;
        [p.animator setAlphaValue:0.0];
    } completionHandler:^{
        [p orderOut:nil];
    }];
}

void bugle_toast_show(const uint8_t *png, size_t len, const char *title, const char *subtitle, const char *open_path,
                      double seconds) {
    @autoreleasepool {
        if (g_toast) {
            [g_toast_timer invalidate];
            [g_toast orderOut:nil];
            g_toast = nil;
            g_toast_timer = nil;
        }
        NSImage *img = (png && len) ? [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]] : nil;
        CGFloat pad = 10, thumb = img ? 56 : 0, width = 320, height = 76;
        NSRect vf = [NSScreen mainScreen].visibleFrame;
        NSRect frame = NSMakeRect(NSMaxX(vf) - width - 16, NSMaxY(vf) - height - 16, width, height);
        NSPanel *panel = [[NSPanel alloc] initWithContentRect:frame
                                                    styleMask:NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO];
        panel.releasedWhenClosed = NO;
        panel.level = NSStatusWindowLevel;
        panel.opaque = NO;
        panel.hasShadow = YES;
        panel.backgroundColor = [NSColor clearColor];
        panel.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary;
        panel.hidesOnDeactivate = NO;
        panel.sharingType = NSWindowSharingNone; // keep it out of screen captures where honoured

        BugleToastView *root = [[BugleToastView alloc] initWithFrame:NSMakeRect(0, 0, width, height)];
        root.material = NSVisualEffectMaterialHUDWindow;
        root.blendingMode = NSVisualEffectBlendingModeBehindWindow;
        root.state = NSVisualEffectStateActive;
        root.wantsLayer = YES;
        root.layer.cornerRadius = 12;
        root.layer.masksToBounds = YES;
        root.openPath = open_path ? [NSString stringWithUTF8String:open_path] : nil;

        CGFloat tx = pad;
        if (img) {
            NSImageView *iv = [[NSImageView alloc] initWithFrame:NSMakeRect(pad, (height - thumb) / 2, thumb, thumb)];
            iv.image = img;
            iv.imageScaling = NSImageScaleProportionallyUpOrDown;
            iv.wantsLayer = YES;
            iv.layer.cornerRadius = 6;
            iv.layer.masksToBounds = YES;
            [root addSubview:iv];
            tx = pad + thumb + pad;
        }
        NSTextField *t = [NSTextField labelWithString:title ? [NSString stringWithUTF8String:title] : @""];
        t.font = [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold];
        t.textColor = [NSColor whiteColor];
        t.lineBreakMode = NSLineBreakByTruncatingTail;
        t.frame = NSMakeRect(tx, height / 2 + 2, width - tx - pad, 18);
        [root addSubview:t];
        NSTextField *s = [NSTextField labelWithString:subtitle ? [NSString stringWithUTF8String:subtitle] : @""];
        s.font = [NSFont systemFontOfSize:11];
        s.textColor = [NSColor colorWithWhite:1 alpha:0.75];
        s.lineBreakMode = NSLineBreakByTruncatingMiddle;
        s.frame = NSMakeRect(tx, height / 2 - 18, width - tx - pad, 16);
        [root addSubview:s];
        panel.contentView = root;
        panel.alphaValue = 1.0;
        [panel orderFrontRegardless];
        g_toast = panel;
        g_toast_timer = [NSTimer scheduledTimerWithTimeInterval:(seconds > 0 ? seconds : 2.5)
                                                        repeats:NO
                                                          block:^(NSTimer *timer) { toast_dismiss(); }];
    }
}

// ---------------------------------------------------------------------------
// Media helpers (AVFoundation)
// ---------------------------------------------------------------------------

int32_t bugle_export_audio_m4a(const char *in_path, const char *out_path) {
    @autoreleasepool {
        clear_error();
        NSURL *in = [NSURL fileURLWithPath:[NSString stringWithUTF8String:in_path]];
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:in options:nil];
        if ([asset tracksWithMediaType:AVMediaTypeAudio].count == 0) {
            set_error(@"no audio track in %s", in_path);
            return -2;
        }
        AVAssetExportSession *s = [[AVAssetExportSession alloc] initWithAsset:asset presetName:AVAssetExportPresetAppleM4A];
        if (!s) { set_error(@"could not create export session"); return -1; }
        s.outputURL = out;
        s.outputFileType = AVFileTypeAppleM4A;
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [s exportAsynchronouslyWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
        if (s.status != AVAssetExportSessionStatusCompleted) {
            set_error(@"audio export failed: %@", s.error.localizedDescription);
            return -1;
        }
        return 0;
    }
}

static int32_t remux(const char *in_path, const char *out_path, AVFileType type) {
    @autoreleasepool {
        clear_error();
        NSURL *in = [NSURL fileURLWithPath:[NSString stringWithUTF8String:in_path]];
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:in options:nil];
        AVAssetExportSession *s = [[AVAssetExportSession alloc] initWithAsset:asset presetName:AVAssetExportPresetPassthrough];
        if (!s) { set_error(@"could not create export session"); return -1; }
        s.outputURL = out;
        s.outputFileType = type;
        s.shouldOptimizeForNetworkUse = YES;
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [s exportAsynchronouslyWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
        if (s.status != AVAssetExportSessionStatusCompleted) {
            set_error(@"%@ export failed: %@", type, s.error.localizedDescription);
            return -1;
        }
        return 0;
    }
}

int32_t bugle_export_mp4(const char *in_path, const char *out_path) {
    return remux(in_path, out_path, AVFileTypeMPEG4);
}

int32_t bugle_export_mov(const char *in_path, const char *out_path) {
    return remux(in_path, out_path, AVFileTypeQuickTimeMovie);
}

int32_t bugle_audio_track_count(const char *path) {
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        return (int32_t)[asset tracksWithMediaType:AVMediaTypeAudio].count;
    }
}

// Loudest sample of every enabled audio track mixed together, in `n` equal
// buckets of `duration` seconds (decoded mono at a low rate: enough for a
// timeline waveform). Returns 1 with `out` filled (0 for silence, full scale
// 1, clamped), 0 when there is no audio (`out` zeroed), 2 when `*cancel`
// (checked once per sample buffer; may be NULL) turned true, -1 on error.
int32_t bugle_audio_peaks(const char *path, double duration, float *out, int32_t n, const volatile bool *cancel) {
    @autoreleasepool {
        clear_error();
        if (!path || !out || n <= 0 || !(duration > 0)) {
            set_error(@"audio peaks: bad arguments");
            return -1;
        }
        memset(out, 0, sizeof(float) * (size_t)n);
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        NSMutableArray<AVAssetTrack *> *audio = [NSMutableArray array];
        for (AVAssetTrack *t in [asset tracksWithMediaType:AVMediaTypeAudio]) {
            if (t.enabled) [audio addObject:t];
        }
        if (audio.count == 0) return 0;
        NSError *err = nil;
        AVAssetReader *r = [[AVAssetReader alloc] initWithAsset:asset error:&err];
        if (!r) { set_error(@"could not read %s: %@", path, err.localizedDescription); return -1; }
        const double rate = 8000;
        NSDictionary *pcm = @{
            AVFormatIDKey: @(kAudioFormatLinearPCM),
            AVSampleRateKey: @(rate),
            AVNumberOfChannelsKey: @1,
            AVLinearPCMBitDepthKey: @32,
            AVLinearPCMIsFloatKey: @YES,
            AVLinearPCMIsBigEndianKey: @NO,
            AVLinearPCMIsNonInterleaved: @NO,
        };
        AVAssetReaderAudioMixOutput *o = [AVAssetReaderAudioMixOutput assetReaderAudioMixOutputWithAudioTracks:audio audioSettings:pcm];
        if (![r canAddOutput:o]) { set_error(@"could not decode the audio of %s", path); return -1; }
        [r addOutput:o];
        if (![r startReading]) { set_error(@"could not read %s: %@", path, r.error.localizedDescription); return -1; }
        const double per = duration / n;
        NSMutableData *buf = [NSMutableData data];
        CMSampleBufferRef sb;
        if (cancel && *cancel) { [r cancelReading]; return 2; }
        while ((sb = [o copyNextSampleBuffer])) {
            if (cancel && *cancel) {
                CFRelease(sb);
                [r cancelReading];
                return 2;
            }
            CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
            double t0 = CMTIME_IS_NUMERIC(pts) ? CMTimeGetSeconds(pts) : 0;
            CMItemCount frames = CMSampleBufferGetNumSamples(sb);
            CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sb);
            size_t bytes = (size_t)frames * sizeof(float);
            BOOL past = NO;
            if (bb && frames > 0 && CMBlockBufferGetDataLength(bb) >= bytes) {
                if (buf.length < bytes) buf.length = bytes;
                if (CMBlockBufferCopyDataBytes(bb, 0, bytes, buf.mutableBytes) == kCMBlockBufferNoErr) {
                    const float *v = buf.bytes;
                    for (CMItemCount k = 0; k < frames; k++) {
                        double t = t0 + (double)k / rate;
                        if (t < 0) continue;
                        int64_t i = (int64_t)(t / per);
                        if (i >= n) { past = YES; break; }
                        float a = fminf(fabsf(v[k]), 1.0f);
                        if (a > out[i]) out[i] = a;
                    }
                }
            }
            CFRelease(sb);
            if (past) {
                // Audio past the end of the video: nothing more to draw.
                [r cancelReading];
                return 1;
            }
        }
        if (r.status == AVAssetReaderStatusFailed) {
            set_error(@"decoding the audio of %s failed: %@", path, r.error.localizedDescription);
            return -1;
        }
        return 1;
    }
}

int32_t bugle_video_track_count(const char *path) {
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        return (int32_t)[asset tracksWithMediaType:AVMediaTypeVideo].count;
    }
}

// Fold every enabled audio track of a movie into one AAC track, or re-level
// each in place (`separate`). A post-pass
// on the finished file: each audio track is decoded to 48 kHz stereo float
// PCM by its own reader (an AVAssetReaderAudioMixOutput over that one track,
// so its edit timeline and offset are kept, and no track's reads wait on
// another's), the tracks are lined up frame for frame by presentation time
// (gaps and late starts filled with silence; in practice the mix output
// already renders a track's offset as leading silence and delivers
// contiguous times, so that path is a guard), and `process` turns each
// block of aligned tracks into the mixed block, which is encoded once. With
// `separate`, `process` returns one block per source instead (levels applied,
// tracks kept apart) and each is encoded as its own AAC track, in order.
// Video tracks are copied sample for sample without re-encoding, on their
// own timescale. Returns 1 and writes nothing when there are fewer than two
// audio tracks; refuses to write over its input.

#define MIX_RATE 48000
#define MIX_CH 2
#define MIX_BLOCK 4096
#define MIX_FRAME_BYTES (sizeof(float) * MIX_CH)

@interface BugleMixSource : NSObject
@property (nonatomic, strong) AVAssetReader *reader;
@property (nonatomic, strong) AVAssetReaderOutput *output;
// Interleaved frames from the mix cursor on.
@property (nonatomic, strong) NSMutableData *fifo;
// A decoded buffer that starts beyond the FIFO's end, and its first frame.
@property (nonatomic, strong) NSData *pending;
@property (nonatomic) int64_t pendingPos;
@property (nonatomic) BOOL eof;
@end

@implementation BugleMixSource
@end

static int64_t mix_frames(NSData *d) { return (int64_t)(d.length / MIX_FRAME_BYTES); }

/// Fill `s` until its FIFO covers [cursor, want) or the track is exhausted.
/// NO when the track's reader failed.
static BOOL mix_fill(BugleMixSource *s, int64_t cursor, int64_t want) {
    for (;;) {
        int64_t have = cursor + mix_frames(s.fifo);
        if (have >= want) return YES;
        if (s.pending) {
            if (s.pendingPos > have) {
                // Silence up to the buffer (zero-filled), but not past `want`:
                // a far-off start is reached one block at a time.
                int64_t to = MIN(s.pendingPos, want);
                [s.fifo increaseLengthBy:(NSUInteger)(to - have) * MIX_FRAME_BYTES];
                if (to == want) return YES;
                have = to;
            }
            // Frames before `have` are already covered: drop the overlap.
            int64_t skip = have - s.pendingPos, n = mix_frames(s.pending);
            if (skip < n) {
                [s.fifo appendBytes:(const char *)s.pending.bytes + skip * MIX_FRAME_BYTES
                             length:(NSUInteger)(n - skip) * MIX_FRAME_BYTES];
            }
            s.pending = nil;
            continue;
        }
        if (s.eof) return YES;
        CMSampleBufferRef sb = [s.output copyNextSampleBuffer];
        if (!sb) {
            s.eof = YES;
            if (s.reader.status == AVAssetReaderStatusFailed) return NO;
            continue;
        }
        CMItemCount n = CMSampleBufferGetNumSamples(sb);
        CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sb);
        size_t need = (size_t)n * MIX_FRAME_BYTES;
        if (n > 0 && bb && CMBlockBufferGetDataLength(bb) >= need) {
            NSMutableData *d = [NSMutableData dataWithLength:need];
            if (CMBlockBufferCopyDataBytes(bb, 0, need, d.mutableBytes) == kCMBlockBufferNoErr) {
                CMTime pts = CMTimeConvertScale(CMSampleBufferGetPresentationTimeStamp(sb), MIX_RATE,
                                                kCMTimeRoundingMethod_RoundHalfAwayFromZero);
                s.pendingPos = CMTIME_IS_NUMERIC(pts) ? pts.value : have;
                s.pending = d;
            }
        }
        CFRelease(sb);
    }
}

/// A sample buffer holding `frames` interleaved float frames at `pos`.
static CMSampleBufferRef mix_buffer(CMAudioFormatDescriptionRef fmt, const float *pcm, int64_t frames, int64_t pos) {
    size_t bytes = (size_t)frames * MIX_FRAME_BYTES;
    CMBlockBufferRef bb = NULL;
    if (CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, NULL, bytes, kCFAllocatorDefault, NULL, 0, bytes,
                                           kCMBlockBufferAssureMemoryNowFlag, &bb) != kCMBlockBufferNoErr) {
        return NULL;
    }
    if (CMBlockBufferReplaceDataBytes(pcm, bb, 0, bytes) != kCMBlockBufferNoErr) {
        CFRelease(bb);
        return NULL;
    }
    CMSampleBufferRef sb = NULL;
    OSStatus st = CMAudioSampleBufferCreateReadyWithPacketDescriptions(kCFAllocatorDefault, bb, fmt, (CMItemCount)frames,
                                                                       CMTimeMake(pos, MIX_RATE), NULL, &sb);
    CFRelease(bb);
    return st == noErr ? sb : NULL;
}

int32_t bugle_mix_audio_tracks(const char *in_path, const char *out_path, int32_t separate, bugle_mix_fn process,
                               void *ctx) {
    @autoreleasepool {
        clear_error();
        NSURL *in = [NSURL fileURLWithPath:[NSString stringWithUTF8String:in_path]];
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        if ([in.URLByResolvingSymlinksInPath.path isEqualToString:out.URLByResolvingSymlinksInPath.path]) {
            set_error(@"mixing %s: input and output are the same file", in_path);
            return -1;
        }
        if (!process) { set_error(@"mixing %s: no mix function", in_path); return -1; }
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:in options:nil];
        NSMutableArray<AVAssetTrack *> *audio = [NSMutableArray array];
        for (AVAssetTrack *t in [asset tracksWithMediaType:AVMediaTypeAudio]) {
            if (t.enabled) [audio addObject:t];
        }
        if (audio.count < 2) return 1;
        NSArray<AVAssetTrack *> *video = [asset tracksWithMediaType:AVMediaTypeVideo];
        NSError *err = nil;
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        AVFileType type = [out.pathExtension.lowercaseString isEqualToString:@"mov"] ? AVFileTypeQuickTimeMovie : AVFileTypeMPEG4;
        AVAssetWriter *writer = [[AVAssetWriter alloc] initWithURL:out fileType:type error:&err];
        if (!writer) { set_error(@"AVAssetWriter: %@", err.localizedDescription); return -1; }
        writer.shouldOptimizeForNetworkUse = YES;

        NSMutableArray<AVAssetReader *> *readers = [NSMutableArray array];
        NSMutableArray<AVAssetReaderOutput *> *videoOutputs = [NSMutableArray array];
        NSMutableArray<AVAssetWriterInput *> *videoInputs = [NSMutableArray array];
        if (video.count > 0) {
            AVAssetReader *vr = [[AVAssetReader alloc] initWithAsset:asset error:&err];
            if (!vr) { set_error(@"could not read %s: %@", in_path, err.localizedDescription); return -1; }
            for (AVAssetTrack *t in video) {
                AVAssetReaderTrackOutput *ro = [AVAssetReaderTrackOutput assetReaderTrackOutputWithTrack:t outputSettings:nil];
                ro.alwaysCopiesSampleData = NO;
                CMFormatDescriptionRef hint = (__bridge CMFormatDescriptionRef)t.formatDescriptions.firstObject;
                AVAssetWriterInput *wi = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo
                                                                            outputSettings:nil
                                                                          sourceFormatHint:hint];
                wi.transform = t.preferredTransform;
                // Keep the source's frame times exactly (the default is 1/600 s).
                if (t.naturalTimeScale > 0) wi.mediaTimeScale = t.naturalTimeScale;
                if (![vr canAddOutput:ro] || ![writer canAddInput:wi]) { set_error(@"could not copy the video track"); return -1; }
                [vr addOutput:ro];
                [writer addInput:wi];
                [videoOutputs addObject:ro];
                [videoInputs addObject:wi];
            }
            [readers addObject:vr];
        }
        NSDictionary *pcm = @{
            AVFormatIDKey: @(kAudioFormatLinearPCM),
            AVSampleRateKey: @(MIX_RATE),
            AVNumberOfChannelsKey: @(MIX_CH),
            AVLinearPCMBitDepthKey: @32,
            AVLinearPCMIsFloatKey: @YES,
            AVLinearPCMIsBigEndianKey: @NO,
            AVLinearPCMIsNonInterleaved: @NO,
        };
        NSMutableArray<BugleMixSource *> *sources = [NSMutableArray array];
        for (AVAssetTrack *t in audio) {
            AVAssetReader *r = [[AVAssetReader alloc] initWithAsset:asset error:&err];
            if (!r) { set_error(@"could not read %s: %@", in_path, err.localizedDescription); return -1; }
            AVAssetReaderAudioMixOutput *o = [AVAssetReaderAudioMixOutput assetReaderAudioMixOutputWithAudioTracks:@[t]
                                                                                                     audioSettings:pcm];
            if (![r canAddOutput:o]) { set_error(@"could not decode audio track %d", t.trackID); return -1; }
            [r addOutput:o];
            BugleMixSource *s = [BugleMixSource new];
            s.reader = r;
            s.output = o;
            s.fifo = [NSMutableData data];
            [sources addObject:s];
            [readers addObject:r];
        }
        NSDictionary *aac = @{
            AVFormatIDKey: @(kAudioFormatMPEG4AAC),
            AVSampleRateKey: @(MIX_RATE),
            AVNumberOfChannelsKey: @(MIX_CH),
            AVEncoderBitRateKey: @160000,
        };
        // One output track for a mix, one per source when the tracks are kept
        // separate (in source order, each keeping its language tag).
        NSUInteger n = sources.count;
        NSUInteger nOut = separate ? n : 1;
        NSMutableArray<AVAssetWriterInput *> *audioInputs = [NSMutableArray array];
        for (NSUInteger k = 0; k < nOut; k++) {
            AVAssetWriterInput *ai = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:aac];
            if (separate) {
                if (audio[k].languageCode) ai.languageCode = audio[k].languageCode;
                if (audio[k].extendedLanguageTag) ai.extendedLanguageTag = audio[k].extendedLanguageTag;
            }
            if (![writer canAddInput:ai]) { set_error(@"could not set up the audio mix"); return -1; }
            [writer addInput:ai];
            [audioInputs addObject:ai];
        }
        AudioStreamBasicDescription asbd = {
            .mSampleRate = MIX_RATE,
            .mFormatID = kAudioFormatLinearPCM,
            .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            .mBytesPerPacket = MIX_FRAME_BYTES,
            .mFramesPerPacket = 1,
            .mBytesPerFrame = MIX_FRAME_BYTES,
            .mChannelsPerFrame = MIX_CH,
            .mBitsPerChannel = 32,
        };
        CMAudioFormatDescriptionRef fmtRef = NULL;
        if (CMAudioFormatDescriptionCreate(kCFAllocatorDefault, &asbd, 0, NULL, 0, NULL, NULL, &fmtRef) != noErr) {
            set_error(@"could not describe the mixed audio");
            return -1;
        }
        id fmt = CFBridgingRelease(fmtRef);

        void (^cancelAll)(void) = ^{
            for (AVAssetReader *r in readers) [r cancelReading];
            [writer cancelWriting];
            [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        };
        if (![writer startWriting]) {
            set_error(@"could not start writing %s: %@", out_path, writer.error.localizedDescription);
            return -1;
        }
        for (AVAssetReader *r in readers) {
            if (![r startReading]) {
                NSString *why = r.error.localizedDescription;
                cancelAll();
                set_error(@"could not read %s: %@", in_path, why);
                return -1;
            }
        }
        [writer startSessionAtSourceTime:kCMTimeZero];
        // One pump per output track: the writer interleaves tracks and holds
        // one back until the others catch up.
        dispatch_group_t group = dispatch_group_create();
        // Once this function gives up on a stuck pump (timeout), `abandoned`
        // is set under the gate: from then on no pump touches the cancelled
        // writer, and the audio pumps never call `process`/`ctx`, which
        // belong to the caller.
        NSLock *gate = [NSLock new];
        __block BOOL abandoned = NO;
        for (NSUInteger i = 0; i < videoInputs.count; i++) {
            AVAssetReaderOutput *ro = videoOutputs[i];
            AVAssetWriterInput *wi = videoInputs[i];
            __block BOOL done = NO;
            dispatch_group_enter(group);
            [wi requestMediaDataWhenReadyOnQueue:dispatch_queue_create("bugle.mix.video", DISPATCH_QUEUE_SERIAL)
                                      usingBlock:^{
                                          while (!done && wi.readyForMoreMediaData) {
                                              CMSampleBufferRef sb = [ro copyNextSampleBuffer];
                                              BOOL ok = sb && [wi appendSampleBuffer:sb];
                                              if (sb) CFRelease(sb);
                                              if (!ok) {
                                                  done = YES;
                                                  [gate lock];
                                                  if (!abandoned) [wi markAsFinished];
                                                  [gate unlock];
                                                  dispatch_group_leave(group);
                                              }
                                          }
                                      }];
        }
        // The audio side: `produce` lines the tracks up for one block, runs
        // `process` and queues one buffer per output track. The audio pumps
        // share one serial queue, so `produce` never runs twice at once, and
        // a pump produces a block only when its own queue is empty (the
        // writer's interleaving keeps the queues short). Buffers are owned by
        // the blocks (`scratch` is captured), so a pump still running after a
        // timeout never touches freed memory.
        NSMutableData *scratch = [NSMutableData dataWithLength:(n + nOut) * MIX_BLOCK * MIX_FRAME_BYTES
                                                               + n * sizeof(float *)];
        NSMutableArray<NSMutableArray *> *queues = [NSMutableArray array];
        for (NSUInteger k = 0; k < nOut; k++) [queues addObject:[NSMutableArray array]];
        __block int64_t cursor = 0;
        __block BOOL sourceDone = NO;
        __block NSString *audioError = nil;
        void (^produce)(void) = ^{
            float *tracks = scratch.mutableBytes;
            float *outBuf = tracks + n * MIX_BLOCK * MIX_CH;
            const float **ptrs = (const float **)(void *)(outBuf + nOut * MIX_BLOCK * MIX_CH);
            BOOL ok = YES;
            int64_t frames = 0;
            for (BugleMixSource *s in sources) {
                if (!mix_fill(s, cursor, cursor + MIX_BLOCK)) {
                    ok = NO;
                    audioError = s.reader.error.localizedDescription ?: @"decoding an audio track failed";
                }
                frames = MAX(frames, MIN((int64_t)MIX_BLOCK, mix_frames(s.fifo)));
            }
            if (ok && frames > 0) {
                for (NSUInteger t = 0; t < n; t++) {
                    BugleMixSource *s = sources[t];
                    int64_t have = MIN(frames, mix_frames(s.fifo));
                    float *dst = tracks + t * MIX_BLOCK * MIX_CH;
                    memcpy(dst, s.fifo.bytes, (size_t)have * MIX_FRAME_BYTES);
                    memset(dst + have * MIX_CH, 0, (size_t)(frames - have) * MIX_FRAME_BYTES);
                    [s.fifo replaceBytesInRange:NSMakeRange(0, (NSUInteger)have * MIX_FRAME_BYTES) withBytes:NULL length:0];
                    ptrs[t] = dst;
                }
                [gate lock];
                if (abandoned) {
                    ok = NO;
                } else if (process(ctx, ptrs, (int32_t)n, MIX_CH, MIX_RATE, outBuf, (int32_t)nOut, (int32_t)frames) != 0) {
                    ok = NO;
                    audioError = @"the mix function failed";
                }
                [gate unlock];
                // Output track k is the k-th run of `frames` frames in `outBuf`.
                for (NSUInteger k = 0; ok && k < nOut; k++) {
                    CMSampleBufferRef sb = mix_buffer((__bridge CMAudioFormatDescriptionRef)fmt,
                                                      outBuf + k * (NSUInteger)frames * MIX_CH, frames, cursor);
                    if (!sb) {
                        audioError = @"could not build a mixed audio buffer";
                        ok = NO;
                    } else {
                        [queues[k] addObject:CFBridgingRelease(sb)];
                    }
                }
                cursor += frames;
            }
            if (!ok || frames == 0) sourceDone = YES;
        };
        dispatch_queue_t audioQueue = dispatch_queue_create("bugle.mix.audio", DISPATCH_QUEUE_SERIAL);
        for (NSUInteger k = 0; k < nOut; k++) {
            AVAssetWriterInput *ai = audioInputs[k];
            NSMutableArray *queue = queues[k];
            __block BOOL done = NO;
            dispatch_group_enter(group);
            [ai requestMediaDataWhenReadyOnQueue:audioQueue
                                      usingBlock:^{
                while (!done && ai.readyForMoreMediaData) {
                    @autoreleasepool {
                        if (queue.count == 0 && !sourceDone) produce();
                        BOOL ok = queue.count > 0;
                        if (ok) {
                            id sb = queue.firstObject;
                            [queue removeObjectAtIndex:0];
                            [gate lock];
                            ok = !abandoned && [ai appendSampleBuffer:(__bridge CMSampleBufferRef)sb];
                            [gate unlock];
                            // A failed append means the writer failed: stop
                            // producing for every track.
                            if (!ok) sourceDone = YES;
                        }
                        if (!ok) {
                            done = YES;
                            [queue removeAllObjects];
                            [gate lock];
                            if (!abandoned) [ai markAsFinished];
                            [gate unlock];
                            dispatch_group_leave(group);
                        }
                    }
                }
            }];
        }
        // A failed writer stops asking for data, so bound the wait by the
        // media length (decoding runs many times faster than real time).
        double secs = CMTimeGetSeconds(asset.duration);
        double limit = 60.0 + (isfinite(secs) && secs > 0 ? secs * 2.0 : 0.0);
        if (dispatch_group_wait(group, dispatch_time(DISPATCH_TIME_NOW, (int64_t)(limit * NSEC_PER_SEC))) != 0) {
            [gate lock];
            abandoned = YES;
            cancelAll();
            [gate unlock];
            // A pump that never returns keeps its enter on the group, and
            // releasing a group with outstanding enters traps: keep it alive.
            (void)CFBridgingRetain(group);
            set_error(@"timed out mixing the audio tracks%@",
                      writer.error ? [NSString stringWithFormat:@" (%@)", writer.error.localizedDescription] : @"");
            return -1;
        }
        NSString *why = audioError;
        for (AVAssetReader *r in readers) {
            if (!why && r.status == AVAssetReaderStatusFailed) why = r.error.localizedDescription ?: @"unknown error";
        }
        if (!why && writer.status == AVAssetWriterStatusFailed) why = writer.error.localizedDescription ?: @"unknown error";
        if (why) {
            cancelAll();
            set_error(@"mixing the audio tracks failed: %@", why);
            return -1;
        }
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
        if (writer.status != AVAssetWriterStatusCompleted) {
            [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
            set_error(@"writing the mixed file failed: %@", writer.error.localizedDescription);
            return -1;
        }
        return 0;
    }
}

double bugle_media_duration(const char *path) {
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        return CMTimeGetSeconds(asset.duration);
    }
}

bugle_image_t bugle_video_frame(const char *path, double seconds) {
    @autoreleasepool {
        clear_error();
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        AVAssetImageGenerator *gen = [[AVAssetImageGenerator alloc] initWithAsset:asset];
        gen.appliesPreferredTrackTransform = YES;
        gen.requestedTimeToleranceBefore = CMTimeMake(1, 10);
        gen.requestedTimeToleranceAfter = CMTimeMake(1, 10);
        NSError *err = nil;
        CGImageRef img = [gen copyCGImageAtTime:CMTimeMakeWithSeconds(seconds, 600) actualTime:NULL error:&err];
        if (!img) { set_error(@"frame extraction failed for %@: %@", url.path, err.localizedDescription); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

// ---------------------------------------------------------------------------
// Editing: trimmed / re-encoded export, frame reader, frame writer
// ---------------------------------------------------------------------------

static AVAssetTrack *first_video_track(AVAsset *asset) {
    return [asset tracksWithMediaType:AVMediaTypeVideo].firstObject;
}

static AVURLAsset *precise_asset(const char *path) {
    return [AVURLAsset URLAssetWithURL:[NSURL fileURLWithPath:[NSString stringWithUTF8String:path]]
                               options:@{AVURLAssetPreferPreciseDurationAndTimingKey: @YES}];
}

/// Wait for `sem` at most `timeout` seconds (<= 0: forever). YES when signalled.
static BOOL wait_bounded(dispatch_semaphore_t sem, double timeout) {
    dispatch_time_t t = timeout > 0 ? dispatch_time(DISPATCH_TIME_NOW, (int64_t)(timeout * NSEC_PER_SEC)) : DISPATCH_TIME_FOREVER;
    return dispatch_semaphore_wait(sem, t) == 0;
}

/// Media time of the sample shown at track time `t` (edit lists map the
/// track timeline onto the media; plain files map 1:1).
static CMTime media_time_for(AVAssetTrack *track, CMTime t) {
    AVAssetTrackSegment *seg = [track segmentForTrackTime:t];
    if (!seg || seg.isEmpty) return t;
    CMTimeMapping m = seg.timeMapping;
    return CMTimeAdd(m.source.start, CMTimeSubtract(t, m.target.start));
}

int32_t bugle_video_info(const char *path, bugle_video_info_t *out) {
    @autoreleasepool {
        clear_error();
        if (!path || !out) { set_error(@"video info: NULL argument"); return -1; }
        memset(out, 0, sizeof(*out));
        AVURLAsset *asset = precise_asset(path);
        AVAssetTrack *track = first_video_track(asset);
        if (!track) { set_error(@"%s has no video track", path); return -1; }
        CGSize s = CGSizeApplyAffineTransform(track.naturalSize, track.preferredTransform);
        out->width = (int32_t)llround(fabs(s.width));
        out->height = (int32_t)llround(fabs(s.height));
        out->nominal_fps = track.nominalFrameRate;
        out->start = CMTimeGetSeconds(track.timeRange.start);
        out->duration = CMTimeGetSeconds(track.timeRange.duration);
        // Every sample stored in the media, including any an edit list hides.
        out->stored_samples = -1;
        if (track.canProvideSampleCursors) {
            AVSampleCursor *c = [track makeSampleCursorAtFirstSampleInDecodeOrder];
            if (c) {
                int64_t n = 1;
                while ([c stepInDecodeOrderByCount:1] == 1) n++;
                out->stored_samples = n;
            }
        }
        return 0;
    }
}

int32_t bugle_video_sync_at(const char *path, double seconds) {
    @autoreleasepool {
        clear_error();
        if (!path || !isfinite(seconds)) { set_error(@"sync check: invalid argument"); return -1; }
        AVURLAsset *asset = precise_asset(path);
        AVAssetTrack *track = first_video_track(asset);
        if (!track) { set_error(@"%s has no video track", path); return -1; }
        if (!track.canProvideSampleCursors) return 0;
        CMTime t = CMTimeMakeWithSeconds(seconds, 60000);
        CMTime mt = media_time_for(track, t);
        AVSampleCursor *c = [track makeSampleCursorWithPresentationTimeStamp:mt];
        if (!c) return 0;
        // The cursor lands on the sample shown at `mt`; a cut is clean when
        // that sample is a full sync sample starting (within 1 ms) at the cut.
        double delta = fabs(CMTimeGetSeconds(CMTimeSubtract(c.presentationTimeStamp, mt)));
        return (c.currentSampleSyncInfo.sampleIsFullSync && delta < 0.001) ? 1 : 0;
    }
}

static NSString *edit_preset_name(int32_t preset) {
    switch (preset) {
        case 0: return AVAssetExportPresetPassthrough;
        case 1: return AVAssetExportPresetHighestQuality;
        case 2: return AVAssetExportPresetHEVCHighestQuality;
        case 3: return AVAssetExportPreset1920x1080;
        case 4: return AVAssetExportPreset1280x720;
        case 5: return AVAssetExportPreset960x540;
        default: return nil;
    }
}

/// Check `n` [start, end) ranges (pairs of seconds): finite, in order and
/// not overlapping. Sets the error and returns NO otherwise.
static BOOL valid_ranges(const double *ranges, int32_t n) {
    if (!ranges || n < 1) { set_error(@"no time range"); return NO; }
    for (int32_t i = 0; i < n; i++) {
        double s = ranges[2 * i], e = ranges[2 * i + 1];
        if (!isfinite(s) || !isfinite(e) || s < 0 || e <= s || (i > 0 && s < ranges[2 * i - 1])) {
            set_error(@"invalid time range %g..%g s", s, e);
            return NO;
        }
    }
    return YES;
}

static CMTimeRange range_at(const double *ranges, int32_t i) {
    return CMTimeRangeFromTimeToTime(CMTimeMakeWithSeconds(ranges[2 * i], 60000), CMTimeMakeWithSeconds(ranges[2 * i + 1], 60000));
}

/// Append the enabled audio tracks of `asset` over each range to `comp`,
/// back to back from 0 (range i starts where the ranges before it end), one
/// composition track per source track. Returns the tracks added, or -1.
static int insert_audio_ranges(AVMutableComposition *comp, AVAsset *asset, const double *ranges, int32_t n) {
    int added = 0;
    NSError *err = nil;
    for (AVAssetTrack *t in [asset tracksWithMediaType:AVMediaTypeAudio]) {
        if (!t.enabled) continue;
        AVMutableCompositionTrack *ca = nil;
        CMTime at = kCMTimeZero;
        for (int32_t i = 0; i < n; i++) {
            CMTimeRange want = range_at(ranges, i);
            CMTimeRange have = CMTimeRangeGetIntersection(want, t.timeRange);
            if (!CMTIMERANGE_IS_INVALID(have) && !CMTIMERANGE_IS_EMPTY(have)) {
                if (!ca) ca = [comp addMutableTrackWithMediaType:AVMediaTypeAudio preferredTrackID:kCMPersistentTrackID_Invalid];
                if (![ca insertTimeRange:have ofTrack:t atTime:CMTimeAdd(at, CMTimeSubtract(have.start, want.start)) error:&err]) {
                    set_error(@"could not copy an audio track: %@", err.localizedDescription);
                    return -1;
                }
            }
            at = CMTimeAdd(at, want.duration);
        }
        if (ca) added++;
    }
    return added;
}

/// The video track and enabled audio tracks of `asset` over each range,
/// back to back from 0. NULL (error set) on failure.
static AVMutableComposition *spliced_composition(AVAsset *asset, const double *ranges, int32_t n) {
    AVAssetTrack *vt = first_video_track(asset);
    AVMutableComposition *comp = [AVMutableComposition composition];
    AVMutableCompositionTrack *cv = [comp addMutableTrackWithMediaType:AVMediaTypeVideo preferredTrackID:kCMPersistentTrackID_Invalid];
    NSError *err = nil;
    CMTime at = kCMTimeZero;
    for (int32_t i = 0; i < n; i++) {
        CMTimeRange want = range_at(ranges, i);
        CMTimeRange have = CMTimeRangeGetIntersection(want, vt.timeRange);
        if (CMTIMERANGE_IS_INVALID(have) || CMTIMERANGE_IS_EMPTY(have)) {
            set_error(@"the range %g..%g s has no video", ranges[2 * i], ranges[2 * i + 1]);
            return nil;
        }
        if (![cv insertTimeRange:have ofTrack:vt atTime:CMTimeAdd(at, CMTimeSubtract(have.start, want.start)) error:&err]) {
            set_error(@"could not copy the video: %@", err.localizedDescription);
            return nil;
        }
        at = CMTimeAdd(at, want.duration);
    }
    cv.preferredTransform = vt.preferredTransform;
    if (insert_audio_ranges(comp, asset, ranges, n) < 0) return nil;
    return comp;
}

int32_t bugle_export_video(const char *in_path, const char *out_path, const double *ranges, int32_t n_ranges, int32_t preset,
                           int32_t mp4, int32_t fps, double timeout) {
    @autoreleasepool {
        clear_error();
        NSString *name = edit_preset_name(preset);
        if (!name) { set_error(@"unknown export preset %d", preset); return -1; }
        if (!in_path || !out_path) { set_error(@"export: NULL path"); return -1; }
        if (!valid_ranges(ranges, n_ranges)) return -1;
        if (fps < 0 || fps > 240) { set_error(@"invalid frame rate %d", fps); return -1; }
        if (fps > 0 && preset == 0) { set_error(@"a frame rate needs a re-encoding preset"); return -1; }
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        AVURLAsset *source = precise_asset(in_path);
        if (!first_video_track(source)) { set_error(@"the source has no video track"); return -1; }
        // One range is exported from the file itself; several are spliced
        // into a composition first, which is then exported whole.
        AVAsset *asset = source;
        if (n_ranges > 1) {
            AVMutableComposition *comp = spliced_composition(source, ranges, n_ranges);
            if (!comp) return -1;
            asset = comp;
        }
        // Passthrough is never listed as "compatible" but always works.
        if (preset != 0 && ![[AVAssetExportSession exportPresetsCompatibleWithAsset:asset] containsObject:name]) {
            set_error(@"export preset %@ is not available for this source", name);
            return -1;
        }
        AVAssetExportSession *s = [[AVAssetExportSession alloc] initWithAsset:asset presetName:name];
        if (!s) { set_error(@"could not create export session"); return -1; }
        AVFileType type = mp4 ? AVFileTypeMPEG4 : AVFileTypeQuickTimeMovie;
        if (![s.supportedFileTypes containsObject:type]) {
            set_error(@"export preset %@ cannot write %@", name, type);
            return -1;
        }
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        s.outputURL = out;
        s.outputFileType = type;
        s.shouldOptimizeForNetworkUse = YES;
        if (n_ranges == 1) s.timeRange = range_at(ranges, 0);
        if (preset != 0) {
            // Re-encodes always render through a video composition: without
            // one the encoder starts at the keyframe before the cut and hides
            // the extra frames with an edit list, leaving cut footage in the
            // file. The composition renders only frames inside the range.
            AVMutableVideoComposition *vc = [AVMutableVideoComposition videoCompositionWithPropertiesOfAsset:asset];
            if (fps > 0) {
                vc.frameDuration = CMTimeMake(1, fps);
                // Without this the source track's own frame timing wins and
                // frameDuration is ignored.
                vc.sourceTrackIDForFrameTiming = kCMPersistentTrackID_Invalid;
            }
            s.videoComposition = vc;
        }
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [s exportAsynchronouslyWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        if (!wait_bounded(sem, timeout)) {
            [s cancelExport];
            wait_bounded(sem, 10);
            [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
            set_error(@"export timed out after %.0f s and was cancelled", timeout);
            return -1;
        }
        if (s.status != AVAssetExportSessionStatusCompleted) {
            set_error(@"export failed: %@", s.error.localizedDescription ?: @"unknown error");
            [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
            return -1;
        }
        return 0;
    }
}

void *bugle_frames_open(const char *path, double tolerance) {
    @autoreleasepool {
        clear_error();
        if (!path) { set_error(@"frames: NULL path"); return NULL; }
        AVURLAsset *asset = precise_asset(path);
        if (!first_video_track(asset)) { set_error(@"the source has no video track"); return NULL; }
        AVAssetImageGenerator *gen = [[AVAssetImageGenerator alloc] initWithAsset:asset];
        gen.appliesPreferredTrackTransform = YES;
        CMTime tol = (isfinite(tolerance) && tolerance > 0) ? CMTimeMakeWithSeconds(tolerance, 60000) : kCMTimeZero;
        gen.requestedTimeToleranceBefore = tol;
        gen.requestedTimeToleranceAfter = tol;
        return (__bridge_retained void *)gen;
    }
}

bugle_image_t bugle_frames_at(void *handle, double seconds) {
    @autoreleasepool {
        clear_error();
        if (!handle || !isfinite(seconds)) { set_error(@"frames: invalid handle or time"); return (bugle_image_t){0}; }
        AVAssetImageGenerator *gen = (__bridge AVAssetImageGenerator *)handle;
        NSError *err = nil;
        CGImageRef img = [gen copyCGImageAtTime:CMTimeMakeWithSeconds(fmax(0, seconds), 60000) actualTime:NULL error:&err];
        if (!img) { set_error(@"frame extraction at %.3fs failed: %@", seconds, err.localizedDescription); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

/// Decode frames scaled down to fit `max_w` x `max_h` pixels (aspect kept;
/// 0 x 0 for full size). Much cheaper than scaling full frames afterwards.
void bugle_frames_set_max_size(void *handle, uint32_t max_w, uint32_t max_h) {
    if (!handle) return;
    @autoreleasepool {
        AVAssetImageGenerator *gen = (__bridge AVAssetImageGenerator *)handle;
        gen.maximumSize = CGSizeMake(max_w, max_h);
    }
}

void bugle_frames_close(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        AVAssetImageGenerator *gen = (__bridge_transfer AVAssetImageGenerator *)handle;
        (void)gen;
    }
}

static NSDictionary *bt709_properties(void) {
    return @{
        AVVideoColorPrimariesKey: AVVideoColorPrimaries_ITU_R_709_2,
        AVVideoTransferFunctionKey: AVVideoTransferFunction_ITU_R_709_2,
        AVVideoYCbCrMatrixKey: AVVideoYCbCrMatrix_ITU_R_709_2,
    };
}

@interface BugleFrameWriter : NSObject
@property(strong) AVAssetWriter *writer;
@property(strong) AVAssetWriterInput *input;
@property(strong) AVAssetWriterInputPixelBufferAdaptor *adaptor;
@property(strong) NSURL *url;
@property int32_t width, height, fps;
@property int64_t frames;
@end
@implementation BugleFrameWriter
@end

void *bugle_writer_open(const char *path, int32_t w, int32_t h, int32_t fps, int32_t mp4, int32_t hevc) {
    @autoreleasepool {
        clear_error();
        if (!path) { set_error(@"writer: NULL path"); return NULL; }
        if (w < 2 || h < 2 || w > BUGLE_MAX_PX || h > BUGLE_MAX_PX || (w & 1) || (h & 1)) {
            set_error(@"writer: frame size %dx%d must be even and within 2...%d", w, h, BUGLE_MAX_PX);
            return NULL;
        }
        if (fps < 1 || fps > 120) { set_error(@"writer: invalid frame rate %d", fps); return NULL; }
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        [[NSFileManager defaultManager] removeItemAtURL:url error:nil];
        NSError *err = nil;
        AVAssetWriter *writer = [[AVAssetWriter alloc] initWithURL:url
                                                          fileType:(mp4 ? AVFileTypeMPEG4 : AVFileTypeQuickTimeMovie)
                                                             error:&err];
        if (!writer) { set_error(@"AVAssetWriter: %@", err.localizedDescription); return NULL; }
        writer.shouldOptimizeForNetworkUse = YES;
        // Same budget as the recorder: ~0.1 bit per pixel per frame.
        double bitrate = fmax(1.0e6, fmin(40.0e6, (double)w * (double)h * (double)fps * 0.1));
        NSMutableDictionary *compression = [@{
            AVVideoAverageBitRateKey: @((int64_t)bitrate),
            AVVideoExpectedSourceFrameRateKey: @(fps),
            AVVideoMaxKeyFrameIntervalKey: @(fps),
            AVVideoAllowFrameReorderingKey: @NO,
        } mutableCopy];
        if (!hevc) compression[AVVideoProfileLevelKey] = AVVideoProfileLevelH264HighAutoLevel;
        NSDictionary *video = @{
            AVVideoCodecKey: hevc ? AVVideoCodecTypeHEVC : AVVideoCodecTypeH264,
            AVVideoWidthKey: @(w),
            AVVideoHeightKey: @(h),
            AVVideoCompressionPropertiesKey: compression,
            // Tagged explicitly: untagged small movies are read back with
            // the SD (601) matrix and shift colour on every round trip.
            AVVideoColorPropertiesKey: bt709_properties(),
        };
        AVAssetWriterInput *input = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:video];
        input.expectsMediaDataInRealTime = NO;
        NSDictionary *attrs = @{
            (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
            (id)kCVPixelBufferWidthKey: @(w),
            (id)kCVPixelBufferHeightKey: @(h),
        };
        AVAssetWriterInputPixelBufferAdaptor *adaptor =
            [AVAssetWriterInputPixelBufferAdaptor assetWriterInputPixelBufferAdaptorWithAssetWriterInput:input
                                                                             sourcePixelBufferAttributes:attrs];
        if (![writer canAddInput:input]) { set_error(@"writer refused the %@ video track", hevc ? @"HEVC" : @"H.264"); return NULL; }
        [writer addInput:input];
        if (![writer startWriting]) {
            set_error(@"could not start writing the movie: %@", writer.error.localizedDescription);
            [[NSFileManager defaultManager] removeItemAtURL:url error:nil];
            return NULL;
        }
        [writer startSessionAtSourceTime:kCMTimeZero];
        BugleFrameWriter *fw = [[BugleFrameWriter alloc] init];
        fw.writer = writer;
        fw.input = input;
        fw.adaptor = adaptor;
        fw.url = url;
        fw.width = w;
        fw.height = h;
        fw.fps = fps;
        return (__bridge_retained void *)fw;
    }
}

int32_t bugle_writer_append(void *handle, const uint8_t *rgba, size_t len, int32_t w, int32_t h) {
    @autoreleasepool {
        clear_error();
        if (!handle) { set_error(@"writer: NULL handle"); return -1; }
        BugleFrameWriter *fw = (__bridge BugleFrameWriter *)handle;
        if (!rgba || w != fw.width || h != fw.height || len < rgba_len(w, h)) {
            set_error(@"writer: frame %dx%d (%zu bytes) does not match %dx%d", w, h, len, fw.width, fw.height);
            return -1;
        }
        // Not real time: wait (bounded) for the encoder to drain.
        for (int i = 0; !fw.input.readyForMoreMediaData; i++) {
            if (fw.writer.status != AVAssetWriterStatusWriting || i > 2000) {
                set_error(@"writer stalled: %@", fw.writer.error.localizedDescription ?: @"encoder not ready");
                return -1;
            }
            usleep(5000);
        }
        CVPixelBufferRef pb = NULL;
        CVReturn rc = fw.adaptor.pixelBufferPool
                          ? CVPixelBufferPoolCreatePixelBuffer(NULL, fw.adaptor.pixelBufferPool, &pb)
                          : CVPixelBufferCreate(NULL, w, h, kCVPixelFormatType_32BGRA, NULL, &pb);
        if (rc != kCVReturnSuccess || !pb) { set_error(@"writer: could not allocate a pixel buffer (%d)", rc); return -1; }
        CVPixelBufferLockBaseAddress(pb, 0);
        uint8_t *base = CVPixelBufferGetBaseAddress(pb);
        size_t stride = CVPixelBufferGetBytesPerRow(pb);
        for (int32_t y = 0; y < h; y++) {
            const uint8_t *src = rgba + (size_t)y * (size_t)w * 4;
            uint8_t *dst = base + (size_t)y * stride;
            for (int32_t x = 0; x < w; x++) {
                dst[x * 4 + 0] = src[x * 4 + 2];
                dst[x * 4 + 1] = src[x * 4 + 1];
                dst[x * 4 + 2] = src[x * 4 + 0];
                dst[x * 4 + 3] = 255;
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, 0);
        BOOL ok = [fw.adaptor appendPixelBuffer:pb withPresentationTime:CMTimeMake(fw.frames, fw.fps)];
        CVPixelBufferRelease(pb);
        if (!ok) { set_error(@"writer: appending frame %lld failed: %@", fw.frames, fw.writer.error.localizedDescription); return -1; }
        fw.frames += 1;
        return 0;
    }
}

int32_t bugle_writer_finish(void *handle, double timeout) {
    @autoreleasepool {
        clear_error();
        if (!handle) { set_error(@"writer: NULL handle"); return -1; }
        BugleFrameWriter *fw = (__bridge_transfer BugleFrameWriter *)handle;
        if (fw.frames == 0) {
            [fw.writer cancelWriting];
            [[NSFileManager defaultManager] removeItemAtURL:fw.url error:nil];
            set_error(@"writer: no frames were written");
            return -1;
        }
        [fw.input markAsFinished];
        // The last frame lasts a full frame interval.
        [fw.writer endSessionAtSourceTime:CMTimeMake(fw.frames, fw.fps)];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [fw.writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        if (!wait_bounded(sem, timeout)) {
            [fw.writer cancelWriting];
            wait_bounded(sem, 10);
            [[NSFileManager defaultManager] removeItemAtURL:fw.url error:nil];
            set_error(@"finalising the movie timed out after %.0f s", timeout);
            return -1;
        }
        if (fw.writer.status != AVAssetWriterStatusCompleted) {
            set_error(@"finalising the movie failed: %@", fw.writer.error.localizedDescription);
            [[NSFileManager defaultManager] removeItemAtURL:fw.url error:nil];
            return -1;
        }
        return 0;
    }
}

void bugle_writer_cancel(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        BugleFrameWriter *fw = (__bridge_transfer BugleFrameWriter *)handle;
        [fw.writer cancelWriting];
        [[NSFileManager defaultManager] removeItemAtURL:fw.url error:nil];
    }
}

// Sequential frame reader: AVAssetReader decodes the video track over a
// time range in presentation order (one decode per frame, unlike the
// random-access generator above). The first frame may start before the range
// (it is the one shown at its start). The preferred transform is not applied.
@interface BugleSeqReader : NSObject
@property(strong) AVAssetReader *reader;
@property(strong) AVAssetReaderTrackOutput *output;
@end
@implementation BugleSeqReader
@end

void *bugle_seq_open(const char *path, double start, double end) {
    @autoreleasepool {
        clear_error();
        if (!path) { set_error(@"reader: NULL path"); return NULL; }
        AVURLAsset *asset = precise_asset(path);
        AVAssetTrack *track = first_video_track(asset);
        if (!track) { set_error(@"the source has no video track"); return NULL; }
        NSError *err = nil;
        AVAssetReader *reader = [[AVAssetReader alloc] initWithAsset:asset error:&err];
        if (!reader) { set_error(@"could not read %s: %@", path, err.localizedDescription); return NULL; }
        NSDictionary *settings = @{
            (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
            AVVideoColorPropertiesKey: bt709_properties(),
        };
        AVAssetReaderTrackOutput *out = [AVAssetReaderTrackOutput assetReaderTrackOutputWithTrack:track outputSettings:settings];
        out.alwaysCopiesSampleData = NO;
        if (![reader canAddOutput:out]) { set_error(@"could not decode the video track"); return NULL; }
        [reader addOutput:out];
        if (isfinite(start) && isfinite(end) && start >= 0 && end > start) {
            reader.timeRange = CMTimeRangeFromTimeToTime(CMTimeMakeWithSeconds(start, 60000), CMTimeMakeWithSeconds(end, 60000));
        }
        if (![reader startReading]) { set_error(@"could not read %s: %@", path, reader.error.localizedDescription); return NULL; }
        BugleSeqReader *r = [[BugleSeqReader alloc] init];
        r.reader = reader;
        r.output = out;
        return (__bridge_retained void *)r;
    }
}

bugle_image_t bugle_seq_next(void *handle, double *pts) {
    @autoreleasepool {
        clear_error();
        if (!handle || !pts) { set_error(@"reader: NULL argument"); return (bugle_image_t){0}; }
        BugleSeqReader *r = (__bridge BugleSeqReader *)handle;
        for (;;) {
            CMSampleBufferRef sb = [r.output copyNextSampleBuffer];
            if (!sb) {
                if (r.reader.status == AVAssetReaderStatusFailed) {
                    set_error(@"decoding failed: %@", r.reader.error.localizedDescription ?: @"unknown error");
                }
                return (bugle_image_t){0};
            }
            CVImageBufferRef ib = CMSampleBufferGetImageBuffer(sb);
            if (!ib) { CFRelease(sb); continue; }
            *pts = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sb));
            CVPixelBufferLockBaseAddress(ib, kCVPixelBufferLock_ReadOnly);
            size_t w = CVPixelBufferGetWidth(ib), h = CVPixelBufferGetHeight(ib), stride = CVPixelBufferGetBytesPerRow(ib);
            const uint8_t *base = CVPixelBufferGetBaseAddress(ib);
            size_t len = rgba_len((int64_t)w, (int64_t)h);
            uint8_t *buf = (len && base) ? malloc(len) : NULL;
            if (buf) {
                for (size_t y = 0; y < h; y++) {
                    const uint8_t *src = base + y * stride;
                    uint8_t *dst = buf + y * w * 4;
                    for (size_t x = 0; x < w; x++) {
                        dst[x * 4 + 0] = src[x * 4 + 2];
                        dst[x * 4 + 1] = src[x * 4 + 1];
                        dst[x * 4 + 2] = src[x * 4 + 0];
                        dst[x * 4 + 3] = 255;
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(ib, kCVPixelBufferLock_ReadOnly);
            CFRelease(sb);
            if (!buf) { set_error(@"reader: could not convert a %zux%zu frame", w, h); return (bugle_image_t){0}; }
            return (bugle_image_t){buf, (int32_t)w, (int32_t)h};
        }
    }
}

void bugle_seq_close(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        BugleSeqReader *r = (__bridge_transfer BugleSeqReader *)handle;
        [r.reader cancelReading];
    }
}

// Copy the video track of `video_path` and the audio tracks of `audio_path`
// over each [start, end) range (back to back from 0) into one movie, without
// re-encoding. Returns 1 (nothing written) when the audio source has no
// audio there.
int32_t bugle_mux_audio(const char *video_path, const char *audio_path, const double *ranges, int32_t n_ranges,
                        const char *out_path, int32_t mp4, double timeout) {
    @autoreleasepool {
        clear_error();
        if (!video_path || !audio_path || !out_path) { set_error(@"mux: NULL path"); return -1; }
        if (!valid_ranges(ranges, n_ranges)) return -1;
        AVURLAsset *v = precise_asset(video_path), *a = precise_asset(audio_path);
        AVAssetTrack *vt = first_video_track(v);
        if (!vt) { set_error(@"%s has no video track", video_path); return -1; }
        AVMutableComposition *comp = [AVMutableComposition composition];
        NSError *err = nil;
        AVMutableCompositionTrack *cv = [comp addMutableTrackWithMediaType:AVMediaTypeVideo preferredTrackID:kCMPersistentTrackID_Invalid];
        if (![cv insertTimeRange:vt.timeRange ofTrack:vt atTime:kCMTimeZero error:&err]) {
            set_error(@"mux: could not copy the video: %@", err.localizedDescription);
            return -1;
        }
        cv.preferredTransform = vt.preferredTransform;
        int added = insert_audio_ranges(comp, a, ranges, n_ranges);
        if (added < 0) return -1;
        if (added == 0) return 1;
        AVAssetExportSession *x = [[AVAssetExportSession alloc] initWithAsset:comp presetName:AVAssetExportPresetPassthrough];
        if (!x) { set_error(@"mux: could not create export session"); return -1; }
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        x.outputURL = out;
        x.outputFileType = mp4 ? AVFileTypeMPEG4 : AVFileTypeQuickTimeMovie;
        x.shouldOptimizeForNetworkUse = YES;
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [x exportAsynchronouslyWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        if (!wait_bounded(sem, timeout)) {
            [x cancelExport];
            wait_bounded(sem, 10);
            [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
            set_error(@"mux timed out after %.0f s and was cancelled", timeout);
            return -1;
        }
        if (x.status != AVAssetExportSessionStatusCompleted) {
            set_error(@"mux failed: %@", x.error.localizedDescription ?: @"unknown error");
            [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
            return -1;
        }
        return 0;
    }
}

void bugle_play_sound(const char *name) {
    @autoreleasepool {
        NSSound *s = [NSSound soundNamed:[NSString stringWithUTF8String:name]];
        [s play];
    }
}

void bugle_open_path(const char *path) {
    @autoreleasepool {
        [[NSWorkspace sharedWorkspace] openURL:[NSURL fileURLWithPath:[NSString stringWithUTF8String:path]]];
    }
}

// ---------------------------------------------------------------------------
// Video recording — SCStream (screen + system audio) and an optional
// AVCaptureSession microphone, written by AVAssetWriter straight to .mp4
// (H.264 + AAC). This is the only path that can capture system audio.
//
// Timing: ScreenCaptureKit stamps samples with the host clock; microphone
// buffers are converted from the capture session's clock to the host clock
// so the three tracks line up. The writer session starts when the stream
// starts. A static screen's first frame carries the time it was last
// rendered, which can be well before the stream started, so earlier frames
// are clamped to the session start. On stop the last frame is re-appended at
// the stop time so the video track covers the whole recording.
// ---------------------------------------------------------------------------

// Live level of one audio source (bugle_level_t), for the menu-bar meter and
// record_status. Measured as the samples arrive (before any gain or ducking,
// which happen after the recording stops). `peak` holds the largest magnitude
// and falls at BUGLE_PEAK_FALL_DB_PER_S once the signal drops; `mean_square`
// is averaged with time constant BUGLE_RMS_TAU_S. Reads decay both to the
// current time without changing them, so any number of readers see the same
// values. A test checks these match bugle_core::audio_level's constants.
#define BUGLE_PEAK_FALL_DB_PER_S 20.0
#define BUGLE_RMS_TAU_S 0.3

static double host_seconds(void) {
    return CMTimeGetSeconds(CMClockGetTime(CMClockGetHostTimeClock()));
}

// Peak magnitude and sum of squares of a linear-PCM sample buffer (float32,
// float64, int16 or int32; interleaved or not); 0 for anything else.
int32_t bugle_audio_buffer_stats(const void *buffer, double *peak, double *sumSquares, size_t *count, double *seconds) {
    CMSampleBufferRef sb = (CMSampleBufferRef)buffer;
    CMFormatDescriptionRef fd = CMSampleBufferGetFormatDescription(sb);
    const AudioStreamBasicDescription *asbd = fd ? CMAudioFormatDescriptionGetStreamBasicDescription(fd) : NULL;
    if (!asbd || asbd->mFormatID != kAudioFormatLinearPCM || (asbd->mFormatFlags & kAudioFormatFlagIsBigEndian)) return 0;
    BOOL isFloat = (asbd->mFormatFlags & kAudioFormatFlagIsFloat) != 0;
    BOOL isSigned = (asbd->mFormatFlags & kAudioFormatFlagIsSignedInteger) != 0;
    UInt32 bits = asbd->mBitsPerChannel;
    if (isFloat ? (bits != 32 && bits != 64) : (!isSigned || (bits != 16 && bits != 32))) return 0;
    if (asbd->mSampleRate <= 0) return 0;
    size_t need = 0;
    if (CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, &need, NULL, 0, NULL, NULL, 0, NULL) != noErr || need == 0) return 0;
    AudioBufferList *abl = malloc(need);
    if (!abl) return 0;
    CMBlockBufferRef block = NULL;
    OSStatus st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb, NULL, abl, need, NULL, NULL, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &block);
    if (st != noErr) { free(abl); return 0; }
    double p = 0, ss = 0;
    size_t n = 0;
    for (UInt32 b = 0; b < abl->mNumberBuffers; b++) {
        const void *data = abl->mBuffers[b].mData;
        size_t len = abl->mBuffers[b].mDataByteSize / (bits / 8);
        if (!data) continue;
        for (size_t i = 0; i < len; i++) {
            double v;
            if (isFloat) v = bits == 32 ? ((const float *)data)[i] : ((const double *)data)[i];
            else v = bits == 16 ? ((const int16_t *)data)[i] / 32768.0 : ((const int32_t *)data)[i] / 2147483648.0;
            if (!isfinite(v)) continue;
            double a = fabs(v);
            if (a > p) p = a;
            ss += v * v;
        }
        n += len;
    }
    if (block) CFRelease(block);
    free(abl);
    *peak = p;
    *sumSquares = ss;
    *count = n;
    *seconds = (double)CMSampleBufferGetNumSamples(sb) / asbd->mSampleRate;
    return 1;
}

void bugle_level_constants(double *out) {
    out[0] = BUGLE_PEAK_FALL_DB_PER_S;
    out[1] = BUGLE_RMS_TAU_S;
}

// Each buffer's samples stand for the `seconds` before it arrived, so the
// average decays only over time no buffer covers (a gap, or the wait after
// the last one); the peak falls for all the time since it was set.
void bugle_level_add(bugle_level_t *l, double now, double peak, double mean_square, double seconds) {
    seconds = fmax(0, seconds);
    l->unreadable = 0;
    if (!l->seen) {
        *l = (bugle_level_t){.seen = 1, .updated = now, .covers = seconds, .peak = peak, .mean_square = mean_square};
        return;
    }
    double dt = fmax(0, now - l->updated);
    l->peak = fmax(l->peak * pow(10.0, -BUGLE_PEAK_FALL_DB_PER_S * dt / 20.0), peak);
    double ms = l->mean_square * exp(-fmax(0, dt - seconds) / BUGLE_RMS_TAU_S);
    // The buffer's own mean square enters the average with the weight of its duration.
    l->mean_square = ms + (1.0 - exp(-seconds / BUGLE_RMS_TAU_S)) * (mean_square - ms);
    l->updated = now;
    l->covers = seconds;
}

bugle_level_t bugle_level_at(const bugle_level_t *in, double now) {
    bugle_level_t l = *in;
    if (!l.seen) return l;
    double dt = fmax(0, now - l.updated);
    l.peak *= pow(10.0, -BUGLE_PEAK_FALL_DB_PER_S * dt / 20.0);
    l.mean_square *= exp(-fmax(0, dt - l.covers) / BUGLE_RMS_TAU_S);
    l.covers = fmax(0, l.covers - dt);
    l.updated = now;
    return l;
}

@interface BugleSckRecorder : NSObject <SCStreamOutput, SCStreamDelegate, AVCaptureAudioDataOutputSampleBufferDelegate>
@property (nonatomic, strong) SCStream *stream;
@property (nonatomic, strong) AVAssetWriter *writer;
@property (nonatomic, strong) AVAssetWriterInput *videoInput;
@property (nonatomic, strong) AVAssetWriterInputPixelBufferAdaptor *videoAdaptor;
@property (nonatomic, strong) AVAssetWriterInput *audioInput;   // system audio
@property (nonatomic, strong) AVAssetWriterInput *micInput;     // microphone
@property (nonatomic, strong) AVCaptureSession *micSession;
@property (nonatomic, strong) dispatch_queue_t videoQueue;
@property (nonatomic, strong) dispatch_queue_t audioQueue;
@property (nonatomic, strong) dispatch_queue_t micQueue;
@property (nonatomic, strong) NSLock *lock;        // guards the fields below
@property (nonatomic) BOOL sessionStarted;
@property (nonatomic) CMTime sessionStart;
@property (nonatomic) BOOL stopping;
@property (nonatomic) BOOL finished;
@property (nonatomic) int32_t result;
@property (nonatomic) int64_t frames;
@property (nonatomic) int32_t fps;
@property (nonatomic) CVPixelBufferRef lastFrame;  // retained
@property (nonatomic) CMTime lastPts;
@property (nonatomic, strong) NSString *failure;   // guarded by `lock`
@property (nonatomic) BOOL captureRequested;        // startCapture was called
@property (nonatomic, strong) NSMutableArray<NSString *> *warnings;
@property (nonatomic) bugle_level_t systemLevel;   // guarded by `lock`
@property (nonatomic) bugle_level_t micLevel;      // guarded by `lock`
@end

@implementation BugleSckRecorder

- (instancetype)init {
    if ((self = [super init])) {
        _videoQueue = dispatch_queue_create("bugle.sck.video", DISPATCH_QUEUE_SERIAL);
        _audioQueue = dispatch_queue_create("bugle.sck.audio", DISPATCH_QUEUE_SERIAL);
        _micQueue = dispatch_queue_create("bugle.sck.mic", DISPATCH_QUEUE_SERIAL);
        _lock = [[NSLock alloc] init];
        _warnings = [NSMutableArray array];
        _sessionStart = kCMTimeInvalid;
        _lastPts = kCMTimeInvalid;
    }
    return self;
}

- (void)dealloc {
    if (_lastFrame) CVPixelBufferRelease(_lastFrame);
}

- (void)warn:(NSString *)fmt, ... {
    va_list ap;
    va_start(ap, fmt);
    NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
    [self.lock lock];
    [self.warnings addObject:s];
    [self.lock unlock];
}

// Caller holds `lock`.
- (void)noteWriterFailure {
    if (self.writer.status == AVAssetWriterStatusFailed && !self.failure) {
        self.failure = self.writer.error.localizedDescription ?: @"asset writer failed";
    }
}

// --- SCStreamOutput ---------------------------------------------------------

- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sb ofType:(SCStreamOutputType)type {
    if (!sb || !CMSampleBufferDataIsReady(sb)) return;
    if (type == SCStreamOutputTypeScreen) {
        [self handleScreen:sb];
    } else if (type == SCStreamOutputTypeAudio) {
        [self meter:sb mic:NO];
        [self handleAudio:sb input:self.audioInput];
    }
}

- (void)meter:(CMSampleBufferRef)sb mic:(BOOL)mic {
    double peak = 0, sumSquares = 0, seconds = 0;
    size_t count = 0;
    BOOL ok = bugle_audio_buffer_stats(sb, &peak, &sumSquares, &count, &seconds) != 0;
    if (ok && count == 0) return;
    double now = host_seconds();
    [self.lock lock];
    bugle_level_t l = mic ? self.micLevel : self.systemLevel;
    // A format the meter cannot read: report the source as unmetered rather
    // than silent.
    if (ok) bugle_level_add(&l, now, peak, sumSquares / (double)count, seconds);
    else l.unreadable = 1;
    if (mic) self.micLevel = l; else self.systemLevel = l;
    [self.lock unlock];
}

- (void)handleScreen:(CMSampleBufferRef)sb {
    CFArrayRef atts = CMSampleBufferGetSampleAttachmentsArray(sb, false);
    if (atts && CFArrayGetCount(atts) > 0) {
        NSDictionary *d = (__bridge NSDictionary *)CFArrayGetValueAtIndex(atts, 0);
        // Idle/blank/suspended frames carry no image; a static screen sends
        // one complete frame and then only idle ones.
        NSNumber *status = d[SCStreamFrameInfoStatus];
        if (status && status.intValue != SCFrameStatusComplete && status.intValue != SCFrameStatusStarted) return;
    }
    CVPixelBufferRef px = CMSampleBufferGetImageBuffer(sb);
    if (!px) return;
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
    [self.lock lock];
    if (self.stopping || !self.sessionStarted) { [self.lock unlock]; return; }
    if (CMTimeCompare(pts, self.sessionStart) < 0) pts = self.sessionStart;
    if (CMTIME_IS_VALID(self.lastPts) && CMTimeCompare(pts, self.lastPts) <= 0) { [self.lock unlock]; return; }
    if (self.videoInput.readyForMoreMediaData) {
        if ([self.videoAdaptor appendPixelBuffer:px withPresentationTime:pts]) {
            self.frames++;
            if (self.lastFrame) CVPixelBufferRelease(self.lastFrame);
            self.lastFrame = CVPixelBufferRetain(px);
            self.lastPts = pts;
        } else {
            [self noteWriterFailure];
        }
    }
    [self.lock unlock];
}

- (void)handleAudio:(CMSampleBufferRef)sb input:(AVAssetWriterInput *)input {
    if (!input) return;
    [self.lock lock];
    if (self.stopping || !self.sessionStarted) { [self.lock unlock]; return; }
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
    if (CMTimeCompare(pts, self.sessionStart) < 0) { [self.lock unlock]; return; }
    if (input.readyForMoreMediaData) {
        if (![input appendSampleBuffer:sb]) [self noteWriterFailure];
    }
    [self.lock unlock];
}

// --- SCStreamDelegate -------------------------------------------------------

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    [self.lock lock];
    BOOL expected = self.stopping;
    [self.lock unlock];
    if (!expected) [self warn:@"capture stream stopped early: %@", error.localizedDescription];
}

// --- AVCaptureAudioDataOutputSampleBufferDelegate (microphone) --------------

- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sb
       fromConnection:(AVCaptureConnection *)connection {
    if (!sb || !CMSampleBufferDataIsReady(sb)) return;
    [self meter:sb mic:YES];
    CMClockRef sessionClock = self.micSession.synchronizationClock;
    CMClockRef host = CMClockGetHostTimeClock();
    if (sessionClock && !CFEqual(sessionClock, host)) {
        CMSampleTimingInfo t;
        t.duration = CMSampleBufferGetDuration(sb);
        t.presentationTimeStamp = CMSyncConvertTime(CMSampleBufferGetPresentationTimeStamp(sb), sessionClock, host);
        t.decodeTimeStamp = kCMTimeInvalid;
        CMSampleBufferRef adjusted = NULL;
        if (CMSampleBufferCreateCopyWithNewTiming(kCFAllocatorDefault, sb, 1, &t, &adjusted) == noErr && adjusted) {
            [self handleAudio:adjusted input:self.micInput];
            CFRelease(adjusted);
            return;
        }
    }
    [self handleAudio:sb input:self.micInput];
}

// --- Setup ------------------------------------------------------------------

- (BOOL)setupMicrophone:(NSDictionary *)audioSettings {
    AVAuthorizationStatus st = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
    if (st == AVAuthorizationStatusNotDetermined) {
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block BOOL granted = NO;
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL g) {
            granted = g;
            dispatch_semaphore_signal(sem);
        }];
        if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC)) != 0) granted = NO;
        st = granted ? AVAuthorizationStatusAuthorized : AVAuthorizationStatusDenied;
    }
    if (st != AVAuthorizationStatusAuthorized) {
        [self warn:@"microphone access not granted; recording without microphone"];
        return NO;
    }
    AVCaptureDevice *dev = [AVCaptureDevice defaultDeviceWithMediaType:AVMediaTypeAudio];
    if (!dev) { [self warn:@"no microphone found; recording without microphone"]; return NO; }
    NSError *err = nil;
    AVCaptureDeviceInput *in = [AVCaptureDeviceInput deviceInputWithDevice:dev error:&err];
    if (!in) { [self warn:@"microphone unavailable (%@); recording without microphone", err.localizedDescription]; return NO; }
    AVCaptureSession *session = [[AVCaptureSession alloc] init];
    AVCaptureAudioDataOutput *out = [[AVCaptureAudioDataOutput alloc] init];
    [out setSampleBufferDelegate:self queue:self.micQueue];
    if (![session canAddInput:in] || ![session canAddOutput:out]) {
        [self warn:@"microphone capture session could not be configured; recording without microphone"];
        return NO;
    }
    [session addInput:in];
    [session addOutput:out];
    AVAssetWriterInput *mic = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:audioSettings];
    mic.expectsMediaDataInRealTime = YES;
    if (![self.writer canAddInput:mic]) {
        [self warn:@"writer refused the microphone track; recording without microphone"];
        return NO;
    }
    [self.writer addInput:mic];
    self.micInput = mic;
    self.micSession = session;
    return YES;
}

- (BOOL)startWithFilter:(SCContentFilter *)filter
                 config:(SCStreamConfiguration *)cfg
                    url:(NSURL *)url
                    fps:(int32_t)fps
            systemAudio:(BOOL)systemAudio
             microphone:(BOOL)microphone {
    NSError *err = nil;
    [[NSFileManager defaultManager] removeItemAtURL:url error:nil];
    [[NSFileManager defaultManager] createDirectoryAtURL:url.URLByDeletingLastPathComponent
                             withIntermediateDirectories:YES attributes:nil error:nil];
    self.writer = [[AVAssetWriter alloc] initWithURL:url fileType:AVFileTypeMPEG4 error:&err];
    if (!self.writer) { set_error(@"AVAssetWriter: %@", err.localizedDescription); return NO; }
    self.writer.shouldOptimizeForNetworkUse = YES;
    self.fps = fps;

    int32_t w = (int32_t)cfg.width, h = (int32_t)cfg.height;
    // ~0.1 bit per pixel per frame: screen content is mostly static.
    double bitrate = fmax(1.0e6, fmin(40.0e6, (double)w * (double)h * (double)fps * 0.1));
    NSDictionary *video = @{
        AVVideoCodecKey: AVVideoCodecTypeH264,
        AVVideoWidthKey: @(w),
        AVVideoHeightKey: @(h),
        AVVideoCompressionPropertiesKey: @{
            AVVideoAverageBitRateKey: @((int64_t)bitrate),
            AVVideoExpectedSourceFrameRateKey: @(fps),
            AVVideoMaxKeyFrameIntervalKey: @(fps * 2),
            AVVideoProfileLevelKey: AVVideoProfileLevelH264HighAutoLevel,
            AVVideoAllowFrameReorderingKey: @NO,
        },
    };
    self.videoInput = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:video];
    self.videoInput.expectsMediaDataInRealTime = YES;
    self.videoAdaptor = [AVAssetWriterInputPixelBufferAdaptor assetWriterInputPixelBufferAdaptorWithAssetWriterInput:self.videoInput
                                                                                       sourcePixelBufferAttributes:nil];
    if (![self.writer canAddInput:self.videoInput]) { set_error(@"writer refused the H.264 video track"); return NO; }
    [self.writer addInput:self.videoInput];

    NSDictionary *audio = @{
        AVFormatIDKey: @(kAudioFormatMPEG4AAC),
        AVSampleRateKey: @48000,
        AVNumberOfChannelsKey: @2,
        AVEncoderBitRateKey: @160000,
    };
    if (systemAudio) {
        self.audioInput = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:audio];
        self.audioInput.expectsMediaDataInRealTime = YES;
        if (![self.writer canAddInput:self.audioInput]) { set_error(@"writer refused the AAC audio track"); return NO; }
        [self.writer addInput:self.audioInput];
    }
    if (microphone) [self setupMicrophone:audio];

    if (![self.writer startWriting]) {
        set_error(@"could not start writing %@: %@", url.path, self.writer.error.localizedDescription);
        return NO;
    }
    // Open the session before the stream starts: the first (and on a static
    // screen, only) complete frame can arrive before startCapture's
    // completion handler runs.
    [self.lock lock];
    self.sessionStart = CMClockGetTime(CMClockGetHostTimeClock());
    [self.writer startSessionAtSourceTime:self.sessionStart];
    self.sessionStarted = YES;
    [self.lock unlock];

    self.stream = [[SCStream alloc] initWithFilter:filter configuration:cfg delegate:self];
    if (![self.stream addStreamOutput:self type:SCStreamOutputTypeScreen sampleHandlerQueue:self.videoQueue error:&err]) {
        set_error(@"ScreenCaptureKit: %@", err.localizedDescription);
        return NO;
    }
    if (systemAudio &&
        ![self.stream addStreamOutput:self type:SCStreamOutputTypeAudio sampleHandlerQueue:self.audioQueue error:&err]) {
        set_error(@"ScreenCaptureKit audio: %@", err.localizedDescription);
        return NO;
    }
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block NSError *startErr = nil;
    self.captureRequested = YES;
    [self.stream startCaptureWithCompletionHandler:^(NSError *e) {
        startErr = e;
        dispatch_semaphore_signal(sem);
    }];
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) != 0) {
        set_error(@"ScreenCaptureKit: timed out starting the stream");
        return NO;
    }
    if (startErr) { set_error(@"ScreenCaptureKit: %@", startErr.localizedDescription); return NO; }
    if (self.micSession) [self.micSession startRunning];
    return YES;
}

// Stop capturing. Idempotent and serialised: a second caller (the auto-stop
// timer vs. an explicit stop) waits for the first to finish.
- (void)stopCapture {
    @synchronized (self) {
        [self.lock lock];
        BOOL already = self.stopping;
        self.stopping = YES;
        CMTime stopTime = CMClockGetTime(CMClockGetHostTimeClock());
        [self.lock unlock];
        if (already) return;
        if (self.micSession) [self.micSession stopRunning];
        if (self.stream && self.captureRequested) {
            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            [self.stream stopCaptureWithCompletionHandler:^(NSError *e) { dispatch_semaphore_signal(sem); }];
            dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC));
        }
        // Extend the last frame to the stop time so the video track covers
        // the whole recording even when the screen was static at the end.
        // The writer gives the final sample the previous sample's duration,
        // so append one frame an interval before the stop and one at it.
        if (self.sessionStarted && self.lastFrame && self.writer.status == AVAssetWriterStatusWriting) {
            CMTime interval = CMTimeMake(1, self.fps > 0 ? self.fps : 30);
            CMTime times[2] = { CMTimeSubtract(stopTime, interval), stopTime };
            for (int k = 0; k < 2; k++) {
                if (CMTimeCompare(times[k], self.lastPts) <= 0) continue;
                for (int i = 0; i < 50 && !self.videoInput.readyForMoreMediaData; i++) usleep(10 * 1000);
                if (!self.videoInput.readyForMoreMediaData) break;
                if ([self.videoAdaptor appendPixelBuffer:self.lastFrame withPresentationTime:times[k]]) self.lastPts = times[k];
            }
        }
    }
}

- (int32_t)finish {
    [self stopCapture];
    @synchronized (self) {
        if (self.finished) return self.result;
        self.finished = YES;
        self.result = -1;
        [self.lock lock];
        NSString *failure = self.failure;
        [self.lock unlock];
        if (!self.sessionStarted) {
            [self.writer cancelWriting];
            set_error(@"no frames were captured%@", failure ? [NSString stringWithFormat:@" (%@)", failure] : @"");
            return -1;
        }
        if (self.writer.status != AVAssetWriterStatusWriting) {
            set_error(@"recording failed: %@", failure ?: self.writer.error.localizedDescription ?: @"unknown writer state");
            return -1;
        }
        [self.videoInput markAsFinished];
        [self.audioInput markAsFinished];
        [self.micInput markAsFinished];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [self.writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC)) != 0) {
            set_error(@"timed out finalising the recording");
            return -1;
        }
        if (self.writer.status != AVAssetWriterStatusCompleted) {
            set_error(@"finalising the recording failed: %@", self.writer.error.localizedDescription);
            return -1;
        }
        self.result = 0;
        return 0;
    }
}

@end

int32_t bugle_sck_record_available(void) {
    return (sck_available() && NSClassFromString(@"SCStream") != nil &&
            [SCStreamConfiguration instancesRespondToSelector:@selector(setCapturesAudio:)]) ? 1 : 0;
}

static size_t even_px(double v) {
    if (!isfinite(v)) v = 2;
    if (v > BUGLE_MAX_PX) v = BUGLE_MAX_PX;
    long long n = llround(v);
    if (n < 2) n = 2;
    return (size_t)(n & ~1LL);
}

void *bugle_sck_record_start(double x, double y, double w, double h, uint32_t window_id, int32_t fps, int32_t retina,
                             int32_t system_audio, int32_t microphone, int32_t show_cursor, int32_t show_clicks,
                             double max_seconds, const char *out_path) {
    @autoreleasepool {
        clear_error();
        if (!bugle_sck_record_available()) { set_error(@"ScreenCaptureKit recording needs macOS 14 or later"); return NULL; }
        if (fps < 1) fps = 1;
        if (fps > 120) fps = 120;
        if (!out_path) { set_error(@"recording: no output path"); return NULL; }
        if (window_id == 0 && (!isfinite(x) || !isfinite(y) || !isfinite(w) || !isfinite(h) || w > BUGLE_MAX_PX ||
                               h > BUGLE_MAX_PX)) {
            set_error(@"invalid recording rect %g,%g %gx%g", x, y, w, h);
            return NULL;
        }
        SCShareableContent *content = sck_content();
        if (!content) return NULL;
        SCContentFilter *filter = nil;
        SCStreamConfiguration *cfg = [[SCStreamConfiguration alloc] init];
        if (window_id != 0) {
            for (SCWindow *win in content.windows) {
                if (win.windowID == window_id) {
                    filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:win];
                    break;
                }
            }
            if (!filter) { set_error(@"window %u is not capturable", window_id); return NULL; }
            double s = retina ? filter.pointPixelScale : 1.0;
            CGRect cr = filter.contentRect;
            cfg.width = even_px(cr.size.width * s);
            cfg.height = even_px(cr.size.height * s);
            cfg.scalesToFit = YES;
        } else {
            CGRect rect = CGRectMake(x, y, w, h);
            if (w < 2 || h < 2) { set_error(@"recording rect is empty"); return NULL; }
            CGDirectDisplayID did = display_for_rect(rect);
            CGRect db = CGDisplayBounds(did);
            for (SCDisplay *d in content.displays) {
                if (d.displayID == did) {
                    filter = [[SCContentFilter alloc] initWithDisplay:d excludingWindows:@[]];
                    break;
                }
            }
            if (!filter) { set_error(@"display %u is not capturable", did); return NULL; }
            double s = retina ? filter.pointPixelScale : 1.0;
            if (w * s > BUGLE_MAX_PX || h * s > BUGLE_MAX_PX) {
                set_error(@"recording size %.0fx%.0f px exceeds %d px per side", w * s, h * s, BUGLE_MAX_PX);
                return NULL;
            }
            cfg.sourceRect = CGRectMake(x - db.origin.x, y - db.origin.y, w, h);
            cfg.width = even_px(w * s);
            cfg.height = even_px(h * s);
        }
        cfg.minimumFrameInterval = CMTimeMake(1, fps);
        cfg.queueDepth = 8;
        cfg.showsCursor = show_cursor != 0;
        BOOL clicksUnsupported = NO;
        if (show_clicks) {
            // A selector check rather than `@available` (see sck_available).
            if ([cfg respondsToSelector:@selector(setShowMouseClicks:)]) {
                cfg.showMouseClicks = YES;
            } else {
                clicksUnsupported = YES;
            }
        }
        cfg.pixelFormat = kCVPixelFormatType_32BGRA;
        cfg.colorSpaceName = kCGColorSpaceSRGB;
        cfg.captureResolution = SCCaptureResolutionBest;
        cfg.shouldBeOpaque = YES;
        if (system_audio) {
            cfg.capturesAudio = YES;
            cfg.sampleRate = 48000;
            cfg.channelCount = 2;
            cfg.excludesCurrentProcessAudio = YES;
        }
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        BugleSckRecorder *rec = [[BugleSckRecorder alloc] init];
        if (clicksUnsupported) [rec warn:@"click highlighting needs macOS 15 with this recorder; recording without it"];
        if (![rec startWithFilter:filter config:cfg url:url fps:fps systemAudio:system_audio != 0 microphone:microphone != 0]) {
            // Whatever failed, make sure nothing keeps running: a stream whose
            // start was requested (even one that timed out and may still come
            // up) is stopped, and so is the microphone session.
            [rec stopCapture];
            [rec.writer cancelWriting];
            [[NSFileManager defaultManager] removeItemAtURL:url error:nil];
            return NULL;
        }
        if (max_seconds > 0) {
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(max_seconds * NSEC_PER_SEC)),
                           dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{ [rec stopCapture]; });
        }
        return (__bridge_retained void *)rec;
    }
}

double bugle_sck_record_session_start(void *handle) {
    if (!handle) return NAN;
    BugleSckRecorder *rec = (__bridge BugleSckRecorder *)handle;
    [rec.lock lock];
    double s = rec.sessionStarted ? CMTimeGetSeconds(rec.sessionStart) : NAN;
    [rec.lock unlock];
    return s;
}

int32_t bugle_sck_record_levels(void *handle, float *out) {
    if (!handle || !out) return 0;
    BugleSckRecorder *rec = (__bridge BugleSckRecorder *)handle;
    double now = host_seconds();
    [rec.lock lock];
    bugle_level_t sysNow = rec.systemLevel, micNow = rec.micLevel;
    int32_t present = (rec.audioInput && !sysNow.unreadable ? 1 : 0) | (rec.micInput && !micNow.unreadable ? 2 : 0);
    [rec.lock unlock];
    bugle_level_t sys = bugle_level_at(&sysNow, now);
    bugle_level_t mic = bugle_level_at(&micNow, now);
    out[0] = (float)sys.peak;
    out[1] = (float)sqrt(fmax(0, sys.mean_square));
    out[2] = (float)mic.peak;
    out[3] = (float)sqrt(fmax(0, mic.mean_square));
    return present;
}

int64_t bugle_sck_record_frames(void *handle) {
    if (!handle) return 0;
    BugleSckRecorder *rec = (__bridge BugleSckRecorder *)handle;
    [rec.lock lock];
    int64_t n = rec.frames;
    [rec.lock unlock];
    return n;
}

char *bugle_sck_record_warnings(void *handle) {
    if (!handle) return NULL;
    @autoreleasepool {
        BugleSckRecorder *rec = (__bridge BugleSckRecorder *)handle;
        [rec.lock lock];
        NSString *s = rec.warnings.count ? [rec.warnings componentsJoinedByString:@"; "] : nil;
        [rec.lock unlock];
        return s ? strdup(s.UTF8String) : NULL;
    }
}

int32_t bugle_sck_record_stop(void *handle) {
    if (!handle) return -1;
    @autoreleasepool {
        clear_error();
        BugleSckRecorder *rec = (__bridge_transfer BugleSckRecorder *)handle;
        return [rec finish];
    }
}
