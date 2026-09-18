//! Isolated per-run execution layer.
//!
//! A contribution run materializes an isolated [`workspace::RunWorkspace`]
//! snapshot at the attested base revision, applies candidate changes inside
//! it, and executes bounded argv commands through [`runner::BoundedRunner`]
//! after deterministic classification. [`adapters`] maps the workspace's
//! manifests to standard build/test/lint checks. Nothing in this module
//! performs network writes; it exists to produce truthful, reproducible
//! evidence before any human review or submission gate.

pub mod adapters;
pub mod runner;
pub mod workspace;
