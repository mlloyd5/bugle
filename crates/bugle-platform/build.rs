fn main() {
    println!("cargo:rerun-if-changed=shim/shim.m");
    println!("cargo:rerun-if-changed=shim/fixtures.m");
    println!("cargo:rerun-if-changed=shim/shim.h");
    println!("cargo:rerun-if-changed=shim/picker.m");
    println!("cargo:rerun-if-changed=shim/panel.m");
    println!("cargo:rerun-if-changed=shim/login_item.m");
    println!("cargo:rerun-if-changed=shim/editor.m");
    println!("cargo:rerun-if-changed=shim/notify.m");
    println!("cargo:rerun-if-changed=shim/video_editor.m");
    println!("cargo:rerun-if-changed=shim/prompt.m");
    println!("cargo:rerun-if-changed=shim/smoke_input.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("shim/shim.m")
        .file("shim/picker.m")
        // Separate object: only tests reference it, so shipped binaries
        // leave it out.
        .file("shim/fixtures.m")
        .file("shim/panel.m")
        .file("shim/login_item.m")
        .file("shim/editor.m")
        .file("shim/notify.m")
        .file("shim/video_editor.m")
        .file("shim/prompt.m")
        .file("shim/smoke_input.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=13.0")
        .flag("-Wno-deprecated-declarations")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-unguarded-availability-new")
        .compile("bugleshim");
    for fw in [
        "Foundation",
        "AppKit",
        "CoreGraphics",
        "CoreFoundation",
        "Vision",
        "ScreenCaptureKit",
        "AVFoundation",
        "CoreMedia",
        "CoreVideo",
        "ImageIO",
        "ApplicationServices",
        "ServiceManagement",
        "UserNotifications",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
