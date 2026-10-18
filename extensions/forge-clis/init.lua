-- Gives sandboxed terminals the logins of the forge command-line tools
-- without their tokens: fj (forgejo-cli), tea, glab and gh. Each tool's
-- login file reaches the sandbox with fake tokens, and scriptschnell-term's
-- proxy puts the real token into requests to the host it belongs to.
--
-- OAuth logins of fj, glab and tea expire. The sandbox's copies say they
-- never do and hold no refresh token, so the tools there never try to
-- refresh one. Before a login expires, this extension marks it expired in
-- the tool's file on this machine and runs the tool here, which refreshes
-- it, and the fake keeps standing for the new token. gh keeps its tokens
-- in the system's keyring; this extension asks `gh auth token` on this
-- machine for them.
--
-- scriptschnell-term installs this extension and keeps it current. Grant its
-- permissions on the Extensions page of the settings (Ctrl+,).

local home = os.getenv("HOME") or ""

-- Refresh OAuth logins that expire within this many seconds.
local MARGIN = 20 * 60

-- What stands in for refresh tokens. Nothing in a sandbox refreshes.
local NO_REFRESH = "refreshed-by-scriptschnell-term-outside-the-sandbox"

-- Expiry dates of the sandbox's copies, and of logins marked expired here.
local NEVER_RFC3339 = "9999-12-31T23:59:59Z"
local NEVER_UNIX = "253402300799"
local EXPIRED_RFC3339 = "1970-01-01T00:00:00Z"

local function read(path)
  local file = io.open(path, "r")
  if not file then
    return nil
  end
  local text = file:read("a")
  file:close()
  return text
end

-- Replaces the content of `path` in place, so the file keeps its
-- permissions.
local function write(path, text)
  local file, problem = io.open(path, "w")
  if not file then
    error(tostring(problem), 0)
  end
  file:write(text)
  file:close()
end

local function lines(text)
  return (text .. "\n"):gmatch("([^\n]*)\n")
end

local function trim(text)
  return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function dir(variable, fallback)
  local value = os.getenv(variable)
  if value and value ~= "" then
    return value
  end
  return home .. "/" .. fallback
end

local function append(list, item)
  for _, have in ipairs(list) do
    if have == item then
      return
    end
  end
  list[#list + 1] = item
end

-- The lowercase host of a URL such as `https://user@host:1/x`.
local function url_host(url)
  local authority = url:match("^[%a][%w+.-]*://([^/?#]+)")
  if not authority then
    return nil
  end
  authority = authority:gsub("^.*@", ""):gsub(":%d*$", ""):gsub("^%[(.*)%]$", "%1")
  return authority:lower()
end

-- Days from 1970-01-01 to the date `year`-`month`-`day`.
local function days_from_civil(year, month, day)
  local y = month <= 2 and year - 1 or year
  local era = (y >= 0 and y or y - 399) // 400
  local yoe = y - era * 400
  local doy = (153 * (month + (month > 2 and -3 or 9)) + 2) // 5 + day - 1
  return era * 146097 + yoe * 365 + yoe // 4 - yoe // 100 + doy - 719468
end

-- Seconds since 1970 of fj's `expires_at`, the `time` crate's
-- [year, day of year, hour, minute, second, nanosecond, offset h, m, s].
local function fj_expiry(at)
  if type(at) ~= "table" or #at < 5 then
    return nil
  end
  local days = days_from_civil(at[1], 1, 1) + at[2] - 1
  local offset = (at[7] or 0) * 3600 + (at[8] or 0) * 60 + (at[9] or 0)
  return days * 86400 + at[3] * 3600 + at[4] * 60 + at[5] - offset
end

-- Seconds since 1970 of an RFC 3339 time such as
-- `2026-10-30T09:07:09.5+01:00`, or nil.
local function rfc3339(text)
  local y, mo, d, h, mi, s, zone =
    text:match("^(%d%d%d%d)-(%d%d)-(%d%d)[Tt ](%d%d):(%d%d):(%d%d)%.?%d*(.*)$")
  if not y then
    return nil
  end
  local offset
  if zone == "Z" or zone == "z" then
    offset = 0
  else
    local sign, oh, om = zone:match("^([+-])(%d%d):?(%d%d)$")
    if not sign then
      return nil
    end
    offset = (tonumber(oh) * 3600 + tonumber(om) * 60) * (sign == "-" and -1 or 1)
  end
  local days = days_from_civil(tonumber(y), tonumber(mo), tonumber(d))
  return days * 86400 + tonumber(h) * 3600 + tonumber(mi) * 60 + tonumber(s) - offset
end

local function fj_path()
  return dir("XDG_DATA_HOME", ".local/share") .. "/forgejo-cli/keys.json"
end

-- fj's logins, or nil without a keys.json.
local function fj_keys()
  local text = read(fj_path())
  if not text then
    return nil
  end
  local keys, problem = term.json_decode(text)
  if not keys then
    error("keys.json: " .. tostring(problem), 0)
  end
  return keys
end

-- keys.json for `keys`. An empty table encodes as an array; `hosts` and
-- `aliases` are objects.
local function fj_encode(keys)
  return (term.json_encode(keys):gsub('"(hosts)":%[%]', '"%1":{}'):gsub('"(aliases)":%[%]', '"%1":{}'))
end

-- Marks the OAuth login of `host` in this machine's keys.json as expired.
-- fj refreshes a login only once it has expired, so this makes the next
-- `fj whoami` refresh it now. True when the login is there.
local function fj_expire(host)
  local keys = fj_keys()
  local login = type(keys) == "table" and type(keys.hosts) == "table" and keys.hosts[host]
  if type(login) ~= "table" or login.expires_at == nil then
    return false
  end
  login.expires_at = { 1970, 1, 0, 0, 0, 0, 0, 0, 0 }
  write(fj_path(), fj_encode(keys))
  return true
end

-- fj: keys.json with fake tokens and OAuth logins that never expire.
local function fj(rules)
  local path = fj_path()
  local keys = fj_keys()
  if type(keys) ~= "table" or type(keys.hosts) ~= "table" then
    return
  end
  local aliases = type(keys.aliases) == "table" and keys.aliases or {}
  for host, login in pairs(keys.hosts) do
    if type(login) == "table" and type(login.token) == "string" and login.token ~= "" then
      local hosts = { host:lower() }
      if type(aliases[host]) == "string" then
        append(hosts, aliases[host]:lower())
      end
      for _, name in ipairs(hosts) do
        append(rules.hosts, name)
      end
      login.token = term.secret{ value = login.token, hosts = hosts, name = "fj:" .. host }
      if login.refresh_token ~= nil then
        login.refresh_token = NO_REFRESH
      end
      if login.expires_at ~= nil then
        login.expires_at = { 9999, 1, 0, 0, 0, 0, 0, 0, 0 }
      end
    end
  end
  rules.files[#rules.files + 1] = { sandbox = path, content = fj_encode(keys) }
end

-- A YAML scalar without its quotes, and the quotes.
local function unquote(value)
  local quote, inner = value:match("^(['\"])(.*)%1$")
  if quote then
    return inner, quote
  end
  return value, ""
end

-- Rewrites `key: value` lines of a YAML file. `visit(state, indent, key,
-- value)` sees every mapping line and returns a new value or nil.
local function rewrite_yaml(text, visit)
  local out, state = {}, {}
  for line in lines(text) do
    local indent, dash, key, sep, value = line:match("^(%s*)(%-?%s*)([%w_.-]+)(:%s*)(.-)%s*$")
    if key and not line:match("^%s*#") then
      local raw, quote = unquote(value)
      local new = visit(state, #indent + #dash, key, raw, dash ~= "")
      if new then
        line = indent .. dash .. key .. sep .. quote .. new .. quote
      end
    end
    out[#out + 1] = line
  end
  return table.concat(out, "\n")
end

local function glab_path()
  local config = os.getenv("GLAB_CONFIG_DIR")
  if not config or config == "" then
    config = dir("XDG_CONFIG_HOME", ".config") .. "/glab-cli"
  end
  return config .. "/config.yml"
end

-- For `rewrite_yaml` over glab's config.yml: true for a line inside a host
-- of `hosts`; `state.host` is that host, in lowercase.
local function glab_track(state, indent, key, value)
  if indent == 0 then
    state.in_hosts = key == "hosts"
    state.host = nil
    return false
  end
  if not state.in_hosts then
    return false
  end
  if value == "" and (not state.host_indent or indent <= state.host_indent) then
    state.host_indent = indent
    state.host = key:lower()
    return false
  end
  return state.host ~= nil and indent > state.host_indent
end

-- glab's OAuth logins that can refresh: `{ host, expiry }`, with the host
-- as config.yml spells it and nil for an expiry it cannot read.
local function glab_logins()
  local text = read(glab_path())
  if not text then
    return {}
  end
  local found, order = {}, {}
  rewrite_yaml(text, function(state, indent, key, value)
    if glab_track(state, indent, key, value) and value ~= "" then
      local login = found[state.host]
      if not login then
        login = { host = state.host }
        found[state.host] = login
        order[#order + 1] = login
      end
      if key == "oauth2_refresh_token" then
        login.refresh = true
      elseif key == "oauth2_expiry_date" then
        login.expiry = rfc3339(value) or false
      end
    end
  end)
  local logins = {}
  for _, login in ipairs(order) do
    if login.refresh and login.expiry ~= nil then
      logins[#logins + 1] = { host = login.host, expiry = login.expiry or nil }
    end
  end
  return logins
end

-- Marks the OAuth login of `host` in this machine's config.yml as expired.
-- glab refreshes a login only within 5 minutes of its expiry, so this makes
-- the next request refresh it now. True when the login is there.
local function glab_expire(host)
  local path = glab_path()
  local text = read(path)
  if not text then
    return false
  end
  local marked = false
  local content = rewrite_yaml(text, function(state, indent, key, value)
    if glab_track(state, indent, key, value) and state.host == host
      and key == "oauth2_expiry_date" and value ~= "" then
      marked = true
      return EXPIRED_RFC3339
    end
  end)
  if marked then
    write(path, content)
  end
  return marked
end

-- glab: config.yml, whose `hosts` map holds a token per host.
local function glab(rules)
  local path = glab_path()
  local text = read(path)
  if not text then
    return
  end
  -- The api_host of each host comes after its token, so look first.
  local api_hosts = {}
  rewrite_yaml(text, function(state, indent, key, value)
    if glab_track(state, indent, key, value) and key == "api_host" and value ~= "" then
      api_hosts[state.host] = value:lower():gsub(":%d+$", "")
    end
  end)
  local content = rewrite_yaml(text, function(state, indent, key, value)
    if not glab_track(state, indent, key, value) or value == "" then
      return nil
    end
    if key == "token" then
      local hosts = { state.host }
      append(rules.hosts, state.host)
      if api_hosts[state.host] then
        append(hosts, api_hosts[state.host])
        append(rules.hosts, api_hosts[state.host])
      end
      return term.secret{ value = value, hosts = hosts, name = "glab:" .. state.host }
    elseif key == "oauth2_refresh_token" then
      return NO_REFRESH
    elseif key == "oauth2_expiry_date" then
      return NEVER_RFC3339
    end
  end)
  rules.files[#rules.files + 1] = { sandbox = path, content = content }
end

local function tea_dir()
  return dir("XDG_CONFIG_HOME", ".config") .. "/tea"
end

-- For `rewrite_yaml` over tea's config.yml: the index of the login a line
-- belongs to, or nil outside `logins`.
local function tea_login(state, indent, key, item)
  if indent == 0 then
    state.logins = key == "logins"
    return nil
  end
  if state.logins and item then
    state.index = (state.index or 0) + 1
  end
  return state.logins and state.index or nil
end

-- tea's logins by index: `{ name, host, refresh, expiry }`. A login's
-- url may follow its token, so this reads the whole file first.
local function tea_logins(text)
  local logins = {}
  rewrite_yaml(text, function(state, indent, key, value, item)
    local index = tea_login(state, indent, key, item)
    if not index or value == "" then
      return
    end
    local login = logins[index] or {}
    logins[index] = login
    if key == "url" then
      login.host = url_host(value)
    elseif key == "name" then
      login.name = value
    elseif key == "refresh_token" then
      login.refresh = true
    elseif key == "token_expiry" then
      local expiry = tonumber(value)
      login.expiry = expiry and expiry > 0 and expiry or nil
    end
  end)
  return logins
end

-- Marks the login `name` in this machine's config.yml as expired. tea
-- refreshes a login only within 5 minutes of its expiry, so this makes the
-- next request refresh it now. True when the login is there.
local function tea_expire(name)
  local path = tea_dir() .. "/config.yml"
  local text = read(path)
  if not text then
    return false
  end
  local logins = tea_logins(text)
  local marked = false
  local content = rewrite_yaml(text, function(state, indent, key, value, item)
    local index = tea_login(state, indent, key, item)
    if index and logins[index].name == name and key == "token_expiry" and value ~= "" then
      marked = true
      return "1"
    end
  end)
  if marked then
    write(path, content)
  end
  return marked
end

-- tea: config.yml, a list of logins with a url and a token each. Newer
-- tea keeps OAuth logins in credentials.json or the system's keyring,
-- which a fake cannot stand in for; the sandbox does not see that file.
local function tea(rules)
  local path = tea_dir() .. "/config.yml"
  append(rules.hidden, tea_dir() .. "/credentials.json")
  local text = read(path)
  if not text then
    return
  end
  local logins = tea_logins(text)
  local content = rewrite_yaml(text, function(state, indent, key, value, item)
    local index = tea_login(state, indent, key, item)
    local host = index and logins[index].host
    if not host or value == "" then
      return nil
    end
    if key == "token" then
      append(rules.hosts, host)
      return term.secret{ value = value, hosts = { host }, name = "tea:" .. index }
    elseif key == "refresh_token" then
      return NO_REFRESH
    elseif key == "token_expiry" and value ~= "0" then
      return NEVER_UNIX
    end
  end)
  rules.files[#rules.files + 1] = { sandbox = path, content = content }
end

local function gh_path()
  local config = os.getenv("GH_CONFIG_DIR")
  if not config or config == "" then
    config = dir("XDG_CONFIG_HOME", ".config") .. "/gh"
  end
  return config .. "/hosts.yml"
end

-- Tokens `gh auth token` printed, by host.
local gh_tokens = {}

-- The hosts of gh's hosts.yml.
local function gh_hosts()
  local text = read(gh_path())
  local hosts = {}
  if text then
    for line in lines(text) do
      local host = line:match("^([%w.-]+):%s*$")
      if host then
        hosts[#hosts + 1] = host:lower()
      end
    end
  end
  return hosts
end

-- gh: hosts.yml with a fake `oauth_token` for every host gh has one for.
-- The directory is hidden by default; this file shows through.
local function gh(rules)
  local path = gh_path()
  local text = read(path)
  if not text then
    return
  end
  local function hosts_of(host)
    if host == "github.com" then
      return { "github.com", "api.github.com", "uploads.github.com" }
    end
    return { host }
  end
  local out, host, has_token = {}, nil, {}
  local function close()
    local token = host and gh_tokens[host]
    if token and not has_token[host] then
      out[#out + 1] = "    oauth_token: "
        .. term.secret{ value = token, hosts = hosts_of(host), name = "gh:" .. host }
    end
  end
  for line in lines(text) do
    local top = line:match("^([%w.-]+):%s*$")
    if top then
      close()
      host = top:lower()
      for _, name in ipairs(hosts_of(host)) do
        append(rules.hosts, name)
      end
    elseif host then
      local head, value = line:match("^(%s+oauth_token:%s*)(.-)%s*$")
      local raw, quote = unquote(value or "")
      if head and raw ~= "" then
        has_token[host] = true
        local fake = term.secret{ value = raw, hosts = hosts_of(host), name = "gh:" .. host .. ":" .. #out }
        line = head .. quote .. fake .. quote
      end
    end
    if line ~= "" then
      out[#out + 1] = line
    end
  end
  close()
  rules.files[#rules.files + 1] = { sandbox = path, content = table.concat(out, "\n") .. "\n" }
end

local function rules()
  local rules = { hosts = {}, files = {}, hidden = {} }
  for name, tool in pairs({ fj = fj, glab = glab, tea = tea, gh = gh }) do
    local ok, error = pcall(tool, rules)
    if not ok then
      term.notify("forge-clis: " .. name .. ": " .. tostring(error), "warning")
    end
  end
  table.sort(rules.files, function(a, b)
    return a.sandbox < b.sandbox
  end)
  table.sort(rules.hosts)
  return rules
end

local function apply()
  local ok, error = pcall(function()
    term.sandbox(rules())
  end)
  if not ok then
    term.notify("forge-clis: " .. tostring(error), "warning")
  end
end

-- Logins whose refresh failed, so the notice comes once.
local warned = {}

-- Logins whose refresh still runs. A second refresh with the same refresh
-- token fails with "token was already used", and the forge may then drop
-- the login.
local refreshing = {}

-- Refreshes the login `id` on this machine when it expires within MARGIN:
-- `expire()` marks it expired in the tool's file, then `cmd` makes the
-- tool refresh it. `login` is what to run when that fails.
local function refresh_login(worktree, id, expiry, expire, cmd, login)
  if not expiry or expiry - os.time() >= MARGIN or refreshing[id] then
    return
  end
  local ok, marked = pcall(expire)
  if not ok then
    if not warned[id] then
      warned[id] = true
      term.notify("forge-clis: " .. tostring(marked), "warning")
    end
    return
  end
  if not marked then
    return
  end
  refreshing[id] = true
  term.run{
    cmd = cmd,
    host = true,
    worktree = worktree,
    done = function(r)
      refreshing[id] = nil
      if r.ok then
        warned[id] = nil
        apply()
      elseif r.code and not warned[id] then
        warned[id] = true
        term.notify(
          "forge-clis: " .. cmd[1] .. " could not refresh its login for " .. id:gsub("^%w+:", "")
            .. ", so sandboxes cannot use it; run " .. login .. " on this machine",
          "warning"
        )
      end
    end,
  }
end

-- Runs the tools on this machine: fj, glab and tea for OAuth logins that
-- are about to expire, after marking them expired, which refreshes them,
-- and `gh auth token` for gh's keyring logins, which it asks for again
-- only with `again`. They run for `worktree`, which must be of a project
-- on this machine.
local function refresh(worktree, again)
  local ok, keys = pcall(fj_keys)
  if ok and type(keys) == "table" and type(keys.hosts) == "table" then
    for host, login in pairs(keys.hosts) do
      local url = "https://" .. host
      refresh_login(
        worktree,
        "fj:" .. host,
        type(login) == "table" and fj_expiry(login.expires_at),
        function()
          return fj_expire(host)
        end,
        { "fj", "whoami", "--host", url },
        "fj auth login --host " .. url
      )
    end
  end
  local ok, logins = pcall(glab_logins)
  for _, login in ipairs(ok and logins or {}) do
    refresh_login(
      worktree,
      "glab:" .. login.host,
      login.expiry,
      function()
        return glab_expire(login.host)
      end,
      { "glab", "api", "user", "--hostname", login.host },
      "glab auth login --hostname " .. login.host
    )
  end
  local text = read(tea_dir() .. "/config.yml")
  for _, login in pairs(text and tea_logins(text) or {}) do
    if login.name and login.refresh then
      refresh_login(
        worktree,
        "tea:" .. login.name,
        login.expiry,
        function()
          return tea_expire(login.name)
        end,
        { "tea", "repos", "list", "--login", login.name, "--limit", "1", "--output", "simple" },
        "tea login oauth-refresh " .. login.name
      )
    end
  end
  for _, host in ipairs(gh_hosts()) do
    if again or not gh_tokens[host] then
      term.run{
        cmd = { "gh", "auth", "token", "--hostname", host },
        host = true,
        worktree = worktree,
        done = function(r)
          local token = r.ok and trim(r.stdout or "") or ""
          if token ~= "" and token ~= gh_tokens[host] then
            gh_tokens[host] = token
            apply()
          end
        end,
      }
    end
  end
end

apply()

-- Pick up changed logins when a worktree on this machine turns its sandbox on.
term.on("environment_ready", function(e)
  if e.kind == "sandbox" then
    apply()
    if e.host == "local" then
      refresh(e.worktree, true)
    end
  end
end)

-- The first worktree of this machine the user opens refreshes right away,
-- so a login that expired while the app was closed works in sandboxes.
term.on("worktree_activated", function(e)
  if e.env == "local" then
    refresh(e.path)
  end
end)

-- OAuth logins last about an hour; refresh them before they run out.
term.task{
  name = "refresh",
  title = "Refresh forge CLI logins for sandboxes",
  every = "10m",
  run = function()
    apply()
    -- The tools run in the active project's environment, which must be
    -- this machine.
    local context = term.context()
    if context.env == "local" and context.worktree then
      refresh(context.worktree, true)
    end
  end,
}
