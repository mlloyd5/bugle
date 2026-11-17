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
// 4 unavailable (no bundle), 5 not determined (the user has not been asked
// yet). After start, the state always comes from the stored setting
// (getNotificationSettings), never from a request's granted/error answer.

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
                             // A refusal often arrives as an error rather than
                             // a plain NO; the stored setting is what counts.
                             [UNUserNotificationCenter.currentNotificationCenter
                                 getNotificationSettingsWithCompletionHandler:^(UNNotificationSettings *s) {
                                     atomic_store(&g_state, state_from_settings(s));
                                 }];
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

/// Attaches the PNG at `path` to `content`. The notification center moves an
/// attachment's file into its own store, so `path` must be a throwaway copy;
/// when it cannot be attached it is removed here.
static void attach_thumbnail(UNMutableNotificationContent *content, NSString *path) {
    NSURL *url = [NSURL fileURLWithPath:path];
    NSError *error = nil;
    UNNotificationAttachment *a =
        [UNNotificationAttachment attachmentWithIdentifier:@"thumbnail"
                                                       URL:url
                                                   options:@{UNNotificationAttachmentOptionsTypeHintKey : @"public.png"}
                                                     error:&error];
    if (a) {
        content.attachments = @[ a ];
    } else {
        NSLog(@"bugle: notification thumbnail not attached: %@", error.localizedDescription);
        [NSFileManager.defaultManager removeItemAtURL:url error:nil];
    }
}

/// Posts a notification when authorized. Returns 1 when it was handed to
/// the notification center, 0 when skipped (not authorized). `open` may be
/// NULL; otherwise clicking the notification opens that path. `thumbnail`
/// may be NULL; otherwise it is a PNG copy the notification center takes
/// (moves) as the attachment. A skipped post leaves it alone; once handed
/// over it is removed here when the request completes (a no-op when the
/// center moved it), or as soon as it cannot be attached.
int bugle_notify_post(const char *title, const char *body, const char *open, const char *thumbnail) {
    if (atomic_load(&g_state) != 2 || !title) return 0;
    UNMutableNotificationContent *content = [UNMutableNotificationContent new];
    content.title = [NSString stringWithUTF8String:title] ?: @"Bugle";
    if (body) content.body = [NSString stringWithUTF8String:body] ?: @"";
    if (open) {
        NSString *p = [NSString stringWithUTF8String:open];
        if (p) content.userInfo = @{@"open" : p};
    }
    NSString *thumb = thumbnail ? [NSString stringWithUTF8String:thumbnail] : nil;
    if (thumb.length > 0) attach_thumbnail(content, thumb);
    UNNotificationRequest *req = [UNNotificationRequest requestWithIdentifier:NSUUID.UUID.UUIDString
                                                                      content:content
                                                                      trigger:nil];
    [UNUserNotificationCenter.currentNotificationCenter
        addNotificationRequest:req
         withCompletionHandler:^(NSError *error) {
             // Either way the copy is done with: delivered, the attachment
             // store holds its own (a moved file is already gone, so this is
             // a no-op); not delivered, it is ours to remove.
             if (thumb.length > 0) [NSFileManager.defaultManager removeItemAtPath:thumb error:nil];
             if (!error) return;
             NSLog(@"bugle: notification not delivered: %@", error.localizedDescription);
             // Switched off in System Settings since start: stop trying.
             if ([error.domain isEqualToString:UNErrorDomain] && error.code == UNErrorCodeNotificationsNotAllowed)
                 atomic_store(&g_state, 3);
         }];
    return 1;
}
