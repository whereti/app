//! What a panic does to a whole process once `stop_on_panic` is set: each
//! case runs in a child (this test program started again), since the ones
//! that matter end the process.

use std::process::Command;

const CASE: &str = "WHERETI_GUARD_CASE";

/// The child's side: does what the case named in the environment says.
/// Without one (the normal test run) it does nothing.
#[test]
fn child() {
    let Ok(case) = std::env::var(CASE) else { return };
    whereti_guard::stop_on_panic();
    match case.as_str() {
        "guarded" => {
            let damaged: &[u8] = &[];
            assert_eq!(whereti_guard::guarded(|| damaged[3]), None);
            assert_eq!(whereti_guard::guarded(|| 7), Some(7));
        }
        "unguarded" => panic!("a bug outside a guard"),
        "thread" => {
            // the panic of a thread must not leave the others running
            let _ = std::thread::spawn(|| panic!("a bug on another thread")).join();
            std::process::exit(0);
        }
        "thread-of-guarded" => {
            // a guard covers its own thread only
            let _ = whereti_guard::guarded(|| std::thread::spawn(|| panic!("a bug on a thread started inside")).join());
            std::process::exit(0);
        }
        "after-guarded" => {
            let _ = whereti_guard::guarded(|| panic!("costs nothing"));
            panic!("a bug after the guard was left");
        }
        other => panic!("unknown case {other}"),
    }
}

fn run(case: &str) -> std::process::ExitStatus {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--test-threads", "1"])
        .env(CASE, case)
        .output()
        .unwrap()
        .status
}

/// Ended on the spot: not a clean exit, and not the test harness reporting
/// a failed test (code 101), which would mean the panic was only unwound.
fn stopped_at_once(case: &str) {
    let status = run(case);
    assert!(!status.success() && status.code() != Some(101), "{case}: {status:?}");
}

#[test]
fn a_guarded_panic_costs_nothing() {
    assert!(run("guarded").success());
}

#[test]
fn any_other_panic_ends_the_program_at_once() {
    stopped_at_once("unguarded");
    stopped_at_once("thread");
    stopped_at_once("thread-of-guarded");
    stopped_at_once("after-guarded");
}
