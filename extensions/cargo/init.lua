-- Gives sandboxed terminals Cargo's registry cache and your Cargo logins
-- without the tokens. credentials.toml reaches the sandbox with fake
-- tokens, and so does CARGO_REGISTRY_TOKEN; scriptschnell-term's proxy puts the real
-- token into requests to the registry it belongs to. A token for a
-- registry whose index host is unknown reaches the sandbox empty.
-- `cargo publish` to crates.io waits for you through scriptschnell-term's own guard;
-- this extension adds one for the other registries in Cargo's config.
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
  local cargo_home = os.getenv("CARGO_HOME") or (home .. "/.cargo")
  local rules = {
    hosts = { "crates.io", "index.crates.io", "static.crates.io" },
    writable = { cargo_home .. "/registry", cargo_home .. "/git" },
    files = {},
    guards = {},
  }
  -- Hosts of the other registries, from their index URLs.
  local registry_hosts = { [""] = "crates.io" }
  local private = {}
  local config = read(cargo_home .. "/config.toml") or read(cargo_home .. "/config") or ""
  local section
  for line in lines(config) do
    section = line:match("^%s*%[registries%.([%w_-]+)%]") or (line:match("^%s*%[") and "?") or section
    local index = line:match('^%s*index%s*=%s*"([^"]+)"')
    local host = index and url_host(index)
    if section and section ~= "?" and host then
      registry_hosts[section] = host
      append(private, host)
    end
  end
  for _, name in ipairs({ "credentials.toml", "credentials" }) do
    local path = cargo_home .. "/" .. name
    local text = read(path)
    if text then
      local out = {}
      section = nil
      for line in lines(text) do
        if line:match("^%s*%[registry%]") then
          section = ""
        elseif line:match("^%s*%[registries%.([%w_-]+)%]") then
          section = line:match("^%s*%[registries%.([%w_-]+)%]")
        elseif line:match("^%s*%[") then
          section = nil
        end
        local head, value = line:match('^(%s*token%s*=%s*")([^"]*)"')
        if head and section then
          local host = registry_hosts[section]
          if host then
            line = head .. secret(value, host) .. '"'
          else
            -- No host to send it to: the sandbox gets no token at all.
            line = head .. '"'
          end
        end
        out[#out + 1] = line
      end
      rules.files[#rules.files + 1] = { sandbox = path, content = table.concat(out, "\n") }
    end
  end
  token_vars(rules, { "CARGO_REGISTRY_TOKEN" }, { "crates.io" })
  if #private > 0 then
    rules.guards[1] = {
      name = "publish",
      title = "publish to a private Cargo registry",
      hosts = private,
      requests = {
        "PUT */api/v1/crates/new",
        "DELETE */api/v1/crates/*/yank",
        "PUT */api/v1/crates/*/unyank",
        "PUT */api/v1/crates/*/owners",
        "DELETE */api/v1/crates/*/owners",
      },
    }
  end
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("cargo: " .. tostring(error), "warning")
  end
end

apply()

-- Pick up changed logins when a worktree turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
  end
end)
