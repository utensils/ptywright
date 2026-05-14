# Reference

ptywright currently exposes a minimal CLI and library skeleton.

## Crate metadata

- Crate: `ptywright`
- Binary: `ptywright`
- Current version: `0.1.0`
- License: MIT
- Repository: <https://github.com/utensils/ptywright>

## Library surface

The current public API is intentionally small:

```rust
pub const NAME: &str;
pub const VERSION: &str;
pub const DESCRIPTION: &str;

pub struct Target {
    pub program: String,
    pub args: Vec<String>,
}
```

This will grow into PTY session management, screen observation, input actions, matchers, and adapters.
