-- Gives sandboxed terminals npm's cache and your npm logins without the
-- tokens. ~/.npmrc (or NPM_CONFIG_USERCONFIG) reaches the sandbox with fake
-- tokens, and so do NPM_TOKEN and NODE_AUTH_TOKEN; scriptschnell-term's proxy puts the
-- real token into requests to the registry it belongs to. Publishing to
-- registry.npmjs.org waits for you through scriptschnell-term's own guard; this
-- extension adds one for the other registries your .npmrc names.
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

local B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

local function b64encode(data)
  local out = {}
  for i = 1, #data, 3 do
    local a, b, c = data:byte(i, i + 2)
    local n = (a << 16) | ((b or 0) << 8) | (c or 0)
    for j = 3, 0, -1 do
      local k = (n >> (j * 6)) & 63
      out[#out + 1] = B64:sub(k + 1, k + 1)
    end
    if not b then
      out[#out], out[#out - 1] = "=", "="
    elseif not c then
      out[#out] = "="
    end
  end
  return table.concat(out)
end

local function b64decode(text)
  text = text:gsub("[^%w+/]", "")
  local out = {}
  local bits, count = 0, 0
  for i = 1, #text do
    bits = (bits << 6) | (B64:find(text:sub(i, i), 1, true) - 1)
    count = count + 6
    if count >= 8 then
      count = count - 8
      out[#out + 1] = string.char((bits >> count) & 255)
    end
  end
  return table.concat(out)
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
  local rules = { hosts = { "registry.npmjs.org" }, writable = { "~/.npm" }, files = {}, guards = {} }
  local private = {}
  local path = os.getenv("NPM_CONFIG_USERCONFIG") or (home .. "/.npmrc")
  local text = read(path)
  if text then
    local default_host = "registry.npmjs.org"
    for line in lines(text) do
      local reg = line:match("^%s*registry%s*=%s*https?://([^/:%s]+)")
      if reg then
        default_host = reg:lower()
      end
    end
    local out = {}
    for line in lines(text) do
      -- //registry.example.com/path/:_authToken=VALUE
      local head, host, key, value = line:match("^(%s*//([^/:]+)[^=]*:(_%a+)%s*=%s*)(.-)%s*$")
      if not head then
        -- Unscoped _authToken=VALUE, _auth=VALUE or _password=VALUE for the default registry.
        local uhead, ukey, uvalue = line:match("^(%s*(_%a+)%s*=%s*)(.-)%s*$")
        if uhead and (ukey == "_authToken" or ukey == "_auth" or ukey == "_password") then
          head, host, key, value = uhead, default_host, ukey, uvalue
        end
      end
      if head then
        host = host:lower()
        if host ~= "registry.npmjs.org" then
          append(private, host)
        end
      end
      if head and value ~= "" and not value:find("${", 1, true) then
        if key == "_password" then
          -- npm sends base64(user:password); the proxy decodes and swaps it.
          line = head .. b64encode(secret(b64decode(value), host))
        elseif key == "_authToken" or key == "_auth" then
          line = head .. secret(value, host)
        end
      end
      out[#out + 1] = line
    end
    rules.files[1] = { sandbox = path, content = table.concat(out, "\n") }
  end
  token_vars(rules, { "NPM_TOKEN", "NODE_AUTH_TOKEN" }, { "registry.npmjs.org" })
  if #private > 0 then
    rules.guards[1] = {
      name = "publish",
      title = "publish to a private npm registry",
      hosts = private,
      requests = { "PUT /*", "DELETE /*" },
    }
  end
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("npm: " .. tostring(error), "warning")
  end
end

apply()

-- Pick up changed logins when a worktree turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
  end
end)
