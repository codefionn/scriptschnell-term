-- Looks up the pull requests of agent branches on Azure DevOps for the
-- rail's Agents tab. scriptschnell-term calls `check` with every repository
-- and branch it wants to know about, at most every few minutes and only for
-- agents that finished. This extension sends one request per project: the
-- project-wide pull request list covers all its repositories, and the
-- extension matches repository and source branch itself.
--
-- An account is dev.azure.com/<organization>, with its own token for
-- that organization, or dev.azure.com for every organization without one.
-- The token never reaches this extension: `term.http{ account = account }`
-- has scriptschnell-term add it to requests to dev.azure.com.

local HOST = "dev.azure.com"

local function encode(text)
  return (text:gsub("[^%w%-%._~]", function(c)
    return ("%%%02X"):format(c:byte())
  end))
end

-- Organization, project and repository of a remote: dev.azure.com/org/
-- project/_git/repo, ssh.dev.azure.com:v3/org/project/repo, or
-- org.visualstudio.com/[DefaultCollection/]project/_git/repo.
local function parts(host, path)
  local org, project, repo = path:match("^([^/]+)/([^/]+)/_git/([^/]+)$")
  if org and host == "dev.azure.com" then
    return org, project, repo
  end
  org, project, repo = path:match("^v3/([^/]+)/([^/]+)/([^/]+)$")
  if org then
    return org, project, repo
  end
  org = host:match("^([^.]+)%.visualstudio%.com$")
  if org then
    path = path:gsub("^DefaultCollection/", "")
    project, repo = path:match("^([^/]+)/_git/([^/]+)$")
    if project then
      return org, project, repo
    end
  end
end

local STATES = { active = "open", completed = "merged", abandoned = "closed" }
local RANK = { open = 3, merged = 2, closed = 1 }

-- `wanted` lists { item, repo } of one project.
local function look(account, org, project, wanted)
  local base = ("https://dev.azure.com/%s/%s"):format(encode(org), encode(project))
  term.http{
    url = base .. "/_apis/git/pullrequests?searchCriteria.status=all&$top=200&api-version=7.1",
    account = account,
    done = function(res)
      if res.status == 429 then
        term.forge_report("azure-devops", {}, { retry_after = tonumber(res.headers["retry-after"] or "") or 300 })
        return
      end
      if not res.ok then
        return
      end
      local answer = term.json_decode(res.body)
      local list = type(answer) == "table" and answer.value
      if type(list) ~= "table" then
        return
      end
      local best = {}
      for _, pr in ipairs(list) do
        local repo = type(pr.repository) == "table" and pr.repository.name
        local branch = type(pr.sourceRefName) == "string" and pr.sourceRefName:match("^refs/heads/(.+)$")
        if repo and branch then
          local key = repo:lower() .. "\n" .. branch
          local state = STATES[pr.status] or "closed"
          local known = best[key]
          if not known or RANK[state] > RANK[known.state] then
            best[key] = { state = state, pr = pr, repo = repo }
          end
        end
      end
      local results = {}
      for _, w in ipairs(wanted) do
        for _, branch in ipairs(w.item.branches) do
          local found = best[w.repo:lower() .. "\n" .. branch]
          results[#results + 1] = {
            account = w.item.account,
            path = w.item.path,
            branch = branch,
            state = found and found.state or "none",
            number = found and found.pr.pullRequestId,
            url = found and ("%s/_git/%s/pullrequest/%d"):format(base, encode(found.repo), found.pr.pullRequestId),
            title = found and found.pr.title,
          }
        end
      end
      term.forge_report("azure-devops", results)
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

local THREAD_OPEN = { active = true, pending = true, unknown = true }

-- Pull request status states.
local CHECKS = { succeeded = "success", failed = "failed", error = "failed", pending = "running", notApplicable = "skipped" }

-- The pull request, its threads and its iterations (one per push, with
-- the reason, such as forcePush or rebase) and its statuses, in four REST
-- requests.
local function discussion(request)
  local org, project, repo = parts(request.host, request.path)
  if not org then
    term.forge_discussion("azure-devops", request.id, nil, "not an Azure DevOps repository: " .. request.path)
    return
  end
  local base = ("https://dev.azure.com/%s/%s"):format(encode(org), encode(project))
  local pr_url = ("%s/_apis/git/repositories/%s/pullRequests/%d"):format(base, encode(repo), request.number)
  local function get(path)
    return { url = pr_url .. path .. (path:find("?", 1, true) and "&" or "?") .. "api-version=7.1", account = request.account }
  end
  all({
    pr = get(""),
    threads = get("/threads"),
    iterations = get("/iterations"),
    statuses = get("/statuses"),
  }, function(answers, failed)
    local pr = answers.pr
    if type(pr) ~= "table" then
      term.forge_discussion("azure-devops", request.id, nil, failed or "no pull request")
      return
    end
    local commits, pushes = {}, {}
    for _, it in ipairs(list(type(answers.iterations) == "table" and answers.iterations.value)) do
      local commit = type(it.sourceRefCommit) == "table" and text(it.sourceRefCommit.commitId)
      if commit then
        commits[it.id] = commit
        pushes[#pushes + 1] = {
          commit = commit,
          at = text(it.createdDate),
          force = it.reason == "forcePush" or it.reason == "rebase",
        }
      end
    end
    local function name(author)
      return type(author) == "table" and text(author.displayName) or ""
    end
    local comments, threads = {}, {}
    for _, t in ipairs(list(type(answers.threads) == "table" and answers.threads.value)) do
      local notes = {}
      for _, c in ipairs(list(t.comments)) do
        if c.commentType ~= "system" and c.isDeleted ~= true then
          notes[#notes + 1] = { author = name(c.author), body = text(c.content) or "", at = text(c.publishedDate) }
        end
      end
      local context = type(t.threadContext) == "table" and t.threadContext
      if t.isDeleted == true or #notes == 0 then
        -- Votes, merges and other system threads.
      elseif context and text(context.filePath) then
        local right = type(context.rightFileStart) == "table" and context.rightFileStart.line
        local left = type(context.leftFileStart) == "table" and context.leftFileStart.line
        local pr_context = type(t.pullRequestThreadContext) == "table" and t.pullRequestThreadContext
        local iteration = pr_context and type(pr_context.iterationContext) == "table"
          and pr_context.iterationContext.secondComparingIteration
        threads[#threads + 1] = {
          path = context.filePath:gsub("^/", ""),
          line = right or left,
          side = right and "new" or "old",
          commit = right and iteration and commits[iteration] or nil,
          resolved = not THREAD_OPEN[t.status or "unknown"],
          comments = notes,
        }
      else
        for _, note in ipairs(notes) do
          comments[#comments + 1] = note
        end
      end
    end
    table.sort(comments, function(a, b)
      return (a.at or "") < (b.at or "")
    end)
    local reviews = {}
    for _, r in ipairs(list(pr.reviewers)) do
      local vote = type(r.vote) == "number" and r.vote or 0
      reviews[#reviews + 1] = {
        author = name(r),
        state = vote > 0 and "approved" or vote < 0 and "changes" or "pending",
      }
    end
    -- Several iterations post a status each; the newest per name counts.
    local checks, at = {}, {}
    for _, s in ipairs(list(type(answers.statuses) == "table" and answers.statuses.value)) do
      local context = type(s.context) == "table" and s.context or {}
      local check_name = text(context.genre) and (context.genre .. "/" .. (text(context.name) or ""))
        or text(context.name) or ""
      local check = {
        name = check_name,
        state = CHECKS[s.state] or "pending",
        url = text(s.targetUrl),
      }
      if at[check_name] then
        checks[at[check_name]] = check
      else
        checks[#checks + 1] = check
        at[check_name] = #checks
      end
    end
    local repo_name = type(pr.repository) == "table" and text(pr.repository.name) or repo
    term.forge_discussion("azure-devops", request.id, {
      title = text(pr.title) or "",
      url = ("%s/_git/%s/pullrequest/%d"):format(base, encode(repo_name), request.number),
      state = STATES[pr.status],
      author = name(pr.createdBy),
      body = text(pr.description),
      draft = pr.isDraft == true,
      target = text(pr.targetRefName) and pr.targetRefName:gsub("^refs/heads/", "") or nil,
      conflicts = pr.mergeStatus == "conflicts",
      reviews = reviews,
      checks = checks,
      pushes = pushes,
      comments = comments,
      threads = threads,
    })
  end)
end

-- What sandboxes may do to pull requests. The app puts these on the hosts
-- of the accounts and their subdomains and on *.visualstudio.com, and the
-- user picks Ask, Block or Allow per kind for every forge. Completing is a
-- PATCH that sets the status or auto-complete, so those patterns read the
-- body.
local GUARDS = {
  {
    name = "create",
    kind = "create",
    title = "open a pull request",
    requests = { "POST */_apis/git/repositories/*/pullrequests" },
  },
  {
    name = "comment",
    kind = "comment",
    title = "comment on or vote on a pull request",
    requests = {
      "POST */_apis/git/repositories/*/pullrequests/*/threads",
      "POST */_apis/git/repositories/*/pullrequests/*/threads/*/comments",
      "PUT */_apis/git/repositories/*/pullrequests/*/reviewers/*",
    },
  },
  {
    name = "merge",
    kind = "merge",
    title = "complete a pull request",
    requests = {
      'PATCH */_apis/git/repositories/*/pullrequests/* *"status"*"completed"*',
      "PATCH */_apis/git/repositories/*/pullrequests/* *autoCompleteSetBy*",
    },
  },
}

term.forge{
  name = "azure-devops",
  title = "Azure DevOps",
  accounts = {
    default = { HOST },
    example = "dev.azure.com/acme",
    hint = "dev.azure.com/<organization> for one organization, with its own token. dev.azure.com covers the organizations without an account of their own, but Sign in cannot check its token.",
  },
  -- Rewrites the other remote forms to dev.azure.com/<org>/..., so the
  -- organization picks the account.
  aliases = {
    ["ssh.dev.azure.com/v3"] = HOST,
    ["*.visualstudio.com"] = HOST .. "/*",
    ["*.visualstudio.com/DefaultCollection"] = HOST .. "/*",
  },
  auth = { header = "Authorization", value = "Basic {basic}" },
  env = { "AZURE_DEVOPS_EXT_PAT", "AZURE_DEVOPS_PAT" },
  scopes = "A personal access token of your organization with the Code (Read) scope.",
  token_url = function(_, account)
    local organization = account:match("^[^/]+/(.+)$")
    if organization then
      return ("https://dev.azure.com/%s/_usersSettings/tokens"):format(encode(organization))
    end
    return "https://dev.azure.com/"
  end,
  whoami = function(_, account)
    local organization = account:match("^[^/]+/(.+)$")
    if organization then
      return ("https://dev.azure.com/%s/_apis/projects?$top=1&api-version=7.1"):format(encode(organization))
    end
  end,
  guards = GUARDS,
  discussion = function(request)
    if not request.signed_in then
      term.forge_discussion("azure-devops", request.id, nil, "sign in to " .. request.account .. " on the Accounts page of the settings")
      return
    end
    discussion(request)
  end,
  check = function(items)
    local projects, order = {}, {}
    for _, item in ipairs(items) do
      local org, project, repo = parts(item.host, item.path)
      if org and item.signed_in then
        local key = item.account .. "\n" .. org .. "/" .. project
        if not projects[key] then
          projects[key] = { account = item.account, org = org, project = project, wanted = {} }
          order[#order + 1] = key
        end
        table.insert(projects[key].wanted, { item = item, repo = repo })
      end
    end
    for _, key in ipairs(order) do
      local p = projects[key]
      look(p.account, p.org, p.project, p.wanted)
    end
  end,
}
