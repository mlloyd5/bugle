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

// ---------------------------------------------------------------------------
// Errors / memory
// ---------------------------------------------------------------------------

static NSString *g_last_error = nil;

static void set_error(NSString *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    g_last_error = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
}

char *bugle_last_error(void) {
    if (!g_last_error) return NULL;
    return strdup(g_last_error.UTF8String);
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

static bugle_image_t cg_to_rgba(CGImageRef img) {
    bugle_image_t out = {0};
    if (!img) return out;
    size_t w = CGImageGetWidth(img), h = CGImageGetHeight(img);
    if (w == 0 || h == 0) return out;
    uint8_t *buf = calloc(w * h * 4, 1);
    if (!buf) return out;
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef ctx = CGBitmapContextCreate(buf, w, h, 8, w * 4, cs,
                                             kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
    CGColorSpaceRelease(cs);
    if (!ctx) { free(buf); return out; }
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

static CGImageRef cg_from_rgba(const uint8_t *rgba, int32_t w, int32_t h) {
    if (!rgba || w <= 0 || h <= 0) return NULL;
    CFDataRef d = CFDataCreate(NULL, rgba, (CFIndex)w * h * 4);
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

static void copy_str(char *dst, size_t cap, NSString *s) {
    if (!s) { dst[0] = 0; return; }
    strncpy(dst, s.UTF8String ?: "", cap - 1);
    dst[cap - 1] = 0;
}

int32_t bugle_list_windows(bugle_window_t *out, int32_t max, int32_t on_screen_only) {
    CGWindowListOption opt = kCGWindowListExcludeDesktopElements;
    if (on_screen_only) opt |= kCGWindowListOptionOnScreenOnly;
    CFArrayRef list = CGWindowListCopyWindowInfo(opt, kCGNullWindowID);
    if (!list) return -1;
    int32_t count = 0;
    NSArray *arr = CFBridgingRelease(list);
    for (NSDictionary *w in arr) {
        if (count >= max) break;
        bugle_window_t win;
        memset(&win, 0, sizeof win);
        win.id = [w[(__bridge NSString *)kCGWindowNumber] unsignedIntValue];
        win.pid = [w[(__bridge NSString *)kCGWindowOwnerPID] intValue];
        win.layer = [w[(__bridge NSString *)kCGWindowLayer] intValue];
        win.on_screen = [w[(__bridge NSString *)kCGWindowIsOnscreen] boolValue] ? 1 : 0;
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
    __block CGImageRef out = NULL;
    __block NSError *err = nil;
    [SCScreenshotManager captureImageWithFilter:filter
                                  configuration:cfg
                              completionHandler:^(CGImageRef img, NSError *e) {
        if (img) out = CGImageRetain(img);
        err = e;
        dispatch_semaphore_signal(sem);
    }];
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) != 0) {
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
        g_last_error = nil;
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
        if (!img) { if (!g_last_error) set_error(@"display %u could not be captured", display_id); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

bugle_image_t bugle_capture_area(double x, double y, double w, double h, int32_t show_cursor) {
    @autoreleasepool {
        g_last_error = nil;
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
        if (!img) { if (!g_last_error) set_error(@"area could not be captured"); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

bugle_image_t bugle_capture_window(uint32_t window_id, int32_t shadow, int32_t show_cursor) {
    @autoreleasepool {
        g_last_error = nil;
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
        if (!img) { if (!g_last_error) set_error(@"window %u could not be captured", window_id); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
    }
}

// ---------------------------------------------------------------------------
// OCR — VNRecognizeTextRequest (accurate,
// language correction, configurable languages).
// ---------------------------------------------------------------------------

int32_t bugle_ocr(const uint8_t *rgba, int32_t w, int32_t h, const char *langs_csv, int32_t accurate,
                  int32_t lang_correction, bugle_ocr_line_t **out_lines, int32_t *out_count) {
    @autoreleasepool {
        g_last_error = nil;
        *out_lines = NULL;
        *out_count = 0;
        CGImageRef img = cg_from_rgba(rgba, w, h);
        if (!img) { set_error(@"OCR: invalid image"); return -1; }
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
        bugle_ocr_line_t *lines = calloc(obs.count + 1, sizeof(bugle_ocr_line_t));
        int32_t n = 0;
        for (VNRecognizedTextObservation *o in obs) {
            VNRecognizedText *top = [o topCandidates:1].firstObject;
            if (!top) continue;
            CGRect bb = o.boundingBox;  // normalized, bottom-left origin
            lines[n].text = strdup(top.string.UTF8String);
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

int32_t bugle_register_translation(const uint8_t *ref_rgba, int32_t rw, int32_t rh, const uint8_t *flt_rgba,
                                   int32_t fw, int32_t fh, double *dx, double *dy) {
    @autoreleasepool {
        g_last_error = nil;
        CGImageRef ref = cg_from_rgba(ref_rgba, rw, rh);
        CGImageRef flt = cg_from_rgba(flt_rgba, fw, fh);
        if (!ref || !flt) { set_error(@"registration: invalid image"); return -1; }
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

typedef void (^BugleSelectCompletion)(BOOL ok, CGRect rect);

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
- (void)finishWithRect:(CGRect)rect ok:(BOOL)ok;
@end

static BugleSelectSession *g_select_session = nil;

@implementation BugleSelectView
- (BOOL)acceptsFirstResponder { return YES; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (void)resetCursorRects { [self addCursorRect:self.bounds cursor:[NSCursor crosshairCursor]]; }
- (void)drawRect:(NSRect)dirty {
    [[NSColor colorWithWhite:0 alpha:0.28] setFill];
    NSRectFill(self.bounds);
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
        [self.session finishWithRect:r ok:YES];
    }
    [self setNeedsDisplay:YES];
}
- (void)keyDown:(NSEvent *)e {
    if (e.keyCode == 53) { // Esc
        [self.session finishWithRect:CGRectZero ok:NO];
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
- (void)finishWithRect:(CGRect)rect ok:(BOOL)ok {
    if (self.finished) return;
    self.finished = YES;
    [NSCursor pop];
    for (NSWindow *win in self.windows) [win orderOut:nil];
    [self.windows removeAllObjects];
    BugleSelectCompletion done = self.completion;
    self.completion = nil;
    if (g_select_session == self) g_select_session = nil;
    // Give keyboard focus back to whatever the user was working in.
    [NSApp deactivate];
    if (done) done(ok, rect);
}
@end

/// Show the overlay on every screen. Returns NO when a selection is already
/// in progress (the completion is then never called).
static BOOL select_begin(BugleSelectCompletion completion) {
    if (g_select_session) return NO;
    NSApplication *app = [NSApplication sharedApplication];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    BugleSelectSession *session = [[BugleSelectSession alloc] init];
    session.windows = [NSMutableArray array];
    session.completion = completion;
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
    g_select_session = session;
    [app activateIgnoringOtherApps:YES];
    [[NSCursor crosshairCursor] push];
    return YES;
}

int32_t bugle_select_rect_blocking(double *x, double *y, double *w, double *h) {
    @autoreleasepool {
        __block BOOL ok = NO;
        __block CGRect r = CGRectZero;
        if (!select_begin(^(BOOL k, CGRect rect) {
                ok = k;
                r = rect;
                stop_app();
            })) {
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

int32_t bugle_select_rect_begin(bugle_select_callback_t cb, void *ctx) {
    @autoreleasepool {
        BOOL started = select_begin(^(BOOL ok, CGRect r) {
            cb(ctx, ok ? 1 : 0, r.origin.x, r.origin.y, r.size.width, r.size.height);
        });
        return started ? 1 : 0;
    }
}

int32_t bugle_select_rect_active(void) { return g_select_session ? 1 : 0; }

/// Dismiss an in-progress selection as cancelled (the callback fires with ok=0).
void bugle_select_rect_cancel(void) {
    @autoreleasepool {
        [g_select_session finishWithRect:CGRectZero ok:NO];
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
        g_last_error = nil;
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
        g_last_error = nil;
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

double bugle_media_duration(const char *path) {
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        return CMTimeGetSeconds(asset.duration);
    }
}

bugle_image_t bugle_video_frame(const char *path, double seconds) {
    @autoreleasepool {
        g_last_error = nil;
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:url options:nil];
        AVAssetImageGenerator *gen = [[AVAssetImageGenerator alloc] initWithAsset:asset];
        gen.appliesPreferredTrackTransform = YES;
        gen.requestedTimeToleranceBefore = CMTimeMake(1, 10);
        gen.requestedTimeToleranceAfter = CMTimeMake(1, 10);
        NSError *err = nil;
        CGImageRef img = [gen copyCGImageAtTime:CMTimeMakeWithSeconds(seconds, 600) actualTime:NULL error:&err];
        if (!img) { set_error(@"frame extraction failed: %@", err.localizedDescription); return (bugle_image_t){0}; }
        bugle_image_t out = cg_to_rgba(img);
        CGImageRelease(img);
        return out;
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
@property (nonatomic, strong) NSString *failure;
@property (nonatomic, strong) NSMutableArray<NSString *> *warnings;
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
        [self handleAudio:sb input:self.audioInput];
    }
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
        if (self.stream) {
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
        if (!self.sessionStarted) {
            [self.writer cancelWriting];
            set_error(@"no frames were captured%@", self.failure ? [NSString stringWithFormat:@" (%@)", self.failure] : @"");
            return -1;
        }
        if (self.writer.status != AVAssetWriterStatusWriting) {
            set_error(@"recording failed: %@", self.failure ?: self.writer.error.localizedDescription ?: @"unknown writer state");
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
    long long n = llround(v);
    if (n < 2) n = 2;
    return (size_t)(n & ~1LL);
}

void *bugle_sck_record_start(double x, double y, double w, double h, uint32_t window_id, int32_t fps, int32_t retina,
                             int32_t system_audio, int32_t microphone, int32_t show_cursor, double max_seconds,
                             const char *out_path) {
    @autoreleasepool {
        g_last_error = nil;
        if (!bugle_sck_record_available()) { set_error(@"ScreenCaptureKit recording needs macOS 14 or later"); return NULL; }
        if (fps < 1) fps = 1;
        if (fps > 120) fps = 120;
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
            cfg.sourceRect = CGRectMake(x - db.origin.x, y - db.origin.y, w, h);
            cfg.width = even_px(w * s);
            cfg.height = even_px(h * s);
        }
        cfg.minimumFrameInterval = CMTimeMake(1, fps);
        cfg.queueDepth = 8;
        cfg.showsCursor = show_cursor != 0;
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
        if (![rec startWithFilter:filter config:cfg url:url fps:fps systemAudio:system_audio != 0 microphone:microphone != 0]) {
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
        g_last_error = nil;
        BugleSckRecorder *rec = (__bridge_transfer BugleSckRecorder *)handle;
        return [rec finish];
    }
}
