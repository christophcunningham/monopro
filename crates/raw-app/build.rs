//! Link-time arguments for the macOS application binary.
//!
//! The packaged app carries Sparkle.framework inside `Contents/Frameworks`, and
//! macOS dyld resolves the framework's install name (`@rpath/Sparkle.framework/…`)
//! against the binary's list of run paths. Xcode adds `@executable_path/../Frameworks`
//! for free; a hand-built bundle has to add it itself, or the app dies at launch with
//! "library is not present" the moment the updater links it in. Harmless in
//! development, where the path simply does not resolve and `DYLD_FRAMEWORK_PATH`
//! (see `./run`) does the work instead.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    }
}
