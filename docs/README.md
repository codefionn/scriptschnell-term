# scriptschnell-term documentation

## User guides

- [Workspace, projects and worktrees](workspace.md): sidebar layout, opening and cloning
  repositories, Git/Mercurial/Jujutsu worktrees, sidebar search, tabs, split panes and
  clipboard/image pasting.
- [Built-in panels](panels.md): file browser (with Markdown, LaTeX, Mermaid, SVG and PDF
  rendering), history, base-branch review, change request discussions, debug browser and
  machine monitor.
- [Keybindings and keymaps](keybindings.md): default shortcuts, mouse gestures, keymap
  presets (`cmdterm`, `tmux`, `vscode`, `cursor`, `windows-terminal`, `konsole`) and
  custom bindings.
- [Sandboxes and devcontainers](sandboxes.md): host, devcontainer and sandbox runtimes,
  filesystem and network isolation, TLS inspection, container engines, guards and
  approvals.
- [Coding agents](agents.md): hooks, skills, Agents tab, session resume, idle freezing,
  CPU modes, the control socket, multi-agent coordination, debug browser MCP tools and
  `scriptschnell-term fix`.
- [Remote machines, SSH, VMs and web access](remote.md): SSH projects, `cmdterm://`
  remote machines, Firecracker VMs, VPN peer discovery (Tailscale, NetBird, Nebula) and
  `scriptschnell-term-web`.
- [Extensions](extensions.md): Lua/Teal extensions, permissions, settings and the 20
  shipped extensions.

## Reference documentation

- [Configuration reference](../crates/cmdterm/CONFIGURATION.md): platform directories,
  every `settings.json` and `state.json` key, CLI flags, environment variables and the
  control socket HTTP API.
- [Extension API reference](../crates/cmdterm/src/ext/README.md): the `term` Lua API,
  manifest format, permissions, events, tasks and Teal types.
- [Devcontainer support](../crates/cmdterm/src/container/README.md): supported
  `devcontainer.json` keys, built-in runner and sidecar engines.
- [Remote server and VMs](../crates/cmdterm-remote/README.md): `scriptschnell-term-remote`
  keys, authorization, Firecracker VMs and persistent sessions.
- [Web access server](../crates/cmdterm-web/README.md): `scriptschnell-term-web` setup,
  authentication (passwords and OIDC), machine linking and HTTP API.
- [Terminal emulator](../crates/cmdterm-vt/README.md) and
  [compatibility notes](../crates/cmdterm-vt/COMPAT.md): `cmdterm-vt` library API and
  test suite.
- [Self-updates and release signing](../crates/cmdterm-update/README.md): signed release
  manifests and update layout.

## Project documents

- [SOUL.md](../SOUL.md): design principles and non-goals.
- [AGENTS.md](../AGENTS.md): repository layout, commands and development conventions.
- [CHANGELOG.md](../CHANGELOG.md): release history.
