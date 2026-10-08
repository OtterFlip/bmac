//! The BMAC dashboard engine: everything the desktop shell does that is not
//! window management. It has no dependency on Tauri so it can be tested
//! headless.

pub mod browse;
pub mod error;
pub mod history;
pub mod platform;
pub mod preflight;
pub mod protocol;
pub mod registry;
pub mod repo;
pub mod settings;
pub mod supervisor;

pub use error::{EngineError, Result};
