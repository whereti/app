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
    // through the junction: refused
    let through = link.join("secret.txt");
    assert!(std::fs::read(&through).is_ok(), "the junction itself works");
    let err = open_real(&through).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(read_shared(&through, 100).is_err());

    let _ = std::fs::remove_dir(&link);
    let _ = std::fs::remove_dir_all(&base);
}
