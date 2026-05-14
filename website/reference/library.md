# Library reference

The library surface is intentionally small while ptywright is a skeleton.

## Constants

```rust
pub const NAME: &str;
pub const VERSION: &str;
pub const DESCRIPTION: &str;
```

These values are sourced from Cargo metadata.

## `Target`

`Target` describes a terminal program that a future driver can spawn or attach to.

```rust
use ptywright::Target;

let target = Target::new("python").arg("-i");
```

Fields:

| Field     | Type          | Meaning                             |
| --------- | ------------- | ----------------------------------- |
| `program` | `String`      | Executable name or path.            |
| `args`    | `Vec<String>` | Arguments passed to the executable. |

## Planned API families

The next public APIs should grow around these reusable concepts:

- `Session` for PTY lifecycle.
- `Screen` for terminal observation.
- `Action` for input, resize, waits, and signals.
- `Matcher` for prompt and output predicates.
- `Turn` for deterministic request/response orchestration.
- `Adapter` for application-specific workflows.
