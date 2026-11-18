// Bugle video editor window.
//
// A preview of one frame above a timeline (the whole recording with its
// audio waveform, the kept range between the in and out handles, the cuts
// struck out of it, the playhead, a marker under each click auto-zoom
// follows) and a control bar: play/pause, the playhead time, Set In /
// Set Out, Cut / Restore, the zoom, the zoom of the click at the playhead
// and pointer options, the export preset and Export. The window owns no
// editing policy:
// input is reported to Rust through a callback (times in seconds on the
// video's timeline), and Rust answers by replacing the frame shown
// (`bugle_video_editor_set_frame`) and the state of every control
// (`bugle_video_editor_set_state`). The only thing the timeline does on its
// own is move the dragged handle or playhead under the pointer until Rust's
// answer arrives, so a drag never lags, and snap the playhead onto a click
// marker it is dropped next to.
//
// Keys: Space play/pause, I / O set in / out at the playhead, C starts or
// ends a cut at the playhead, Delete restores the cut under the playhead
// (or drops the one being marked), ← / → step one frame, ↑ / Home go to the
// in point, ↓ / End go to the out point, Esc pauses (or drops a cut being
// marked), ⌘E exports, ⌘W closes.
//
// Main thread only; needs a running AppKit loop. One video editor at a time.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdint.h>
#include "shim.h"

typedef void (*bugle_video_editor_callback_t)(void *ctx, int32_t kind, double value, int32_t arg);
typedef void (*bugle_video_editor_free_t)(void *ctx);

// Event kinds. Mirrored in src/video_editor.rs.
enum {
    VEV_SEEK = 0,     // value = seconds (playhead dragged or clicked)
    VEV_IN = 1,       // value = seconds (in handle dragged)
    VEV_OUT = 2,      // value = seconds (out handle dragged)
    VEV_COMMAND = 3,  // arg = VCMD_*
    VEV_ZOOM = 4,     // arg = zoom choice index
    VEV_SMOOTH = 5,   // arg = 0 / 1
    VEV_PRESET = 6,   // arg = preset index
    VEV_CLICK_ZOOM = 7, // arg = click zoom choice index
};
enum {
    VCMD_PLAY_PAUSE = 0,
    VCMD_MARK_IN = 1,
    VCMD_MARK_OUT = 2,
    VCMD_EXPORT = 3,
    VCMD_STEP_BACK = 4,
    VCMD_STEP_FORWARD = 5,
    VCMD_ESCAPE = 6,
    VCMD_GO_IN = 7,
    VCMD_GO_OUT = 8,
    VCMD_CUT = 9,
    VCMD_UNCUT = 10,
};

static const CGFloat kBarHeight = 48;
static const CGFloat kTimelineHeight = 64;
static const CGFloat kTrackHeight = 32;
static const CGFloat kTimelineInset = 16;
// How close (points) to a handle a click must land to grab it.
static const CGFloat kHandleGrab = 7;
// How close (points) to a click marker the playhead snaps onto it.
static const CGFloat kMarkerSnap = 5;

@class BugleVideoEditor;
static BugleVideoEditor *g_video_editor = nil;

// What is being dragged on the timeline.
typedef enum { DRAG_NONE, DRAG_PLAYHEAD, DRAG_IN, DRAG_OUT } DragKind;

@interface BugleTimeline : NSView
@property(nonatomic, weak) BugleVideoEditor *editor;
@property(nonatomic) double duration, playhead, inPoint, outPoint;
// Removed ranges as pairs of seconds; NAN when no cut is being marked.
@property(nonatomic, copy) NSData *cuts;
@property(nonatomic) double cutMark;
@property(nonatomic) DragKind drag;
/// The pointer has not moved since mouse-down: a click, which snaps the
/// playhead onto a nearby marker. A drag scrubs freely.
@property(nonatomic) BOOL snapping;
// Peak levels (float, 0..1) in equal slices of the recording; nil: none.
@property(nonatomic, copy) NSData *waveform;
// Click markers: times and zoom levels (double each), and the selected one.
@property(nonatomic, copy) NSData *clickTimes, *clickLevels;
@property(nonatomic) NSInteger selectedClick;
@end

@interface BugleVideoEditorWindow : NSWindow
@property(nonatomic, weak) BugleVideoEditor *editor;
@end

@interface BugleVideoEditor : NSObject <NSWindowDelegate>
@property(nonatomic, strong) BugleVideoEditorWindow *window;
@property(nonatomic, strong) NSImageView *preview;
@property(nonatomic, strong) BugleTimeline *timeline;
@property(nonatomic, strong) NSButton *playButton;
@property(nonatomic, strong) NSTextField *timeLabel;
@property(nonatomic, strong) NSButton *inButton, *outButton;
@property(nonatomic, strong) NSButton *cutButton, *uncutButton;
@property(nonatomic, strong) NSPopUpButton *zoomPopup;
@property(nonatomic, strong) NSPopUpButton *clickPopup;
@property(nonatomic, strong) NSButton *smoothBox;
@property(nonatomic, strong) NSPopUpButton *presetPopup;
@property(nonatomic, strong) NSButton *exportButton;
@property(nonatomic) bugle_video_editor_callback_t cb;
@property(nonatomic) void *ctx;
@property(nonatomic) bugle_video_editor_free_t freeCtx;
@property(nonatomic) BOOL busy;
@property(nonatomic) BOOL closing;
- (void)emit:(int32_t)kind value:(double)value arg:(int32_t)arg;
- (void)command:(int32_t)cmd;
- (void)teardown;
@end

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

@implementation BugleTimeline
- (BOOL)acceptsFirstResponder { return YES; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }

- (NSRect)track {
    NSRect b = self.bounds;
    return NSMakeRect(kTimelineInset, floor(NSMidY(b) - kTrackHeight / 2), MAX(1, NSWidth(b) - 2 * kTimelineInset), kTrackHeight);
}

- (CGFloat)xFor:(double)t {
    NSRect r = [self track];
    double d = self.duration > 0 ? self.duration : 1;
    return NSMinX(r) + NSWidth(r) * fmin(fmax(t / d, 0), 1);
}

- (double)timeAt:(CGFloat)x {
    NSRect r = [self track];
    return fmin(fmax((x - NSMinX(r)) / NSWidth(r), 0), 1) * self.duration;
}

- (void)drawRect:(NSRect)dirty {
    NSRect r = [self track];
    // Whole recording, dimmed.
    [[NSColor colorWithWhite:0.5 alpha:0.25] setFill];
    [[NSBezierPath bezierPathWithRoundedRect:r xRadius:4 yRadius:4] fill];
    // Kept range.
    CGFloat xi = [self xFor:self.inPoint], xo = [self xFor:self.outPoint];
    NSRect kept = NSMakeRect(xi, NSMinY(r), MAX(0, xo - xi), NSHeight(r));
    [[NSColor.controlAccentColor colorWithAlphaComponent:0.45] setFill];
    NSRectFill(kept);
    // Waveform: the loudest bucket under each point column, mirrored about
    // the middle of the track.
    NSUInteger nw = self.waveform.length / sizeof(float);
    if (nw > 0) {
        const float *peaks = self.waveform.bytes;
        [[NSColor.labelColor colorWithAlphaComponent:0.4] setFill];
        CGFloat mid = NSMidY(r), half = NSHeight(r) / 2 - 2;
        NSInteger cols = (NSInteger)ceil(NSWidth(r));
        for (NSInteger c = 0; c < cols; c++) {
            NSUInteger a = (NSUInteger)((double)c / cols * nw), b = (NSUInteger)((double)(c + 1) / cols * nw);
            if (b <= a) b = a + 1;
            if (b > nw) b = nw;
            float v = 0;
            for (NSUInteger k = a; k < b; k++) v = fmaxf(v, peaks[k]);
            CGFloat hgt = fmax(0.5, fmin(1.0, v) * half);
            NSRectFillUsingOperation(NSMakeRect(NSMinX(r) + c, mid - hgt, 1, 2 * hgt), NSCompositingOperationSourceOver);
        }
    }
    [NSColor.controlAccentColor setStroke];
    NSBezierPath *frame = [NSBezierPath bezierPathWithRect:NSInsetRect(kept, 0.5, 0.5)];
    frame.lineWidth = 1;
    [frame stroke];
    // Cuts: darkened and struck through with diagonal lines.
    const double *cuts = self.cuts.bytes;
    NSUInteger n = self.cuts.length / (2 * sizeof(double));
    for (NSUInteger i = 0; i < n; i++) {
        CGFloat xa = [self xFor:cuts[2 * i]], xb = [self xFor:cuts[2 * i + 1]];
        NSRect cut = NSMakeRect(xa, NSMinY(r), MAX(1, xb - xa), NSHeight(r));
        [[NSColor colorWithWhite:0.05 alpha:0.6] setFill];
        NSRectFill(cut);
        [NSGraphicsContext saveGraphicsState];
        NSRectClip(cut);
        NSBezierPath *hatch = [NSBezierPath bezierPath];
        for (CGFloat x = xa - NSHeight(r); x < xb; x += 6) {
            [hatch moveToPoint:NSMakePoint(x, NSMinY(r))];
            [hatch lineToPoint:NSMakePoint(x + NSHeight(r), NSMaxY(r))];
        }
        hatch.lineWidth = 1;
        [[NSColor.systemRedColor colorWithAlphaComponent:0.7] setStroke];
        [hatch stroke];
        [NSGraphicsContext restoreGraphicsState];
    }
    // A cut being marked: from the mark to the playhead.
    if (isfinite(self.cutMark)) {
        CGFloat xm = [self xFor:self.cutMark], xp = [self xFor:self.playhead];
        [[NSColor.systemRedColor colorWithAlphaComponent:0.35] setFill];
        NSRectFillUsingOperation(NSMakeRect(MIN(xm, xp), NSMinY(r), MAX(1, fabs(xp - xm)), NSHeight(r)), NSCompositingOperationSourceOver);
        [NSColor.systemRedColor setFill];
        NSRectFill(NSMakeRect(round(xm) - 1, NSMinY(r) - 3, 2, NSHeight(r) + 6));
    }
    // Handles: a tab on the outside of each end of the kept range.
    [NSColor.controlAccentColor setFill];
    NSRectFill(NSMakeRect(xi - 5, NSMinY(r) - 3, 5, NSHeight(r) + 6));
    NSRectFill(NSMakeRect(xo, NSMinY(r) - 3, 5, NSHeight(r) + 6));
    // Playhead.
    CGFloat xp = round([self xFor:self.playhead]) + 0.5;
    [NSColor.labelColor setFill];
    NSRectFill(NSMakeRect(xp - 1, NSMinY(r) - 8, 2, NSHeight(r) + 16));
    NSBezierPath *knob = [NSBezierPath bezierPathWithOvalInRect:NSMakeRect(xp - 5, NSMaxY(r) + 4, 10, 10)];
    [knob fill];
    // Click markers under the track: a diamond each, filled when the click
    // zooms, hollow when it is left out; the selected one larger.
    NSUInteger nc = MIN(self.clickTimes.length, self.clickLevels.length) / sizeof(double);
    const double *ct = self.clickTimes.bytes, *cl = self.clickLevels.bytes;
    for (NSUInteger i = 0; i < nc; i++) {
        BOOL sel = (NSInteger)i == self.selectedClick;
        CGFloat x = round([self xFor:ct[i]]) + 0.5, s = sel ? 5 : 3.5, y = NSMinY(r) - 9;
        NSBezierPath *d = [NSBezierPath bezierPath];
        [d moveToPoint:NSMakePoint(x, y + s)];
        [d lineToPoint:NSMakePoint(x + s, y)];
        [d lineToPoint:NSMakePoint(x, y - s)];
        [d lineToPoint:NSMakePoint(x - s, y)];
        [d closePath];
        NSColor *c = sel ? NSColor.labelColor : NSColor.controlAccentColor;
        if (cl[i] > 1.0) {
            [c setFill];
            [d fill];
        } else {
            [c setStroke];
            d.lineWidth = 1;
            [d stroke];
        }
    }
}

/// `t`, or the time of a click marker within `kMarkerSnap` points of `x`.
- (double)snap:(double)t x:(CGFloat)x {
    NSUInteger nc = self.clickTimes.length / sizeof(double);
    const double *ct = self.clickTimes.bytes;
    CGFloat best = kMarkerSnap + 1;
    for (NSUInteger i = 0; i < nc; i++) {
        CGFloat d = fabs([self xFor:ct[i]] - x);
        if (d <= kMarkerSnap && d < best) {
            best = d;
            t = ct[i];
        }
    }
    return t;
}

- (void)report:(NSEvent *)event kind:(DragKind)kind {
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    double t = [self timeAt:p.x];
    BugleVideoEditor *e = self.editor;
    // Move what is dragged at once. During a drag the handle follows the
    // pointer; on mouse-up the drag is already over, so Rust's clamped answer
    // (which arrives inside the emit) replaces the local position.
    switch (kind) {
        case DRAG_IN:
            self.inPoint = fmin(t, self.outPoint);
            [e emit:VEV_IN value:t arg:0];
            break;
        case DRAG_OUT:
            self.outPoint = fmax(t, self.inPoint);
            [e emit:VEV_OUT value:t arg:0];
            break;
        case DRAG_PLAYHEAD:
            if (self.snapping) t = [self snap:t x:p.x];
            self.playhead = t;
            [e emit:VEV_SEEK value:t arg:0];
            break;
        case DRAG_NONE:
            return;
    }
    [self setNeedsDisplay:YES];
}

- (void)mouseDown:(NSEvent *)event {
    if (self.editor.busy || self.duration <= 0) return;
    [self.window makeFirstResponder:self];
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    CGFloat di = fabs(p.x - ([self xFor:self.inPoint] - 2.5)), dout = fabs(p.x - ([self xFor:self.outPoint] + 2.5));
    if (di <= kHandleGrab && di <= dout) self.drag = DRAG_IN;
    else if (dout <= kHandleGrab) self.drag = DRAG_OUT;
    else self.drag = DRAG_PLAYHEAD;
    self.snapping = YES;
    [self report:event kind:self.drag];
}

- (void)mouseDragged:(NSEvent *)event {
    self.snapping = NO;
    [self report:event kind:self.drag];
}

- (void)mouseUp:(NSEvent *)event {
    DragKind kind = self.drag;
    self.drag = DRAG_NONE;
    [self report:event kind:kind];
}

- (void)resetCursorRects {
    [self addCursorRect:self.bounds cursor:NSCursor.pointingHandCursor];
}

- (void)keyDown:(NSEvent *)event {
    BugleVideoEditor *e = self.editor;
    NSEventModifierFlags mods = event.modifierFlags & NSEventModifierFlagDeviceIndependentFlagsMask;
    if (mods & (NSEventModifierFlagCommand | NSEventModifierFlagControl | NSEventModifierFlagOption)) {
        [super keyDown:event];
        return;
    }
    NSString *chars = event.charactersIgnoringModifiers.lowercaseString;
    unichar c = chars.length ? [chars characterAtIndex:0] : 0;
    int32_t cmd = -1;
    switch (c) {
        case ' ': cmd = VCMD_PLAY_PAUSE; break;
        case 'i': cmd = VCMD_MARK_IN; break;
        case 'o': cmd = VCMD_MARK_OUT; break;
        case 'c': cmd = VCMD_CUT; break;
        case 127:
        case NSDeleteFunctionKey: cmd = VCMD_UNCUT; break;
        case NSLeftArrowFunctionKey: cmd = VCMD_STEP_BACK; break;
        case NSRightArrowFunctionKey: cmd = VCMD_STEP_FORWARD; break;
        case NSUpArrowFunctionKey:
        case NSHomeFunctionKey: cmd = VCMD_GO_IN; break;
        case NSDownArrowFunctionKey:
        case NSEndFunctionKey: cmd = VCMD_GO_OUT; break;
        case 27: cmd = VCMD_ESCAPE; break;
        default: break;
    }
    if (cmd >= 0) [e command:cmd];
    else [super keyDown:event];
}

- (void)cancelOperation:(id)sender { [self.editor command:VCMD_ESCAPE]; }
@end

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

@implementation BugleVideoEditorWindow
- (BOOL)performKeyEquivalent:(NSEvent *)event {
    NSEventModifierFlags mods = event.modifierFlags & NSEventModifierFlagDeviceIndependentFlagsMask;
    if (!(mods & NSEventModifierFlagCommand) || (mods & (NSEventModifierFlagControl | NSEventModifierFlagOption))) {
        return [super performKeyEquivalent:event];
    }
    NSString *key = event.charactersIgnoringModifiers.lowercaseString;
    if ([key isEqualToString:@"e"]) {
        [self.editor command:VCMD_EXPORT];
    } else if ([key isEqualToString:@"w"]) {
        [self performClose:nil];
    } else {
        return [super performKeyEquivalent:event];
    }
    return YES;
}
@end

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

@implementation BugleVideoEditor
- (void)emit:(int32_t)kind value:(double)value arg:(int32_t)arg {
    if (self.closing || !self.cb || !self.ctx) return;
    self.cb(self.ctx, kind, value, arg);
}

- (void)command:(int32_t)cmd { [self emit:VEV_COMMAND value:0 arg:cmd]; }
- (void)buttonCommand:(NSButton *)sender { [self command:(int32_t)sender.tag]; }
- (void)zoomPicked:(NSPopUpButton *)sender { [self emit:VEV_ZOOM value:0 arg:(int32_t)sender.indexOfSelectedItem]; }
- (void)clickZoomPicked:(NSPopUpButton *)sender { [self emit:VEV_CLICK_ZOOM value:0 arg:(int32_t)sender.indexOfSelectedItem]; }
- (void)presetPicked:(NSPopUpButton *)sender { [self emit:VEV_PRESET value:0 arg:(int32_t)sender.indexOfSelectedItem]; }
- (void)smoothToggled:(NSButton *)sender { [self emit:VEV_SMOOTH value:0 arg:sender.state == NSControlStateValueOn ? 1 : 0]; }

- (BOOL)windowShouldClose:(NSWindow *)sender {
    if (self.busy) {
        // An export is being written.
        NSBeep();
        return NO;
    }
    [self teardown];
    return NO;
}

/// Take the window down and release the Rust context on a later turn, so a
/// close requested from inside the callback never frees the running handler.
- (void)teardown {
    if (self.closing) return;
    self.closing = YES;
    __attribute__((objc_precise_lifetime)) BugleVideoEditor *keep = self;
    if (g_video_editor == self) g_video_editor = nil;
    NSWindow *w = self.window;
    w.delegate = nil;
    [w orderOut:nil];
    void *ctx = self.ctx;
    bugle_video_editor_free_t f = self.freeCtx;
    self.cb = NULL;
    self.ctx = NULL;
    self.freeCtx = NULL;
    // Focus goes to an open markup editor, else back to the app the user
    // came from.
    bugle_editor_window_closed();
    dispatch_async(dispatch_get_main_queue(), ^{
        if (ctx && f) f(ctx);
        (void)keep;
    });
}
@end

// ---------------------------------------------------------------------------
// C API
// ---------------------------------------------------------------------------

static NSString *vstr(const char *s) { return s ? ([NSString stringWithUTF8String:s] ?: @"") : @""; }

static NSButton *vbutton(NSString *title, NSString *tip, BugleVideoEditor *target, int32_t tag) {
    NSButton *b = [NSButton buttonWithTitle:title target:target action:@selector(buttonCommand:)];
    b.bezelStyle = NSBezelStyleRounded;
    b.toolTip = tip;
    b.tag = tag;
    b.refusesFirstResponder = YES;
    return b;
}

static NSPopUpButton *vpopup(const char *const *labels, int32_t n, BugleVideoEditor *target, SEL action, NSString *tip) {
    NSPopUpButton *p = [[NSPopUpButton alloc] initWithFrame:NSMakeRect(0, 0, 120, 24) pullsDown:NO];
    for (int32_t i = 0; i < n; i++) {
        [p addItemWithTitle:@""];
        // Set afterwards: addItemWithTitle: drops duplicate titles.
        [p itemAtIndex:i].title = vstr(labels[i]);
    }
    p.target = target;
    p.action = action;
    p.toolTip = tip;
    p.refusesFirstResponder = YES;
    return p;
}

/// Open the video editor for a recording `duration` seconds long whose
/// frames are `w` x `h` pixels. `zoom_labels`, `click_labels` and
/// `preset_labels` fill the three pop-up menus. Returns 1, or 0 when a video editor is already open
/// (the context is not taken then).
int32_t bugle_video_editor_open(const char *title, double duration, uint32_t w, uint32_t h, const char *const *zoom_labels,
                                int32_t n_zoom, const char *const *click_labels, int32_t n_click,
                                const char *const *preset_labels, int32_t n_presets,
                                bugle_video_editor_callback_t cb, void *ctx, bugle_video_editor_free_t free_ctx) {
    @autoreleasepool {
        if (g_video_editor || !cb || !(duration > 0) || w == 0 || h == 0) return 0;
        NSApplication *app = [NSApplication sharedApplication];
        BugleVideoEditor *e = [[BugleVideoEditor alloc] init];
        e.cb = cb;
        e.ctx = ctx;
        e.freeCtx = free_ctx;

        // ---- control bar ----
        NSImage *play = [NSImage imageWithSystemSymbolName:@"play.fill" accessibilityDescription:@"Play"];
        NSButton *playButton = play ? [NSButton buttonWithImage:play target:e action:@selector(buttonCommand:)]
                                    : [NSButton buttonWithTitle:@"Play" target:e action:@selector(buttonCommand:)];
        playButton.bezelStyle = NSBezelStyleRounded;
        playButton.tag = VCMD_PLAY_PAUSE;
        playButton.toolTip = @"Play or pause (Space)";
        playButton.refusesFirstResponder = YES;
        [playButton.widthAnchor constraintEqualToConstant:44].active = YES;
        e.playButton = playButton;

        NSTextField *time = [NSTextField labelWithString:@"0:00.00 / 0:00.00"];
        time.font = [NSFont monospacedDigitSystemFontOfSize:[NSFont smallSystemFontSize] weight:NSFontWeightRegular];
        time.textColor = NSColor.secondaryLabelColor;
        time.lineBreakMode = NSLineBreakByTruncatingTail;
        [time setContentCompressionResistancePriority:NSLayoutPriorityDefaultLow forOrientation:NSLayoutConstraintOrientationHorizontal];
        // Room for "0:00.00 / 0:00.00 · clip 0:00.00" (longer recordings
        // grow it while the window has room).
        [time.widthAnchor constraintGreaterThanOrEqualToConstant:
                              ceil([@"00:00.00 / 00:00.00 · clip 00:00.00" sizeWithAttributes:@{NSFontAttributeName : time.font}].width) + 4]
            .active = YES;
        e.timeLabel = time;

        e.inButton = vbutton(@"Set In", @"Start the clip at the playhead (I)", e, VCMD_MARK_IN);
        e.outButton = vbutton(@"Set Out", @"End the clip at the playhead (O)", e, VCMD_MARK_OUT);
        e.cutButton = vbutton(@"Cut", @"Start a cut at the playhead; press again to end it and remove the part between (C)", e, VCMD_CUT);
        e.uncutButton = vbutton(@"Restore", @"Restore the cut under the playhead, or drop the cut being marked (Delete)", e, VCMD_UNCUT);

        NSTextField *zoomLabel = [NSTextField labelWithString:@"Zoom"];
        e.zoomPopup = vpopup(zoom_labels, n_zoom, e, @selector(zoomPicked:), @"Zoom in on clicks (needs a recording made with cursor events)");
        NSTextField *clickLabel = [NSTextField labelWithString:@"Click"];
        e.clickPopup = vpopup(click_labels, n_click, e, @selector(clickZoomPicked:),
                              @"Zoom of the click at the playhead (the markers under the timeline)");
        e.smoothBox = [NSButton checkboxWithTitle:@"Smooth pointer" target:e action:@selector(smoothToggled:)];
        e.smoothBox.toolTip = @"Draw the pointer along a smoothed path (recordings made with the pointer hidden)";
        e.smoothBox.refusesFirstResponder = YES;
        e.presetPopup = vpopup(preset_labels, n_presets, e, @selector(presetPicked:), @"Export format");
        e.exportButton = vbutton(@"Export", @"Export the clip as a new recording (⌘E)", e, VCMD_EXPORT);
        e.exportButton.keyEquivalent = @"";

        NSView *spacer = [[NSView alloc] initWithFrame:NSZeroRect];
        [spacer setContentHuggingPriority:1 forOrientation:NSLayoutConstraintOrientationHorizontal];
        NSStackView *stack = [NSStackView stackViewWithViews:@[
            playButton, time, e.inButton, e.outButton, e.cutButton, e.uncutButton, spacer, zoomLabel, e.zoomPopup, clickLabel,
            e.clickPopup, e.smoothBox,
            e.presetPopup, e.exportButton
        ]];
        stack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
        stack.spacing = 8;
        stack.distribution = NSStackViewDistributionFill;
        stack.edgeInsets = NSEdgeInsetsMake(0, 12, 0, 12);
        [stack setCustomSpacing:4 afterView:e.inButton];
        [stack setCustomSpacing:4 afterView:e.cutButton];
        [stack setCustomSpacing:4 afterView:zoomLabel];
        [stack setCustomSpacing:4 afterView:clickLabel];
        [stack setCustomSpacing:14 afterView:e.smoothBox];
        CGFloat minW = MAX(720, ceil(stack.fittingSize.width) + 40);

        // ---- size: the frame at up to its natural size, within most of the screen ----
        NSScreen *screen = [NSScreen mainScreen] ?: [NSScreen screens].firstObject;
        NSRect vf = screen.visibleFrame;
        CGFloat backing = MAX(1.0, screen.backingScaleFactor);
        CGFloat maxW = NSWidth(vf) * 0.8, maxH = NSHeight(vf) * 0.8 - kBarHeight - kTimelineHeight - 28;
        CGFloat natW = w / backing, natH = h / backing;
        CGFloat fit = MIN(1.0, MIN(maxW / natW, maxH / natH));
        CGFloat pw = MAX(minW, ceil(natW * fit)), ph = MAX(260, ceil(natH * fit));
        NSRect content = NSMakeRect(0, 0, pw, ph + kTimelineHeight + kBarHeight);

        BugleVideoEditorWindow *win = [[BugleVideoEditorWindow alloc]
            initWithContentRect:content
                      styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable |
                                NSWindowStyleMaskMiniaturizable
                        backing:NSBackingStoreBuffered
                          defer:NO];
        win.releasedWhenClosed = NO;
        win.title = vstr(title);
        win.contentMinSize = NSMakeSize(minW, 320);
        win.editor = e;
        win.delegate = e;
        e.window = win;

        NSView *root = [[NSView alloc] initWithFrame:content];

        NSView *backdrop = [[NSView alloc] initWithFrame:NSMakeRect(0, kBarHeight + kTimelineHeight, pw, ph)];
        backdrop.wantsLayer = YES;
        backdrop.layer.backgroundColor = [NSColor colorWithWhite:0.08 alpha:1].CGColor;
        backdrop.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        [root addSubview:backdrop];
        NSImageView *preview = [[NSImageView alloc] initWithFrame:NSInsetRect(backdrop.bounds, 8, 8)];
        preview.imageScaling = NSImageScaleProportionallyUpOrDown;
        preview.imageFrameStyle = NSImageFrameNone;
        preview.editable = NO;
        preview.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        [backdrop addSubview:preview];
        e.preview = preview;

        BugleTimeline *timeline = [[BugleTimeline alloc] initWithFrame:NSMakeRect(0, kBarHeight, pw, kTimelineHeight)];
        timeline.autoresizingMask = NSViewWidthSizable | NSViewMaxYMargin;
        timeline.editor = e;
        timeline.duration = duration;
        timeline.inPoint = 0;
        timeline.outPoint = duration;
        timeline.cutMark = NAN;
        timeline.selectedClick = -1;
        [root addSubview:timeline];
        e.timeline = timeline;

        NSVisualEffectView *bar = [[NSVisualEffectView alloc] initWithFrame:NSMakeRect(0, 0, pw, kBarHeight)];
        bar.material = NSVisualEffectMaterialTitlebar;
        bar.blendingMode = NSVisualEffectBlendingModeWithinWindow;
        bar.autoresizingMask = NSViewWidthSizable | NSViewMaxYMargin;
        [root addSubview:bar];
        [bar addSubview:stack];
        [NSLayoutConstraint activateConstraints:@[
            [stack.leadingAnchor constraintEqualToAnchor:bar.leadingAnchor],
            [stack.trailingAnchor constraintEqualToAnchor:bar.trailingAnchor],
            [stack.topAnchor constraintEqualToAnchor:bar.topAnchor],
            [stack.bottomAnchor constraintEqualToAnchor:bar.bottomAnchor],
        ]];

        win.contentView = root;
        [win center];
        g_video_editor = e;
        [win makeFirstResponder:timeline];
        [app activateIgnoringOtherApps:YES];
        [win makeKeyAndOrderFront:nil];
        return 1;
    }
}

/// Show a frame. `rgba` is copied (RGBA8, not premultiplied, `w * h * 4`
/// bytes); it is scaled to fit the preview area.
void bugle_video_editor_set_frame(const uint8_t *rgba, uint32_t w, uint32_t h) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e || !rgba || w == 0 || h == 0) return;
        CFDataRef data = CFDataCreate(NULL, rgba, (CFIndex)w * h * 4);
        if (!data) return;
        CGDataProviderRef provider = CGDataProviderCreateWithCFData(data);
        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        CGImageRef img = CGImageCreate(w, h, 8, 32, (size_t)w * 4, space, (CGBitmapInfo)kCGImageAlphaLast | kCGBitmapByteOrderDefault,
                                       provider, NULL, true, kCGRenderingIntentDefault);
        CGColorSpaceRelease(space);
        CGDataProviderRelease(provider);
        CFRelease(data);
        if (!img) return;
        // Sized in points so the image view scales by aspect, not by pixels.
        e.preview.image = [[NSImage alloc] initWithCGImage:img size:NSMakeSize(w, h)];
        CGImageRelease(img);
    }
}

/// Replace the state of every control. `cuts` holds `n_cuts` removed ranges
/// as pairs of seconds; `cut_mark` is where a cut being marked starts (NAN:
/// none). `time_text` is the line next to the play button. A busy window (an export is running) disables its controls
/// and refuses to close.
void bugle_video_editor_set_state(double playhead, double in_point, double out_point, const double *cuts, int32_t n_cuts,
                                  double cut_mark, int32_t can_uncut, int32_t playing, int32_t zoom, int32_t smooth,
                                  int32_t preset, int32_t can_zoom, int32_t can_smooth, int32_t busy, int32_t click_zoom,
                                  const char *time_text) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e) return;
        BugleTimeline *t = e.timeline;
        // A handle being dragged follows the pointer; the rest follows Rust.
        if (t.drag != DRAG_PLAYHEAD) t.playhead = playhead;
        if (t.drag != DRAG_IN) t.inPoint = in_point;
        if (t.drag != DRAG_OUT) t.outPoint = out_point;
        t.cuts = (cuts && n_cuts > 0) ? [NSData dataWithBytes:cuts length:(NSUInteger)n_cuts * 2 * sizeof(double)] : nil;
        t.cutMark = cut_mark;
        [t setNeedsDisplay:YES];
        NSString *symbol = playing ? @"pause.fill" : @"play.fill";
        NSImage *img = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:playing ? @"Pause" : @"Play"];
        if (img) e.playButton.image = img;
        else e.playButton.title = playing ? @"Pause" : @"Play";
        e.timeLabel.stringValue = vstr(time_text);
        if (zoom >= 0 && zoom < e.zoomPopup.numberOfItems) [e.zoomPopup selectItemAtIndex:zoom];
        if (preset >= 0 && preset < e.presetPopup.numberOfItems) [e.presetPopup selectItemAtIndex:preset];
        BOOL onClick = click_zoom >= 0 && click_zoom < e.clickPopup.numberOfItems;
        if (e.clickPopup.numberOfItems > 0) [e.clickPopup selectItemAtIndex:onClick ? click_zoom : 0];
        e.smoothBox.state = smooth ? NSControlStateValueOn : NSControlStateValueOff;
        e.busy = busy != 0;
        BOOL on = !e.busy;
        e.playButton.enabled = on;
        e.inButton.enabled = on;
        e.outButton.enabled = on;
        e.cutButton.enabled = on;
        e.cutButton.title = isfinite(cut_mark) ? @"End Cut" : @"Cut";
        e.uncutButton.enabled = on && can_uncut;
        e.zoomPopup.enabled = on && can_zoom;
        e.clickPopup.enabled = on && can_zoom && onClick;
        e.smoothBox.enabled = on && can_smooth;
        e.presetPopup.enabled = on;
        e.exportButton.enabled = on;
        e.exportButton.title = e.busy ? @"Exporting…" : @"Export";
    }
}

/// Replace the click markers: `n` times (seconds) and zoom levels (1: the
/// click is left out), and the index of the selected one (-1: none).
void bugle_video_editor_set_clicks(const double *times, const double *levels, int32_t n, int32_t selected) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e) return;
        BugleTimeline *t = e.timeline;
        BOOL some = n > 0 && times && levels;
        NSData *ts = some ? [NSData dataWithBytes:times length:(NSUInteger)n * sizeof(double)] : nil;
        NSData *ls = some ? [NSData dataWithBytes:levels length:(NSUInteger)n * sizeof(double)] : nil;
        if ((ts == t.clickTimes || [ts isEqualToData:t.clickTimes]) && (ls == t.clickLevels || [ls isEqualToData:t.clickLevels]) &&
            t.selectedClick == selected) {
            return;
        }
        t.clickTimes = ts;
        t.clickLevels = ls;
        t.selectedClick = selected;
        [t setNeedsDisplay:YES];
    }
}

/// Draw `n` peak levels (0..1, equal slices of the recording) as the
/// timeline's waveform; `n` 0 removes it. The levels are copied.
void bugle_video_editor_set_waveform(const float *peaks, int32_t n) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e) return;
        e.timeline.waveform = (n > 0 && peaks) ? [NSData dataWithBytes:peaks length:(NSUInteger)n * sizeof(float)] : nil;
        [e.timeline setNeedsDisplay:YES];
    }
}

/// Close the video editor. Safe to call from inside the callback.
void bugle_video_editor_close(void) {
    @autoreleasepool {
        __attribute__((objc_precise_lifetime)) BugleVideoEditor *e = g_video_editor;
        [e teardown];
    }
}

int32_t bugle_video_editor_active(void) { return g_video_editor ? 1 : 0; }

NSWindow *bugle_video_editor_window(void) { return g_video_editor.window; }

/// Bring the open video editor to the front.
void bugle_video_editor_focus(void) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e) return;
        [NSApp activateIgnoringOtherApps:YES];
        [e.window makeKeyAndOrderFront:nil];
    }
}

/// Deliver an event to the callback as if it came from the window. For
/// smoke tests, which cannot click.
void bugle_video_editor_inject(int32_t kind, double value, int32_t arg) {
    @autoreleasepool {
        [g_video_editor emit:kind value:value arg:arg];
    }
}

/// Smoke test: where part `which` of the video editor is, in window
/// coordinates (points, origin at the bottom left): 0 the timeline's track
/// (time 0 at its left edge, the end at its right), 1 play, 2 Set In,
/// 3 Set Out, 4 Cut, 5 Restore, 6 the zoom menu, 7 the click menu,
/// 8 Smooth pointer, 9 the preset menu, 10 Export. Writes x, y, width,
/// height to `out` and returns 1; 0 with no video editor open. Main thread
/// only.
int32_t bugle_video_editor_smoke_rect(int32_t which, double *out) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e || !out) return 0;
        NSView *view = nil;
        NSRect r = NSZeroRect;
        switch (which) {
            case 0: view = e.timeline; r = [e.timeline track]; break;
            case 1: view = e.playButton; break;
            case 2: view = e.inButton; break;
            case 3: view = e.outButton; break;
            case 4: view = e.cutButton; break;
            case 5: view = e.uncutButton; break;
            case 6: view = e.zoomPopup; break;
            case 7: view = e.clickPopup; break;
            case 8: view = e.smoothBox; break;
            case 9: view = e.presetPopup; break;
            case 10: view = e.exportButton; break;
            default: return 0;
        }
        if (which != 0) r = view.bounds;
        NSRect w = [view convertRect:r toView:nil];
        out[0] = NSMinX(w);
        out[1] = NSMinY(w);
        out[2] = NSWidth(w);
        out[3] = NSHeight(w);
        return 1;
    }
}
