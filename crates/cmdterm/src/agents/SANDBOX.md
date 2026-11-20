---
name: scriptschnell-term-sandbox
description: How scriptschnell-term runs this shell and what its sandbox lets through. Use only when $SCRIPTSCHNELL_TERM is set, and then whenever a command fails in a way a sandbox or devcontainer could explain (a refused or unreachable host, a 403 from "cmdterm sandbox", a read-only or missing path, a missing credential, a blocked git push), and before you ask the user to change a scriptschnell-term setting.
---

# Working in a scriptschnell-term terminal

`$SCRIPTSCHNELL_TERM` is set, so this shell is a terminal of scriptschnell-term. Each
worktree's terminals run in one of three places: on the project's machine itself, in a
sandbox there, or in the worktree's devcontainer. The sandbox is the usual one. It
changes what a command can do, and the user decides how much. Do not work around it;
find out what it blocks and suggest a change the user can allow.

When `$SCRIPTSCHNELL_TERM_SOCKET` is set, the app answers on its control socket, and
`sh ~/.local/share/cmdterm/control` talks to it. Without the socket, explain the change
in your reply instead, with the `settings.json` keys from `{docs}/CONFIGURATION.md`.

## Find out where you run

```sh
sh ~/.local/share/cmdterm/control sandbox
```

It prints JSON:

- `terminal_runs_in`: `sandbox`, `container`, or the machine kind (`local`, `ssh`,
  `remote`, `vm`) for a terminal without a sandbox.
- `network`: `none`, `allowed hosts only, through the proxy`, `any host over HTTP and
  HTTPS, through the proxy` (the default: every host, but only proxy-aware traffic gets
  out), or `everything` (the host's network, with `host_network` or outside a local
  Linux sandbox).
- `loopback`: `sandbox` when you run in a local Linux bubblewrap sandbox and servers you
  start listen on the sandbox's own `localhost` (the default). `host` when
  `settings.sandbox.host_network` shares the host's, and for every other terminal
  (macOS, SSH hosts, remote machines, Windows).
- `windows_appcontainer`: present only in a local Windows sandbox with the experimental
  AppContainer on. The `network` value then holds, unless the launcher printed that it
  fell back to the restricted token, in which case you have the host's network.
- `allowed_hosts`: what the proxy lets through, from `settings.sandbox.allowed_hosts`
  and from extensions (`from_extensions.hosts`). `*.example.com` covers subdomains.
- `recently_refused`: hosts the proxy refused lately, newest first, with the reason.
  They come from every sandbox of the app, not only yours.
- `settings.sandbox`: the rest of the sandbox settings: `writable` and `hidden` paths,
  `intercept_tls`, `engines`, `guards` with the user's choice per guard, and
  `change_requests` with the choice per kind for forges' guards on pull and merge
  requests.
- `guards`: requests the proxy holds back until the user answers, such as `git push`,
  or opening, commenting on and merging a pull request (`kind` is `create`, `comment`
  or `merge`), with each guard's `mode`.
- `agent_locked_settings`: keys you cannot suggest.
- `docs`: `CONFIGURATION.md` on the app's machine, which explains every key.

## What the sandbox does

- The file system is read-only except the worktree, the paths in `writable` and
  the agents' own state directories. Paths in `hidden`, such as `~/.ssh`, are empty or
  missing. `/tmp` is private to the sandbox. On Windows, the restricted token cannot
  limit reads, so hidden paths are readable there unless `windows_appcontainer` is on, and
  then only inside directories the sandbox may read or write. Agent settings and hooks are
  read-only on Windows as well, and each project has its own temp dir.
- The repository's configuration and hooks are read-only: `.git/config`,
  `.git/config.worktree`, `.git/hooks`, `.git/worktrees`, `.git/commondir`, the `.git`
  file of a linked worktree, and hg's `.hg/hgrc`, `requires` and `sharedpath`. The same
  holds for a git directory outside the worktree, such as one made with
  `--separate-git-dir`. The app runs git and hg on them outside the sandbox. Its git
  commands read the git directory recorded when the project was added, never the
  worktree's `.git`, so replacing `.git` or pointing it elsewhere changes only what git
  in the sandbox sees. The user's own git on the host does follow it, and the app warns
  the user when a worktree's `.git` leads elsewhere, so leave `.git` as it is.
  `git config`, `git remote add`, `git branch -u`, `git submodule init` and
  `git worktree add` fail, `git push -u` pushes but does not record the upstream, and
  `git branch -m` renames the branch but then exits with 128 ("update of config-file
  failed"), so check `git branch` before you retry it. Pass settings per command
  (`git -c key=value`), push with `git push origin HEAD`, or ask the user to run the
  command on the host.
- A project can have folders of its own: writable ones, and read-only ones that show
  through `hidden`. The user sets them on the Project page of the settings (palette:
  Project settings); `suggest` cannot change them, so ask the user when a project needs
  a folder outside the worktree.
- With a restricted network, every connection goes through an HTTP proxy that only
  reaches `allowed_hosts`. A refused host answers `403` with a body starting
  `cmdterm sandbox:`. Tools report that as a proxy error, a 403, or a TLS failure.
  On macOS, on Windows and with `host_network`, the proxy URL in `HTTP_PROXY` carries a
  password (`http://cmdterm:PASSWORD@127.0.0.1:PORT`). A `407` means the request came
  without it: pass the URL on whole, user and password included, when you configure a
  tool's proxy by hand.
- A local Linux sandbox has a network namespace of its own, even with the whole network. Servers
  you start bind no host ports, and only HTTP and HTTPS reach the outside, through the
  proxy. `ssh`, raw TCP and UDP fail. Terminals of one worktree reach each other's
  servers on `localhost` as usual. The host does not, until a port is exposed:
  `sh ~/.local/share/cmdterm/control ports` lists your listening ports,
  `control expose PORT [HOST_PORT]` opens one on the host's `127.0.0.1` and
  `control unexpose PORT` closes it. The user can do the same from the command palette.
  With `host_network` (which needs `restrict_network` off), none of this applies.
- With `intercept_tls`, the proxy decrypts HTTPS with its own CA, which the sandbox
  trusts through `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`,
  `NODE_EXTRA_CA_CERTS`, `PIP_CERT` and the other CA variables. A tool that pins its
  own certificates or ignores these variables fails there, and `recently_refused` names
  the host with "turned down the proxy's certificate". On Windows only the hosts that
  extensions' secrets and guards need are decrypted. .NET, PowerShell and winget fail
  on those hosts because they trust only the Windows store; use curl, or git, which the
  sandbox points at the bundle.
- Extensions add hosts, paths and fake credentials that the proxy swaps for real ones,
  so a login can work although the real secret is not in the sandbox. The login files
  of `fj`, `tea`, `glab` and `gh` hold such fakes (extension `forge-clis`); a refresh of
  an OAuth login happens outside the sandbox, never inside it.
- The variables the sandbox's machine started it with reach it, except the ones whose
  names look like credentials: a name with `TOKEN`, `SECRET`, `PASSWORD`,
  `PASSWD`, `API_KEY`, `APIKEY`, `ACCESS_KEY`, `PRIVATE_KEY` or `CREDENTIAL` in it, or
  with a word containing `AUTH`, such as `GITHUB_TOKEN`, `GH_TOKEN`,
  `AWS_SECRET_ACCESS_KEY` or `NPM_TOKEN`. Agents' logins stay (`ANTHROPIC_API_KEY`,
  `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`, `GEMINI_API_KEY` and the others listed in
  `CONFIGURATION.md`), and so does every variable the app sets, such as
  `SCRIPTSCHNELL_TERM_TOKEN`, the worktree's own variables and extensions' fake
  credentials. When a tool fails because such a variable is empty, an extension that
  hands out a fake credential for it is the better fix. Otherwise suggest
  `{"add": {"sandbox.pass_env": ["HF_TOKEN"]}}` with the variable's name; terminals
  started after the user allows it get the variable.
- A guarded request (a push, a publish, `gh pr create`, `gh pr merge`) waits until the
  user answers a dialog; a blocked one fails with 403. The user may allow it for this
  session or for good in this project, so the same request may pass later without a
  dialog.
- On Linux, `DBUS_SESSION_BUS_ADDRESS` points to a stand-in for the session bus that
  reaches only the keyring (Secret Service). Reading, changing or deleting a secret
  waits until the user answers a dialog, under the `cmdterm.keyring` guard. Coding
  agents' own logins, such as Antigravity CLI's and Kimi Code's, pass without one. A refused
  call fails with `org.freedesktop.DBus.Error.AccessDenied` and a message starting
  `cmdterm sandbox:`. Other desktop services, such as notifications, are not reachable.
- On Linux, your own settings files (`~/.claude.json`, `~/.claude/settings.json`,
  `~/.codex/config.toml`, the `settings.json` of Gemini CLI, Antigravity CLI and Qwen Code) are
  copies.
  Writing them works, and the app brings the change over to the host. Trusting a folder,
  allowing a tool, hooks and MCP servers wait until the user answers a dialog; a blocked
  change stays in the sandbox only. Other new files directly in `~` vanish when the
  sandbox ends.
- The sandbox cannot read the clipboard. When the user presses Ctrl+V or Alt+V, the
  app hands the clipboard's image to `xclip` and `wl-paste` for a minute; reading text
  from them fails. Ask the user to paste text with Ctrl+Shift+V instead.
- Copying text works: `wl-copy`, `xclip` and `xsel` are shims that send the text to
  the terminal as OSC 52, and the app puts it on the user's clipboard. Copying an
  image does not.
- On Linux, GUI programs find no display, even when `DISPLAY` or `WAYLAND_DISPLAY` is
  set: the X11 and Wayland sockets are hidden. With `host_network`, and on remote
  machines, VMs and SSH hosts, Landlock keeps you from abstract Unix sockets outside
  the sandbox, X11's among them, and from signalling processes outside it, where the
  kernel is Linux 6.12 or later. On an SSH host this needs the helper the app copies to
  `~/.cache/scriptschnell-term/bin/`, which may arrive after your terminal started.
  Run browsers and test runners headless.
- On macOS, `open`, `osascript`, `launchctl` and login items fail: the sandbox cannot
  ask LaunchServices, Apple Events or launchd to start a program, since it would run
  outside the sandbox. Unix sockets connect only in the worktree and other writable
  paths, in `/tmp` and `$TMPDIR` (but not the SSH agent's or a tmux server's), the
  system's DNS and log sockets, and the ones the app passes in, such as
  `$SCRIPTSCHNELL_TERM_SOCKET` and a container engine's. That holds with the network on
  too. `ps` shows little about processes outside the sandbox, and no process's
  arguments or environment can be read, so `ps -o args` and `pgrep -f` see no
  arguments, not even of your own processes. GUI programs cannot open a window, since
  the window server is out of reach: run browsers and test runners headless.
- On Windows, the sandbox has a desktop of its own that the user never sees. Windows
  you open stay invisible, and programs in the sandbox cannot send input or messages to
  the user's windows. The clipboard stays readable there, because image paste into an
  agent reads it from inside the sandbox.
- A tool that opens a browser, such as a login with `gh auth login --web`, cannot do so
  from a sandbox on macOS or Windows: `open`, `start` and `explorer` fail or start the
  browser inside the sandbox. On Linux no browser shows either. Print the URL and ask
  the user to Ctrl+click it in the terminal; the app opens it in a browser panel, outside
  the sandbox. The control socket has no command to open URLs.

## Suggest a fix

Find the smallest change that fixes the failure, then suggest it with a reason the user
can judge. The app shows it in a dialog with each key before and after; nothing changes
until the user allows it. On Allow the change applies at once; new connections see it
without a restart.

```sh
# a host the proxy refused
sh ~/.local/share/cmdterm/control allow-host pypi.org 'pip install needs the package index'
# several steps at once; key paths are dotted, as in settings.json
cat > /tmp/suggest.json <<'JSON'
{
  "reason": "cargo needs the crates index and its downloads",
  "add": { "sandbox.allowed_hosts": ["index.crates.io", "static.crates.io"] }
}
JSON
sh ~/.local/share/cmdterm/control suggest /tmp/suggest.json
```

- `add` and `remove` take list items; `set` replaces a value, such as
  `{"set": {"sandbox.intercept_tls": true}}`.
- Prefer the narrowest change: one host over `*.domain`, an allowed host over turning
  `restrict_network` off, a `writable` cache directory over the whole home.
- Exit status 1 with "the user denied the change" means no. Do not ask again for the
  same change unless the user asks you to.
- `agent_control`, `extension_permissions`, `remote` and `debug_browser` are the
  user's alone.
- A guard set to block (`settings.sandbox.guards` or `settings.sandbox.change_requests`)
  is a decision the user made; say
  that a request was blocked instead of suggesting to allow it.
- Some failures are not the sandbox. When `recently_refused` does not list the host
  and the network is `everything`, look for the cause elsewhere.

## The debug browser

The `scriptschnell-term-browser` MCP tools drive a browser that runs on the user's
machine, outside your sandbox. From a sandbox they cannot open `file:` pages, and with
a restricted network only pages on the allowed hosts. Your sessions reach your own
sandbox, though. Open `http://localhost:PORT` to see a server you started inside it. The
browser sends `localhost`, `127.0.0.1` and `[::1]` into your sandbox and other hosts
through the sandbox's proxy, so its allowed hosts and blocklists apply. Exposing a port
(`control expose PORT`) is only needed to open it from the user's own browser. When a tool refuses a page, say so instead of working around it.

## Other help

- Settings and extensions in general: the `configure-scriptschnell-term` skill.
- An extension that should set up the sandbox for a tool (hosts, credentials):
  `{docs}/extensions/README.md`, functions `term.sandbox` and `term.secret`.
- New worktree with an agent in it: `sh ~/.local/share/cmdterm/control help`.
