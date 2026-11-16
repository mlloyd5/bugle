// Bugle markup editor window.
//
// A regular window with a toolbar (tools, colours, stroke sizes, undo/redo,
// Copy, Save) above a canvas that shows an image scaled to fit. The window
// owns no editing policy: pointer and key input is reported to Rust through a
// callback in image pixel coordinates, and Rust answers by replacing the
// image (`bugle_editor_set_image`), the overlay drawn on top of it
// (`bugle_editor_set_overlay`) and the toolbar state
// (`bugle_editor_set_state`). The only thing the window does by itself is
// host the text field for typing a label and ask before closing with
// unsaved changes.
//
// Main thread only; needs a running AppKit loop. One editor at a time.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdint.h>

typedef void (*bugle_editor_callback_t)(void *ctx, int32_t kind, double x, double y, int32_t arg, const char *text);
typedef void (*bugle_editor_free_t)(void *ctx);

// Event kinds. Mirrored in src/editor.rs.
enum {
    EV_DOWN = 0,     // x, y image px; arg = shift (bit 0) | click count << 8
    EV_DRAG = 1,     // x, y; arg = shift
    EV_UP = 2,       // x, y; arg = shift
    EV_TOOL = 3,     // arg = tool index
    EV_COLOR = 4,    // arg = colour index
    EV_WIDTH = 5,    // arg = size index
    EV_COMMAND = 6,  // arg = CMD_*
    EV_TEXT = 7,     // text = what was typed (may be empty)
    EV_KEY = 8,      // arg = unicode scalar of an unmodified key
    EV_SCALE = 9,    // x = image pixels per point at the current zoom
};
enum { CMD_UNDO = 0, CMD_REDO = 1, CMD_DELETE = 2, CMD_SAVE = 3, CMD_COPY = 4, CMD_ESCAPE = 5 };

// Overlay primitive kinds. Mirrored in src/editor.rs.
enum { PRIM_POLYLINE = 0, PRIM_ARROW = 1, PRIM_RECT = 2, PRIM_ELLIPSE = 3, PRIM_FILL = 4, PRIM_SELECTION = 5, PRIM_HANDLE = 6 };

static const CGFloat kToolbarHeight = 44;
static const CGFloat kCanvasMargin = 24;

@class BugleEditor;
static BugleEditor *g_editor = nil;

@interface BugleEditorCanvas : NSView
@property(nonatomic, weak) BugleEditor *editor;
@end

@interface BugleEditorWindow : NSWindow
@property(nonatomic, weak) BugleEditor *editor;
@end

@interface BugleEditorSwatch : NSButton
@property(nonatomic, strong) NSColor *color;
@property(nonatomic) BOOL chosen;
@end

@interface BugleEditor : NSObject <NSWindowDelegate, NSTextFieldDelegate>
@property(nonatomic, strong) BugleEditorWindow *window;
@property(nonatomic, strong) BugleEditorCanvas *canvas;
@property(nonatomic) bugle_editor_callback_t cb;
@property(nonatomic) void *ctx;
@property(nonatomic) bugle_editor_free_t freeCtx;
@property(nonatomic) CGImageRef image;  // owned
@property(nonatomic) CGFloat imgW, imgH;
@property(nonatomic, strong) NSData *overlay;  // doubles, see bugle_editor_set_overlay
@property(nonatomic, strong) NSSegmentedControl *tools;
@property(nonatomic, strong) NSSegmentedControl *widths;
@property(nonatomic, strong) NSMutableArray<BugleEditorSwatch *> *swatches;
@property(nonatomic, strong) NSButton *undoButton, *redoButton;
@property(nonatomic) int32_t tool;
@property(nonatomic) BOOL dirty;
@property(nonatomic) BOOL closing;
@property(nonatomic) BOOL busy;  // a save is running: no closing
@property(nonatomic, strong) NSTextField *field;  // label being typed, or nil
@property(nonatomic) CGFloat lastScale;
- (void)emit:(int32_t)kind x:(double)x y:(double)y arg:(int32_t)arg text:(const char *)text;
- (void)command:(int32_t)cmd;
- (void)endText:(BOOL)commit;
- (void)teardown;
@end

// ---------------------------------------------------------------------------
// Swatch
// ---------------------------------------------------------------------------

@implementation BugleEditorSwatch
- (void)drawRect:(NSRect)dirty {
    NSRect r = NSInsetRect(self.bounds, 4, 4);
    if (self.chosen) {
        [[NSColor controlAccentColor] setStroke];
        NSBezierPath *ring = [NSBezierPath bezierPathWithOvalInRect:NSInsetRect(self.bounds, 1.5, 1.5)];
        ring.lineWidth = 2;
        [ring stroke];
    }
    [self.color setFill];
    NSBezierPath *dot = [NSBezierPath bezierPathWithOvalInRect:r];
    [dot fill];
    [[NSColor colorWithWhite:0.5 alpha:0.6] setStroke];
    dot.lineWidth = 0.5;
    [dot stroke];
}
@end

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

@implementation BugleEditorCanvas
- (BOOL)isFlipped { return YES; }
- (BOOL)acceptsFirstResponder { return YES; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }

/// Points per image pixel at the current size.
- (CGFloat)zoom {
    BugleEditor *e = self.editor;
    if (!e || e.imgW <= 0 || e.imgH <= 0) return 1;
    CGFloat sx = (NSWidth(self.bounds) - 2 * kCanvasMargin) / e.imgW;
    CGFloat sy = (NSHeight(self.bounds) - 2 * kCanvasMargin) / e.imgH;
    return MAX(MIN(sx, sy), 0.01);
}

- (NSRect)imageRect {
    BugleEditor *e = self.editor;
    CGFloat z = [self zoom];
    CGFloat w = e.imgW * z, h = e.imgH * z;
    return NSMakeRect(floor((NSWidth(self.bounds) - w) / 2), floor((NSHeight(self.bounds) - h) / 2), w, h);
}

- (NSPoint)toView:(double)x y:(double)y {
    NSRect r = [self imageRect];
    CGFloat z = [self zoom];
    return NSMakePoint(r.origin.x + x * z, r.origin.y + y * z);
}

- (NSPoint)toImage:(NSEvent *)event {
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    NSRect r = [self imageRect];
    CGFloat z = [self zoom];
    return NSMakePoint((p.x - r.origin.x) / z, (p.y - r.origin.y) / z);
}

- (void)setFrameSize:(NSSize)size {
    [super setFrameSize:size];
    [self reportScale];
}

- (void)reportScale {
    BugleEditor *e = self.editor;
    CGFloat z = [self zoom];
    if (!e || fabs(z - e.lastScale) < 1e-6) return;
    e.lastScale = z;
    [e emit:EV_SCALE x:1.0 / z y:0 arg:0 text:NULL];
}

- (void)resetCursorRects {
    NSCursor *c = self.editor.tool == 0 ? [NSCursor arrowCursor] : [NSCursor crosshairCursor];
    [self addCursorRect:[self imageRect] cursor:c];
}

- (void)drawRect:(NSRect)dirty {
    BugleEditor *e = self.editor;
    [[NSColor colorWithWhite:0.13 alpha:1] setFill];
    NSRectFill(self.bounds);
    if (!e) return;
    NSRect r = [self imageRect];
    CGContextRef cg = [NSGraphicsContext currentContext].CGContext;
    if (e.image) {
        // A soft edge so a dark capture does not melt into the backdrop.
        [[NSColor colorWithWhite:0 alpha:0.5] setFill];
        NSRectFill(NSInsetRect(r, -1, -1));
        CGContextSaveGState(cg);
        CGContextSetInterpolationQuality(cg, kCGInterpolationHigh);
        CGContextTranslateCTM(cg, r.origin.x, r.origin.y + r.size.height);
        CGContextScaleCTM(cg, 1, -1);
        CGContextDrawImage(cg, CGRectMake(0, 0, r.size.width, r.size.height), e.image);
        CGContextRestoreGState(cg);
    }
    [self drawOverlay];
}

- (void)drawOverlay {
    NSData *data = self.editor.overlay;
    if (!data) return;
    const double *v = data.bytes;
    size_t n = data.length / sizeof(double), i = 0;
    CGFloat z = [self zoom];
    // Each primitive: kind, r, g, b, a (0..1), width (image px), point count,
    // then that many x, y pairs.
    while (i + 7 <= n) {
        int kind = (int)v[i];
        NSColor *color = [NSColor colorWithSRGBRed:v[i + 1] green:v[i + 2] blue:v[i + 3] alpha:v[i + 4]];
        CGFloat width = MAX(1.0, v[i + 5] * z);
        double count = v[i + 6];
        i += 7;
        if (!(count >= 0) || count > (double)(n - i) / 2) return;
        size_t np = (size_t)count;
        const double *p = v + i;
        i += np * 2;
        switch (kind) {
            case PRIM_POLYLINE:
            case PRIM_ARROW: {
                if (np < 1) break;
                NSBezierPath *path = [NSBezierPath bezierPath];
                path.lineWidth = width;
                path.lineCapStyle = NSLineCapStyleRound;
                path.lineJoinStyle = NSLineJoinStyleRound;
                [path moveToPoint:[self toView:p[0] y:p[1]]];
                if (np == 1) [path lineToPoint:[self toView:p[0] y:p[1]]];
                for (size_t k = 1; k < np; k++) [path lineToPoint:[self toView:p[2 * k] y:p[2 * k + 1]]];
                [color setStroke];
                [path stroke];
                if (kind == PRIM_ARROW && np >= 2) {
                    NSPoint a = [self toView:p[0] y:p[1]], b = [self toView:p[2] y:p[3]];
                    CGFloat dx = b.x - a.x, dy = b.y - a.y, len = hypot(dx, dy);
                    if (len > 0.5) {
                        dx /= len;
                        dy /= len;
                        CGFloat head = MIN(MAX(width * 4.5, 10), len), half = head * 0.45;
                        NSPoint base = NSMakePoint(b.x - dx * head, b.y - dy * head);
                        NSBezierPath *tri = [NSBezierPath bezierPath];
                        [tri moveToPoint:NSMakePoint(b.x + dx * width * 0.5, b.y + dy * width * 0.5)];
                        [tri lineToPoint:NSMakePoint(base.x - dy * half, base.y + dx * half)];
                        [tri lineToPoint:NSMakePoint(base.x + dy * half, base.y - dx * half)];
                        [tri closePath];
                        [color setFill];
                        [tri fill];
                    }
                }
                break;
            }
            case PRIM_RECT:
            case PRIM_ELLIPSE:
            case PRIM_FILL:
            case PRIM_SELECTION: {
                if (np < 2) break;
                NSPoint o = [self toView:p[0] y:p[1]];
                NSRect rr = NSMakeRect(o.x, o.y, p[2] * z, p[3] * z);
                if (kind == PRIM_FILL) {
                    [color setFill];
                    NSRectFillUsingOperation(rr, NSCompositingOperationSourceOver);
                } else if (kind == PRIM_SELECTION) {
                    rr = NSInsetRect(rr, -3, -3);
                    NSBezierPath *path = [NSBezierPath bezierPathWithRect:rr];
                    path.lineWidth = 1;
                    [[NSColor colorWithWhite:1 alpha:0.9] setStroke];
                    [path stroke];
                    CGFloat dash[2] = {4, 4};
                    [path setLineDash:dash count:2 phase:0];
                    [[NSColor controlAccentColor] setStroke];
                    [path stroke];
                } else {
                    NSBezierPath *path = kind == PRIM_RECT ? [NSBezierPath bezierPathWithRect:rr]
                                                           : [NSBezierPath bezierPathWithOvalInRect:rr];
                    path.lineWidth = width;
                    [color setStroke];
                    [path stroke];
                }
                break;
            }
            case PRIM_HANDLE: {
                if (np < 1) break;
                NSPoint c = [self toView:p[0] y:p[1]];
                NSBezierPath *dot = [NSBezierPath bezierPathWithOvalInRect:NSMakeRect(c.x - 4.5, c.y - 4.5, 9, 9)];
                [[NSColor whiteColor] setFill];
                [dot fill];
                dot.lineWidth = 1.5;
                [[NSColor controlAccentColor] setStroke];
                [dot stroke];
                break;
            }
            default:
                break;
        }
    }
}

static int32_t mouse_flags(NSEvent *e) { return (e.modifierFlags & NSEventModifierFlagShift) ? 1 : 0; }

- (void)mouseDown:(NSEvent *)event {
    BugleEditor *e = self.editor;
    // A click outside the text field finishes the label first.
    if (e.field) [e endText:YES];
    [self.window makeFirstResponder:self];
    NSPoint p = [self toImage:event];
    [e emit:EV_DOWN x:p.x y:p.y arg:mouse_flags(event) | ((int32_t)MIN(event.clickCount, 3) << 8) text:NULL];
}
- (void)mouseDragged:(NSEvent *)event {
    NSPoint p = [self toImage:event];
    [self.editor emit:EV_DRAG x:p.x y:p.y arg:mouse_flags(event) text:NULL];
}
- (void)mouseUp:(NSEvent *)event {
    NSPoint p = [self toImage:event];
    [self.editor emit:EV_UP x:p.x y:p.y arg:mouse_flags(event) text:NULL];
}

- (void)keyDown:(NSEvent *)event {
    BugleEditor *e = self.editor;
    NSEventModifierFlags mods = event.modifierFlags & (NSEventModifierFlagCommand | NSEventModifierFlagControl | NSEventModifierFlagOption);
    if (mods) return [super keyDown:event];
    switch (event.keyCode) {
        case 51:   // delete
        case 117:  // forward delete
            return [e command:CMD_DELETE];
        case 53:  // esc
            return [e command:CMD_ESCAPE];
        default:
            break;
    }
    NSString *chars = event.charactersIgnoringModifiers.lowercaseString;
    if (chars.length == 1) {
        unichar c = [chars characterAtIndex:0];
        if (c >= 0x20 && c < 0x7F) return [e emit:EV_KEY x:0 y:0 arg:c text:NULL];
    }
    [super keyDown:event];
}
- (void)cancelOperation:(id)sender { [self.editor command:CMD_ESCAPE]; }
@end

// ---------------------------------------------------------------------------
// Window: the app has no main menu, so ⌘-keys are handled here.
// ---------------------------------------------------------------------------

@implementation BugleEditorWindow
- (BOOL)performKeyEquivalent:(NSEvent *)event {
    NSEventModifierFlags mods = event.modifierFlags & NSEventModifierFlagDeviceIndependentFlagsMask;
    if (!(mods & NSEventModifierFlagCommand) || (mods & (NSEventModifierFlagControl | NSEventModifierFlagOption))) {
        return [super performKeyEquivalent:event];
    }
    BOOL shift = (mods & NSEventModifierFlagShift) != 0;
    NSString *key = event.charactersIgnoringModifiers.lowercaseString;
    BugleEditor *e = self.editor;
    if ([self.firstResponder isKindOfClass:[NSTextView class]]) {
        // Typing a label: the usual editing keys go to the text field.
        SEL action = NULL;
        if ([key isEqualToString:@"c"]) action = @selector(copy:);
        else if ([key isEqualToString:@"v"]) action = @selector(paste:);
        else if ([key isEqualToString:@"x"]) action = @selector(cut:);
        else if ([key isEqualToString:@"a"]) action = @selector(selectAll:);
        else if ([key isEqualToString:@"z"]) action = shift ? NSSelectorFromString(@"redo:") : NSSelectorFromString(@"undo:");
        if (action) return [NSApp sendAction:action to:nil from:self];
        if ([key isEqualToString:@"s"]) {
            [e endText:YES];
            [e command:CMD_SAVE];
            return YES;
        }
        return [super performKeyEquivalent:event];
    }
    if ([key isEqualToString:@"z"]) {
        [e command:shift ? CMD_REDO : CMD_UNDO];
    } else if ([key isEqualToString:@"s"]) {
        [e command:CMD_SAVE];
    } else if ([key isEqualToString:@"c"]) {
        [e command:CMD_COPY];
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

@implementation BugleEditor
- (void)dealloc {
    if (_image) CGImageRelease(_image);
}

- (void)emit:(int32_t)kind x:(double)x y:(double)y arg:(int32_t)arg text:(const char *)text {
    if (self.closing || !self.cb || !self.ctx) return;
    self.cb(self.ctx, kind, x, y, arg, text);
}

- (void)command:(int32_t)cmd { [self emit:EV_COMMAND x:0 y:0 arg:cmd text:NULL]; }

- (void)toolPicked:(NSSegmentedControl *)sender { [self emit:EV_TOOL x:0 y:0 arg:(int32_t)sender.selectedSegment text:NULL]; }
- (void)widthPicked:(NSSegmentedControl *)sender { [self emit:EV_WIDTH x:0 y:0 arg:(int32_t)sender.selectedSegment text:NULL]; }
- (void)colorPicked:(NSButton *)sender { [self emit:EV_COLOR x:0 y:0 arg:(int32_t)sender.tag text:NULL]; }
- (void)buttonCommand:(NSButton *)sender {
    if (self.field) [self endText:YES];
    [self command:(int32_t)sender.tag];
}

// ---- label text field ----

- (void)beginTextAt:(NSPoint)at fontPx:(double)fontPx initial:(NSString *)initial color:(NSColor *)color {
    if (self.field) {
        // Replaced without a report: this runs inside the callback, which
        // must not be re-entered.
        NSTextField *old = self.field;
        self.field = nil;
        old.delegate = nil;
        [old removeFromSuperview];
    }
    CGFloat z = [self.canvas zoom];
    CGFloat size = MAX(9, fontPx * z);
    NSFont *font = [NSFont fontWithName:@"Arial-BoldMT" size:size] ?: [NSFont boldSystemFontOfSize:size];
    NSPoint o = [self.canvas toView:at.x y:at.y];
    CGFloat h = ceil([[NSLayoutManager new] defaultLineHeightForFont:font]) + 4;
    CGFloat w = MAX(120, NSWidth(self.canvas.bounds) - o.x - 8);
    NSTextField *f = [[NSTextField alloc] initWithFrame:NSMakeRect(o.x - 2, o.y - 2, w, h)];
    f.font = font;
    f.textColor = color;
    f.stringValue = initial ?: @"";
    f.bordered = NO;
    f.bezeled = NO;
    f.drawsBackground = YES;
    f.backgroundColor = [NSColor colorWithWhite:0 alpha:0.35];
    f.focusRingType = NSFocusRingTypeNone;
    f.placeholderString = @"Type, then Return";
    f.cell.wraps = NO;
    f.cell.scrollable = YES;
    f.delegate = self;
    [self.canvas addSubview:f];
    self.field = f;
    [self.window makeFirstResponder:f];
}

/// Remove the text field and report what was typed (empty when cancelled).
- (void)endText:(BOOL)commit {
    NSTextField *f = self.field;
    if (!f) return;
    self.field = nil;  // first: removing the field ends editing, which calls back here
    NSString *text = commit ? [f.stringValue copy] : @"";
    f.delegate = nil;
    [f removeFromSuperview];
    [self.window makeFirstResponder:self.canvas];
    [self emit:EV_TEXT x:0 y:0 arg:0 text:text.UTF8String];
}

- (BOOL)control:(NSControl *)control textView:(NSTextView *)textView doCommandBySelector:(SEL)sel {
    if (sel == @selector(insertNewline:)) {
        [self endText:YES];
        return YES;
    }
    if (sel == @selector(cancelOperation:)) {
        [self endText:NO];
        return YES;
    }
    return NO;
}

- (void)controlTextDidEndEditing:(NSNotification *)note {
    if (note.object == self.field) [self endText:YES];
}

// ---- closing ----

- (BOOL)windowShouldClose:(NSWindow *)sender {
    if (self.busy) {
        NSBeep();
        return NO;
    }
    if (self.field) [self endText:YES];
    if (!self.dirty) {
        [self teardown];
        return NO;
    }
    NSAlert *alert = [[NSAlert alloc] init];
    alert.messageText = @"Save your annotations?";
    alert.informativeText = @"Saving writes an annotated copy; the original capture is left as it is.";
    [alert addButtonWithTitle:@"Save"];
    [alert addButtonWithTitle:@"Cancel"];
    [alert addButtonWithTitle:@"Discard"];
    __weak BugleEditor *weak = self;
    [alert beginSheetModalForWindow:self.window
                  completionHandler:^(NSModalResponse r) {
                      BugleEditor *e = weak;
                      if (!e || e.closing) return;
                      if (r == NSAlertFirstButtonReturn) [e command:CMD_SAVE];
                      else if (r == NSAlertThirdButtonReturn) [e teardown];
                  }];
    return NO;
}

/// Take the window down and release the Rust context on a later turn, so a
/// close requested from inside the callback never frees the running handler.
- (void)teardown {
    if (self.closing) return;
    self.closing = YES;
    __attribute__((objc_precise_lifetime)) BugleEditor *keep = self;
    if (g_editor == self) g_editor = nil;
    NSWindow *w = self.window;
    if (w.attachedSheet) [w endSheet:w.attachedSheet];
    w.delegate = nil;
    [w orderOut:nil];
    void *ctx = self.ctx;
    bugle_editor_free_t f = self.freeCtx;
    self.cb = NULL;
    self.ctx = NULL;
    self.freeCtx = NULL;
    // Hand keyboard focus back to the app the user came from.
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

static NSString *str(const char *s) { return s ? ([NSString stringWithUTF8String:s] ?: @"") : @""; }

static NSButton *text_button(NSString *title, BugleEditor *target, int32_t tag) {
    NSButton *b = [NSButton buttonWithTitle:title target:target action:@selector(buttonCommand:)];
    b.bezelStyle = NSBezelStyleRounded;
    b.tag = tag;
    return b;
}

static NSButton *symbol_button(NSString *symbol, NSString *tip, BugleEditor *target, int32_t tag) {
    NSImage *img = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:tip];
    NSButton *b = img ? [NSButton buttonWithImage:img target:target action:@selector(buttonCommand:)]
                      : [NSButton buttonWithTitle:tip target:target action:@selector(buttonCommand:)];
    b.bezelStyle = NSBezelStyleTexturedRounded;
    b.toolTip = tip;
    b.tag = tag;
    return b;
}

/// Open the editor for an image of `img_w` x `img_h` pixels. `tool_labels`
/// and `tool_symbols` (SF Symbol names) describe the tool buttons, `colors`
/// (0xRRGGBBAA) the swatches, `width_labels` the stroke sizes. Returns 1, or
/// 0 when an editor is already open (the context is not taken then).
int32_t bugle_editor_open(const char *title, uint32_t img_w, uint32_t img_h, const char *const *tool_labels,
                          const char *const *tool_symbols, int32_t n_tools, const uint32_t *colors, int32_t n_colors,
                          const char *const *width_labels, int32_t n_widths, bugle_editor_callback_t cb, void *ctx,
                          bugle_editor_free_t free_ctx) {
    @autoreleasepool {
        if (g_editor || !cb || img_w == 0 || img_h == 0) return 0;
        NSApplication *app = [NSApplication sharedApplication];
        BugleEditor *e = [[BugleEditor alloc] init];
        e.cb = cb;
        e.ctx = ctx;
        e.freeCtx = free_ctx;
        e.imgW = img_w;
        e.imgH = img_h;

        // Size the canvas so the image shows at its natural (Retina) size
        // when it fits, and scaled down to most of the screen when not.
        NSScreen *screen = [NSScreen mainScreen] ?: [NSScreen screens].firstObject;
        NSRect vf = screen.visibleFrame;
        CGFloat backing = MAX(1.0, screen.backingScaleFactor);
        CGFloat maxW = NSWidth(vf) * 0.86 - 2 * kCanvasMargin, maxH = NSHeight(vf) * 0.86 - kToolbarHeight - 2 * kCanvasMargin - 28;
        CGFloat natW = img_w / backing, natH = img_h / backing;
        CGFloat fit = MIN(1.0, MIN(maxW / natW, maxH / natH));
        CGFloat cw = MAX(860, ceil(natW * fit) + 2 * kCanvasMargin), ch = MAX(380, ceil(natH * fit) + 2 * kCanvasMargin);
        NSRect content = NSMakeRect(0, 0, cw, ch + kToolbarHeight);

        BugleEditorWindow *win = [[BugleEditorWindow alloc]
            initWithContentRect:content
                      styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable |
                                NSWindowStyleMaskMiniaturizable
                        backing:NSBackingStoreBuffered
                          defer:NO];
        win.releasedWhenClosed = NO;
        win.title = str(title);
        win.minSize = NSMakeSize(860, 360);
        win.editor = e;
        win.delegate = e;
        e.window = win;

        NSView *root = [[NSView alloc] initWithFrame:content];
        BugleEditorCanvas *canvas = [[BugleEditorCanvas alloc] initWithFrame:NSMakeRect(0, 0, cw, ch)];
        canvas.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        canvas.wantsLayer = YES;
        canvas.editor = e;
        e.canvas = canvas;
        [root addSubview:canvas];

        NSVisualEffectView *bar = [[NSVisualEffectView alloc] initWithFrame:NSMakeRect(0, ch, cw, kToolbarHeight)];
        bar.material = NSVisualEffectMaterialTitlebar;
        bar.blendingMode = NSVisualEffectBlendingModeWithinWindow;
        bar.autoresizingMask = NSViewWidthSizable | NSViewMinYMargin;
        [root addSubview:bar];

        NSSegmentedControl *tools = [[NSSegmentedControl alloc] initWithFrame:NSZeroRect];
        tools.segmentCount = n_tools;
        tools.trackingMode = NSSegmentSwitchTrackingSelectOne;
        tools.segmentStyle = NSSegmentStyleTexturedRounded;
        for (int32_t i = 0; i < n_tools; i++) {
            NSString *label = str(tool_labels[i]);
            NSImage *img = [NSImage imageWithSystemSymbolName:str(tool_symbols[i]) accessibilityDescription:label];
            if (img) [tools setImage:img forSegment:i];
            else [tools setLabel:label forSegment:i];
            [tools setToolTip:label forSegment:i];
            [tools setWidth:30 forSegment:i];
        }
        tools.target = e;
        tools.action = @selector(toolPicked:);
        e.tools = tools;

        NSStackView *swatches = [[NSStackView alloc] initWithFrame:NSZeroRect];
        swatches.orientation = NSUserInterfaceLayoutOrientationHorizontal;
        swatches.spacing = 0;
        e.swatches = [NSMutableArray array];
        for (int32_t i = 0; i < n_colors; i++) {
            uint32_t c = colors[i];
            BugleEditorSwatch *s = [[BugleEditorSwatch alloc] initWithFrame:NSMakeRect(0, 0, 22, 22)];
            s.color = [NSColor colorWithSRGBRed:((c >> 24) & 0xFF) / 255.0
                                          green:((c >> 16) & 0xFF) / 255.0
                                           blue:((c >> 8) & 0xFF) / 255.0
                                          alpha:1.0];
            s.title = @"";
            s.bordered = NO;
            s.tag = i;
            s.target = e;
            s.action = @selector(colorPicked:);
            [s.widthAnchor constraintEqualToConstant:22].active = YES;
            [s.heightAnchor constraintEqualToConstant:22].active = YES;
            [swatches addArrangedSubview:s];
            [e.swatches addObject:s];
        }

        NSSegmentedControl *widths = [[NSSegmentedControl alloc] initWithFrame:NSZeroRect];
        widths.segmentCount = n_widths;
        widths.trackingMode = NSSegmentSwitchTrackingSelectOne;
        widths.segmentStyle = NSSegmentStyleTexturedRounded;
        for (int32_t i = 0; i < n_widths; i++) {
            [widths setLabel:str(width_labels[i]) forSegment:i];
            [widths setWidth:26 forSegment:i];
        }
        [widths setToolTip:@"Stroke and text size" forSegment:0];
        widths.target = e;
        widths.action = @selector(widthPicked:);
        e.widths = widths;

        e.undoButton = symbol_button(@"arrow.uturn.backward", @"Undo (⌘Z)", e, CMD_UNDO);
        e.redoButton = symbol_button(@"arrow.uturn.forward", @"Redo (⇧⌘Z)", e, CMD_REDO);
        NSView *spacer = [[NSView alloc] initWithFrame:NSZeroRect];
        [spacer setContentHuggingPriority:1 forOrientation:NSLayoutConstraintOrientationHorizontal];
        NSButton *copy = text_button(@"Copy", e, CMD_COPY);
        copy.toolTip = @"Copy the annotated image (⌘C)";
        NSButton *save = text_button(@"Save", e, CMD_SAVE);
        save.toolTip = @"Save an annotated copy (⌘S)";
        save.keyEquivalent = @"";

        NSStackView *stack = [NSStackView stackViewWithViews:@[ tools, swatches, widths, e.undoButton, e.redoButton, spacer, copy, save ]];
        stack.orientation = NSUserInterfaceLayoutOrientationHorizontal;
        stack.spacing = 10;
        stack.distribution = NSStackViewDistributionFill;  // the spacer takes the slack: Copy and Save sit right
        stack.edgeInsets = NSEdgeInsetsMake(0, 12, 0, 12);
        [stack setCustomSpacing:4 afterView:e.undoButton];
        // A stack view made this way positions itself by constraints only.
        [bar addSubview:stack];
        [NSLayoutConstraint activateConstraints:@[
            [stack.leadingAnchor constraintEqualToAnchor:bar.leadingAnchor],
            [stack.trailingAnchor constraintEqualToAnchor:bar.trailingAnchor],
            [stack.topAnchor constraintEqualToAnchor:bar.topAnchor],
            [stack.bottomAnchor constraintEqualToAnchor:bar.bottomAnchor],
        ]];

        win.contentView = root;
        [win center];
        g_editor = e;
        [win makeFirstResponder:canvas];
        [app activateIgnoringOtherApps:YES];
        [win makeKeyAndOrderFront:nil];
        [canvas reportScale];
        return 1;
    }
}

/// Replace the image on the canvas. `rgba` is copied (RGBA8, not
/// premultiplied, `w * h * 4` bytes).
void bugle_editor_set_image(const uint8_t *rgba, uint32_t w, uint32_t h) {
    @autoreleasepool {
        BugleEditor *e = g_editor;
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
        if (e.image) CGImageRelease(e.image);
        e.image = img;
        [e.canvas setNeedsDisplay:YES];
    }
}

/// Replace the overlay drawn above the image: `n` doubles holding
/// primitives, each `kind, r, g, b, a, width, point_count` followed by
/// `point_count` x, y pairs in image pixels (see the PRIM_* kinds; rect-like
/// kinds take origin then size). `n == 0` clears it.
void bugle_editor_set_overlay(const double *values, size_t n) {
    @autoreleasepool {
        BugleEditor *e = g_editor;
        if (!e) return;
        e.overlay = (values && n) ? [NSData dataWithBytes:values length:n * sizeof(double)] : nil;
        [e.canvas setNeedsDisplay:YES];
    }
}

/// Reflect the editor state in the toolbar and title bar.
void bugle_editor_set_state(int32_t tool, int32_t color, int32_t width, int32_t can_undo, int32_t can_redo, int32_t dirty) {
    @autoreleasepool {
        BugleEditor *e = g_editor;
        if (!e) return;
        if (tool >= 0 && tool < e.tools.segmentCount) e.tools.selectedSegment = tool;
        if (width >= 0 && width < e.widths.segmentCount) e.widths.selectedSegment = width;
        for (BugleEditorSwatch *s in e.swatches) {
            BOOL chosen = s.tag == color;
            if (s.chosen != chosen) {
                s.chosen = chosen;
                [s setNeedsDisplay:YES];
            }
        }
        e.undoButton.enabled = can_undo != 0;
        e.redoButton.enabled = can_redo != 0;
        e.dirty = dirty != 0;
        e.window.documentEdited = e.dirty;
        if (e.tool != tool) {
            e.tool = tool;
            [e.window invalidateCursorRectsForView:e.canvas];
        }
    }
}

/// Put a text field on the canvas at image pixel (x, y) for typing a label
/// `font_px` image pixels tall. The result arrives as one EV_TEXT event
/// (empty text when the user cancels).
void bugle_editor_begin_text(double x, double y, double font_px, const char *initial, uint32_t rgba) {
    @autoreleasepool {
        BugleEditor *e = g_editor;
        if (!e) return;
        NSColor *c = [NSColor colorWithSRGBRed:((rgba >> 24) & 0xFF) / 255.0
                                         green:((rgba >> 16) & 0xFF) / 255.0
                                          blue:((rgba >> 8) & 0xFF) / 255.0
                                         alpha:1.0];
        [e beginTextAt:NSMakePoint(x, y) fontPx:font_px initial:str(initial) color:c];
    }
}

/// While busy the window refuses to close (a save is being written).
void bugle_editor_set_busy(int32_t busy) { g_editor.busy = busy != 0; }

/// Close the editor without asking. Safe to call from inside the callback.
void bugle_editor_close(void) {
    @autoreleasepool {
        __attribute__((objc_precise_lifetime)) BugleEditor *e = g_editor;
        [e teardown];
    }
}

int32_t bugle_editor_active(void) { return g_editor ? 1 : 0; }

/// Bring the open editor to the front.
void bugle_editor_focus(void) {
    @autoreleasepool {
        BugleEditor *e = g_editor;
        if (!e) return;
        [NSApp activateIgnoringOtherApps:YES];
        [e.window makeKeyAndOrderFront:nil];
    }
}

/// Deliver an event to the callback as if it came from the window. For
/// smoke tests, which cannot click.
void bugle_editor_inject(int32_t kind, double x, double y, int32_t arg, const char *text) {
    @autoreleasepool {
        [g_editor emit:kind x:x y:y arg:arg text:text];
    }
}
