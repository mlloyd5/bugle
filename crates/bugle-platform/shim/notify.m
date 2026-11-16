// System notifications through UNUserNotificationCenter.
//
// Kept apart from shim.m: like login_item.m it only works from an app bundle
// (UNUserNotificationCenter raises for a process without a bundle id), and it
// is the only user of UserNotifications. Bindings live in src/notify.rs, which
// checks bugle_running_in_app_bundle() before calling anything here; the
// checks below are a second line of defence.
//
// State is one atomic int the completion handlers write from their own
// queues: 0 not started, 1 waiting for an answer, 2 authorized, 3 denied,
// 4 unavailable (no bundle, or the framework reported an error), 5 not
// determined (the user has not been asked yet).

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#import <UserNotifications/UserNotifications.h>
#include <stdatomic.h>

int bugle_running_in_app_bundle(void);

static _Atomic int g_state = 0;

/// Shows banners while Bugle is the active app (an accessory app rarely is,
/// but the selection overlay makes it active) and opens the attached path
/// when the notification is clicked.
@interface BugleNotifyDelegate : NSObject <UNUserNotificationCenterDelegate>
@end

@implementation BugleNotifyDelegate
- (void)userNotificationCenter:(UNUserNotificationCenter *)center
       willPresentNotification:(UNNotification *)notification
         withCompletionHandler:(void (^)(UNNotificationPresentationOptions))completionHandler {
    completionHandler(UNNotificationPresentationOptionBanner | UNNotificationPresentationOptionList);
}

- (void)userNotificationCenter:(UNUserNotificationCenter *)center
    didReceiveNotificationResponse:(UNNotificationResponse *)response
             withCompletionHandler:(void (^)(void))completionHandler {
    if ([response.actionIdentifier isEqualToString:UNNotificationDefaultActionIdentifier]) {
        id path = response.notification.request.content.userInfo[@"open"];
        if ([path isKindOfClass:NSString.class] && [(NSString *)path length] > 0) {
            NSURL *url = [NSURL fileURLWithPath:path];
            dispatch_async(dispatch_get_main_queue(), ^{
                [NSWorkspace.sharedWorkspace openURL:url];
            });
        }
    }
    completionHandler();
}
@end

static BugleNotifyDelegate *g_delegate;

static int state_from_settings(UNNotificationSettings *s) {
    switch (s.authorizationStatus) {
        case UNAuthorizationStatusAuthorized:
        case UNAuthorizationStatusProvisional:
            return 2;
        case UNAuthorizationStatusDenied:
            return 3;
        default:
            return 5;  // not determined: nobody has asked yet
    }
}

/// Installs the delegate and either asks for authorization (`request` != 0;
/// shows the system prompt once, then answers from the stored decision) or
/// only reads the current setting (never prompts). Returns the state at the
/// time of the call; the answer arrives asynchronously (bugle_notify_state).
int bugle_notify_start(int request) {
    if (!bugle_running_in_app_bundle()) {
        atomic_store(&g_state, 4);
        return 4;
    }
    UNUserNotificationCenter *c = UNUserNotificationCenter.currentNotificationCenter;
    if (!g_delegate) {
        g_delegate = [BugleNotifyDelegate new];
        c.delegate = g_delegate;
    }
    atomic_store(&g_state, 1);
    if (request) {
        [c requestAuthorizationWithOptions:(UNAuthorizationOptionAlert | UNAuthorizationOptionSound)
                         completionHandler:^(BOOL granted, NSError *error) {
                             if (error) NSLog(@"bugle: notification authorization: %@", error.localizedDescription);
                             atomic_store(&g_state, granted ? 2 : (error ? 4 : 3));
                         }];
    } else {
        [c getNotificationSettingsWithCompletionHandler:^(UNNotificationSettings *s) {
            atomic_store(&g_state, state_from_settings(s));
        }];
    }
    return 1;
}

int bugle_notify_state(void) { return atomic_load(&g_state); }

/// Re-reads the setting without prompting, so turning notifications on in
/// System Settings takes effect without a restart. No-op before start.
void bugle_notify_refresh(void) {
    if (!g_delegate) return;
    [UNUserNotificationCenter.currentNotificationCenter getNotificationSettingsWithCompletionHandler:^(UNNotificationSettings *s) {
        atomic_store(&g_state, state_from_settings(s));
    }];
}

/// Posts a notification when authorized. Returns 1 when it was handed to
/// the notification center, 0 when skipped (not authorized). `open` may be
/// NULL; otherwise clicking the notification opens that path.
int bugle_notify_post(const char *title, const char *body, const char *open) {
    if (atomic_load(&g_state) != 2 || !title) return 0;
    UNMutableNotificationContent *content = [UNMutableNotificationContent new];
    content.title = [NSString stringWithUTF8String:title] ?: @"Bugle";
    if (body) content.body = [NSString stringWithUTF8String:body] ?: @"";
    if (open) {
        NSString *p = [NSString stringWithUTF8String:open];
        if (p) content.userInfo = @{@"open" : p};
    }
    UNNotificationRequest *req = [UNNotificationRequest requestWithIdentifier:NSUUID.UUID.UUIDString
                                                                      content:content
                                                                      trigger:nil];
    [UNUserNotificationCenter.currentNotificationCenter
        addNotificationRequest:req
         withCompletionHandler:^(NSError *error) {
             if (!error) return;
             NSLog(@"bugle: notification not delivered: %@", error.localizedDescription);
             // Switched off in System Settings since start: stop trying.
             if ([error.domain isEqualToString:UNErrorDomain] && error.code == UNErrorCodeNotificationsNotAllowed)
                 atomic_store(&g_state, 3);
         }];
    return 1;
}
