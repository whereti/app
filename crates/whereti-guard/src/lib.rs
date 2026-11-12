//! What a panic does in whereti's programs.
//!
//! A panic is a bug, and a program that carries on after one may be in any
//! state: every program stops at once (`stop_on_panic`, called first thing
//! in `main`), as it would with `panic = "abort"`.
//!
//! The one exception is a parser of someone else's file format run inside
//! `guarded`: it works on bytes it was given and touches nothing else, so a
//! panic in it (a bug met on a damaged file) costs only that file. This
//! needs `panic = "unwind"` in the release profile (Cargo.toml): with
//! "abort" there is nothing to catch, and the guard would do nothing.

use std::cell::Cell;

#[cfg(not(panic = "unwind"))]
compile_error!("whereti needs panic = \"unwind\": `guarded` catches nothing without it (see [profile.release] in Cargo.toml)");

thread_local! {
    /// How many `guarded` calls this thread is inside of.
    static GUARDED: Cell<u32> = const { Cell::new(0) };
}

/// From here on a panic ends the program on the spot, with its message,
/// unless it happens inside `guarded`. Called once, at the start of `main`.
pub fn stop_on_panic() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // a thread that is ending has no count any more: not guarded
        if GUARDED.try_with(Cell::get).unwrap_or(0) == 0 {
            default(info);
            std::process::abort();
        }
    }));
}

/// Runs `work`; `None` if it panicked. Only for work that keeps to its own
/// data (a parser on bytes), on this thread: a thread it starts is not
/// guarded.
pub fn guarded<T>(work: impl FnOnce() -> T) -> Option<T> {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            let _ = GUARDED.try_with(|g| g.set(g.get().saturating_sub(1)));
        }
    }
    GUARDED.with(|g| g.set(g.get() + 1));
    let _leave = Leave;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth() -> u32 {
        GUARDED.with(Cell::get)
    }

    #[test]
    fn a_panic_inside_costs_only_that_work() {
        assert_eq!(guarded(|| 7), Some(7));
        let damaged: &[u8] = &[];
        assert_eq!(guarded(|| damaged[3]), None);
        assert_eq!(depth(), 0, "left again, after a panic too");
    }

    #[test]
    fn guards_nest() {
        let inner_failed = guarded(|| {
            assert_eq!(depth(), 1);
            let inner = guarded(|| -> u8 { panic!("inner") });
            assert_eq!(depth(), 1, "still inside the outer one");
            inner.is_none()
        });
        assert_eq!(inner_failed, Some(true));
        assert_eq!(depth(), 0);
    }
}
