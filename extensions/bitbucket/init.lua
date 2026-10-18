-- Looks up the pull requests of agent branches on Bitbucket Cloud for the
-- rail's Agents tab. scriptschnell-term calls `check` with every repository
-- and branch it wants to know about, at most every few minutes and only for
-- agents that finished. Bitbucket has no GraphQL API, so this extension
-- sends one request per repository and up to 20 branches, filtered with
-- `q=source.branch.name="a" OR ...`. Public repositories work without a
-- token.
--
-- Bitbucket's API tokens log in with Basic auth as `email:token`, so the
-- user pastes both, and `{base64}` encodes them as typed. Each workspace
-- on the Accounts page is an account of its own, bitbucket.org/<workspace>,
-- with its own login. The token never reaches this extension:
-- `term.http{ account = account }` has scriptschnell-term add it to
-- requests to bitbucket.org and its subdomains, such as api.bitbucket.org.

local API = "https://api.bitbucket.org/2.0"
-- Branches per request, to keep the URL short.
local BATCH = 20

local function encode(text)
  return (text:gsub("[^%w%-%._~]", function(c)
    return ("%%%02X"):format(c:byte())
  end))
end

-- A BBQL string literal.
local function quote(text)
  return '"' .. (text:gsub('[\\"]', "\\%0")) .. '"'
end

local STATES = { OPEN = "open", MERGED = "merged", DECLINED = "closed", SUPERSEDED = "closed" }
-- Which request of a branch wins: an open one, else the newest.
local RANK = { open = 3, merged = 2, closed = 1 }

local function look(item, branches)
  local workspace, repo = item.path:match("^([^/]+)/([^/]+)$")
  if not workspace then
    return
  end
  local filter = {}
  for i, branch in ipairs(branches) do
    filter[i] = "source.branch.name = " .. quote(branch)
  end
  local url = ("%s/repositories/%s/%s/pullrequests?pagelen=50&sort=-updated_on"
    .. "&state=OPEN&state=MERGED&state=DECLINED&state=SUPERSEDED"
    .. "&fields=values.id,values.title,values.state,values.links.html.href,values.source.branch.name&q=%s")
    :format(API, encode(workspace), encode(repo), encode(table.concat(filter, " OR ")))
  term.http{
    url = url,
    account = item.account,
    done = function(res)
      if res.status == 429 then
        term.forge_report("bitbucket", {}, { retry_after = tonumber(res.headers["retry-after"] or "") or 900 })
        return
      end
      if not res.ok then
        return
      end
      local answer = term.json_decode(res.body)
      local list = type(answer) == "table" and answer.values
      if type(list) ~= "table" then
        return
      end
      local best = {}
      for _, pr in ipairs(list) do
        local branch = type(pr.source) == "table" and type(pr.source.branch) == "table" and pr.source.branch.name
        if branch then
          local state = STATES[pr.state] or "closed"
          local known = best[branch]
          if not known or RANK[state] > RANK[known.state] then
            best[branch] = { state = state, pr = pr }
          end
        end
      end
      local results = {}
      for _, branch in ipairs(branches) do
        local found = best[branch]
        local links = found and type(found.pr.links) == "table" and found.pr.links
        results[#results + 1] = {
          account = item.account,
          path = item.path,
          branch = branch,
          state = found and found.state or "none",
          number = found and found.pr.id,
          url = links and type(links.html) == "table" and links.html.href or nil,
          title = found and found.pr.title,
        }
      end
      term.forge_report("bitbucket", results)
    end,
  }
end

-- The text of `value` when it is a string, else nil (JSON null).
local function text(value)
  return type(value) == "string" and value or nil
end

local function list(value)
  return type(value) == "table" and value or {}
end

-- Sends the `term.http` requests of `requests`, a table of name to request,
-- and calls `finish(answers, error)` once every answer came back, with the
-- decoded bodies by name and the first error.
local function all(requests, finish)
  local left, answers, failed = 0, {}, nil
  for _ in pairs(requests) do
    left = left + 1
  end
  for name, request in pairs(requests) do
    request.done = function(res)
      if res.ok then
        answers[name] = term.json_decode(res.body)
      elseif not failed then
        failed = res.error or ("HTTP " .. res.status)
      end
      left = left - 1
      if left == 0 then
        finish(answers, failed)
      end
    end
    term.http(request)
  end
end

-- Commit status states.
local CHECKS = { SUCCESSFUL = "success", FAILED = "failed", INPROGRESS = "running", STOPPED = "canceled" }

-- The pull request, its comments, its statuses and its activity, whose
-- updates name the source commit of every push. Bitbucket keeps no commit
-- with a code comment, so the app takes its line as it is on the head.
local function discussion(request)
  local workspace, repo = request.path:match("^([^/]+)/([^/]+)$")
  if not workspace then
    term.forge_discussion("bitbucket", request.id, nil, "not a repository path: " .. request.path)
    return
  end
  local base = ("%s/repositories/%s/%s/pullrequests/%d"):format(API, encode(workspace), encode(repo), request.number)
  all({
    pr = { url = base, account = request.account },
    comments = { url = base .. "/comments?pagelen=100", account = request.account },
    activity = { url = base .. "/activity?pagelen=50", account = request.account },
    statuses = { url = base .. "/statuses?pagelen=100", account = request.account },
  }, function(answers, failed)
    local pr = answers.pr
    if type(pr) ~= "table" then
      term.forge_discussion("bitbucket", request.id, nil, failed or "no pull request")
      return
    end
    local function name(user)
      return type(user) == "table" and (text(user.display_name) or text(user.nickname)) or ""
    end
    local values = list(type(answers.comments) == "table" and answers.comments.values)
    local by_id, roots = {}, {}
    for _, c in ipairs(values) do
      if c.deleted ~= true then
        by_id[c.id] = c
      end
    end
    local comments, threads, thread_of = {}, {}, {}
    for _, c in ipairs(values) do
      if c.deleted ~= true then
        local root = c
        while type(root.parent) == "table" and by_id[root.parent.id] do
          root = by_id[root.parent.id]
        end
        local note = {
          author = name(c.user),
          body = type(c.content) == "table" and text(c.content.raw) or "",
          at = text(c.created_on),
        }
        local inline = type(root.inline) == "table" and root.inline
        if inline and text(inline.path) then
          local thread = thread_of[root.id]
          if not thread then
            local to = type(inline.to) == "number" and inline.to or nil
            local from = type(inline.from) == "number" and inline.from or nil
            thread = {
              path = inline.path,
              line = to or from,
              side = to and "new" or "old",
              resolved = type(root.resolution) == "table",
              outdated = inline.outdated == true,
              comments = {},
            }
            thread_of[root.id] = thread
            threads[#threads + 1] = thread
          end
          table.insert(thread.comments, note)
        else
          comments[#comments + 1] = note
        end
      end
    end
    local pushes = {}
    for _, a in ipairs(list(type(answers.activity) == "table" and answers.activity.values)) do
      local update = type(a.update) == "table" and a.update
      local source = update and type(update.source) == "table" and update.source
      local commit = source and type(source.commit) == "table" and text(source.commit.hash)
      if commit then
        pushes[#pushes + 1] = { commit = commit, at = text(update.date) }
      end
    end
    local reviews = {}
    for _, p in ipairs(list(pr.participants)) do
      local state = p.state == "approved" and "approved"
        or p.state == "changes_requested" and "changes"
        or p.role == "REVIEWER" and "pending"
        or nil
      if state then
        reviews[#reviews + 1] = { author = name(p.user), state = state }
      end
    end
    local checks = {}
    for _, s in ipairs(list(type(answers.statuses) == "table" and answers.statuses.values)) do
      checks[#checks + 1] = {
        name = text(s.name) or text(s.key) or "",
        state = CHECKS[s.state] or "pending",
        url = text(s.url),
      }
    end
    local destination = type(pr.destination) == "table" and pr.destination
    local links = type(pr.links) == "table" and pr.links
    term.forge_discussion("bitbucket", request.id, {
      title = text(pr.title) or "",
      url = links and type(links.html) == "table" and text(links.html.href) or nil,
      state = STATES[pr.state],
      author = name(pr.author),
      body = text(pr.description),
      draft = pr.draft == true,
      target = destination and type(destination.branch) == "table" and text(destination.branch.name) or nil,
      reviews = reviews,
      checks = checks,
      pushes = pushes,
      comments = comments,
      threads = threads,
    })
  end)
end

-- What sandboxes may do to pull requests. The app puts these on the hosts
-- of the accounts and their subdomains, such as api.bitbucket.org, and the
-- user picks Ask, Block or Allow per kind for every forge.
local GUARDS = {
  {
    name = "create",
    kind = "create",
    title = "open a pull request",
    requests = { "POST */2.0/repositories/*/pullrequests" },
  },
  {
    name = "comment",
    kind = "comment",
    title = "comment on or review a pull request",
    requests = {
      "POST */2.0/repositories/*/pullrequests/*/comments",
      "POST */2.0/repositories/*/pullrequests/*/approve",
      "POST */2.0/repositories/*/pullrequests/*/request-changes",
    },
  },
  {
    name = "merge",
    kind = "merge",
    title = "merge a pull request",
    requests = { "POST */2.0/repositories/*/pullrequests/*/merge" },
  },
}

term.forge{
  name = "bitbucket",
  title = "Bitbucket",
  accounts = {
    default = { "bitbucket.org" },
    example = "bitbucket.org/acme",
    hint = "bitbucket.org, or bitbucket.org/<workspace> for the repositories of that workspace, each with its own login. A repository takes the account with the longest matching path.",
  },
  auth = { header = "Authorization", value = "Basic {base64}" },
  scopes = "Paste your Atlassian email and an API token as email:token. The token needs the read:pullrequest:bitbucket, read:repository:bitbucket and read:user:bitbucket scopes.",
  token_url = function()
    return "https://id.atlassian.com/manage-profile/security/api-tokens"
  end,
  whoami = function()
    return API .. "/user", "display_name"
  end,
  guards = GUARDS,
  discussion = discussion,
  check = function(items)
    for _, item in ipairs(items) do
      for start = 1, #item.branches, BATCH do
        look(item, { table.unpack(item.branches, start, math.min(start + BATCH - 1, #item.branches)) })
      end
    end
  end,
}
