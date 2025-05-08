//! `wt inspect <path>` - what Windows, the MFT and the index say about one file.
//! A diagnostic for index differences (Administrator).

use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;

use clap::Args;
use whereti_core::snapshot;
use whereti_ntfs::parse::FRN_MASK;
use whereti_ntfs::{journal, Volume};

use crate::index_cmd::default_index_path;

#[derive(Args)]
pub struct InspectArgs {
    /// File or folder to inspect
    path: PathBuf,
    /// Index file (default: %LOCALAPPDATA%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
}

pub fn run(args: InspectArgs) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(0x80) // FILE_READ_ATTRIBUTES
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&args.path)
        .map_err(|e| format!("cannot open {}: {e}", args.path.display()))?;
    // SAFETY: valid handle, zeroed out-parameter.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let frn = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    let record = frn & FRN_MASK;
    let ft = ((info.ftLastWriteTime.dwHighDateTime as u64) << 32) | info.ftLastWriteTime.dwLowDateTime as u64;
    println!("path              {}", args.path.display());
    println!("MFT record        {record} (sequence {})", frn >> 48);
    println!("Windows says      modified minute {}", ft / 600_000_000);

    let letter = args.path.to_string_lossy().chars().next().unwrap_or('C');
    let vol = Volume::open(letter, false).map_err(|e| crate::open_error(letter, e))?;
    let mut buf = Vec::new();
    match journal::read_record(&vol, record, &mut buf).map_err(|e| e.to_string())? {
        Some(r) => {
            let std = r.std.unwrap_or_default();
            let names: Vec<_> = r.names.iter().map(|n| (n.parent_ref & FRN_MASK, n.namespace, n.name.as_str())).collect();
            println!(
                "MFT record says   modified minute {} · size {:?} · names {names:?} · attribute list {}",
                std.modified / 600_000_000,
                r.data_size,
                r.has_attribute_list
            );
        }
        None => println!("MFT record says   not in use"),
    }

    let path = args.index.unwrap_or_else(default_index_path);
    let index = snapshot::load(&path).map_err(|e| e.to_string())?;
    let Some(v) = index.volume_index(letter) else {
        println!("index             volume {letter}: not indexed");
        return Ok(());
    };
    let entries = index.entries_for_record(v, record);
    if entries.is_empty() {
        println!("index             no entry for this record");
    }
    for e in entries {
        println!(
            "index entry {e:>8}  modified minute {} · size {} · flags {:#06x} · {}",
            index.modified(e),
            index.size(e),
            index.flags(e),
            index.full_path(e)
        );
    }
    Ok(())
}
