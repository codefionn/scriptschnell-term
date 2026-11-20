---@meta
-- Type annotations for the scriptschnell-term extension API, for
-- lua-language-server (LuaCATS). scriptschnell-term writes this file into
-- <config>/extensions/types/ and keeps it current; the `.luarc.json` next to
-- your extensions points the language server here. Do not edit it there:
-- the next start replaces it. `README.md` in the extensions directory is
-- the reference these annotations summarize.

---@alias term.EventName
---| "app_started"
---| "project_added"
---| "worktree_activated"
---| "terminal_opened"
---| "terminal_exited"
---| "environment_ready"

---@class term.Event
---@field event term.EventName

---@class term.AppStarted: term.Event

---@class term.ProjectAdded: term.Event
---@field name string
---@field root string
---@field env string
---@field project_id string the project's id for `term.project_store`

---@class term.WorktreeActivated: term.Event
---@field project string
---@field project_id string
---@field path string
---@field branch string? nil when detached
---@field env string

---@class term.TerminalOpened: term.Event
---@field id integer
---@field cwd string?

---@class term.TerminalExited: term.Event
---@field id integer
---@field code integer? nil when the app has no exit code

---@alias term.EnvironmentKind "devcontainer" | "sandbox" | "vm" | "remote" | "ssh" | "custom"

---@class term.EnvironmentReady: term.Event
---@field project string
---@field worktree string
---@field kind term.EnvironmentKind
---@field env string
---@field host string the project's environment, the machine under a devcontainer or sandbox; "local" for this machine
---@field fresh boolean true only for a devcontainer this start created

---@class term.CommandSpec
---@field name string unique across extensions, such as "my-ext.do-thing"
---@field title? string palette title; defaults to `name`
---@field keys? string | table<string, string> one sequence for every keymap, or keymap id to sequence with `default` for the rest
---@field run fun()

---@class term.TaskSpec
---@field name string letters, digits, `-` and `_`; the task's id is "<extension>.<name>"
---@field title? string palette title; defaults to `name`
---@field every integer | string seconds, or a count with s, m, h, d or w such as "1d"; at least 60 seconds
---@field projects? string | string[] project names or root paths, `*` matches anything; leave out for a task without a project
---@field run fun(e: term.TaskRun)

---@class term.TaskWorktree
---@field path string
---@field branch? string nil when detached
---@field runtime "host" | "devcontainer" | "sandbox"

---@class term.TaskRun
---@field task string the task's id
---@field project? string nil for a task without `projects`
---@field project_id? string
---@field root? string the main worktree
---@field env? string
---@field worktrees? term.TaskWorktree[]

---@class term.OpenTerminal
---@field cwd? string
---@field cmd? string[]
---@field title? string

---@alias term.NotifyLevel "info" | "warning" | "error"

---@class term.Result
---@field ok boolean the command exited with 0, or the copy or trust worked
---@field code integer? the exit code; nil when the command did not run or was killed
---@field stdout string up to 64 KiB
---@field stderr string up to 64 KiB
---@field error string? why it did not run or failed

---@class term.Copy
---@field from string local file or directory
---@field to string directory in the worktree's environment; may start with `~`
---@field worktree? string defaults to the active worktree
---@field exclude? string[] tar patterns
---@field done? fun(r: term.Result) called once it finished; without it failures become notices

---@class term.Run
---@field cmd string[]
---@field worktree? string defaults to the active worktree
---@field cwd? string defaults to the worktree
---@field host? boolean run in the project's environment, under the devcontainer or sandbox; needs `process`
---@field done? fun(r: term.Result) called once it finished; without it failures become notices

---@class term.Trust
---@field name string names the certificate in the store: letters, digits, `-`, `_` and `.`
---@field path string PEM file on this machine
---@field worktree? string defaults to the active worktree
---@field done? fun(r: term.Result) `stdout` holds the CA bundle path of the environment

---@class term.Redirect
---@field host string host directory, created if needed
---@field sandbox string sandbox path it stands in for; must exist

---@class term.BlocklistSource
---@field name string letters, digits, `-` and `_`
---@field urls string[] https:// URLs of the same list, mirrors after the first, each on one of the extension's hosts
---@field max_age? integer seconds a cached copy lasts; a week by default

---@class term.SandboxFile
---@field sandbox string sandbox file the content stands in for; must exist on this machine
---@field content string

---@class term.Guard
---@field name string letters, digits, `-` and `_`
---@field title? string finishes "A sandbox wants to ..."
---@field hosts string[] host patterns, or "*"; a forge's guards may leave them out and get its accounts' hosts
---@field requests string[] "METHOD TARGET" or "METHOD TARGET BODY" patterns such as "PUT /api/v1/crates/new" or "POST */graphql *mutation*createPullRequest*"
---@field kind? "create"|"comment"|"merge" what it holds back on a change request; guards without a mode of their own follow the user's mode for the kind

---@class term.Sandbox
---@field hosts? string[] host names or "*.domain"
---@field writable? string[]
---@field hidden? string[]
---@field redirects? term.Redirect[]
---@field vars? table<string, string>
---@field files? term.SandboxFile[]
---@field guards? term.Guard[]
---@field blocked? string[] hosts to refuse, with their subdomains
---@field unblocked? string[] hosts no block applies to
---@field blocklists? term.BlocklistSource[] lists the app downloads; needs `network`

---@class term.Secret
---@field value string the real credential
---@field hosts string[] hosts whose requests get the real value
---@field name? string keeps the fake when the value changes, such as a refreshed token

---@alias term.Runtime "host" | "devcontainer" | "sandbox"

---@class term.Context
---@field project string?
---@field project_id string? the active project's id for `term.project_store`
---@field worktree string?
---@field branch string?
---@field env string?
---@field runtime term.Runtime?

---@alias term.Os "linux" | "macos" | "windows" | string

---A value `term.store` keeps and tools take and return: what JSON can hold.
---@alias term.Json nil | boolean | number | string | table

---@class term.Store
---@field id? string the project's id, for a store from `term.project_store`
---@field get fun(key: string): term.Json the value, nil when the key is not there
---@field set fun(key: string, value: term.Json) stores the value; nil removes the key
---@field delete fun(key: string) removes the key
---@field keys fun(prefix?: string): string[] the keys that start with prefix, sorted

---@class term.Page
---@field title string the tab's title
---@field markdown string the panel's content

---@class term.ViewSpec
---@field name string letters, digits, `-` and `_`
---@field render fun(key: string): term.Page? nil shows that the key is gone

---@class term.ToolCaller
---@field project string?
---@field project_id string? the terminal's project, for `term.project_store`
---@field worktree string?
---@field environment string `local`, `sandbox`, `container`, `vm`, `remote`, `ssh` or `custom`
---@field agent string? the agent's name on the agent list

---@class term.ToolSpec
---@field name string letters, digits, `-` and `_`, unique across extensions
---@field description string what the tool does, for the agent
---@field input? table the JSON schema of its arguments
---@field run fun(args: table, caller: term.ToolCaller): term.Json, string? return nil and a message for an error

---@class term.HttpSpec
---@field url string `https://` or `http://`, on one of the extension's hosts: manifest `hosts`, its forges' accounts and their subdomains, its `hosts` and `urls` settings
---@field method? "GET"|"POST"|"HEAD" GET by default
---@field headers? table<string, string>
---@field body? string
---@field account? string a host of one of this extension's forges: the app adds its token
---@field forge? string the forge `account` belongs to; defaults to this extension's first
---@field done? fun(res: term.HttpResponse)

---@class term.HttpResponse
---@field ok boolean true for a 2xx answer
---@field status integer 0 when no answer came
---@field headers table<string, string> lowercase names
---@field body string up to 4 MiB
---@field error string? why the request failed

---@class term.ForgeCheck
---@field account string the account the remote belongs to: a host, or `host/path` such as `github.com/acme`
---@field host string the remote's own host
---@field path string the repository path of the remote URL, without `.git`
---@field remote string the remote URL
---@field branches string[] branches to look up
---@field signed_in boolean whether the app has a token for `account`

---@class term.ForgeAccounts
---@field default? string[] accounts before the user changes them, each a host or `host/path`; a remote takes the account on its host with the longest path its repository path starts with
---@field example? string an account the add field shows, such as `github.com/acme`
---@field hint? string what an account is, shown under the add field

---@class term.ForgeSpec
---@field name string letters, digits, `-` and `_`
---@field title? string shown on the Accounts page
---@field accounts? term.ForgeAccounts the accounts the user signs in to; the Accounts page of the settings edits them
---@field aliases? table<string, string> remote host pattern (`*.domain` allowed), with an optional path to cut off, to the account host and path it stands for (`*` takes the label `*.domain` matched)
---@field auth? { header: string?, value: string? } `value` holds `{token}`, `{basic}` (Base64 of `:token`) or `{base64}` (Base64 of the token); `Authorization: Bearer {token}` by default
---@field env? string[] variables with a token for the accounts on the first account's host
---@field scopes? string what the token needs, shown under the token field
---@field token_url? fun(host: string, account: string): string? the page where the user creates a token
---@field whoami? fun(host: string, account: string): string?, string? a URL on one of the extension's hosts that names the token's owner, and the dotted field of its JSON answer
---@field check fun(items: term.ForgeCheck[])
---@field discussion? fun(request: term.DiscussionRequest) reads a change request's comments and answers with `term.forge_discussion`
---@field guards? term.Guard[] requests on change requests to hold back, on the hosts of the accounts and their subdomains; needs `guards`, and without it they are left out

---@class term.EnvironmentSpec
---@field name string the location scheme, such as `coder` for `coder://ws/path`: a lowercase letter, then lowercase letters, digits and `-`
---@field title? string shown where the app names the environment
---@field program string the program on this machine that reaches a target
---@field args string[] its arguments for commands without a terminal; `{target}` becomes the target, an argument `{script}` the shell script to run there, which goes last without one
---@field pty_args? string[] the same for terminals; `args` by default

---@class term.AgentHook
---@field event string the agent's hook event, such as `SessionStart`
---@field matcher? string the agent's matcher for the event
---@field args string arguments for the `agent-state` helper, such as `context my-agent` or `permission -`

---@class term.AgentIntegration
---@field file string the file the app changes, relative to `$HOME`
---@field hooks? term.AgentHook[] hook entries for a Claude Code style JSON file the user keeps too
---@field timeout_unit? 1|1000 hook timeouts per second: 1 for seconds (the default), 1000 for milliseconds
---@field content? string a file the app owns whole instead of `hooks`; `{helper}`, `{control}` and `{browser_mcp}` become the helpers' paths relative to `$HOME`

---@class term.AgentMcp
---@field file string the JSON file that lists MCP servers, relative to `$HOME`
---@field key? string the object that lists them; `mcpServers` by default
---@field style? "stdio"|"command"|"local" the shape of an entry; `stdio` by default

---@class term.AgentSandbox
---@field writable? string[] where the agent keeps its login, sessions and state
---@field read_only? string[] entries of `writable` that stay read-only: settings, hooks, plugins
---@field synced? string[] settings files the agent changes itself; copied into sandboxes and brought back after asking
---@field home_var? string[] `{ variable, directory }` for an agent that renames new settings over old ones
---@field login_vars? string[] variables the agent logs in with, which sandboxes keep although they look like credentials; needs the `env` permission
---@field keyring? table<string, string>[] the keyring secrets the agent logs in with, each by attributes it has, such as `{ service = "my-agent" }`; Linux sandboxes read and renew them without asking; needs the `secrets` permission
---@field guarded? table<string, string[][]> per synced file, the key paths whose changes ask; `*` matches any key

---@class term.AgentSessions
---@field dir string the directory relative to `$HOME`
---@field by? "name"|"contents" whether a session's id is in a file's name (default) or in its contents

---@class term.AgentSpec
---@field name string what its hooks pass to `agent-state context`: a lowercase letter, then lowercase letters, digits and `-`
---@field title? string shown on the Agents page and in the palette; `name` by default
---@field program? string the program that starts it; `name` by default
---@field config_dir string its configuration directory relative to `$HOME`; the agent counts as installed where it exists
---@field skills_dir? string where the app puts its skills
---@field integration? term.AgentIntegration how it reports its state; none for an agent that reports it itself
---@field mcp? term.AgentMcp where the debug browser's MCP server goes
---@field resume? string[] arguments after `program` that resume a session, in front of its id
---@field session_at_start? boolean false when its session start hook waits for the first prompt
---@field sessions? term.AgentSessions where it keeps its sessions, checked before one resumes
---@field supports? ("state"|"session"|"resume"|"messages"|"prompt"|"paste")[] what it can do; the app relies on nothing else
---@field prompt? string[] arguments after `program` that start it with a first prompt, `{prompt}` among them
---@field sandbox? term.AgentSandbox what it may change in sandboxes; needs `sandbox`
---@field npm? string[] npm packages it is published as, for update checks

---@class term.DiscussionRequest
---@field id string pass it back to `term.forge_discussion`
---@field account string
---@field host string
---@field path string
---@field remote string
---@field branch string
---@field number integer the change request's number, as `check` reported it
---@field signed_in boolean

---@class term.DiscussionComment
---@field author string
---@field body string Markdown
---@field at? string ISO 8601

---@class term.DiscussionPush
---@field commit string the branch's head after the push
---@field at? string ISO 8601
---@field before? string the head a force push replaced
---@field force? boolean

---@class term.DiscussionThread
---@field path string
---@field line? integer the line in `commit`'s version of the file
---@field side? "new"|"old" `old` for a removed line of the base
---@field commit? string the commit the comment was written on
---@field hunk? string the diff hunk above the comment; its last line is the commented one
---@field resolved? boolean
---@field outdated? boolean whether the forge calls it outdated
---@field comments term.DiscussionComment[]

---@class term.DiscussionReview
---@field author string
---@field state? "approved"|"changes"|"commented"|"pending" `pending` (the default) for a reviewer asked who gave no verdict yet

---@class term.DiscussionCheck
---@field name string
---@field state? "success"|"failed"|"running"|"pending"|"skipped"|"canceled" `pending` by default
---@field url? string

---@class term.Discussion
---@field title string
---@field url? string
---@field state? "open"|"merged"|"closed"
---@field author? string
---@field body? string Markdown
---@field draft? boolean
---@field target? string the branch it merges into
---@field conflicts? boolean whether the forge says it does not merge cleanly
---@field reviews? term.DiscussionReview[] each reviewer's latest verdict
---@field checks? term.DiscussionCheck[] CI pipelines, jobs and status checks on the head
---@field pushes? term.DiscussionPush[]
---@field comments? term.DiscussionComment[] on the change request as a whole
---@field threads? term.DiscussionThread[] on lines of code

---@class term.BranchReport
---@field account string
---@field path string
---@field branch string
---@field state? "open"|"merged"|"closed"|"none"
---@field number? integer
---@field url? string
---@field title? string

---@class term
---@field version string the app version
---@field os term.Os this machine's operating system
---@field store term.Store this extension's data for the whole app. Needs `storage`.
term = {}

---Registers a handler. Handlers run in load order.
---@overload fun(event: "app_started", handler: fun(e: term.AppStarted))
---@overload fun(event: "project_added", handler: fun(e: term.ProjectAdded))
---@overload fun(event: "worktree_activated", handler: fun(e: term.WorktreeActivated))
---@overload fun(event: "terminal_opened", handler: fun(e: term.TerminalOpened))
---@overload fun(event: "terminal_exited", handler: fun(e: term.TerminalExited))
---@overload fun(event: "environment_ready", handler: fun(e: term.EnvironmentReady))
---@param event term.EventName
---@param handler fun(e: term.Event)
function term.on(event, handler) end

---Adds or replaces a command palette entry.
---@param spec term.CommandSpec
function term.command(spec) end

---Adds or replaces a recurring task. The app runs it once per period, for
---every project `projects` matches, and remembers the last run across restarts.
---@param spec term.TaskSpec
function term.task(spec) end

---Opens a terminal. Needs the `terminal` permission.
---@param opts? term.OpenTerminal
function term.open_terminal(opts) end

---Opens a browser panel. Needs the `browser` permission.
---@param url string
function term.open_browser(url) end

---Shows a notice in the status bar.
---@param message string
---@param level? term.NotifyLevel defaults to "info"
function term.notify(message, level) end

---Types text into a terminal, the focused one without an id. Needs `terminal`.
---@param text string
---@param terminal_id? integer
function term.send_text(text, terminal_id) end

---Switches to a worktree. Needs the `worktree` permission.
---@param path string
function term.switch_worktree(path) end

---Copies a local file or directory into the worktree's environment, in the
---background. Needs the `copy` permission.
---@param spec term.Copy
function term.copy(spec) end

---Runs a command in the worktree's environment, in the background. Needs `run`.
---@param spec term.Run
function term.run(spec) end

---Adds a PEM certificate of this machine to the trust store of the worktree's
---environment: a devcontainer, VM, remote machine or SSH host. Needs `trust`.
---@param spec term.Trust
function term.trust(spec) end

---This extension's own directory in the app's cache, created on first call.
---@return string path
function term.cache_dir() end

---Replaces this extension's sandbox rules. Needs `sandbox`, `guards` for guards
---and `network` for blocklists.
---@param spec term.Sandbox
function term.sandbox(spec) end

---Returns a fake for a credential; the proxy swaps in the real value for the
---listed hosts. Needs the `secrets` permission.
---@param spec term.Secret
---@return string fake
function term.secret(spec) end

---The active project, worktree and runtime.
---@return term.Context
function term.context() end

---This extension's settings from the `[[settings]]` of its manifest.toml: the
---user's value, else the default. No permission needed.
---@return table<string, boolean|integer|string|string[]>
function term.settings() end

---This extension's data for one project, kept across runs. Needs `project_storage`.
---@param project_id? string a `project_id` from an event, the context or a tool's caller; the active project's by default
---@return term.Store
function term.project_store(project_id) end

---Adds or replaces a view: Markdown the app shows in a panel. No permission needed.
---@param spec term.ViewSpec
function term.view(spec) end

---Shows a view in a panel of the active worktree: the one that shows it with
---this key already, else a new tab.
---@param name string the view's name, or `<extension>.<name>` for another's
---@param key? string what `render` gets, "" by default
function term.open_view(name, key) end

---Renders the open panels of a view again: every one, or the one of `key`.
---@param name string
---@param key? string
function term.refresh_view(name, key) end

---Adds or replaces a tool agents call through the control socket and its MCP
---server. Needs the `agents` permission.
---@param spec term.ToolSpec
function term.tool(spec) end

---Sends an HTTP request in the background and calls `done` with the
---answer. Needs `network`.
---@param spec term.HttpSpec
function term.http(spec) end

---Registers a forge: accounts the user signs in to on the Accounts page,
---and `check`, which looks up the change requests of agent branches and
---answers with `term.forge_report`. Needs `network`.
---@param spec term.ForgeSpec
function term.forge(spec) end

---Tells the app what a forge's `check` found. `opts.retry_after` pauses the
---forge's checks for that many seconds. Needs `network`.
---@param name string the forge's name
---@param results term.BranchReport[]
---@param opts? { retry_after: integer? }
function term.forge_report(name, results, opts) end

---Answers a forge's `discussion` call: the discussion, or `nil` and why it
---could not be read. Needs `network`.
---@param name string the forge's name
---@param id string the request's `id`
---@param discussion term.Discussion?
---@param error? string
function term.forge_discussion(name, id, discussion, error) end

---Registers a scheme for project locations whose commands run through a
---program of this machine, such as `coder ssh {target} --`. Needs
---`environments`.
---@param spec term.EnvironmentSpec
function term.environment(spec) end

---Adds or replaces a coding agent the app integrates like its own: the
---Agents page installs it, the palette starts it, and its sessions resume.
---Needs `agents`.
---@param spec term.AgentSpec
function term.agent(spec) end

---Encodes a value as JSON. Empty tables become `[]`.
---@param value term.Json
---@return string
function term.json_encode(value) end

---Decodes JSON. Returns nil and a message for invalid text.
---@param text string
---@return term.Json, string?
function term.json_decode(text) end

---The name before the rename to scriptschnell-term. Use `term`.
---@deprecated
cmdterm = term
