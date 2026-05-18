//! Core library surface for `ptywright`.
//!
//! ptywright is an early-stage Rust CLI and library for driving interactive
//! terminal applications through real PTYs. The core stays generic:
//! application-specific behavior (Claude Code today, additional TUIs tomorrow)
//! lives in Lua plugins under `plugins/<name>/`, layered on top of the
//! reusable target, session, screen, action, matcher, transcript, and
//! extension primitives exposed here. ptywright does not carry typed Rust
//! shims per TUI.

pub mod action;
pub mod config;
pub mod error;
pub mod extension;
pub mod logging;
pub mod lua_plugin;
pub mod matcher;
pub mod paths;
pub mod plugin;
pub mod redaction;
#[cfg(feature = "repl")]
pub mod repl;
pub mod rpc;
pub mod screen;
pub mod session;
pub mod target;
pub mod transcript;

pub use action::{Action, Key, Signal};
pub use config::{Config, LogFormat, LoggingConfig};
pub use error::{Error, Result};
pub use extension::{
    ActionPlan, ClassifyContext, Extension, ExtensionEvent, ExtensionHandle,
    ExtensionStateSnapshot, HostMark, LuaExtension, StateCandidate,
};
pub use logging::{
    LogGuard, RedactingMakeWriter, RedactingWriter, cleanup_old_logs, init_for_oneshot,
    init_for_run, init_for_serve_socket, init_for_serve_stdio,
};
pub use lua_plugin::{LuaPlugin, LuaPluginRegistry};
pub use matcher::{
    MatchOutcome, MatchResult, Matcher, MatcherContext, PluginRegistry, PredicateContext,
    PredicateOutcome,
};
pub use paths::{Paths, expand_tilde};
pub use plugin::{
    DefaultTarget, PluginHostCapabilities, PluginKind, PluginManifest, PluginManifestError,
    PluginPermission, PluginRuntime, builtin_manifests,
};
pub use redaction::RedactionPolicy;
pub use rpc::{
    RpcServer, RpcServerState, serve_lsp, serve_lsp_with_state, serve_ndjson,
    serve_ndjson_with_state,
};
pub use screen::{CursorState, ScreenCell, ScreenCellStyle, ScreenSnapshot, Terminal};
pub use session::{CancellationToken, Session, SessionConfig, SessionEvent, SessionExitStatus};
pub use target::{Target, TerminalSize};
pub use transcript::{Transcript, TranscriptConfig, TranscriptFileConfig};

/// Crate and binary name.
pub const NAME: &str = "ptywright";

/// Package version from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short package description from Cargo metadata.
pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
