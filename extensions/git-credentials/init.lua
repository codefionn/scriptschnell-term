-- Gives sandboxed terminals your HTTPS logins for git hosts without the
-- tokens. ~/.git-credentials reaches the sandbox with fake tokens (it is
-- hidden by default, and the rewritten file stands in for it), and so do
-- GH_TOKEN, GITHUB_TOKEN and GITLAB_TOKEN; scriptschnell-term's proxy puts the real
-- token into requests to the host it belongs to. `git push` waits for you
-- through scriptschnell-term's own guard, whatever the host.
--
-- scriptschnell-term installs this extension and keeps it current. Grant its
-- permissions on the Extensions page of the settings (Ctrl+,).
-- Reload again after you change a login.

local home = os.getenv("HOME") or ""

local function read(path)
  local file = io.open(path, "r")
  if not file then
    return nil
  end
  local text = file:read("a")
  file:close()
  return text
end

local function lines(text)
  return (text .. "\n"):gmatch("([^\n]*)\n")
end

-- The lowercase host of a URL such as `sparse+https://user@host:1/x`.
local function url_host(url)
  local authority = url:match("^[%a][%w+.-]*://([^/?#]+)")
  if not authority then
    return nil
  end
  authority = authority:gsub("^.*@", ""):gsub(":%d*$", ""):gsub("^%[(.*)%]$", "%1")
  return authority:lower()
end


local function secret(value, host)
  return term.secret{ value = value, hosts = { host } }
end

-- Fakes for token variables that are set here.
local function token_vars(rules, names, hosts)
  for _, name in ipairs(names) do
    local value = os.getenv(name)
    if value and value ~= "" then
      rules.vars = rules.vars or {}
      rules.vars[name] = term.secret{ value = value, hosts = hosts }
    end
  end
end

local function rules()
  local rules = { files = {} }
  local path = home .. "/.git-credentials"
  local text = read(path)
  if text then
    local out = {}
    for line in lines(text) do
      -- https://user:token@host/path
      local head, value, tail = line:match("^(%a[%w+.-]*://[^:/@]*:)([^@]*)(@.*)$")
      local host = head and url_host(line)
      if host and value ~= "" then
        line = head .. secret(value, host) .. tail
      end
      out[#out + 1] = line
    end
    rules.files[1] = { sandbox = path, content = table.concat(out, "\n") }
  end
  token_vars(rules, { "GH_TOKEN", "GITHUB_TOKEN" }, { "github.com", "api.github.com" })
  token_vars(rules, { "GITLAB_TOKEN" }, { "gitlab.com" })
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("git-credentials: " .. tostring(error), "warning")
  end
end

apply()

-- Pick up changed logins when a worktree turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
  end
end)
