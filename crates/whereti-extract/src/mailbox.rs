//! Outlook data files (PLAN.md §67): .pst and .ost of classic Outlook,
//! read without Outlook, with Microsoft's own reader (outlook-pst).
//!
//! A mailbox is often gigabytes, so it is not sent to the sandbox as bytes:
//! the service opens it read-only and duplicates that one handle into the
//! worker (in its job, memory-capped, killed after 60 s). Only `.pst` is
//! read: Outlook's `.ost` (the cache of an Exchange / Microsoft 365
//! account) is a format this reader does not open.
//!
//! The text is the messages newest first: subject, sender, recipients and
//! date on their own lines, then the start of the body, until the budget.

use std::io::{Read, Seek};
use std::rc::Rc;

use outlook_pst::ltp::prop_context::PropertyValue;
use outlook_pst::messaging::folder::Folder;
use outlook_pst::messaging::store::{AnsiStore, Store, UnicodeStore};
use outlook_pst::ndb::node_id::NodeId;
use outlook_pst::{AnsiPstFile, PstReader, UnicodePstFile};

use crate::{html_to_text, ExtractError, Out};

/// Characters of each message's body.
const BODY_CHARS: usize = 3000;
/// Folders walked (a damaged file cannot loop forever).
const MAX_FOLDERS: usize = 5000;

const SUBJECT: u16 = 0x0037;
const SENDER_NAME: u16 = 0x0C1A;
const SENDER_EMAIL: u16 = 0x0C1F;
const DISPLAY_TO: u16 = 0x0E04;
const DELIVERY_TIME: u16 = 0x0E06;
const BODY: u16 = 0x1000;
const BODY_HTML: u16 = 0x1013;

/// `open` gives a fresh reader of the same file each time (the Unicode
/// format is tried first, then the old ANSI one).
pub(crate) fn mailbox<R: Read + Seek + 'static>(open: impl Fn() -> std::io::Result<R>, out: &mut Out) -> Result<(), ExtractError> {
    let damaged = |e: std::io::Error| ExtractError::Damaged(e.to_string());
    let store: Rc<dyn Store> = match UnicodePstFile::read_from(Box::new(open().map_err(damaged)?) as Box<dyn PstReader>) {
        Ok(pst) => UnicodeStore::read(Rc::new(pst)).map_err(damaged)?,
        Err(_) => {
            let pst = AnsiPstFile::read_from(Box::new(open().map_err(damaged)?) as Box<dyn PstReader>).map_err(damaged)?;
            AnsiStore::read(Rc::new(pst)).map_err(damaged)?
        }
    };
    // every message, by when it arrived, from every folder under the root
    let root = store.properties().ipm_sub_tree_entry_id().map_err(damaged)?;
    let mut folders: Vec<Rc<dyn Folder>> = vec![store.open_folder(&root).map_err(damaged)?];
    let mut messages: Vec<(i64, u32)> = Vec::new();
    let mut seen = 0;
    while let Some(folder) = folders.pop() {
        seen += 1;
        if seen > MAX_FOLDERS {
            break;
        }
        if let Some(table) = folder.hierarchy_table() {
            for row in table.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                if let Ok(id) = store.properties().make_entry_id(node) {
                    if let Ok(f) = store.open_folder(&id) {
                        folders.push(f);
                    }
                }
            }
        }
        if let Some(table) = folder.contents_table() {
            let context = table.context();
            let time_col = context.columns().iter().position(|c| c.prop_id() == DELIVERY_TIME);
            for row in table.rows_matrix() {
                let time = time_col
                    .and_then(|col| {
                        let columns = row.columns(context).ok()?;
                        let value = columns.get(col)?.as_ref()?;
                        match table.read_column(value, context.columns()[col].prop_type()).ok()? {
                            PropertyValue::Time(t) => Some(t),
                            _ => None,
                        }
                    })
                    .unwrap_or(0);
                messages.push((time, u32::from(row.id())));
            }
        }
    }
    messages.sort_by_key(|m| std::cmp::Reverse(m.0));
    for (time, node) in messages {
        if out.full() {
            out.truncated = true;
            break;
        }
        let Ok(id) = store.properties().make_entry_id(NodeId::from(node)) else { continue };
        let Ok(message) = store.open_message(&id, None) else { continue };
        let props = message.properties();
        let text = |id: u16| props.get(id).and_then(string);
        out.newline();
        out.push(&text(SUBJECT).unwrap_or_default());
        out.newline();
        let from = match (text(SENDER_NAME), text(SENDER_EMAIL)) {
            (Some(n), Some(e)) if n != e => format!("{n} <{e}>"),
            (Some(n), _) => n,
            (None, Some(e)) => e,
            (None, None) => String::new(),
        };
        let mut head = vec![from];
        if let Some(to) = text(DISPLAY_TO) {
            head.push(to);
        }
        if let Some(day) = day_of(time) {
            head.push(day);
        }
        out.push(&head.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "));
        out.newline();
        let body = text(BODY).or_else(|| {
            props.get(BODY_HTML).and_then(|v| match v {
                PropertyValue::Binary(b) => Some(html_to_text(&String::from_utf8_lossy(b.buffer()))),
                other => string(other).map(|h| html_to_text(&h)),
            })
        });
        if let Some(body) = body {
            let short: String = body.chars().take(BODY_CHARS).collect();
            out.push(&short);
            out.newline();
        }
    }
    Ok(())
}

/// A string property (PST strings may start with a 1-byte or 1-char marker).
fn string(v: &PropertyValue) -> Option<String> {
    let s = match v {
        PropertyValue::String8(s) => {
            let b = s.buffer();
            let b = if b.first() == Some(&1) { &b[2.min(b.len())..] } else { b };
            b.iter().map(|&c| c as char).collect::<String>()
        }
        PropertyValue::Unicode(s) => {
            let b = s.buffer();
            let b = if b.first() == Some(&1) { &b[2.min(b.len())..] } else { b };
            String::from_utf16_lossy(b)
        }
        _ => return None,
    };
    let s = s.trim_end_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// A FILETIME (100 ns since 1601) as "2025-08-14".
fn day_of(filetime: i64) -> Option<String> {
    if filetime <= 0 {
        return None;
    }
    let days = filetime / 10_000_000 / 86_400;
    // 1601-01-01 is day 0: back to a civil date (proleptic Gregorian)
    let z = days - 134_774 + 719_468; // to days since 0000-03-01
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_an_empty_mailbox() {
        // 2024-01-01 00:00 UTC as FILETIME
        assert_eq!(day_of(133_485_408_000_000_000).as_deref(), Some("2024-01-01"));
        assert_eq!(day_of(0), None);
        // Microsoft's sample: a valid mailbox with no messages
        let sample = std::env::var_os("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| std::path::PathBuf::from(h).join(".cargo")))
            .and_then(|home| {
                let reg = home.join("registry").join("src");
                std::fs::read_dir(reg).ok()?.flatten().map(|d| d.path().join("outlook-pst-1.2.0").join("examples").join("Empty.pst")).find(|p| p.is_file())
            });
        let Some(sample) = sample else { return };
        let mut out = Out::new(10_000);
        mailbox(|| std::fs::File::open(&sample), &mut out).unwrap();
        assert!(!out.truncated);
        // junk is damaged, not a crash
        let mut out = Out::new(100);
        assert!(mailbox(|| Ok(std::io::Cursor::new(b"!BDN not really a mailbox".to_vec())), &mut out).is_err());
    }
}
