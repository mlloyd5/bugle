// Status-menu probe for `bugle-app --smoke-test`: opens the status item's
// menu the way a click on it does and, while the menu tracks, samples the
// status item title and the menu's item titles on a timer, then closes the
// menu. It shows whether what the app sets while the menu is open reaches
// the open menu, without real mouse input. Bindings live in src/status_menu.rs.
//
// The menu is opened from a main-queue block, not from inside the caller's
// event-loop callback: a click reaches the status item from NSApplication's
// own event dispatch, outside the event loop's handler, and the probe opens
// the menu from the same kind of place.

#import <AppKit/AppKit.h>
#include <stdint.h>
#include "shim.h"

typedef void (*bugle_menu_probe_callback_t)(void *ctx, const char *json);

static BOOL g_probe_running = NO;

// Report (JSON, to `cb` on the main thread once the menu has closed):
// {"opened": bool, "open_seconds": s, "samples": [{"t": s, "mode": run-loop mode,
//  "title": status item title, "items": [menu item titles], "width": menu width in points}]}.
// Returns 0 without calling `cb` when a probe is running, an argument is
// missing, or not on the main thread. Main thread only.
int32_t bugle_status_menu_probe(void *status_item, void *menu, double seconds, double interval, bugle_menu_probe_callback_t cb,
                                void *ctx) {
    if (![NSThread isMainThread] || !status_item || !menu || !cb || g_probe_running) return 0;
    NSStatusItem *item = (__bridge NSStatusItem *)status_item;
    NSMenu *m = (__bridge NSMenu *)menu;
    g_probe_running = YES;
    dispatch_async(dispatch_get_main_queue(), ^{
        NSMutableArray *samples = [NSMutableArray array];
        __block BOOL opened = NO;
        __block CFAbsoluteTime openedAt = 0;
        id obs = [[NSNotificationCenter defaultCenter] addObserverForName:NSMenuDidBeginTrackingNotification
                                                                   object:m
                                                                    queue:nil
                                                               usingBlock:^(NSNotification *n) {
                                                                   opened = YES;
                                                                   openedAt = CFAbsoluteTimeGetCurrent();
                                                               }];
        CFAbsoluteTime t0 = CFAbsoluteTimeGetCurrent();
        NSTimer *sampler = [NSTimer timerWithTimeInterval:interval repeats:YES block:^(NSTimer *t) {
            NSMutableArray *titles = [NSMutableArray array];
            for (NSMenuItem *mi in m.itemArray) [titles addObject:mi.title ?: @""];
            [samples addObject:@{
                @"t" : @(CFAbsoluteTimeGetCurrent() - t0),
                @"mode" : [NSRunLoop currentRunLoop].currentMode ?: @"",
                @"title" : item.button.title ?: @"",
                @"items" : titles,
                @"width" : @(m.size.width),
            }];
        }];
        NSTimer *closer = [NSTimer timerWithTimeInterval:seconds repeats:NO block:^(NSTimer *t) { [m cancelTracking]; }];
        [[NSRunLoop mainRunLoop] addTimer:sampler forMode:NSRunLoopCommonModes];
        [[NSRunLoop mainRunLoop] addTimer:closer forMode:NSRunLoopCommonModes];
        // As the status item's own click handling does: the menu is attached
        // only for the click, and the click tracks it until it closes.
        item.menu = m;
        [item.button performClick:nil];
        item.menu = nil;
        CFAbsoluteTime closedAt = CFAbsoluteTimeGetCurrent();
        [sampler invalidate];
        [closer invalidate];
        [[NSNotificationCenter defaultCenter] removeObserver:obs];
        g_probe_running = NO;
        NSDictionary *report = @{
            @"opened" : @(opened),
            @"open_seconds" : @(opened ? closedAt - openedAt : 0),
            @"samples" : samples,
        };
        NSData *json = [NSJSONSerialization dataWithJSONObject:report options:0 error:nil];
        NSString *s = json ? [[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding] : @"{}";
        cb(ctx, s.UTF8String);
    });
    return 1;
}
