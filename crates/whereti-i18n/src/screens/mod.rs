//! One catalog per screen, as in SilentSilo (`src/i18n/screens`).

use crate::Entry;

pub mod common;
pub mod dates;
pub mod preview;
pub mod search;
pub mod settings;
pub mod setup;
pub mod tray;
pub mod welcome;

/// Every catalog.
pub const ALL: &[&[Entry]] = &[
    common::ENTRIES,
    search::ENTRIES,
    preview::ENTRIES,
    dates::ENTRIES,
    welcome::ENTRIES,
    settings::ENTRIES,
    tray::ENTRIES,
    setup::ENTRIES,
];
