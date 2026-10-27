//! The searches the user made (PLAN.md §60), newest first, in
//! `%APPDATA%\whereti\searches.txt`. A search is remembered when it was used:
//! a result opened, shown in Explorer, dragged, renamed, deleted or copied,
//! or the window closed on its results. Everything stays on this PC;
//! Settings > Search turns it off and forgets it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Settings > Search > "Remember your searches".
static ON: AtomicBool = AtomicBool::new(true);
/// Searches kept.
const MAX: usize = 50;
/// Longer text is not a search anyone types (a pasted paragraph).
const MAX_LEN: usize = 200;

static LIST: Mutex<Option<Vec<String>>> = Mutex::new(None);

fn file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("searches.txt")
}

pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

fn with<T>(f: impl FnOnce(&mut Vec<String>) -> T) -> Option<T> {
    let mut guard = LIST.lock().ok()?;
    let list = guard.get_or_insert_with(|| {
        std::fs::read_to_string(file()).map(|t| t.lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect()).unwrap_or_default()
    });
    Some(f(list))
}

/// Puts `query` first (once: an older copy goes away).
fn push(list: &mut Vec<String>, query: &str) {
    list.retain(|q| !q.eq_ignore_ascii_case(query));
    list.insert(0, query.to_string());
    list.truncate(MAX);
}

/// A search was used.
pub fn record(query: &str) {
    let query = query.trim();
    if !ON.load(Ordering::Relaxed) || query.is_empty() || query.len() > MAX_LEN || query.contains(['\n', '\r']) {
        return;
    }
    let Some(Some(text)) = with(|list| {
        if list.first().is_some_and(|q| q == query) {
            return None; // already first: nothing to write
        }
        push(list, query);
        Some(list.join("\n") + "\n")
    }) else {
        return;
    };
    SAVED.save(file(), text);
}

static SAVED: crate::shell::Saved = crate::shell::Saved::new();

/// The searches, newest first (none while it is off).
pub fn list() -> Vec<String> {
    if !ON.load(Ordering::Relaxed) {
        return Vec::new();
    }
    with(|l| l.clone()).unwrap_or_default()
}

/// The × on one past search: forget just that one.
pub fn remove(query: &str) {
    let Some(text) = with(|list| {
        list.retain(|q| q != query);
        list.join("\n") + "\n"
    }) else {
        return;
    };
    SAVED.save(file(), text);
}

/// Settings > Search > "Forget".
pub fn forget() {
    if let Ok(mut l) = LIST.lock() {
        *l = Some(Vec::new());
    }
    SAVED.delete(&file());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_once_and_at_most_max() {
        let mut list = Vec::new();
        push(&mut list, "factura");
        push(&mut list, "raport");
        push(&mut list, "Factura");
        assert_eq!(list, ["Factura", "raport"]);
        for i in 0..MAX + 10 {
            push(&mut list, &format!("q{i}"));
        }
        assert_eq!(list.len(), MAX);
        assert_eq!(list[0], format!("q{}", MAX + 9));
    }
}
