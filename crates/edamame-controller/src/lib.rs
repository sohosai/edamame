//! Edamame の controller

pub mod config;
pub mod http;
pub mod state;
pub mod storage;

pub use config::ControllerConfig;
pub use http::{HttpError, router};
pub use state::{ControllerState, IngestOutcome};
pub use storage::CanonicalStore;
