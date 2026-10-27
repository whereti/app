//! The service reads a file only at its real path: through a junction it
//! is refused, so a folder swapped for one cannot send it elsewhere.

use whereti_extract::worker::{open_real, read_shared};

#[test]
fn a_file_is_read_only_at_its_real_path() {
    let base = std::env::temp_dir().join(format!("whereti-real-{}", std::process::id()));
    let target = base.join("someone else");
    let link = base.join("mine");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("secret.txt"), b"not yours").unwrap();
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .unwrap();
    assert!(made.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&made.stderr));

    // at its real path: fine (any case)
    assert!(open_real(&target.join("secret.txt")).is_ok());
    assert!(open_real(&base.join("SOMEONE ELSE").join("SECRET.TXT")).is_ok());
    assert_eq!(read_shared(&target.join("secret.txt"), 100).unwrap(), b"not yours");
    // by its short 8.3 name (as a temp folder often is): still its real path
    let short = short_path(&target.join("secret.txt"));
    if short != target.join("secret.txt") {
        assert!(open_real(&short).is_ok(), "short path {} refused", short.display());
    }
    // with a \\?\ prefix: the same path
    let prefixed = std::path::PathBuf::from(format!(r"\\?\{}", target.join("secret.txt").display()));
    assert!(open_real(&prefixed).is_ok(), "{} refused", prefixed.display());
    // through a SUBST drive letter: the same file, at its real path
    if let Some(drive) = Subst::new(&target) {
        let through = std::path::PathBuf::from(format!(r"{}\secret.txt", drive.0));
        assert!(open_real(&through).is_ok(), "{} refused", through.display());
    }
    // through the junction: refused
    let through = link.join("secret.txt");
    assert!(std::fs::read(&through).is_ok(), "the junction itself works");
    let err = open_real(&through).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(read_shared(&through, 100).is_err());

    let _ = std::fs::remove_dir(&link);
    let _ = std::fs::remove_dir_all(&base);
}

/// A SUBST drive letter for a folder, for this test only (removed on drop).
struct Subst(String);

impl Subst {
    fn new(dir: &std::path::Path) -> Option<Subst> {
        let free = ('M'..='Y').rev().map(|c| format!("{c}:")).find(|d| !std::path::Path::new(&format!(r"{d}\")).exists())?;
        let made = std::process::Command::new("subst").arg(&free).arg(dir).status().ok()?;
        made.success().then_some(Subst(free))
    }
}

impl Drop for Subst {
    fn drop(&mut self) {
        let _ = std::process::Command::new("subst").arg(&self.0).arg("/d").status();
    }
}

/// The 8.3 form of a path, if the volume keeps short names.
fn short_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 512];
    // SAFETY: a NUL-terminated path and a buffer of its length.
    let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return path.to_path_buf();
    }
    std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n]))
}
