// Shared by the build scripts of the three executables (`include!`d from
// each `build.rs`, so there is no extra crate): embeds the MixerCraft icon
// (`mixercraft.ico`, made by `assets/generate-icons.py`) and version
// information into the binary when the target is Windows. Explorer, the
// taskbar, Alt-Tab and Task Manager read the icon from the .exe; Task Manager
// shows `FileDescription` as the process name.

/// Embeds the icon and version resource. `exe` is the file name without
/// `.exe`; `file_description` is the name Windows shows for the executable
/// (e.g. in Task Manager).
fn embed_windows_resources(exe: &str, file_description: &str) {
    let manifest_dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let icon = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate lives in crates/<name>")
        .join("assets")
        .join("windows")
        .join("mixercraft.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=build.rs");
    // The build script runs on the host; the target decides (cross builds).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    // Version fields (FileVersion, ProductVersion) default to CARGO_PKG_VERSION.
    res.set_icon(icon.to_str().expect("UTF-8 path to the icon"))
        .set("ProductName", "MixerCraft")
        .set("FileDescription", file_description)
        .set("CompanyName", "MixerCraft contributors")
        .set("LegalCopyright", "MixerCraft contributors, MIT OR Apache-2.0")
        .set("InternalName", exe)
        .set("OriginalFilename", &format!("{exe}.exe"));
    res.compile().expect("embed the Windows icon and version resource (needs rc.exe from the Windows SDK)");
}
