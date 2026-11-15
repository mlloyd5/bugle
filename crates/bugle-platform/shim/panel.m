// Bugle quick-access panel.
//
// A non-activating floating panel shown by the menu-bar app after a capture:
// the thumbnail, two lines of text, a row of buttons and a close button. The
// panel owns no policy: every click is reported to Rust through a callback and
// the panel closes. Which buttons exist, what they do and when the panel times
// out are decided by `bugle-app` (`quick_access.rs`).
//
// Main thread only; needs a running AppKit loop. One panel at a time: showing a
// new one replaces the old one.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdint.h>

typedef void (*bugle_panel_callback_t)(void *ctx, int32_t choice);
typedef void (*bugle_panel_free_t)(void *ctx);

// Choice codes passed to the callback besides button indices (>= 0).
enum { BUGLE_PANEL_CLOSE = -1, BUGLE_PANEL_THUMBNAIL = -2 };

static NSPanel *g_panel = nil;
static NSImageView *g_panel_thumb = nil;
static bugle_panel_callback_t g_panel_cb = NULL;
static bugle_panel_free_t g_panel_free = NULL;
static void *g_panel_ctx = NULL;
// Panels still fading out: off limits for clicks, and ordered out at once
// when the app needs the screen clear (before a capture).
static NSMutableArray<NSPanel *> *g_panel_fading = nil;

/// Release the Rust context exactly once.
static void panel_release_ctx(void) {
    void *ctx = g_panel_ctx;
    bugle_panel_free_t f = g_panel_free;
    g_panel_ctx = NULL;
    g_panel_free = NULL;
    g_panel_cb = NULL;
    if (ctx && f) f(ctx);
}

static void panel_close(BOOL animated) {
    NSPanel *p = g_panel;
    g_panel = nil;
    g_panel_thumb = nil;
    panel_release_ctx();
    if (!animated) {
        for (NSPanel *f in g_panel_fading) [f orderOut:nil];
        [g_panel_fading removeAllObjects];
        [p orderOut:nil];
        return;
    }
    if (!p) return;
    p.ignoresMouseEvents = YES;
    if (!g_panel_fading) g_panel_fading = [NSMutableArray array];
    [g_panel_fading addObject:p];
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

/// Report a click, then close. The callback only queues an event on the Rust
/// side, so it never re-enters the panel.
static void panel_choose(int32_t choice) {
    if (g_panel_cb && g_panel_ctx) g_panel_cb(g_panel_ctx, choice);
    panel_close(YES);
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
- (void)mouseDown:(NSEvent *)e {
    // A click on a panel that is already closing must not reach the next one.
    if (self.window == g_panel) panel_choose(BUGLE_PANEL_THUMBNAIL);
}
- (void)resetCursorRects { [self addCursorRect:self.bounds cursor:[NSCursor pointingHandCursor]]; }
@end

@interface BuglePanelTarget : NSObject
@end
@implementation BuglePanelTarget
- (void)clicked:(BuglePanelButton *)sender {
    if (sender.window == g_panel) panel_choose(sender.choice);
}
@end

static BuglePanelTarget *g_panel_target = nil;

static NSScreen *screen_under_mouse(void) {
    NSPoint m = [NSEvent mouseLocation];
    for (NSScreen *s in [NSScreen screens]) {
        if (NSPointInRect(m, s.frame)) return s;
    }
    return [NSScreen mainScreen];
}

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

int32_t bugle_panel_show(const uint8_t *png, size_t len, const char *title, const char *subtitle,
                         const char *const *labels, int32_t n_labels, int32_t corner, bugle_panel_callback_t cb,
                         void *ctx, bugle_panel_free_t free_ctx) {
    @autoreleasepool {
        if (g_panel) panel_close(NO);
        if (!g_panel_target) g_panel_target = [BuglePanelTarget new];
        g_panel_cb = cb;
        g_panel_ctx = ctx;
        g_panel_free = free_ctx;

        NSImage *img = (png && len) ? [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]] : nil;
        const CGFloat pad = 10, thumbW = 128, height = 104, minWidth = 360, gap = 6;
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

        NSScreen *screen = screen_under_mouse();
        NSRect vf = screen.visibleFrame;
        BOOL left = corner == 1 || corner == 3, bottom = corner == 2 || corner == 3;
        CGFloat x = left ? NSMinX(vf) + 16 : NSMaxX(vf) - width - 16;
        CGFloat y = bottom ? NSMinY(vf) + 16 : NSMaxY(vf) - height - 16;

        BuglePanelWindow *panel =
            [[BuglePanelWindow alloc] initWithContentRect:NSMakeRect(x, y, width, height)
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
        // the app also closes the panel before every capture it starts.
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
        g_panel_thumb = iv;

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
        panel.alphaValue = 0.0;
        [panel orderFrontRegardless];
        [NSAnimationContext runAnimationGroup:^(NSAnimationContext *c) {
          c.duration = 0.15;
          [panel.animator setAlphaValue:1.0];
        }];
        g_panel = panel;
        return 1;
    }
}

/// Swap the thumbnail (a sharper one arrives after the panel is up).
void bugle_panel_set_image(const uint8_t *png, size_t len) {
    @autoreleasepool {
        if (!g_panel_thumb || !png || !len) return;
        NSImage *img = [[NSImage alloc] initWithData:[NSData dataWithBytes:png length:len]];
        if (img) g_panel_thumb.image = img;
    }
}

void bugle_panel_dismiss(int32_t animated) {
    @autoreleasepool {
        panel_close(animated != 0);
    }
}

int32_t bugle_panel_active(void) { return g_panel != nil ? 1 : 0; }

int32_t bugle_panel_hovered(void) {
    if (!g_panel) return 0;
    return NSPointInRect([NSEvent mouseLocation], g_panel.frame) ? 1 : 0;
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
