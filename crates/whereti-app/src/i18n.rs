//! Interface language: see the whereti-i18n crate. Settings picks one, or
//! follows the Windows display language.

pub use whereti_i18n::{number, separators, set, tr, Lang};

#[cfg(test)]
pub use whereti_i18n::TEST_LOCK;
