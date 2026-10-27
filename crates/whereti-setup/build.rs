//! The installer carries the four programs it installs and ONNX Runtime
//! (for recognising what is in photos; Microsoft's signed DLL), each
//! compressed with zstd at its highest level (the installer is less than
//! half the size; unpacking takes a fraction of a second). They are packed at
//! build time from `WHERETI_PAYLOAD` (set by scripts/package.ps1, which signs
//! them first for a production release) or, for a local build, from
//! target/release.

use std::path::PathBuf;

const FILES: [&str; 5] = ["whereti.exe", "whereti-service.exe", "whereti-extract.exe", "wt.exe", "onnxruntime.dll"];

fn main() {
    println!("cargo:rerun-if-env-changed=WHERETI_PAYLOAD");
    // The installer often runs from Downloads, and elevated: the DLLs it
    // links to are looked for in System32 only, never beside it, where
    // anything downloaded could sit (LOAD_LIBRARY_SEARCH_SYSTEM32).
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
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
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let mut installed = 0u64;
    for f in FILES {
        let raw = std::fs::read(dir.join(f)).expect("reading a program to pack");
        installed += raw.len() as u64;
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 22).expect("zstd");
        enc.long_distance_matching(true).expect("zstd");
        enc.window_log(27).expect("zstd");
        let _ = enc.multithread(std::thread::available_parallelism().map_or(1, |n| n.get() as u32));
        std::io::Write::write_all(&mut enc, &raw).expect("compressing");
        let packed = enc.finish().expect("compressing");
        std::fs::write(out.join(format!("{f}.zst")), packed).expect("writing the packed program");
    }
    println!("cargo:rustc-env=WHERETI_PAYLOAD_DIR={}", out.display());
    // what the programs take once unpacked (Settings > Apps shows it)
    println!("cargo:rustc-env=WHERETI_INSTALLED_KB={}", installed / 1024);

    // the whereti icon and file details on the installer itself
    let ico = concat!(env!("CARGO_MANIFEST_DIR"), "/../../media/icons/whereti.ico");
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico)
            .set("ProductName", "whereti")
            .set("FileDescription", "whereti setup")
            .set("CompanyName", "Software Hive S.R.L.")
            .set("LegalCopyright", "Copyright (C) 2026 Software Hive S.R.L. MIT license.");
        res.compile().expect("embedding the icon (needs rc.exe from the Windows SDK)");
    }
}
