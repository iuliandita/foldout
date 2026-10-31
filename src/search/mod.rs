//! Configured-source searches. Handles bind an exact source and GUID digest to a user.
mod archive_units;
pub mod matching;
pub mod selection;
mod service;
pub use archive_units::*;
pub mod torrent;

pub use service::*;

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;
