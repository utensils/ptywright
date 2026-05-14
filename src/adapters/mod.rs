//! Application-specific adapters built on generic ptywright primitives.

pub mod claude_code;

pub use claude_code::{
    ClaudeCodeAdapter, ClaudeCodeConfig, ClaudeCodeState, ClaudeCodeStateSnapshot,
};
