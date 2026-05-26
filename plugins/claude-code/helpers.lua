-- Generic Lua string utilities used by every layer of the claude-code
-- plugin. Nothing in here is application-specific — these are plain
-- text operations so the structural indicators / parsers / classifier
-- in `main.lua` can stay focused on Claude Code's specific TUI shapes.
--
-- Pre-loaded as a Lua global by the host (`BUILTIN_PLUGINS.modules` in
-- `src/plugin.rs`), so `main.lua` reads it via `helpers.<fn>` without
-- a `require` call.

local M = {}

function M.contains(text, needle)
  return string.find(text, needle, 1, true) ~= nil
end

function M.contains_any(text, needles)
  for _, needle in ipairs(needles) do
    if M.contains(text, needle) then
      return true
    end
  end
  return false
end

function M.starts_with(text, prefix)
  return string.sub(text, 1, #prefix) == prefix
end

function M.trim(text)
  return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

function M.lower(text)
  return string.lower(text or "")
end

-- Best-effort plugin-side redaction for text we expose through
-- `ExtensionStateSnapshot.metadata` (permission dialog summaries today).
-- The host applies `RedactionPolicy::default()` to screen/transcript
-- reads but state-shaped responses (adapter.state / send / wait) don't
-- go through that filter, so we scrub here. Mirror the most common
-- Rust-side patterns; document on the wire that callers handling
-- secrets must apply their own redaction layer for guarantees beyond
-- best-effort.
local SECRET_PATTERNS = {
  -- `token=value`, `password=value`, `api_key=value` — keep the prefix.
  "([Tt]oken%s*=%s*)([^%s\"']+)",
  "([Pp]assword%s*=%s*)([^%s\"']+)",
  "([Aa]pi[_-]?[Kk]ey%s*=%s*)([^%s\"']+)",
  "([Ss]ecret%s*=%s*)([^%s\"']+)",
  -- Anthropic / OpenAI style API key prefixes.
  "()(sk%-[%w%-_]+)",
  "()(sk%-ant%-[%w%-_]+)",
  -- AWS access key id.
  "()(AKIA[%w]+)",
}

function M.redact_secret_patterns(text)
  if text == nil or text == "" then
    return text
  end
  for _, pattern in ipairs(SECRET_PATTERNS) do
    text = (text:gsub(pattern, function(prefix, _value)
      -- Lua's `()` empty-capture returns the match POSITION (a number),
      -- not an empty string. The two-capture patterns above use `()` as
      -- a sentinel meaning "no key= prefix to preserve" — treat any
      -- non-string prefix as the redact-the-whole-match path so the
      -- prefix-less patterns (sk-…, sk-ant-…, AKIA…) don't accidentally
      -- prepend the position digit to the replacement.
      if prefix == nil or prefix == "" or type(prefix) ~= "string" then
        return "[REDACTED]"
      end
      return prefix .. "[REDACTED]"
    end))
  end
  return text
end

-- Strip a single leading "$" so a number prefixed by a currency glyph
-- still parses as a number. Returns the trailing slice, never nil.
function M.strip_dollar(text)
  return (text:gsub("^%$", ""))
end

-- FNV-1a 32-bit hash used to mint stable dialog correlation ids. Lua
-- 5.4's bitwise operators keep this pure-Lua. Returns 8 lowercase hex
-- chars. Used by the dialog parsers in main.lua to seed
-- `metadata.dialog_id` from the dialog body fingerprint — stable
-- across reclassifications of the same dialog, distinct between
-- dialogs whose content differs.
function M.fnv1a_hex(s)
  if s == nil or s == "" then
    return "00000000"
  end
  local h = 2166136261
  for i = 1, #s do
    h = h ~ string.byte(s, i)
    h = (h * 16777619) & 0xffffffff
  end
  return string.format("%08x", h)
end

return M
