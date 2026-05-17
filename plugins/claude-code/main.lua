-- File layout (with `helpers.lua` providing shared utilities):
--
--   helpers.lua            — generic string utilities + secret scrubbing
--                            (pre-loaded as a Lua global by the host)
--   main.lua (this file)
--     ├─ host bindings       — ptywright.action / ptywright.matcher
--     ├─ classifier helpers  — state_snapshot
--     ├─ structural indicators — has_*_indicator, has_*_screen, …
--     ├─ structural parsers    — parse_status_bar / parse_usage_screen / …
--     ├─ classify(input)       — the screen-state classifier
--     ├─ intent plans          — send_prompt, cancel, approve, … (M.*)
--     ├─ matcher builders      — wait_*_matcher (M.*)
--     └─ key-alias router      — M.key
--
-- When a section in this file grows past roughly 200 lines, extract it
-- into its own module file alongside `helpers.lua`: add a `(name,
-- source)` pair to `BUILTIN_PLUGINS.modules` in `src/plugin.rs`, write
-- `<name>.lua` returning a module table, and reference its functions
-- here as `local <fn> = <name>.<fn>` (the host loader pre-registers
-- each module as a Lua global before main.lua runs). Tests under
-- `tests/lua_plugin_intents.rs` and the fixture matrix at
-- `tests/lua_classifier_tests.rs` exercise the public surface either
-- way — module splits are intentionally transparent to callers.

assert(ptywright, "ptywright host API not installed")
assert(ptywright.action, "ptywright action host API not installed")
assert(ptywright.matcher, "ptywright matcher host API not installed")
assert(helpers, "helpers module not pre-loaded (see BUILTIN_PLUGINS in src/plugin.rs)")

local M = {}
local action = ptywright.action
local matcher = ptywright.matcher

-- Bring the shared helpers in as locals so the rest of this file reads
-- identically to the pre-split version. Anything new that needs a
-- helper but isn't aliased here can still call `helpers.<fn>` directly.
local contains               = helpers.contains
local contains_any           = helpers.contains_any
local starts_with            = helpers.starts_with
local trim                   = helpers.trim
local lower                  = helpers.lower
local redact_secret_patterns = helpers.redact_secret_patterns
local strip_dollar           = helpers.strip_dollar

local function state_snapshot(state, confidence, evidence, sequence, metadata)
  return {
    state = state,
    confidence = confidence,
    evidence = evidence,
    sequence = sequence,
    metadata = metadata,
  }
end

-- Banner phrases that the TUI renders as their OWN line at the start
-- of the row (no leading prose context). Anchoring with `starts_with`
-- ensures assistant prose that quotes these phrases inside a sentence
-- doesn't trip the detector — e.g. an answer that says `Note that
-- "you've reached your usage limit" can be reset by …` should NOT
-- classify as `error`.
local ERROR_BANNER_PREFIXES = {
  "error:",
  "request failed",
  "api error",
  -- Rate-limit / plan-limit banners. Claude Code 2.1.x renders one of
  -- these as a standalone body row when you exhaust your plan budget.
  "5-hour limit",
  "rate limit reached",
  "you've used your pro plan",
  "you've used your max plan",
  "you've reached your usage limit",
  "credit balance is too low",
  -- Connection / network failure banner.
  "connection error",
  "connection issue",
  "could not connect",
  "network error",
}

local function has_error_indicator(screen)
  for line in string.gmatch(screen or "", "[^\n]+") do
    local text = lower(trim(line))
    for _, prefix in ipairs(ERROR_BANNER_PREFIXES) do
      if starts_with(text, prefix) then
        return true
      end
    end
    -- Retry hints — these appear on their own line after a banner,
    -- typically as just the action verb. Still anchored at line
    -- start because Claude doesn't write "Press r to retry" inside
    -- flowing answer prose.
    if starts_with(text, "please retry") or starts_with(text, "press r to retry") then
      return true
    end
  end
  return false
end

-- Returns true if any line in `screen` is an input-prompt row.
-- Recognised shapes (left-anchored unless noted):
--   * Bare prompt glyph (`>` or `❯`), optionally followed by ASCII
--     whitespace OR NBSP (U+00A0) padding OR user/ghost text.
--   * Line ENDING in ` >` — legacy shell-style prompt position where
--     `text >` is the input cursor (e.g. some older Claude builds).
-- The `❯` glyph case stays anchored at line start because the modern
-- Claude Code 2.1.x TUI always renders it as the first non-space
-- char on the input row. The legacy ASCII `>` had to be supported
-- at line end too; we keep that side intact so old fixtures still
-- match while symmetrically also accepting `>` at line start.
-- `trim` strips ASCII whitespace only, so NBSP padding is handled
-- explicitly with byte-level checks on the multi-byte sequence.
local NBSP_BYTES = "\194\160"  -- U+00A0 as UTF-8

local function has_input_prompt(screen)
  for line in string.gmatch(screen or "", "[^\n]+") do
    local text = trim(line)
    if text == ">" or text == "❯" then return true end
    if string.sub(text, -2) == " >" then return true end
    if starts_with(text, "❯") then return true end
    -- Legacy ASCII `>` at line start, with either ASCII space or
    -- NBSP between the glyph and any following text. Restores the
    -- behaviour `has_empty_input_prompt_line` had explicitly for
    -- NBSP-padded `>` rows on terminals that render the ASCII
    -- prompt fallback.
    if starts_with(text, "> ") then return true end
    if starts_with(text, ">" .. NBSP_BYTES) then return true end
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

local TURN_COMPLETION_GLYPH = "✻"

-- Lines we expect to find AFTER a real end-of-turn `✻ <Verb> for <N>`
-- marker on a settled screen:
--   * blank lines,
--   * the empty input prompt (`❯` / `>` alone),
--   * the prompt with a post-turn suggested follow-up (`❯ run the
--     tests`), which Claude Code 2.1.x renders as ghost text after a
--     completed turn,
--   * horizontal rules,
--   * the status-bar rows (`user @ host …`, `⏵⏵ … on …`).
-- Anything else (tool-progress rows like `⏺ Reading 1 file…`, answer
-- bullets, search results, `⎿` tool output) means we're looking at an
-- intermediate render or a still-in-flight turn, not the actual end
-- boundary. The submitted prompt echo (`❯ Read all files…`) renders
-- ABOVE the answer/marker in Claude Code's layout, never below, so
-- accepting `❯ <text>` here cannot let a mid-turn marker match by
-- mistaking the submitted prompt for trailing chrome.
local function is_post_marker_trailing_line(line)
  local t = trim(line)
  if t == "" then return true end
  if t == ">" or t == "❯" then return true end
  -- Prompt with text — post-turn ghost-text suggestion.
  if starts_with(t, "❯ ") or starts_with(t, "> ") then return true end
  -- Same with NBSP (U+00A0, bytes 0xC2 0xA0) — Claude Code pads ghost
  -- text with a non-breaking space rather than a regular space.
  if starts_with(t, "❯\194\160") or starts_with(t, ">\194\160") then return true end
  -- Horizontal rule — single repeated box-drawing character.
  if t:match("^[─━═]+$") then return true end
  -- Status-bar rows Claude Code 2.1.x renders below the prompt. The
  -- real TUI fills these with the actual `<username> @ <host>` and a
  -- `[<Model> <Version>]` suffix, e.g.
  --   `james @ laptop /Users/james/Projects                  [Sonnet 4.6]`
  --   `⏵⏵ auto mode on (shift+tab to cycle) · ← for agents`
  -- Recognise both structural shapes generically — never anchor on
  -- the fixture's sanitized literal `user ` prefix, which would leave
  -- real users with a marker followed by their own status bar stuck
  -- in `thinking`:
  --   * `⏵⏵ … on …` — permission-mode hint glyph.
  --   * `[<word(s)> <digit>.<digit>]` — model bracket with version.
  --   * `<token> @ <token>` followed by a `[<word> <digit>]` later
  --     on the same line (the user@host + model layout).
  if t:find("⏵⏵") then return true end
  if t:find("%[%u%a-%s%d+%.%d+%]") then return true end
  if t:find("%S+%s*@%s*%S+") and t:find("%[") and t:find("%]") then return true end
  return false
end

local function has_turn_completion_marker(text)
  -- Claude Code 2.1.x renders a "tea verb" completion line at the very
  -- end of every turn — e.g. "✻ Brewed for 1s", "✻ Worked for 5s",
  -- "✻ Heated for 12s". The verb rotates from a fixed set; what's stable
  -- across versions is the leading `✻` glyph plus a "for <duration>"
  -- tail.
  --
  -- The spinner cycle ALSO uses `✻` via "✻ Working…" etc., so the
  -- glyph alone isn't enough. Spinner lines end with the ellipsis;
  -- completion lines end with a numeric duration (` for %d`). That tail
  -- difference is necessary but not sufficient: parser-captured
  -- intermediate renders can occasionally show a `✻ <verb> for <N>`
  -- frame mid-turn (between spinner repaints, when the counter-only
  -- variant happens to land in the snapshot). To rule that out we
  -- additionally require the marker to be STRUCTURALLY TERMINAL — no
  -- content lines after it, just blank rows / the input prompt /
  -- horizontal rules / the status bar. End-of-turn screens satisfy
  -- this; mid-turn screens have tool-progress rows (`⏺ Reading…`,
  -- `⎿ result`, etc.) below whatever flickered into view.
  local lines = {}
  for line in string.gmatch(text or "", "[^\n]+") do
    table.insert(lines, line)
  end
  -- Walk lines in reverse so we find the LAST marker on screen; that's
  -- the candidate end-of-turn boundary.
  for i = #lines, 1, -1 do
    local trimmed = trim(lines[i])
    if string.sub(trimmed, 1, #TURN_COMPLETION_GLYPH) == TURN_COMPLETION_GLYPH then
      if not line_ends_with_ellipsis(trimmed) and trimmed:find(" for %d") then
        -- Structural-terminal check: everything after this row must be
        -- trailing chrome (blank, prompt, rule, status bar).
        local terminal = true
        for j = i + 1, #lines do
          if not is_post_marker_trailing_line(lines[j]) then
            terminal = false
            break
          end
        end
        if terminal then return true end
      end
    end
  end
  return false
end

-- Returns true if any line in `text` is a Claude Code 2.1.x
-- collapsible tool-progress row: starts with `⏺`, contains
-- `(ctrl+o to expand)` (the collapse hint the TUI puts at the end
-- of every collapsible row), and ends with that hint. Anchoring
-- both the leading glyph AND the trailing hint position keeps
-- assistant prose that merely quotes the hint phrase from tripping
-- the detector — Claude can reasonably write "(ctrl+o to expand)"
-- inside an answer, but it can't render it as part of a
-- `⏺ <verb> N <thing>… (ctrl+o to expand)` row in the body during
-- a completed turn.
local CTRL_O_HINT = "(ctrl+o to expand)"

local function has_collapsible_tool_progress_row(text)
  for line in string.gmatch(text or "", "[^\n]+") do
    local trimmed = trim(line)
    -- Must start with `⏺` (Claude's tool-progress glyph; 3 bytes
    -- in UTF-8) and end with the collapse hint.
    if starts_with(trimmed, "⏺")
        and #trimmed >= #CTRL_O_HINT
        and string.sub(trimmed, -#CTRL_O_HINT) == CTRL_O_HINT
    then
      return true
    end
  end
  return false
end

local function has_active_work_indicator(text)
  -- Phrases / row shapes the TUI renders ONLY during active work:
  --   * "esc to interrupt"   — control hint shown only while a turn is
  --                            in flight (Claude never writes this in
  --                            answer text).
  --   * A `⏺ <verb>… (ctrl+o to expand)` row — Claude Code 2.1.x's
  --                            collapsible tool-progress row for
  --                            Read/Glob/Grep/Bash interim output.
  --                            These rows START with `⏺` rather than
  --                            a spinner glyph, so
  --                            `has_thinking_spinner_line` misses
  --                            them; without this anchor the
  --                            classifier reads the gap between two
  --                            tool calls as "no active work" and can
  --                            mis-fire `completed_turn` while
  --                            Claude is still mid-turn. The row
  --                            shape (leading `⏺` + trailing hint)
  --                            is dialog-unique — prose can't
  --                            reproduce both anchors at the same
  --                            line positions.
  -- The bare word "thinking" alone (and "running tool", "reading
  -- file", "searching" etc.) was removed in favor of these
  -- structural anchors — those bare words frequently appear in
  -- assistant prose when Claude explains its own behavior,
  -- blocking the classifier from ever reaching `completed_turn`
  -- on those turns.
  if contains(text, "esc to interrupt") then
    return true
  end
  if has_collapsible_tool_progress_row(text) then
    return true
  end
  -- Claude Code 2.1.x spinner-and-ellipsis status line. This is the
  -- primary signal — a spinner glyph at line start + verb + ellipsis
  -- is unique to the live status line and cannot appear in answer
  -- prose because the spinner glyphs are not in any natural text.
  return has_thinking_spinner_line(text)
end

local function has_permission_indicator(text)
  -- Structural detection only. Substring-only anchors (even
  -- dialog-specific phrases like "Do you want to proceed?") false-positive
  -- on assistant prose that *quotes* the anchor — e.g. an answer that
  -- summarises this plugin's own behaviour will render the literal
  -- quoted question text on the screen and trip the matcher. The fix is
  -- to require the two-part structural shape every real dialog has,
  -- which prose mentions don't reproduce:
  --
  --   Numbered layout: the "Do you want to proceed?" line is followed,
  --     within a few rows, by a focused numbered option of the form
  --     `❯ <digit>.`. The `❯` glyph is the cursor on the selected
  --     option; prose never renders it next to a numbered list.
  --
  --   Enter/Esc layout: the bracketed labels `[Enter] Approve` and
  --     `[Esc] Deny` appear on the *same* rendered row (the TUI puts
  --     them together separated by spaces). Prose paraphrasing the
  --     hotkeys typically splits them across sentences or quotes only
  --     one of them at a time.
  --
  -- The `❯` glyph is multi-byte UTF-8 (`%s` doesn't include it and a
  -- Lua `[...]` class can't represent a multi-byte char as a unit), so
  -- we use `string.find` with the literal byte sequence and walk
  -- subsequent lines looking for a numbered option after it.
  local lines = {}
  for line in string.gmatch(text, "[^\n]+") do
    table.insert(lines, line)
  end
  for idx, line in ipairs(lines) do
    -- Enter/Esc structural anchor: both bracket labels on one line.
    if contains(line, "[enter]") and contains(line, "[esc]") then
      return true
    end
    -- Numbered structural anchor: "do you want to proceed?" header
    -- followed within a small window by a `❯ <digit>.` focused option.
    if contains(line, "do you want to proceed") then
      local window_end = math.min(idx + 6, #lines)
      for j = idx + 1, window_end do
        if lines[j]:find("❯%s*%d+%.") then
          return true
        end
      end
    end
  end
  return false
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
  -- Collect lines so we can anchor the tool / summary extraction to
  -- the actual dialog rows rather than running text:match over the
  -- whole screen (which would let prior conversation or prose like
  -- "He used a Bash command earlier" leak into permission.tool).
  local lines = {}
  for line in string.gmatch(text, "[^\n]+") do
    table.insert(lines, line)
  end
  for idx, line in ipairs(lines) do
    local trimmed = trim(line)
    -- Enter/Esc layout: "Allow Bash command: cargo test" on one line.
    local enter_tool, enter_summary = trimmed:match("^[Aa]llow ([%w%-]+) command:%s*(.+)$")
    if not enter_tool then
      enter_tool, enter_summary = trimmed:match("^[Aa]llow ([%w%-]+) tool:%s*(.+)$")
    end
    if enter_tool then
      permission.tool = permission.tool or enter_tool
      if not permission.summary then
        permission.summary = trim(enter_summary)
      end
    end
    -- Numbered layout: a line that *is* "<Tool> command" (or "tool"),
    -- with no extra preamble, followed by the command body on the next
    -- non-empty line. Anchoring on the exact line shape rejects matches
    -- against prior prose mentioning the word "command".
    local list_tool = trimmed:match("^([%w%-]+) command$") or trimmed:match("^([%w%-]+) tool$")
    if list_tool then
      permission.tool = permission.tool or list_tool
      for follow_idx = idx + 1, #lines do
        local follow = trim(lines[follow_idx])
        if follow ~= "" then
          if not contains(lower(follow), "do you want to proceed") then
            permission.summary = permission.summary or follow
          end
          break
        end
      end
    end
  end

  local options = {}
  -- Numbered options are anchored to the *start of a line* (after optional
  -- whitespace and the `❯` / `>` selector glyph), not "anywhere in the
  -- line." Without that anchor, a visible command summary like
  -- `echo '1. hello'` would be picked up as an option and the actual
  -- Enter/Esc labels below could fail to parse. Lua patterns can't
  -- represent the multi-byte `❯` in a `[...]` class, so strip it (and
  -- the `>` ASCII fallback) up front, then anchor with `^`.
  local selector_glyph = "❯"
  for _, line in ipairs(lines) do
    local trimmed = trim(line)
    if trimmed:sub(1, #selector_glyph) == selector_glyph then
      trimmed = trim(trimmed:sub(#selector_glyph + 1))
    elseif trimmed:sub(1, 1) == ">" then
      trimmed = trim(trimmed:sub(2))
    end
    local body = trimmed:match("^%d+%.%s+(.+)$")
    if body then
      table.insert(options, trim(body))
    end
  end
  if #options == 0 then
    -- Enter/Esc bracketed layout: preserve whichever label the TUI
    -- actually rendered rather than normalising to Approve / Deny.
    -- Plugin authors who'd rather see normalised labels can post-process
    -- the metadata themselves.
    local enter_label = text:match("%[Enter%]%s*([%w%-]+)")
    if enter_label then
      table.insert(options, enter_label)
    end
    local esc_label = text:match("%[Esc%]%s*([%w%-]+)")
    if esc_label then
      table.insert(options, esc_label)
    end
  end
  if #options > 0 then
    permission.options = options
  end
  if permission.summary then
    permission.summary = redact_secret_patterns(permission.summary)
  end
  if next(permission) == nil then
    return nil
  end
  return { permission = permission }
end

local function has_plan_indicator(text)
  -- Structural pattern: a "plan" header on its own line followed by
  -- a numbered list within a line or two. Both fixtures match this:
  --   plan_approval.txt: "Plan ready:\n1. Update tests"
  --   plan_variant.txt:  "Plan\n  1. Update tests"
  --
  -- Word-level matchers (like "approve plan to proceed") false-positive
  -- on assistant prose that quotes the dialog text, blocking the
  -- classifier from ever reaching `completed_turn` on turns that
  -- summarise the plugin's own behaviour. The numbered-list-following-
  -- "plan"-header pattern is dialog-unique because prose mentions of
  -- "plan" don't continue with "1." on the next render row.
  if text:match("\nplan[^\n]*\n%s*1[%.%)]") then return true end
  -- Match at start-of-text too (in case the body starts with "Plan").
  if text:match("^plan[^\n]*\n%s*1[%.%)]") then return true end
  return false
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

-- `/model` opens an interactive selection panel: a "Select a model:"
-- (or "Switch to model:") header followed by a numbered list of
-- available models, with the focus glyph `❯` on one row. Three
-- structural anchors are required so assistant prose that explains
-- model selection — and might reasonably list `1. Sonnet 2. Opus
-- 3. Haiku` in the middle of a paragraph — can't trip the detector:
--   1. A header phrase (`select a model:` / `choose a model:` /
--      `switch to model:` / `available models:`) — note the COLON,
--      which Claude's TUI renders and prose typically doesn't.
--   2. A focused numbered option line (`❯ 1.` / `❯ 1)`).
--   3. A keyboard hint line (`enter to select`, `esc to cancel`,
--      `↑/↓` arrows). Real pickers always render this; prose lists
--      don't.
-- All three together are dialog-unique. A completed answer can
-- contain any one of them, but very rarely all three with the
-- right structure.
local MODEL_PICKER_HEADERS = {
  "select a model:",
  "switch to model:",
  "choose a model:",
  "available models:",
}

local function has_model_picker_indicator(text)
  local has_header = contains_any(text, MODEL_PICKER_HEADERS)
  if not has_header then
    return false
  end
  -- Require the focused-option glyph (`❯ <digit>.`/`)`); the dialog
  -- never renders without a cursor on one option.
  if not text:find("❯%s*%d+[%.%)]") then
    return false
  end
  -- Require a navigation-hint line. Either keyboard verbs Claude's
  -- prose wouldn't naturally use in a model-list explanation
  -- (`↑/↓`, `esc to cancel`, `enter to select`).
  return contains_any(text, {
    "enter to select",
    "esc to cancel",
    "↑/↓",
    "↑ / ↓",
  })
end

-- Claude Code's signed-out / not-authenticated screen. Two shapes:
--   * an OAuth prompt: "Log in to Claude Code" / "Press Enter to log in"
--     with a hint about an `anthropic.com` URL,
--   * an API-key fallback: "Sign in to Claude" / "API key" / "Enter your
--     API key:".
-- A turn whose prose mentions "log in" can't realistically combine all
-- three anchors at once (URL hint + action verb + prompt phrase), so the
-- two-anchor pairing keeps prose mentions from tripping the detector.
-- Login / sign-in dialog. Three structural anchors required so an
-- assistant answer explaining how to authenticate (which can
-- reasonably mention "log in to Claude", "API key", and an Anthropic
-- URL together) doesn't trip the detector:
--   1. A title-style login phrase rendered at line start. The TUI
--      puts "Welcome to Claude Code" / "Log in to Claude Code" /
--      "Sign in to Claude" / "Continue with Anthropic" on its own
--      panel row. Prose typically embeds those phrases inside
--      sentences.
--   2. An action prompt — "Press Enter to log in" / "Open the link
--      in your browser" / `https://claude.ai/login`. Prose
--      explaining authentication is unlikely to render an action
--      verb of this exact shape.
--   3. The fallback "API key" affordance is accepted as the second
--      anchor only when paired with a setenv hint ("ANTHROPIC_API_KEY"
--      in caps), so a generic prose mention of "api key" doesn't
--      single-handedly satisfy the second anchor.
local LOGIN_TITLE_PREFIXES = {
  "welcome to claude code",
  "log in to claude",
  "login to claude",
  "sign in to claude",
  "continue with anthropic",
  "continue with google",
}

local function has_login_indicator(text)
  -- Anchor #1: a title-style login phrase appears somewhere in the
  -- body. Use `contains` rather than `starts_with` because the TUI
  -- wraps the title inside a box-drawing panel (`│   Welcome to
  -- Claude Code`); trimming the box border isn't enough because
  -- multiple panel rows have border chars before content. Prose
  -- often quotes login titles too, so this anchor alone isn't
  -- sufficient — pair it with anchor #2.
  local has_title = contains_any(text, LOGIN_TITLE_PREFIXES)
  if not has_title then
    return false
  end
  -- Anchor #2: at least one dialog-unique structural cue. Each of
  -- these is something the TUI renders but assistant prose would
  -- rarely combine with a login title:
  --   * a verbatim action prompt rendered on its own row,
  --   * a real OAuth URL anchored at the start of a line (typically
  --     `https://claude.ai/login...` or `https://anthropic.com/...`),
  --   * the API-key env var name (only the TUI panel renders that
  --     verbatim).
  if contains(text, "press enter to log in") then return true end
  if contains(text, "open the link") then return true end
  for line in string.gmatch(text or "", "[^\n]+") do
    local t = trim(line)
    if t:find("^https?://[%w%./%-_?=&%%]+claude%.ai") then return true end
    if t:find("^https?://[%w%./%-_?=&%%]+anthropic%.com") then return true end
  end
  -- API-key fallback: the TUI renders `ANTHROPIC_API_KEY` in caps.
  -- `text` is the body lowered for classifier consistency, so check
  -- the lowered form here.
  if contains(text, "anthropic_api_key") then return true end
  return false
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
  -- Match the *last* `[...]` rather than the first. Claude renders the
  -- model at the end of the cwd row (`user @ host /path [Haiku 4.5]`)
  -- and a future TUI tweak that introduced an earlier bracketed token
  -- (e.g. a `[?]` help chip or a key-hint pill) would otherwise quietly
  -- shadow the model string. Walking the iterator and keeping the last
  -- capture is cheap on a 3-row status bar and stays robust to those
  -- insertions.
  local last_bracket
  for bracketed in string.gmatch(text, "%[([^%]]+)%]") do
    last_bracket = bracketed
  end
  if last_bracket then
    status.model = trim(last_bracket)
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
-- `status`, `permission`, …) so a deep merge is unnecessary. Returns a
-- fresh table rather than mutating `a` in place; if a future refactor
-- ever calls the merger more than once per classify (e.g. for memoised
-- closures), per-branch writes won't bleed back into the shared
-- `status_metadata` captured in the classify scope.
local function merge_metadata(a, b)
  if a == nil and b == nil then
    return nil
  end
  local merged = {}
  if a ~= nil then
    for key, value in pairs(a) do
      merged[key] = value
    end
  end
  if b ~= nil then
    for key, value in pairs(b) do
      merged[key] = value
    end
  end
  return merged
end

local function has_welcome_screen(text)
  -- Claude Code's first-launch welcome panel renders after the workspace
  -- trust dialog is accepted. It shows "Welcome back <user>!" alongside a
  -- "Tips for getting started" / "What's new" pane and traps the first
  -- Enter keypress (dismissing the welcome rather than submitting the
  -- caller's prompt). Detect it so the classifier reports `starting`
  -- instead of `waiting_for_user_input` — Claude is not actually ready to
  -- accept a prompt yet, even though the input cursor `❯` is on screen.
  --
  -- Two-anchor pairing: `welcome back` / `claude code v` (a TUI-only
  -- header phrase) PLUS one of the "what's new" / "tips for getting
  -- started" / "ask claude to" panels. The compact-launch view that
  -- ships under tight terminals omits one of the side panels but
  -- always renders the welcome header AND at least one suggestion
  -- block, so accepting either side panel keeps small terminals
  -- working. Requiring the welcome-header anchor keeps assistant
  -- prose that says "tips for getting started" by itself from
  -- tripping the detector.
  local has_welcome_header = contains_any(text, {
    "welcome back",
    "claude code v",
    "welcome to claude code",
  })
  if not has_welcome_header then
    return false
  end
  return contains_any(text, {
    "tips for getting started",
    "what's new",
    "ask claude to",
    "/release-notes for more",
    "/help for help",
  })
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

  -- Plan-approval and permission dialogs in the Claude Code TUI can
  -- straddle the body/status split: the "Plan ready"/"Plan" header or
  -- "Do you want to proceed?" question sits in the body while the
  -- numbered options ("❯ 1. Yes" / "  2. No") and the approve/accept
  -- hints render in the bottom rows. Both dialog branches below need
  -- visibility into the full screen for that reason. The
  -- `bypass permissions on` false positive (Milestone 21.4) is no
  -- longer a risk because `has_permission_indicator` now requires
  -- structural anchors (focus glyph + numbered option, or both
  -- bracket labels on one line) rather than loose substring matches.
  local body_and_status = body_text .. "\n" .. lower(status)

  -- Login / sign-in dialog runs before any other dialog branch because
  -- nothing else is actionable when the user isn't authenticated:
  -- send_prompt's leading Enter would either trigger the OAuth flow
  -- (opening a browser, which a script driver can't complete) or land
  -- in the API-key entry box. Callers must `claude login` interactively
  -- once before driving the adapter; the script-side handler surfaces
  -- this as a non-zero exit with a clear message.
  if has_login_indicator(body_text) then
    return state_snapshot("waiting_for_login", 0.86, "login / sign-in dialog detected", sequence)
  end

  -- Workspace-trust dialog is checked before permission/plan because its
  -- approve action differs (numbered selection, not a single Enter press).
  -- Trust questions and answers live entirely in the dialog body; the
  -- status bar carries only navigation hints.
  if has_trust_indicator(body_text) then
    return state_snapshot("waiting_for_trust", 0.86, "workspace trust dialog detected", sequence)
  end

  -- Model picker (opened by `/model`). Anchored on the header phrase
  -- plus either a focused numbered option or two consecutive numbered
  -- list entries, so prose mentioning "select a model" can't trip it.
  if has_model_picker_indicator(body_text) then
    return state_snapshot("waiting_for_model_select", 0.82, "model picker dialog detected", sequence)
  end

  if has_plan_indicator(body_text) or (contains(body_text, "plan") and has_plan_indicator(body_and_status)) then
    return state_snapshot("waiting_for_plan_approval", 0.8, "plan approval text detected", sequence)
  end

  if has_permission_indicator(screen_text) then
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

  -- Mid-turn determinism — when a turn is in flight (`prompt_submitted`)
  -- and no completion marker is on screen yet, classify as `thinking`
  -- regardless of whether the active-work indicator happens to be visible
  -- on this particular frame. Without this branch the classifier
  -- oscillates between `thinking` (spinner glyph captured) and
  -- `waiting_for_user_input` (between spinner repaints — the submitted
  -- prompt is still visible so `has_input_prompt` keeps returning true)
  -- on every polling tick, which makes downstream consumers (the
  -- `claude-stream` body diff loop, anything driving `adapter.state` on a
  -- heartbeat) see spurious state churn. The completion marker
  -- (`✻ <Verb> for <duration>`) is the TUI's structural end-of-turn
  -- signal, so its absence is the durable mid-turn signal: any screen
  -- where it isn't present yet is by definition still mid-turn.
  --
  -- This branch deliberately runs after the active-work-indicator
  -- branch above so that branch's higher 0.76 confidence wins when the
  -- spinner IS visible; this fallback fires only between repaints.
  if last_intent == "prompt_submitted" and not has_turn_completion_marker(screen) then
    return state_snapshot(
      "thinking",
      0.6,
      "turn in flight; no completion marker on screen",
      sequence
    )
  end

  -- Poll-path completed_turn — fires when adapter.state polling sees
  -- the "Claude is back at idle after answering" pattern: input
  -- prompt visible + tea-verb completion marker on screen + no active
  -- work spinner.
  --
  -- The completion-marker (`✻ <Verb> for <duration>`) gate is what
  -- prevents the classic preamble false positive: a turn that starts
  -- with "⏺ I'll explore the project..." renders a prompt for a moment
  -- before the next tool spinner repaints; without the marker
  -- requirement, that frame looks identical to a real turn boundary and
  -- the classifier flips to `completed_turn` mid-stream. The
  -- `✻ <Verb> for <duration>` line is the TUI's own end-of-turn signal
  -- — it's never rendered between tool calls. Long answers can scroll
  -- the original answer bullet out of the visible body by the time the
  -- completion marker appears, so do not require `⏺` here.
  --
  -- Gated on `stable_ms < completed_turn_stable_ms` so callers driving
  -- `adapter.wait` (which always supplies a stable_ms >= the threshold)
  -- fall through to the higher-confidence stable-path branches below
  -- and get the matcher-gated answer instead. This branch exists for
  -- polling consumers that don't have screen-stability evidence.
  if last_intent == "prompt_submitted"
      and (stable_ms == 0 or completed_turn_stable_ms == 0 or stable_ms < completed_turn_stable_ms)
      and not has_active_work_indicator(body_text)
      and has_turn_completion_marker(screen)
      and has_input_prompt(body)
  then
    return state_snapshot(
      "completed_turn",
      0.7,
      "completion marker plus input prompt visible without active work",
      sequence
    )
  end

  -- The previous "ready prompt text" branch matched assistant prose
  -- (any answer mentioning "how can I help" or "what would you like"),
  -- which blocked the classifier from reaching `completed_turn` on
  -- turns that summarized Claude Code's own help text. Distinguishing
  -- the actual ready screen from prose is brittle — older Claude Code
  -- showed the question as standalone text below a clean banner, but
  -- 2.1.x dropped it from the compact welcome view entirely. The
  -- `waiting_for_user_input` branch below (which gates on the prompt
  -- glyph being visible) covers the same intent without the false
  -- positive, so we collapse `ready` into it.

  if has_input_prompt(screen) then
    if last_intent == "prompt_submitted"
        and completed_turn_stable_ms > 0
        and stable_ms >= completed_turn_stable_ms
        and not has_active_work_indicator(body_text)
        and has_turn_completion_marker(screen)
    then
      return state_snapshot("completed_turn", 0.78, "stable input prompt after prompt submission", sequence)
    end
    -- The welcome panel renders an input cursor even though Claude won't
    -- treat the first Enter as a prompt submission. Report `starting` so
    -- callers don't race-send before `dismiss_welcome` is invoked.
    --
    -- Gate this on `last_intent ~= "prompt_submitted"`: if the caller has
    -- already submitted a prompt, the welcome chrome is stale visual
    -- residue from Claude Code 2.1.x not redrawing the screen — the
    -- adapter is mid-turn or post-turn, not waiting for welcome dismissal.
    -- Without this gate the classifier oscillates between `thinking` (on
    -- ticks where the spinner glyph is captured) and `starting` (on ticks
    -- between spinner frames), which makes turn-boundary polling
    -- unreliable on real 2.1.x sessions.
    if has_welcome_screen(body_text) and last_intent ~= "prompt_submitted" then
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
  --
  -- The leading Enter handles Claude Code 2.1.x's first-keypress
  -- interceptors (welcome panel, compact-launch view). On a clean
  -- input box Claude treats Enter on empty input as a no-op submit;
  -- on a welcome / interceptor screen it dismisses the overlay and
  -- focuses the input box, so the bracketed paste that follows lands
  -- in the right place. Without this leading Enter, callers had to
  -- send their own Enter and synchronise on stability before pasting,
  -- which is fragile across machine speeds and Claude Code versions.
  -- driving programs that have not opted into bracketed paste.
  --
  -- Empty-prompt guard: an empty bracketed paste leaves Claude at
  -- idle (the two Enters are no-ops on an empty input box), so do
  -- NOT advance to the `prompt_submitted` intent. The classifier's
  -- mid-turn `thinking` branch keys off that intent; claiming a
  -- turn started when no actual prompt was submitted would lock the
  -- state machine in `thinking` forever, since the TUI never
  -- renders the `✻ <Verb> for <duration>` completion marker that's
  -- the durable mid-turn release signal.
  --
  -- Equally important: actively CLEAR any previously-recorded
  -- intent. If a caller submitted a real prompt, the turn completed,
  -- and then called `send_prompt("")` again, leaving the recorded
  -- intent at `prompt_submitted` would keep the mid-turn /
  -- completed-turn branches active against an idle screen. We
  -- communicate "clear the intent" by returning `last_intent = ""`;
  -- the host's `apply_plan` interprets the empty string as an
  -- explicit reset (see `src/extension.rs::apply_plan`).
  local prompt = input.prompt or ""
  if prompt == "" then
    return { actions = {}, last_intent = "" }
  end
  return {
    actions = {
      action.key("enter"),
      action.bracketed_paste(prompt),
      action.key("enter"),
    },
    last_intent = "prompt_submitted",
  }
end

function M.wait_cancel_settled_matcher(input)
  -- After `cancel`, the classifier holds in `cancelling` until the screen
  -- has been quiet for `completed_turn_stable_ms`. Mirror that exact
  -- threshold here so callers waiting for the transition see the same
  -- behaviour as the classifier — no risk of the wait firing while the
  -- classifier still reports `cancelling`.
  local completed_turn_stable_ms = tonumber(input.completed_turn_stable_ms) or 0
  return matcher.screen_stable(completed_turn_stable_ms)
end

-- Mid-turn steering: inject an additional prompt while Claude is still
-- working on the previous turn. Same action shape as `send_prompt`
-- (bracketed paste + Enter) but deliberately leaves `last_intent` alone
-- so the classifier's `completed_turn` gate keeps waiting for the
-- original turn to actually finish. Otherwise a mid-turn stable screen
-- (Claude paused before another tool call) would look like "turn done"
-- to `wait_turn_matcher`.
function M.steer(input)
  return {
    actions = {
      action.bracketed_paste(input.prompt or ""),
      action.key("enter"),
    },
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
      -- Login / sign-in dialog. Mirrors `has_login_indicator` so
      -- callers using `adapter.wait` after `adapter.start` against
      -- an unauthenticated user wake on the sign-in panel instead
      -- of timing out. "Press Enter to log in" is the action prompt
      -- the TUI always renders inside the panel.
      matcher.contains_text("Press Enter to log in"),
      matcher.contains_text("Log in to Claude Code"),
      -- Model picker dialog (opened by `/model`). Mirrors
      -- `has_model_picker_indicator` so callers waiting after a
      -- `slash_command("model")` send wake on the picker rather
      -- than timing out.
      matcher.contains_text("Select a model:"),
      matcher.contains_text("Switch to model:"),
      -- Prompt glyph alone is NOT a completion anchor — it
      -- appears for one frame during preambles before a tool call
      -- while the next spinner is between repaints, and `adapter.wait`
      -- would otherwise return with `waiting_for_user_input` on that
      -- frame instead of holding until the turn actually ends. Pair
      -- it with the tea-verb completion marker (`✻ <Verb> for <N>`)
      -- the TUI renders only at end-of-turn, matching the classifier's
      -- `completed_turn` gate. Claude may render ghost text or a
      -- suggested follow-up after the prompt glyph (for example
      -- `❯ run the tests`), so do not require an empty prompt row here.
      -- Dialog / usage anchors above still wake the matcher on their own.
      matcher.all({
        matcher.screen_regex("(?m)^\\s*(?:>|❯).*"),
        matcher.screen_regex("✻ \\S+ for \\d"),
      }),
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

-- Toggle expansion of the focused collapsible row (`Reading N files… (ctrl+o
-- to expand)`, search results, Bash output, etc.). Claude Code 2.1.x binds
-- Ctrl+O to expand/collapse the row under the cursor; callers driving the
-- TUI from outside the keyboard use this intent so they don't have to know
-- the key binding.
function M.expand(_input)
  return {
    actions = {
      action.key("ctrl_o"),
    },
  }
end

-- Submit a slash command (`/btw`, `/clear`, `/help`, `/usage`, `/model`,
-- `/release-notes`, project-defined commands, …). Bracketed-pastes the
-- token then presses Enter — same shape as `send_prompt` but with two
-- important differences:
--
--   1. The leading Enter dismissal is omitted. Slash commands are only
--      meaningful when the input box already has focus (no welcome
--      panel covering it); spurious Enter on a settled input row would
--      submit an empty turn first, which can race with the slash text.
--   2. `last_intent` is NOT set to `prompt_submitted`. Slash commands
--      open a UI (modal / panel / inline action) rather than starting a
--      conversation turn, so the classifier's mid-turn `thinking`
--      branch must not engage. Leaving `last_intent` alone lets the
--      classifier read whatever the slash command rendered on screen
--      (`waiting_for_user_input` for a panel back at idle, the panel-
--      specific dialog branches for `/model` or `/usage`, etc.).
--
-- Accepts either bare `name` ("btw") or the leading-slash form
-- ("/btw"); the plugin normalises so callers don't have to.
function M.slash_command(input)
  local name = (input and (input.command or input.name)) or ""
  if name == "" then
    return { actions = {} }
  end
  if string.sub(name, 1, 1) ~= "/" then
    name = "/" .. name
  end
  return {
    actions = {
      action.bracketed_paste(name),
      action.key("enter"),
    },
  }
end

function M.cancel(_input)
  -- Claude Code 2.1.x captures Escape as the mid-turn interrupt key —
  -- the active-work indicator literally renders "esc to interrupt".
  -- Ctrl-C is reserved for a different role at idle (one press warns,
  -- two presses exit Claude entirely), so sending it mid-turn would
  -- either be ignored or trigger Claude's exit confirmation flow
  -- instead of cancelling the current turn cleanly. `action.key`
  -- with the "escape" alias is what the active-work hint maps to.
  return {
    actions = {
      action.key("escape"),
    },
    last_intent = "cancelling",
  }
end

-- Two-Escape escalation for tool calls that are already in flight
-- when the first Escape arrives. Real Claude Code occasionally needs
-- the second press to actually interrupt — a single Escape sometimes
-- lands during an API request that's already serializing, which
-- Claude completes before honoring the interrupt. This intent sends
-- both presses in one plan so callers don't have to script the
-- escalation themselves. Same `last_intent = "cancelling"` so the
-- classifier's hold-state behaves identically.
function M.force_cancel(_input)
  return {
    actions = {
      action.key("escape"),
      action.key("escape"),
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
