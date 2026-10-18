-- Opens projects in Coder workspaces. `coder://dev/~/src/app` is a project
-- at `~/src/app` in the workspace `dev`, and `coder://dev.main/...` one in
-- the agent `main` of it. The app runs its VCS queries, terminals and
-- extension jobs there through `coder ssh`, which uses the login of the
-- `coder` CLI on this machine.
--
-- scriptschnell-term installs this extension turned off. Turn it on on the
-- Extensions page of the settings (Ctrl+,).

local settings = term.settings()

-- `coder ssh <flags> {target} -- {script}`: like ssh, coder hands the
-- command to the workspace's shell, so the script goes as one argument.
local function args(tty)
  local list = { "ssh", "--wait=" .. settings.wait }
  if not settings.autostart then
    table.insert(list, "--disable-autostart")
  end
  if settings.forward_agent then
    table.insert(list, "--forward-agent")
  end
  if tty then
    table.insert(list, "--tty")
  end
  for _, arg in ipairs({ "{target}", "--", "{script}" }) do
    table.insert(list, arg)
  end
  return list
end

term.environment{
  name = "coder",
  title = "Coder workspaces",
  program = "coder",
  args = args(false),
  pty_args = args(true),
}
