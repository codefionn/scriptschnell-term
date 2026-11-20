-- Claude Code in devcontainers, VMs and remote machines.
--
-- When a worktree's environment comes up, this copies your Claude Code
-- setup into it (settings, CLAUDE.md, agents, commands, skills, hooks,
-- plugins) and installs Claude Code there if it is missing. The palette
-- command "Claude Code: set up this environment" does the same on demand.
--
-- scriptschnell-term installs this extension and keeps it current. Grant its
-- permissions on the Extensions page of the settings (Ctrl+,).
--
-- Your login is NOT copied unless you turn on "Copy login credentials".
-- Copying it gives everything that runs in the environment your Claude
-- account. Without it, run `claude` once inside and log in there.

-- The settings come from manifest.toml; change them on the extension's
-- page of the settings.
local settings = term.settings()
local config = {
  kinds = {},
  copy = settings.copy,
  -- Never copied: caches, history and per-machine state.
  exclude = { "cache", "*.log", "*.lock" },
  credentials = settings.credentials,
  install = settings.install,
}
for _, kind in ipairs(settings.kinds) do
  config.kinds[kind] = true
end

local home = os.getenv("HOME") or ""
local source = home .. "/.claude"

local function exists(path)
  local file = io.open(path, "r")
  if file then
    file:close()
    return true
  end
  return false
end

local function set_up(worktree)
  if not exists(source) then
    term.notify("claude-code: no ~/.claude to copy", "warning")
    return
  end
  local copied = 0
  for _, name in ipairs(config.copy) do
    local path = source .. "/" .. name
    if exists(path) then
      term.copy{ from = path, to = "~/.claude", worktree = worktree, exclude = config.exclude }
      copied = copied + 1
    end
  end
  if config.credentials and exists(source .. "/.credentials.json") then
    term.copy{ from = source .. "/.credentials.json", to = "~/.claude", worktree = worktree }
  end
  if config.install then
    term.run{
      cmd = {
        "sh", "-c",
        "command -v claude >/dev/null 2>&1 || [ -x \"$HOME/.local/bin/claude\" ] "
          .. "|| curl -fsSL https://claude.ai/install.sh | bash",
      },
      worktree = worktree,
    }
  end
  term.notify("claude-code: copying " .. copied .. " entries of ~/.claude")
end

term.on("environment_ready", function(e)
  if config.kinds[e.kind] then
    set_up(e.worktree)
  end
end)

term.command{
  name = "claude-code.setup",
  title = "Claude Code: set up this environment",
  run = function()
    local ctx = term.context()
    if not ctx.worktree then
      term.notify("claude-code: no worktree selected", "error")
      return
    end
    if ctx.runtime == "sandbox" then
      term.notify(
        "claude-code: a sandbox already uses your ~/.claude and ~/.claude.json",
        "info"
      )
      return
    end
    set_up(ctx.worktree)
  end,
}
