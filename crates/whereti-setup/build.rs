//! The installer carries the four programs it installs. They are packed at
//! build time from `WHERETI_PAYLOAD` (set by scripts/package.ps1, which signs
//! them first for a production release) or, for a local build, from
//! target/release.

use std::path::PathBuf;

const FILES: [&str; 4] = ["whereti.exe", "whereti-service.exe", "whereti-extract.exe", "wt.exe"];

fn main() {
    println!("cargo:rerun-if-env-changed=WHERETI_PAYLOAD");
    let dir = match std::env::var_os("WHERETI_PAYLOAD") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"),
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    for f in FILES {
        let path = dir.join(f);
        if !path.is_file() {
            panic!(
                "{} is missing: build the release binaries first (scripts/package.ps1 does it), \
                 or point WHERETI_PAYLOAD at a folder that has them",
                path.display()
            );
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rustc-env=WHERETI_PAYLOAD_DIR={}", dir.display());

    // the whereti icon and file details on the installer itself
    let ico = concat!(env!("CARGO_MANIFEST_DIR"), "/../../media/icons/whereti.ico");
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico)
            .set("ProductName", "whereti")
            .set("FileDescription", "whereti setup")
            .set("LegalCopyright", "MIT license");
        res.compile().expect("embedding the icon (needs rc.exe from the Windows SDK)");
    }
}
