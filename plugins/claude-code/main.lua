assert(ptywright, "ptywright host API not installed")
assert(ptywright.action, "ptywright action host API not installed")
assert(ptywright.matcher, "ptywright matcher host API not installed")

local M = {}
local action = ptywright.action
local matcher = ptywright.matcher

local function contains(text, needle)
  return string.find(text, needle, 1, true) ~= nil
end

local function contains_any(text, needles)
  for _, needle in ipairs(needles) do
    if contains(text, needle) then
      return true
    end
  end
  return false
end

local function starts_with(text, prefix)
  return string.sub(text, 1, #prefix) == prefix
end

local function trim(text)
  return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function lower(text)
  return string.lower(text or "")
end

local function state_snapshot(state, confidence, evidence, sequence, metadata)
  return {
    state = state,
    confidence = confidence,
    evidence = evidence,
    sequence = sequence,
    metadata = metadata,
  }
end

-- Strip a single leading "$" so a number prefixed by a currency glyph still
-- parses as a number. Returns the trailing slice, never nil. Cheap to call.
local function strip_dollar(text)
  return (text:gsub("^%$", ""))
end

local function has_error_indicator(screen)
  for line in string.gmatch(screen or "", "[^\n]+") do
    local text = lower(trim(line))
    if starts_with(text, "error:")
      or starts_with(text, "request failed")
      or contains(text, "please retry")
      or contains(text, "press r to retry") then
      return true
    end
  end
  return false
end

local function has_input_prompt(screen)
  for line in string.gmatch(screen or "", "[^\n]+") do
    local text = trim(line)
    if text == ">" or string.sub(text, -2) == " >" or starts_with(text, "❯") then
      return true
    end
  end
  return false
end

-- A line is considered a Claude Code 2.1.x "thinking" indicator if it
-- starts with a known spinner glyph AND ends with the Unicode horizontal
-- ellipsis `…` (U+2026). Claude renders a single status line of the form
-- `<spinner> <Verb>…` while a turn is in flight, where the spinner glyph
-- rotates through a fixed set (`✶`, `✻`, `✺`, `✦`, `·`, `•`, plus the
-- standard braille spinner range `⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏`) and the verb is
-- a randomized whimsy word (Razzle-dazzling, Cogitating, Brewing,
-- Pondering, …). The verb changes per turn and per release, so matching
-- specific words is fragile — but the spinner glyph at line start plus
-- the trailing ellipsis are both stable signals.
--
-- Requiring the spinner anchor (not just ellipsis + short line) is
-- important: assistant prose lines like `One moment…` or `Done…` are
-- short and end with the ellipsis but are NOT thinking indicators, and
-- without the anchor they would keep the classifier stuck on `thinking`
-- after the prompt returned. This is the explicit anchor the Copilot
-- review on PR #21 asked for.
local ELLIPSIS = "…"
local THINKING_LINE_MAX_BYTES = 80
local SPINNER_GLYPHS = {
  "✶", "✻", "✺", "✦", "·", "•",
  "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
}

local function starts_with_spinner_glyph(line)
  for _, glyph in ipairs(SPINNER_GLYPHS) do
    if string.sub(line, 1, #glyph) == glyph then
      return true
    end
  end
  return false
end

local function line_ends_with_ellipsis(line)
  local n = #line
  return n >= #ELLIPSIS and string.sub(line, n - #ELLIPSIS + 1, n) == ELLIPSIS
end

local function has_thinking_spinner_line(text)
  for line in string.gmatch(text or "", "[^\n]+") do
    local trimmed = trim(line)
    if #trimmed > 0 and #trimmed <= THINKING_LINE_MAX_BYTES then
      if starts_with_spinner_glyph(trimmed) and line_ends_with_ellipsis(trimmed) then
        return true
      end
    end
  end
  return false
end

local function has_active_work_indicator(text)
  -- Direct text hints kept across Claude Code releases.
  if contains_any(text, {
    "esc to interrupt",
    "thinking",
    "running tool",
    "using tool",
    "calling tool",
    "tool use",
    "reading file",
    "editing file",
    "searching",
  }) then
    return true
  end
  -- Claude Code 2.1.x spinner-and-ellipsis status line.
  return has_thinking_spinner_line(text)
end

local function has_permission_indicator(text)
  return contains_any(text, {
    "do you want to proceed",
    "permission",
    "allow",
    "approve",
    "yes, and don't ask again",
    "yes, and don’t ask again",
  })
end

-- Parse the permission dialog into `{ permission = { tool?, summary?,
-- options } }`. Claude Code ships two layouts today:
--   numbered list:    "Bash command\n  cmd\nDo you want to proceed?\n
--                       ❯ 1. Yes\n  2. Yes, and don't ask again\n  3. No"
--   Enter/Esc style:  "Allow Bash command: cmd\n[Enter] Approve  [Esc] Deny"
-- Both forms expose the tool name (the word before "command" / "tool"),
-- the command summary text, and the explicit options the user can pick.
-- Lossy vs. `--permission-prompt-tool stdio`'s structured control_request
-- — the dialog has no JSON payload here — but enough to apply simple
-- policy gates from a calling consumer. Returns nil when nothing parsed.
local function parse_permission_dialog(text)
  if text == nil or text == "" then
    return nil
  end
  local permission = {}
  local tool = text:match("([%w%-]+) command") or text:match("([%w%-]+) tool")
  if tool then
    permission.tool = tool
  end
  local summary = text:match("[Aa]llow [%w%-]+ command:%s*([^\n]+)")
    or text:match("[Aa]llow [%w%-]+ tool:%s*([^\n]+)")
    or text:match("[%w%-]+ command\n%s*([^\n]+)")
    or text:match("[%w%-]+ tool\n%s*([^\n]+)")
  if summary then
    summary = trim(summary)
    -- Reject summaries that are actually the next dialog line
    -- ("Do you want to proceed?") rather than the command body. Those
    -- show up when the multi-line layout has no body text — the parser
    -- shouldn't surface the prompt as the command summary.
    if summary ~= "" and not contains(lower(summary), "do you want to proceed") then
      permission.summary = summary
    end
  end
  local options = {}
  -- Lua patterns are byte-oriented, so the multi-byte `❯` selector glyph
  -- can't sit in a `[...]` class. We instead anchor on "first digit
  -- followed by '. '" anywhere in the line and let the leading bytes be
  -- whatever they are. Reliable across both numbered-list variants Claude
  -- Code ships (`❯ 1. Yes` and `  2. Yes, and don't ask again`).
  for line in string.gmatch(text, "[^\n]+") do
    local body = line:match("%d+%.%s+(.+)$")
    if body then
      table.insert(options, trim(body))
    end
  end
  if #options == 0 then
    -- Enter/Esc bracketed layout.
    if text:match("%[Enter%]%s*[Aa]pprove") or text:match("%[Enter%]%s*[Aa]llow") then
      table.insert(options, "Approve")
    end
    if text:match("%[Esc%]%s*[Dd]eny") or text:match("%[Esc%]%s*[Dd]ecline") then
      table.insert(options, "Deny")
    end
  end
  if #options > 0 then
    permission.options = options
  end
  if next(permission) == nil then
    return nil
  end
  return { permission = permission }
end

local function has_plan_indicator(text)
  return contains(text, "plan") and contains_any(text, {
    "approve",
    "accept",
    "proceed",
    "looks good",
  })
end

local function has_trust_indicator(text)
  -- Claude Code's workspace-trust dialog has shipped two phrasings:
  --   pre-2.1: "Do you trust the files in this folder?"
  --            with "1. Yes, proceed / 2. No, exit"
  --   2.1.x:  "Accessing workspace: <path>\nIs this a project you created
  --            or one you trust?" with "1. Yes, I trust this folder / 2. No, exit"
  -- Both forms keep "no, exit" as the second option so we anchor on that
  -- to confirm the numbered list is actually rendered, while accepting
  -- either question phrasing on the body side. Both branches still demand
  -- one of the option strings; that means a model that quotes the
  -- question but does not render the dialog body cannot trigger the
  -- numeric approve action.
  local has_question = contains(text, "do you trust the files")
    or contains(text, "trust the files")
    or contains(text, "trust this folder")
    or contains(text, "accessing workspace")
  return has_question
    and contains_any(text, {
      "yes, proceed",
      "yes, i trust this folder",
      "no, exit",
    })
end

local function has_usage_screen(text)
  return contains(text, "total cost:")
    and contains(text, "usage:")
    and contains_any(text, { "current session", "current week", "total duration" })
end

-- Parse the "Total cost: $0.0000" / "Usage: 0 input, 0 output, ..." panel
-- into the structured `usage` table the host exposes on
-- `ExtensionStateSnapshot::metadata`. Every field is best-effort: keys whose
-- patterns didn't match are simply absent, so a future TUI tweak that drops
-- (say) "Total code changes" just leaves the field out instead of breaking
-- classification. Caller pre-confirms the screen is visible via
-- `has_usage_screen` — no redundant guard here because that detector runs
-- against lower-cased text while these patterns must see the original case
-- to extract literal currency/number tokens.
local function parse_usage_screen(text)
  local usage = {}
  local function maybe_set(key, value)
    if value then
      usage[key] = value
    end
  end
  -- "Total cost: $0.0000" — strip the dollar before tonumber so it works
  -- whether the TUI renders the glyph or not.
  local cost = text:match("[Tt]otal cost:%s*([%$%-%.%d]+)")
  if cost then
    maybe_set("cost_usd", tonumber(strip_dollar(cost)))
  end
  -- "Total duration (API): 0s" / "Total duration (wall): 4s".
  local api_dur = text:match("[Tt]otal duration %(API%):%s*([%-%.%d]+)%s*s")
  maybe_set("api_duration_s", tonumber(api_dur))
  local wall_dur = text:match("[Tt]otal duration %(wall%):%s*([%-%.%d]+)%s*s")
  maybe_set("wall_duration_s", tonumber(wall_dur))
  -- "Usage: 0 input, 0 output, 0 cache read, 0 cache write".
  local input_tok, output_tok, cache_read, cache_write = text:match(
    "[Uu]sage:%s*(%d+)%s*input,%s*(%d+)%s*output,%s*(%d+)%s*cache read,%s*(%d+)%s*cache write"
  )
  maybe_set("input_tokens", tonumber(input_tok))
  maybe_set("output_tokens", tonumber(output_tok))
  maybe_set("cache_read_tokens", tonumber(cache_read))
  maybe_set("cache_write_tokens", tonumber(cache_write))
  -- "Total code changes: 0 lines added, 0 lines removed".
  local added, removed = text:match("[Tt]otal code changes:%s*(%d+)%s*lines added,%s*(%d+)%s*lines removed")
  maybe_set("lines_added", tonumber(added))
  maybe_set("lines_removed", tonumber(removed))
  return { usage = usage }
end

-- Parse the bottom status-bar text into the structured `status` table the
-- host exposes on `ExtensionStateSnapshot::metadata`. Claude Code renders
-- the active model inside square brackets (`[Haiku 4.5]`) plus a
-- permission-mode hint (`⏵⏵ bypass permissions on`, `⏵⏵ auto mode on`,
-- `⏵⏵ plan mode on`, …). Every field is best-effort; the host omits keys
-- that didn't match. Returns nil only when no fields parsed at all so
-- callers don't have to special-case an empty table.
local function parse_status_bar(text)
  if text == nil or text == "" then
    return nil
  end
  local status = {}
  local model = text:match("%[([^%]]+)%]")
  if model then
    status.model = trim(model)
  end
  local lowered = lower(text)
  if contains(lowered, "bypass permissions on") then
    status.permission_mode = "bypass"
  elseif contains(lowered, "auto mode on") then
    status.permission_mode = "auto"
  elseif contains(lowered, "plan mode on") then
    status.permission_mode = "plan"
  end
  if next(status) == nil then
    return nil
  end
  return { status = status }
end

-- Merge two metadata tables (either may be nil). Plain shallow merge —
-- callers structure their metadata under disjoint top-level keys (`usage`,
-- `status`, `permission`, …) so a deep merge is unnecessary.
local function merge_metadata(a, b)
  if a == nil then
    return b
  end
  if b == nil then
    return a
  end
  for key, value in pairs(b) do
    a[key] = value
  end
  return a
end

local function has_welcome_screen(text)
  -- Claude Code's first-launch welcome panel renders after the workspace
  -- trust dialog is accepted. It shows "Welcome back <user>!" alongside a
  -- "Tips for getting started" / "What's new" pane and traps the first
  -- Enter keypress (dismissing the welcome rather than submitting the
  -- caller's prompt). Detect it so the classifier reports `starting`
  -- instead of `waiting_for_user_input` — Claude is not actually ready to
  -- accept a prompt yet, even though the input cursor `❯` is on screen.
  return contains(text, "tips for getting started")
    and contains(text, "what's new")
    and contains_any(text, { "welcome back", "claude code v" })
end

function M.classify(input)
  local screen = input.screen or ""
  -- body_text excludes the bottom status-bar rows so that benign status
  -- strings like `⏵⏵ bypass permissions on (shift+tab to cycle)` do not
  -- false-positive on substring matches such as `permission`. The host
  -- (src/extension.rs::split_status_bar) computes the split with
  -- STATUS_BAR_ROWS; if for any reason the host doesn't provide the split
  -- (older callers, tests) fall back to the full screen so we degrade
  -- gracefully rather than misclassifying everything as starting.
  local body = input.body_text or screen
  local status = input.status_text or ""
  local transcript = input.transcript or ""
  local sequence = input.sequence or 0
  local last_intent = input.last_intent
  local stable_ms = tonumber(input.stable_ms) or 0
  local text = lower(body .. "\n" .. transcript)
  local body_text = lower(body)
  local screen_text = lower(screen)
  local completed_turn_stable_ms = tonumber(input.completed_turn_stable_ms) or 0

  -- Parse the bottom status-bar text once and merge it into every branch's
  -- return. Shadowing the module-level `state_snapshot` keeps each branch
  -- one-liner without having to thread `status_metadata` through 13 call
  -- sites. Branches that already attach their own metadata (the
  -- `completed_turn` usage screen) get a shallow merge with status on top.
  local outer_state_snapshot = state_snapshot
  local status_metadata = parse_status_bar(status)
  local function state_snapshot(state, confidence, evidence, seq, metadata)
    return outer_state_snapshot(
      state,
      confidence,
      evidence,
      seq,
      merge_metadata(status_metadata, metadata)
    )
  end

  if trim(text) == "" then
    return state_snapshot(last_intent or "starting", 0.35, "no screen evidence yet", sequence)
  end

  -- Recently-sent cancel: report `cancelling` until the screen has been
  -- quiet for at least the configured stability window. After that the
  -- classifier falls through to the regular branches and reports
  -- whatever the post-cancel screen actually shows (usually back at the
  -- idle prompt, sometimes a completed-turn summary if the interrupt
  -- arrived right at a boundary). Without this branch the only signal
  -- that cancel landed is the disappearance of the thinking spinner,
  -- which is brittle to observe from a polling caller.
  --
  -- Note for callers: `adapter.state` polling does not supply
  -- `stable_ms`, so this branch will fire on every state read until the
  -- next mutating intent clears `last_intent`. To observe the transition
  -- out of cancelling, call `adapter.wait` after `cancel`; the wait
  -- matcher returns once the screen has been stable for the configured
  -- threshold and then this branch falls through to the normal idle /
  -- completed-turn classification.
  if last_intent == "cancelling" and stable_ms < completed_turn_stable_ms then
    return state_snapshot(
      "cancelling",
      0.72,
      "cancel intent recently applied; screen not yet stable",
      sequence
    )
  end

  -- Plan-approval dialogs in the Claude Code TUI sometimes straddle the
  -- body/status split: the "Plan ready"/"Plan" header sits in the body
  -- while the approve/accept/proceed hint can render in the bottom rows.
  -- The plan branch below uses body_and_status for that reason and keeps
  -- the false-positive guard by requiring "plan" to appear in body first.
  -- Permission dialogs do not straddle the split in practice (the entire
  -- dialog renders in the body area), so the permission branch stays on
  -- body_text alone — matching it across the status bar would re-introduce
  -- the `bypass permissions on` false-positive we fixed in Milestone 21.4.
  local body_and_status = body_text .. "\n" .. lower(status)

  -- Workspace-trust dialog is checked before permission/plan because its
  -- approve action differs (numbered selection, not a single Enter press).
  -- Trust questions and answers live entirely in the dialog body; the
  -- status bar carries only navigation hints.
  if has_trust_indicator(body_text) then
    return state_snapshot("waiting_for_trust", 0.86, "workspace trust dialog detected", sequence)
  end

  if has_plan_indicator(body_text) or (contains(body_text, "plan") and has_plan_indicator(body_and_status)) then
    return state_snapshot("waiting_for_plan_approval", 0.8, "plan approval text detected", sequence)
  end

  if has_permission_indicator(body_text) then
    -- Parse against the unlowered *full screen* — the dialog tool/summary
    -- live in the body but the numbered options can land in the bottom
    -- rows that `body_text` strips, and we want all three fields when
    -- present. Original case matters for the tool name and summary
    -- ("Bash command: cargo test").
    local permission_metadata = parse_permission_dialog(screen)
    return state_snapshot("waiting_for_permission", 0.84, "permission or approval prompt text detected", sequence, permission_metadata)
  end

  if has_active_work_indicator(body_text) then
    return state_snapshot("thinking", 0.76, "active work indicator detected", sequence)
  end

  if has_error_indicator(body) then
    return state_snapshot("error", 0.72, "visible error banner detected", sequence)
  end

  if has_usage_screen(body_text) and last_intent == "prompt_submitted" and completed_turn_stable_ms > 0 and stable_ms >= completed_turn_stable_ms then
    -- Pull the cost / tokens / duration out of the rendered usage panel
    -- so callers get a machine-readable view alongside the state. Parsing
    -- the unlowered body avoids losing currency casing if Claude ever
    -- ships a tweak that depends on it; `parse_usage_screen` lower-cases
    -- only the bits it asserts against.
    local usage_metadata = parse_usage_screen(body)
    return state_snapshot("completed_turn", 0.86, "stable usage screen detected", sequence, usage_metadata)
  end

  if contains_any(body_text, { "what would you like", "how can i help", "type a message" }) then
    return state_snapshot("ready", 0.74, "ready prompt text detected", sequence)
  end

  if has_input_prompt(screen) then
    if last_intent == "prompt_submitted" and completed_turn_stable_ms > 0 and stable_ms >= completed_turn_stable_ms and not has_active_work_indicator(body_text) then
      return state_snapshot("completed_turn", 0.78, "stable input prompt after prompt submission", sequence)
    end
    -- The welcome panel renders an input cursor even though Claude won't
    -- treat the first Enter as a prompt submission. Report `starting` so
    -- callers don't race-send before `dismiss_welcome` is invoked. This
    -- check runs *after* the completed_turn branch so a real turn boundary
    -- with welcome chrome still visible (transient just after start) isn't
    -- demoted back to starting — in practice the welcome chrome is gone
    -- long before a turn completes, so the ordering here is conservative.
    if has_welcome_screen(body_text) then
      return state_snapshot("starting", 0.7, "welcome screen visible", sequence)
    end
    return state_snapshot("waiting_for_user_input", 0.62, "input prompt glyph detected", sequence)
  end

  return state_snapshot(last_intent or "ready", 0.2, "no Claude Code-specific evidence detected", sequence)
end

function M.send_prompt(input)
  -- Use bracketed paste explicitly. Claude Code v2.1+ enables bracketed
  -- paste mode (`CSI ? 2004 h`) for its input box, so the wrapper lets
  -- the trailing Enter submit cleanly. Without the wrapper a long
  -- prompt races against Claude's input tokeniser: the bytes land but
  -- the Enter gets absorbed and the prompt sits un-submitted. The
  -- generic `action.paste(...)` still exists for callers / plugins
  -- driving programs that have not opted into bracketed paste.
  return {
    actions = {
      action.bracketed_paste(input.prompt or ""),
      action.key("enter"),
    },
    last_intent = "prompt_submitted",
  }
end

function M.wait_turn_matcher(input)
  local completed_turn_stable_ms = tonumber(input.completed_turn_stable_ms) or 0
  -- Boundary anchors: any single one of these is enough to wake the wait.
  -- Keep this list in sync with the classifier — every screen the
  -- classifier reports as anything other than `starting` / `thinking`
  -- needs at least one anchor here, or `adapter.wait` will time out on
  -- it. The v2 trust strings (`Accessing workspace`, `Yes, I trust
  -- this folder`) are mirrored from `has_trust_indicator` so callers
  -- waiting after `adapter.start` against an untrusted directory wake
  -- on either the pre-2.1 or 2.1.x dialog.
  return matcher.all({
    matcher.any({
      matcher.contains_text("Do you want to proceed"),
      matcher.contains_text("Do you trust the files"),
      matcher.contains_text("Accessing workspace"),
      matcher.contains_text("Yes, I trust this folder"),
      matcher.contains_text("Approve"),
      matcher.contains_text("Allow"),
      matcher.contains_text("Total cost:"),
      matcher.screen_regex("(?m)^\\s*(?:>|❯)\\s*$"),
    }),
    matcher.screen_stable(completed_turn_stable_ms),
  })
end

function M.approve(_input)
  return {
    actions = {
      action.key("enter"),
    },
  }
end

function M.deny(_input)
  return {
    actions = {
      action.key("escape"),
    },
  }
end

-- Trust-dialog approval needs a numbered selection (1 = Yes, proceed)
-- followed by Enter, since the TUI does not treat a bare Enter on the
-- list as accepting option 1. Kept as a separate intent so callers can
-- dispatch on `waiting_for_trust` explicitly instead of overloading
-- `approve`.
function M.approve_trust(_input)
  return {
    actions = {
      action.text("1"),
      action.key("enter"),
    },
  }
end

function M.deny_trust(_input)
  return {
    actions = {
      action.text("2"),
      action.key("enter"),
    },
  }
end

-- Dismiss Claude Code's first-launch welcome panel. The panel appears
-- after the workspace trust dialog is accepted and traps the first
-- Enter keypress (the panel disappears rather than the caller's prompt
-- submitting). Send a bare Enter to clear it before `send_prompt`.
function M.dismiss_welcome(_input)
  return {
    actions = {
      action.key("enter"),
    },
  }
end

function M.cancel(_input)
  return {
    actions = {
      action.interrupt(),
    },
    last_intent = "cancelling",
  }
end

-- Generic key/text intent. The REPL's `send.key("…")` and any other
-- caller that wants to nudge the PTY with a single named key or a short
-- text token can use this without knowing whether the target is a
-- recognised key alias or a literal character. Hyphenated names
-- (`"ctrl-c"`, `"shift-tab"`, `"page-up"`) are normalised to the
-- underscore form the host's `Key` serde enum expects.
--
-- Keep this table in sync with `src/action.rs::Key`. Anything missing
-- here falls through to `action.text`, which means the caller would see
-- a literal character or short string typed into the PTY instead of an
-- escape sequence — almost always a bug rather than the intent.
local KEY_ALIASES = {
  -- Submission / line editing
  enter = true, escape = true, tab = true, shift_tab = true,
  backspace = true, delete = true, space = true,
  -- Arrows
  up = true, down = true, left = true, right = true,
  -- Navigation cluster
  home = true, ["end"] = true, page_up = true, page_down = true,
  insert = true,
  -- Ctrl combos (ctrl_h / ctrl_i / ctrl_j / ctrl_m are intentionally
  -- absent — use backspace / tab / enter so transcripts stay readable)
  ctrl_a = true, ctrl_b = true, ctrl_c = true, ctrl_d = true,
  ctrl_e = true, ctrl_f = true, ctrl_g = true,
  ctrl_k = true, ctrl_l = true, ctrl_n = true, ctrl_o = true,
  ctrl_p = true, ctrl_q = true, ctrl_r = true, ctrl_s = true,
  ctrl_t = true, ctrl_u = true, ctrl_v = true, ctrl_w = true,
  ctrl_x = true, ctrl_y = true, ctrl_z = true,
  -- Function keys
  f1 = true, f2 = true, f3 = true, f4 = true,
  f5 = true, f6 = true, f7 = true, f8 = true,
  f9 = true, f10 = true, f11 = true, f12 = true,
}

function M.key(input)
  local raw = (input and input.key) or ""
  local normalised = raw:gsub("-", "_")
  if KEY_ALIASES[normalised] then
    return {
      actions = {
        action.key(normalised),
      },
    }
  end
  -- Fall through: arbitrary single characters or short strings ("y",
  -- "n", "1", "q") are sent as raw text so the PTY treats them as
  -- typed input.
  return {
    actions = {
      action.text(raw),
    },
  }
end

return M
