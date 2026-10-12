//! Over-the-air updates for ESP32 Rust firmware, delivered over Matter.
//!
//! The core is `no_std`, allocation-free and independent of any ESP crate:
//! partition-table parsing, ESP app-image checks, power-loss-safe OTA-data
//! (boot selection) records, a verified slot writer, and [`Ota`], which ties
//! them to boot confirmation and uploads. The optional `rs-matter-0-2` feature
//! adds [`matter`], a manufacturer-specific cluster that `esp-ota` drives
//! over an ordinary CASE session.
#![no_std]

pub mod chip;
pub mod flash;
pub mod image;
pub mod name;
pub mod otadata;
pub mod partition;
pub mod service;
pub mod upload;

#[cfg(feature = "rs-matter-0-2")]
pub mod matter;

pub use chip::Chip;
pub use name::Name;
pub use otadata::{ImageState, Slot};
pub use service::{BootInfo, Identity, Ota, Rollback, Staged, StartError, Status};
pub use upload::{UploadError, MAX_CHUNK};
