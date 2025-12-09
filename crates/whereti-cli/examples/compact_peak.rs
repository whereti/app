//! Measures the memory a compaction of the name index takes (PLAN.md §52):
//! a synthetic index of `N` entries (default 2.6 million, like a busy PC),
//! then `rebuild_with`, the call the service makes to compact.
//!
//!   cargo run --release -p whereti-cli --example compact_peak [-- 2600000]

use whereti_core::{build, Exclusions, VolumeScan};
use whereti_ntfs::mft::{MftEntry, MftScan};

fn mb(b: usize) -> String {
    format!("{} MB", b >> 20)
}

/// (private bytes now, peak private bytes)
fn memory() -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the current process pseudo-handle and a counters struct of the size given.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.PagefileUsage, c.PeakPagefileUsage)
}

fn main() {
    let n: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2_600_000);
    let mut scan = MftScan::default();
    scan.push(MftEntry { record: 5, parent: 5, is_dir: true, seq: 1, ..Default::default() }, ".");
    // a folder for every 20 files, three levels deep
    let dirs = n / 20;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for d in 0..dirs {
        let parent = if d < 50 { 5 } else { 100 + rnd() % d.min(dirs) };
        scan.push(
            MftEntry { record: 100 + d, parent, is_dir: true, seq: 1, ..Default::default() },
            &format!("folder {d} {:x}", rnd() % 0xffff),
        );
    }
    let exts = ["pdf", "docx", "jpg", "txt", "rs", "dll", "png", "md"];
    for f in 0..n - dirs {
        let r = rnd();
        scan.push(
            MftEntry {
                record: 100 + dirs + f,
                parent: 100 + r % dirs,
                size: r % 1_000_000,
                modified: 133_000_000_000_000_000 + (r % 1_000_000_000_000),
                seq: 1,
                ..Default::default()
            },
            &format!("file_{:x}_{}.{}", r >> 20, f % 997, exts[(r % 8) as usize]),
        );
    }
    let vols = vec![VolumeScan { letter: 'C', serial: 1, journal_id: 1, next_usn: 0, scan }];
    let idx = build(vols, &Exclusions::default()).0;
    let (now, _) = memory();
    println!("index of {} entries: {} private", idx.len(), mb(now));
    let before = memory();
    let t = std::time::Instant::now();
    let fresh = idx.rebuild_with(Vec::new());
    let after = memory();
    println!(
        "compaction: {} ms, peak {} (before {}, +{})",
        t.elapsed().as_millis(),
        mb(after.1),
        mb(before.0),
        mb(after.1.saturating_sub(before.0))
    );
    drop(idx);
    println!("after swap: {} private", mb(memory().0));
    drop(fresh);
}
