---
applyTo: "tests/fixtures/claude_code/**"
---

# Claude Code classifier fixtures

`tests/fixtures/claude_code/<name>.txt` is a sanitized screen capture. The sibling `<name>.expected.json` declares the expected classification.

## Adding a fixture (documentation-only change)

1. Drop `<name>.txt` with the screen text.
2. Drop `<name>.expected.json` with:
   ```json
   {
     "state": "<classifier state>",
     "evidence": "<exact evidence string>",
     "last_intent": "<optional intent string or omit>",
     "min_confidence": <float, e.g. 0.6>,
     "metadata": { "<key>": "<value>" }
   }
   ```
   `metadata` is OPTIONAL — assert specific keys / values in the
   plugin's `state_snapshot` metadata when the test needs that
   precision (welcome-screen / usage / permission / status-bar
   metadata). Omit it otherwise.
3. `tests/lua_classifier_tests.rs::classifier_matches_sanitized_claude_code_fixtures` picks it up automatically.

## Hand-edit vs real capture

Real captures from a working Claude session are accurate but messy (welcome banner, status bar variations, ANSI residue). Hand-edited fixtures isolate the structural variation under test:

- For "classifier should/shouldn't fire `completed_turn`": include the marker row + trailing chrome + (when applicable) a prompt-echo row above with the substance pattern you want to test.
- For "classifier handles status-bar shape": include a representative status bar (`<user> @ <host>  /path  [Model X.Y]` and `⏵⏵ auto mode on …`).
- For "no false-positive on prose": include the prose phrase verbatim plus the trailing chrome / status bar Claude renders around it.

## Sanitization

- Replace real user names / hostnames with `user` / `host` or `james` / `laptop` style placeholders, but keep the structural shape (token + `@` + token + `/path  [Model]`).
- Replace project paths with `/Users/me/Projects/sample` style placeholders.
- Replace tokens / API keys / URLs with shape-preserving fakes.

## Review anti-patterns

- **"Add a fixture for every code change"** — fixtures are for STRUCTURAL classification variations. Logic bugs that don't change classification shape get unit tests, not fixtures.
- **"Use raw uncleaned capture"** — leaks user-identifying info. Sanitize.
- **"Add a `description` field to the expected JSON"** — the supported schema is `state` / `evidence` / `last_intent` / `min_confidence` / `metadata`. Test-discovery rejects unknown fields.
- **"Capture against an outdated Claude Code version"** — the test matrix is the working contract against current Claude Code. If Claude Code's TUI changes, update the fixtures AND the classifier together; don't pin to old shapes.
