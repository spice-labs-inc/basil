//! The basil DSL: a Dockerfile superset plus `TASK`, `PLATFORM`, `OUTPUT`, and
//! top-level `ON` reactions.
//!
//! Lexer + parser + AST.

pub mod ast;
pub mod lexer;
pub mod parser;