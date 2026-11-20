//! #7: the snapshot types are canonical in the orchestrator (the module that
//! publishes them on the hub); the web layer re-exports them here so the SSE
//! payload and the late-subscriber DB rebuild share one serialized shape.
pub use orchestrator::snapshot::{build_snapshot, LaneSnapshot, RunSnapshot, Snapshot};
