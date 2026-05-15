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

local function state_snapshot(state, confidence, evidence, sequence)
  return {
    state = state,
    confidence = confidence,
    evidence = evidence,
    sequence = sequence,
  }
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
    return state_snapshot("waiting_for_permission", 0.84, "permission or approval prompt text detected", sequence)
  end

  if has_active_work_indicator(body_text) then
    return state_snapshot("thinking", 0.76, "active work indicator detected", sequence)
  end

  if has_error_indicator(body) then
    return state_snapshot("error", 0.72, "visible error banner detected", sequence)
  end

  if has_usage_screen(body_text) and last_intent == "prompt_submitted" and completed_turn_stable_ms > 0 and stable_ms >= completed_turn_stable_ms then
    return state_snapshot("completed_turn", 0.86, "stable usage screen detected", sequence)
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

return M
