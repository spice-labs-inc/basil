//! Basil — a plain-process, buildkit-graph-owning build tool.
//!
//! See `docs/DESIGN.md` for the architecture and `docs/PLAN.md` for the phased
//! implementation plan.

pub mod dsl;
pub mod emitter;
pub mod event;
pub mod identity;