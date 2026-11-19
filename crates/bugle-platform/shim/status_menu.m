// Status-menu probe for `bugle-app --smoke-test`: opens the status item's
// menu the way a click on it does, reads the status item title and the
// menu's item titles once while it is open, then closes it. The app logs its
// own meter updates; comparing their times with the open window shows
// whether the meter keeps updating while the menu tracks. The probe wakes
// the run loop only twice (its one sample and the close), so it cannot be
// what drives those updates. Bindings live in src/status_menu.rs.
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
// {"opened": bool, "opened_at": s, "closed_at": s (Unix time), "firings": the
//  probe's own timer firings while open, "sample": {"t": s after opening,
//  "mode": run-loop mode, "title": status item title, "items": [menu item
//  titles], "width": menu width in points} or null}.
// The sample is taken `sample_at` seconds after the menu opens; it closes
// after `seconds`. Returns 0 without calling `cb` when a probe is running,
// an argument is missing, or not on the main thread. Main thread only.
int32_t bugle_status_menu_probe(void *status_item, void *menu, double seconds, double sample_at, bugle_menu_probe_callback_t cb,
                                void *ctx) {
    if (![NSThread isMainThread] || !status_item || !menu || !cb || g_probe_running) return 0;
    NSStatusItem *item = (__bridge NSStatusItem *)status_item;
    NSMenu *m = (__bridge NSMenu *)menu;
    g_probe_running = YES;
    dispatch_async(dispatch_get_main_queue(), ^{
        __block NSDictionary *sample = nil;
        __block int firings = 0;
        __block BOOL opened = NO;
        __block CFAbsoluteTime openedAt = 0;
        id obs = [[NSNotificationCenter defaultCenter] addObserverForName:NSMenuDidBeginTrackingNotification
                                                                   object:m
                                                                    queue:nil
                                                               usingBlock:^(NSNotification *n) {
                                                                   opened = YES;
                                                                   openedAt = CFAbsoluteTimeGetCurrent();
                                                               }];
        NSTimer *sampler = [NSTimer timerWithTimeInterval:sample_at repeats:NO block:^(NSTimer *t) {
            firings++;
            NSMutableArray *titles = [NSMutableArray array];
            for (NSMenuItem *mi in m.itemArray) [titles addObject:mi.title ?: @""];
            sample = @{
                @"t" : @(opened ? CFAbsoluteTimeGetCurrent() - openedAt : -1),
                @"mode" : [NSRunLoop currentRunLoop].currentMode ?: @"",
                @"title" : item.button.title ?: @"",
                @"items" : titles,
                @"width" : @(m.size.width),
            };
        }];
        NSTimer *closer = [NSTimer timerWithTimeInterval:seconds repeats:NO block:^(NSTimer *t) {
            firings++;
            [m cancelTracking];
        }];
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
            @"opened_at" : @(opened ? openedAt + kCFAbsoluteTimeIntervalSince1970 : 0),
            @"closed_at" : @(closedAt + kCFAbsoluteTimeIntervalSince1970),
            @"firings" : @(firings),
            @"sample" : sample ?: [NSNull null],
        };
        NSData *json = [NSJSONSerialization dataWithJSONObject:report options:0 error:nil];
        NSString *s = json ? [[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding] : @"{}";
        cb(ctx, s.UTF8String);
    });
    return 1;
}

// A menu-item title for the level meter: `head` in the menu font, `tail` in
// Menlo at the same size, so a tail of a constant character count always
// draws at a constant width. Menlo rather than the monospaced system font
// (SF Mono): SF Mono has no ▮/▯, so CoreText would draw the bar cells from
// a fallback font of its choosing; Menlo has every character a gauge uses,
// at one advance. The monospaced system font is used only if Menlo is
// missing. No color is set, so AppKit draws the title in the colour of the
// item's state (grey, as the meter lines are disabled). Returns a +1
// NSAttributedString, or NULL for a null argument.
void *bugle_menu_title_mono_tail(const char *head, const char *tail) {
    if (!head || !tail) return NULL;
    @autoreleasepool {
        NSFont *menuFont = [NSFont menuFontOfSize:0];
        CGFloat size = menuFont.pointSize;
        NSFont *mono = [NSFont fontWithName:@"Menlo-Regular" size:size]
                           ?: [NSFont monospacedSystemFontOfSize:size weight:NSFontWeightRegular];
        NSDictionary *headAttrs = @{NSFontAttributeName : menuFont};
        NSDictionary *tailAttrs = @{NSFontAttributeName : mono};
        NSMutableAttributedString *s = [[NSMutableAttributedString alloc] initWithString:@(head) ?: @"" attributes:headAttrs];
        [s appendAttributedString:[[NSAttributedString alloc] initWithString:@(tail) ?: @"" attributes:tailAttrs]];
        NSAttributedString *out = [s copy];
        return (__bridge_retained void *)out;
    }
}

// The drawn width (points) of an NSAttributedString.
double bugle_attributed_width(void *s) {
    if (!s) return 0;
    return ((__bridge NSAttributedString *)s).size.width;
}
