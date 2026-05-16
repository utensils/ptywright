<script setup lang="ts">
// Operator home — recreates the homepage-explorations artboards (01 dark, 02
// light cool). All chrome is driven by --op-* CSS variables defined in style.css
// so light/dark switches automatically with the VitePress theme toggle.
//
// The PTY frame block (.op-pty-*) is intentionally hard-coded dark in both
// modes — design intent: a real terminal sitting on the page.
import { withBase } from 'vitepress'
import RuntimeSchematic from './RuntimeSchematic.vue'

const ptyLines = [
  {
    prompt: 'pty>',
    body: 'session.spawn(',
    arg: '"claude"',
    suffix: ').attach()',
    note: 'spawned pid 48211 · 80×24 · alt-screen',
  },
  {
    prompt: 'pty>',
    body: 'wait(',
    arg: 'screen_stable',
    suffix: '(',
    arg2: '250ms',
    suffix2: '))',
    note: 'stable after 412ms · seq 14',
  },
  {
    prompt: 'pty>',
    body: 'send.text(',
    arg: '"summarize SPEC.md"',
    suffix: ')',
    note: 'wrote 18 bytes · cursor 02:18',
  },
  {
    prompt: 'pty>',
    body: 'wait(',
    arg: 'matches',
    suffix: '(',
    arg2: 'r"Approve\\? \\(y/n\\)"',
    suffix2: '))',
    badge: true,
    note: 'matched · row 19 ·',
  },
  {
    prompt: 'pty>',
    body: 'send.key(',
    arg: '"y"',
    suffix: ') · transcript.snapshot()',
    note: '1.2 KiB · redacted 2 patterns',
  },
]
</script>

<template>
  <div class="op-home-root">
    <!-- Subtle 8x16 grid backdrop -->
    <div class="op-grid" aria-hidden="true" />

    <!-- Hero -->
    <section class="op-hero">
      <div class="op-hero-copy">
        <div class="op-eyebrow">// rust library · cli · json-rpc</div>
        <h1 class="op-h1">
          Drive any<br />
          interactive<br />
          terminal,<br />
          <span class="op-h1-accent">from code.</span>
        </h1>
        <p class="op-lede">
          ptywright is a Rust crate and CLI for spawning, observing, and driving
          PTY-backed terminal applications. Use it as a library in your tests, a
          binary in your scripts, or a JSON-RPC server for agents and UAT
          pipelines — one vocabulary across all three.
        </p>
        <div class="op-actions">
          <a class="op-btn-pri" :href="withBase('/guide/installation')"
            >$ cargo add ptywright</a
          >
          <a class="op-btn-sec" :href="withBase('/guide/architecture')"
            >read the spec →</a
          >
        </div>
        <div class="op-meta">
          <span class="op-meta-item"
            ><span class="op-dot op-dot-accent" />macOS</span
          >
          <span class="op-meta-item"
            ><span class="op-dot op-dot-accent" />Linux</span
          >
          <span class="op-meta-item"
            ><span class="op-dot op-dot-accent" />Windows</span
          >
          <span class="op-meta-item"
            ><span class="op-dot op-dot-mute" />early dev</span
          >
        </div>
      </div>

      <!-- PTY frame — always dark -->
      <div class="op-pty">
        <div class="op-pty-bar">
          <span class="op-pty-btn" />
          <span class="op-pty-btn" />
          <span class="op-pty-btn" />
          <div class="op-pty-tabs">
            <span class="op-pty-tab is-active"
              >session-01 · claude-code</span
            >
            <span class="op-pty-tab">session-02 · zsh</span>
            <span class="op-pty-tab-plus">+</span>
          </div>
          <span class="op-pty-status">80×24 · stable</span>
        </div>
        <div class="op-pty-body">
          <template v-for="(l, i) in ptyLines" :key="i">
            <div class="op-line" :style="{ marginTop: i === 0 ? 0 : '14px' }">
              <span class="op-prompt">{{ l.prompt }}</span>
              <span>
                {{ l.body }}<span class="op-cyan">{{ l.arg }}</span
                ><template v-if="l.arg2">{{ l.suffix
                  }}<span class="op-amber">{{ l.arg2 }}</span
                  >{{ l.suffix2 }}</template
                ><template v-else>{{ l.suffix }}</template>
              </span>
            </div>
            <div class="op-line op-line-out">
              <span class="op-dim">   ↳</span>
              <span class="op-dim">
                {{ l.note }}
                <span v-if="l.badge" class="op-badge">turn complete</span>
              </span>
            </div>
          </template>
          <div class="op-line" style="margin-top: 14px">
            <span class="op-prompt">pty&gt;</span>
            <span class="op-cursor-line"
              >_<span class="op-cursor">▌</span></span
            >
          </div>
        </div>
        <pre class="op-ascii">┌── observer ──┬── matcher ──┬── transcript ──┐
│ 80×24 cells  │ regex+temp. │ 64 KiB ring    │
│ vt100 engine │ on-stable   │ raw stream opt │
└──────────────┴─────────────┴────────────────┘</pre>
      </div>
    </section>

    <!-- 4-cell strip -->
    <section class="op-strip">
      <div class="op-cell">
        <div class="op-cell-num">01 / target</div>
        <div class="op-cell-title">Spawn anything</div>
        <div class="op-cell-body">
          Program, args, env, cwd, terminal size. PTY-first, no shimming.
        </div>
      </div>
      <div class="op-cell">
        <div class="op-cell-num">02 / observe</div>
        <div class="op-cell-title">Cell-level snapshots</div>
        <div class="op-cell-body">
          Style, mode, scrollback, alt-screen. Bounded transcript.
        </div>
      </div>
      <div class="op-cell">
        <div class="op-cell-num">03 / wait</div>
        <div class="op-cell-title">Deterministic matchers</div>
        <div class="op-cell-body">
          Regex, temporal, lifecycle. No sleeps, no flake.
        </div>
      </div>
      <div class="op-cell">
        <div class="op-cell-num">04 / automate</div>
        <div class="op-cell-title">JSON-RPC + Lua</div>
        <div class="op-cell-body">
          NDJSON, LSP framing, sockets. Trusted plugins for adapters.
        </div>
      </div>
    </section>

    <!-- Runtime schematic -->
    <RuntimeSchematic />
  </div>
</template>

<style scoped>
.op-home-root {
  position: relative;
  background: var(--op-bg);
  color: var(--op-ink-2);
  font-family: var(--op-mono);
  font-size: 13px;
  line-height: 1.55;
  overflow: hidden;
}

.op-grid {
  position: absolute;
  inset: 0;
  background-image:
    linear-gradient(var(--op-grid) 1px, transparent 1px),
    linear-gradient(90deg, var(--op-grid) 1px, transparent 1px);
  background-size: 8px 16px;
  pointer-events: none;
  z-index: 0;
}

/* Hero */
.op-hero {
  position: relative;
  z-index: 1;
  display: grid;
  grid-template-columns: minmax(0, 1.05fr) minmax(0, 1.4fr);
  gap: 56px;
  padding: 60px 36px 0;
  max-width: 1440px;
  margin: 0 auto;
}

@media (max-width: 960px) {
  .op-hero {
    grid-template-columns: 1fr;
    gap: 36px;
    padding: 40px 24px 0;
  }
}

.op-eyebrow {
  color: var(--op-accent-t);
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  margin-bottom: 24px;
}
.dark .op-eyebrow {
  color: var(--op-accent);
}

.op-h1 {
  font-family: var(--op-mono);
  font-weight: 500;
  font-size: 56px;
  line-height: 1.02;
  letter-spacing: -0.02em;
  color: var(--op-ink);
  margin: 0 0 28px;
}

@media (max-width: 600px) {
  .op-h1 {
    font-size: 40px;
  }
}

.op-h1-accent {
  color: var(--op-ink);
  background: linear-gradient(
    180deg,
    transparent 60%,
    var(--op-accent) 60%,
    var(--op-accent) 94%,
    transparent 94%
  );
  padding-right: 4px;
}

.dark .op-h1-accent {
  background: none;
  color: var(--op-accent);
}

.op-lede {
  max-width: 460px;
  color: var(--op-mid);
  font-size: 14px;
  line-height: 1.65;
  margin: 0 0 36px;
  font-family: var(--op-mono);
}

.op-actions {
  display: flex;
  gap: 12px;
  margin-bottom: 40px;
  flex-wrap: wrap;
}

.op-btn-pri,
.op-btn-sec {
  padding: 11px 18px;
  font-family: var(--op-mono);
  font-size: 12px;
  letter-spacing: 0.02em;
  text-decoration: none;
  border-radius: 0;
  font-weight: 600;
  transition:
    background 0.15s,
    color 0.15s,
    border-color 0.15s;
}

.op-btn-pri {
  background: var(--op-btn-bg);
  color: var(--op-btn-fg);
  border: 1px solid var(--op-btn-bg);
}
.op-btn-pri:hover {
  background: var(--op-ink-2);
  color: var(--op-btn-fg);
  border-color: var(--op-ink-2);
}
.dark .op-btn-pri:hover {
  background: #d2f56b;
  border-color: #d2f56b;
}

.op-btn-sec {
  background: transparent;
  color: var(--op-ink-2);
  border: 1px solid var(--op-rule-2);
}
.op-btn-sec:hover {
  border-color: var(--op-accent-t);
  color: var(--op-ink);
}
.dark .op-btn-sec {
  color: var(--op-ink-2);
}
.dark .op-btn-sec:hover {
  border-color: var(--op-accent);
  color: var(--op-ink);
}

.op-meta {
  color: var(--op-mute);
  font-size: 11px;
  display: flex;
  gap: 18px;
  flex-wrap: wrap;
}
.op-meta-item {
  display: flex;
  align-items: center;
  gap: 6px;
}
.op-dot {
  width: 8px;
  height: 8px;
  border-radius: 1px;
  display: inline-block;
}
.op-dot-accent {
  background: var(--op-accent);
  box-shadow: 0 0 0 1px rgba(61, 79, 31, 0.2);
}
.op-dot-mute {
  background: var(--op-dim);
}

/* ── PTY frame (always dark) ──────────────────────────────────────────── */
.op-pty {
  background: #0e1014;
  border: 1px solid #1f2126;
  border-radius: 6px;
  box-shadow:
    0 30px 60px -20px rgba(0, 0, 0, 0.6),
    0 2px 0 rgba(0, 0, 0, 0.05),
    inset 0 1px 0 rgba(255, 255, 255, 0.03);
  overflow: hidden;
  margin-top: 8px;
  font-size: 12px;
  color: #bdbdb5;
}

.op-pty-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  border-bottom: 1px solid #1f2126;
  background: #0a0b0d;
}

.op-pty-btn {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: #3a3c41;
}

.op-pty-tabs {
  display: flex;
  gap: 0;
  margin-left: 16px;
  font-size: 11px;
  color: #5b5c55;
}
.op-pty-tab {
  padding: 4px 12px;
  color: #5b5c55;
  border-right: 1px solid #1f2126;
}
.op-pty-tab.is-active {
  color: #d6d6cf;
  background: #15161a;
}
.op-pty-tab-plus {
  padding: 4px 12px;
  color: #5b5c55;
}

.op-pty-status {
  margin-left: auto;
  font-size: 10px;
  color: #5b5c55;
  white-space: nowrap;
}

.op-pty-body {
  padding: 18px 20px;
  min-height: 380px;
}

.op-line {
  display: flex;
  gap: 10px;
  align-items: flex-start;
}
.op-line-out {
  margin-top: 6px;
}

.op-prompt {
  color: var(--op-accent);
  font-weight: 500;
}
.op-dim {
  color: #5b5c55;
}
.op-cyan {
  color: #7dc7d0;
}
.op-amber {
  color: #f5a623;
}

.op-badge {
  display: inline-block;
  padding: 1px 8px;
  background: rgba(198, 242, 78, 0.08);
  color: var(--op-accent);
  border: 1px solid rgba(198, 242, 78, 0.22);
  font-size: 10px;
  margin-left: 2px;
}

.op-cursor-line {
  color: #f4f4ee;
}
.op-cursor {
  color: var(--op-accent);
  animation: op-blink 1s steps(2) infinite;
  margin-left: 2px;
}

.op-ascii {
  border-top: 1px solid #1f2126;
  padding: 14px 20px;
  background: #0a0b0d;
  font-size: 11px;
  color: #7e7f78;
  white-space: pre;
  font-family: var(--op-mono);
  margin: 0;
  overflow-x: auto;
}

/* ── Strip ────────────────────────────────────────────────────────────── */
.op-strip {
  position: relative;
  z-index: 1;
  margin: 60px auto 0;
  max-width: 1440px;
  padding: 28px 36px;
  border-top: 1px solid var(--op-rule);
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 28px;
}

@media (max-width: 800px) {
  .op-strip {
    grid-template-columns: repeat(2, 1fr);
    padding: 28px 24px;
  }
}

.op-cell-num {
  color: var(--op-mute);
  font-size: 11px;
  margin-bottom: 8px;
}
.op-cell-title {
  color: var(--op-ink);
  font-size: 13px;
  margin-bottom: 6px;
  font-weight: 500;
}
.op-cell-body {
  color: var(--op-mid);
  font-size: 12px;
  line-height: 1.55;
}

/* Respect prefers-reduced-motion — freeze the blinking PTY cursor. */
@media (prefers-reduced-motion: reduce) {
  .op-cursor {
    animation: none;
    opacity: 1;
  }
}
</style>
