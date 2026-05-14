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

local function has_usage_screen(text)
  return contains(text, "total cost:")
    and contains(text, "usage:")
    and contains_any(text, { "current session", "current week", "total duration" })
end

function M.classify(input)
  local screen = input.screen or ""
  local transcript = input.transcript or ""
  local sequence = input.sequence or 0
  local last_intent = input.last_intent
  local stable_ms = tonumber(input.stable_ms) or 0
  local text = lower(screen .. "\n" .. transcript)
  local screen_text = lower(screen)
  local completed_turn_stable_ms = tonumber(input.completed_turn_stable_ms) or 0

  if trim(text) == "" then
    return state_snapshot(last_intent or "starting", 0.35, "no screen evidence yet", sequence)
  end

  if has_plan_indicator(screen_text) then
    return state_snapshot("waiting_for_plan_approval", 0.8, "plan approval text detected", sequence)
  end

  if has_permission_indicator(screen_text) then
    return state_snapshot("waiting_for_permission", 0.84, "permission or approval prompt text detected", sequence)
  end

  if has_active_work_indicator(screen_text) then
    return state_snapshot("thinking", 0.76, "active work indicator detected", sequence)
  end

  if has_error_indicator(screen) then
    return state_snapshot("error", 0.72, "visible error banner detected", sequence)
  end

  if has_usage_screen(screen_text) and last_intent == "prompt_submitted" and completed_turn_stable_ms > 0 and stable_ms >= completed_turn_stable_ms then
    return state_snapshot("completed_turn", 0.86, "stable usage screen detected", sequence)
  end

  if contains_any(screen_text, { "what would you like", "how can i help", "type a message" }) then
    return state_snapshot("ready", 0.74, "ready prompt text detected", sequence)
  end

  if has_input_prompt(screen) then
    if last_intent == "prompt_submitted" and completed_turn_stable_ms > 0 and stable_ms >= completed_turn_stable_ms and not has_active_work_indicator(screen_text) then
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

function M.cancel(_input)
  return {
    actions = {
      action.interrupt(),
    },
    last_intent = "cancelling",
  }
end

return M
