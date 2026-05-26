-- Smoke specs for plugins/claude-code/strings.lua — the verbatim
-- TUI catalog extracted from the installed Claude Code binary.
--
-- These specs do not assert specific contents (the catalog is
-- expected to drift with upstream Claude Code releases). They
-- assert *shape*: every documented top-level field exists, lists
-- are non-empty arrays, GLYPHS is a string map. A failure here
-- means strings.lua was edited in a way that breaks main.lua's
-- consumption pattern; a Claude Code version bump that changes
-- specific strings is a content edit, not a shape change.

local strings = strings

describe("strings module shape", function()
  it("exports TURN_COMPLETION_VERBS as a non-empty array of strings", function()
    expect(type(strings.TURN_COMPLETION_VERBS)).to_equal("table")
    expect(#strings.TURN_COMPLETION_VERBS > 0).to_be_truthy()
    for _, verb in ipairs(strings.TURN_COMPLETION_VERBS) do
      expect(type(verb)).to_equal("string")
      expect(#verb > 0).to_be_truthy()
    end
  end)

  it("exports SPINNER_VERBS as a non-empty array of strings", function()
    expect(type(strings.SPINNER_VERBS)).to_equal("table")
    expect(#strings.SPINNER_VERBS > 100).to_be_truthy()
    for _, verb in ipairs(strings.SPINNER_VERBS) do
      expect(type(verb)).to_equal("string")
    end
  end)

  it("exports TOOL_NAMES as a non-empty array of strings", function()
    expect(type(strings.TOOL_NAMES)).to_equal("table")
    expect(#strings.TOOL_NAMES > 10).to_be_truthy()
    for _, name in ipairs(strings.TOOL_NAMES) do
      expect(type(name)).to_equal("string")
      -- Tool names must be valid identifiers (no spaces, no parens).
      expect(name:find("[^%w_]") == nil).to_be_truthy()
    end
  end)

  it("exports ERROR_BANNERS grouped by category", function()
    expect(type(strings.ERROR_BANNERS)).to_equal("table")
    for _, category in ipairs({ "rate_limit", "quota", "auth", "connection", "api" }) do
      expect(type(strings.ERROR_BANNERS[category])).to_equal("table")
      expect(#strings.ERROR_BANNERS[category] > 0).to_be_truthy()
    end
  end)

  it("exports PERMISSION_HEADERS, TRUST_DIALOG, MODEL_PICKER, WELCOME, USAGE_PANEL, TOOL_PROGRESS_CHROME", function()
    expect(type(strings.PERMISSION_HEADERS)).to_equal("table")
    expect(strings.PERMISSION_HEADERS.default_question).to_equal("Do you want to proceed?")
    expect(type(strings.TRUST_DIALOG)).to_equal("table")
    expect(type(strings.MODEL_PICKER)).to_equal("table")
    expect(type(strings.WELCOME)).to_equal("table")
    expect(type(strings.USAGE_PANEL)).to_equal("table")
    expect(type(strings.TOOL_PROGRESS_CHROME)).to_equal("table")
  end)

  it("exports GLYPHS as a string map", function()
    expect(type(strings.GLYPHS)).to_equal("table")
    expect(strings.GLYPHS.BLACK_CIRCLE_DARWIN).to_equal("⏺")
    expect(strings.GLYPHS.BLACK_CIRCLE_OTHER).to_equal("●")
    expect(strings.GLYPHS.TEARDROP_ASTERISK).to_equal("✻")
    expect(strings.GLYPHS.PROMPT_GLYPH).to_equal("❯")
  end)
end)

describe("strings catalog content sanity", function()
  it("includes Sautéed in the TURN_COMPLETION_VERBS list", function()
    -- The é character round-trip — broken UTF-8 here would surface
    -- as a missing entry. This is the canary for encoding drift.
    local found = false
    for _, verb in ipairs(strings.TURN_COMPLETION_VERBS) do
      if verb == "Sautéed" then
        found = true
        break
      end
    end
    expect(found).to_be_truthy()
  end)

  it("includes Worked and Thinking which are the most common verbs in the wild", function()
    local has_worked, has_thinking = false, false
    for _, verb in ipairs(strings.TURN_COMPLETION_VERBS) do
      if verb == "Worked" then has_worked = true end
    end
    for _, verb in ipairs(strings.SPINNER_VERBS) do
      if verb == "Thinking" then has_thinking = true end
    end
    expect(has_worked).to_be_truthy()
    expect(has_thinking).to_be_truthy()
  end)

  it("includes Bash + Read + Edit + Write in TOOL_NAMES (the four most-rendered tools)", function()
    local required = { Bash = false, Read = false, Edit = false, Write = false }
    for _, name in ipairs(strings.TOOL_NAMES) do
      if required[name] ~= nil then required[name] = true end
    end
    for tool, present in pairs(required) do
      expect(present).to_be_truthy()
    end
  end)
end)
