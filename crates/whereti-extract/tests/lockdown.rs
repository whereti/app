//! The worker gives up its rights before reading anything: once it has,
//! it cannot write a file even where its user can (here, the temp folder).
//! Started by the broker, it also has a restricted token: it cannot read a
//! file only its user may read, yet it reads what it is handed.

use std::path::PathBuf;

use whereti_extract::worker::{EmbedWorker, Sandbox, TagWorker};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_whereti-extract"))
}

/// A folder of the temp folder: only its user (and SYSTEM, Administrators) may read it.
fn private_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("whereti-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_worker_cannot_write_after_its_lockdown() {
    let dir = private_dir("lockdown");
    let probe = dir.join("probe.txt");
    // the test itself can write there
    std::fs::write(dir.join("ours.txt"), b"x").unwrap();
    let status = std::process::Command::new(exe()).args(["--probe-lockdown", probe.to_str().unwrap()]).status().unwrap();
    let written = probe.exists();
    // the same, started by the broker
    let code = Sandbox::new(exe()).probe_exit(&["--probe-lockdown", probe.to_str().unwrap()]);
    let written_too = probe.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(0), "the worker could still write after its lockdown");
    assert!(!written, "the worker wrote its file");
    assert_eq!(code, Some(0), "the sandboxed worker could still write");
    assert!(!written_too, "the sandboxed worker wrote its file");
}

#[test]
fn the_sandboxed_worker_cannot_read_its_users_files() {
    let dir = private_dir("restricted");
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, b"not for the parser").unwrap();
    let sandbox = Sandbox::new(exe());
    assert!(sandbox.restricted(), "{}", sandbox.sandbox_report());
    // started as is, the worker could read it (it runs as this user)...
    let status = std::process::Command::new(exe()).args(["--probe-read", secret.to_str().unwrap()]).status().unwrap();
    assert_eq!(status.code(), Some(1));
    // ...started by the broker, it cannot
    let code = sandbox.probe_exit(&["--probe-read", secret.to_str().unwrap()]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, Some(0), "the sandboxed worker read a file only its user may read");
}

#[test]
fn the_sandboxed_worker_still_reads_what_it_is_handed() {
    // bytes on stdin
    let sandbox = Sandbox::new(exe());
    let e = sandbox.extract_bytes("html", "<p>caf&eacute; &amp; ceai</p>".as_bytes().to_vec(), 1000).unwrap();
    assert_eq!(e.text, "café & ceai");
    // a mailbox as a handle (Microsoft's empty sample), from a private folder
    let sample = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))
        .and_then(|home| {
            let reg = home.join("registry").join("src");
            std::fs::read_dir(reg).ok()?.flatten().map(|d| d.path().join("outlook-pst-1.2.0").join("examples").join("Empty.pst")).find(|p| p.is_file())
        });
    if let Some(sample) = sample {
        let dir = private_dir("mailbox");
        let pst = dir.join("mail.pst");
        std::fs::copy(&sample, &pst).unwrap();
        let got = sandbox.extract_file(&pst, 10_000);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.unwrap().text, "");
    }
}

/// The photo model's worker gets its model as a handle: with the
/// restricted token it could not open it (here a private temp file, like
/// ProgramData\whereti for the service). Needs onnxruntime.dll beside the
/// worker; without it, skipped.
#[test]
fn the_model_worker_gets_its_model_as_a_handle() {
    if !exe().with_file_name("onnxruntime.dll").is_file() {
        eprintln!("no onnxruntime.dll beside the worker: skipped");
        return;
    }
    let dir = private_dir("model");
    let model = dir.join("model.onnx");
    std::fs::write(&model, b"not a real model").unwrap();
    let tokenizer = dir.join("tokenizer.json");
    std::fs::write(&tokenizer, b"{}").unwrap();
    let mut w = TagWorker::with_exe(exe(), &model);
    let first = w.tags(b"photo").unwrap_err();
    // the model was read (and then refused by ONNX Runtime), not unreadable
    assert!(first.contains("cannot load the model"), "{first}");
    // and not loaded again for every photo
    let again = w.tags(b"photo").unwrap_err();
    drop(w);
    // the text model gets both its files
    let mut e = EmbedWorker::with_exe(exe(), &model, &tokenizer);
    let embed = e.embed("passage: text").unwrap_err();
    drop(e);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(first, again);
    assert!(embed.contains("cannot load the model"), "{embed}");
}
