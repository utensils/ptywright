-- events.lua — append-only TurnEvent emitter for the claude-code plugin.
--
-- Bridges the gap between the polled-snapshot classifier and the
-- structured event stream consumers want. Each `M.tick(state)` call
-- diffs newly-extracted text / tools against what has been emitted
-- and returns only the new events. Callers thread the host-supplied
-- `last_event_seq` through `M.filter(events, last_event_seq)` so each
-- classifier tick returns at most-once delivery per consumer
-- watermark.
--
-- The emitter is per-adapter (each `adapter.start` mints a fresh Lua
-- VM) so module-level state cannot leak across adapters. Within a
-- single adapter, the emitter survives across classify calls and
-- resets at turn boundaries (`reset_for_turn`).
--
-- Wire shape (matches src/extension.rs::TurnEvent):
--   { kind = "text_delta",     seq, text }
--   { kind = "tool_started",   seq, id, name, input? }
--   { kind = "tool_progress",  seq, id, summary }
--   { kind = "tool_completed", seq, id, status, result? }
--   { kind = "turn_complete",  seq, text? }
--   { kind = "error",          seq, category, message? }

local M = {}

M._next_seq = 1
M._emitted_text = ""
M._active_tools = {}
M._completed_tool_keys = {}
M._turn_complete_emitted = false
M._error_signature = nil
M._buffer = {}
M._last_turn_start = nil

-- Hard cap on the buffered event queue. The plugin re-emits events
-- with `seq > last_event_seq`, so a consumer that never advances
-- its watermark would otherwise see unbounded growth. 1024 is more
-- than enough for any single turn — a turn with that many events
-- would already overflow visible chrome.
local MAX_BUFFER = 1024

local function next_seq()
  local seq = M._next_seq
  M._next_seq = seq + 1
  return seq
end

local function push(event)
  table.insert(M._buffer, event)
  if #M._buffer > MAX_BUFFER then
    table.remove(M._buffer, 1)
  end
  return event
end

-- Reset the emitter for a fresh turn. Called when the host's
-- `turn_start` marker has advanced past the last value the emitter
-- saw — meaning a new prompt was submitted and a fresh turn began.
-- The seq counter keeps advancing so it stays globally monotonic
-- across turns; consumers do not need to know about turn boundaries
-- to dedupe correctly.
function M.reset_for_turn()
  M._emitted_text = ""
  M._active_tools = {}
  M._completed_tool_keys = {}
  M._turn_complete_emitted = false
  M._error_signature = nil
end

-- Returns true if the supplied `turn_start` value indicates a new
-- turn (i.e. it differs from the last value observed by the
-- emitter). Idempotent for repeated calls with the same value, so
-- it's safe to call on every classify tick.
--
-- `turn_start` is the host's transcript marker for the current
-- turn — it advances each time the plugin's `send_prompt` intent
-- requests a fresh `Action::MarkTranscript("turn_start")` mark, and
-- the host stamps it at a higher transcript cursor. Polling the
-- same turn returns the same marker value, so the emitter only
-- resets on a real turn boundary.
function M.observe_turn_start(turn_start)
  if turn_start == nil then return false end
  if M._last_turn_start == turn_start then return false end
  M._last_turn_start = turn_start
  M.reset_for_turn()
  return true
end

-- Discover the longest common prefix between `prev` and `next` as
-- a UTF-8-safe byte count. The cleaner path: when the new text
-- contains the previously-emitted text as a prefix, return the
-- byte-length of `prev`; otherwise fall back to byte-level common
-- prefix scan. Avoids the chars-iter overhead of the old Rust
-- `streaming_text_delta` while preserving its intent.
local function common_prefix_bytes(prev, nextv)
  if prev == "" then return 0 end
  if nextv == "" then return 0 end
  if string.sub(nextv, 1, #prev) == prev then return #prev end
  local limit = math.min(#prev, #nextv)
  local i = 0
  while i < limit and string.byte(prev, i + 1) == string.byte(nextv, i + 1) do
    i = i + 1
  end
  return i
end

-- Emit a TextDelta for `new_text` versus what's already been
-- emitted. Handles three shapes:
--   * `new_text` extends the prior text — emit the suffix.
--   * `new_text` shrinks (reflow / chrome change) — re-emit from
--     scratch as a single delta. This is rare but matches what a
--     consumer needs when the renderer rewrote the answer area.
--   * `new_text == prev` — nothing to do.
-- Returns the emitted event, or nil when no delta was produced.
function M.observe_text(new_text)
  new_text = new_text or ""
  if new_text == M._emitted_text then return nil end
  local prev = M._emitted_text
  local common = common_prefix_bytes(prev, new_text)
  local delta
  if common == #prev then
    delta = string.sub(new_text, common + 1)
  elseif common > 0 then
    delta = string.sub(new_text, common + 1)
  else
    -- Disjoint rewrite. Surface it as a single delta carrying the
    -- whole new text rather than emit a no-op or attempt overlap
    -- detection — overlap heuristics caused half of Claudette's
    -- previous streaming bugs.
    delta = new_text
  end
  if delta == "" then
    M._emitted_text = new_text
    return nil
  end
  M._emitted_text = new_text
  return push({
    kind = "text_delta",
    seq = next_seq(),
    text = delta,
  })
end

-- Diff `visible_tools` (the plugin's `parse_visible_tool_calls`
-- output) against in-flight + completed state to emit `tool_started`
-- / `tool_progress` / `tool_completed` events. `visible_tools` is the
-- table the classifier already builds — passing it through here
-- avoids duplicate parsing.
--
-- Tool keys are matched as-is (the plugin owns the key shape).
-- Status transitions:
--   * Not previously seen and currently visible → tool_started.
--   * Previously visible and now disappeared    → tool_completed.
--   * Previously visible with different summary → tool_progress.
--   * Currently visible with non-running status → tool_started
--     followed by tool_completed in the same tick (visible-then-
--     done tools complete before the next poll).
function M.observe_tools(visible_tools)
  visible_tools = visible_tools or {}
  local emitted = {}
  local current_keys = {}
  for _, tool in ipairs(visible_tools) do
    if tool.key then current_keys[tool.key] = tool end
  end

  -- Newly-visible tools.
  for _, tool in ipairs(visible_tools) do
    local key = tool.key
    if key and not M._completed_tool_keys[key] then
      local active = M._active_tools[key]
      if not active then
        local started = push({
          kind = "tool_started",
          seq = next_seq(),
          id = key,
          name = tool.name or "",
          input = tool.input,
        })
        table.insert(emitted, started)
        M._active_tools[key] = {
          name = tool.name,
          summary = tool.summary or "",
          status = tool.status or "running",
        }
        if (tool.status or "running") ~= "running" then
          local done = push({
            kind = "tool_completed",
            seq = next_seq(),
            id = key,
            status = tool.status,
          })
          table.insert(emitted, done)
          M._completed_tool_keys[key] = true
          M._active_tools[key] = nil
        end
      elseif (tool.summary or "") ~= active.summary then
        local progress = push({
          kind = "tool_progress",
          seq = next_seq(),
          id = key,
          summary = tool.summary or "",
        })
        table.insert(emitted, progress)
        active.summary = tool.summary or ""
      end
    end
  end

  -- Tools that left the visible set complete.
  local stale = {}
  for key, _ in pairs(M._active_tools) do
    if not current_keys[key] then
      table.insert(stale, key)
    end
  end
  for _, key in ipairs(stale) do
    local active = M._active_tools[key]
    local done = push({
      kind = "tool_completed",
      seq = next_seq(),
      id = key,
      status = active and active.status or "completed",
    })
    table.insert(emitted, done)
    M._completed_tool_keys[key] = true
    M._active_tools[key] = nil
  end

  return emitted
end

-- Emit `turn_complete` exactly once per turn. `final_text` is the
-- plugin's best extract of the assistant's final message — carried
-- on the event so simple consumers don't have to rebuild from
-- deltas. Returns nil after the first call for the current turn.
function M.observe_turn_complete(final_text)
  if M._turn_complete_emitted then return nil end
  M._turn_complete_emitted = true
  return push({
    kind = "turn_complete",
    seq = next_seq(),
    text = final_text,
  })
end

-- Emit `error` exactly once per banner. `signature` lets the caller
-- dedupe — typically the trimmed banner line — so screen-stable
-- error states do not re-emit on every tick.
function M.observe_error(category, message, signature)
  if not category or category == "" then return nil end
  signature = signature or message or category
  if M._error_signature == signature then return nil end
  M._error_signature = signature
  return push({
    kind = "error",
    seq = next_seq(),
    category = category,
    message = message,
  })
end

-- Filter the emitter's full buffer down to events with `seq >
-- last_event_seq`. Called by the classifier when assembling the
-- response so consumers see at-most-once delivery against their
-- watermark.
function M.filter(last_event_seq)
  if last_event_seq == nil or last_event_seq == 0 then
    -- Take a copy so the caller can mutate freely without poking
    -- the emitter's internal buffer.
    local copy = {}
    for i, e in ipairs(M._buffer) do copy[i] = e end
    return copy
  end
  local out = {}
  for _, e in ipairs(M._buffer) do
    if e.seq and e.seq > last_event_seq then
      table.insert(out, e)
    end
  end
  return out
end

-- Test-only helper: returns the raw internal buffer length. Used by
-- the Lua test harness to assert bounded growth.
function M._buffer_size()
  return #M._buffer
end

-- Test-only helper: returns the current sequence counter. Used by
-- tests to verify monotonicity across reset_for_turn.
function M._next_seq_value()
  return M._next_seq
end

return M
