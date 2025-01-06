//! NTFS metadata readers for whereti.
//!
//! - [`mft`]: reads `$MFT` directly from the volume (names, sizes, dates in one pass).
//! - [`usn_enum`]: lists files with `FSCTL_ENUM_USN_DATA` (documented API, fallback).
//! - [`compare`]: checks that both readers agree.
//!
//! Opening a volume requires administrator rights.

#![cfg(windows)]

pub mod compare;
pub mod mft;
pub mod parse;
pub mod usn_enum;
pub mod volume;

pub use volume::Volume;
