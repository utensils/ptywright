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

local function has_active_work_indicator(text)
  return contains_any(text, {
    "esc to interrupt",
    "thinking",
    "thinking…",
    "thinking...",
    "running tool",
    "running…",
    "running...",
    "using tool",
    "calling tool",
    "tool use",
    "reading file",
    "editing file",
    "searching",
  })
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
  -- Claude Code's workspace-trust dialog asks "Do you trust the files in
  -- this folder?" with a numbered list (1. Yes, proceed / 2. No, exit).
  -- Require BOTH the question phrasing and at least one numbered-option
  -- string. The earlier "question OR (phrase + option)" form let any
  -- assistant prose containing "do you trust the files" classify as
  -- waiting_for_trust, after which automation might submit `1`+Enter
  -- against the user's actual conversation. Both branches now demand
  -- one of the option strings; that means a model that quotes the
  -- question but does not render the dialog body cannot trigger the
  -- numeric approve action.
  return (contains(text, "do you trust the files") or contains(text, "trust the files"))
    and contains_any(text, {
      "yes, proceed",
      "no, exit",
    })
end

local function has_usage_screen(text)
  return contains(text, "total cost:")
    and contains(text, "usage:")
    and contains_any(text, { "current session", "current week", "total duration" })
end

function M.classify(input)
  local screen = input.screen or ""
  -- body_text excludes the bottom status-bar rows so that benign status
  -- strings like `⏵⏵ bypass permissions on (shift+tab to cycle)` do not
  -- false-positive on substring matches such as `permission`. The host
  -- (src/adapters/claude_code.rs::split_status_bar) computes the split with
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

  -- Plan and permission dialogs in the Claude Code TUI sometimes straddle the
  -- body/status split (the question line is in body, the answer hint sits in
  -- the bottom rows). Look at body+status_text together for those classifier
  -- branches, but keep the false-positive guard for status-bar-only matches
  -- by requiring at least one body match too.
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
    return state_snapshot("waiting_for_user_input", 0.62, "input prompt glyph detected", sequence)
  end

  return state_snapshot(last_intent or "ready", 0.2, "no Claude Code-specific evidence detected", sequence)
end

function M.send_prompt(input)
  return {
    actions = {
      action.paste(input.prompt or ""),
      action.key("enter"),
    },
    last_intent = "prompt_submitted",
  }
end

function M.wait_turn_matcher(input)
  local completed_turn_stable_ms = tonumber(input.completed_turn_stable_ms) or 0
  return matcher.all({
    matcher.any({
      matcher.contains_text("Do you want to proceed"),
      matcher.contains_text("Do you trust the files"),
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

function M.cancel(_input)
  return {
    actions = {
      action.interrupt(),
    },
    last_intent = "cancelling",
  }
end

return M
