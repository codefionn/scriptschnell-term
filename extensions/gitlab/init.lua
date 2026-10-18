-- Looks up the merge requests of agent branches on GitLab for the rail's
-- Agents tab. scriptschnell-term calls `check` with every repository and
-- branch it wants to know about, at most every few minutes and only for
-- agents that finished. This extension asks for all of a host's projects
-- in one GraphQL request, one aliased `project` field per project with
-- `mergeRequests(sourceBranches: ...)`.
--
-- An account is a host, or a host and an owner path such as
-- gitlab.com/group for the repositories under it, each with its own token.
-- The token never reaches this extension: `term.http{ account = account }`
-- has scriptschnell-term add it to requests to the account's host.

-- Projects per request; GitLab limits a query's complexity.
local BATCH = 20

local function quote(text)
  return '"' .. (text:gsub('[\\"]', "\\%0"):gsub("\n", "\\n")) .. '"'
end

local STATES = { opened = "open", merged = "merged", closed = "closed", locked = "closed" }
-- Which request of a branch wins: an open one, else the newest.
local RANK = { open = 3, merged = 2, closed = 1 }

local function query(account, items)
  local parts = { "query {" }
  for i, item in ipairs(items) do
    local branches = {}
    for b, branch in ipairs(item.branches) do
      branches[b] = quote(branch)
    end
    parts[#parts + 1] = ("p%d: project(fullPath: %s) { mergeRequests(sourceBranches: [%s], sort: CREATED_DESC, first: 100) { nodes { iid state webUrl title sourceBranch } } }")
      :format(i, quote(item.path), table.concat(branches, ", "))
  end
  parts[#parts + 1] = "}"
  term.http{
    method = "POST",
    url = "https://" .. account:match("^[^/]+") .. "/api/graphql",
    account = account,
    headers = { ["Content-Type"] = "application/json" },
    body = term.json_encode({ query = table.concat(parts, " ") }),
    done = function(res)
      if res.status == 429 then
        term.forge_report("gitlab", {}, { retry_after = tonumber(res.headers["retry-after"] or "") or 900 })
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
      for i, item in ipairs(items) do
        local project = data["p" .. i]
        if type(project) == "table" then
          local best = {}
          local nodes = project.mergeRequests and project.mergeRequests.nodes or {}
          for _, mr in ipairs(nodes) do
            local state = STATES[mr.state] or "closed"
            local known = best[mr.sourceBranch]
            if not known or RANK[state] > RANK[known.state] then
              best[mr.sourceBranch] = { state = state, mr = mr }
            end
          end
          for _, branch in ipairs(item.branches) do
            local found = best[branch]
            results[#results + 1] = {
              account = account,
              path = item.path,
              branch = branch,
              state = found and found.state or "none",
              number = found and tonumber(found.mr.iid),
              url = found and found.mr.webUrl,
              title = found and found.mr.title,
            }
          end
        end
      end
      term.forge_report("gitlab", results)
    end,
  }
end

local function encode(text)
  return (text:gsub("[^%w%-%._~]", function(c)
    return ("%%%02X"):format(c:byte())
  end))
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

-- Pipeline and job states.
local CHECKS = {
  success = "success",
  failed = "failed",
  canceled = "canceled",
  skipped = "skipped",
  running = "running",
  manual = "skipped",
}

local function user(value)
  return type(value) == "table" and text(value.username) or nil
end

-- Approvals, then reviewers who gave none.
local function reviews(mr, approvals)
  local found, seen = {}, {}
  local approved = type(approvals) == "table" and list(approvals.approved_by) or {}
  for _, a in ipairs(approved) do
    local name = user(a.user)
    if name then
      seen[name] = true
      found[#found + 1] = { author = name, state = "approved" }
    end
  end
  for _, r in ipairs(list(mr.reviewers)) do
    local name = user(r)
    if name and not seen[name] then
      found[#found + 1] = { author = name, state = "pending" }
    end
  end
  return found
end

-- The merge request, its discussions, versions (one per push) and
-- approvals in four REST requests, then the jobs of its pipeline. GitLab
-- keeps each code comment's position on the version it was written on;
-- scriptschnell-term follows it to the head.
local function discussion(request)
  local host = request.account:match("^[^/]+")
  local project = ("https://%s/api/v4/projects/%s"):format(host, encode(request.path))
  local base = ("%s/merge_requests/%d"):format(project, request.number)
  all({
    mr = { url = base, account = request.account },
    discussions = { url = base .. "/discussions?per_page=100", account = request.account },
    versions = { url = base .. "/versions?per_page=100", account = request.account },
    approvals = { url = base .. "/approvals", account = request.account },
  }, function(answers, failed)
    local mr = answers.mr
    if type(mr) ~= "table" then
      term.forge_discussion("gitlab", request.id, nil, failed or "no merge request")
      return
    end
    local comments, threads = {}, {}
    for _, d in ipairs(list(answers.discussions)) do
      local notes = {}
      for _, note in ipairs(list(d.notes)) do
        if note.system ~= true then
          notes[#notes + 1] = note
        end
      end
      local first = notes[1]
      local position = first and type(first.position) == "table" and first.position
      if position then
        local new_line = type(position.new_line) == "number" and position.new_line or nil
        local old_line = type(position.old_line) == "number" and position.old_line or nil
        local thread = {
          path = text(position.new_path) or text(position.old_path) or "",
          line = new_line or old_line,
          side = new_line and "new" or "old",
          commit = new_line and text(position.head_sha) or text(position.base_sha),
          resolved = first.resolved == true,
          comments = {},
        }
        for _, note in ipairs(notes) do
          table.insert(thread.comments, {
            author = type(note.author) == "table" and text(note.author.username) or "",
            body = text(note.body) or "",
            at = text(note.created_at),
          })
        end
        threads[#threads + 1] = thread
      else
        for _, note in ipairs(notes) do
          comments[#comments + 1] = {
            author = type(note.author) == "table" and text(note.author.username) or "",
            body = text(note.body) or "",
            at = text(note.created_at),
          }
        end
      end
    end
    local pushes = {}
    for _, version in ipairs(list(answers.versions)) do
      if text(version.head_commit_sha) then
        -- Newest first; the app sorts by time.
        pushes[#pushes + 1] = { commit = version.head_commit_sha, at = text(version.created_at) }
      end
    end
    local pipeline = type(mr.head_pipeline) == "table" and mr.head_pipeline
    local result = {
      title = text(mr.title) or "",
      url = text(mr.web_url),
      state = STATES[mr.state],
      author = type(mr.author) == "table" and text(mr.author.username) or nil,
      body = text(mr.description),
      draft = mr.draft == true or mr.work_in_progress == true,
      target = text(mr.target_branch),
      conflicts = mr.has_conflicts == true,
      reviews = reviews(mr, answers.approvals),
      checks = {},
      pushes = pushes,
      comments = comments,
      threads = threads,
    }
    if not pipeline or type(pipeline.id) ~= "number" then
      term.forge_discussion("gitlab", request.id, result)
      return
    end
    result.checks[1] = {
      name = "pipeline #" .. math.tointeger(pipeline.id),
      state = CHECKS[pipeline.status] or "pending",
      url = text(pipeline.web_url),
    }
    -- The pipeline's jobs, so a failure names its job.
    local jobs = ("%s/pipelines/%d/jobs?per_page=100"):format(project, pipeline.id)
    all({ jobs = { url = jobs, account = request.account } }, function(more)
      for _, job in ipairs(list(more.jobs)) do
        local name = text(job.name) or ""
        if text(job.stage) then
          name = job.stage .. ": " .. name
        end
        result.checks[#result.checks + 1] = {
          name = name,
          state = CHECKS[job.status] or "pending",
          url = text(job.web_url),
        }
      end
      term.forge_discussion("gitlab", request.id, result)
    end)
  end)
end

-- What sandboxes may do to merge requests. The app puts these on the hosts
-- of the accounts and their subdomains, and the user picks Ask, Block or
-- Allow per kind for every forge. The GraphQL patterns read the mutation
-- in the body.
local GUARDS = {
  {
    name = "create",
    kind = "create",
    title = "open a merge request",
    requests = {
      "POST */api/v4/projects/*/merge_requests",
      "POST */api/graphql *mutation*mergeRequestCreate*",
    },
  },
  {
    name = "comment",
    kind = "comment",
    title = "comment on or approve a merge request",
    requests = {
      "POST */api/v4/projects/*/merge_requests/*/notes",
      "POST */api/v4/projects/*/merge_requests/*/discussions",
      "POST */api/v4/projects/*/merge_requests/*/draft_notes",
      "POST */api/v4/projects/*/merge_requests/*/draft_notes/bulk_publish",
      "POST */api/v4/projects/*/merge_requests/*/approve",
      "POST */api/graphql *mutation*createNote*",
      "POST */api/graphql *mutation*createDiffNote*",
    },
  },
  {
    name = "merge",
    kind = "merge",
    title = "merge a merge request",
    requests = {
      "PUT */api/v4/projects/*/merge_requests/*/merge",
      "POST */api/v4/projects/*/merge_trains/merge_requests/*",
      "POST */api/graphql *mutation*mergeRequestAccept*",
    },
  },
}

term.forge{
  name = "gitlab",
  title = "GitLab",
  accounts = {
    default = { "gitlab.com" },
    example = "gitlab.com/acme/team",
    hint = "A host such as gitlab.com or a self-managed host, or a host and a group such as gitlab.com/acme/team for the projects under it. A project takes the account with the longest matching path.",
  },
  auth = { header = "Authorization", value = "Bearer {token}" },
  env = { "GITLAB_TOKEN" },
  scopes = "A personal access token with the read_api scope.",
  token_url = function(host)
    return "https://" .. host .. "/-/user_settings/personal_access_tokens?name=scriptschnell-term&scopes=read_api"
  end,
  whoami = function(host)
    return "https://" .. host .. "/api/v4/user", "username"
  end,
  guards = GUARDS,
  discussion = discussion,
  check = function(items)
    local by_account = {}
    for _, item in ipairs(items) do
      local list = by_account[item.account] or {}
      by_account[item.account] = list
      list[#list + 1] = item
    end
    for account, list in pairs(by_account) do
      for start = 1, #list, BATCH do
        query(account, { table.unpack(list, start, math.min(start + BATCH - 1, #list)) })
      end
    end
  end,
}
