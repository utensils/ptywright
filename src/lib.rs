//! Core library surface for `ptywright`.
//!
//! ptywright is an early-stage Rust CLI and library for driving interactive
//! terminal applications through real PTYs. The core stays generic: Claude Code
//! and other application-specific behavior should be layered on top of reusable
//! target, session, screen, action, matcher, and transcript primitives.

pub mod action;
pub mod adapters;
pub mod error;
pub mod matcher;
pub mod plugin;
pub mod rpc;
pub mod screen;
pub mod session;
pub mod target;
pub mod transcript;

pub use action::{Action, Key};
pub use adapters::{ClaudeCodeAdapter, ClaudeCodeConfig, ClaudeCodeState, ClaudeCodeStateSnapshot};
pub use error::{Error, Result};
pub use matcher::{MatchResult, Matcher, MatcherContext};
pub use plugin::{
    PluginHostCapabilities, PluginKind, PluginManifest, PluginManifestError, PluginPermission,
};
pub use rpc::{RpcServer, serve_lsp, serve_ndjson};
pub use screen::{CursorState, ScreenCell, ScreenCellStyle, ScreenSnapshot, Terminal};
pub use session::{Session, SessionConfig, SessionExitStatus};
pub use target::{Target, TerminalSize};
pub use transcript::{Transcript, TranscriptConfig};

/// Crate and binary name.
pub const NAME: &str = "ptywright";

/// Package version from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short package description from Cargo metadata.
pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
