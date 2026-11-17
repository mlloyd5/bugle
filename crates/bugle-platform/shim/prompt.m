// A small modal text prompt (an NSAlert with a text field), for values the
// menu-bar app asks for that a menu of presets cannot hold, such as a custom
// audio level. Bindings live in src/prompt.rs.

#import <AppKit/AppKit.h>
#include <string.h>

/// Runs a modal alert titled `title` with `message` below it and a text
/// field holding `initial`, on the main thread. Returns 1 with the typed text
/// in *out (malloc'd, free with bugle_free) for OK, 0 for Cancel, and -1 off
/// the main thread.
int bugle_prompt_text(const char *title, const char *message, const char *initial, char **out) {
    if (out) *out = NULL;
    if (![NSThread isMainThread]) return -1;
    @autoreleasepool {
        NSAlert *alert = [[NSAlert alloc] init];
        alert.messageText = [NSString stringWithUTF8String:title ? title : ""] ?: @"";
        alert.informativeText = [NSString stringWithUTF8String:message ? message : ""] ?: @"";
        [alert addButtonWithTitle:@"OK"];
        [alert addButtonWithTitle:@"Cancel"];
        NSTextField *field = [[NSTextField alloc] initWithFrame:NSMakeRect(0, 0, 200, 24)];
        field.stringValue = [NSString stringWithUTF8String:initial ? initial : ""] ?: @"";
        alert.accessoryView = field;
        // A menu-bar app is not frontmost; without this the alert opens
        // behind the active app's windows.
        [NSApp activateIgnoringOtherApps:YES];
        [alert layout];
        alert.window.initialFirstResponder = field;
        NSModalResponse r = [alert runModal];
        if (r != NSAlertFirstButtonReturn) return 0;
        const char *u = field.stringValue.UTF8String;
        if (out) *out = strdup(u ? u : "");
        return 1;
    }
}
