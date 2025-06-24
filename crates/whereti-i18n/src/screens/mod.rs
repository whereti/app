//! One catalog per screen, as in SilentSilo (`src/i18n/screens`).

use crate::Entry;

pub mod common;
pub mod search;
pub mod preview;
pub mod dates;
pub mod welcome;
pub mod settings;
pub mod updates;
pub mod tray;
pub mod setup;

/// Every catalog.
pub const ALL: &[&[Entry]] = &[common::ENTRIES, search::ENTRIES, preview::ENTRIES, dates::ENTRIES, welcome::ENTRIES, settings::ENTRIES, updates::ENTRIES, tray::ENTRIES, setup::ENTRIES];
