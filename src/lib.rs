//! Ferry gives every parallel AI coding agent its own git worktree with an
//! instantly forked, already-warm Cargo build.
//!
//! The binary (`ferry`) is the supported interface; this library exists so the
//! CLI and the integration tests share one implementation. Its API is not
//! stable.

pub mod agent;
pub mod base;
pub mod cli;
pub mod config;
pub mod context;
pub mod doctor;
pub mod error;
pub mod fork;
pub mod freshness;
pub mod fsutil;
pub mod git;
pub mod hook;
pub mod lock;
pub mod platform;
pub mod prune;
pub mod remap;
pub mod sandbox;
pub mod setup;
pub mod shim;
pub mod store;
pub mod sys;
pub mod toolchain;
pub mod watch;

pub use error::{Error, Result};
