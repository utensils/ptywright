-- Verbatim TUI strings extracted from the installed Claude Code
-- binary (version 2.1.150, built 2026-05-23T01:22:49Z, git SHA
-- 28d4819e0f0a51840356d175c2a710f0c83db5b4 — pulled from
-- `~/.local/share/claude/versions/2.1.150`).
--
-- This module is the *ground-truth* anchor table for `main.lua`'s
-- structural matchers. Where the classifier currently uses regex
-- heuristics or hand-rolled phrase lists, it should ideally consult
-- the relevant constant here so an upstream string drift is a one-
-- file fix instead of a regex hunt across `main.lua`.
--
-- **Cross-check at every Claude Code version bump.** Re-run
-- `python3 tests/extract_claude_strings.py` (TODO add the script) to
-- regenerate from the installed binary, then diff against this file.
-- A change in any of these constants is a TUI contract change that
-- the plugin needs to react to.
--
-- Pre-loaded as a Lua global by the host via
-- `BUILTIN_PLUGINS.modules` in `src/plugin.rs`, so `main.lua` reads
-- it as `strings.<field>` without a `require` call.

local M = {}

-- Closed list of past-tense verbs used in completion markers.
-- Format on screen: `✻ <Verb> for <duration>` (e.g.
-- `✻ Worked for 12s`). Verified verbatim from `Sb9` array constant
-- in the binary.
M.TURN_COMPLETION_VERBS = {
  "Baked", "Brewed", "Churned", "Cogitated",
  "Cooked", "Crunched", "Sautéed", "Worked",
}

-- Full present-participle spinner verb list (187 entries). Rendered
-- as `✻ <Verb>…` in the spinner row while the model is producing
-- output. Sampled randomly per turn — the plugin should NOT assume
-- a small fixed subset.
M.SPINNER_VERBS = {
  "Accomplishing", "Actioning", "Actualizing", "Architecting", "Baking",
  "Beaming", "Beboppin'", "Befuddling", "Billowing", "Blanching",
  "Bloviating", "Boogieing", "Boondoggling", "Booping", "Bootstrapping",
  "Brewing", "Bunning", "Burrowing", "Calculating", "Canoodling",
  "Caramelizing", "Cascading", "Catapulting", "Cerebrating", "Channeling",
  "Channelling", "Choreographing", "Churning", "Clauding", "Coalescing",
  "Cogitating", "Combobulating", "Composing", "Computing", "Concocting",
  "Considering", "Contemplating", "Cooking", "Crafting", "Creating",
  "Crunching", "Crystallizing", "Cultivating", "Deciphering", "Deliberating",
  "Determining", "Dilly-dallying", "Discombobulating", "Doing", "Doodling",
  "Drizzling", "Ebbing", "Effecting", "Elucidating", "Embellishing",
  "Enchanting", "Envisioning", "Evaporating", "Fermenting", "Fiddle-faddling",
  "Finagling", "Flambéing", "Flibbertigibbeting", "Flowing", "Flummoxing",
  "Fluttering", "Forging", "Forming", "Frolicking", "Frosting",
  "Gallivanting", "Galloping", "Garnishing", "Generating", "Gesticulating",
  "Germinating", "Gitifying", "Grooving", "Gusting", "Harmonizing",
  "Hashing", "Hatching", "Herding", "Honking", "Hullaballooing",
  "Hyperspacing", "Ideating", "Imagining", "Improvising", "Incubating",
  "Inferring", "Infusing", "Ionizing", "Jitterbugging", "Julienning",
  "Kneading", "Leavening", "Levitating", "Lollygagging", "Manifesting",
  "Marinating", "Meandering", "Metamorphosing", "Misting", "Moonwalking",
  "Moseying", "Mulling", "Mustering", "Musing", "Nebulizing",
  "Nesting", "Newspapering", "Noodling", "Nucleating", "Orbiting",
  "Orchestrating", "Osmosing", "Perambulating", "Percolating", "Perusing",
  "Philosophising", "Photosynthesizing", "Pollinating", "Pondering", "Pontificating",
  "Pouncing", "Precipitating", "Prestidigitating", "Processing", "Proofing",
  "Propagating", "Puttering", "Puzzling", "Quantumizing", "Razzle-dazzling",
  "Razzmatazzing", "Recombobulating", "Reticulating", "Roosting", "Ruminating",
  "Sautéing", "Scampering", "Schlepping", "Scurrying", "Seasoning",
  "Shenaniganing", "Shimmying", "Simmering", "Skedaddling", "Sketching",
  "Slithering", "Smooshing", "Sock-hopping", "Spelunking", "Spinning",
  "Sprouting", "Stewing", "Sublimating", "Swirling", "Swooping",
  "Symbioting", "Synthesizing", "Tempering", "Thinking", "Thundering",
  "Tinkering", "Tomfoolering", "Topsy-turvying", "Transfiguring", "Transmuting",
  "Twisting", "Undulating", "Unfurling", "Unravelling", "Vibing",
  "Waddling", "Wandering", "Warping", "Whatchamacalliting", "Whirlpooling",
  "Whirring", "Whisking", "Wibbling", "Working", "Wrangling",
  "Zesting", "Zigzagging",
}

-- Built-in tool names that appear in tool-call rows shaped as
-- `<ToolName>(...)`. Extracted by probing the binary for each known
-- candidate; MCP tools are not in this list (they're dynamic and
-- ship under a runtime-resolved name pattern).
M.TOOL_NAMES = {
  "Agent", "AskUserQuestion", "Bash", "Brief", "Config",
  "CronCreate", "CronDelete", "CronList", "Edit", "EnterPlanMode",
  "EnterWorktree", "ExitPlanMode", "ExitWorktree", "Glob", "Grep",
  "LSP", "MCP", "NotebookEdit", "NotebookRead", "PowerShell",
  "REPL", "Read", "RemoteTrigger", "ScheduleWakeup", "SendMessage",
  "Skill", "Task", "TaskCreate", "TaskGet", "TaskList",
  "TaskOutput", "TaskStop", "TaskUpdate", "TeamCreate", "TeamDelete",
  "TodoWrite", "ToolSearch", "WebFetch", "WebSearch", "Write",
}

-- Error / quota / auth / connection banners, grouped by category.
-- Each entry is the verbatim left-anchored prefix the TUI renders.
-- Used by `parse_error_subtype` in `main.lua` to assign
-- `metadata.error.kind` — the classifier picks the most specific
-- match within a category, so ordering here is by specificity
-- (longer / more specific prefixes first).
M.ERROR_BANNERS = {
  -- Rate-limit banners changed shape in 2.1.x — older "You've hit
  -- your <X> limit" phrasing was retired. The 2.1.150 set is:
  rate_limit = {
    "You've used",                       -- `You've used N% of your <kind>`
    "You're now using extra usage",
    "You're close to",                   -- `You're close to your session/weekly/extra usage spending limit`
    "You're out of extra usage",
    "Now using extra usage",
  },
  quota = {
    "Credit balance is too low",
    "Prompt is too long",
    "PDF too large",
    "Image was too large",
  },
  auth = {
    "Not logged in · Please run /login",
    "Invalid API key · Fix external API key",
    "Your ANTHROPIC_API_KEY belongs to a disabled organization",
    "OAuth token revoked",
    "Authentication error",
  },
  connection = {
    "Unable to connect to API: SSL certificate is not yet valid",
    "Unable to connect to API: SSL error",
    "Unable to connect to API. Check your internet connection",
    "Unable to connect to API",
    "Connection error.",
    "Request timed out",
  },
  api = {
    "API Error",
    "API error (status",
  },
}

-- Permission / plan-mode dialog header strings. Each appears in
-- the dialog body when the corresponding workflow is active.
M.PERMISSION_HEADERS = {
  default_question = "Do you want to proceed?",
  enter_plan_mode = "Enter plan mode?",
  ready_to_code = "Ready to code?",
  exit_plan_mode = "Exit plan mode?",
  waiting_for_permission = "Waiting for permission",
  approve_label = "Approve",
  allow_label = "Allow",
}

-- Workspace-trust dialog phrases (the first-run "do you trust this
-- folder" prompt). Strings verified verbatim against the installed
-- binary; the older `Do you trust the files` shape is gone in 2.1.x.
M.TRUST_DIALOG = {
  "Accessing workspace",
  "Yes, I trust this folder",
  "No, exit",
}

-- Model-picker dialog headers / option labels (opened by /model).
M.MODEL_PICKER = {
  "Select model",
  "Switch between Claude models",
  "Auto Mode Active",
  "Opus 4.7 only",
  "Opus 4.6/4.7, Sonnet 4.6",
}

-- Welcome / onboarding panel phrases.
M.WELCOME = {
  "Welcome to Claude Code",
  "Welcome back",
  "Tips for getting started",
  "What's new",
}

-- Tool-progress chrome — phrases that show up around an in-flight
-- tool call but are not themselves the tool name. The plugin's
-- chrome strippers consume these in transcript extraction.
M.TOOL_PROGRESS_CHROME = {
  "Running...",
  "Running…",
  "Running in the background",
  "(ctrl+o to expand)",
  "tool use",
  "tool uses",
  "Done",
}

-- /usage panel phrases. Used by `parse_usage_screen` to identify
-- and slice the usage view.
M.USAGE_PANEL = {
  "Total cost:",
  "Current week (all models)",
  "Loading usage data",
  "usage credits",
  "usage credit limit",
  "% of your",
  "session limit",
  "weekly limit",
  "Opus limit",
  "Sonnet limit",
}

-- Glyph constants. `BLACK_CIRCLE` is platform-conditional in the
-- upstream source (`figures.ts`): darwin renders `⏺`, linux/win
-- render `●`. The plugin matches both since the rendered host may
-- not be the same as the classifier's host.
M.GLYPHS = {
  BLACK_CIRCLE_DARWIN  = "⏺",
  BLACK_CIRCLE_OTHER   = "●",
  TEARDROP_ASTERISK    = "✻",  -- spinner + completion marker
  PROMPT_GLYPH         = "❯",
  TOOL_CALLOUT         = "⎿",
  STATUS_BAR_BULLETS   = "⏵⏵",
  BOX_VERTICAL         = "│",
  BOX_BRANCH           = "├",
  BOX_CORNER           = "└",
  BLOCKQUOTE_BAR       = "▎",
  DIAMOND_OPEN         = "◇",
  DIAMOND_FILLED       = "◆",
  EFFORT_LOW           = "○",
  EFFORT_MEDIUM        = "◐",
  EFFORT_HIGH          = "●",
  EFFORT_MAX           = "◉",
  UP_ARROW             = "↑",
  DOWN_ARROW           = "↓",
}

return M
