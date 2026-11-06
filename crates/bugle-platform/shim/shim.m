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

bugle_image_t bugle_capture_rect(double x, double y, double w, double h, int32_t show_cursor) {
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

static CGRect g_selection = {{0, 0}, {0, 0}};
static BOOL g_selection_ok = NO;

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

@interface BugleSelectView : NSView
@property(nonatomic) NSPoint start;
@property(nonatomic) NSRect sel;
@property(nonatomic) BOOL dragging;
@end

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
        g_selection = CGRectMake(screenRect.origin.x, primaryH - screenRect.origin.y - screenRect.size.height,
                                 screenRect.size.width, screenRect.size.height);
        g_selection_ok = YES;
        stop_app();
    }
    [self setNeedsDisplay:YES];
}
- (void)keyDown:(NSEvent *)e {
    if (e.keyCode == 53) { // Esc
        g_selection_ok = NO;
        stop_app();
    }
}
@end

@interface BugleOverlayWindow : NSWindow
@end
@implementation BugleOverlayWindow
- (BOOL)canBecomeKeyWindow { return YES; }
- (BOOL)canBecomeMainWindow { return YES; }
@end

int32_t bugle_select_rect_blocking(double *x, double *y, double *w, double *h) {
    @autoreleasepool {
        g_selection_ok = NO;
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        NSMutableArray *windows = [NSMutableArray array];
        for (NSScreen *screen in [NSScreen screens]) {
            BugleOverlayWindow *win = [[BugleOverlayWindow alloc] initWithContentRect:screen.frame
                                                                            styleMask:NSWindowStyleMaskBorderless
                                                                              backing:NSBackingStoreBuffered
                                                                                defer:NO
                                                                               screen:screen];
            win.level = NSScreenSaverWindowLevel;
            win.backgroundColor = [NSColor clearColor];
            win.opaque = NO;
            win.hasShadow = NO;
            win.ignoresMouseEvents = NO;
            win.acceptsMouseMovedEvents = YES;
            win.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary;
            BugleSelectView *v = [[BugleSelectView alloc] initWithFrame:win.contentView.bounds];
            v.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
            win.contentView = v;
            [win makeFirstResponder:v];
            [win makeKeyAndOrderFront:nil];
            [windows addObject:win];
        }
        [app activateIgnoringOtherApps:YES];
        [[NSCursor crosshairCursor] push];
        [app run];
        [NSCursor pop];
        for (NSWindow *win in windows) [win orderOut:nil];
        if (!g_selection_ok) return 0;
        *x = g_selection.origin.x;
        *y = g_selection.origin.y;
        *w = g_selection.size.width;
        *h = g_selection.size.height;
        return 1;
    }
}

// ---------------------------------------------------------------------------
// Pin to screen: a floating panel. Blocks until closed.
// Esc or double-click closes; drag anywhere to move; scroll to resize.
// ---------------------------------------------------------------------------

@interface BuglePinView : NSImageView
@end
@implementation BuglePinView
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (BOOL)acceptsFirstResponder { return YES; }
- (void)mouseDown:(NSEvent *)e {
    if (e.clickCount == 2) { stop_app(); return; }
    [self.window performWindowDragWithEvent:e];
}
- (void)keyDown:(NSEvent *)e {
    if (e.keyCode == 53 || (e.keyCode == 13 && (e.modifierFlags & NSEventModifierFlagCommand))) stop_app();
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

void bugle_pin_image_blocking(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity) {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        NSImage *img = [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]];
        if (!img) return;
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
        [win makeKeyAndOrderFront:nil];
        [app activateIgnoringOtherApps:YES];
        [app run];
        [win orderOut:nil];
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

int32_t bugle_export_mp4(const char *in_path, const char *out_path) {
    @autoreleasepool {
        g_last_error = nil;
        NSURL *in = [NSURL fileURLWithPath:[NSString stringWithUTF8String:in_path]];
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:in options:nil];
        AVAssetExportSession *s = [[AVAssetExportSession alloc] initWithAsset:asset presetName:AVAssetExportPresetPassthrough];
        if (!s) { set_error(@"could not create export session"); return -1; }
        s.outputURL = out;
        s.outputFileType = AVFileTypeMPEG4;
        s.shouldOptimizeForNetworkUse = YES;
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [s exportAsynchronouslyWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
        if (s.status != AVAssetExportSessionStatusCompleted) {
            set_error(@"mp4 export failed: %@", s.error.localizedDescription);
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
