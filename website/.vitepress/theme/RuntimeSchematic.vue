<script setup lang="ts">
// fig.01 — the runtime, end to end. Nine nodes wired into a single SVG canvas.
// Light and dark variants share geometry; only stroke/fill colors swap via
// CSS variables exposed by .op-schem-root + .dark scope.
import { computed } from 'vue'
import { useData } from 'vitepress'

const { isDark } = useData()

interface Node {
  id: string
  x: number
  y: number
  w: number
  h: number
  label: string
  sub: string
  file?: string
}

const SVG_W = 1440
const SVG_H = 950

const nodes: Node[] = [
  {
    id: 'caller',
    x: 590,
    y: 60,
    w: 280,
    h: 64,
    label: 'AGENT · CLI · LIB · RPC',
    sub: 'three surfaces, one runtime',
  },
  {
    id: 'rpc',
    x: 590,
    y: 174,
    w: 280,
    h: 64,
    label: 'JSON-RPC',
    sub: 'stdio · ndjson · lsp · socket',
    file: 'RPC.RS',
  },
  {
    id: 'session',
    x: 590,
    y: 288,
    w: 280,
    h: 64,
    label: 'SESSION',
    sub: 'PTY lifecycle · reader · writer',
    file: 'SESSION.RS',
  },
  {
    id: 'screen',
    x: 380,
    y: 412,
    w: 280,
    h: 76,
    label: 'SCREEN',
    sub: 'cell · style · mode · cursor',
    file: 'SCREEN.RS',
  },
  {
    id: 'trans',
    x: 800,
    y: 412,
    w: 280,
    h: 76,
    label: 'TRANSCRIPT',
    sub: 'bounded · redacted · streamed',
    file: 'TRANS.RS',
  },
  {
    id: 'match',
    x: 380,
    y: 564,
    w: 280,
    h: 76,
    label: 'MATCHER',
    sub: 'regex · stable · exited',
    file: 'MATCH.RS',
  },
  {
    id: 'action',
    x: 800,
    y: 564,
    w: 280,
    h: 76,
    label: 'ACTION',
    sub: 'keys · write · resize · int',
    file: 'ACTION.RS',
  },
  {
    id: 'turn',
    x: 590,
    y: 716,
    w: 280,
    h: 64,
    label: 'TURN',
    sub: 'orchestrate · capture · respond',
    file: 'TURN.RS',
  },
  {
    id: 'adapter',
    x: 590,
    y: 830,
    w: 280,
    h: 64,
    label: 'ADAPTER (lua)',
    sub: 'claude-code · your tui',
  },
]

const map = Object.fromEntries(nodes.map((n) => [n.id, n]))
const cx = (n: Node) => n.x + n.w / 2

const edges: [string, string][] = [
  ['caller', 'rpc'],
  ['rpc', 'session'],
  ['session', 'screen'],
  ['session', 'trans'],
  ['screen', 'match'],
  ['trans', 'action'],
  ['screen', 'action'],
  ['trans', 'match'],
  ['match', 'turn'],
  ['action', 'turn'],
  ['turn', 'adapter'],
]

const annotations = [
  { y: 206, text: '└ optional · framed protocols' },
  { y: 320, text: '└ portable-pty · cross-platform' },
  { y: 450, text: '└ vt100 seam · swappable engine' },
  { y: 602, text: '└ event-driven · no sleeps' },
  { y: 748, text: '└ orchestration boundary' },
  { y: 862, text: '└ trusted lua · permissioned' },
]

const edgePaths = computed(() =>
  edges.map(([a, b], i) => {
    const A = map[a]
    const B = map[b]
    const x1 = cx(A)
    const y1 = A.y + A.h
    const x2 = cx(B)
    const y2 = B.y
    const path = `M ${x1} ${y1} C ${x1} ${(y1 + y2) / 2}, ${x2} ${(y1 + y2) / 2}, ${x2} ${y2}`
    return { i, path, dur: 2.6 + (i % 3) * 0.5, begin: i * 0.22 }
  })
)
</script>

<template>
  <section class="op-schem-root" :class="{ 'is-dark': isDark }">
    <div class="op-schem-grid" aria-hidden="true" />
    <div class="op-schem-glow" aria-hidden="true" />

    <div class="op-schem-inner">
      <header class="op-schem-head">
        <div>
          <div class="op-schem-eyebrow">
            // fig.01 · the runtime, end to end
          </div>
          <h2 class="op-schem-title">
            Nine layers.<br />
            <span class="op-schem-title-accent"
              >Every one reusable on its own.</span
            >
          </h2>
        </div>
        <div class="op-schem-meta">
          DATA FLOWS TOP TO BOTTOM.<br />
          ↳ HOVER ANY NODE TO INSPECT.<br />
          ↳ EACH BOX MAPS TO A FILE IN
          <span class="op-schem-src">SRC/</span>.
        </div>
      </header>

      <div class="op-schem-svg-wrap">
        <svg
          :viewBox="`0 0 ${SVG_W} ${SVG_H}`"
          class="op-schem-svg"
          width="100%"
          preserveAspectRatio="xMidYMid meet"
        >
          <defs>
            <marker
              id="op-arrow"
              viewBox="0 0 10 10"
              refX="9"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto"
            >
              <path d="M0,0 L10,5 L0,10 z" class="op-schem-arrow-fill" />
            </marker>
          </defs>

          <!-- Edges -->
          <g class="op-schem-edges">
            <template v-for="ep in edgePaths" :key="ep.i">
              <path
                :d="ep.path"
                class="op-schem-edge"
                fill="none"
                marker-end="url(#op-arrow)"
              />
              <path :id="`op-p${ep.i}`" :d="ep.path" fill="none" stroke="none" />
              <circle r="3" class="op-schem-dot">
                <animateMotion
                  :dur="`${ep.dur}s`"
                  repeatCount="indefinite"
                  :begin="`${ep.begin}s`"
                >
                  <mpath :href="`#op-p${ep.i}`" />
                </animateMotion>
              </circle>
            </template>
          </g>

          <!-- Nodes -->
          <g class="op-schem-nodes">
            <g v-for="n in nodes" :key="n.id">
              <rect
                :x="n.x"
                :y="n.y"
                :width="n.w"
                :height="n.h"
                rx="3"
                class="op-schem-node-rect"
              />
              <!-- corner brackets -->
              <g
                v-for="(c, k) in [
                  [n.x, n.y, 1, 1],
                  [n.x + n.w, n.y, -1, 1],
                  [n.x, n.y + n.h, 1, -1],
                  [n.x + n.w, n.y + n.h, -1, -1],
                ]"
                :key="k"
              >
                <line
                  :x1="c[0]"
                  :y1="c[1]"
                  :x2="c[0] + c[2] * 10"
                  :y2="c[1]"
                  class="op-schem-bracket"
                />
                <line
                  :x1="c[0]"
                  :y1="c[1]"
                  :x2="c[0]"
                  :y2="c[1] + c[3] * 10"
                  class="op-schem-bracket"
                />
              </g>
              <text
                :x="n.x + 16"
                :y="n.y + 26"
                class="op-schem-node-label"
              >
                {{ n.label }}
              </text>
              <text :x="n.x + 16" :y="n.y + 46" class="op-schem-node-sub">
                {{ n.sub }}
              </text>
              <text
                v-if="n.file"
                :x="n.x + n.w - 16"
                :y="n.y + n.h - 10"
                text-anchor="end"
                class="op-schem-node-file"
              >
                {{ n.file }}
              </text>
            </g>
          </g>

          <!-- Side annotations -->
          <g class="op-schem-annotation">
            <text
              v-for="(a, i) in annotations"
              :key="i"
              x="90"
              :y="a.y"
            >
              {{ a.text }}
            </text>
          </g>
          <g class="op-schem-annotation" text-anchor="end">
            <text :x="SVG_W - 90" y="450">observation ┐</text>
            <text :x="SVG_W - 90" y="602">side-effects ┐</text>
          </g>

          <!-- Frame ticks -->
          <g class="op-schem-frame">
            <line x1="60" y1="40" x2="60" :y2="SVG_H - 40" />
            <line :x1="SVG_W - 60" y1="40" :x2="SVG_W - 60" :y2="SVG_H - 40" />
          </g>
          <g class="op-schem-frame-text">
            <text x="68" y="50">N 00</text>
            <text x="68" :y="SVG_H - 44">N 09</text>
            <text :x="SVG_W - 92" y="50">FIG·01</text>
          </g>
        </svg>
      </div>

      <footer class="op-schem-foot">
        <div>
          <div class="op-schem-foot-label">// caller</div>
          <div class="op-schem-foot-body">
            Drop into Rust with
            <span class="op-schem-cargo">cargo add</span>, shell out to the CLI,
            or speak JSON-RPC from any language.
          </div>
        </div>
        <div>
          <div class="op-schem-foot-label">// observe</div>
          <div class="op-schem-foot-body">
            Screen and transcript update on every reader tick. Snapshots are
            cheap, structured, and serializable.
          </div>
        </div>
        <div>
          <div class="op-schem-foot-label">// wait</div>
          <div class="op-schem-foot-body">
            Matcher fires on regex, stable screen, or process exit. Action
            commits the next step. Sleeps are forbidden by design.
          </div>
        </div>
        <div>
          <div class="op-schem-foot-label">// adapt</div>
          <div class="op-schem-foot-body">
            Wrap an app's prompt grammar in a trusted Lua adapter. Claude Code
            ships in-tree; bring your own TUI next.
          </div>
        </div>
      </footer>
    </div>
  </section>
</template>

<style scoped>
.op-schem-root {
  position: relative;
  border-top: 1px solid var(--op-rule);
  margin-top: 60px;
  padding: 60px 36px 80px;
  background: var(--op-bg-2);
  font-family: var(--op-mono);
  overflow: hidden;

  /* Schematic-scoped palette; values flip with .is-dark below. */
  --schem-node-bg: #ffffff;
  --schem-node-bd: #d4d7d0;
  --schem-bracket: #7a9a2a;
  --schem-edge: #aab2a3;
  --schem-dot-stroke: var(--op-accent-t);
  --schem-glow: rgba(198, 242, 78, 0.3);
  --schem-arrow: #aab2a3;
}

.op-schem-root.is-dark {
  --schem-node-bg: #0c0d10;
  --schem-node-bd: #1f2126;
  --schem-bracket: var(--op-accent);
  --schem-edge: #3d4f1f;
  --schem-dot-stroke: var(--op-accent);
  --schem-glow: rgba(198, 242, 78, 0.05);
  --schem-arrow: #3d4f1f;
}

.op-schem-grid {
  position: absolute;
  inset: 0;
  background-image:
    linear-gradient(var(--op-grid) 1px, transparent 1px),
    linear-gradient(90deg, var(--op-grid) 1px, transparent 1px);
  background-size: 8px 16px;
  pointer-events: none;
}

.op-schem-glow {
  position: absolute;
  inset: 0 0 auto 0;
  height: 380px;
  background: radial-gradient(
    ellipse 50% 70% at 50% 0%,
    var(--schem-glow) 0%,
    transparent 70%
  );
  pointer-events: none;
}

.op-schem-inner {
  position: relative;
  max-width: 1440px;
  margin: 0 auto;
}

.op-schem-head {
  display: flex;
  justify-content: space-between;
  align-items: flex-end;
  margin-bottom: 36px;
  gap: 24px;
  flex-wrap: wrap;
}

.op-schem-eyebrow {
  color: var(--op-accent-t);
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  margin-bottom: 14px;
  font-family: var(--op-mono);
}

.op-schem-title {
  font-family: var(--op-mono);
  font-weight: 500;
  font-size: 36px;
  line-height: 1.08;
  letter-spacing: -0.02em;
  color: var(--op-ink);
  margin: 0;
  max-width: 640px;
}

.op-schem-title-accent {
  color: var(--op-ink);
  background: linear-gradient(
    180deg,
    transparent 62%,
    var(--op-accent) 62%,
    var(--op-accent) 94%,
    transparent 94%
  );
  padding-right: 4px;
}

.is-dark .op-schem-title-accent {
  background: none;
  color: var(--op-accent);
}

.op-schem-meta {
  font-size: 11px;
  color: var(--op-mute);
  text-align: right;
  letter-spacing: 0.08em;
  max-width: 280px;
}

.op-schem-src {
  color: var(--op-accent-t);
}
.is-dark .op-schem-src {
  color: var(--op-accent);
}

.op-schem-svg-wrap {
  position: relative;
  width: 100%;
  overflow-x: auto;
}

.op-schem-svg {
  display: block;
  min-width: 880px;
}

.op-schem-edge {
  stroke: var(--schem-edge);
  stroke-width: 1.4;
  opacity: 0.95;
}

.op-schem-arrow-fill {
  fill: var(--schem-arrow);
}

.op-schem-dot {
  fill: var(--op-accent);
  stroke: var(--schem-dot-stroke);
  stroke-width: 1;
}

.op-schem-node-rect {
  fill: var(--schem-node-bg);
  stroke: var(--schem-node-bd);
  stroke-width: 1;
}

.op-schem-bracket {
  stroke: var(--schem-bracket);
  stroke-width: 1.6;
}
.is-dark .op-schem-bracket {
  stroke-width: 1.4;
}

.op-schem-node-label {
  fill: var(--op-ink);
  font-family: var(--op-mono);
  font-size: 13px;
  letter-spacing: 0.08em;
  font-weight: 600;
}
.is-dark .op-schem-node-label {
  font-weight: 500;
  fill: var(--op-ink);
}

.op-schem-node-sub {
  fill: var(--op-mute);
  font-family: var(--op-mono);
  font-size: 11px;
}

.op-schem-node-file {
  fill: var(--op-accent-t);
  font-family: var(--op-mono);
  font-size: 9.5px;
  letter-spacing: 0.14em;
  opacity: 0.6;
}
.is-dark .op-schem-node-file {
  opacity: 1;
  fill: var(--schem-edge);
}

.op-schem-annotation {
  font-family: var(--op-mono);
  font-size: 10.5px;
  fill: var(--op-mute);
  letter-spacing: 0.04em;
}
.is-dark .op-schem-annotation {
  fill: var(--op-dim);
}

.op-schem-frame line {
  stroke: var(--op-dim);
  stroke-width: 1;
}
.is-dark .op-schem-frame line {
  stroke: var(--schem-edge);
}

.op-schem-frame-text {
  font-family: var(--op-mono);
  font-size: 9px;
  fill: var(--op-dim);
  letter-spacing: 0.14em;
}
.is-dark .op-schem-frame-text {
  fill: var(--schem-edge);
}

.op-schem-foot {
  margin-top: 36px;
  padding-top: 24px;
  border-top: 1px solid var(--op-rule);
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 28px;
  font-family: var(--op-mono);
}

@media (max-width: 800px) {
  .op-schem-foot {
    grid-template-columns: repeat(2, 1fr);
  }
  .op-schem-root {
    padding: 40px 24px 60px;
  }
  .op-schem-meta {
    text-align: left;
  }
}

.op-schem-foot-label {
  color: var(--op-mute);
  font-size: 11px;
  margin-bottom: 6px;
}

.op-schem-foot-body {
  color: var(--op-ink-2);
  font-size: 12px;
  line-height: 1.6;
}
.is-dark .op-schem-foot-body {
  color: var(--op-mid);
}

.op-schem-cargo {
  color: var(--op-accent-t);
  font-weight: 600;
}
.is-dark .op-schem-cargo {
  color: var(--op-accent);
}
</style>
