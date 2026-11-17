// Bugle quick-access panels.
//
// Non-activating floating panels shown by the menu-bar app after a capture:
// the thumbnail, two lines of text, a row of buttons and a close button. A
// panel owns no policy: every click is reported to Rust through a callback and
// the panel closes. Which buttons exist, what they do and when a panel times
// out are decided by `bugle-app` (`quick_access.rs`).
//
// Several panels stack up: the newest sits in the corner and the older ones
// move away from it (up from a bottom corner, down from a top one), sliding
// back together when one closes. Rust names each panel by an id of its own
// choosing; showing a panel with an id that is up replaces that panel.
//
// Main thread only; needs a running AppKit loop.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdint.h>

typedef void (*bugle_panel_callback_t)(void *ctx, int32_t choice);
typedef void (*bugle_panel_free_t)(void *ctx);

// Choice codes passed to the callback besides button indices (>= 0).
enum { BUGLE_PANEL_CLOSE = -1, BUGLE_PANEL_THUMBNAIL = -2 };

static const CGFloat kPanelHeight = 104;
// Distance from the screen edges, and between stacked panels.
static const CGFloat kPanelMargin = 16, kPanelGap = 8;

@interface BuglePanelEntry : NSObject
@property(nonatomic) uint64_t ident;
@property(nonatomic, strong) NSPanel *panel;
@property(nonatomic, strong) NSImageView *thumb;
@property(nonatomic) bugle_panel_callback_t cb;
@property(nonatomic) void *ctx;
@property(nonatomic) bugle_panel_free_t freeCtx;
@end
@implementation BuglePanelEntry
@end

// The panels on screen, oldest first.
static NSMutableArray<BuglePanelEntry *> *g_panels = nil;
// Panels still fading out: off limits for clicks, and ordered out at once
// when the app needs the screen clear (before a capture).
static NSMutableArray<NSPanel *> *g_panel_fading = nil;
// Set while a capture runs: the panels are ordered out but kept.
static BOOL g_panels_hidden = NO;
// Where the stack sits; the newest panel decides.
static int32_t g_panel_corner = 0;
static NSScreen *g_panel_screen = nil;

static BuglePanelEntry *panel_by_id(uint64_t ident) {
    for (BuglePanelEntry *e in g_panels) {
        if (e.ident == ident) return e;
    }
    return nil;
}

/// The live panel a control belongs to; nil for one that is closing.
static BuglePanelEntry *panel_by_window(NSWindow *w) {
    if (!w) return nil;
    for (BuglePanelEntry *e in g_panels) {
        if (e.panel == w) return e;
    }
    return nil;
}

/// Release the Rust context exactly once.
static void panel_release_ctx(BuglePanelEntry *e) {
    void *ctx = e.ctx;
    bugle_panel_free_t f = e.freeCtx;
    e.ctx = NULL;
    e.freeCtx = NULL;
    e.cb = NULL;
    if (ctx && f) f(ctx);
}

static NSScreen *screen_under_mouse(void) {
    NSPoint m = [NSEvent mouseLocation];
    for (NSScreen *s in [NSScreen screens]) {
        if (NSPointInRect(m, s.frame)) return s;
    }
    return [NSScreen mainScreen];
}

/// Frame of a panel `slot` places away from the corner (0: in the corner).
static NSRect panel_slot(NSPanel *p, NSUInteger slot) {
    NSScreen *screen = (g_panel_screen && [[NSScreen screens] containsObject:g_panel_screen]) ? g_panel_screen
                                                                                              : screen_under_mouse();
    NSRect vf = screen.visibleFrame;
    CGFloat w = p.frame.size.width, h = p.frame.size.height;
    BOOL left = g_panel_corner == 1 || g_panel_corner == 3, bottom = g_panel_corner == 2 || g_panel_corner == 3;
    CGFloat step = (h + kPanelGap) * slot;
    CGFloat x = left ? NSMinX(vf) + kPanelMargin : NSMaxX(vf) - w - kPanelMargin;
    CGFloat y = bottom ? NSMinY(vf) + kPanelMargin + step : NSMaxY(vf) - h - kPanelMargin - step;
    return NSMakeRect(x, y, w, h);
}

/// Put every panel in its place, the newest in the corner. `fresh` (just
/// made) and hidden panels go there directly; the others slide.
static void panel_layout(BuglePanelEntry *fresh) {
    NSUInteger n = g_panels.count;
    for (NSUInteger i = 0; i < n; i++) {
        BuglePanelEntry *e = g_panels[i];
        NSPanel *p = e.panel;
        NSRect f = panel_slot(p, n - 1 - i);
        if (NSEqualRects(p.frame, f)) continue;
        if (e == fresh || g_panels_hidden || !p.isVisible) {
            [p setFrame:f display:NO];
        } else {
            [NSAnimationContext runAnimationGroup:^(NSAnimationContext *c) {
              c.duration = 0.2;
              [p.animator setFrame:f display:YES];
            }];
        }
    }
}

static void panel_close(BuglePanelEntry *e, BOOL animated) {
    if (!e) return;
    [g_panels removeObject:e];
    panel_release_ctx(e);
    NSPanel *p = e.panel;
    e.panel = nil;
    e.thumb = nil;
    if (!animated || g_panels_hidden || !p.isVisible) {
        [p orderOut:nil];
    } else {
        p.ignoresMouseEvents = YES;
        if (!g_panel_fading) g_panel_fading = [NSMutableArray array];
        [g_panel_fading addObject:p];
        // The others slide into its slot while it fades: keep them above it.
        for (BuglePanelEntry *o in g_panels) [o.panel orderFrontRegardless];
        [NSAnimationContext
            runAnimationGroup:^(NSAnimationContext *ctx) {
              ctx.duration = 0.2;
              [p.animator setAlphaValue:0.0];
            }
            completionHandler:^{
              [p orderOut:nil];
              [g_panel_fading removeObject:p];
            }];
    }
    panel_layout(nil);
}

/// Order out the panels still fading, at once.
static void panel_clear_fading(void) {
    for (NSPanel *f in g_panel_fading) [f orderOut:nil];
    [g_panel_fading removeAllObjects];
}

/// Report a click on the panel holding `w`, then close it. The callback only
/// queues an event on the Rust side, so it never re-enters the panel. A
/// click on a panel that is already closing reaches nobody.
static void panel_choose(NSWindow *w, int32_t choice) {
    BuglePanelEntry *e = panel_by_window(w);
    if (!e) return;
    if (e.cb && e.ctx) e.cb(e.ctx, choice);
    panel_close(e, YES);
}

@interface BuglePanelWindow : NSPanel
@end
@implementation BuglePanelWindow
// Never take key focus: the app the user was in keeps its keyboard.
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
@end

@interface BuglePanelButton : NSButton
@property(nonatomic) int32_t choice;
@end
@implementation BuglePanelButton
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
@end

@interface BuglePanelThumb : NSImageView
@end
@implementation BuglePanelThumb
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (void)mouseDown:(NSEvent *)e { panel_choose(self.window, BUGLE_PANEL_THUMBNAIL); }
- (void)resetCursorRects { [self addCursorRect:self.bounds cursor:[NSCursor pointingHandCursor]]; }
@end

@interface BuglePanelTarget : NSObject
@end
@implementation BuglePanelTarget
- (void)clicked:(BuglePanelButton *)sender { panel_choose(sender.window, sender.choice); }
@end

static BuglePanelTarget *g_panel_target = nil;

static BuglePanelButton *make_button(NSString *title, NSString *symbol, int32_t choice) {
    BuglePanelButton *b = [[BuglePanelButton alloc] initWithFrame:NSZeroRect];
    b.title = title;
    b.bezelStyle = NSBezelStyleRounded;
    b.controlSize = NSControlSizeSmall;
    b.font = [NSFont systemFontOfSize:[NSFont systemFontSizeForControlSize:NSControlSizeSmall]];
    if (symbol) {
        NSImage *img = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:title];
        if (img) {
            b.image = img;
            b.imagePosition = NSImageOnly;
            b.toolTip = title;
        }
    }
    b.choice = choice;
    b.target = g_panel_target;
    b.action = @selector(clicked:);
    [b sizeToFit];
    return b;
}

/// Show panel `ident` (non-zero) in front of the stack, replacing a panel
/// with the same id. Returns 1, or 0 without taking `ctx` when the
/// arguments are unusable. Brings hidden panels back too.
int32_t bugle_panel_show(uint64_t ident, const uint8_t *png, size_t len, const char *title, const char *subtitle,
                         const char *const *labels, int32_t n_labels, int32_t corner, bugle_panel_callback_t cb, void *ctx,
                         bugle_panel_free_t free_ctx) {
    @autoreleasepool {
        if (ident == 0 || !cb) return 0;
        panel_close(panel_by_id(ident), NO);
        if (!g_panel_target) g_panel_target = [BuglePanelTarget new];
        if (!g_panels) g_panels = [NSMutableArray array];
        BuglePanelEntry *e = [BuglePanelEntry new];
        e.ident = ident;
        e.cb = cb;
        e.ctx = ctx;
        e.freeCtx = free_ctx;
        // The newest panel decides where the stack is.
        g_panel_corner = corner;
        g_panel_screen = screen_under_mouse();

        NSImage *img = (png && len) ? [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]] : nil;
        const CGFloat pad = 10, thumbW = 128, height = kPanelHeight, minWidth = 360, gap = 6;
        CGFloat tx = pad + thumbW + pad;

        // Buttons first: their natural widths decide the panel width.
        NSMutableArray<BuglePanelButton *> *buttons = [NSMutableArray array];
        CGFloat row = 0;
        for (int32_t i = 0; i < n_labels; i++) {
            NSString *label = labels[i] ? [NSString stringWithUTF8String:labels[i]] : @"";
            BuglePanelButton *b = make_button(label, nil, i);
            [buttons addObject:b];
            row += b.frame.size.width + (i ? gap : 0);
        }
        CGFloat width = MAX(minWidth, tx + row + pad);

        BuglePanelWindow *panel =
            [[BuglePanelWindow alloc] initWithContentRect:NSMakeRect(0, 0, width, height)
                                                styleMask:NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel
                                                  backing:NSBackingStoreBuffered
                                                    defer:NO];
        panel.releasedWhenClosed = NO;
        panel.level = NSStatusWindowLevel;
        panel.opaque = NO;
        panel.hasShadow = YES;
        panel.backgroundColor = [NSColor clearColor];
        panel.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces |
                                   NSWindowCollectionBehaviorFullScreenAuxiliary |
                                   NSWindowCollectionBehaviorIgnoresCycle;
        panel.hidesOnDeactivate = NO;
        panel.becomesKeyOnlyIfNeeded = YES;
        // Keep it out of screen captures where the window server honours this;
        // the app also hides the panels before every capture it starts.
        panel.sharingType = NSWindowSharingNone;
        panel.appearance = [NSAppearance appearanceNamed:NSAppearanceNameDarkAqua];

        NSVisualEffectView *root = [[NSVisualEffectView alloc] initWithFrame:NSMakeRect(0, 0, width, height)];
        root.material = NSVisualEffectMaterialHUDWindow;
        root.blendingMode = NSVisualEffectBlendingModeBehindWindow;
        root.state = NSVisualEffectStateActive;
        root.wantsLayer = YES;
        root.layer.cornerRadius = 12;
        root.layer.masksToBounds = YES;

        BuglePanelThumb *iv = [[BuglePanelThumb alloc] initWithFrame:NSMakeRect(pad, pad, thumbW, height - 2 * pad)];
        iv.image = img;
        iv.editable = NO;
        iv.imageScaling = NSImageScaleProportionallyDown;
        iv.wantsLayer = YES;
        iv.layer.cornerRadius = 6;
        iv.layer.masksToBounds = YES;
        iv.layer.backgroundColor = [[NSColor colorWithWhite:0 alpha:0.25] CGColor];
        iv.toolTip = @"Open";
        [root addSubview:iv];
        e.thumb = iv;

        BuglePanelButton *close = make_button(@"Close", @"xmark.circle.fill", BUGLE_PANEL_CLOSE);
        close.bordered = NO;
        close.contentTintColor = [NSColor colorWithWhite:1 alpha:0.6];
        close.frame = NSMakeRect(width - pad - 16, height - pad - 16, 16, 16);
        [root addSubview:close];

        CGFloat textW = width - tx - pad - 20;
        NSTextField *t = [NSTextField labelWithString:title ? [NSString stringWithUTF8String:title] : @""];
        t.font = [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold];
        t.textColor = [NSColor whiteColor];
        t.lineBreakMode = NSLineBreakByTruncatingTail;
        t.frame = NSMakeRect(tx, height - pad - 18, textW, 18);
        [root addSubview:t];
        NSTextField *s = [NSTextField labelWithString:subtitle ? [NSString stringWithUTF8String:subtitle] : @""];
        s.font = [NSFont systemFontOfSize:11];
        s.textColor = [NSColor colorWithWhite:1 alpha:0.75];
        s.lineBreakMode = NSLineBreakByTruncatingMiddle;
        s.frame = NSMakeRect(tx, height - pad - 36, textW + 20, 16);
        [root addSubview:s];

        CGFloat bx = tx;
        for (BuglePanelButton *b in buttons) {
            NSRect f = b.frame;
            f.origin = NSMakePoint(bx, pad);
            b.frame = f;
            [root addSubview:b];
            bx += f.size.width + gap;
        }

        panel.contentView = root;
        e.panel = panel;
        [g_panels addObject:e];
        // The new panel goes in front: older panels slide away from the
        // corner. While the stack is hidden (a recording or another capture
        // is running) it joins the stack off screen and comes back with
        // `bugle_panel_show_all`.
        panel_layout(e);
        if (g_panels_hidden) return 1;
        panel.alphaValue = 0.0;
        [panel orderFrontRegardless];
        [NSAnimationContext runAnimationGroup:^(NSAnimationContext *c) {
          c.duration = 0.15;
          [panel.animator setAlphaValue:1.0];
        }];
        return 1;
    }
}

/// Swap panel `ident`'s thumbnail (a sharper one arrives after it is up).
void bugle_panel_set_image(uint64_t ident, const uint8_t *png, size_t len) {
    @autoreleasepool {
        BuglePanelEntry *e = panel_by_id(ident);
        if (!e.thumb || !png || !len) return;
        NSImage *img = [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]];
        if (img) e.thumb.image = img;
    }
}

/// Close panel `ident` (fading out when `animated`) without reporting a
/// click; the others close up behind it.
void bugle_panel_dismiss(uint64_t ident, int32_t animated) {
    @autoreleasepool {
        panel_close(panel_by_id(ident), animated != 0);
    }
}

/// Close every panel; when not `animated`, also the ones still fading.
void bugle_panel_dismiss_all(int32_t animated) {
    @autoreleasepool {
        for (BuglePanelEntry *e in [g_panels copy]) panel_close(e, animated != 0);
        if (!animated) panel_clear_fading();
    }
}

/// Take every panel (and any still fading) off screen at once, keeping
/// them, so a capture cannot contain them. `bugle_panel_show_all` brings
/// them back; panels shown meanwhile stay off screen with the rest.
void bugle_panel_hide_all(void) {
    @autoreleasepool {
        g_panels_hidden = YES;
        for (BuglePanelEntry *e in g_panels) [e.panel orderOut:nil];
        panel_clear_fading();
    }
}

/// Bring back the panels hidden by `bugle_panel_hide_all`.
void bugle_panel_show_all(void) {
    @autoreleasepool {
        if (!g_panels_hidden) return;
        g_panels_hidden = NO;
        panel_layout(nil);
        for (BuglePanelEntry *e in g_panels) {
            e.panel.alphaValue = 1.0;
            [e.panel orderFrontRegardless];
        }
    }
}

/// How many panels are up (hidden ones included).
int32_t bugle_panel_count(void) { return (int32_t)g_panels.count; }

/// Whether the pointer is over any panel on screen.
int32_t bugle_panel_hovered(void) {
    if (g_panels_hidden) return 0;
    NSPoint m = [NSEvent mouseLocation];
    for (BuglePanelEntry *e in g_panels) {
        if (NSPointInRect(m, e.panel.frame)) return 1;
    }
    return 0;
}

void bugle_reveal_path(const char *path) {
    @autoreleasepool {
        NSURL *u = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        [[NSWorkspace sharedWorkspace] activateFileViewerSelectingURLs:@[ u ]];
    }
}

/// Open `path` with the application `bundle_id` (or the default application
/// when `bundle_id` is NULL/empty or not installed) and wait for the result.
/// Returns 1 on success, 2 when it fell back to the default application
/// because `bundle_id` is not installed, 0 on failure with a message in
/// `err` (NUL-terminated, truncated to `err_len`). Not on the main thread:
/// it blocks until the workspace reports back.
int32_t bugle_open_with(const char *path, const char *bundle_id, char *err, size_t err_len) {
    @autoreleasepool {
        NSWorkspace *ws = [NSWorkspace sharedWorkspace];
        NSURL *u = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
        NSString *bid = (bundle_id && *bundle_id) ? [NSString stringWithUTF8String:bundle_id] : nil;
        NSURL *app = bid ? [ws URLForApplicationWithBundleIdentifier:bid] : nil;
        int32_t fell_back = (bid && !app) ? 1 : 0;
        if (!app) app = [ws URLForApplicationToOpenURL:u];
        if (!app) {
            if (err && err_len) snprintf(err, err_len, "no application opens %s", path);
            return 0;
        }
        NSWorkspaceOpenConfiguration *cfg = [NSWorkspaceOpenConfiguration configuration];
        cfg.activates = YES;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block NSString *failure = nil;
        [ws openURLs:@[ u ]
            withApplicationAtURL:app
                   configuration:cfg
               completionHandler:^(NSRunningApplication *running, NSError *error) {
                 if (error) failure = error.localizedDescription ?: @"could not open the file";
                 dispatch_semaphore_signal(done);
               }];
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) != 0) {
            failure = @"timed out waiting for the application";
        }
        if (failure) {
            if (err && err_len) snprintf(err, err_len, "%s", failure.UTF8String);
            return 0;
        }
        return fell_back ? 2 : 1;
    }
}
