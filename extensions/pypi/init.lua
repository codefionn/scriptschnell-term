-- Gives sandboxed terminals your PyPI logins without the passwords.
-- ~/.pypirc reaches the sandbox with fake passwords, and so does
-- TWINE_PASSWORD; scriptschnell-term's proxy puts the real password into uploads to
-- the repository it belongs to. Uploads to PyPI wait for you through
-- scriptschnell-term's own guard; this extension adds one for your other repositories.
-- pip's cache is under ~/.cache, which sandboxes may write by default.
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

local function append(list, ...)
  for _, item in ipairs({ ... }) do
    local known = false
    for _, have in ipairs(list) do
      if have == item then
        known = true
      end
    end
    if not known then
      list[#list + 1] = item
    end
  end
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
  local rules = {
    hosts = { "pypi.org", "files.pythonhosted.org", "upload.pypi.org" },
    files = {},
    guards = {},
  }
  local private = {}
  local path = home .. "/.pypirc"
  local text = read(path)
  if text then
    -- Find each section's repository first, then rewrite its password.
    local repositories = { pypi = "upload.pypi.org", testpypi = "test.pypi.org" }
    local section
    for line in lines(text) do
      section = line:match("^%s*%[([^%]]+)%]") or section
      local url = line:match("^%s*repository%s*[=:]%s*(%S+)")
      local host = url and url_host(url)
      if section and host then
        repositories[section] = host
        if not host:find("pypi%.org$") then
          append(private, host)
        end
      end
    end
    local out = {}
    section = nil
    for line in lines(text) do
      section = line:match("^%s*%[([^%]]+)%]") or section
      local head, value = line:match("^(%s*password%s*[=:]%s*)(.-)%s*$")
      if head and value ~= "" then
        local host = section and repositories[section]
        line = host and (head .. secret(value, host)) or head
      end
      out[#out + 1] = line
    end
    rules.files[1] = { sandbox = path, content = table.concat(out, "\n") }
  end
  local twine = os.getenv("TWINE_REPOSITORY_URL")
  token_vars(rules, { "TWINE_PASSWORD" }, { twine and url_host(twine) or "upload.pypi.org" })
  if #private > 0 then
    rules.guards[1] = {
      name = "upload",
      title = "upload to a private Python repository",
      hosts = private,
      requests = { "POST /*" },
    }
  end
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("pypi: " .. tostring(error), "warning")
  end
end

apply()

-- Pick up changed logins when a worktree turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
  end
end)
