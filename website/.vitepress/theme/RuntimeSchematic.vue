<script setup lang="ts">
// fig.01 — the runtime, end to end.
//
// Nine nodes mapping to ptywright's actual abstraction layers (see
// website/guide/architecture.md). Edges show the two architectural pipelines:
//
//   observation (down)   — pty bytes flow up through screen + transcript,
//                          matcher evaluates predicates, turn waits on a match
//   control     (up)     — turn dispatches actions; actions write back to the
//                          session — this is the "writeback" feedback loop
//                          that makes ptywright a *driver*, not just an observer
//
// Light and dark variants share geometry; only stroke/fill colors swap via
// CSS variables exposed by .op-schem-root + .is-dark scope.
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
    label: 'CALLER',
    sub: 'agent · cli · library · rpc client',
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
    sub: 'target · pty · reader · writer',
    file: 'SESSION.RS',
  },
  {
    id: 'screen',
    x: 380,
    y: 412,
    w: 280,
    h: 76,
    label: 'SCREEN',
    sub: 'vt100 · cell · style · cursor',
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
    sub: 'regex · stable · exited · timeout',
    file: 'MATCH.RS',
  },
  {
    id: 'action',
    x: 800,
    y: 564,
    w: 280,
    h: 76,
    label: 'ACTION',
    sub: 'keys · write · paste · resize · int',
    file: 'ACTION.RS',
  },
  {
    id: 'turn',
    x: 590,
    y: 716,
    w: 280,
    h: 64,
    label: 'TURN · EXTENSION',
    sub: 'classify · plan · wait · capture',
    file: 'EXT.RS',
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

const map = Object.fromEntries(nodes.map((n) => [n.id, n])) as Record<
  string,
  Node
>
const cx = (n: Node) => n.x + n.w / 2

// Forward edges — observation pipeline + dispatch lineage (top to bottom).
// Smooth cubic bezier from bottom-center of source to top-center of target.
const forwardEdges: [string, string][] = [
  ['caller', 'rpc'],
  ['rpc', 'session'],
  ['session', 'screen'],
  ['session', 'trans'],
  ['screen', 'match'],
  ['trans', 'match'],
  ['match', 'turn'],
  ['turn', 'adapter'],
]

const forwardPaths = computed(() =>
  forwardEdges.map(([a, b], i) => {
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

// Dispatch edge — turn → action. Turn is below action in the layout, so the
// arrow has to curve up the right side of TURN and arc into ACTION's bottom.
// This is the control path: TURN decides, ACTION runs.
const dispatchPath = computed(() => {
  const turn = map.turn
  const action = map.action
  const startX = turn.x + turn.w // right edge of turn
  const startY = turn.y + turn.h / 2 // middle of turn vertically
  const endX = action.x + action.w / 2 // bottom-center of action
  const endY = action.y + action.h
  // C-curve out to the right then back in
  return `M ${startX} ${startY} C ${startX + 90} ${startY}, ${endX + 110} ${endY + 40}, ${endX} ${endY}`
})

// Writeback edge — action → session. ACTION sits two rows below SESSION; the
// curve sweeps up the right side, over the top, into SESSION's right edge.
// This is the loop that makes ptywright a *driver* rather than a passive
// observer: every send.key / send.text ends here.
const writebackPath = computed(() => {
  const action = map.action
  const session = map.session
  const startX = action.x + action.w // right edge of action
  const startY = action.y + action.h / 2
  const endX = session.x + session.w // right edge of session
  const endY = session.y + session.h / 2
  // Sweep right, then up, then left
  return `M ${startX} ${startY} C ${startX + 180} ${startY}, ${endX + 180} ${endY}, ${endX} ${endY}`
})

const annotations = [
  { y: 206, text: '└ optional · framed protocols' },
  { y: 320, text: '└ portable-pty · target spawns process' },
  { y: 450, text: '└ vt100 seam · swappable engine' },
  { y: 602, text: '└ event-driven · no sleeps' },
  { y: 748, text: '└ orchestration · classify+plan+wait' },
  { y: 862, text: '└ trusted lua · permissioned plugins' },
]
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
          OBSERVATION FLOWS DOWN.<br />
          ↳ CONTROL LOOPS BACK UP.<br />
          ↳ EACH BOX MAPS TO A FILE IN
          <span class="op-schem-src">SRC/</span>.
        </div>
      </header>

      <div class="op-schem-svg-wrap">
        <svg
          :viewBox="`0 0 ${SVG_W} ${SVG_H}`"
          class="op-schem-svg"
          width="100%"
          role="img"
          aria-label="Nine-layer runtime diagram: caller → JSON-RPC → session → screen + transcript → matcher + action → turn → adapter. Action loops back to session."
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
            <marker
              id="op-arrow-back"
              viewBox="0 0 10 10"
              refX="9"
              refY="5"
              markerWidth="7"
              markerHeight="7"
              orient="auto"
            >
              <path d="M0,0 L10,5 L0,10 z" class="op-schem-arrow-back-fill" />
            </marker>
          </defs>

          <!-- Forward (observation) edges with animated chartreuse dots -->
          <g class="op-schem-edges">
            <template v-for="ep in forwardPaths" :key="ep.i">
              <path
                :d="ep.path"
                class="op-schem-edge"
                fill="none"
                marker-end="url(#op-arrow)"
              />
              <path
                :id="`op-p${ep.i}`"
                :d="ep.path"
                fill="none"
                stroke="none"
              />
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

          <!-- Dispatch edge — turn → action (control: orchestrator dispatches) -->
          <g class="op-schem-edges-back">
            <path
              :d="dispatchPath"
              class="op-schem-edge-back"
              fill="none"
              marker-end="url(#op-arrow-back)"
            />
            <path
              id="op-dispatch"
              :d="dispatchPath"
              fill="none"
              stroke="none"
            />
            <circle r="3" class="op-schem-dot-back">
              <animateMotion dur="3.4s" repeatCount="indefinite" begin="0.4s">
                <mpath href="#op-dispatch" />
              </animateMotion>
            </circle>

            <!-- Writeback — action → session (side-effect: keys/write hit pty) -->
            <path
              :d="writebackPath"
              class="op-schem-edge-back"
              fill="none"
              marker-end="url(#op-arrow-back)"
              stroke-dasharray="6 4"
            />
            <path
              id="op-writeback"
              :d="writebackPath"
              fill="none"
              stroke="none"
            />
            <circle r="3" class="op-schem-dot-back">
              <animateMotion dur="4.2s" repeatCount="indefinite" begin="1.1s">
                <mpath href="#op-writeback" />
              </animateMotion>
            </circle>
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
              <text :x="n.x + 16" :y="n.y + 26" class="op-schem-node-label">
                {{ n.label }}
              </text>
              <text :x="n.x + 16" :y="n.y + 46" class="op-schem-node-sub">
                {{ n.sub }}
              </text>
              <text
                v-if="n.file"
                :x="n.x + n.w - 16"
                :y="n.y + 16"
                text-anchor="end"
                class="op-schem-node-file"
              >
                {{ n.file }}
              </text>
            </g>
          </g>

          <!-- Side annotations (left = layer notes; right = pipeline labels) -->
          <g class="op-schem-annotation">
            <text v-for="(a, i) in annotations" :key="i" x="90" :y="a.y">
              {{ a.text }}
            </text>
          </g>
          <g class="op-schem-annotation" text-anchor="end">
            <text :x="SVG_W - 90" y="380">observation ┐</text>
            <text :x="SVG_W - 90" y="540">predicates ┐</text>
            <text :x="SVG_W - 90" y="640">dispatch ↑</text>
            <text :x="SVG_W - 90" y="340">writeback ↑</text>
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

      <!-- Mobile fallback. The SVG above shrinks below ~720px to the point
           where the in-SVG text (sized in viewBox coordinates) becomes
           unreadable. Below that breakpoint we hide the SVG and present
           the same nine layers as a vertical card stack with arrow
           connectors. Observation flows top → bottom; the writeback
           note below the list mirrors the SVG's amber control-loop
           arrow. -->
      <div
        class="op-schem-mobile"
        role="group"
        aria-label="Runtime layers diagram"
      >
        <ol
          class="op-schem-mobile-list"
          aria-label="Runtime layers, observation flowing top to bottom"
        >
          <li v-for="(n, i) in nodes" :key="n.id" class="op-schem-mobile-node">
            <div class="op-schem-mobile-card">
              <div class="op-schem-mobile-head">
                <span class="op-schem-mobile-num"
                  >N{{ String(i).padStart(2, '0') }}</span
                >
                <span class="op-schem-mobile-label">{{ n.label }}</span>
                <span v-if="n.file" class="op-schem-mobile-file">{{
                  n.file
                }}</span>
              </div>
              <div class="op-schem-mobile-sub">{{ n.sub }}</div>
            </div>
            <div
              v-if="i < nodes.length - 1"
              class="op-schem-mobile-arrow"
              aria-hidden="true"
            >
              ↓
            </div>
          </li>
        </ol>
        <div class="op-schem-mobile-loop">
          <span class="op-schem-mobile-loop-text">
            ↑ writeback · action → session
          </span>
        </div>
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
  --schem-back-edge: #a8530b; /* amber for the control loop */
  --schem-back-arrow: #a8530b;
}

.op-schem-root.is-dark {
  --schem-node-bg: #0c0d10;
  --schem-node-bd: #1f2126;
  --schem-bracket: var(--op-accent);
  --schem-edge: #3d4f1f;
  --schem-dot-stroke: var(--op-accent);
  --schem-glow: rgba(198, 242, 78, 0.05);
  --schem-arrow: #3d4f1f;
  --schem-back-edge: #f5a623;
  --schem-back-arrow: #f5a623;
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
}

.op-schem-svg {
  display: block;
  width: 100%;
  height: auto;
}

/* Mobile fallback is hidden on desktop; the SVG above handles the diagram. */
.op-schem-mobile {
  display: none;
}

.op-schem-mobile-list {
  list-style: none;
  margin: 0;
  padding: 0;
}

.op-schem-edge {
  stroke: var(--schem-edge);
  stroke-width: 1.4;
  opacity: 0.95;
}

.op-schem-edge-back {
  stroke: var(--schem-back-edge);
  stroke-width: 1.5;
  opacity: 0.85;
}

.op-schem-arrow-fill {
  fill: var(--schem-arrow);
}

.op-schem-arrow-back-fill {
  fill: var(--schem-back-arrow);
}

.op-schem-dot {
  fill: var(--op-accent);
  stroke: var(--schem-dot-stroke);
  stroke-width: 1;
}

.op-schem-dot-back {
  fill: var(--schem-back-edge);
  stroke: var(--schem-back-arrow);
  stroke-width: 0.5;
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

/* Below 720px the SVG text (sized in viewBox coordinates) shrinks past
 * the legibility floor. Swap to the stacked-card fallback. */
@media (max-width: 720px) {
  .op-schem-svg-wrap {
    display: none;
  }
  .op-schem-mobile {
    display: block;
  }
}

.op-schem-mobile-node {
  margin: 0;
  padding: 0;
}

.op-schem-mobile-card {
  background: var(--schem-node-bg);
  border: 1px solid var(--schem-node-bd);
  border-radius: 3px;
  padding: 12px 14px;
  position: relative;
}

/* L-bracket corners on each card to mirror the SVG node chrome. */
.op-schem-mobile-card::before,
.op-schem-mobile-card::after {
  content: '';
  position: absolute;
  width: 10px;
  height: 10px;
  border-color: var(--schem-bracket);
  border-style: solid;
  border-width: 0;
}
.op-schem-mobile-card::before {
  top: -1px;
  left: -1px;
  border-top-width: 2px;
  border-left-width: 2px;
}
.op-schem-mobile-card::after {
  bottom: -1px;
  right: -1px;
  border-bottom-width: 2px;
  border-right-width: 2px;
}

.op-schem-mobile-head {
  display: flex;
  align-items: baseline;
  gap: 10px;
  flex-wrap: wrap;
  margin-bottom: 4px;
}

.op-schem-mobile-num {
  font-family: var(--op-mono);
  font-size: 10px;
  letter-spacing: 0.14em;
  color: var(--op-mute);
}
.is-dark .op-schem-mobile-num {
  color: var(--schem-edge);
}

.op-schem-mobile-label {
  font-family: var(--op-mono);
  font-size: 13px;
  font-weight: 600;
  letter-spacing: 0.08em;
  color: var(--op-ink);
}
.is-dark .op-schem-mobile-label {
  font-weight: 500;
}

.op-schem-mobile-file {
  margin-left: auto;
  font-family: var(--op-mono);
  font-size: 9.5px;
  letter-spacing: 0.14em;
  color: var(--op-accent-t);
  opacity: 0.7;
}
.is-dark .op-schem-mobile-file {
  color: var(--schem-edge);
  opacity: 1;
}

.op-schem-mobile-sub {
  font-family: var(--op-mono);
  font-size: 11px;
  color: var(--op-mute);
}

.op-schem-mobile-arrow {
  text-align: center;
  font-family: var(--op-mono);
  font-size: 14px;
  color: var(--schem-arrow);
  line-height: 1;
  padding: 8px 0;
}

.op-schem-mobile-loop {
  margin-top: 14px;
  padding-top: 14px;
  border-top: 1px dashed var(--schem-back-edge);
  text-align: center;
}

.op-schem-mobile-loop-text {
  font-family: var(--op-mono);
  font-size: 11px;
  letter-spacing: 0.04em;
  color: var(--schem-back-edge);
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

/* Respect prefers-reduced-motion — drop the SMIL <animateMotion> dots so the
 * diagram is fully static. Static paths + arrowheads still convey the
 * observation and control pipelines without continuous motion.
 */
@media (prefers-reduced-motion: reduce) {
  .op-schem-dot,
  .op-schem-dot-back {
    display: none;
  }
}
</style>
