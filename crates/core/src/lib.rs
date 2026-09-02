//! item-core: shared domain model and persistence for the item-finders pipeline.
//!
//! Data flow: detections (per frame) -> observations (tracked object x zone,
//! deduplicated over a time window) -> queried via text search / VLM sidecar.

pub mod geo;
pub mod model;
pub mod store;

pub use model::{Detection, FrameMeta, Observation, Region, Trajectory};
