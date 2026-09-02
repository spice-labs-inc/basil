//! The basil DSL: a Dockerfile superset plus `TASK`, `PLATFORM`, and top-level
//! `ON` reactions.
//!
//! Phase 1: lexer + parser + AST. See `docs/PLAN.md` Phase 1.

pub mod ast;
pub mod lexer;
pub mod parser;