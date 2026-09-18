//! Edamame の controller

pub mod config;
mod external;
pub mod http;
pub mod state;
pub mod storage;

pub use config::ControllerConfig;
pub use http::{HttpError, router};
pub use state::{ControllerState, ExternalRouting, IngestOutcome, MasterResponse};
pub use storage::CanonicalStore;
