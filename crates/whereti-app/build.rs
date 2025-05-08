//! The whereti icon and file details, embedded in whereti.exe (Explorer,
//! taskbar, Start menu, Settings → Apps).

fn main() {
    let ico = concat!(env!("CARGO_MANIFEST_DIR"), "/../../media/icons/whereti.ico");
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico)
            .set("ProductName", "whereti")
            .set("FileDescription", "whereti, instant file search")
            .set("LegalCopyright", "MIT license");
        res.compile().expect("embedding the icon (needs rc.exe from the Windows SDK)");
    }
}
