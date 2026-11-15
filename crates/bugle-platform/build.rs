fn main() {
    println!("cargo:rerun-if-changed=shim/shim.m");
    println!("cargo:rerun-if-changed=shim/fixtures.m");
    println!("cargo:rerun-if-changed=shim/shim.h");
    println!("cargo:rerun-if-changed=shim/picker.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("shim/shim.m")
        .file("shim/picker.m")
        // Separate object: only tests reference it, so shipped binaries
        // leave it out.
        .file("shim/fixtures.m")
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
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
