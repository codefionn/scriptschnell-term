-- Teal declarations for the scriptschnell-term extension API.
-- scriptschnell-term writes this file into <config>/extensions/types/ and
-- keeps it current; the `tlconfig.lua` next to your extensions loads it as
-- the global environment. Do not edit it there: the next start replaces it.
-- `README.md` in the extensions directory is the reference.

global record Term
   enum EventName
      "app_started"
      "project_added"
      "worktree_activated"
      "terminal_opened"
      "terminal_exited"
      "environment_ready"
   end

   enum AppStartedName "app_started" end
   enum ProjectAddedName "project_added" end
   enum WorktreeActivatedName "worktree_activated" end
   enum TerminalOpenedName "terminal_opened" end
   enum TerminalExitedName "terminal_exited" end
   enum EnvironmentReadyName "environment_ready" end

   record AppStarted
      event: EventName
   end

   record ProjectAdded
      event: EventName
      name: string
      root: string
      env: string
      project_id: string
   end

   record WorktreeActivated
      event: EventName
      project: string
      project_id: string
      path: string
      -- nil when detached
      branch: string
      env: string
   end

   record TerminalOpened
      event: EventName
      id: integer
      cwd: string
   end

   record TerminalExited
      event: EventName
      id: integer
      -- nil when the app has no exit code
      code: integer
   end

   enum EnvironmentKind
      "devcontainer"
      "sandbox"
      "vm"
      "remote"
      "ssh"
      "custom"
   end

   record EnvironmentReady
      event: EventName
      project: string
      worktree: string
      kind: EnvironmentKind
      env: string
      -- the project's environment, the machine under a devcontainer or sandbox; "local" for this machine
      host: string
      -- true only for a devcontainer this start created
      fresh: boolean
   end

   record CommandSpec
      -- unique across extensions, such as "my-ext.do-thing"
      name: string
      title: string
      -- a string for every keymap, or keymap id to sequence (`default` for the rest)
      keys: string | {string: string}
      run: function()
   end

   record TaskWorktree
      path: string
      -- nil when detached
      branch: string
      -- "host", "devcontainer" or "sandbox"
      runtime: string
   end

   record TaskRun
      -- the task's id, "<extension>.<name>"
      task: string
      -- the rest is nil for a task without `projects`
      project: string
      project_id: string
      root: string
      env: string
      worktrees: {TaskWorktree}
   end

   record TaskSpec
      -- letters, digits, `-` and `_`
      name: string
      title: string
      -- seconds, or a count with s, m, h, d or w such as "1d"; at least 60 seconds
      every: integer | string
      -- project names or root paths, `*` matches anything
      projects: string | {string}
      run: function(e: TaskRun)
   end

   record OpenTerminal
      cwd: string
      cmd: {string}
      title: string
   end

   enum NotifyLevel
      "info"
      "warning"
      "error"
   end

   record Result
      -- the command exited with 0, or the copy or trust worked
      ok: boolean
      -- nil when the command did not run or was killed
      code: integer
      stdout: string
      stderr: string
      -- why it did not run or failed
      error: string
   end

   record Copy
      from: string
      to: string
      worktree: string
      exclude: {string}
      done: function(r: Result)
   end

   record Run
      cmd: {string}
      worktree: string
      cwd: string
      -- run in the project's environment; needs `process`
      host: boolean
      done: function(r: Result)
   end

   record Trust
      -- letters, digits, `-`, `_` and `.`
      name: string
      -- PEM file on this machine
      path: string
      worktree: string
      -- `stdout` holds the CA bundle path of the environment
      done: function(r: Result)
   end

   record Redirect
      host: string
      sandbox: string
   end

   record SandboxFile
      sandbox: string
      content: string
   end

   record Guard
      name: string
      title: string
      -- a forge's guards may leave them out and get its accounts' hosts
      hosts: {string}
      -- "METHOD TARGET" or "METHOD TARGET BODY"
      requests: {string}
      -- "create", "comment" or "merge", for a forge's guards on change requests
      kind: string
   end

   record BlocklistSource
      name: string
      urls: {string}
      max_age: integer
   end

   record Sandbox
      hosts: {string}
      writable: {string}
      hidden: {string}
      redirects: {Redirect}
      vars: {string: string}
      files: {SandboxFile}
      guards: {Guard}
      blocked: {string}
      unblocked: {string}
      blocklists: {BlocklistSource}
   end

   record Secret
      value: string
      hosts: {string}
      name: string
   end

   enum Runtime
      "host"
      "devcontainer"
      "sandbox"
   end

   record Context
      project: string
      project_id: string
      worktree: string
      branch: string
      env: string
      runtime: Runtime
   end

   -- What `store` keeps and tools take and return: what JSON can hold.
   type Json = boolean | number | string | table

   record Store
      -- the project's id, for a store from `project_store`
      id: string
      get: function(key: string): Json
      set: function(key: string, value: Json)
      delete: function(key: string)
      keys: function(prefix?: string): {string}
   end

   record Page
      title: string
      markdown: string
   end

   record ViewSpec
      name: string
      render: function(key: string): Page
   end

   record ToolCaller
      project: string
      project_id: string
      worktree: string
      environment: string
      agent: string
   end

   record ToolSpec
      name: string
      description: string
      input: table
      run: function(args: table, caller: ToolCaller): Json, string
   end

   record HttpSpec
      url: string
      method: string
      headers: {string:string}
      body: string
      account: string
      forge: string
      done: function(res: HttpResponse)
   end

   record HttpResponse
      ok: boolean
      status: integer
      headers: {string:string}
      body: string
      error: string
   end

   record ForgeCheck
      account: string
      host: string
      path: string
      remote: string
      branches: {string}
      signed_in: boolean
   end

   record ForgeAuth
      header: string
      value: string
   end

   record ForgeAccounts
      -- accounts before the user changes them on the Accounts page, each a host or "host/path"
      default: {string}
      -- an account the add field shows, such as "github.com/acme"
      example: string
      -- what an account is, shown under the add field
      hint: string
   end

   record ForgeSpec
      name: string
      title: string
      accounts: ForgeAccounts
      aliases: {string:string}
      auth: ForgeAuth
      env: {string}
      scopes: string
      token_url: function(host: string, account: string): string
      whoami: function(host: string, account: string): string, string
      check: function(items: {ForgeCheck})
      discussion: function(request: DiscussionRequest)
      -- needs the guards permission; left out without it
      guards: {Guard}
   end

   record EnvironmentSpec
      -- the location scheme, such as "coder" for coder://ws/path
      name: string
      title: string
      -- the program on this machine that reaches a target
      program: string
      -- "{target}" becomes the target, an argument "{script}" the shell script to run there
      args: {string}
      -- for terminals; args by default
      pty_args: {string}
   end

   record AgentHook
      -- the agent's hook event, such as "SessionStart"
      event: string
      matcher: string
      -- arguments for the agent-state helper, such as "context my-agent"
      args: string
   end

   record AgentIntegration
      -- relative to $HOME
      file: string
      hooks: {AgentHook}
      -- 1 for seconds, 1000 for milliseconds
      timeout_unit: integer
      -- a file the app owns whole; {helper}, {control}, {browser_mcp} become paths
      content: string
   end

   record AgentMcp
      file: string
      -- "mcpServers" by default
      key: string
      -- "stdio", "command" or "local"
      style: string
   end

   record AgentSandbox
      writable: {string}
      read_only: {string}
      synced: {string}
      -- { variable, directory }
      home_var: {string}
      -- variables the agent logs in with, which sandboxes keep; needs "env"
      login_vars: {string}
      -- keyring secrets the agent logs in with, by attributes; needs "secrets"
      keyring: {{string: string}}
      -- per synced file, key paths whose changes ask
      guarded: {string: {{string}}}
   end

   record AgentSessions
      -- relative to $HOME
      dir: string
      -- "name" (default) or "contents"
      by: string
   end

   record AgentSpec
      -- a lowercase letter, then lowercase letters, digits and "-"
      name: string
      title: string
      -- name by default
      program: string
      -- relative to $HOME
      config_dir: string
      skills_dir: string
      integration: AgentIntegration
      mcp: AgentMcp
      -- arguments after program, in front of the session id
      resume: {string}
      session_at_start: boolean
      -- checked before a saved session resumes
      sessions: AgentSessions
      -- state, session, resume, messages, prompt, paste
      supports: {string}
      -- arguments after program, "{prompt}" among them
      prompt: {string}
      -- needs the sandbox permission
      sandbox: AgentSandbox
      npm: {string}
   end

   record DiscussionRequest
      id: string
      account: string
      host: string
      path: string
      remote: string
      branch: string
      number: integer
      signed_in: boolean
   end

   record DiscussionComment
      author: string
      body: string
      at: string
   end

   record DiscussionPush
      commit: string
      at: string
      before: string
      force: boolean
   end

   record DiscussionThread
      path: string
      line: integer
      -- "new" (default) or "old" for a removed line of the base
      side: string
      commit: string
      hunk: string
      resolved: boolean
      outdated: boolean
      comments: {DiscussionComment}
   end

   record DiscussionReview
      author: string
      -- "approved", "changes", "commented" or "pending" (default)
      state: string
   end

   record DiscussionCheck
      name: string
      -- "success", "failed", "running", "pending" (default), "skipped" or "canceled"
      state: string
      url: string
   end

   record Discussion
      title: string
      url: string
      -- "open", "merged" or "closed"
      state: string
      author: string
      body: string
      draft: boolean
      -- the branch it merges into
      target: string
      conflicts: boolean
      reviews: {DiscussionReview}
      checks: {DiscussionCheck}
      pushes: {DiscussionPush}
      comments: {DiscussionComment}
      threads: {DiscussionThread}
   end

   record BranchReport
      account: string
      path: string
      branch: string
      -- "open", "merged", "closed" or "none"
      state: string
      number: integer
      url: string
      title: string
   end

   record ForgeReportOptions
      retry_after: integer
   end

   -- A value of an extension setting declared in manifest.toml.
   type SettingValue = boolean | integer | string | {string}

   version: string
   -- this machine's operating system: "linux", "macos", "windows", ...
   os: string

   on: function(event: AppStartedName, handler: function(e: AppStarted))
   on: function(event: ProjectAddedName, handler: function(e: ProjectAdded))
   on: function(event: WorktreeActivatedName, handler: function(e: WorktreeActivated))
   on: function(event: TerminalOpenedName, handler: function(e: TerminalOpened))
   on: function(event: TerminalExitedName, handler: function(e: TerminalExited))
   on: function(event: EnvironmentReadyName, handler: function(e: EnvironmentReady))
   command: function(spec: CommandSpec)
   task: function(spec: TaskSpec)
   open_terminal: function(opts?: OpenTerminal)
   open_browser: function(url: string)
   notify: function(message: string, level?: NotifyLevel)
   send_text: function(text: string, terminal_id?: integer)
   switch_worktree: function(path: string)
   copy: function(spec: Copy)
   run: function(spec: Run)
   trust: function(spec: Trust)
   cache_dir: function(): string
   sandbox: function(spec: Sandbox)
   secret: function(spec: Secret): string
   context: function(): Context
   settings: function(): {string: SettingValue}
   store: Store
   project_store: function(project_id?: string): Store
   view: function(spec: ViewSpec)
   open_view: function(name: string, key?: string)
   refresh_view: function(name: string, key?: string)
   tool: function(spec: ToolSpec)
   http: function(spec: HttpSpec)
   forge: function(spec: ForgeSpec)
   forge_report: function(name: string, results: {BranchReport}, opts?: ForgeReportOptions)
   forge_discussion: function(name: string, id: string, discussion: Discussion, error?: string)
   environment: function(spec: EnvironmentSpec)
   agent: function(spec: AgentSpec)
   json_encode: function(value: Json): string
   json_decode: function(text: string): Json, string
end

global term: Term

-- The name before the rename to scriptschnell-term. Use `term`.
global cmdterm: Term
