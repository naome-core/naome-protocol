//! Local storage, exchange and evaluation of Research Evidence Protocol records.
//! Disable the default `cli` feature to embed the library without the command line or trial fixtures.

pub mod adoption;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "cli")]
pub mod conformance;
pub mod evaluation;
pub mod executor;
#[cfg(feature = "cli")]
pub mod fixture;
pub mod investigation;
pub mod kit;
pub mod learning;
pub mod migration;
pub mod payload;
pub mod profile;
pub mod projection;
pub mod store;
pub mod training_target;
pub mod wire;
