# Sandboxes and devcontainers

A worktree's terminals can run in one of three places while VCS queries for the sidebar
stay on the host:

- **Sandbox**: isolates terminals so they can write only the worktree (and approved paths)
  and cannot read your private keys or reach unapproved hosts.
- **Devcontainer**: runs terminals inside the worktree's `devcontainer.json` container.
- **Host**: runs terminals directly on the project's machine without extra isolation.

## Choosing a runtime

- **Global default**: **Settings > Sandboxing** sets `default_runtime` for newly opened
  worktrees (`sandbox` on Linux and macOS; `host` on Windows).
- **Per project**: **Settings > Project** overrides the runtime for one project's
  worktrees.
- **Per worktree**: the command palette (**Run terminals in a sandbox**, **... in the
  devcontainer**, **... on the host**) switches a single worktree. The status bar shows
  the active runtime and the worktree remembers its choice.

## Platform sandbox mechanisms

| Platform | Mechanism | Notes |
| --- | --- | --- |
| Linux | `bubblewrap` (`bwrap`) | Isolated filesystem, network namespace and loopback by default; Landlock restricts abstract sockets when `host_network` is on (Linux 6.12+) |
| macOS | Seatbelt (`sandbox-exec`) | Denies hidden paths, launch services, window server and cross-process inspection; `/tmp` stays shared |
| Windows | `scriptschnell-term-sandbox` launcher | Write-restricted token with per-project identity and temp dir on a separate desktop; optional experimental AppContainer (`sandbox.windows_appcontainer`) |

On Windows, new worktrees default to **host** because the write-restricted token breaks
MSYS tools (such as Git Bash) and Node.js child-process pipes (used by Claude Code). Use a
devcontainer on Windows when you want worktree isolation without those limits.

## Filesystem and environment isolation

- **Writable paths**: the worktree is always writable, plus extra folders listed in
  `sandbox.writable` (edited on **Settings > Sandboxing > Files**) or per project on
  **Settings > Project**.
- **Coding agents**: agents (Claude Code, Codex, opencode, pi, Copilot CLI, Qwen Code,
  Gemini CLI, Antigravity CLI, Kimi Code, Mistral Vibe) can write their own login,
  session and state files, while their settings, hooks, plugins and skills remain
  read-only.
- **Protected VCS metadata**: `.git/config`, `.git/hooks`, `.git/commondir` and the
  equivalent `.hg` files are kept read-only so a sandbox cannot make the app's host git or
  hg commands execute arbitrary code. Pass one-off git options with `git -c key=value` or
  run `git config` / `git remote add` on the host.
- **Hidden paths**: `sandbox.hidden` hides SSH and GPG keys, cloud credentials,
  `~/.netrc`, `~/.git-credentials`, password stores and the app's own data directories
  (except the read-only references and extensions). On Linux, `XDG_RUNTIME_DIR` is hidden
  and `/tmp` is private to the sandbox.
- **Environment variables**: sandboxes strip `SSH_AUTH_SOCK`, `GPG_AGENT_INFO`,
  `DBUS_SESSION_BUS_ADDRESS` and variables whose names look like credentials (`TOKEN`,
  `SECRET`, `PASSWORD`, `API_KEY`, `ACCESS_KEY`, `PRIVATE_KEY`, `CREDENTIAL`, `AUTH`).
  Coding agent login variables are preserved, and `sandbox.pass_env` lets you keep extra
  variables by name. Shipped extensions (`npm`, `cargo`, `nuget`, `pypi`,
  `git-credentials`, `forge-clis`) inject fake tokens that the proxy swaps for real ones
  outbound so real secrets never enter the sandbox.

## Network isolation and proxy

Sandboxes have three network modes, configured on **Settings > Sandboxing**:

1. **Allowed hosts only** (`network: true`, `restrict_network: true`, the default):
   local Linux and macOS sandboxes have no direct network access and reach only
   `sandbox.allowed_hosts` (such as `crates.io` or `*.github.com`) through scriptschnell-term's
   filtering HTTP proxy via `HTTP_PROXY` / `HTTPS_PROXY`. Disallowed hosts receive `403`.
   The first-start setup and **Settings > Sandboxing > Hosts > Choose common hosts** let
   you toggle common groups (forges, search engines and APIs, AI model APIs, docs,
   package registries).
2. **Full network** (`network: true`, `restrict_network: false`): all hosts are allowed.
   On Linux, unless `sandbox.host_network` is turned on, the sandbox still keeps its own
   loopback so servers inside it do not bind host ports directly and only HTTP/HTTPS and
   proxied git-over-SSH leave through the proxy.
3. **No network** (`network: false`): cuts network access completely.

On Linux, `scriptschnell-term-proxy-bridge` (shipped alongside `scriptschnell-term`)
carries traffic from the bubblewrap sandbox to the proxy. A remote project's sandbox
cannot reach your local proxy and runs without network when restricted.

### Ports and TLS inspection

- **Exposing sandbox ports**: on Linux with an isolated loopback, listening ports are
  forwarded between terminals of the same worktree automatically. To reach a server from
  your host browser, run **Expose port N** in the palette or `control expose N`. To reach
  a server on the host's `127.0.0.1` from inside the sandbox, `control expose-host
  HOST_PORT [PORT]` forwards it onto the sandbox's own `localhost` (`control host-ports`
  lists the forwardings, `control unexpose-host HOST_PORT` stops one).
- **HTTPS inspection** (`sandbox.intercept_tls`, on by default on Linux and macOS): the
  proxy decrypts HTTPS using two per-run in-memory CAs. Hosts with valid certificates get
  signed by the trusted CA (wired into `SSL_CERT_FILE`, curl, git, cargo, Python, Node
  and system CA bundles inside bubblewrap). Hosts with invalid certificates get signed by
  an untrusted CA so clients still reject them unless overridden.
- **Git over SSH**: local Linux and macOS sandboxes with the proxy can run `git fetch`,
  `git pull` and `git push` over SSH to allowed hosts via `GIT_SSH_COMMAND`, using your
  host SSH config and agent without exposing your SSH keys to the sandbox.
- **Commit signing**: the same sandboxes, and Linux ones with `host_network`, sign
  commits and tags with your GPG or SSH key, a security key included. git runs
  `scriptschnell-term-proxy-bridge` as `gpg` and `ssh-keygen`, and the proxy runs your
  own on the host after asking under the `cmdterm.git-sign` guard. PIN and touch prompts
  show on the host, so gpg-agent needs a graphical pinentry.
- **DNS blocklists**: the shipped `blocklist` extension blocks ad, tracker and malware
  hosts from HaGeZi's DNS blocklist in the sandbox proxy.

## Container engines in sandboxes

Controlled by `sandbox.engines` on **Settings > Sandboxing**:

- **`passthrough`** (default): exposes this machine's container engine sockets (docker,
  podman, containerd, buildkit, WSL interop, or relayed Windows pipes). Containers started
  through host engines run outside the sandbox and are not restricted by it; until you
  acknowledge this on the Sandboxing page, the status bar marks sandboxed worktrees as
  `sandbox · weakened`.
- **`sidecar`** (Linux only): hides host engines and starts a dedicated podman-in-podman
  (or docker-in-docker) container per project that sees only the project's worktrees and
  host registry certificates. Sidecars stop when the app quits.
- **`off`**: hides container engine sockets completely.

## Guards and approvals

Guards pause sensitive outbound actions from sandboxes and show an approval dialog where
you can allow once, allow for the session, always allow in the project, or always block in
the project:

- **Publishing and pushes**: `cmdterm.git-push`, `cmdterm.npm-publish`,
  `cmdterm.cargo-publish`, `cmdterm.nuget-push`, `cmdterm.pypi-upload`
- **Pull and merge requests**: creating, commenting on or merging change requests on
  configured forges (`sandbox.change_requests`)
- **Linux keyring** (`cmdterm.keyring`): a filtered Secret Service bus proxy that asks
  before reading, changing or deleting secrets (coding agents' own logins pass in `ask`
  mode)
- **Agent settings sync** (`cmdterm.agent-settings`): merges changes a sandboxed agent
  makes to its config copy back to your host file key by key after asking
- **Opening URLs** (`cmdterm.open-url`): asks before opening `$BROWSER` / `xdg-open`
  links from a sandbox and forwards localhost OAuth callback ports for 10 minutes

Manage per-project allow and block rules on **Settings > Project > Allowed / Blocked**,
and global guard defaults on **Settings > Extensions**.

## Devcontainers

When a worktree contains a `.devcontainer/devcontainer.json` (or `.devcontainer.json`),
running terminals in the devcontainer starts or reuses its container using the
`devcontainer` CLI if installed, or scriptschnell-term's built-in docker/podman runner
otherwise. Configure the engine with `container_engine` on **Settings > Sandboxing**.

For full details on supported `devcontainer.json` keys and limitations, see
[crates/cmdterm/src/container/README.md](../crates/cmdterm/src/container/README.md). For
every `sandbox.*` key, see
[CONFIGURATION.md](../crates/cmdterm/CONFIGURATION.md#sandboxes-and-containers).
