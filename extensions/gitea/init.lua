-- Looks up the pull requests of agent branches on Gitea for the rail's
-- Agents tab. scriptschnell-term calls `check` with every repository and
-- branch it wants to know about, at most every few minutes and only for
-- agents that finished. Gitea has no GraphQL API, so this extension asks
-- for the 50 most recently updated pull requests of each repository, one
-- request per repository whatever the number of branches, and matches
-- their head branches. Public repositories work without a token.
--
-- An account is a host, or a host and an owner path such as
-- gitea.com/acme for the repositories under it, each with its own token.
-- The token never reaches this extension: `term.http{ account = account }`
-- has scriptschnell-term add it to requests to the account's host.

local NAME = "gitea"

-- Percent-encodes one path segment.
local function encode(text)
  return (text:gsub("[^%w%-%._~]", function(c)
    return ("%%%02X"):format(c:byte())
  end))
end

-- Which request of a branch wins: an open one, else the newest.
local RANK = { open = 3, merged = 2, closed = 1 }

local function look(item)
  local owner, repo = item.path:match("^([^/]+)/([^/]+)$")
  if not owner then
    return
  end
  term.http{
    url = ("https://%s/api/v1/repos/%s/%s/pulls?state=all&sort=recentupdate&limit=50")
      :format(item.account:match("^[^/]+"), encode(owner), encode(repo)),
    account = item.account,
    done = function(res)
      if res.status == 429 then
        term.forge_report(NAME, {}, { retry_after = tonumber(res.headers["retry-after"] or "") or 900 })
        return
      end
      if not res.ok then
        return
      end
      local pulls = term.json_decode(res.body)
      if type(pulls) ~= "table" then
        return
      end
      local best = {}
      for _, pull in ipairs(pulls) do
        local branch = type(pull.head) == "table" and pull.head.ref
        if branch then
          local state = pull.merged and "merged" or (pull.state == "open" and "open" or "closed")
          local known = best[branch]
          if not known or RANK[state] > RANK[known.state] then
            best[branch] = { state = state, pull = pull }
          end
        end
      end
      local results = {}
      for _, branch in ipairs(item.branches) do
        local found = best[branch]
        results[#results + 1] = {
          account = item.account,
          path = item.path,
          branch = branch,
          state = found and found.state or "none",
          number = found and found.pull.number,
          url = found and found.pull.html_url,
          title = found and found.pull.title,
        }
      end
      term.forge_report(NAME, results)
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

-- Gitea and Forgejo name the line of a review comment `position` on the
-- new side and `original_position` on the old side.
local function review_comment(c)
  return {
    author = type(c.user) == "table" and text(c.user.login) or "",
    body = text(c.body) or "",
    at = text(c.created_at),
  }
end

local VERDICTS = { APPROVED = "approved", REQUEST_CHANGES = "changes", COMMENT = "commented", REQUEST_REVIEW = "pending" }
local CHECKS = { success = "success", pending = "running", error = "failed", failure = "failed", warning = "success" }

-- Each reviewer's latest verdict; a later comment keeps an approval or a
-- request for changes.
local function verdicts(list_)
  local found, at = {}, {}
  for _, review in ipairs(list_) do
    local name = type(review.user) == "table" and text(review.user.login)
      or type(review.team) == "table" and text(review.team.name)
    local state = VERDICTS[review.state]
    if name and state and review.dismissed ~= true then
      local index = at[name]
      if not index then
        found[#found + 1] = { author = name, state = state }
        at[name] = #found
      elseif state ~= "commented" or found[index].state == "pending" or found[index].state == "commented" then
        found[index].state = state
      end
    end
  end
  return found
end

-- The pull request, its comments, reviews and timeline, then the comments
-- of every review that has some and the statuses of the head commit.
local function discussion(request)
  local owner, repo = request.path:match("^([^/]+)/([^/]+)$")
  if not owner then
    term.forge_discussion(NAME, request.id, nil, "not a repository path: " .. request.path)
    return
  end
  local host = request.account:match("^[^/]+")
  local base = ("https://%s/api/v1/repos/%s/%s"):format(host, encode(owner), encode(repo))
  local function get(path)
    return { url = base .. path, account = request.account }
  end
  local n = request.number
  all({
    pull = get("/pulls/" .. n),
    comments = get(("/issues/%d/comments?limit=100"):format(n)),
    reviews = get(("/pulls/%d/reviews?limit=50"):format(n)),
    timeline = get(("/issues/%d/timeline?limit=100"):format(n)),
  }, function(answers, failed)
    local pull = answers.pull
    if type(pull) ~= "table" then
      term.forge_discussion(NAME, request.id, nil, failed or "no pull request")
      return
    end
    local comments = {}
    for _, c in ipairs(list(answers.comments)) do
      comments[#comments + 1] = review_comment(c)
    end
    local wanted = {}
    for _, review in ipairs(list(answers.reviews)) do
      local body = text(review.body) or ""
      local verdict = review.state == "APPROVED" and "Approved. "
        or review.state == "REQUEST_CHANGES" and "Requested changes. " or ""
      if body ~= "" or verdict ~= "" then
        comments[#comments + 1] = {
          author = type(review.user) == "table" and text(review.user.login) or "",
          body = verdict .. body,
          at = text(review.submitted_at),
        }
      end
      if type(review.id) == "number" and (review.comments_count or 0) > 0 then
        wanted["r" .. review.id] = get(("/pulls/%d/reviews/%d/comments"):format(n, review.id))
      end
    end
    table.sort(comments, function(a, b)
      return (a.at or "") < (b.at or "")
    end)
    local pushes = {}
    for _, event in ipairs(list(answers.timeline)) do
      if event.type == "pull_push" then
        local push = term.json_decode(text(event.body) or "") or {}
        local ids = list(push.commit_ids)
        if push.is_force_push == true and ids[2] then
          pushes[#pushes + 1] = { commit = ids[2], before = ids[1], at = text(event.created_at), force = true }
        elseif ids[#ids] then
          pushes[#pushes + 1] = { commit = ids[#ids], at = text(event.created_at) }
        end
      end
    end
    local function finish(reviews)
      local checks = {}
      local status = reviews.status
      reviews.status = nil
      for _, s in ipairs(type(status) == "table" and list(status.statuses) or {}) do
        checks[#checks + 1] = {
          name = text(s.context) or "",
          state = CHECKS[s.status] or "pending",
          url = text(s.target_url),
        }
      end
      local threads, by_place = {}, {}
      for _, answer in pairs(reviews) do
        for _, c in ipairs(list(answer)) do
          local new_line = type(c.position) == "number" and c.position > 0 and c.position or nil
          local old_line = type(c.original_position) == "number" and c.original_position > 0 and c.original_position or nil
          local key = ("%s\n%s\n%s"):format(text(c.path) or "", new_line or -(old_line or 0), text(c.commit_id) or "")
          local thread = by_place[key]
          if not thread then
            thread = {
              path = text(c.path) or "",
              line = new_line or old_line,
              side = new_line and "new" or "old",
              commit = text(c.commit_id),
              hunk = text(c.diff_hunk),
              resolved = type(c.resolver) == "table",
              comments = {},
            }
            by_place[key] = thread
            threads[#threads + 1] = thread
          end
          table.insert(thread.comments, review_comment(c))
        end
      end
      for _, thread in ipairs(threads) do
        table.sort(thread.comments, function(a, b)
          return (a.at or "") < (b.at or "")
        end)
      end
      term.forge_discussion(NAME, request.id, {
        title = text(pull.title) or "",
        url = text(pull.html_url),
        state = pull.merged == true and "merged" or (pull.state == "open" and "open" or "closed"),
        author = type(pull.user) == "table" and text(pull.user.login) or nil,
        body = text(pull.body),
        draft = pull.draft == true,
        target = type(pull.base) == "table" and text(pull.base.ref) or nil,
        conflicts = pull.state == "open" and pull.mergeable == false,
        reviews = verdicts(list(answers.reviews)),
        checks = checks,
        pushes = pushes,
        comments = comments,
        threads = threads,
      })
    end
    local head = type(pull.head) == "table" and text(pull.head.sha)
    if head then
      wanted.status = get("/commits/" .. encode(head) .. "/status")
    end
    if next(wanted) then
      all(wanted, finish)
    else
      finish({})
    end
  end)
end

-- What sandboxes may do to pull requests. The app puts these on the hosts
-- of the accounts and their subdomains, and the user picks Ask, Block or
-- Allow per kind for every forge.
local GUARDS = {
  {
    name = "create",
    kind = "create",
    title = "open a pull request",
    requests = { "POST */api/v1/repos/*/pulls" },
  },
  {
    name = "comment",
    kind = "comment",
    title = "comment on or review a pull request",
    requests = {
      "POST */api/v1/repos/*/issues/*/comments",
      "POST */api/v1/repos/*/pulls/*/reviews",
      "POST */api/v1/repos/*/pulls/*/reviews/*",
    },
  },
  {
    name = "merge",
    kind = "merge",
    title = "merge a pull request",
    requests = { "POST */api/v1/repos/*/pulls/*/merge" },
  },
}

term.forge{
  name = NAME,
  title = "Gitea",
  accounts = {
    default = { "gitea.com" },
    example = "gitea.com/acme",
    hint = "A host such as gitea.com or a self-hosted one, or a host and an owner such as gitea.com/acme for the repositories under it. A repository takes the account with the longest matching path.",
  },
  auth = { header = "Authorization", value = "token {token}" },
  env = { "GITEA_TOKEN" },
  scopes = "An access token with read access to repositories (read:repository).",
  token_url = function(host)
    return "https://" .. host .. "/user/settings/applications"
  end,
  whoami = function(host)
    return "https://" .. host .. "/api/v1/user", "login"
  end,
  guards = GUARDS,
  discussion = discussion,
  check = function(items)
    for _, item in ipairs(items) do
      look(item)
    end
  end,
}
