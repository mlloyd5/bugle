// Bugle video editor window.
//
// A preview of one frame above a timeline (the whole recording, the kept
// range between the in and out handles, the playhead) and a control bar:
// play/pause, the playhead time, Set In / Set Out, the zoom and pointer
// options, the export preset and Export. The window owns no editing policy:
// input is reported to Rust through a callback (times in seconds on the
// video's timeline), and Rust answers by replacing the frame shown
// (`bugle_video_editor_set_frame`) and the state of every control
// (`bugle_video_editor_set_state`). The only thing the timeline does on its
// own is move the dragged handle or playhead under the pointer until Rust's
// answer arrives, so a drag never lags.
//
// Keys: Space play/pause, I / O set in / out at the playhead, ← / → step
// one frame, ↑ / Home go to the in point, ↓ / End go to the out point,
// Esc pauses, ⌘E exports, ⌘W closes.
//
// Main thread only; needs a running AppKit loop. One video editor at a time.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdint.h>

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
};

static const CGFloat kBarHeight = 48;
static const CGFloat kTimelineHeight = 44;
static const CGFloat kTimelineInset = 16;
// How close (points) to a handle a click must land to grab it.
static const CGFloat kHandleGrab = 7;

@class BugleVideoEditor;
static BugleVideoEditor *g_video_editor = nil;

// What is being dragged on the timeline.
typedef enum { DRAG_NONE, DRAG_PLAYHEAD, DRAG_IN, DRAG_OUT } DragKind;

@interface BugleTimeline : NSView
@property(nonatomic, weak) BugleVideoEditor *editor;
@property(nonatomic) double duration, playhead, inPoint, outPoint;
@property(nonatomic) DragKind drag;
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
@property(nonatomic, strong) NSPopUpButton *zoomPopup;
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
    return NSMakeRect(kTimelineInset, floor(NSMidY(b) - 9), MAX(1, NSWidth(b) - 2 * kTimelineInset), 18);
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
    [NSColor.controlAccentColor setStroke];
    NSBezierPath *frame = [NSBezierPath bezierPathWithRect:NSInsetRect(kept, 0.5, 0.5)];
    frame.lineWidth = 1;
    [frame stroke];
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
}

- (void)report:(NSEvent *)event {
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    double t = [self timeAt:p.x];
    BugleVideoEditor *e = self.editor;
    // Move what is dragged at once; Rust's answer (clamped) replaces it.
    switch (self.drag) {
        case DRAG_IN:
            self.inPoint = fmin(t, self.outPoint);
            [e emit:VEV_IN value:t arg:0];
            break;
        case DRAG_OUT:
            self.outPoint = fmax(t, self.inPoint);
            [e emit:VEV_OUT value:t arg:0];
            break;
        case DRAG_PLAYHEAD:
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
    [self report:event];
}

- (void)mouseDragged:(NSEvent *)event { [self report:event]; }

- (void)mouseUp:(NSEvent *)event {
    [self report:event];
    self.drag = DRAG_NONE;
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
    [NSApp deactivate];
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
/// frames are `w` x `h` pixels. `zoom_labels` and `preset_labels` fill the
/// two pop-up menus. Returns 1, or 0 when a video editor is already open
/// (the context is not taken then).
int32_t bugle_video_editor_open(const char *title, double duration, uint32_t w, uint32_t h, const char *const *zoom_labels,
                                int32_t n_zoom, const char *const *preset_labels, int32_t n_presets,
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

        NSTextField *zoomLabel = [NSTextField labelWithString:@"Zoom"];
        e.zoomPopup = vpopup(zoom_labels, n_zoom, e, @selector(zoomPicked:), @"Zoom in on clicks (needs a recording made with cursor events)");
        e.smoothBox = [NSButton checkboxWithTitle:@"Smooth pointer" target:e action:@selector(smoothToggled:)];
        e.smoothBox.toolTip = @"Draw the pointer along a smoothed path (recordings made with the pointer hidden)";
        e.smoothBox.refusesFirstResponder = YES;
        e.presetPopup = vpopup(preset_labels, n_presets, e, @selector(presetPicked:), @"Export format");
        e.exportButton = vbutton(@"Export", @"Export the clip as a new recording (⌘E)", e, VCMD_EXPORT);
        e.exportButton.keyEquivalent = @"";

        NSView *spacer = [[NSView alloc] initWithFrame:NSZeroRect];
        [spacer setContentHuggingPriority:1 forOrientation:NSLayoutConstraintOrientationHorizontal];
        NSStackView *stack = [NSStackView stackViewWithViews:@[
            playButton, time, e.inButton, e.outButton, spacer, zoomLabel, e.zoomPopup, e.smoothBox, e.presetPopup, e.exportButton
        ]];
        stack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
        stack.spacing = 8;
        stack.distribution = NSStackViewDistributionFill;
        stack.edgeInsets = NSEdgeInsetsMake(0, 12, 0, 12);
        [stack setCustomSpacing:4 afterView:e.inButton];
        [stack setCustomSpacing:4 afterView:zoomLabel];
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

/// Replace the state of every control. `time_text` is the line next to the
/// play button. A busy window (an export is running) disables its controls
/// and refuses to close.
void bugle_video_editor_set_state(double playhead, double in_point, double out_point, int32_t playing, int32_t zoom,
                                  int32_t smooth, int32_t preset, int32_t can_zoom, int32_t can_smooth, int32_t busy,
                                  const char *time_text) {
    @autoreleasepool {
        BugleVideoEditor *e = g_video_editor;
        if (!e) return;
        BugleTimeline *t = e.timeline;
        // A handle being dragged follows the pointer; the rest follows Rust.
        if (t.drag != DRAG_PLAYHEAD) t.playhead = playhead;
        if (t.drag != DRAG_IN) t.inPoint = in_point;
        if (t.drag != DRAG_OUT) t.outPoint = out_point;
        [t setNeedsDisplay:YES];
        NSString *symbol = playing ? @"pause.fill" : @"play.fill";
        NSImage *img = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:playing ? @"Pause" : @"Play"];
        if (img) e.playButton.image = img;
        else e.playButton.title = playing ? @"Pause" : @"Play";
        e.timeLabel.stringValue = vstr(time_text);
        if (zoom >= 0 && zoom < e.zoomPopup.numberOfItems) [e.zoomPopup selectItemAtIndex:zoom];
        if (preset >= 0 && preset < e.presetPopup.numberOfItems) [e.presetPopup selectItemAtIndex:preset];
        e.smoothBox.state = smooth ? NSControlStateValueOn : NSControlStateValueOff;
        e.busy = busy != 0;
        BOOL on = !e.busy;
        e.playButton.enabled = on;
        e.inButton.enabled = on;
        e.outButton.enabled = on;
        e.zoomPopup.enabled = on && can_zoom;
        e.smoothBox.enabled = on && can_smooth;
        e.presetPopup.enabled = on;
        e.exportButton.enabled = on;
        e.exportButton.title = e.busy ? @"Exporting…" : @"Export";
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
