// Bugle window picker overlay.
//
// A see-through overlay on every screen that reports the pointer position
// (global points, top-left origin) through a C callback. The host decides
// which window is under the pointer and calls `bugle_pick_highlight` to draw
// it; this file knows nothing about windows. A left click, a right click,
// Esc, switching apps, `bugle_pick_cancel` or the timeout ends the session:
// the overlay is removed first and the final callback is delivered on a later
// main-queue turn, so it never re-enters the caller.

#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#include "shim.h"

@class BuglePickSession;

@interface BuglePickView : NSView
@property(nonatomic, weak) BuglePickSession *session;
@end

@interface BuglePickWindow : NSWindow
@end

@interface BuglePickSession : NSObject
@property(nonatomic, strong) NSMutableArray<NSWindow *> *windows;
@property(nonatomic) bugle_pick_callback_t cb;
@property(nonatomic) void *ctx;
@property(nonatomic) CGRect highlight;  // global, top-left origin; empty = none
@property(nonatomic, copy) NSString *label;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, strong) id resignObserver;
@property(nonatomic) BOOL finished;
- (void)moved;
- (void)finish:(int32_t)kind;
@end

static BuglePickSession *g_pick_session = nil;

static CGFloat pick_primary_height(void) { return [NSScreen screens].firstObject.frame.size.height; }

/// Pointer position in global top-left points.
static CGPoint pick_mouse(void) {
    NSPoint p = [NSEvent mouseLocation];
    return CGPointMake(p.x, pick_primary_height() - p.y);
}

@implementation BuglePickWindow
- (BOOL)canBecomeKeyWindow { return YES; }
- (BOOL)canBecomeMainWindow { return YES; }
@end

@implementation BuglePickView
- (BOOL)acceptsFirstResponder { return YES; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (void)updateTrackingAreas {
    [super updateTrackingAreas];
    for (NSTrackingArea *t in [self.trackingAreas copy]) [self removeTrackingArea:t];
    // ActiveAlways: every screen's overlay reports moves, not just the key one.
    NSTrackingAreaOptions opts = NSTrackingMouseMoved | NSTrackingActiveAlways | NSTrackingInVisibleRect;
    [self addTrackingArea:[[NSTrackingArea alloc] initWithRect:NSZeroRect options:opts owner:self userInfo:nil]];
}
- (void)resetCursorRects { [self addCursorRect:self.bounds cursor:[NSCursor pointingHandCursor]]; }
- (void)drawRect:(NSRect)dirty {
    // A faint wash: shows the mode is on, and keeps every pixel of the
    // overlay opaque to clicks.
    [[NSColor colorWithWhite:0 alpha:0.12] setFill];
    NSRectFill(self.bounds);
    BuglePickSession *s = self.session;
    if (!s || CGRectIsEmpty(s.highlight)) return;
    CGRect g = s.highlight;
    NSRect cocoa = NSMakeRect(g.origin.x, pick_primary_height() - g.origin.y - g.size.height, g.size.width, g.size.height);
    NSRect r = [self convertRect:[self.window convertRectFromScreen:cocoa] fromView:nil];
    if (!NSIntersectsRect(r, self.bounds)) return;
    [[NSColor clearColor] setFill];
    NSRectFillUsingOperation(r, NSCompositingOperationCopy);
    [[[NSColor systemBlueColor] colorWithAlphaComponent:0.22] setFill];
    NSRectFillUsingOperation(r, NSCompositingOperationSourceOver);
    [[NSColor systemBlueColor] setStroke];
    NSBezierPath *p = [NSBezierPath bezierPathWithRect:NSInsetRect(r, 1.5, 1.5)];
    p.lineWidth = 3.0;
    [p stroke];
    if (s.label.length == 0) return;
    NSDictionary *attrs = @{
        NSFontAttributeName : [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold],
        NSForegroundColorAttributeName : [NSColor whiteColor]
    };
    NSSize sz = [s.label sizeWithAttributes:attrs];
    CGFloat w = MIN(sz.width + 16, MAX(NSWidth(r) - 16, 40));
    NSRect lr = NSMakeRect(NSMidX(r) - w / 2, NSMidY(r) - (sz.height + 10) / 2, w, sz.height + 10);
    [[NSColor colorWithWhite:0 alpha:0.72] setFill];
    [[NSBezierPath bezierPathWithRoundedRect:lr xRadius:6 yRadius:6] fill];
    NSMutableParagraphStyle *ps = [[NSMutableParagraphStyle alloc] init];
    ps.lineBreakMode = NSLineBreakByTruncatingTail;
    ps.alignment = NSTextAlignmentCenter;
    NSMutableDictionary *a2 = [attrs mutableCopy];
    a2[NSParagraphStyleAttributeName] = ps;
    [s.label drawInRect:NSInsetRect(lr, 8, 5) withAttributes:a2];
}
- (void)mouseMoved:(NSEvent *)e { [self.session moved]; }
- (void)mouseDown:(NSEvent *)e { [self.session finish:1]; }
- (void)rightMouseDown:(NSEvent *)e { [self.session finish:2]; }
- (void)keyDown:(NSEvent *)e {
    if (e.keyCode == 53) [self.session finish:2];  // Esc
}
- (void)cancelOperation:(id)sender { [self.session finish:2]; }
@end

@implementation BuglePickSession
- (void)moved {
    if (self.finished) return;
    CGPoint p = pick_mouse();
    self.cb(self.ctx, 0, p.x, p.y);
}
- (void)finish:(int32_t)kind {
    if (self.finished) return;
    // The global may be the only strong reference. Hold one for the whole
    // method, and touch nothing but locals after the global is cleared.
    __attribute__((objc_precise_lifetime)) BuglePickSession *keep = self;
    self.finished = YES;
    [self.timer invalidate];
    self.timer = nil;
    if (self.resignObserver) [[NSNotificationCenter defaultCenter] removeObserver:self.resignObserver];
    self.resignObserver = nil;
    for (NSWindow *win in self.windows) [win orderOut:nil];
    [self.windows removeAllObjects];
    CGPoint p = pick_mouse();
    bugle_pick_callback_t cb = self.cb;
    void *ctx = self.ctx;
    self.cb = NULL;
    self.ctx = NULL;
    if (g_pick_session == keep) g_pick_session = nil;
    // Give keyboard focus back to whatever the user was working in.
    [NSApp deactivate];
    dispatch_async(dispatch_get_main_queue(), ^{
        cb(ctx, kind, p.x, p.y);
    });
}
@end

int32_t bugle_pick_begin(bugle_pick_callback_t cb, void *ctx, double timeout_seconds) {
    @autoreleasepool {
        if (!cb || g_pick_session || bugle_select_rect_active()) return 0;
        NSApplication *app = [NSApplication sharedApplication];
        BuglePickSession *session = [[BuglePickSession alloc] init];
        session.windows = [NSMutableArray array];
        session.cb = cb;
        session.ctx = ctx;
        session.highlight = CGRectZero;
        for (NSScreen *screen in [NSScreen screens]) {
            BuglePickWindow *win = [[BuglePickWindow alloc] initWithContentRect:screen.frame
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
            win.collectionBehavior =
                NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary;
            BuglePickView *v = [[BuglePickView alloc] initWithFrame:win.contentView.bounds];
            v.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
            v.session = session;
            win.contentView = v;
            [win makeFirstResponder:v];
            [win makeKeyAndOrderFront:nil];
            [session.windows addObject:win];
        }
        // The overlay must never outlive its welcome: it swallows every click.
        double t = timeout_seconds > 0 ? timeout_seconds : 30.0;
        __weak BuglePickSession *weak = session;
        session.timer = [NSTimer timerWithTimeInterval:t
                                               repeats:NO
                                                 block:^(NSTimer *timer) {
                                                     [weak finish:2];
                                                 }];
        [[NSRunLoop mainRunLoop] addTimer:session.timer forMode:NSRunLoopCommonModes];
        g_pick_session = session;
        [app activateIgnoringOtherApps:YES];
        // Esc only reaches the overlay while Bugle is active. Switching to
        // another app (Cmd-Tab) therefore cancels, so the overlay is never
        // left up with a dead Esc key; click, right click and the timeout
        // work regardless.
        session.resignObserver = [[NSNotificationCenter defaultCenter]
            addObserverForName:NSApplicationDidResignActiveNotification
                        object:nil
                         queue:[NSOperationQueue mainQueue]
                    usingBlock:^(NSNotification *note) {
                        [weak finish:2];
                    }];
        // Highlight whatever is under the pointer before it moves.
        dispatch_async(dispatch_get_main_queue(), ^{
            [weak moved];
        });
        return 1;
    }
}

void bugle_pick_highlight(double x, double y, double w, double h, const char *label) {
    @autoreleasepool {
        BuglePickSession *s = g_pick_session;
        if (!s || s.finished) return;
        s.highlight = (w > 0 && h > 0) ? CGRectMake(x, y, w, h) : CGRectZero;
        s.label = label ? [NSString stringWithUTF8String:label] : nil;
        for (NSWindow *win in s.windows) [win.contentView setNeedsDisplay:YES];
    }
}

int32_t bugle_pick_active(void) { return g_pick_session ? 1 : 0; }

void bugle_pick_cancel(void) {
    @autoreleasepool {
        __attribute__((objc_precise_lifetime)) BuglePickSession *s = g_pick_session;  // strong across the call
        [s finish:2];
    }
}
