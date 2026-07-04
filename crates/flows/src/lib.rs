//! Orchestration flows for jaum-logs: combine store + adapters to run the
//! pipeline (play, review, ingest, docs, conflict, finish).
//! Thin shell: orchestrates only, never writes feature code and never merges.

/// Claude model used in interactive play/review/setup sessions.
/// Centralized here so it can be swapped in one place.
pub const AGENT_MODEL: &str = "claude-opus-4-8";

pub mod conflict;
pub mod finish;
pub mod ingest;
pub mod parallel;
pub mod play;
pub mod review;
pub mod setup;
