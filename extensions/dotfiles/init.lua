-- Copies chosen dotfiles into devcontainers, VMs and remote machines when
-- they come up, so shells there feel like yours. Pick the files on the
-- extension's page of the settings.
--
-- scriptschnell-term installs this extension and keeps it current. Grant its
-- permissions on the Extensions page of the settings (Ctrl+,).

-- The settings come from manifest.toml; change them on the extension's
-- page of the settings.
local settings = term.settings()
local config = { kinds = {}, files = settings.files }
for _, kind in ipairs(settings.kinds) do
  config.kinds[kind] = true
end

local home = os.getenv("HOME") or ""

local function exists(path)
  local file = io.open(path, "r")
  if file then
    file:close()
    return true
  end
  return false
end

-- "a/b/c" -> "~/a/b", the directory the copy goes into.
local function dest_dir(relative)
  local dir = relative:match("^(.*)/[^/]+$")
  if dir then
    return "~/" .. dir
  end
  return "~"
end

local function copy_all(worktree)
  local count = 0
  for _, relative in ipairs(config.files) do
    local path = home .. "/" .. relative
    if exists(path) then
      term.copy{ from = path, to = dest_dir(relative), worktree = worktree }
      count = count + 1
    end
  end
  return count
end

term.on("environment_ready", function(e)
  if config.kinds[e.kind] then
    copy_all(e.worktree)
  end
end)

term.command{
  name = "dotfiles.copy",
  title = "Dotfiles: copy into this environment",
  run = function()
    local ctx = term.context()
    if ctx.worktree then
      term.notify("dotfiles: copying " .. copy_all(ctx.worktree) .. " paths")
    end
  end,
}
