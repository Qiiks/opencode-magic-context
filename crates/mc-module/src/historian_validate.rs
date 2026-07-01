//! Historian output validation: parse the historian's compartment output and
//! validate it against the already-persisted store (contiguity, coverage,
//! unprocessed_from, no-progress and discard-last healing) before anything is
//! published. A malformed or stale output is rejected here, never persisted.
//!
//! Stub: implementation lands with the writer-relocation W2 unit.
