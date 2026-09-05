//! Shared integrity boundary for Edge Canon Build Outputs and their provider
//! derivations.
//!
//! This crate intentionally contains no compiler, framework adapter, provider
//! client, or control-plane code. Build, deploy and ingestion therefore apply
//! one validator without pulling those execution layers across trust
//! boundaries.

pub mod canonical_artifact;
pub mod provider_derivation;
