-- Gives devcontainers this machine's ASP.NET Core HTTPS development
-- certificate. Kestrel in the container then serves the same certificate
-- your browser here trusts, and programs in the container trust it too.
-- Works the same on Windows, macOS and Linux hosts, with any Linux image
-- that has the .NET SDK.
--
-- The steps run one after the other through `done` functions:
--   1. Check that the container has `dotnet`; containers without it are left alone.
--   2. On this machine: create the certificate if needed (`dotnet dev-certs https`)
--      and, unless turned off, trust it here (`--trust`).
--   3. Export it here as a PFX with a one-time password and as a PEM certificate.
--   4. Copy the PFX into the container and import it there with
--      `dotnet dev-certs https --clean --import`, which Kestrel picks up.
--   5. Add the PEM certificate to the container's system trust store (`term.trust`).
-- The exports are deleted as soon as the next step has them.
--
-- scriptschnell-term installs this extension turned off. Turn it on on the
-- Extensions page of the settings (Ctrl+,).

local settings = term.settings()
local sep = package.config:sub(1, 1)
local busy = {}

local function join(dir, name)
  return dir .. sep .. name
end

local function warn(message)
  term.notify("aspnet-devcerts: " .. message, "warning")
end

-- Why a step failed, in one line.
local function reason(r)
  if r.error then
    return r.error
  end
  local last
  for line in (r.stderr ~= "" and r.stderr or r.stdout):gmatch("[^\r\n]+") do
    if line:find("%S") then
      last = line
    end
  end
  return last or ("exit code " .. tostring(r.code))
end

-- A password for the PFX that lives for a few seconds.
local function password()
  local chars = "abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789"
  local out = {}
  for i = 1, 32 do
    local n = math.random(#chars)
    out[i] = chars:sub(n, n)
  end
  return table.concat(out)
end

-- Imports the copied PFX for the container's user and deletes it.
local IMPORT = [[
pfx="$HOME/.aspnet/https/cmdterm-aspnetcore.pfx"
trap 'rm -f "$pfx"' EXIT
dotnet dev-certs https --clean --import "$pfx" --password "$1"
]]

local function share(worktree, quiet)
  if busy[worktree] then
    return
  end
  busy[worktree] = true
  local dir = term.cache_dir()
  local pfx = join(dir, "cmdterm-aspnetcore.pfx")
  local crt = join(dir, "cmdterm-aspnetcore.crt")
  local secret = password()

  local function finish(message, level)
    busy[worktree] = nil
    os.remove(pfx)
    os.remove(crt)
    os.remove(join(dir, "cmdterm-aspnetcore.key"))
    if message then
      term.notify("aspnet-devcerts: " .. message, level)
    end
  end

  local function host(cmd, done)
    term.run{ cmd = cmd, host = true, worktree = worktree, done = done }
  end

  local function trust_inside()
    if not settings.trust_container then
      return finish("the devcontainer serves this machine's dev certificate")
    end
    term.trust{ name = "aspnetcore-dev", path = crt, worktree = worktree, done = function(r)
      if not r.ok then
        return finish("the devcontainer serves the dev certificate, but trusting it there failed: "
          .. reason(r), "warning")
      end
      finish("the devcontainer serves and trusts this machine's dev certificate")
    end }
  end

  local function import()
    term.copy{ from = pfx, to = "~/.aspnet/https", worktree = worktree, done = function(r)
      os.remove(pfx)
      if not r.ok then
        return finish("copying the certificate into the devcontainer failed: " .. reason(r), "warning")
      end
      term.run{ cmd = { "sh", "-c", IMPORT, "sh", secret }, worktree = worktree, done = function(r)
        if not r.ok then
          return finish("importing the certificate in the devcontainer failed: " .. reason(r), "warning")
        end
        trust_inside()
      end }
    end }
  end

  local function export()
    host({ "dotnet", "dev-certs", "https", "--export-path", pfx, "--password", secret }, function(r)
      if not r.ok then
        return finish("exporting the dev certificate failed: " .. reason(r), "warning")
      end
      host({ "dotnet", "dev-certs", "https", "--export-path", crt, "--format", "PEM", "--no-password" }, function(r)
        -- The PEM export writes the private key next to it; only the PFX carries it on.
        os.remove(join(dir, "cmdterm-aspnetcore.key"))
        if not r.ok then
          return finish("exporting the dev certificate as PEM failed: " .. reason(r), "warning")
        end
        import()
      end)
    end)
  end

  term.run{ cmd = { "sh", "-c", "command -v dotnet" }, worktree = worktree, done = function(r)
    if not r.ok then
      return finish(not quiet and "the devcontainer has no dotnet" or nil, "warning")
    end
    local create = { "dotnet", "dev-certs", "https" }
    if settings.trust_host then
      create[#create + 1] = "--trust"
    end
    host(create, function(r)
      if r.code == nil then
        return finish("this machine has no dotnet: " .. reason(r), "warning")
      end
      if not r.ok then
        -- `--trust` fails where this machine's store needs a password or root,
        -- such as Linux without sudo. The certificate exists anyway.
        warn("trusting the dev certificate on this machine failed: " .. reason(r))
      end
      export()
    end)
  end }
end

term.on("environment_ready", function(e)
  if e.kind == "devcontainer" and e.host == "local" then
    share(e.worktree, true)
  end
end)

term.command{
  name = "aspnet-devcerts.share",
  title = "ASP.NET Core: share the dev certificate with this devcontainer",
  run = function()
    local ctx = term.context()
    if ctx.runtime ~= "devcontainer" or ctx.env ~= "local" then
      warn("the active worktree runs in no devcontainer on this machine")
      return
    end
    share(ctx.worktree, false)
  end,
}
