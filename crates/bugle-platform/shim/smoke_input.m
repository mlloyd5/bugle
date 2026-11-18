// Synthetic mouse and key input for `bugle-app --smoke-test`. Bindings live
// in src/smoke_input.rs.
//
// The events are built in this process with +[NSEvent mouseEventWithType:...]
// and +[NSEvent keyEventWithType:...] and handed to one of Bugle's own
// windows, so they travel AppKit's real path from there: the window's
// hit-testing, the view that took the mouse-down getting the drags and the
// mouse-up, key equivalents before the first responder, the responder chain,
// the field editor of a text field, a button's highlight and action. Nothing is
// posted to the window server: the real pointer does not move, no
// Accessibility permission is involved, and other apps see none of these
// events (the opt-in paste step, bugle_smoke_paste, does write the general
// pasteboard, which other apps can read). It is still not a person's input:
// the window server's own steps (which window is under the pointer,
// activation by a click, the input method's keyboard layout) are left out.
//
// Every function may be called from any thread except the main one (it runs
// its work there with dispatch_sync, so the main thread must be running its
// loop); from the main thread it runs at once.

#import <AppKit/AppKit.h>
#include <stdint.h>
#include "shim.h"

// Targets. Mirrored in src/smoke_input.rs.
enum { TARGET_VIDEO_EDITOR = 0, TARGET_MARKUP_EDITOR = 1, TARGET_PROMPT = 2 };
// Mouse phases. Mirrored in src/smoke_input.rs.
enum { PHASE_DOWN = 0, PHASE_DRAG = 1, PHASE_UP = 2 };

static NSInteger g_smoke_event_number = 0;

static void on_main(dispatch_block_t block) {
    if ([NSThread isMainThread]) block();
    else dispatch_sync(dispatch_get_main_queue(), block);
}

static NSWindow *target_window(int32_t target) {
    NSWindow *w = nil;
    switch (target) {
        case TARGET_VIDEO_EDITOR: w = bugle_video_editor_window(); break;
        case TARGET_MARKUP_EDITOR: w = bugle_markup_editor_front_window(); break;
        case TARGET_PROMPT: w = bugle_prompt_window(); break;
        default: break;
    }
    return w.isVisible ? w : nil;
}

static NSEvent *mouse_event(NSEventType type, NSWindow *w, NSPoint p, NSInteger clicks, NSEventModifierFlags mods) {
    return [NSEvent mouseEventWithType:type
                              location:p
                         modifierFlags:mods
                             timestamp:NSProcessInfo.processInfo.systemUptime
                          windowNumber:w.windowNumber
                               context:nil
                           eventNumber:++g_smoke_event_number
                            clickCount:clicks
                              pressure:type == NSEventTypeLeftMouseUp ? 0.0 : 1.0];
}

/// Run the C function `f(ctx)` on the main thread and wait for it.
void bugle_smoke_on_main(void (*f)(void *), void *ctx) {
    if (!f) return;
    on_main(^{
        @autoreleasepool {
            f(ctx);
        }
    });
}

/// One left-button event at window point (x, y) (points, origin at the
/// bottom left of the window), through -[NSWindow sendEvent:]: the window
/// hit-tests a mouse-down and sends drags and the mouse-up to the view that
/// took it. Returns 1, or 0 when the target window is not on screen.
int32_t bugle_smoke_mouse(int32_t target, int32_t phase, double x, double y, int32_t clicks, uint64_t mods) {
    __block int32_t rc = 0;
    on_main(^{
        @autoreleasepool {
            NSWindow *w = target_window(target);
            if (!w) return;
            NSEventType type = phase == PHASE_DOWN ? NSEventTypeLeftMouseDown
                             : phase == PHASE_DRAG ? NSEventTypeLeftMouseDragged
                                                   : NSEventTypeLeftMouseUp;
            [w sendEvent:mouse_event(type, w, NSMakePoint(x, y), MAX(1, clicks), (NSEventModifierFlags)mods)];
            rc = 1;
        }
    });
    return rc;
}

/// A click at window point (x, y): the mouse-up is queued at the head of
/// the app's event queue, then the mouse-down is sent to the window. A
/// control that tracks the mouse in a loop of its own would pull the queued
/// mouse-up from the queue, as it would a real one, instead of blocking the
/// main thread; otherwise the mouse-up, still at the head of the queue, is
/// taken off it and sent after the mouse-down. (On macOS 26 NSButton runs no
/// such loop: it highlights on the mouse-down and acts on the mouse-up the
/// window sends it, so the tracking-loop path has not been run on a macOS
/// where a button tracks.) Returns 1 when something took the mouse-up off the
/// queue during the mouse-down, 2 when it was sent afterwards, 0 when the
/// target window is not on screen.
int32_t bugle_smoke_click(int32_t target, double x, double y) {
    __block int32_t rc = 0;
    on_main(^{
        @autoreleasepool {
            NSWindow *w = target_window(target);
            if (!w) return;
            NSPoint p = NSMakePoint(x, y);
            NSEvent *down = mouse_event(NSEventTypeLeftMouseDown, w, p, 1, 0);
            NSEvent *up = mouse_event(NSEventTypeLeftMouseUp, w, p, 1, 0);
            [NSApp postEvent:up atStart:YES];
            [w sendEvent:down];
            // Ours is at the head unless a tracking loop took it: only then is
            // anything dequeued, so no other event changes place.
            NSEvent *head = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:nil inMode:NSDefaultRunLoopMode dequeue:NO];
            if (head.type == NSEventTypeLeftMouseUp && head.eventNumber == up.eventNumber && head.windowNumber == up.windowNumber) {
                [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:nil inMode:NSDefaultRunLoopMode dequeue:YES];
                [w sendEvent:up];
                rc = 2;
            } else {
                rc = 1;
            }
        }
    });
    return rc;
}

/// Deliver key event `ev` to `w` the way a real one is delivered: posted at
/// the head of the queue and taken off it again, so it becomes
/// NSApp.currentEvent (code that looks at the current event's modifiers sees
/// this one, not the last real event), then sent through NSApplication when
/// `w` is Bugle's key window and Bugle is active (returns 1), else through
/// -performKeyEquivalent: and -sendEvent: on the window (returns 2).
static int32_t deliver_key(NSWindow *w, NSEvent *ev) {
    NSEventMask mask = NSEventMaskFromType(ev.type);
    [NSApp postEvent:ev atStart:YES];
    NSEvent *got = [NSApp nextEventMatchingMask:mask untilDate:nil inMode:NSDefaultRunLoopMode dequeue:YES];
    // The queue hands back a copy (its timestamp rounded to the nanosecond).
    BOOL ours = got && fabs(got.timestamp - ev.timestamp) < 1e-6 && got.keyCode == ev.keyCode && got.windowNumber == ev.windowNumber &&
                got.type == ev.type && [got.characters isEqualToString:ev.characters];
    if (got && !ours) {
        // Not ours (should not happen: ours was at the head); put it back.
        [NSApp postEvent:got atStart:YES];
        got = nil;
    }
    NSEvent *send = got ?: ev;
    if (NSApp.isActive && NSApp.keyWindow == w) {
        [NSApp sendEvent:send];
        return 1;
    }
    if (send.type != NSEventTypeKeyDown || ![w performKeyEquivalent:send]) [w sendEvent:send];
    return 2;
}

static NSEvent *key_event(NSWindow *w, NSEventType type, uint16_t key_code, NSString *c, NSString *ci, uint64_t mods) {
    return [NSEvent keyEventWithType:type
                            location:NSZeroPoint
                       modifierFlags:(NSEventModifierFlags)mods
                           timestamp:NSProcessInfo.processInfo.systemUptime
                        windowNumber:w.windowNumber
                             context:nil
                          characters:c
         charactersIgnoringModifiers:ci
                           isARepeat:NO
                             keyCode:key_code];
}

/// A key press (key-down, then key-up) with `key_code` (a virtual key code
/// of the ANSI layout), `chars` and `chars_ignoring` (UTF-8, the event's
/// `characters` and `charactersIgnoringModifiers`) and `mods`
/// (NSEventModifierFlags), delivered as deliver_key describes: through
/// -[NSApplication sendEvent:] when the target is Bugle's key window and
/// Bugle is active (returns 1), else by hand on the target window
/// (-performKeyEquivalent:, then -sendEvent:; returns 2). Either way the
/// event is NSApp.currentEvent while it is handled. 0 when the target window
/// is not on screen.
int32_t bugle_smoke_key(int32_t target, uint16_t key_code, const char *chars, const char *chars_ignoring, uint64_t mods) {
    NSString *c = [NSString stringWithUTF8String:chars ? chars : ""] ?: @"";
    NSString *ci = [NSString stringWithUTF8String:chars_ignoring ? chars_ignoring : ""] ?: @"";
    __block int32_t rc = 0;
    on_main(^{
        @autoreleasepool {
            NSWindow *w = target_window(target);
            if (!w) return;
            rc = deliver_key(w, key_event(w, NSEventTypeKeyDown, key_code, c, ci, mods));
            // The key-up goes the same way to the target window, if it is
            // still on screen (⌘W closes it).
            NSWindow *still = target_window(target);
            if (still) deliver_key(still, key_event(still, NSEventTypeKeyUp, key_code, c, ci, mods));
        }
    });
    return rc;
}

/// Pick item `index` of the pop-up button under window point (x, y). The
/// button is found by the window's own hit-testing and clicked with a
/// mouse-down, which opens its menu. An open menu runs a modal tracking
/// session of its own that blocks the main thread; steering it with more
/// synthetic events is not attempted (a miss would leave a menu up on the
/// user's screen), so once it has opened it is closed again unchosen, and
/// the item is chosen through the menu (-[NSMenu
/// performActionForItemAtIndex:], what choosing it does), which selects it
/// and sends the button's action. Returns 1 when the menu
/// opened and the item was chosen, 2 when the item was chosen but the menu
/// was not seen opening, 0 when there is no window, -1 when no enabled
/// pop-up button is at (x, y), -2 when `index` is out of range.
int32_t bugle_smoke_pick_popup(int32_t target, double x, double y, int32_t index) {
    __block int32_t rc = 0;
    on_main(^{
        @autoreleasepool {
            NSWindow *w = target_window(target);
            if (!w) return;
            NSPoint p = NSMakePoint(x, y);
            NSView *hit = [w.contentView.superview hitTest:p];
            while (hit && ![hit isKindOfClass:[NSPopUpButton class]]) hit = hit.superview;
            NSPopUpButton *popup = (NSPopUpButton *)hit;
            if (!popup || !popup.isEnabled) {
                rc = -1;
                return;
            }
            if (index < 0 || index >= popup.numberOfItems) {
                rc = -2;
                return;
            }
            NSMenu *menu = popup.menu;
            __block BOOL opened = NO;
            // The menu being tracked (the button's, or one it shows for it).
            __block NSMenu *tracked = nil;
            id observer = [NSNotificationCenter.defaultCenter
                addObserverForName:NSMenuDidBeginTrackingNotification
                            object:nil
                             queue:nil
                        usingBlock:^(NSNotification *note) {
                            opened = YES;
                            NSMenu *m = note.object;
                            tracked = m;
                            // Close it from inside its tracking loop.
                            CFRunLoopPerformBlock(CFRunLoopGetMain(), kCFRunLoopCommonModes, ^{
                                [m cancelTrackingWithoutAnimation];
                            });
                            CFRunLoopWakeUp(CFRunLoopGetMain());
                        }];
            // Never leave a menu open on the user's screen.
            NSTimer *guard = [NSTimer timerWithTimeInterval:2.0
                                                    repeats:NO
                                                      block:^(NSTimer *t) {
                                                          [(tracked ?: menu) cancelTrackingWithoutAnimation];
                                                      }];
            [NSRunLoop.mainRunLoop addTimer:guard forMode:NSRunLoopCommonModes];
            [w sendEvent:mouse_event(NSEventTypeLeftMouseDown, w, p, 1, 0)];
            [guard invalidate];
            [NSNotificationCenter.defaultCenter removeObserver:observer];
            [menu performActionForItemAtIndex:index];
            rc = opened ? 1 : 2;
        }
    });
    return rc;
}

/// Plain-text types whose data the pasteboard can hand over at once. AppKit
/// has no public way to tell data an app has only promised from data it has
/// written, so anything else on the pasteboard makes the paste step stand
/// down rather than force it to be provided (or lose it).
static BOOL plain_type(NSPasteboardType t) {
    static NSSet<NSString *> *plain;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        plain = [NSSet setWithArray:@[ NSPasteboardTypeString, @"public.utf16-plain-text", @"public.utf16-external-plain-text",
                                       @"NSStringPboardType", NSPasteboardTypeRTF, NSPasteboardTypeHTML ]];
    });
    return [plain containsObject:t];
}

/// Press ⌘V in the target window with `text` on the general pasteboard, and
/// put the previous contents back, all in one main-thread turn. Only when
/// the pasteboard is empty or holds nothing but plain text (copied first):
/// otherwise nothing is touched and -1 is returned. The previous contents go
/// back only if nothing else wrote the pasteboard in between (its change
/// count is what this wrote); else they are left and 2 is returned. 1:
/// pasted and put back; 0: the target window is not on screen. The change
/// count goes up twice either way.
int32_t bugle_smoke_paste(int32_t target, const char *text) {
    NSString *t = [NSString stringWithUTF8String:text ? text : ""] ?: @"";
    __block int32_t rc = 0;
    on_main(^{
        @autoreleasepool {
            NSWindow *w = target_window(target);
            if (!w) return;
            NSPasteboard *pb = NSPasteboard.generalPasteboard;
            NSMutableArray<NSPasteboardItem *> *saved = [NSMutableArray array];
            for (NSPasteboardItem *item in pb.pasteboardItems) {
                for (NSPasteboardType type in item.types) {
                    if (!plain_type(type)) {
                        rc = -1;
                        return;
                    }
                }
            }
            for (NSPasteboardItem *item in pb.pasteboardItems) {
                NSPasteboardItem *copy = [[NSPasteboardItem alloc] init];
                for (NSPasteboardType type in item.types) {
                    NSData *d = [item dataForType:type];
                    if (d) [copy setData:d forType:type];
                }
                [saved addObject:copy];
            }
            [pb clearContents];
            [pb setString:t forType:NSPasteboardTypeString];
            NSInteger written = pb.changeCount;
            deliver_key(w, key_event(w, NSEventTypeKeyDown, 9, @"v", @"v", NSEventModifierFlagCommand));
            NSWindow *still = target_window(target);
            if (still) deliver_key(still, key_event(still, NSEventTypeKeyUp, 9, @"v", @"v", NSEventModifierFlagCommand));
            if (pb.changeCount != written) {
                rc = 2;
                return;
            }
            [pb clearContents];
            if (saved.count) [pb writeObjects:saved];
            rc = 1;
        }
    });
    return rc;
}
