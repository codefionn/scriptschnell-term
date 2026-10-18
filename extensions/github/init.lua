-- Looks up the pull requests of agent branches on GitHub for the rail's
-- Agents tab. scriptschnell-term calls `check` with every repository and
-- branch it wants to know about, at most every few minutes and only for
-- agents that finished. This extension asks for all of them in one GraphQL
-- request per 50 branches, which costs one point of the 5000 an hour a
-- token gets. Without a token GitHub's GraphQL API answers nothing, so
-- repositories without a signed-in account are skipped.
--
-- An account is a host, or a host and an owner path such as
-- github.com/acme for the repositories under it, each with its own token.
-- The token never reaches this extension: `term.http{ account = account }`
-- has scriptschnell-term add it to requests to the account's host.

local BATCH = 50

local function api(account)
  local host = account:match("^[^/]+")
  if host == "github.com" then
    return "https://api.github.com"
  end
  return "https://" .. host .. "/api"
end

-- A GraphQL string literal.
local function quote(text)
  return '"' .. (text:gsub('[\\"]', "\\%0"):gsub("\n", "\\n")) .. '"'
end

local STATES = { OPEN = "open", MERGED = "merged", CLOSED = "closed" }

-- Asks for `pairs`, a list of { item, branch }, in one query.
local function query(account, pairs)
  local repos, order = {}, {}
  for _, pair in ipairs(pairs) do
    local path = pair.item.path
    if not repos[path] then
      repos[path] = {}
      order[#order + 1] = path
    end
    table.insert(repos[path], pair.branch)
  end
  local parts, fields = { "query {" }, {}
  for r, path in ipairs(order) do
    local owner, name = path:match("^([^/]+)/([^/]+)$")
    if owner then
      parts[#parts + 1] = ("r%d: repository(owner: %s, name: %s) {"):format(r, quote(owner), quote(name))
      for b, branch in ipairs(repos[path]) do
        parts[#parts + 1] = ("b%d: pullRequests(headRefName: %s, first: 1, states: [OPEN, MERGED, CLOSED], orderBy: {field: CREATED_AT, direction: DESC}) { nodes { number state url title } }")
          :format(b, quote(branch))
        fields[#fields + 1] = { repo = "r" .. r, field = "b" .. b, path = path, branch = branch }
      end
      parts[#parts + 1] = "}"
    end
  end
  parts[#parts + 1] = "rateLimit { remaining resetAt } }"
  if #fields == 0 then
    return
  end
  term.http{
    method = "POST",
    url = api(account) .. "/graphql",
    account = account,
    headers = { ["Content-Type"] = "application/json" },
    body = term.json_encode({ query = table.concat(parts, " ") }),
    done = function(res)
      if res.status == 403 or res.status == 429 then
        local wait = tonumber(res.headers["retry-after"] or "") or 900
        term.forge_report("github", {}, { retry_after = wait })
        return
      end
      if not res.ok then
        return
      end
      local answer = term.json_decode(res.body)
      local data = answer and answer.data
      if type(data) ~= "table" then
        return
      end
      local results = {}
      for _, f in ipairs(fields) do
        local repo = data[f.repo]
        local connection = type(repo) == "table" and repo[f.field]
        if type(connection) == "table" then
          local pr = connection.nodes and connection.nodes[1]
          results[#results + 1] = {
            account = account,
            path = f.path,
            branch = f.branch,
            state = pr and STATES[pr.state] or "none",
            number = pr and pr.number,
            url = pr and pr.url,
            title = pr and pr.title,
          }
        end
      end
      term.forge_report("github", results)
    end,
  }
end

-- The text of `value` when it is a string, else nil (JSON null).
local function text(value)
  return type(value) == "string" and value or nil
end

local function login(actor)
  return type(actor) == "table" and text(actor.login) or "ghost"
end

local function nodes(connection)
  return type(connection) == "table" and type(connection.nodes) == "table" and connection.nodes or {}
end

local DISCUSSION = [[
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title url state body author { login } isDraft baseRefName mergeable
      latestReviews(first: 50) { nodes { author { login } state } }
      reviewRequests(first: 20) {
        nodes { requestedReviewer { ... on User { login } ... on Bot { login } ... on Team { name } } }
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun { name status conclusion detailsUrl }
                  ... on StatusContext { context state targetUrl }
                }
              }
            }
          }
        }
      }
      comments(first: 100) { nodes { author { login } body createdAt } }
      reviews(first: 50) { nodes { author { login } body state submittedAt } }
      reviewThreads(first: 100) {
        nodes {
          path originalLine diffSide isResolved isOutdated
          comments(first: 50) {
            nodes { author { login } body createdAt diffHunk originalCommit { oid } }
          }
        }
      }
      timelineItems(last: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT, PULL_REQUEST_COMMIT]) {
        nodes {
          __typename
          ... on HeadRefForcePushedEvent { createdAt beforeCommit { oid } afterCommit { oid } }
          ... on PullRequestCommit { commit { oid committedDate } }
        }
      }
    }
  }
}]]

local VERDICTS = { APPROVED = "approved", CHANGES_REQUESTED = "changes", COMMENTED = "commented" }

-- A check run's or a commit status's state.
local function check_state(node)
  if node.__typename == "StatusContext" then
    return ({ SUCCESS = "success", FAILURE = "failed", ERROR = "failed", PENDING = "running" })[node.state] or "pending"
  end
  if node.status ~= "COMPLETED" then
    return node.status == "IN_PROGRESS" and "running" or "pending"
  end
  return ({
    SUCCESS = "success",
    NEUTRAL = "success",
    SKIPPED = "skipped",
    CANCELLED = "canceled",
    STALE = "canceled",
  })[node.conclusion] or "failed"
end

-- The reviewers' latest verdicts, then the reviewers asked who gave none.
local function reviews(pr)
  local list = {}
  for _, r in ipairs(nodes(pr.latestReviews)) do
    if VERDICTS[r.state] then
      list[#list + 1] = { author = login(r.author), state = VERDICTS[r.state] }
    end
  end
  for _, r in ipairs(nodes(pr.reviewRequests)) do
    local who = type(r.requestedReviewer) == "table" and r.requestedReviewer
    local name = who and (text(who.login) or text(who.name))
    if name then
      list[#list + 1] = { author = name, state = "pending" }
    end
  end
  return list
end

-- The checks on the pull request's last commit.
local function checks(pr)
  local list = {}
  local last = nodes(pr.commits)[1]
  local commit = type(last) == "table" and type(last.commit) == "table" and last.commit
  local rollup = commit and type(commit.statusCheckRollup) == "table" and commit.statusCheckRollup
  for _, node in ipairs(rollup and nodes(rollup.contexts) or {}) do
    list[#list + 1] = {
      name = text(node.name) or text(node.context) or "",
      state = check_state(node),
      url = text(node.detailsUrl) or text(node.targetUrl),
    }
  end
  return list
end

-- One GraphQL request for the pull request, its status, comments, review
-- threads and force pushes.
local function discussion(request)
  local owner, name = request.path:match("^([^/]+)/([^/]+)$")
  if not owner or not request.signed_in then
    term.forge_discussion("github", request.id, nil, "sign in to " .. request.account .. " on the Accounts page of the settings")
    return
  end
  term.http{
    method = "POST",
    url = api(request.account) .. "/graphql",
    account = request.account,
    headers = { ["Content-Type"] = "application/json" },
    body = term.json_encode({
      query = DISCUSSION,
      variables = { owner = owner, name = name, number = request.number },
    }),
    done = function(res)
      local answer = res.ok and term.json_decode(res.body)
      local repo = type(answer) == "table" and type(answer.data) == "table" and answer.data.repository
      local pr = type(repo) == "table" and repo.pullRequest
      if type(pr) ~= "table" then
        term.forge_discussion("github", request.id, nil, res.error or ("HTTP " .. res.status))
        return
      end
      local comments = {}
      for _, c in ipairs(nodes(pr.comments)) do
        comments[#comments + 1] = { author = login(c.author), body = text(c.body) or "", at = text(c.createdAt) }
      end
      for _, r in ipairs(nodes(pr.reviews)) do
        local body = text(r.body) or ""
        if body ~= "" or r.state == "APPROVED" or r.state == "CHANGES_REQUESTED" then
          local verdict = r.state == "APPROVED" and "Approved. "
            or r.state == "CHANGES_REQUESTED" and "Requested changes. " or ""
          comments[#comments + 1] = { author = login(r.author), body = verdict .. body, at = text(r.submittedAt) }
        end
      end
      table.sort(comments, function(a, b)
        return (a.at or "") < (b.at or "")
      end)
      local threads = {}
      for _, t in ipairs(nodes(pr.reviewThreads)) do
        local list = nodes(t.comments)
        local first = list[1] or {}
        local thread = {
          path = text(t.path) or "",
          line = type(t.originalLine) == "number" and t.originalLine or nil,
          side = t.diffSide == "LEFT" and "old" or "new",
          commit = type(first.originalCommit) == "table" and text(first.originalCommit.oid) or nil,
          hunk = text(first.diffHunk),
          resolved = t.isResolved == true,
          outdated = t.isOutdated == true,
          comments = {},
        }
        for _, c in ipairs(list) do
          table.insert(thread.comments, { author = login(c.author), body = text(c.body) or "", at = text(c.createdAt) })
        end
        threads[#threads + 1] = thread
      end
      local pushes = {}
      for _, item in ipairs(nodes(pr.timelineItems)) do
        if item.__typename == "HeadRefForcePushedEvent" and type(item.afterCommit) == "table" then
          pushes[#pushes + 1] = {
            commit = item.afterCommit.oid,
            before = type(item.beforeCommit) == "table" and text(item.beforeCommit.oid) or nil,
            at = text(item.createdAt),
            force = true,
          }
        elseif item.__typename == "PullRequestCommit" and type(item.commit) == "table" then
          pushes[#pushes + 1] = {
            commit = item.commit.oid,
            at = text(item.commit.committedDate),
          }
        end
      end
      term.forge_discussion("github", request.id, {
        title = text(pr.title) or "",
        url = text(pr.url),
        state = STATES[pr.state],
        author = login(pr.author),
        body = text(pr.body),
        draft = pr.isDraft == true,
        target = text(pr.baseRefName),
        conflicts = pr.mergeable == "CONFLICTING",
        reviews = reviews(pr),
        checks = checks(pr),
        pushes = pushes,
        comments = comments,
        threads = threads,
      })
    end,
  }
end

-- What sandboxes may do to pull requests. The app puts these on the hosts
-- of the accounts and their subdomains, such as api.github.com, and the
-- user picks Ask, Block or Allow per kind for every forge. `gh` speaks
-- GraphQL, so those patterns read the mutation in the body.
local GUARDS = {
  {
    name = "create",
    kind = "create",
    title = "open a pull request",
    requests = {
      "POST */repos/*/pulls",
      "POST */graphql *mutation*createPullRequest*",
    },
  },
  {
    name = "comment",
    kind = "comment",
    title = "comment on or review a pull request",
    requests = {
      "POST */repos/*/issues/*/comments",
      "POST */repos/*/pulls/*/comments",
      "POST */repos/*/pulls/*/comments/*/replies",
      "POST */repos/*/pulls/*/reviews",
      "POST */repos/*/pulls/*/reviews/*/events",
      "POST */graphql *mutation*addComment*",
      "POST */graphql *mutation*addPullRequestReview*",
      "POST */graphql *mutation*submitPullRequestReview*",
    },
  },
  {
    name = "merge",
    kind = "merge",
    title = "merge a pull request",
    requests = {
      "PUT */repos/*/pulls/*/merge",
      "POST */graphql *mutation*mergePullRequest*",
      "POST */graphql *mutation*enablePullRequestAutoMerge*",
      "POST */graphql *mutation*enqueuePullRequest*",
    },
  },
}

term.forge{
  name = "github",
  title = "GitHub",
  accounts = {
    default = { "github.com" },
    example = "github.com/acme",
    hint = "A host such as github.com or a GitHub Enterprise Server host, or a host and an owner such as github.com/acme for the repositories under it. A repository takes the account with the longest matching path.",
  },
  auth = { header = "Authorization", value = "Bearer {token}" },
  env = { "GH_TOKEN", "GITHUB_TOKEN" },
  scopes = "A fine-grained token with read access to pull requests, commit statuses and metadata, or a classic token with the repo scope.",
  token_url = function(host)
    return "https://" .. host .. "/settings/personal-access-tokens/new?pull_requests=read&contents=read&statuses=read"
  end,
  whoami = function(host)
    if host == "github.com" then
      return "https://api.github.com/user", "login"
    end
    return "https://" .. host .. "/api/v3/user", "login"
  end,
  guards = GUARDS,
  discussion = discussion,
  check = function(items)
    local by_account = {}
    for _, item in ipairs(items) do
      if item.signed_in then
        local pairs_ = by_account[item.account] or {}
        by_account[item.account] = pairs_
        for _, branch in ipairs(item.branches) do
          pairs_[#pairs_ + 1] = { item = item, branch = branch }
        end
      end
    end
    for account, all in pairs(by_account) do
      for start = 1, #all, BATCH do
        query(account, { table.unpack(all, start, math.min(start + BATCH - 1, #all)) })
      end
    end
  end,
}
