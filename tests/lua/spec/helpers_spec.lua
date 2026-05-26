-- Unit specs for plugins/claude-code/helpers.lua — the shared
-- string / hashing / redaction utilities.
--
-- helpers.lua is conceptually generic (nothing Claude-specific in
-- the function bodies), so the specs here only test the pure
-- behavior. main.lua-level parsers and structural indicators are
-- exercised through the fixture-driven matrix in
-- `tests/lua_classifier_tests.rs` instead — testing them here would
-- duplicate that surface against the same Lua code.

local helpers = helpers

describe("helpers.starts_with", function()
  it("matches exact and prefix cases", function()
    expect(helpers.starts_with("hello world", "hello")).to_be_truthy()
    expect(helpers.starts_with("hello", "hello")).to_be_truthy()
    expect(helpers.starts_with("hello", "world")).to_be_falsy()
  end)

  it("handles empty prefix and empty haystack", function()
    expect(helpers.starts_with("hello", "")).to_be_truthy()
    expect(helpers.starts_with("", "")).to_be_truthy()
    expect(helpers.starts_with("", "x")).to_be_falsy()
  end)
end)

describe("helpers.contains", function()
  it("uses plain (non-pattern) substring search", function()
    -- The `1, true` arg to string.find disables Lua's pattern syntax.
    -- A literal "." should match exactly that char, not "any char".
    expect(helpers.contains("a.b", ".")).to_be_truthy()
    expect(helpers.contains("axb", ".")).to_be_falsy()
  end)
end)

describe("helpers.contains_any", function()
  it("returns true when any needle matches", function()
    expect(helpers.contains_any("the quick brown fox", { "slow", "quick" })).to_be_truthy()
  end)

  it("returns false when none match", function()
    expect(helpers.contains_any("the quick brown fox", { "slow", "lazy" })).to_be_falsy()
  end)

  it("returns false on an empty needle list", function()
    expect(helpers.contains_any("anything", {})).to_be_falsy()
  end)
end)

describe("helpers.trim", function()
  it("strips leading and trailing whitespace", function()
    expect(helpers.trim("  hello world  ")).to_equal("hello world")
    expect(helpers.trim("\tfoo\n")).to_equal("foo")
  end)

  it("preserves internal whitespace", function()
    expect(helpers.trim("  a   b  ")).to_equal("a   b")
  end)
end)

describe("helpers.lower", function()
  it("lowercases ASCII content", function()
    expect(helpers.lower("HELLO")).to_equal("hello")
  end)

  it("tolerates nil input by returning empty string", function()
    expect(helpers.lower(nil)).to_equal("")
  end)
end)

describe("helpers.fnv1a_hex", function()
  it("is deterministic for the same input", function()
    expect(helpers.fnv1a_hex("dialog:permission:bash")).to_equal(
      helpers.fnv1a_hex("dialog:permission:bash")
    )
  end)

  it("produces different hashes for different inputs", function()
    local a = helpers.fnv1a_hex("dialog:permission:bash:ls -la")
    local b = helpers.fnv1a_hex("dialog:permission:bash:rm -rf")
    expect(a == b).to_be_falsy()
  end)

  it("returns 8 hex chars and zeroes for empty input", function()
    expect(helpers.fnv1a_hex("")).to_equal("00000000")
    expect(helpers.fnv1a_hex(nil)).to_equal("00000000")
    expect(#helpers.fnv1a_hex("anything")).to_equal(8)
  end)
end)

describe("helpers.redact_secret_patterns", function()
  it("redacts token=, password=, api_key= shaped pairs", function()
    expect(helpers.redact_secret_patterns("token=abc123")).to_equal("token=[REDACTED]")
    expect(helpers.redact_secret_patterns("Token=abc")).to_equal("Token=[REDACTED]")
    expect(helpers.redact_secret_patterns("api_key=foo")).to_equal("api_key=[REDACTED]")
    expect(helpers.redact_secret_patterns("api-key=bar")).to_equal("api-key=[REDACTED]")
  end)

  it("redacts Anthropic / OpenAI style API key prefixes", function()
    expect(helpers.redact_secret_patterns("sk-abcDEF12345")).to_equal("[REDACTED]")
    expect(helpers.redact_secret_patterns("sk-ant-api03-XYZ")).to_equal("[REDACTED]")
  end)

  it("redacts AWS access key ids", function()
    expect(helpers.redact_secret_patterns("AKIAEXAMPLE0123")).to_equal("[REDACTED]")
  end)

  it("is a no-op on empty or nil input", function()
    expect(helpers.redact_secret_patterns(nil)).to_be_nil()
    expect(helpers.redact_secret_patterns("")).to_equal("")
  end)

  it("does not over-match plain prose", function()
    expect(helpers.redact_secret_patterns("the token is somewhere")).to_equal(
      "the token is somewhere"
    )
  end)
end)

describe("helpers.strip_dollar", function()
  it("strips a single leading $", function()
    expect(helpers.strip_dollar("$0.10")).to_equal("0.10")
  end)

  it("is a no-op when no leading dollar", function()
    expect(helpers.strip_dollar("0.10")).to_equal("0.10")
  end)
end)
