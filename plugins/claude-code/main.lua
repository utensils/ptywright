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

function M.classify(input)
  local screen = input.screen or ""
  local transcript = input.transcript or ""
  local sequence = input.sequence or 0
  local last_intent = input.last_intent
  local combined = screen .. "\n" .. transcript
  local text = lower(combined)

  if trim(text) == "" then
    return state_snapshot(last_intent or "starting", 0.35, "no screen evidence yet", sequence)
  end

  if contains(text, "plan") and contains_any(text, { "approve", "accept", "proceed" }) then
    return state_snapshot("waiting_for_plan_approval", 0.78, "plan approval text detected", sequence)
  end

  if contains_any(text, { "do you want to proceed", "permission", "allow", "approve" }) then
    return state_snapshot("waiting_for_permission", 0.82, "permission or approval prompt text detected", sequence)
  end

  if contains_any(text, { "esc to interrupt", "thinking", "thinking…", "thinking..." }) then
    return state_snapshot("thinking", 0.72, "thinking indicator detected", sequence)
  end

  if has_error_indicator(screen) then
    return state_snapshot("error", 0.72, "visible error banner detected", sequence)
  end

  if contains_any(text, { "what would you like", "how can i help", "type a message" }) then
    return state_snapshot("ready", 0.74, "ready prompt text detected", sequence)
  end

  if has_input_prompt(screen) then
    local state = "waiting_for_user_input"
    if last_intent == "prompt_submitted" then
      state = "completed_turn"
    end
    return state_snapshot(state, 0.62, "input prompt glyph detected", sequence)
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

function M.wait_turn_matcher(_input)
  return matcher.any({
    matcher.contains_text("Do you want to proceed"),
    matcher.contains_text("Approve"),
    matcher.contains_text("Allow"),
    matcher.screen_regex("(?m)^\\s*(?:>|❯)\\s*$"),
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
