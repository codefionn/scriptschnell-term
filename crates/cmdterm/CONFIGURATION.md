# Configuring scriptschnell-term

scriptschnell-term keeps its files in the usual places of each platform, each in a
`scriptschnell-term` directory:

| Kind | Linux and BSD | macOS | Windows |
| --- | --- | --- | --- |
| Configuration | `$XDG_CONFIG_HOME`, else `~/.config` | `~/Library/Application Support` | `%APPDATA%` |
| Data | `$XDG_DATA_HOME`, else `~/.local/share` | `~/Library/Application Support` | `%LOCALAPPDATA%` |
| State | `$XDG_STATE_HOME`, else `~/.local/state` | `~/Library/Application Support` | `%LOCALAPPDATA%` |
| Cache | `$XDG_CACHE_HOME`, else `~/.cache` | `~/Library/Caches` | `cache\` in the data directory |
| Logs | `logs/` in the state directory | `~/Library/Logs` | `logs\` in the data directory |

`--config-dir DIR` on the command line, else `$SCRIPTSCHNELL_TERM_CONFIG_DIR` (the older
`$CMDTERM_CONFIG_DIR` still works), puts all of them into that one directory instead,
with the same file names. Use it for a scratch configuration.

| Path | What it is | Who writes it |
| --- | --- | --- |
| Configuration: `settings.json` | Preferences, described below | Settings overlay (Ctrl+,), or you |
| Configuration: `extensions/` | Your own Lua extensions | You |
| Configuration: `remote/` | Keys, trusted and allowed machines, `server.json` for VMs | Settings pages Remote machines and Virtual machines |
| Data: `CONFIGURATION.md` | This file | The app, on every start |
| Data: `extensions/` | The shipped extensions, `README.md` (the extension reference) and `types/` | The app, on every start |
| Data: `remote/` | VM images and disks | Settings page Virtual machines |
| Data: `bin/` | `scriptschnell-term-remote` and `scriptschnell-term-proxy-bridge`, copied out of the AppImage, which takes them along when it quits and which sandboxes do not see | The app, on every start from an AppImage |
| Data: `extension-data/` | What extensions keep with `term.store`, one directory per extension and one JSON file per key | Extensions |
| Data: `project-data/` | What extensions keep per project with `term.project_store`, as `<project id>/<extension>/` | Extensions |
| State: `state.json` | Projects, worktrees, panel layout per worktree | The app |
| State: `sandbox/` | Shell history, neovim state and stand-in files of sandboxes | The app |
| Cache: `blocklists/` | Downloaded blocklists | The app |
| Logs | Devcontainer, job and VM image logs | The app |

The app loads the shipped extensions and your own; one of yours replaces a shipped one of
the same name. A build without the `third-party-extensions` Cargo feature, which is on by
default, loads only the shipped extensions, as shipped, and never reads your
`extensions/`. Its control socket refuses extension changes from agents with 403. The app also writes `.luarc.json` and `tlconfig.lua` into your
`extensions/`, pointing lua-language-server and Teal at the type files.

Older versions kept everything in `~/.config/scriptschnell-term` (before that
`~/.config/cmdterm`). The first start of this version moves each file to its place, unless
one is already there. A shipped extension you changed stays in your `extensions/` as
your own, and VM images and disks leave a symlink at their old path, because
`server.json` names them by absolute path.

The app reads `settings.json` at start and writes it whenever the settings overlay changes
something. Edit it by hand while the app is closed, or press Reload extensions afterwards
for extension-related keys. A missing key takes its default, so a trimmed file still
loads. A file that does not parse is reported and the defaults are used for this run.
Reset all settings, at the end of the Project page in the settings overlay, writes the defaults after a
second click, stops serving this machine and opens the first-start setup again.

Sandboxed terminals cannot read these directories, except `CONFIGURATION.md` and
`extensions/` of the data directory and your `extensions/`, which are mounted read-only.
A directory inside a path the sandbox may write, such as the cache inside `~/.cache`,
stays hidden there. Windows sandboxes cannot hide it, so there it is read-only.

## settings.json

```json
{
  "font_size": 14.0,
  "font_family": "Iosevka",
  "color_scheme": "system",
  "language": "system",
  "worktree_titles": "agent",
  "window_frame": "app",
  "jj": false,
  "keymap": "cmdterm",
  "keybindings": { "ctrl+b |": "split_side_by_side", "ctrl+b %": null },
  "disabled_extensions": ["dotfiles"],
  "enabled_extensions": ["aspnet-devcerts"],
  "extension_permissions": { "npm": ["env", "filesystem", "sandbox", "secrets", "guards"] },
  "extension_filter": "all",
  "extension_settings": { "dotfiles": { "files": [".gitconfig", ".tmux.conf"] } },
  "forge_hosts": { "forgejo.forgejo": ["codeberg.org", "git.example.org"] },
  "container_engine": "podman",
  "default_runtime": "sandbox",
  "sandbox": {
    "writable": ["~/.cache"],
    "hidden": ["~/.ssh", "~/.gnupg"],
    "pass_env": ["HF_TOKEN"],
    "network": true,
    "restrict_network": true,
    "host_network": false,
    "allowed_hosts": ["github.com", "*.github.com", "crates.io"],
    "intercept_tls": true,
    "hosts_chosen": true,
    "engines": "passthrough",
    "sidecar": { "engine": "podman" },
    "guards": { "cmdterm.git-push": "allow", "npm.publish": "block" },
    "change_requests": { "create": "ask", "comment": "allow", "merge": "block" }
  },
  "remote": { "serve": false, "listen": "0.0.0.0:7722", "fused": [] },
  "vpn": { "nebula_config": "/etc/nebula/config.yml" },
  "check_updates": true,
  "auto_update": true,
  "freeze_idle_agents": 15,
  "cpu_mode": { "local": "balanced", "remote": "off", "efficiency_cores": true },
  "agents_use_bash": true,
  "default_agent": "claude",
  "agent_control": "host",
  "debug_browser": { "browser": "auto", "headless": true, "agents": "local" }
}
```

### Appearance

- `font_size`: terminal font size in logical pixels. `--font-size` overrides it for one run.
- `font_family`: an installed monospace family. Leave it out for the bundled face.
- `color_scheme`: `system` (follows the desktop's light or dark mode), `spruce`,
  `spruce-light`, `gruvbox-dark`, `solarized-light` or `nord`.
- `language`: `en`, `de`, or `system` for the system locale.
- `worktree_titles`: what the sidebar names a worktree after. `agent`, the default, shows
  the window title the last agent session in its terminals gave itself, such as Claude
  Code's name for the task, with the branch under it. A worktree where no agent ran yet
  shows its branch. `branch` always shows the branch. The palette switches between them.
  Titles count from the first state an agent's hooks report (Settings, Agents) or the
  agent reports itself (OSC 7501) until its session ends, so a shell's title never
  replaces the agent's. Each worktree keeps its last 8 titles in `state.json`.
- `window_frame`: who draws the window's title bar. `app`, the default, opens the window
  without the system's frame: the tab strip ends in Minimize, Maximize (Restore while
  maximized) and Close buttons, a press on its empty part moves the window, a double press
  there maximizes it, and a 4 px band along the window's edges resizes it. `system` lets
  the window system draw the title bar and the border. The palette switches between them,
  and the window follows at once.

### Jujutsu

- `jj`: off by default. On, the app reads a repository that has a Jujutsu workspace with
  `jj`, also when git shares the directory (a colocated repository). Worktrees are then
  jj workspaces and branches are bookmarks. A machine without `jj` falls back to git or
  Mercurial. A new clone also becomes a colocated jj repository (`jj git init
  --colocate`). One project can choose for itself with `jj` in `state.json`; the palette
  has Use jj for jj repositories and Use jj in this project, and their Stop using
  counterparts.
- With jj, a worktree's revision is the parent of its working-copy commit (`@-`), so a
  new worktree starts there and leaves the working copy's changes behind. A new branch is
  a bookmark on the new worktree's working-copy commit. The sidebar shows a bookmark at
  `@`, else one at `@-`, and the change id when there is none. The status poll snapshots
  the working copy like `jj status`, so it counts new files as changes; jj tracks them
  itself, so nothing shows as untracked. Removing a worktree runs `jj workspace forget`
  and deletes the directory; its last snapshot stays in the repository as a commit.
  Review compares against `trunk()`, and leaves out the working-copy commit unless W adds
  it.

### Sidebar search

The field at the top of the sidebar (`search_worktrees`, Ctrl+Shift+S in the `cmdterm`
keymap) filters the sidebar as you type: only the projects and worktrees that match
stay, best match first, with the usual rows and status. It
reads each worktree's agent session titles, branch, project, folder, path, change request
and what its agents wait for. Words match when they are equal, share a stem ("fixing"
and "fixed"), one starts the other, they mean the same thing in a coding project ("bug"
and "fix", "login" and "auth", "Suche" and "search"), they are one or two typos apart,
or the word is a field's initials (`frs` for `feature/rail-search`). Filler words such
as "the" or "wo" are left out, and half of the remaining words have to match. Up and
Down move the marked worktree, Enter opens it and clears the field, Escape clears it.

### Keyboard

- `keymap`: the preset. `cmdterm` (Ctrl+Shift shortcuts that leave every control code to
  the shell), `tmux` (Ctrl+B prefix), `vscode`, `cursor`, `windows-terminal` or `konsole`.
- `keybindings`: your own bindings on top of the preset and of extension bindings. The key
  is a sequence such as `"ctrl+alt+g"` or `"ctrl+b g"` (two strokes). The value is an
  action, or `null` to remove a binding.

Actions: `palette`, `settings`, `search_worktrees`, `toggle_sidebar`, `focus_tabs`, `focus_back`, `new_terminal`, `new_agent`, `files`, `explore`, `review`, `monitor`,
`tasks`, `debug_browser`, `close_panel`, `close_pane`, `close_tab`, `move_pane_to_tab`, `split_side_by_side`,
`split_stacked`, `focus_next_pane`, `focus_previous_pane`, `grow_pane`, `shrink_pane`, `next_tab`, `previous_tab`,
`tab_1` to `tab_9`, `pane_1` to `pane_8`, `send_text:<text>` and
`command:<extension command name>`.

The Keyboard page of the settings lists every binding in effect and where it comes from.
Clicking a binding there records the next key as its new one, Escape cancels. The page
writes the key into `keybindings` and sets the action's old keys to `null`, or drops them
when they were your own.

### Extensions

- `disabled_extensions`: extensions that are listed but not loaded.
- `enabled_extensions`: extensions that are off until turned on, because their
  `manifest.toml` says `enabled = false`, and that you turned on. The shipped
  `aspnet-devcerts` is one of them. A name in `disabled_extensions` stays off.
- `extension_permissions`: what you granted each extension, the exact set of
  permissions its `manifest.toml` declares that are on. The extension runs with just
  those; while none of the declared ones are on it is listed as waiting and not
  loaded. Permissions are `terminal`, `browser`, `worktree`, `copy`, `run`, `trust`, `sandbox`, `secrets`, `guards`,
  `network`, `filesystem`, `process` and `env`; `extensions/README.md` says what each one unlocks.
  Shipped extensions have what their shipped manifest declares until you set an entry;
  changing one pins the set to exactly what you leave on.
- `extension_filter`: which extensions the Extensions page lists: `all` (the default),
  `active` (every extension not turned off) or `disabled`.
- `extension_settings`: values of the settings an extension declares under
  `[[settings]]` in its `manifest.toml`, per extension and key. A missing key takes the
  manifest's default. Each extension's page of the settings edits them, and each shipped
  extension's `README.md` lists its keys.
- `forge_hosts`: the accounts of each forge, per forge id (`<extension>.<forge>`), as
  the Accounts page of the settings (Ctrl+, then Accounts) edits them, such as
  `"forgejo.forgejo": ["codeberg.org", "git.example.org"]`. A forge without an entry has
  the accounts its extension names. Values that older versions kept under
  `extension_settings` (`hosts` of `github`, `gitlab`, `forgejo` and `gitea`, `accounts`
  of `bitbucket` and `azure-devops`) move here on load.
- `forge_accounts`: forge accounts signed in to on the Accounts page, from
  `<extension>.<forge>/<account>` to the user name the forge gave, such as
  `"github.github/github.com": "octocat"` or `"github.github/github.com/acme": "acme-bot"`
  for an account that holds only the repositories of one owner. The tokens are in the system's secret storage
  under the service `scriptschnell-term-forge` and that key, never in this file. Sign
  out on the Accounts page removes both.

The rail's Agents tab lists the worktrees where agent sessions ran: finished ones whose
branch is not merged on top, working and merged ones in collapsed groups. Forge
extensions (`github`, `gitlab`, `bitbucket`, `forgejo`, `gitea`, `azure-devops`) tell it which
branches are merged. The button beside a row with a change request opens its comments in
a read-only panel, with each code comment placed on the worktree's current head, also
after force pushes.

### Sandboxes and containers

A worktree's terminals run on the host, in its devcontainer, or in a sandbox. Settings are
not per project except on the Project page: the Sandboxing page sets `default_runtime` for
every project, a project can choose for itself on its Project page (`runtime` in
`state.json`), and the palette switches one worktree. The worktree keeps the choice in
`state.json`.

- `default_runtime`: where a worktree the app opens for the first time runs: `sandbox`,
  `devcontainer` or `host`, unless its project chose another. The Sandboxing page
  picks it, and changing it moves only worktrees opened afterwards. The default is `sandbox`, except on Windows, where it is
  `host` (see "Why Windows runs on the host" below). With `devcontainer`, a worktree without a
  `devcontainer.json` uses that default. A project's choice moves the worktrees it
  knows when it is picked, each where it fits: a devcontainer only where the worktree
  has a `devcontainer.json`, the others take `default_runtime`. The first-start setup asks for it. A worktree
  keeps its runtime in `state.json` only when it differs from that default, so on
  Windows a worktree saved in a sandbox before this default changed now runs on the host;
  pick the sandbox for it again in its runtime menu to keep it there.

- `container_engine`: `docker`, `podman` or a path. Leave it out for docker when
  installed, else podman.
- `sandbox.writable`: extra paths sandboxes may write. The worktree is always writable,
  and so is the state of coding agents: `~/.claude`, `~/.claude.json`, `~/.codex`,
  `~/.local/share/opencode`, `~/.local/state/opencode`, `~/.pi/agent`, `~/.copilot`,
  `~/.cache/copilot`, `~/.qwen`, `~/.gemini` (with Antigravity CLI's
  `~/.gemini/antigravity-cli`), `~/.config/antigravity`, `~/.kimi-code` and `~/.vibe`. Their settings,
  hooks, plugins, skills and instruction files stay read-only, because the agents on the
  host run what those name. Windows sandboxes deny writes to these too. On Linux and
  Windows a sandbox can keep only an existing path read-only, so when one is missing in
  an agent's directory, the app creates it first: an empty directory, an empty Markdown or
  TOML file, or a JSON file with no settings (`{}`, or `{"mcpServers": {}}` for MCP
  configurations and `{"providers": {}}` for pi's `models.json`). Neovim's state
  directory is replaced by one per project.
- The repository's metadata stays writable, so git and hg work in a sandbox, but not
  the parts that make git and hg run commands or lead them to other metadata, since the
  app runs them on this machine for the sidebar and the explore panel: `.git/config`, `.git/config.worktree`,
  `.git/hooks`, `.git/commondir`, the list of worktrees in `.git/worktrees` (the
  sandbox's own worktree keeps its entry, except `commondir`, `gitdir` and
  `config.worktree`), a linked worktree's `.git` file, and hg's `.hg/hgrc`,
  `.hg/hgrc-not-shared`, `.hg/requires`, `.hg/sharedpath` and `.hg/cmdterm-shares`. A git
  directory outside the worktree, such as one made with `--separate-git-dir` or a
  submodule's checkout opened as a project, gets the same, and so does the directory a
  `.git` symlink leads to. bubblewrap cannot mount over the link itself, so a sandbox can
  replace it, and a sandbox can rewrite the `.git` file of a linked worktree inside its
  own worktree. So the app's git commands never read a worktree's `.git`: they read the
  common git directory the app recorded when the project was added (`git_dir` in
  `state.json`; see below for projects added with an older version), and a
  linked worktree's directory from that directory's list of worktrees. Missing files are created empty first, except `commondir`, which
  git reads in every git directory and which is created holding `.`, the directory
  itself: without it a sandbox could point the app's git commands at metadata of its
  own. On macOS nothing is created, since the sandbox denies writes to missing paths
  too. In a project on another machine that machine creates them before the sandbox
  starts. The `.git` and `.hg` directories cannot be renamed or replaced. So in a sandbox
  `git config`, `git remote add`, `git branch --set-upstream-to`, `git submodule init`
  and `git worktree add` fail, `git push -u` pushes but cannot record the upstream, and
  `git branch -m` renames the branch but exits with 128 because it cannot update the
  configuration. Pass a setting for one command with `git -c key=value` or
  `GIT_CONFIG_COUNT`, push with `git push origin HEAD`, or run the command in a terminal
  on the host. A submodule's own configuration stays writable, so the app's git commands
  run no fsmonitor command and no hook, and do not look for uncommitted changes inside
  submodules: a submodule counts as changed in the sidebar when its commit changes, not
  when its files do. Removing a worktree from the app refuses one with changes or with a
  checked out submodule, as git does, but the app checks that itself with the recorded
  metadata, before git looks inside it. In a project on another machine the app does not
  read the `.git` file when a sandbox starts. It protects the recorded git directory
  instead, so a separate git directory is protected there too, and a linked worktree's
  entry in that directory's `worktrees` list as the last poll found it. Before the first
  poll it takes the entry to be named after the worktree, as `git worktree add` names
  it; if it is named otherwise, git in that sandbox cannot write it until the terminal
  starts again. A local project added with an older version gets its git directory
  recorded when the app starts, before any sandbox. One on another machine gets it at
  its first poll; until then its sandboxes keep the main worktree's `.git` in place and
  do not start when it is a file or a link.
  The app's own git commands are safe from a replaced `.git`, but git you run yourself
  in a terminal on the host, and shell prompts and editors that run git there, still
  follow it. When a poll finds a worktree's `.git` on this machine leading elsewhere
  than the recorded metadata, the app shows a notice naming the worktree. Do not run git
  there on the host until you have looked: `ls -la .git` shows what it is, `git
  rev-parse --git-dir` where it leads. Put the link or the `.git` file back, or run git
  with `GIT_DIR` set to the recorded directory (`git_dir` in `state.json`). jj keeps a
  repository's settings in `~/.config/jj/repos`, outside the sandbox's reach; versions
  of jj that still read `.jj/repo/config.toml` get no protection.
- `sandbox.hidden`: paths sandboxes cannot read. The default list covers SSH and GPG keys,
  cloud credentials, `~/.netrc`, `~/.git-credentials`, password stores and this directory.
  Where a hidden path and a writable or project folder lie inside each other, the deeper
  one wins: a hidden directory inside the worktree stays hidden, and a writable folder
  inside a hidden directory shows through.
  On Windows, the restricted token cannot limit reads, so only an AppContainer hides them,
  and only inside directories it may read or write (for example credentials in `.cargo`).
  Hiding a path is the only way to keep a sandbox from a Unix socket in it. The read-only
  file system does not stop a program from connecting to one, so the system bus
  (`/run/dbus`) and sockets in the home directory, such as `~/.1password/agent.sock`,
  stay reachable unless you add them here. `XDG_RUNTIME_DIR`, with the session bus, the
  Wayland socket and the SSH and GPG agents, is always hidden, and `/tmp`, with the X11
  sockets, is the sandbox's own.
- Sandboxes on this machine inherit the app's environment variables, minus a few.
  `SSH_AUTH_SOCK`, `GPG_AGENT_INFO` and `DBUS_SESSION_BUS_ADDRESS` always go, and so do
  the engines' variables (see `sandbox.engines`). Every variable whose name looks like a
  credential goes too. A name counts when it contains `TOKEN`, `SECRET`, `PASSWORD`,
  `PASSWD`, `API_KEY`, `APIKEY`, `ACCESS_KEY`, `PRIVATE_KEY` or `CREDENTIAL`, in any case,
  when it is `AZURE_DEVOPS_EXT_PAT`, or when a word of it between underscores contains
  `AUTH`. Words with `AUTHOR` in them
  don't count, so `GIT_AUTHOR_NAME` and `XAUTHORITY` stay. That removes `GITHUB_TOKEN`,
  `GH_TOKEN`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
  `NPM_TOKEN`, `CARGO_REGISTRY_TOKEN`, `HF_TOKEN` and `GOOGLE_APPLICATION_CREDENTIALS`,
  among others. Only the name is checked: a password inside `DATABASE_URL`, or a key in
  a variable named `STRIPE_KEY`, still gets in. Start the app without those.
- Agents keep the variables they log in with: `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN` and `CLAUDE_CODE_OAUTH_TOKEN` (Claude Code), `OPENAI_API_KEY`
  and `CODEX_API_KEY` (Codex), `OPENCODE_API_KEY` and `OPENROUTER_API_KEY` (OpenCode),
  `ANTHROPIC_OAUTH_TOKEN` (Pi; both also read the Anthropic, OpenAI and Gemini keys),
  `COPILOT_GITHUB_TOKEN` (Copilot CLI), `DASHSCOPE_API_KEY` (Qwen Code, which also reads
  `OPENAI_API_KEY`), `GEMINI_API_KEY` and `GOOGLE_API_KEY` (Gemini CLI), `KIMI_API_KEY`
  and `MOONSHOT_API_KEY` (Kimi) and `MISTRAL_API_KEY` (Mistral Vibe). Antigravity CLI
  keeps a Google login in the keyring (see `cmdterm.keyring` below) and reads
  `GEMINI_API_KEY` only with `modelProvider` set to `gemini`. This is a trade-off.
  The app builds one sandbox per worktree before it knows which agent a terminal will
  run, so every one of these that you set reaches every command in every sandbox, not
  only the agent that needs it. Start the app without the keys of agents you don't use.
  Copilot CLI logged in through `GH_TOKEN` or `GITHUB_TOKEN`, Claude Code on Bedrock
  with AWS keys and Gemini CLI on Vertex with `GOOGLE_APPLICATION_CREDENTIALS` need
  `sandbox.pass_env`. Agents that extensions add keep the variables they name in
  `sandbox.login_vars` of `term.agent`.
- Variables the app sets for a sandbox stay even when their names look like
  credentials: the proxy's, `SCRIPTSCHNELL_TERM_TOKEN`, a project's or worktree's
  `env_vars`, and the
  fake credentials extensions hand out (`term.secret` in the extension reference),
  which the proxy swaps for the real ones on the way out. A fake credential is the safer
  way to give sandboxes a login, since the real one never enters them.
- Sandboxes on another machine start from that machine's environment. A shell there
  removes the variables that look like credentials by the same rule, keeping the same
  names, before it starts bubblewrap. It lists them with `awk`; without `awk` the
  sandbox does not start. A name the shell cannot `unset`, such as `MY-TOKEN`, goes to
  bubblewrap's `--unsetenv` instead. Such a name cannot be kept with `sandbox.pass_env`
  there.
- `sandbox.pass_env`: names of variables that sandboxes keep although they look like
  credentials, such as `["HF_TOKEN"]`, compared ignoring case. Empty by default.
- Seatbelt on macOS starts from a profile that allows what it does not deny. It denies
  asking LaunchServices, Apple Events and launchd to start programs, which would run
  outside the sandbox, so `open`, `osascript`, `launchctl` and adding login items fail
  in a sandbox. Whether `network` is on or off, programs in it connect only to the Unix
  sockets the app passes in (its control socket, container engines), sockets in writable
  folders and in the shared temporary directories (`/tmp`, `/var/folders`), and the
  system's DNS and log sockets. In the temporary directories, launchd's sockets (the SSH
  agent, XQuartz) and tmux servers are denied, but a socket another program of yours
  keeps there, such as an editor's, stays reachable. Programs in a sandbox cannot look
  at processes outside it, and cannot read any process's arguments or environment, so
  one project's sandbox cannot read another's proxy password. `ps` and `pgrep -f` show
  no arguments there, not even of the sandbox's own processes. The window server is out
  of reach too, so a sandbox cannot post keystrokes or clicks to your windows; GUI
  programs started in a sandbox, including a headed browser for tests, cannot open a
  window. Run them headless.
- macOS versions differ in which sandbox rules they know, and `sandbox-exec` refuses a
  profile with one it does not know. When it starts, the app tries the full profile
  once with `/usr/bin/true`, in the background. If it is refused, the app tries each
  group of rules (launch, mach services, window server, Unix sockets, process info,
  process arguments) on its own, keeps the ones that load and shows a notice naming the
  dropped ones. A sandboxed terminal that opens before the probe is done waits for it up
  to two seconds, then starts with every group; so do later ones until the probe ends.
  The app probes again after a restart.
- Windows sandboxes run each command on a desktop of its own, which you never see, in a
  job object. Windows they open stay invisible there, and they cannot send keystrokes or
  clicks to your windows or hook them. No window messages, DDE or global atoms reach
  programs outside the job, and it cannot log you off, switch desktops or change system
  and display settings. If a command cannot connect to its desktop, the launcher runs it
  again on your desktop and warns once, in the terminal. An AppContainer
  (`windows_appcontainer`) shares your desktop, but runs at low integrity, which keeps
  its input from your windows. The clipboard stays open, because pasting an image into
  an agent (Alt+V or Ctrl+V in Claude Code) reads it from inside the sandbox, so a
  program there can read what you copy. COM servers that already run with your full
  rights, such as Explorer's, still take calls from a sandbox outside an AppContainer.
- A tool in a sandbox that opens a browser for a login fails on macOS and Windows, or
  starts the browser inside the sandbox: `open`, `start` and `explorer` cannot hand the
  URL to a browser outside. Ctrl+click the URL the tool prints instead; the app opens
  it in a browser panel, outside the sandbox. The control socket has no request that
  opens a URL.
- One project can add folders for its own sandboxes on the Project page of the settings
  (palette: Project settings). They land in `state.json`, see `sandbox` there.
- `sandbox.network`: off cuts the network entirely.
- `sandbox.restrict_network`: with `network` on, sandboxes reach only `allowed_hosts`,
  through a filtering proxy in the app. On by default. The proxy listens on a port on
  `127.0.0.1` per project, and every program on this machine can connect to it, so each
  port takes a random password. Sandboxes get it in the proxy variables
  (`HTTP_PROXY=http://cmdterm:PASSWORD@127.0.0.1:PORT`), and curl, git, npm, pip, cargo,
  Node and Python send it as `Proxy-Authorization`. A connection without it gets `407`,
  and the proxy never passes the header on. A new password is made each time the app
  starts. A local bubblewrap sandbox with a network of its own reaches the proxy through a
  socket bound into it and gets no password. bubblewrap gets the proxy variables, and the
  other variables the app sets for the sandbox, through its environment and not on its
  command line, which other users of this machine can read. The stand-ins for shell
  startup files (see `allowed_hosts`) hold the password, so the app writes them readable
  by you only.
- `sandbox.host_network`: with `network` on and `restrict_network` off, a local bubblewrap sandbox shares this
  machine's network and ports. Off by default: the sandbox has a loopback of its own, so
  servers in it bind no ports on this machine, and only proxy-aware traffic (HTTP, HTTPS
  and CONNECT) leaves through the app's filtering proxy, which allows every host when
  `restrict_network` is off, and so does git over SSH (see `sandbox.allowed_hosts`).
  Other SSH, raw TCP and UDP do not get out. The app forwards a
  sandbox's listening ports to the other terminals of the same worktree, and to this
  machine when you expose one (palette entries "Expose port N" and "Stop exposing port
  N", or `control expose`). Without `network`, the sandbox also has its own loopback and
  forwarding works. Sandboxes on another machine, Seatbelt on macOS and the Windows
  launcher ignore it and keep their network (see `windows_appcontainer` for Windows).
  Terminals already open keep the network they started with. With `host_network` the
  sandbox also shares this machine's abstract Unix sockets, which belong to the network
  and not to the file system, so hiding paths does not cover them. X11's
  `@/tmp/.X11-unix/X0` would let programs in the sandbox open windows on your display
  and type into them. So the app restricts such a sandbox with Landlock (Linux 6.12 or
  later): programs in it cannot connect to abstract sockets bound outside it, or send
  signals to processes outside it. GUI programs in the sandbox then find no display.
  On older kernels, or with Landlock turned off at boot, the sandbox starts without this
  restriction and the abstract sockets stay reachable. The app then shows a notice once.
  Landlock does not restrict binding. A program in the sandbox can still bind an
  abstract name that nothing holds yet, and a program outside that connects to that name
  later talks to the sandbox. For example, when no X server runs, as in a Wayland
  session without Xwayland, a sandboxed program can bind `@/tmp/.X11-unix/X0`. The next
  X11 program you start outside the sandbox connects to it, so the sandbox sees what
  that program draws and copies, and can send it fake input.
  Sandboxes on a remote machine or a VM always keep that machine's network and its
  abstract sockets. A scriptschnell-term server there (`scriptschnell-term-remote serve`, or an app
  that serves its machine) restricts them the same way when that machine's kernel has
  Landlock ABI 6. Older servers and kernels start them without the restriction, and no
  notice says so. Sandboxes on a host reached over SSH, or on a host of an extension's
  environment, keep its network too. The first sandbox there makes the app copy its
  `scriptschnell-term-proxy-bridge` to `${XDG_CACHE_HOME:-~/.cache}/scriptschnell-term/bin/` on that
  host, in a folder named after the program's SHA-256, readable by you alone. It skips
  the copy when the file there has the same SHA-256, and replaces it when not. Later
  sandboxes run that copy to restrict themselves as above, after checking its SHA-256
  again; a copy that changed is not run, and the sandbox prints a warning and starts
  without the restriction. The copy happens in the background, so sandboxes that start
  before it is done start without it. Hosts that do not run Linux on this machine's
  processor architecture, hosts whose C library is too old for the program, and app
  builds without a `scriptschnell-term-proxy-bridge` beside the app (a plain `cargo build -p
  cmdterm`) get no copy, and their sandboxes start without the restriction.
- `sandbox.windows_appcontainer`: experimental, Windows only, off by default. The
  `scriptschnell-term-sandbox` launcher runs commands in an AppContainer (profile
  `cmdterm-sandbox-<project id>`) instead of under a write-restricted token. Writable
  paths are granted to the AppContainer, and so are read and run rights on the toolchain
  directories of your user profile (`.cargo`, `.rustup`, `.dotnet`, `.nuget`, `.nvm`,
  `.volta`, `.deno`, `scoop`, `AppData\Local\Programs`) and on the proxy's CA bundle.
  Hidden paths inside those directories are denied to the AppContainer. The
  AppContainer has no network capability, so it reaches only the filtering proxy and the
  port hub on this machine's loopback, which needs a loopback exemption. Setting the
  exemption needs an administrator once. Without it, or when the profile cannot be
  created, the launcher prints the reason and uses the restricted token. A path that
  cannot be granted is reported and skipped, and the command still runs in the
  AppContainer. Ports that a
  command listens on show up in the palette like in a Linux sandbox, but they are not
  mirrored into other terminals, because all of them share the host's loopback. Windows
  lets connections into an AppContainer only with a server capability, which the launcher
  does not grant because it would also open the local network, so exposing a port and
  opening it in the debug browser may fail. Whatever
  the mode, Windows sandboxes get the proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, CA
  bundle variables) when the settings need the proxy. Programs that ignore them are not
  held back unless the AppContainer is on. The setting does not apply with
  `host_network`, or when the proxy is not running and `network` is on: those sandboxes
  keep the host's network under the restricted token.

  In both modes each project gets its own Windows sandbox identity
  (`cmdterm-sandbox-<project id>`) and its own temp dir, `%TEMP%\cmdterm-sandbox\cmdterm-sandbox-<project id>`.
  A sandbox can write only the paths granted to its project, not those of other projects.
  The launcher removes the grants of the old shared identity from the paths it handles.
  Grants are written to the file system and stay when you remove a path from the settings.
- `sandbox.allowed_hosts`: names or `*.example.com`, which matches subdomains but not
  `example.com` itself. The default allows forges (GitHub, GitLab, Codeberg, Bitbucket),
  search engines, search APIs (Exa, Tavily, Brave, Bing, Serper, SerpApi, Jina, Firecrawl,
  Perplexity), AI model APIs (with the endpoints of Copilot CLI, Gemini CLI, Antigravity
  CLI, Qwen Code and Kimi Code), documentation sites, the Rust, npm, PyPI and Go
  registries, and `localhost`, `127.0.0.1` and `::1`, which the proxy reaches on this
  machine. Only such a name, listed exactly, reaches this machine's loopback or a
  link-local address such as a cloud's metadata service: allowing every host
  (`restrict_network` off) does not cover them, and neither does an allowed name that
  resolves to one. Sandboxes still send loopback requests past the proxy by default (`NO_PROXY`),
  so those reach servers inside the sandbox. On Linux, a system startup file that sets
  proxy variables of its own (such as `/etc/profile.d/proxy.sh`) gets a stand-in inside
  the sandbox that sets the sandbox's proxy again at its end, so login shells, such as
  the `bash -lc` Codex runs commands in, keep using the sandbox's proxy. The first-start dialog also offers Maven and Gradle, NuGet, RubyGems and
  Packagist, container registries, and Debian, Ubuntu and Alpine packages.
  While `api.anthropic.com` is allowed, so are `platform.claude.com` and
  `console.anthropic.com`, without being listed here. Claude Code logs in and renews its
  login there. Without them a sandboxed Claude Code cannot finish `/login`, and its login
  ends once the access token expires.
- Git over SSH reaches allowed hosts from local bubblewrap sandboxes with a network of
  their own and from Seatbelt sandboxes on macOS, when the sandbox has the proxy. These
  sandboxes get `GIT_SSH_COMMAND`, which runs `scriptschnell-term-proxy-bridge --git-ssh` in place of
  `ssh`, and `GIT_SSH_VARIANT=ssh`. The bridge hands git's remote command to the proxy,
  and the proxy runs this machine's `ssh` for it, with your SSH configuration, agent and
  known hosts, so your keys never enter the sandbox. Only `git-upload-pack`,
  `git-receive-pack` and `git-upload-archive` pass. Host aliases work: the proxy asks
  `ssh -G` where the host leads, and that name must be in `allowed_hosts` and on no
  blocklist. SSH from a sandbox never reaches this machine or a link-local address, even
  with `localhost` allowed. Without a `ProxyCommand` or `ProxyJump` for the host, the
  proxy resolves the name itself and `ssh` connects to that address. `ssh` runs with
  `BatchMode=yes`, so keys must come from your agent or be unencrypted, and the host must
  already be in your known hosts: run `ssh -T git@HOST` once outside the sandbox to add
  it. Guards see the command as the HTTP request of the same service, such as `POST
  /org/repo.git/git-receive-pack`, so a push asks under `cmdterm.git-push` and under
  extension guards on pushes, as one over HTTPS does, and an answer for a repository
  covers both. `ssh` itself, an SSH shell and `hg` over SSH still fail in a
  sandbox, and so do Windows sandboxes, sandboxes with `host_network` and sandboxes on
  other machines, which keep their own SSH. A repository's `core.sshCommand` gives way
  to `GIT_SSH_COMMAND`.
- `sandbox.intercept_tls`: the proxy also decrypts HTTPS, with a CA that sandboxes trust.
  On by default. Sandboxes trust the CA through `SSL_CERT_FILE`, `CURL_CA_BUNDLE`,
  `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, `GIT_SSL_CAINFO`, `CARGO_HTTP_CAINFO`,
  `DENO_CERT` and `PIP_CERT`. Bubblewrap sandboxes also see the proxy's bundle in place
  of the system's CA bundle files (`/etc/ssl/certs/ca-certificates.crt`, `/etc/ssl/cert.pem`
  and their Fedora and openSUSE counterparts), for programs that read only those, such as
  appimagetool. On Windows, git is also switched to its OpenSSL backend
  (`GIT_CONFIG_COUNT` with `http.sslBackend=openssl`), because the schannel backend
  ignores `GIT_SSL_CAINFO`. Programs on Windows' own TLS stack, such as .NET, PowerShell
  and winget, trust only the Windows certificate store, which never gets the proxy's CA.
  So on Windows the setting has no effect, and the proxy only decrypts the hosts that
  extensions' secrets and guards need; those programs fail on those hosts. When a
  program turns down the proxy's certificate, `control sandbox` lists the host under
  `recently_refused`.
- `sandbox.hosts_chosen`: `true` once the first-start setup was finished. While it is
  `false`, every start opens it. The setup asks for the language, keymap and color scheme,
  `default_runtime`, which coding agents get hooks, and, when new worktrees run in
  sandboxes that reach allowed hosts only, which common hosts to allow.
- `sandbox.engines`: `passthrough` (the default), `sidecar` or `off`. With `passthrough`, sandboxes of
  local projects reach this machine's container engines directly: docker (rootful,
  rootless, Docker Desktop, Colima, OrbStack, Rancher Desktop), podman, containerd and
  buildkit, through the socket that `DOCKER_HOST`, `CONTAINER_HOST`, `CONTAINERD_ADDRESS`
  or `BUILDKIT_HOST` names, or else the engine's default socket. Sandboxes get these
  variables set to the socket they can reach, and `DOCKER_HOST` points to podman when
  there is no docker. Inside WSL, the interop socket stays open, so Windows programs such
  as `wsl.exe`, `wslc.exe` and `docker.exe` start. On Windows the `scriptschnell-term-sandbox`
  launcher relays the engines' named pipes (`docker_engine`, `podman-machine-default`,
  `containerd-containerd`, `buildkitd`). The engines run outside the sandbox: a container
  a sandbox starts can mount any host path and reach any host, whatever `hidden` and
  `allowed_hosts` say. A docker daemon reached over TLS (`DOCKER_TLS_VERIFY`, or a
  `tcp://` `DOCKER_HOST`) keeps its client certificates: `ca.pem`, `cert.pem` and `key.pem`
  in `DOCKER_CERT_PATH` or `~/.docker` show through. Registry certificates
  (`/etc/docker/certs.d`, `/etc/containers/certs.d`, `/etc/containerd/certs.d`) need
  nothing, since the engines read them on the host, and `nerdctl` reads containerd's from
  the read-only root. `off` hides the sockets and the interop socket. On Linux it also
  hides the engines' own directories (`/run/podman`, `/run/containerd`, `/run/buildkit`,
  `~/.docker/run` and the like), and the sandbox sees `/run` as it was when the sandbox
  started, so an engine started later, such as rootful docker with
  `/run/docker.sock`, stays out of reach of sandboxes that were open already. On macOS
  the engines' default sockets are denied whether they exist or not.
  With `sidecar`, the host's engines are hidden as with `off`, and each local project gets
  an engine of its own in a container on this machine's engine (`container_engine`, or
  podman when installed, else docker). The project's first sandboxed terminal starts it,
  and the sandboxes of all its worktrees share it. Terminals show the sidecar's log until
  it listens, then restart with `DOCKER_HOST` (and `CONTAINER_HOST` for podman) pointing
  to its socket. The sidecar runs privileged and sees the project's worktrees at the same
  path. A worktree added after it started restarts it, which stops the containers running
  inside. It also sees its own storage volume `cmdterm-sidecar-<hash>`, which survives restarts,
  and, read-only, the host's registry certificate directories and CA bundle, so private
  registries and CAs work inside. It sees no other host files, but its network is not
  limited to `allowed_hosts`. Sidecars stop when the app quits. They need Linux, because
  their socket must be on the host's kernel.
- `sandbox.engines_acknowledged`: `true` once the user keeps `passthrough` on purpose,
  with Keep passthrough on the Sandboxing page or by picking it there. Until then the
  status bar shows a sandboxed worktree as "sandbox · weakened" in the warning color;
  after that as a plain "sandbox", and the Sandboxing page keeps only the warning text,
  without its buttons. Missing means `false`.
- `sandbox.sidecar.engine`: `podman` (the default, `quay.io/podman/stable`) or `docker`
  (`docker:dind`).
- `sandbox.sidecar.image`: an image in place of the engine's default one.
- `sandbox.guards`: `ask` (the default), `allow` or `block` per guard id. Guards hold back
  requests that publish, such as `git push`. The proxy's own guards have ids starting with
  `cmdterm.`; extension guards are `<extension>.<name>`. A forge's guard on change
  requests without an entry here follows `sandbox.change_requests`.
  In `ask` mode the dialog offers "Allow in this session", which holds for the guard and
  host for requests from the same project until the app quits, and "Always allow in this
  project" and "Always block in this project" for requests from a local project's
  sandboxes. That answer holds for the guard and host (or keyring secret) in that project
  only, and the project keeps it in `state.json` (`always_allowed` or `always_blocked`).
  A blocked entry refuses the request without a question and wins over every other answer.
  Settings, Project, Allowed and Blocked list the active project's entries: Remove takes
  one back, "Block instead" and "Allow instead" move it to the other page, and the form at
  the end adds one for a guard and a host (or a keyring secret's label). Host names are
  kept in lowercase. Settings, Extensions, scriptschnell-term lists the allowed entries of
  every project.
  On Linux, `cmdterm.keyring` covers this machine's keyring. Sandboxes don't see the
  session bus. Instead `DBUS_SESSION_BUS_ADDRESS` points to a stand-in that reaches only
  the Secret Service (`org.freedesktop.secrets`, such as gnome-keyring or KWallet), and
  that only when the app itself has a session bus. Listing secrets and reading their
  labels always works. Reading, changing, creating or deleting a secret follows the guard:
  `ask` shows a dialog with the secret's label, and the answer holds for that secret for
  one minute. Coding agents' own logins are the exception in `ask` mode: sandboxes read,
  renew and create them without a dialog, as they may write the agents' login files.
  These are Antigravity CLI's (`service` `gemini`, `username` `antigravity`), which
  waits only five seconds for the keyring and then shows its login screen, Kimi Code's
  (`service` `kimi-code`), Mistral Vibe's API key and account login (`service`
  `ai.mistral.vibe` and `ai.mistral.vibe.oidc`) and those extensions name in `sandbox.keyring` of
  `term.agent`. Deleting one still asks, and `block` blocks them too. Calls to other
  services, owning a bus name and listening to other programs' signals are refused.
  On Linux, `cmdterm.agent-settings` covers the settings coding agents change themselves,
  such as when you trust a folder or allow a tool: `~/.claude.json` and
  `~/.claude/settings.json` for Claude Code, `~/.codex/config.toml` for Codex, and
  `settings.json` of Gemini CLI, Antigravity CLI and Qwen Code, and `~/.vibe/config.toml` for
  Mistral Vibe. A local project's sandboxes write a copy of
  each, kept per project in the app's state directory, and the app compares the copies
  with this machine's files once a second. Changes on this machine go into the copies.
  Changes in a copy go to this machine's file key by key, so both sides may change other
  keys meanwhile. In `~/.claude.json` only the trust, allowed tools, MCP servers, API key
  answers and the bypass-permissions answer follow the guard; counters, caches and the
  rest come over at once. Every change to the other files follows the guard. `ask` shows
  a dialog with the agent, the file and each changed key. A blocked change stays in the
  sandbox's copy and is not asked about again until the app restarts. Codex gets
  `CODEX_HOME` pointing to a directory of its own with the copy and links to the rest of
  `~/.codex`, unless `CODEX_HOME` is set already, and Mistral Vibe gets `VIBE_HOME` the
  same way, as both replace their settings by renaming a new file over them. So that Claude Code can lock
  `~/.claude.json`, the sandbox sees the home directory as a scratch directory: the
  existing entries are the host's, and new files there vanish when the sandbox ends.
  `cmdterm.open-url` covers links that programs in a sandbox open in your browser, such
  as the page of Claude Code's `/login` or `gh auth login --web`. A sandbox with a
  proxy gets `$BROWSER` set to a shim, and on Linux the shim is also bound over
  `xdg-open`. The shim takes `http` and `https` links only and sends them through the
  sandbox's proxy to the app (`GET http://open.scriptschnell-term.invalid/open?url=LINK`,
  which never leaves the proxy). `ask` shows the link with its host; "Always allow in
  this project" holds for that host. Once the link may open, every port on `localhost`,
  `127.0.0.1` or `::1` it names, itself or in a query value such as an OAuth
  `redirect_uri`, is forwarded from the same port on this machine to the sandbox's
  server on it for 10 minutes. So the browser's redirect back to a login's own server
  reaches it. Then `xdg-open` (`open` on macOS) opens the link. Programs that ignore
  `$BROWSER` and `xdg-open`, such as most on macOS, still print the link instead.

- `sandbox.change_requests`: `{ "create": "ask", "comment": "ask", "merge": "ask" }`, the
  mode of every forge's guards on pull and merge requests: opening one, commenting on or
  reviewing one, and merging or completing one. Each is `ask` (the default), `allow` or
  `block`. They cover the shipped `github`, `gitlab`, `bitbucket`, `forgejo`, `gitea` and
  `azure-devops` extensions and any extension guard with a `kind`, for requests to the
  forge's account hosts and their subdomains. A mode set for one guard in
  `sandbox.guards` wins. Settings, Extensions, scriptschnell-term edits them under "What
  sandboxes may do to pull and merge requests".

Extensions add hosts, paths, variables, fake credentials and guards on top of these, see
`extensions/README.md`.

### Serving this machine

- `remote.serve`: let other machines open projects here while the app runs.
- `remote.listen`: address and port, default `0.0.0.0:7722`.

The Remote machines page of the settings manages keys; `scriptschnell-term-remote` has the same
commands on the command line.

While the app serves this machine, it also connects to the scriptschnell-term-web servers this
machine is linked to with `scriptschnell-term-remote link`, so their web app reaches it from a
browser or a phone. `crates/cmdterm-web/README.md` covers the web server.

### Projects of other machines

The sidebar can show the projects of trusted remote machines next to this machine's
own. Switching to one of their worktrees then works like switching to a local one: its
terminals and VCS commands run on that machine. Turn it on with the switch on the
machine's row under Remote machines, or by hand:

- `remote.fused`: the machines, as `[{"address": "box:7722"}]`. The app asks each one
  for its projects at startup, when the list changes, when this machine wakes from
  sleep and every five minutes. An ask without an answer within 45 seconds is given
  up. A machine that did not answer is asked again after 15 seconds, then after twice
  as long each time, up to five minutes. Its row under Remote machines says so
  meanwhile.
- `remote.fused[].hidden`: roots of that machine's projects you removed from the
  sidebar. They stay out until you delete them from this list.

A machine shares the repositories in its own `state.json`, the ones its scriptschnell-term
sidebar lists, and only those on the machine itself. A project the machine stops
listing leaves the sidebar, except the one shown right now. While the machine does not
answer, its projects stay. Turning the switch off takes them out of the sidebar and
ends their terminals there.

The serving side turns sharing off with "Share this machine's projects" on the same
page, which writes `share_projects` to `server.json`. Keys limited to VMs never see the
list.

That switch and "Offer this machine's shell" are the defaults for every allowed key.
Each key under "Keys allowed to connect" has its own Shell and Projects switches. They
show what the key gets now. Flipping one gives that key its own setting in
`authorized_keys` (`host` or `vm-only`, `projects` or `no-projects`), so one laptop can
get this machine's shell while every other key stays limited to VMs. Sharing projects
with a key that has no shell gives it the shell too. Defaults removes the key's own
settings. A change applies at the key's next login.

### VPN networks

The VPN networks page under Remote machines lists the machines on the Tailscale,
NetBird and Nebula networks this machine is on. It asks `tailscale status --json` and
`netbird status --json`. Nebula has no command that lists peers, so the page shows its
lighthouses, the keys of `static_host_map`, and `/etc/hosts` entries inside its
network. For each peer, SSH opens a project there over SSH and Remote opens it as a
remote machine on port 7722, after you trust its key. Serve listens on this machine's
VPN address only, so only machines on that VPN reach it.

- `vpn.nebula_config`: Nebula's configuration file or directory. Without it the app
  reads the one the running `nebula` was started with, else `/etc/nebula/config.yml`.

The page connects to Tailscale peers by their MagicDNS name when MagicDNS is on, and to
NetBird and Nebula peers by address. Nebula's firewall must let port 22 (SSH) or 7722
(remote machines) in on the peer.

### Agent control socket

- `agent_control`: which terminals get the control socket, through which agents read
  the extensions and change them, suggest settings changes and start worktrees (see
  Control socket below). `host`, the default: only terminals that run directly on this
  machine, outside sandboxes and devcontainers. `local`: terminals on this machine, in
  its sandboxes and devcontainers. `on`: every terminal that can reach it, on this
  machine, in its sandboxes and devcontainers, on SSH hosts, remote machines and VMs.
  `off`: none. The Agents page of the settings has the same switch. A change applies to
  terminals opened afterwards.

### Debug browser

The app runs one Chrome or Firefox for all its windows and worktrees, the debug browser.
Ctrl+Shift+P, Debug browser opens a panel on it (the `debug_browser` key action does the
same), and Open URL in debug browser loads a page there. The panel shows the tab an
agent used last and passes your clicks and keys to it. Agents drive the browser through
MCP tools (see Debug browser tools below). It starts when a panel shows it or an agent
first uses it, and stops when the app quits.

- `debug_browser.browser`: `auto`, the default, starts Chrome or Chromium when one is on
  `PATH` (or in `/Applications` on macOS), else Firefox. `chrome` and `firefox` pick one.
- `debug_browser.executable`: the browser's program, for one that is not on `PATH`.
- `debug_browser.headless`: on by default; the browser has no window and the panels show
  it. Turn it off to get the browser's own window as well.
- `debug_browser.agents`: whose agents may use the tools. `local`, the default: terminals
  on this machine, also in sandboxes and devcontainers. `on` adds SSH hosts, remote
  machines and VMs. `off`: none.

The browser has profiles of its own in `debug-browser/` of the cache directory, never
yours. Chrome talks to the app over a pipe (`--remote-debugging-pipe`), so no other
program can drive it. Firefox has no pipe: its WebDriver BiDi port listens on loopback,
where other programs of this machine could reach it while it runs.

The browser runs on this machine, outside every sandbox and container. An agent in a
sandbox, a devcontainer or on another machine cannot open `file:` or browser pages
(`chrome:`, `about:` other than `about:blank`). An agent in a sandbox that may reach only
allowed hosts can open only pages on those hosts, and a sandbox without network none.
Pages that a page loads by itself are not checked. Sessions of a sandboxed agent open
`http://localhost:PORT`, `127.0.0.1` and `[::1]` inside that agent's own sandbox, through the
worktree's port hub, and other hosts through the sandbox's proxy (without TLS interception,
so HTTPS pages show their real certificates; hosts that proxy rules inspect stay refused).
The port hub's proxy takes a random password like the sandbox proxy, which the app gives
Chrome through the DevTools protocol (`Fetch`) and Firefox through WebDriver BiDi, only
when the challenge comes from that proxy. While such a session exists, Chrome hands every
request to the app before it goes out. A
session belongs to the terminal that started it: a sandboxed agent cannot drive another
terminal's session. Exposing a port is needed only for the panels and the host's own programs.
Agents cannot suggest changes to `debug_browser`.

### Idle agents

- `freeze_idle_agents`: minutes, 15 by default, 0 turns it off. An agent that finished its
  turn and sits at its prompt in a terminal that has not been shown for that long is
  stopped (SIGTERM, SIGKILL after five seconds). The terminal keeps showing what it
  showed. When it shows again, the app types the resume command into its shell (see
  `state.json`) and shows the terminal live once the agent's hooks report the resumed
  session, or for Codex, which reports a session only when a turn starts, once its
  screen has been still for a moment. Keys, typed text and pastes in the meantime wait and reach the resumed agent
  in order. Only agents whose hooks named a session id freeze, only in a shell on this
  machine or in its sandbox, and not after you typed at the agent's prompt since its
  turn ended.

  On Linux the app reads `/proc/meminfo` and `/proc/pressure/memory` every five seconds
  and freezes sooner while memory runs out. With less than 20% of memory available and
  falling fast enough to run out within 15 minutes, less than 10% available, or tasks
  stalled on memory for at least 10% of the last ten seconds, idle agents freeze after
  one minute, or after `freeze_idle_agents` if that is shorter. With less than 5%
  available, less than 20% and running out within a minute, or every task stalled for at
  least 10%, they freeze within five seconds. A setting of 0 turns this off too.

### CPU modes

- `cpu_mode.local`: how much CPU terminals on this machine may use, also in its sandboxes,
  devcontainers and VMs. `balanced` by default.
- `cpu_mode.remote`: the same for terminals on SSH hosts, remote machines and extensions'
  environments. `off` by default.

| Mode | Cores all terminals of the machine share | Agents in hidden terminals |
| --- | --- | --- |
| `off` | all | as shown ones |
| `balanced` | all but one | nice 10 |
| `quiet` | half, at least one | nice 19 |

Three places switch the modes while terminals run, and the change reaches the terminals
that already run, not only new ones:

- The status bar shows the mode where the active project runs, such as `CPU balanced`.
  A click switches it to the next mode: off, balanced, quiet, then off again.
- Settings (Ctrl+,), Agents page, under CPU: one dropdown for this machine, one for
  other machines.
- The command palette (Ctrl+Shift+P): type "CPU".

`cpu_mode.balanced` and `cpu_mode.quiet` change what those two modes do. Each takes
any of these keys, and a key left out keeps the value in the table above:

- `free_cores`: cores the machine's terminals leave free together.
- `max_percent`: percent of the machine's cores they may use at most, 1 to 100.
- `background_nice`: the nice level of agents in hidden terminals, 0 to 19. 0 leaves
  them at the default weight.
- `performance_cores`: whether shown terminals may use the performance cores, on
  machines that also have efficiency cores. `true` in balanced, `false` in quiet.

The terminals always keep at least one core. For example, this keeps two cores free in
balanced and lets quiet use at most a quarter of the cores:

```json
"cpu_mode": {
  "local": "balanced",
  "remote": "off",
  "balanced": { "free_cores": 2 },
  "quiet": { "max_percent": 25, "background_nice": 15 }
}
```

`off` has no settings: it never caps and never lowers hidden agents.

#### Efficiency and performance cores

On a machine with efficiency and performance cores, such as Intel's hybrid processors
or ARM's big.LITTLE, the modes also pick the cores terminals run on:

- Hidden terminals run on the efficiency cores, in every mode, whatever they run.
- Shown terminals, the panes of the worktree on screen, may use every core in off and
  balanced, and only the efficiency cores in quiet.
- The cores a mode keeps free are performance cores, from the lowest number up. With
  balanced on a 6+8 core Intel, CPU 0 stays free for the rest of the machine, and
  nothing the app's terminals start runs there.

The app asks each machine for its cores once: Intel's `/sys/devices/cpu_atom/cpus`,
else the cores whose `cpu_capacity` is below the highest, and on Windows the cores below
the highest `EfficiencyClass`. Machines whose cores are all alike are left to the
kernel, as are macOS and BSD. On Linux the cores go to the slice
and to each terminal's scope as `AllowedCPUs`. Where the user's systemd has no cpuset
controller, `taskset` moves the scope's processes instead, and without systemd it
moves the terminal's shell and its foreground processes on this machine only.

- `cpu_mode.efficiency_cores`: `true` by default. `false` leaves the cores to the
  kernel and lets every terminal use all of them again.

Where `systemd-run --user` works on the machine (most Linux desktops and servers), every
terminal starts in a systemd scope of its own, whatever the mode, in the slice
`app-scriptschnell_term.slice`. The mode sets the slice's `CPUQuota` to its cores,
counted on that machine, so agents, the builds they start and everything
else in the app's terminals stay within it together, and the other programs of the
machine keep the rest. Scopes share the slice fairly: a terminal that starts 32 compiler
processes gets the same part as one that runs a single agent, when both want more than
they get. An agent in a terminal that is not shown gets the mode's nice level as its
scope's `CPUWeight` and `IOWeight` (nice 10 is weight 11, nice 19 is weight 1, the
default is 100), and the default back when it shows. Unlike a nice level, a weight can
go back up without root. `control run` starts its own scopes in the same slice, so its
`--cpu-quota` applies inside the mode's cap.

Without systemd, Linux pins each new terminal to the mode's cores with `taskset` when it
starts, so a switch reaches only terminals opened after it, and on this machine the
nice level goes to the terminal's autogroup (`/proc/PID/autogroup`). In a devcontainer
the cap applies inside the container, as `taskset` there, and agents keep their weight.
macOS and BSD run terminals unlimited.

#### Windows

On Windows, job objects do what the slice and scopes do on Linux, also on remote
machines that run Windows (see below). Each terminal starts suspended, joins the
app's job and then a job of its own nested in it, and only then runs, so nothing it
starts gets out.

- The app's job has the mode's cap as a hard CPU rate limit. Each app instance has its
  own, unlike the slice that all instances on a Linux machine share.
- An agent in a hidden terminal runs below normal priority at nice 10 to 14, and at idle
  priority from nice 15, with a shorter scheduling class. Its job puts it back to
  normal when it shows. Where Windows needs a privilege for a job's priority class, the
  app sets the priority of each process in the job, which their children inherit.
- On machines with efficiency cores, the job's affinity does what `AllowedCPUs` does on
  Linux, for the first 64 cores. Machines with more processor groups are capped but not
  placed.
- A Windows sandbox's own job cannot join the app's job, because it limits the
  desktop. The launcher gives it the mode's cap when the terminal starts. A later
  switch, hidden agents and core placement do not reach it.

#### Remote machines

A scriptschnell-term remote machine or VM applies the mode itself: the client passes
the limit with the terminal (`exec --cpu`), and the server starts the terminal under it
as for its own, through systemd on Linux and job objects on Windows. Mode switches and
hidden agents reach it through the channel `cmdterm-remote:cpu`. A server from before
CPU modes starts terminals unlimited. SSH hosts and custom environments run the script
through `sh` on the host, so they need a POSIX shell there.

### Default agent

- `default_agent`: the agent the `new_agent` action (New agent tab) starts in a new tab,
  by its program or name: `claude`, `codex`, `opencode`, `pi`, `copilot`, `qwen`,
  `gemini`, `kimi`, `vibe`, `agy`, or the name of an agent an extension registers. Leave
  it out to start the first agent with an integration on the project's machine; without
  one the action opens the new panel list at its agents. The first-start setup and
  Settings, Agents pick it. The keymap presets bind `new_agent` to Ctrl+Shift+A
  (scriptschnell-term, VS Code, Cursor), Ctrl+B A (tmux) and Alt+Shift+A (Windows
  Terminal, Konsole).

### Agent shell

- `agents_use_bash`: on by default, Linux only. A terminal that runs an agent itself, such
  as one an agent launch or a saved layout opens, starts it through `sh`, which sets `SHELL`
  to the first `bash` on the environment's `PATH` instead of the user's shell. Agents run
  their tool commands in `$SHELL` and often expect bash there, not fish or nushell. This
  works wherever the agent runs: on this machine, in sandboxes and devcontainers, on SSH
  hosts, remote machines and VMs. Where `PATH` has no bash, the agent keeps the `SHELL` it
  had. A shell you start an agent from yourself keeps the user's `SHELL`. Set it to `false`
  to keep the user's shell for agents too.
- An agent the app starts itself that is not on the app's `PATH` is looked up on the
  `PATH` of the user's login shell (`$SHELL -l`), then on that of an interactive login
  shell (`$SHELL -l -i`), on every Unix and whatever `agents_use_bash` says. The second
  covers a `~/.bashrc` that returns early when not interactive, before `mise activate`,
  nvm or the line that adds `~/.local/bin`. An app started from the desktop often lacks what a shell
  profile adds, such as `~/.local/bin`, where uv, pipx and npm install Mistral Vibe,
  Kimi Code and other agents. On Windows a program the app's `PATH` lacks is looked
  for in `%USERPROFILE%\.local\bin`, winget's `Links` folder, npm's `%APPDATA%\npm`
  and the folders of the packages winget installed for you, such as Claude Code's. The
  Agents page counts an agent found there as installed before it first ran.

### Shells on Windows

Settings, Shells lists the shells the app finds: Git Bash (`git-bash`), PowerShell 7
(`pwsh`, also the Microsoft Store's), Windows PowerShell 5.1 (`powershell`), the Command
Prompt (`cmd`) and `wsl`. The first-start setup asks for the default shell among them. The
page is missing on other systems, where terminals start `$SHELL`. A tab whose shell
titles it with its program's full path, as ConPTY does, shows the program's name.

- `shell`: the shell new terminals start on this machine and in its sandboxes. An id from
  the list, a program path, or a list of program and arguments, such as
  `["C:\\msys64\\usr\\bin\\zsh.exe", "-l"]`. Leave it out for `%COMSPEC%`. Open terminals
  keep their shell.
- `shells`: more shells the new panel list (the `+` after the tabs) and the palette offer
  as "New shell: NAME", in the same forms. Only local projects that do not run in a
  devcontainer get them. A saved layout reopens such a terminal with the same program.

When Git for Windows is installed, every process the app starts gets
`CLAUDE_CODE_GIT_BASH_PATH` set to its `bin\bash.exe`, which Claude Code needs on
Windows, unless the app has the variable already. A program named `sh` or `bash` that is
not on `PATH` runs Git's `usr\bin\sh.exe` or `usr\bin\bash.exe`.

### Why Windows runs on the host

On Windows, new worktrees run on the host unless you pick a sandbox or a devcontainer,
and the setup does not recommend the sandbox there. Its write-restricted token breaks
too many common tools:

Programs on Git's MSYS runtime, such as Git Bash, `sh` and Git's `less`, cannot start in
the Windows sandbox: its write-restricted token cannot use the shared memory and pipes
the runtime makes for the user. A sandboxed terminal whose shell is one of them starts
the first native shell instead and says so once. The sandbox sets `PAGER=cat` unless
`PAGER` is set, so git prints without a pager; `GIT_PAGER` and `core.pager` still win.
Node.js programs cannot start other programs in the Windows sandbox either. Node gives
each child pipes it creates by name, and the default access list Windows puts on such a
pipe lets only the user write to it, not the sandbox's restricted token, so every start
fails with `EPERM`. Claude Code is such a program: its hooks, shell commands and tools
fail there with "operation not permitted", and the app says so the first time Claude
Code starts in one. Run it on the host or in a devcontainer instead.

A devcontainer isolates a worktree on Windows without these limits.

### Update checks

- `check_updates`: on by default. Ten seconds after start and every 12 hours, the app
  checks every machine a project lives on for agent integrations that an older version
  installed (Settings, Agents) and for newer releases of Claude Code, Codex, opencode,
  pi, Copilot CLI, Qwen Code, Gemini CLI and Kimi Code installed with `npm install -g` or
  mise. Mistral Vibe updates through uv or Homebrew and is not checked. Antigravity CLI
  updates itself or through Homebrew, and only a mise install (`antigravity-cli`) is
  checked. It runs `npm ls -g`,
  `npm root -g`, `npm view`, `mise ls` and `mise latest`. An npm package the user cannot
  write to, such as Gemini CLI installed by pacman into `/usr/lib/node_modules`, is left to
  the package manager that installed it. Results show under the bell in the bottom right corner of
  the sidebar, next to recent status bar notices. Update reinstalls the integration, or
  runs `npm install -g <package>@latest` or `mise upgrade <tool>` (`--bump` for a pinned
  version). The check also looks for a newer scriptschnell-term release. It asks the
  GitHub releases of `codefionn/scriptschnell-term` first and the mirror
  `https://releases.codefionn.eu/scriptschnell-term` only when GitHub fails. A release
  counts only when its
  manifest's signature checks out against the release key built into the app. The
  Windows installer's copy and an AppImage install it; any other copy, such as one from a
  package manager or `cargo install`, only gets a notification with the release page.
  Installing downloads every file, checks each against its SHA3-256 hash in the manifest,
  and only then replaces the files next to `scriptschnell-term.exe` or the AppImage file;
  the new version runs after a restart. A build without a release key, such as a local
  `cargo build`, never asks. Turn `check_updates` off to stop the checks; the bell then
  shows only notices, and its Check for updates button still checks once.
- `auto_update`: on by default. A check that finds a newer release installs it at once
  and the bell says so. Off, the bell shows the release with an Update button. It changes
  nothing for copies that only get notified.

## state.json

The app writes it after every structural change: projects (with their environment, root,
VCS and forge), the active project and worktree, and per worktree its tabs, panels, extra
environment variables (`env_vars`), where its terminals run and the titles of its last agent sessions
(`agent_titles`, oldest first). A terminal on another machine, in a VM or in a container
gets `env_vars` on the command line of the ssh, `scriptschnell-term-remote` or container engine
process the app starts for it, where other users of this machine can read them with `ps`,
so keep secrets out of them. On Unix the app writes `state.json` and `settings.json` with
mode 0600. `allows_launches` is true for a project whose agents
may create worktrees and start agents through the control socket without asking.
`always_allowed` and `always_blocked` hold a project's guarded requests answered for good,
each entry a `guard` id and a `host` (or a keyring secret's label).
`jj` is `true` or `false` for a project that chose for itself whether it reads its jj
repository with `jj`, and missing for one that follows the `jj` setting.
`base` is a project's reference branch, the branch or revision its reviews compare
against when none is named, as the Project page of the settings sets it. Missing, reviews
detect the base: git's `origin/HEAD`, else `origin/main`, `origin/master`, `main`,
`master` or `trunk`; Mercurial's `default`; jj's `trunk()`.
`runtime` is `host`, `devcontainer` or `sandbox` for a project that chose where its
worktrees run on the Project page of the settings, in place of `default_runtime`, and
missing for one that follows it. See "Sandboxes and containers" above.
`sandbox` holds a project's own sandbox folders, which the Project page of the settings
edits: `writable` ones its sandboxes may write and `readable` ones they may read, as
`{"writable": ["~/src/shared-lib"], "readable": ["~/.ssh/known_hosts"]}`. A read-only
folder may lie in a path of `sandbox.hidden` and shows through it, and stays read-only
inside a writable one. A folder that contains a hidden path or one of the app's
directories, or lies in one of the app's directories, is skipped. A project on another
machine uses only its absolute paths, on that machine. Terminals pick changes up when
they start.
`env_vars` on a project holds per-project environment variables edited on the Project
page of the settings, passed to every newly started terminal of that project's worktrees
(including sandboxed ones, even when a variable's name looks like a credential). Worktree
`env_vars` override project `env_vars` of the same name. When the operating system's
secret storage is available (Secret Service through `secret-tool` on Linux, the login
keychain on macOS, or the Credential Manager on Windows, outside `--config-dir`), only
each variable's `name` is saved in
`state.json` and the values live encrypted in the keyring under service
`scriptschnell-term-project-env`. When no secret storage is reachable, each entry falls
back to keeping its `value` in `state.json` (mode 0600 on Unix). On Linux, local
sandboxes receive these variables through the process environment rather than `bwrap`
command-line arguments.
`task_runs` holds when each extension
task (`term.task`) last ran, per project; delete an entry to make that task due again.
A terminal on a remote machine or a local VM saves the name of its `session`. The shell
keeps running on that machine when the connection drops or the app quits, and the
terminal attaches to it again on the next start (see Sessions in the `scriptschnell-term-remote`
README). Closing the terminal ends the session. With the name it saves `control`: the
name of the control socket forwarded to the shell and the shell's token, so the shell
still reaches the socket after the terminal attaches again. Changing where a worktree's
terminals run, or rebuilding its devcontainer, ends their sessions and forgets both.
A terminal with a coding agent open saves the agent and its session id (`agent`, such
as `{"agent": "claude", "session": "3f2a…"}`), from the moment the agent's hooks
(Settings, Agents) report a session start until they report its end. On the next start
the app resumes that session: a shell gets the agent's resume command typed once it
reads commands, after a prompt it marks (OSC 133, as fish does) or once its output paused
(`claude --resume ID`, `codex resume ID`, `copilot --resume ID`,
`qwen --resume ID`, `gemini --resume ID`, `kimi --session ID`, `pi --session ID`,
`cursor-agent --resume ID`), and a terminal that runs the agent itself, such as one an
agent launched with a prompt, starts it with those arguments in place of the ones it
first started with, so the prompt does not run again. Without an id the agent starts a new session.
Before it resumes, the app checks where the agent runs that the session still exists:
Claude Code's in `$CLAUDE_CONFIG_DIR/projects` (default `~/.claude/projects`), Codex's in
`$CODEX_HOME/sessions` (default `~/.codex/sessions`), Copilot's in
`~/.copilot/session-state`, Gemini's in `~/.gemini/tmp` and Qwen Code's in `~/.qwen`.
When that directory exists but has no trace of the session, the agent starts a new
session instead and gets no prompt to continue. Extension agents name their directory
with `sessions` in `term.agent`. When the directory is missing, and for the other
agents, the app resumes without checking. A terminal that
attaches to a session on a remote machine or a local VM finds its agent still running
and gets nothing typed. opencode, Mistral Vibe and Antigravity CLI report no session yet.
The entry also notes `"working": true` while the agent is in the middle of a turn: busy,
or asking for a permission or an answer. When the app stopped then, the resumed agent
gets a prompt telling it to continue, typed once its hooks report the resumed session
and its screen has been still for a second. Codex runs its session-start hook only when
a turn starts, so it gets the prompt once its screen has been still for a second and a
half. An agent whose hooks report nothing within a minute gets no prompt.
The Codex integration sets `daemon_auto_start = false` under `[features]` in
`~/.codex/config.toml`, marked with a `# scriptschnell-term` comment, unless the file
sets it already. By default Codex moves sessions into a shared background server, and
hooks that run there have no terminal, so they cannot tell the app which terminal a
session runs in. Turning the integration off removes the line again.
Edit it only while the app is closed.
Forgetting a project in the palette is the safer way to remove one.

### Sessions

A local session is a project opened temporarily, with "Open project temporarily…" in
the palette or `--temporary` on the command line. It works like any project until the
app quits, but never reaches `state.json`. Opening the same project for good keeps it.

A global session is a folder without a repository, by default a machine's home
directory, for work on the machine itself: dotfiles, installed tools, system settings.
Open one with "New global session…" or `--global`. It takes a folder, `host:` for the
home on an SSH host, or any location the project input takes. Its terminals run on the
host, since a sandbox may only write the worktree; "Run terminals in a sandbox" in the palette
changes that like for any worktree. Each machine and folder has one global session.

Both start temporary, marked `temporary` in the rail. "Keep session NAME" in the palette
saves it from then on; "Close session NAME" closes it and ends its terminals. A kept
global session is saved without a `vcs` key.

## Command line

```sh
scriptschnell-term [LOCATIONS...] [--temporary] [--global [LOCATION]] [--config-dir DIR]
                   [--serve [ADDR]] [--font-size PX] [--width W --height H]
scriptschnell-term --config-dir DIR --screenshot shot.png [--dump-a11y tree.json]
                   [--palette TEXT] [--settings SECTION] [--setup STAGE] [--settle-ms MS]
                   LOCATIONS...
scriptschnell-term fix [--agent PROGRAM] [--config-dir DIR]
```

`fix` starts a coding agent instead of the app, to repair a configuration that keeps the
app from starting or working. The agent runs on this machine, unsandboxed, in the
configuration directory. The app types a prompt into it that names `settings.json`,
`state.json`, your `extensions/`, the logs and this reference, but does not send it: add
what broke and press Enter. If the agent opens with a question first, such as Claude Code
asking whether to trust the folder, the prompt is typed again after you answer it.
`--agent` takes the agent's program (`claude`, `codex`, `opencode`, `pi`, `copilot`,
`qwen`, `gemini`, `kimi`, `vibe`, `agy`). Without it the app picks the first agent on `PATH`
whose integration is turned on in Settings, Agents, else the first one set up on this
machine, else the first one on `PATH`. On Windows the prompt is printed for you to paste.
A project folder named `fix` needs a path such as `./fix`.

Locations are a local path, `host:/path`, `ssh://[user@]host[:port]/path`,
`cmdterm://host[:port]/path[?vm=name]` for a machine that runs `scriptschnell-term-remote serve`,
`vm://name/path` for a local VM, `scheme://target/path` for an environment an extension
registers with `term.environment` (such as `k8s://pod/path` from the shipped
`kubernetes` extension or `coder://workspace/path` from `coder`), or a repository URL to clone.
`tailscale://[user@]peer[:port]/path`, `netbird://…` and `nebula://…` open a project on
a VPN peer over SSH; the app finds the peer's address when it adds the project and
stores that address. `?remote` opens the peer as a remote machine instead, and
`?vm=name` one of its VMs.

## Environment variables

- `SCRIPTSCHNELL_TERM_CONFIG_DIR`: one directory for everything, as `--config-dir`.
- `SCRIPTSCHNELL_TERM_SSH_PROGRAM`: run this program instead of `ssh`, such as a wrapper
  around `tsh ssh`.
- `GITLAB_TOKEN`, `AZURE_DEVOPS_EXT_PAT`, `FORGEJO_TOKEN`: show change requests and CI
  status of those forges in the rail.
- `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`: the proxy for the app's own
  requests (forges, `term.http`, blocklists, updates). A `NO_PROXY` entry such as
  `example.com`, `.example.com` or `*.example.com` covers the domain and its subdomains,
  as in curl. CIDR ranges are ignored. A desktop proxy setting that names a server wins
  over these variables: manual mode in GNOME and the desktops that share its settings
  (Cinnamon, Budgie, Pantheon, MATE, Unity), a manual proxy in KDE Plasma, and the
  manual proxy in Windows' settings, with its exceptions as `NO_PROXY`. When the desktop
  proxy is off or uses a PAC script, the variables apply. The app reads the setting again
  at most every 10 seconds.
- Telemetry: `OTEL_*`, `CLAUDE_CODE_OTEL_*` and `CLAUDE_CODE_ENABLE_TELEMETRY`, such as
  `OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_METRICS_EXPORTER`, so agents like Claude Code
  report where you tell them to. Local terminals and sandboxes inherit them, and
  devcontainers get them set, unless their `remoteEnv` sets the same name. The hosts of
  `OTEL_EXPORTER_OTLP_ENDPOINT` and its `_TRACES_`, `_METRICS_` and `_LOGS_` variants
  join `sandbox.allowed_hosts`. A collector on `localhost` stays out of reach of
  sandboxes without the host's network and of containers; point the endpoint at an
  address they can reach.
- Trusted CAs: `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`,
  `NODE_EXTRA_CA_CERTS`, `GIT_SSL_CAINFO`, `CARGO_HTTP_CAINFO` and `DENO_CERT`, such as a
  company's TLS inspection CA. Sandboxes inherit them and may read the files and
  directories they name, even under hidden paths (not in the app's directories). With
  `sandbox.intercept_tls`, the proxy trusts the certificates in them for its own
  connections and adds them to the bundle it hands sandboxes, so a CA from
  `NODE_EXTRA_CA_CERTS` still counts when the sandbox's variable points to that bundle.
  `SSL_CERT_FILE` and `SSL_CERT_DIR` replace the system store for the proxy, as they do
  for OpenSSL. A devcontainer gets the certificates in its trust store (this needs root
  or passwordless `sudo` in it; the container log says when it fails) and in
  `/tmp/.cmdterm-host-ca.pem`. Its `NODE_EXTRA_CA_CERTS` points to that file, and each
  other variable of the list that you set points to the container's bundle.

The older `CMDTERM_` names of the first two still work.

Every terminal the app starts gets these, also on other machines:

- `SCRIPTSCHNELL_TERM=1`: the shell runs in a scriptschnell-term terminal. Agents use it to
  tell whether they run inside the app.
- `SCRIPTSCHNELL_TERM_SOCKET`: the control socket's path as this terminal sees it. Only
  set where the terminal reaches the socket.
- `SCRIPTSCHNELL_TERM_TOKEN`: this terminal's secret for the control socket. It changes
  with every terminal and stops working when the terminal closes. A terminal in a
  session on a remote machine or a local VM keeps its token and socket path when it
  attaches to the session again after a restart, so the shell that kept running still
  reaches the socket. `state.json` saves both with the session's name.
- `TERM_PROGRAM=scriptschnell-term` and `TERM_PROGRAM_VERSION`: the terminal's name and
  version, the same it answers an XTVERSION query (`CSI > q`) with.
- `COLORTERM=truecolor`, `FORCE_HYPERLINK=1` and `CLAUDE_CODE_FORCE_STRIKETHROUGH=1`: the
  terminal draws 24-bit color, OSC 8 links and strikethrough. Programs that check for
  these by terminal name would otherwise not know the name.

Local terminals do not inherit the variables of the terminal the app was started from,
such as `KITTY_WINDOW_ID`, `WEZTERM_PANE`, `VTE_VERSION` or `TMUX`, so programs do not
take them for that terminal.

## Control socket

The app listens on a Unix socket in `$XDG_RUNTIME_DIR/scriptschnell-term/` (else `run/`
in the state directory), a directory only your user can open. On Windows, which has
Unix sockets since Windows 10 version 1803, it is in
`%LOCALAPPDATA%\scriptschnell-term\run\`, in your profile, which only you, SYSTEM and
administrators can open. Agents use it to
list, read and change the extensions of the running app, to look up what their sandbox
lets through and suggest a settings change, such as one more allowed host, and to create
a worktree in their project with agents started in it. Reads need only the token.
Every change waits until you allow it in a dialog that names the terminal it comes from,
lists the files it writes or removes and the permissions its `manifest.toml` asks for.
On Allow the app writes the files into `extensions/<name>/` and reloads the extensions.
The permissions still need your grant on the Extensions page, as for any extension.
A new worktree or agent start waits for you the same way: the dialog names the branch or
worktree and shows each agent's full command line. On Allow the app creates the worktree
next to the main one (as Ctrl+Shift+P, New worktree does), switches to it and opens one
terminal per agent there, in the worktree's runtime.
A settings suggestion shows the agent's reason and each key it changes, before and after.
On Allow the app changes `settings.json` as the settings overlay would, and the change
applies at once. Agents cannot suggest `agent_control`, `extension_permissions`,
`remote` or `debug_browser`.

When the sandbox proxy refuses a host, its 403 says so and names the `control allow-host`
command, and the app remembers the last 32 refused hosts for `GET /v1/sandbox`. The
`scriptschnell-term-sandbox` skill, which the Agents page installs next to
`configure-scriptschnell-term`, tells agents how to read that report and what to suggest.

Terminals on other machines reach the same socket through a forward, so a change made
there lands in this machine's extensions, the ones the app runs:

| Terminal | How it reaches the socket |
| --- | --- |
| This machine | Directly |
| A sandbox here | The socket is bound into the sandbox (bubblewrap) or allowed (Seatbelt) |
| An SSH host, also sandboxed there | `ssh -R` on the terminal's own connection, to `/tmp/scriptschnell-term-<random>.sock`, mode 0600; the shell removes it when it exits |
| A remote machine or VM | The relay of `scriptschnell-term-remote exec --forward`, over the connection both machines' keys authenticate, to the same kind of path |
| A devcontainer of this machine's engine | The socket's directory is mounted at `/run/scriptschnell-term` when the container is created. Rebuild an older container to get it. The container's user must be yours (rootless podman, or `--userns=keep-id`, which the app adds) or root |
| A devcontainer on another machine, a Windows sandbox | Not reached. `SCRIPTSCHNELL_TERM=1` is still set |

The forward listens only for the user the terminal runs as, and each terminal's token
works only while that terminal is open. Before the first terminal on an SSH host gets the
forward, the app checks once per run, with a throwaway forward over the shared
connection, whether the host's `sshd` allows it (`AllowStreamLocalForwarding`). Hosts
that refuse get no forward, so ssh prints no warning and keeps sharing its connection. A
terminal that opens while the check still runs (it waits up to two seconds) goes without
the socket. A remote machine whose `scriptschnell-term-remote` is older than the relay runs the
terminal without it.

The integrations on the Agents settings page install a client at
`~/.local/share/cmdterm/control` (it needs `curl`). `control help` lists its commands;
`control push NAME DIR` uploads a directory as an extension and waits for your answer;
`control sandbox` prints the sandbox report; `control allow-host pypi.org 'pip needs it'`
suggests a host; `control worktree feature/x --prompt 'fix the test' claude` asks for a
worktree with Claude Code in it, started with that prompt. `--from REV` starts a new
branch at REV, `control worktree --detach --from v2.1 claude` checks out a revision
without a branch, and `control agents --worktree feature/x codex` starts agents in an
existing worktree.

The agent hooks tell the terminal their state with OSC 1337 SetUserVar, written to the
terminal. A hook that finds no terminal, as Claude Code's hooks on Windows, where Git's
`sh` gets pipes and no console it knows as a terminal, sends the same value to
`PUT /v1/uservars/cmdterm_agent` (or `cmdterm_session`) instead, with the value as the
body. Other names get 404.

The dialog for these two asks has Deny and Allow, plus two answers that cover every later
ask from the same project. "Allow in this session" holds until the app quits. "Always
allow in this project" is saved in `state.json` (`allows_launches`). Settings, Project,
Allowed has the switch for it, and Settings, Extensions, scriptschnell-term lists it under
"Always allowed", where Remove takes it back.

### Debug browser tools

`POST /v1/mcp` takes one MCP message (JSON-RPC 2.0), or a batch, and answers with one
line of JSON; a notification gets `202` and no body. The app keeps no state per
connection. The integrations on the Agents page install
`~/.local/share/cmdterm/browser-mcp`, which turns this into an MCP server on stdio, and
register it as `scriptschnell-term-browser` with Claude Code (`~/.claude.json`), Codex
(`~/.codex/config.toml`), opencode (`~/.config/opencode/opencode.json`), Copilot CLI
(`~/.copilot/mcp-config.json`), Qwen Code (`~/.qwen/settings.json`), Gemini CLI
(`~/.gemini/settings.json`), Antigravity CLI (`~/.gemini/config/mcp_config.json`), Kimi
Code (`~/.kimi-code/mcp.json`) and Mistral Vibe (`~/.vibe/config.toml`). On Windows the
entry starts Git's `sh.exe` by its full path, since Windows has no `sh`. pi has no MCP support. Gemini CLI with
`security.enableEnvironmentVariableRedaction` on hides `SCRIPTSCHNELL_TERM_TOKEN` from
the server and from hooks, so neither reaches the app. Outside a scriptschnell-term terminal the server offers no tools.

The tools follow Playwright's MCP server. `browser_session_new` starts a session, an
isolated browser context with its own cookies, storage and tabs, and returns its id.
Every other tool takes that id as `session`, so agents can pass a session on to each
other. `browser_sessions` lists them, `browser_session_close` closes one; a restart of
the browser ends them all.

| Tool | Does |
| --- | --- |
| `browser_navigate`, `browser_navigate_back`, `browser_navigate_forward` | Loads a page and returns a snapshot |
| `browser_snapshot` | The page as an accessibility tree, with a `ref` per element |
| `browser_click`, `browser_hover`, `browser_type`, `browser_select_option` | Act on an element by `ref`, with real mouse and key events |
| `browser_press_key` | A key or chord such as `Enter` or `Control+a` |
| `browser_evaluate` | Runs a JavaScript function in the page, with the element when given a `ref` |
| `browser_take_screenshot` | A PNG of the viewport |
| `browser_console_messages` | The tab's console messages and errors |
| `browser_wait_for` | Waits for text to appear or go, or for some seconds |
| `browser_resize` | Fixes the viewport of the session's tabs; otherwise a shown tab takes the panel's size |
| `browser_tabs` | Lists, opens, selects and closes the session's tabs |

Dialogs (`alert`, `confirm`) are accepted and show up among the console messages.

### Change requests

Agents read the pull or merge request of a worktree's branch from the app's own forge
checks, the ones behind the rail's Agents tab, so they never ask the forge or its CLI
themselves. It works with every forge an extension registers (`term.forge`).

| Tool | HTTP | Does |
| --- | --- | --- |
| `change_request` | `GET /v1/change-request` | The change request as the checks last saw it |
| `change_request_wait` | `GET /v1/change-request?wait=SECONDS` | Waits until it changes, then answers the same |

The MCP server above lists both before the extensions' tools, and `GET /v1/tools` and
`POST /v1/tools/<name>` reach them too. `control change-request` prints one, and
`control change-request --wait` waits (exit 1 on timeout). Arguments, as JSON for the
tools and as query parameters over HTTP:

- `worktree`: a worktree of the terminal's project by path, branch or folder name. It
  defaults to the terminal's own worktree.
- `full`: `true` (`1` over HTTP) adds `page`, the change request page of the explore
  panel as Markdown, with the review threads placed on the current code.
- `since`: a `revision` from an earlier answer. The wait returns at once when the
  change request changed after it, else when it changes next.
- `timeout` (tool) or `wait` (HTTP): seconds to wait. The tool waits 240 seconds by
  default and 840 at most, under the 900 seconds the MCP server gives an answer. Over
  HTTP the limit is two hours.

An answer has `project`, `worktree`, `branch`, `forge`, `revision` and `state`:
`open`, `merged`, `closed`, `none` (the branch has no change request), `unknown` (not
checked yet), `unsupported` (no forge extension knows the remote) or `no_branch`. With
a change request come `number`, `title` and `url`, and once the app has read its
discussion, `details`: `draft`, `conflicts`, `target`, `author`, `reviews`, `checks`
with their worst state in `checks_state`, `head` (the last push the forge saw),
`comments`, `open_threads` and `last_activity`. A wait adds `changed`. The
`revision` moves only when one of these fields changes.

The regular checks only ask about agents that finished their turn. A worktree an agent
reads or waits on is watched as well, for as long as the wait runs and 15 minutes
after. The app then puts its branch into every check (every three minutes, at most
every 30 seconds per forge), reads its repository's refs with `git ls-remote`, which
no forge counts against its API limits, and asks for its discussion again when the refs
moved, when the change request changed, or 30 minutes after it asked last. Pushes and
merges show within minutes. A comment, a review or a finished CI run without a push
can take up to 30 minutes.

### Extension tools

Extensions with the `agents` permission offer tools of their own (`term.tool`), such as
the `knowledge_*` tools of the shipped `knowledge` extension. `GET /v1/tools` lists them
with their JSON schemas; `POST /v1/tools/<name>` runs one with the JSON object in the body
as its arguments and answers `{"result": ...}`, or `422` with `{"error": ...}` when the
tool refuses. The MCP server above lists them after the browser tools and runs them the
same way. A tool runs on the app's UI thread with the same 500 ms budget as any extension
code, and the app answers within 30 seconds or reports that it is busy. The extension sees
the calling terminal's project with its `project_id`, worktree and environment, and the
agent's id on the agent list. The names `change_request` and `change_request_wait` are the
app's own.

### Agents working together

The socket also tells the agents of all terminals about each other, and none of this asks
you. Each terminal with a token gets an id from its worktree's directory name, such as
`app-3`. With the Claude Code, Codex, Copilot CLI, Qwen Code, Gemini CLI and Kimi Code
integrations, the session-start hook puts the agent
on the app's list under that id, with the harness's session id as its own id next to it,
and tells the agent its id. Cursor runs Claude Code's hooks too and joins as `cursor`.
Other agents (pi, opencode, Mistral Vibe, Antigravity CLI, a shell script) join with
`control join --agent NAME --id ID --note TEXT`. `control who` lists the agents of every
project with their harness, note, project, worktree, machine, whether they are busy, idle
or wait for you (from the same hook state the rail shows), when they last used the socket
and which locks they hold.

`control send ID TEXT` writes to one agent, by either id; `--worktree`, `--project` or
`--all` in place of the id write to every agent of this worktree, this project or the app.
`--untrusted-file PATH` relays outside content, such as an issue or a web page, which the
recipient sees inside an `<untrusted-...>` block with a random suffix. The hooks deliver
messages as context of the agent's next prompt or tool call, and the Stop hook keeps the
turn going while messages wait. An agent that waits at its prompt, idle after a turn or
just started, runs no hooks, so the app types a short prompt into its terminal that tells
it to read the inbox; it does so at most once every 5 seconds per agent, and never while
the agent asks you something or waits for a permission. `control inbox` and `control wait --timeout SECONDS`
read them without hooks. An inbox keeps the last 256 messages and goes away with the
terminal or `control leave`.

`control run --mutex NAME -- COMMAND` waits until lock `NAME` is free on the machine the
terminal runs on, then runs the command and frees the lock when it ends, also when it is
killed. While it waits it prints what holds the lock, and says so when a command from the
same terminal does, such as one that runs on in the background. A run whose shell was
killed with SIGKILL leaves its `curl` behind; the app sends that request an empty line
every 10 seconds, which ends the orphaned `curl` and frees the lock. `control unlock NAME`
frees the holds this terminal has on `NAME` right away; the commands keep running.
`--max-processes N` lets N commands hold the lock at once. `--reserve-ram SIZE`
waits until SIZE fits next to the reservations of the commands running on that machine,
counted against its memory minus `--keep-free-ram` (1 GiB by default). `--max-ram SIZE`
reserves the same and is also a hard limit. `--cpus 0-3`, `--cpu-quota 200%`,
`--max-tasks N` and `--max-cpu-time SECONDS` limit the command further. On Linux the
client runs it in a `systemd-run --user --scope` with `MemoryMax`, `MemorySwapMax=0`,
`AllowedCPUs`, `CPUQuota` and `TasksMax`. Where that does not work, as in a bubblewrap
sandbox, `--cpus` uses `taskset`, `--max-ram` limits address space with `ulimit -v`, and
`--cpu-quota` and `--max-tasks` fail. On macOS only `--reserve-ram` and `--max-cpu-time`
work. `control locks` lists who holds and waits for what.

The API is HTTP/1.1, one request per connection, with `Authorization: Bearer
$SCRIPTSCHNELL_TERM_TOKEN`. Paths under `/v1`:

| Request | Does |
| --- | --- |
| `GET /info` | The app's version, this terminal's project, worktree and environment, the extensions directory |
| `GET /sandbox` | Where this terminal runs (`terminal_runs_in`), the network mode, the effective `allowed_hosts`, what extensions add, the guards and their modes, the sandbox settings, `recently_refused` hosts with their reason, and the keys agents cannot suggest |
| `GET /ports` | The ports servers in this terminal's worktree listen on inside their sandboxes, as `{"ports": [{"port": 3000, "exposed_on": null}]}`. `exposed_on` is the host port, on 127.0.0.1, once exposed. 404 when the worktree's sandboxes share the host's network |
| `POST /ports/PORT/expose` | Forward a host port on 127.0.0.1 to `PORT` while the server keeps running. Optional body `{"host_port": 8080}`; without it the same number, or a free one when that is taken. Returns `{"port": 3000, "exposed_on": 3000}`, 409 when nothing listens on `PORT` or the host port cannot be bound, 400 for a bad body |
| `POST /ports/PORT/unexpose` | Stop exposing `PORT`. Open connections finish. Returns `{"port": 3000, "exposed_on": null}` |
| `POST /settings` | Body `{"reason": "...", "set": {"sandbox.intercept_tls": true}, "add": {"sandbox.allowed_hosts": ["pypi.org"]}, "remove": {...}}`, keys as dotted paths into `settings.json`: ask the user and apply. Returns the changed keys' new values, 403 when denied, 400 for a locked key, a missing reason or a result that is no valid `settings.json` |
| `GET /extensions` | Every extension with `loaded`, `turned_off`, `error`, `permissions`, `missing_permissions` and `shipped` |
| `GET /extensions/NAME` | One extension: its status, files and what this terminal staged |
| `GET /extensions/NAME/files` | Its files, one per line |
| `GET /extensions/NAME/files/PATH` | One file |
| `PUT /extensions/NAME/files/PATH` | Stage the body as `PATH`, up to 1 MiB |
| `DELETE /extensions/NAME/files/PATH` | Stage the removal of `PATH` |
| `GET /extensions/NAME/staged` | What this terminal staged for `NAME` |
| `POST /extensions/NAME/discard` | Drop it |
| `POST /extensions/NAME/apply` | Ask the user; returns `{"applied": true, "extension": {...}}`, or 403 when denied, 408 after ten minutes without an answer, 400 for a `manifest.toml` that does not parse |
| `POST /worktrees` | Body `{"branch": "feature/x", "agents": [...]}` or `{"detach": true, "agents": [...]}`, each with optional `"from"` and `"name"`: ask the user, create a worktree in this terminal's project next to the main one, switch to it and start the agents. `branch` is checked out when it exists and created at `from` when missing; `detach` checks out `from` without a branch. `from` is a revision: for git a branch, tag, commit or `origin/main`, for Mercurial a revision or revset, of which the first match counts, for jj a revset, of which the newest match counts. It defaults to the main worktree's revision and is refused for a branch that exists. In Mercurial a new branch is a bookmark and a detached worktree has no active bookmark. In jj a new branch is a bookmark on the new workspace's working-copy commit, which sits on `from`. `name` names the directory, `<repo>-NAME`; it defaults to the branch with `-` for `/`, else to `from`. Returns `{"worktree": PATH, "vcs": "git", "mercurial" or "jujutsu", "agents": [...]}` once the worktree exists, 403 when denied, 500 when the VCS fails |
| `POST /agents` | Body `{"worktree": WORKTREE, "agents": [...]}`: ask the user, switch to that worktree of the project (default: this terminal's) and start the agents. `WORKTREE` is an absolute path, the branch checked out there or the directory's name. Returns like `POST /worktrees`, 500 for a worktree the project does not have |
| `GET /agents` | The agents, as a list of `id`, `aux_id`, `agent`, `note`, `joined`, `you`, `state` (`busy`, `idle`, `input`, `permission`, `exited` or `unknown`), `active`, `message`, `project`, `worktree`, `machine`, `environment`, `seen_secs`, `inbox`, `holds`, `waits` and `capabilities` (what the agent can do: `state`, `session`, `resume`, `messages`, `prompt`, `paste`). `?scope=worktree`, `project` or `all` (default), `&worktree=PATH` or `&project=NAME` for another one, `&format=text` for lines. Lists the agents that joined and terminals whose hooks report a state |
| `GET /agents/self` | This terminal's entry |
| `PUT /agents/self` | Body `{"agent": "claude", "id": "SESSION", "note": "..."}`, each optional: join the list or change what it shows. An extension agent without `messages` in its `supports` is refused. `?format=id` returns only the given id |
| `DELETE /agents/self` | Leave the list; the inbox goes with it |
| `POST /messages` | Body `{"to": ID, "text": "..."}` or `{"scope": "worktree", "text": "..."}` (`project`, `all`; `worktree` or `project` may name another one), with `"untrusted": "..."` for relayed content. Returns `{"to": [ids]}`, 404 for an unknown agent, 409 for one that has not joined |
| `GET /messages` | The inbox, marked read. `?peek=1` leaves it, `wait=SECONDS` (up to 7200) waits for the first message, `format=text`, `hook&event=EVENT` (hook JSON with the messages as `additionalContext`; `shape=flat` puts it at the top level, `shape=text` sends plain text) or `stop` (a Stop hook's block while messages wait). 410 when this terminal's agent has not joined |
| `GET /locks` | Every lock with its machine, capacity, holders and those waiting; `?format=text` for lines |
| `POST /locks/NAME` | Body `{"max_processes": 1, "ram": BYTES, "total_ram": BYTES, "keep_free": BYTES, "command": "..."}`, each optional: queue for the lock. The response streams `waiting N` with a `held by ID for AGE: COMMAND` line per holder it waits for (`ID (this terminal)` for the caller's own), then `granted`, and stays open while the lock is held, with an empty line every 10 seconds without other news; closing the connection frees it, and `released` ends it when the hold went away. 409 when the lock runs with another `max_processes`, 413 when `ram` exceeds what the machine has |
| `DELETE /locks/NAME` | Frees the holds the caller's terminal has on lock `NAME` of its machine; its waiting requests stay. Answers `{"lock": NAME, "freed": N}` |

An agent is `"claude"`, `"codex"`, `"opencode"`, `"pi"`, `"copilot"`, `"qwen"`,
`"gemini"`, `"kimi"`, `"vibe"`, `"agy"` or an extension agent's name, or `{"agent": "claude",
"args": [...], "prompt": "fix the failing test", "via": "args"}` to start it with
arguments or a first prompt. `"prompt"` goes where the agent takes it: in its arguments
(`claude TEXT`, `codex TEXT`, `pi TEXT`, `vibe TEXT`, `copilot -i TEXT`, `qwen -i TEXT`,
`gemini -i TEXT`, `agy -i TEXT`, `opencode --prompt TEXT`, or an extension agent's `prompt`), else typed
once the agent named its session after it started (Kimi Code, and extension agents
with `paste`). `"via": "paste"` types it for every agent that names its session at start:
Claude Code, Copilot CLI, Qwen Code, Gemini CLI, pi and Kimi Code. An agent that takes
the prompt neither way is refused with 400. Up
to 8 agents per request. Extension names are letters, digits, `-` and `_`. Paths are relative, without hidden or `..`
parts. Staging belongs to the terminal and is lost when it closes. An extension that is a
single `NAME.lua` cannot be changed this way; move it to `NAME/init.lua` first.

```sh
curl --unix-socket "$SCRIPTSCHNELL_TERM_SOCKET" \
  -H "Authorization: Bearer $SCRIPTSCHNELL_TERM_TOKEN" \
  -X PUT --data-binary @init.lua http://scriptschnell-term/v1/extensions/hello/files/init.lua
curl --unix-socket "$SCRIPTSCHNELL_TERM_SOCKET" \
  -H "Authorization: Bearer $SCRIPTSCHNELL_TERM_TOKEN" \
  -X POST http://scriptschnell-term/v1/extensions/hello/apply
```

On Windows `SCRIPTSCHNELL_TERM_SOCKET` is a Windows path, such as
`C:\Users\you\AppData\Local\scriptschnell-term\run\control-1234-0.sock`. The curl of Git
for Windows and Windows' own `curl.exe` both take it for `--unix-socket`, and the scripts
the Agents page installs run in Git Bash.
