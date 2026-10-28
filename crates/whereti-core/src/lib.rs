//! whereti name index: build from MFT scans, search, save and load.

pub mod build;
pub mod index;
pub mod live;
pub mod search;
pub mod snapshot;

pub use build::{build, filetime_to_minutes, BuildStats, Exclusions, VolumeScan, DEFAULT_GROUPS};
pub use index::{NameIndex, VolumeInfo};
pub use live::{ApplyStats, RecordSource, RecordState, SIZE_UNKNOWN};
pub use search::{SearchOptions, SearchResults, SortBy};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_live;
