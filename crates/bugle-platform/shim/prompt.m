// A small text prompt (a panel with a title, a message, a text field and
// OK/Cancel), for values the menu-bar app asks for that a menu of presets
// cannot hold, such as a custom audio level. Bindings live in src/prompt.rs.
//
// The panel is not modal: opening it returns at once and the answer comes
// later through the callback, so the app's event loop (hotkeys, the menu,
// worker results) keeps running while it is open. One prompt at a time.

#import <AppKit/AppKit.h>
#include <stdint.h>
#include "shim.h"

typedef void (*bugle_prompt_callback_t)(void *ctx, int32_t ok, const char *text);
typedef void (*bugle_prompt_free_t)(void *ctx);

@class BuglePrompt;

// The app has no main menu (no Edit menu), so the usual editing keys are
// mapped here, as the editors do.
@interface BuglePromptPanel : NSPanel
@end

@implementation BuglePromptPanel
- (BOOL)canBecomeKeyWindow { return YES; }
- (BOOL)performKeyEquivalent:(NSEvent *)event {
    NSEventModifierFlags mods = event.modifierFlags & NSEventModifierFlagDeviceIndependentFlagsMask;
    if (!(mods & NSEventModifierFlagCommand) || (mods & (NSEventModifierFlagControl | NSEventModifierFlagOption))) {
        return [super performKeyEquivalent:event];
    }
    BOOL shift = (mods & NSEventModifierFlagShift) != 0;
    NSString *key = event.charactersIgnoringModifiers.lowercaseString;
    SEL action = NULL;
    if ([key isEqualToString:@"c"]) action = @selector(copy:);
    else if ([key isEqualToString:@"v"]) action = @selector(paste:);
    else if ([key isEqualToString:@"x"]) action = @selector(cut:);
    else if ([key isEqualToString:@"a"]) action = @selector(selectAll:);
    else if ([key isEqualToString:@"z"]) action = shift ? NSSelectorFromString(@"redo:") : NSSelectorFromString(@"undo:");
    else if ([key isEqualToString:@"w"]) {
        [self performClose:nil];
        return YES;
    }
    if (action && [NSApp sendAction:action to:nil from:self]) return YES;
    return [super performKeyEquivalent:event];
}
@end

@interface BuglePrompt : NSObject <NSWindowDelegate, NSTextFieldDelegate>
@property(nonatomic, strong) BuglePromptPanel *panel;
@property(nonatomic, strong) NSTextField *field;
@property(nonatomic) bugle_prompt_callback_t cb;
@property(nonatomic) void *ctx;
@property(nonatomic) bugle_prompt_free_t freeCtx;
- (void)finish:(BOOL)ok;
@end

static BuglePrompt *g_prompt;

static NSTextField *label(NSString *text, NSFont *font) {
    NSTextField *l = [NSTextField wrappingLabelWithString:text];
    l.font = font;
    l.selectable = NO;
    l.preferredMaxLayoutWidth = 300;
    l.translatesAutoresizingMaskIntoConstraints = NO;
    return l;
}

@implementation BuglePrompt

- (void)ok:(id)sender { [self finish:YES]; }
- (void)cancel:(id)sender { [self finish:NO]; }

// Return (or Enter) in the field answers OK: a hardening, not a confirmed
// bug fix. With the default button alone, a synthetic Return key-down sent
// to the panel while the field is editing (through -[NSApp sendEvent:],
// -postEvent:, or posted, dequeued and sent) never pressed OK: the field
// editor took it and only ended editing. The same happens on a bare
// NSPanel with an NSTextField and a default button, so it is AppKit's
// behaviour for such events; whether a hardware Return behaves the same was
// not checked. Taking insertNewline: here, as the markup editor's label
// field does, makes Return answer OK either way.
- (BOOL)control:(NSControl *)control textView:(NSTextView *)textView doCommandBySelector:(SEL)sel {
    if (sel == @selector(insertNewline:)) {
        [self finish:YES];
        return YES;
    }
    return NO;
}

// The title-bar close button.
- (BOOL)windowShouldClose:(NSWindow *)sender {
    [self finish:NO];
    return NO;
}

/// Report the answer once, take the panel down and hand the focus on.
- (void)finish:(BOOL)ok {
    bugle_prompt_callback_t cb = self.cb;
    void *ctx = self.ctx;
    bugle_prompt_free_t f = self.freeCtx;
    if (!cb && !f) return;
    self.cb = NULL;
    self.ctx = NULL;
    self.freeCtx = NULL;
    NSString *text = self.field.stringValue ?: @"";
    if (cb) cb(ctx, ok ? 1 : 0, ok ? (text.UTF8String ?: "") : NULL);
    self.panel.delegate = nil;
    [self.panel orderOut:nil];
    BuglePrompt *keep = self;
    if (g_prompt == self) g_prompt = nil;
    // Focus goes to an open editor, else back to the app the user came from.
    bugle_editor_window_closed();
    dispatch_async(dispatch_get_main_queue(), ^{
        if (ctx && f) f(ctx);
        (void)keep;
    });
}

- (void)buildWithTitle:(NSString *)title message:(NSString *)message initial:(NSString *)initial {
    BuglePromptPanel *panel = [[BuglePromptPanel alloc] initWithContentRect:NSMakeRect(0, 0, 340, 150)
                                                                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
                                                                    backing:NSBackingStoreBuffered
                                                                      defer:NO];
    panel.title = title;
    panel.releasedWhenClosed = NO;
    // A panel hides when its app deactivates by default; this one must stay
    // up until answered (clicking Custom… again brings it to the front).
    panel.hidesOnDeactivate = NO;
    panel.delegate = self;

    NSTextField *heading = label(title, [NSFont boldSystemFontOfSize:NSFont.systemFontSize]);
    NSTextField *body = label(message, [NSFont systemFontOfSize:NSFont.smallSystemFontSize]);
    NSTextField *field = [NSTextField textFieldWithString:initial];
    field.translatesAutoresizingMaskIntoConstraints = NO;
    field.delegate = self;
    // Return presses OK (see doCommandBySelector above; OK is also the
    // default button), Esc presses Cancel.
    NSButton *ok = [NSButton buttonWithTitle:@"OK" target:self action:@selector(ok:)];
    ok.keyEquivalent = @"\r";
    NSButton *cancel = [NSButton buttonWithTitle:@"Cancel" target:self action:@selector(cancel:)];
    cancel.keyEquivalent = @"\033";
    ok.translatesAutoresizingMaskIntoConstraints = NO;
    cancel.translatesAutoresizingMaskIntoConstraints = NO;

    NSView *v = panel.contentView;
    for (NSView *s in @[ heading, body, field, ok, cancel ]) [v addSubview:s];
    [NSLayoutConstraint activateConstraints:@[
        [heading.topAnchor constraintEqualToAnchor:v.topAnchor constant:16],
        [heading.leadingAnchor constraintEqualToAnchor:v.leadingAnchor constant:20],
        [heading.trailingAnchor constraintEqualToAnchor:v.trailingAnchor constant:-20],
        [body.topAnchor constraintEqualToAnchor:heading.bottomAnchor constant:6],
        [body.leadingAnchor constraintEqualToAnchor:heading.leadingAnchor],
        [body.trailingAnchor constraintEqualToAnchor:heading.trailingAnchor],
        [field.topAnchor constraintEqualToAnchor:body.bottomAnchor constant:12],
        [field.leadingAnchor constraintEqualToAnchor:heading.leadingAnchor],
        [field.trailingAnchor constraintEqualToAnchor:heading.trailingAnchor],
        [ok.topAnchor constraintEqualToAnchor:field.bottomAnchor constant:16],
        [ok.trailingAnchor constraintEqualToAnchor:heading.trailingAnchor],
        [ok.bottomAnchor constraintEqualToAnchor:v.bottomAnchor constant:-16],
        [ok.widthAnchor constraintGreaterThanOrEqualToConstant:76],
        [cancel.centerYAnchor constraintEqualToAnchor:ok.centerYAnchor],
        [cancel.trailingAnchor constraintEqualToAnchor:ok.leadingAnchor constant:-10],
        [cancel.widthAnchor constraintGreaterThanOrEqualToConstant:76],
    ]];
    panel.initialFirstResponder = field;
    self.panel = panel;
    self.field = field;
}

- (void)show {
    // A menu-bar app is not frontmost; without this the panel opens behind
    // the active app's windows and gets no keys.
    [NSApp activateIgnoringOtherApps:YES];
    [self.panel makeKeyAndOrderFront:nil];
    [self.panel makeFirstResponder:self.field];
    // Select the current value so typing replaces it.
    [self.field selectText:nil];
}

@end

/// Open a prompt titled `title` with `message` below it and a text field
/// holding `initial`, and return at once. `cb` runs once on the main thread
/// when it is answered: ok = 1 with the typed text, or ok = 0 with NULL for
/// Cancel, Esc or the close button. `free_ctx` runs after that, on a later
/// main-loop turn. Returns 1 when opened (the shim owns `ctx`), 0 when a
/// prompt is already open (it is brought to the front; `ctx` is not taken),
/// and -1 off the main thread.
int32_t bugle_prompt_open(const char *title, const char *message, const char *initial, bugle_prompt_callback_t cb, void *ctx,
                          bugle_prompt_free_t free_ctx) {
    if (![NSThread isMainThread]) return -1;
    @autoreleasepool {
        if (g_prompt) {
            [g_prompt show];
            return 0;
        }
        BuglePrompt *p = [[BuglePrompt alloc] init];
        p.cb = cb;
        p.ctx = ctx;
        p.freeCtx = free_ctx;
        NSString *(^s)(const char *) = ^NSString *(const char *c) { return [NSString stringWithUTF8String:c ? c : ""] ?: @""; };
        [p buildWithTitle:s(title) message:s(message) initial:s(initial)];
        [p.panel center];
        g_prompt = p;
        [p show];
        return 1;
    }
}

/// Whether a prompt is open.
int32_t bugle_prompt_active(void) { return g_prompt ? 1 : 0; }

/// Answer the open prompt as if OK (with `text`, or the field's text when
/// NULL) or Cancel were pressed. Any thread; for smoke tests, which cannot
/// click.
void bugle_prompt_answer(int32_t ok, const char *text) {
    NSString *t = text ? [NSString stringWithUTF8String:text] : nil;
    dispatch_async(dispatch_get_main_queue(), ^{
        BuglePrompt *p = g_prompt;
        if (!p) return;
        if (t) p.field.stringValue = t;
        [p finish:ok != 0];
    });
}

/// The open prompt's panel, or nil. Main thread only.
NSWindow *bugle_prompt_window(void) { return g_prompt.panel; }

/// Smoke test: the text in the open prompt's field, as UTF-8 truncated to
/// `cap - 1` bytes and NUL-terminated (while it is being edited, the field
/// editor's text). Returns 1, or 0 with no prompt open. Main thread only.
int32_t bugle_prompt_smoke_text(char *buf, size_t cap) {
    @autoreleasepool {
        BuglePrompt *p = g_prompt;
        if (!p || !buf || cap == 0) return 0;
        NSText *editor = p.field.currentEditor;
        NSString *s = (editor ? editor.string : p.field.stringValue) ?: @"";
        strlcpy(buf, s.UTF8String ?: "", cap);
        return 1;
    }
}
