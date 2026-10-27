//! The worker gives up its rights before reading anything: once it has,
//! it cannot write a file even where its user can (here, the temp folder).

#[test]
fn the_worker_cannot_write_after_its_lockdown() {
    let dir = std::env::temp_dir().join(format!("whereti-lockdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let probe = dir.join("probe.txt");
    // the test itself can write there
    std::fs::write(dir.join("ours.txt"), b"x").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_whereti-extract"))
        .args(["--probe-lockdown", probe.to_str().unwrap()])
        .status()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(0), "the worker could still write after its lockdown");
    assert!(!probe.exists());
}
