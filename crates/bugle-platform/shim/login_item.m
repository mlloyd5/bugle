// Open at Login for Bugle.app through SMAppService.mainAppService (macOS 13+).
//
// Kept apart from shim.m: it only means something when the process runs from
// an app bundle, and it is the only user of ServiceManagement. Bindings live
// in src/login_item.rs.

#import <Foundation/Foundation.h>
#import <ServiceManagement/ServiceManagement.h>
#include <string.h>

/// 1 when the process runs from a `.app` bundle with a bundle identifier.
int bugle_running_in_app_bundle(void) {
    NSBundle *b = NSBundle.mainBundle;
    return (b.bundleIdentifier != nil && [b.bundlePath.pathExtension isEqualToString:@"app"]) ? 1 : 0;
}

/// SMAppServiceStatus of the main app: 0 not registered, 1 enabled,
/// 2 requires approval, 3 not found; -1 before macOS 13.
int bugle_login_item_status(void) {
    if (@available(macOS 13.0, *)) {
        return (int)SMAppService.mainAppService.status;
    }
    return -1;
}

/// Registers (enabled != 0) or unregisters the app as a login item. Returns
/// the new status, or -1 with a malloc'd message in *err (free with bugle_free).
int bugle_login_item_set(int enabled, char **err) {
    if (err) *err = NULL;
    if (@available(macOS 13.0, *)) {
        SMAppService *svc = SMAppService.mainAppService;
        NSError *e = nil;
        BOOL ok = enabled ? [svc registerAndReturnError:&e] : [svc unregisterAndReturnError:&e];
        if (!ok) {
            if (err) {
                const char *u = (e.localizedDescription ?: @"login item request failed").UTF8String;
                *err = strdup(u ? u : "login item request failed");
            }
            return -1;
        }
        return (int)svc.status;
    }
    if (err) *err = strdup("Open at Login needs macOS 13 or later");
    return -1;
}
