include!("../../assets/windows/resources.rs");

fn main() {
    embed_windows_resources("mixercraftd", "MixerCraft engine");
    embed_macos_info_plist();
}

/// macOS asks for microphone access per program and needs a usage text in
/// the program's Info.plist. The engine runs outside the app bundle's main
/// executable (started by launchd at sign-in, or by the app), so it carries
/// its own Info.plist in the `__TEXT,__info_plist` section; without one,
/// capture devices deliver silence instead of showing the permission prompt.
fn embed_macos_info_plist() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("set by cargo");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>org.mixercraft.engine</string>
  <key>CFBundleName</key>
  <string>MixerCraft engine</string>
  <key>CFBundleShortVersionString</key>
  <string>{version}</string>
  <key>CFBundleVersion</key>
  <string>{version}</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>MixerCraft mixes and routes the audio of your input devices.</string>
</dict>
</plist>
"#
    );
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo")).join("Info.plist");
    std::fs::write(&out, plist).expect("write Info.plist");
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}", out.display());
}
