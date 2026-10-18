-- Gives sandboxed terminals NuGet's package cache and your feed logins
-- without the passwords. NuGet.Config reaches the sandbox with fake
-- ClearTextPassword values, and scriptschnell-term's proxy puts the real password into
-- requests to the feed it belongs to. Encrypted passwords stay as they are,
-- since only Windows can read them. Pushing to nuget.org waits for you
-- through scriptschnell-term's own guard; this extension adds one for your other feeds.
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
    hosts = { "api.nuget.org", "www.nuget.org", "globalcdn.nuget.org" },
    writable = { "~/.nuget/packages", "~/.local/share/NuGet" },
    files = {},
    guards = {},
  }
  local private = {}
  for _, path in ipairs({ home .. "/.nuget/NuGet/NuGet.Config", home .. "/.config/NuGet/NuGet.Config" }) do
    local text = read(path)
    if text then
      -- Source names map to hosts; credentials sit in an element per name,
      -- with spaces written as _x0020_.
      local sources = {}
      local block = text:match("<packageSources>(.-)</packageSources>") or ""
      for key, value in block:gmatch('<add%s+key="([^"]*)"%s+value="([^"]*)"') do
        local host = url_host(value)
        sources[key:gsub(" ", "_x0020_")] = host
        if host and not host:find("nuget%.org$") then
          append(private, host)
        end
      end
      local function source(open, name, body, close)
        local host = sources[name]
        if not host then
          -- No host to send it to: the sandbox gets no password at all.
          return open .. body:gsub('(key="ClearTextPassword"%s+value=")[^"]*', "%1") .. close
        end
        body = body:gsub('(key="ClearTextPassword"%s+value=")([^"]*)', function(head, value)
          return head .. secret(value, host)
        end)
        return open .. body .. close
      end
      local content = text:gsub(
        "(<packageSourceCredentials>)(.-)(</packageSourceCredentials>)",
        function(open, credentials, close)
          credentials = credentials:gsub("(<([%w_.-]+)>)(.-)(</%2>)", source)
          return open .. credentials .. close
        end
      )
      rules.files[#rules.files + 1] = { sandbox = path, content = content }
    end
  end
  if #private > 0 then
    rules.guards[1] = {
      name = "push",
      title = "push to a private NuGet feed",
      hosts = private,
      requests = { "PUT */api/v2/package*", "DELETE */api/v2/package/*", "PUT */nuget/v2*" },
    }
  end
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("nuget: " .. tostring(error), "warning")
  end
end

apply()

-- Pick up changed logins when a worktree turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
  end
end)
