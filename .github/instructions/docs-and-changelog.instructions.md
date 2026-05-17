---
applyTo: "website/**,*.md,CHANGELOG.md"
---

# Docs + changelog — review constraints

## CHANGELOG.md

- Hand-maintained, Keep-a-Changelog style. User-visible changes get an entry under `[Unreleased]` in the same PR that makes the change. Release tooling promotes `[Unreleased]` on tag.
- Entries describe behaviour as it ships — never "we plan to". If a behaviour is gated behind a flag or feature, mention it.
- Don't reference commit SHAs or PR numbers in entries; the GitHub release page links to those automatically.

## Website (`website/`)

- VitePress site. Pages live under `website/guide/`, `website/reference/`, `website/concepts/`.
- The `claude-code.md` page documents the plugin's contract. When the plugin's intents, states, or behaviour change, update the page in the same PR.
- Mark planned features as planned. Do NOT imply that unimplemented Windows named-pipe details, embedded plugin runtimes, WASM runtime, redaction policy, or high-fidelity Claude Code turn detection works today.
- Honest install / release docs about platform support.

## AGENTS.md / CLAUDE.md

`AGENTS.md` is canonical; `CLAUDE.md` is a symlink. Edits to either land in `AGENTS.md`. When workflow, layer boundaries, or test conventions change, update `AGENTS.md` in the same PR.

## Review anti-patterns

- **"Move CHANGELOG entries into commit messages and auto-generate"** — out of scope. The hand-maintained changelog is the source of truth and release notes.
- **"Add a docs/ folder mirroring website/"** — pick one. `website/` is the VitePress source; rendered HTML lives at `https://utensils.io/ptywright/`.
- **"Expand the AGENTS.md TOC"** — keep AGENTS.md focused on what an agent needs to make a correct change. Reference pages live in `website/`.
- **"Use Mermaid for diagrams"** — VitePress doesn't render Mermaid by default. Use ASCII or commit pre-rendered SVG.
