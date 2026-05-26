-- Unit specs for plugins/claude-code/events.lua — the TurnEvent
-- emitter that pairs with `main.lua`'s `M.classify` hook.
--
-- Loaded by `tests/lua_specs.rs`: each spec file runs in a fresh Lua
-- VM with `helpers` and `events` pre-loaded as globals, mirroring
-- the production runtime's `BUILTIN_PLUGINS.modules` bootstrap. The
-- emitter's module-level state (`_next_seq`, `_buffer`, …) therefore
-- starts clean for each `it()` block — same isolation guarantee the
-- production adapter gets (one VM per `adapter.start`).

local events = events

describe("events.observe_text", function()
  before_each(function()
    events._next_seq = 1
    events._emitted_text = ""
    events._buffer = {}
    events._active_tools = {}
    events._completed_tool_keys = {}
    events._turn_complete_emitted = false
    events._error_signature = nil
    events._last_turn_start = nil
  end)

  it("returns nil when the text is unchanged", function()
    expect(events.observe_text("")).to_be_nil()
    expect(events.observe_text("hello")).to_be_truthy()
    expect(events.observe_text("hello")).to_be_nil()
  end)

  it("emits the suffix when text extends", function()
    events.observe_text("Hello ")
    local ev = events.observe_text("Hello world.")
    expect(ev.kind).to_equal("text_delta")
    expect(ev.text).to_equal("world.")
    expect(ev.seq > 1).to_be_truthy()
  end)

  it("emits the full new text when the prior content is rewritten", function()
    events.observe_text("First draft response.")
    local ev = events.observe_text("Completely different message.")
    expect(ev.kind).to_equal("text_delta")
    -- Disjoint rewrite: emit the whole new text (no overlap heuristics).
    expect(ev.text).to_equal("Completely different message.")
  end)

  it("ignores empty deltas while still recording the new emitted state", function()
    events.observe_text("Stable answer.")
    expect(events.observe_text("Stable answer.")).to_be_nil()
    -- _emitted_text should still match — calling observe_text with
    -- the same value is idempotent.
    expect(events._emitted_text).to_equal("Stable answer.")
  end)

  it("assigns strictly increasing seq values across deltas", function()
    local seqs = {}
    events.observe_text("alpha")
    table.insert(seqs, events._buffer[#events._buffer].seq)
    events.observe_text("alpha beta")
    table.insert(seqs, events._buffer[#events._buffer].seq)
    events.observe_text("alpha beta gamma")
    table.insert(seqs, events._buffer[#events._buffer].seq)
    expect(seqs[2] > seqs[1]).to_be_truthy()
    expect(seqs[3] > seqs[2]).to_be_truthy()
  end)
end)

describe("events.observe_tools", function()
  before_each(function()
    events._next_seq = 1
    events._emitted_text = ""
    events._buffer = {}
    events._active_tools = {}
    events._completed_tool_keys = {}
    events._turn_complete_emitted = false
    events._error_signature = nil
    events._last_turn_start = nil
  end)

  it("emits tool_started for newly visible running tools", function()
    local emitted = events.observe_tools({
      { key = "Bash:ls -la", name = "Bash", summary = "ls -la", status = "running", input = { command = "ls -la" } },
    })
    expect(#emitted).to_equal(1)
    expect(emitted[1].kind).to_equal("tool_started")
    expect(emitted[1].id).to_equal("Bash:ls -la")
    expect(emitted[1].name).to_equal("Bash")
  end)

  it("emits tool_started followed by tool_completed when a tool appears already-finished", function()
    local emitted = events.observe_tools({
      { key = "Read:src/lib.rs", name = "Read", summary = "src/lib.rs", status = "completed", input = { file_path = "src/lib.rs" } },
    })
    expect(#emitted).to_equal(2)
    expect(emitted[1].kind).to_equal("tool_started")
    expect(emitted[2].kind).to_equal("tool_completed")
    expect(emitted[2].id).to_equal("Read:src/lib.rs")
  end)

  it("emits tool_completed when an active tool leaves the visible set", function()
    events.observe_tools({
      { key = "Bash:ls", name = "Bash", summary = "ls", status = "running", input = nil },
    })
    local emitted = events.observe_tools({})
    expect(#emitted).to_equal(1)
    expect(emitted[1].kind).to_equal("tool_completed")
    expect(emitted[1].id).to_equal("Bash:ls")
  end)

  it("emits tool_progress when the summary advances on an active tool", function()
    events.observe_tools({
      { key = "Agent:explore", name = "Agent", summary = "Running 2 Explore agents", status = "running" },
    })
    local emitted = events.observe_tools({
      { key = "Agent:explore", name = "Agent", summary = "Running 3 Explore agents", status = "running" },
    })
    expect(#emitted).to_equal(1)
    expect(emitted[1].kind).to_equal("tool_progress")
    expect(emitted[1].summary).to_equal("Running 3 Explore agents")
  end)

  it("never re-emits tool_started for a key once it has been completed", function()
    events.observe_tools({
      { key = "Bash:once", name = "Bash", summary = "once", status = "running" },
    })
    events.observe_tools({})
    -- Same key reappearing — should not re-emit tool_started.
    local emitted = events.observe_tools({
      { key = "Bash:once", name = "Bash", summary = "once", status = "running" },
    })
    expect(#emitted).to_equal(0)
  end)
end)

describe("events.observe_turn_complete", function()
  before_each(function()
    events._next_seq = 1
    events._buffer = {}
    events._turn_complete_emitted = false
  end)

  it("emits exactly once per turn", function()
    local first = events.observe_turn_complete("final answer")
    expect(first.kind).to_equal("turn_complete")
    expect(first.text).to_equal("final answer")
    local second = events.observe_turn_complete("final answer")
    expect(second).to_be_nil()
  end)

  it("accepts nil text and surfaces it as nil on the event", function()
    local ev = events.observe_turn_complete(nil)
    expect(ev.kind).to_equal("turn_complete")
    expect(ev.text).to_be_nil()
  end)
end)

describe("events.observe_error", function()
  before_each(function()
    events._next_seq = 1
    events._buffer = {}
    events._error_signature = nil
  end)

  it("dedupes by signature so the same banner is not re-emitted", function()
    local first = events.observe_error("rate_limit", "5-hour limit reached", "5-hour limit reached")
    expect(first.kind).to_equal("error")
    expect(first.category).to_equal("rate_limit")
    local repeat_ev = events.observe_error("rate_limit", "5-hour limit reached", "5-hour limit reached")
    expect(repeat_ev).to_be_nil()
  end)

  it("emits when the signature changes even with the same category", function()
    events.observe_error("rate_limit", "5-hour limit reached", "5-hour limit reached")
    local newer = events.observe_error("rate_limit", "7-day limit reached", "7-day limit reached")
    expect(newer).to_be_truthy()
    expect(newer.category).to_equal("rate_limit")
    expect(newer.message).to_equal("7-day limit reached")
  end)

  it("ignores empty or missing categories", function()
    expect(events.observe_error(nil, "x", "x")).to_be_nil()
    expect(events.observe_error("", "x", "x")).to_be_nil()
  end)
end)

describe("events.filter", function()
  before_each(function()
    events._next_seq = 1
    events._buffer = {}
    events._emitted_text = ""
    events._active_tools = {}
    events._completed_tool_keys = {}
  end)

  it("returns the full buffer when no watermark is supplied", function()
    events.observe_text("alpha")
    events.observe_text("alpha beta")
    expect(#events.filter(nil)).to_equal(2)
    expect(#events.filter(0)).to_equal(2)
  end)

  it("returns only events whose seq exceeds the watermark", function()
    events.observe_text("a")
    events.observe_text("ab")
    events.observe_text("abc")
    local last_seq = events._buffer[2].seq
    local filtered = events.filter(last_seq)
    expect(#filtered).to_equal(1)
    expect(filtered[1].seq > last_seq).to_be_truthy()
  end)

  it("yields an isolated copy of the buffer", function()
    events.observe_text("alpha")
    local snapshot = events.filter(nil)
    table.remove(snapshot, 1)
    -- Mutating the returned slice must not bleed into the emitter's
    -- internal buffer.
    expect(#events._buffer).to_equal(1)
  end)
end)

describe("events.observe_turn_start", function()
  before_each(function()
    events._next_seq = 1
    events._buffer = {}
    events._emitted_text = ""
    events._active_tools = {}
    events._completed_tool_keys = {}
    events._turn_complete_emitted = true
    events._error_signature = "stale"
    events._last_turn_start = nil
  end)

  it("resets per-turn state when turn_start advances", function()
    expect(events.observe_turn_start(100)).to_be_truthy()
    expect(events._turn_complete_emitted).to_be_falsy()
    expect(events._error_signature).to_be_nil()
    expect(events._emitted_text).to_equal("")
  end)

  it("is idempotent for the same turn_start value", function()
    events.observe_turn_start(100)
    -- Re-emit some state and ensure a repeat call with the same
    -- value does not reset it again.
    events._turn_complete_emitted = true
    expect(events.observe_turn_start(100)).to_be_falsy()
    expect(events._turn_complete_emitted).to_be_truthy()
  end)

  it("ignores nil turn_start (fresh adapter with no marker stamped yet)", function()
    expect(events.observe_turn_start(nil)).to_be_falsy()
  end)
end)

describe("events buffer bounds", function()
  before_each(function()
    events._next_seq = 1
    events._buffer = {}
    events._emitted_text = ""
  end)

  it("caps the buffer to the MAX_BUFFER size", function()
    -- 1100 distinct deltas exceeds the 1024 cap; the emitter drops
    -- the oldest entries first so consumers that never advance
    -- their watermark do not accumulate unbounded state.
    local cumulative = ""
    for i = 1, 1100 do
      cumulative = cumulative .. tostring(i) .. ","
      events.observe_text(cumulative)
    end
    expect(events._buffer_size() <= 1024).to_be_truthy()
  end)
end)
