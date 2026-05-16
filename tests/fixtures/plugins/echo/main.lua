-- Trivial trusted-local plugin used by tests/cli_tests.rs to exercise
-- third-party plugin loading via `--plugin <manifest.toml>` and the
-- `plugin.load` / `plugin.unload` JSON-RPC methods. Not application-specific
-- in any way -- just enough to satisfy the Extension trait.

local M = {}

function M.classify(_ctx)
  return {
    state = "ready",
    confidence = 1.0,
    evidence = "echo plugin is always ready",
  }
end

function M.send_prompt(input)
  return {
    actions = {
      ptywright.action.text(input.prompt or ""),
      ptywright.action.key("enter"),
    },
    last_intent = "prompt_submitted",
  }
end

function M.wait_turn_matcher(_input)
  return ptywright.matcher.screen_stable(150)
end

return M
