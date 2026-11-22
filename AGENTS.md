# scriptschnell-term agent guide

Read `SOUL.md` before changing code. It ranks the principles; this file holds the
conventions the code does not show on its own.

## Layout

scriptschnell-term is six crates. Every process it starts goes through `cmdterm-env`'s
`Environment` (SOUL principle 4). `cmdterm-remote`, `cmdterm-update` and `cmdterm-web` are
standalone tools with their own binaries. `cmdterm-vt` is the terminal emulator library. `crates/cmdterm` is the app: the `scriptschnell-term`
binary, a library crate its integration tests link, and the subsystems it orchestrates
as modules under `crates/cmdterm/src/`.

| Crate | Owns |
| --- | --- |
| `crates/cmdterm-env` | `Environment`: local, SSH, a scriptschnell-term remote machine or a local VM, plus container and sandbox wrappers around any of them (bubblewrap, Seatbelt on macOS, the `cmdterm-sandbox` launcher on Windows), the allowlist proxy for sandboxes with a restricted network (`proxy.rs`, `cmdterm-proxy-bridge`), which also swaps extensions' fake credentials for real ones, holds back guarded requests such as pushes (`proxy/rules.rs`, `proxy/approval.rs`) and refuses blocklisted hosts (`proxy/blocklist.rs`), the keyring bus that stands in for the session bus in bubblewrap sandboxes and passes only the Secret Service, asking before secrets are read or changed (`keyring.rs`), container engine and WSL passthrough into sandboxes (`engines.rs`, with the launcher's pipe relays in `windows_sandbox/relay.rs`), `copy_in`, CPU limits for terminals (`cpu.rs`: the systemd scope and slice script, which cores are efficiency cores and which a terminal gets, `taskset` and autogroup fallbacks, the `cmdterm-remote:cpu` answers of servers; `cpu/windows.rs`: the same with nested job objects, which `pty/windows.rs` starts terminals in), `trust_certificate` for the trust store of an environment (`trust.rs`), and the operating system's secret storage (`os_secrets.rs`, through `secret-tool` or `security`). Every process scriptschnell-term starts goes through it. `pty/` (`cmdterm_env::pty`) is the PTY layer: it opens the PTYs and starts their children, with `CommandBuilder`, `Pty`, `Child` and `ChildKiller`; its unsafe code is in `pty/unix.rs` and `pty/windows.rs`, and the job objects' in `cpu/windows.rs`. `Environment::shell` and `EnvCommand::to_pty` return its `CommandBuilder`. `dirs.rs` says where the app keeps its files on each platform (`AppDirs`) and moves those of older versions. |
| `crates/cmdterm-remote` | Remote machines: `cmdterm-remote` server, client and master, hybrid post-quantum handshake, Firecracker VMs for clients and for this machine (`vmd`), and sessions that keep terminals running across lost connections and app restarts (`session.rs`), and the list of projects a server shares with clients that show them in their sidebar (`projects.rs`). The server starts terminals under the client's CPU limit and changes it on `cmdterm-remote:cpu` channels (`exec.rs`). Its `README.md` covers setup and the protocol. |
| `crates/cmdterm-web` | The web server for browsers and phones (`cmdterm-web` binary): password, single sign-on (OpenID Connect) and `--dev` token logins (`auth.rs`, `oidc.rs`), accounts, linked machines and browser sessions in `store.json` (`store.rs`), the port linked machines dial and the channels it opens on them (`hub.rs`), the routes and terminal WebSockets (`api.rs`), and the offline-capable web app in `web/` (embedded from `web/FILES.txt` by `build.rs`). Machines link with `cmdterm-remote link` (`crates/cmdterm-remote/src/link.rs`). Its `README.md` covers setup, security and the HTTP API. |
| `crates/cmdterm-update` | Self-updates: release keys (Ed25519 plus ML-DSA-65), signed `manifest-<platform>.json` release assets listing each file's SHA3-256, the release check against the codefionn.eu mirror and GitHub releases, the file swap of Windows installs and the AppImage swap on Linux (`Target`), and `cmdterm-release`, which the release workflow (`.github/workflows/ci.yml`) runs to sign manifests. Its `README.md` covers the release layout and key setup. |
| `crates/cmdterm-vt` | The terminal emulator core, with no I/O and no unsafe code: the escape sequence parser, `Terminal` (screen, scrollback with reflow, styles, grapheme clusters, DEC modes, OSC handling, device replies), the input encoders for keys (legacy, modifyOtherKeys, kitty), mouse, paste and focus, and the palette. `Terminal::advance` takes PTY output; replies and events are read with `take_replies` and `drain_events` and written or handled by the caller after it drops the lock. Its `README.md` covers the API and the tests, and `COMPAT.md` lists where it differs on purpose from xterm and from wezterm-term, which it was first compared against. |
| `crates/cmdterm` | The app: `workspace.rs` (live model, dirty flags), `ui.rs` and `ui/` (tokens and lane palette in `theme.rs`; `rail.rs`, `tabs.rs`, `panel.rs`, `status.rs` build one region each; `palette.rs` is the Ctrl+Shift+P overlay with its own region and dirty flags; `settings.rs` is the Ctrl+, overlay for font, color scheme, keyboard, extensions, agents, remote machines with their VPN networks, VMs, containers and language; `machines.rs` builds those pages; `approval.rs` is the overlay that asks whether a sandbox request an extension guard holds back may pass; `notifications.rs` is the overlay behind the bell in the rail's footer; `change.rs` builds the explore panel's change request page from widgets), `i18n.rs` (English and German string tables), `runtime.rs` (where a worktree's terminals run: host, devcontainer or sandbox; extension copy, run and trust jobs, and the `done` callbacks they end in), `machines.rs` (serving this machine from the app, trusted and allowed keys, `server.json`, local VMs, container and sandbox settings), `vpn.rs` (Tailscale, NetBird and Nebula peers, and the `tailscale://`, `netbird://` and `nebula://` locations), `fusion.rs` (fused remote machines: the worker that asks the machines in `remote.fused` for their shared projects and the sidebar entries it keeps for them), `keymap.rs` (keymap presets, extension and user bindings, key matching), `files.rs` (the read-only file panel: one terminal grid patched per row, with `files/tree.rs` for the VCS file tree, `files/document.rs` for loaded files, `files/highlight.rs` for the allocation-free per-line lexers, `files/render.rs` for the rendered mode of pictures, SVG and PDF pages, placed on the grid as one image, and `files/markdown.rs` for Markdown laid out as styled lines), `explore.rs` (the read-only history and review panel on the same grid technique, with `explore/diff.rs` parsing git's extended diff format; its change request mode is `ui/change.rs`), `bundle.rs` (the files every start writes into the configuration directory), `control.rs` (the control socket: an HTTP API on a Unix socket through which agents in the app's terminals read and change extensions, every change shown in the approval overlay first; `control/bus.rs` is the agent list, the messages between agents and the locks they queue for), `blocklists.rs` (downloads and caches the blocklists extensions name), `agents.rs` (agent hooks and the `configure-scriptschnell-term` skill in `agents/SKILL.md`; `agents/custom.rs` keeps the agents extensions register with `term.agent`, which `Agent::Custom` carries everywhere a built-in agent goes), `agent_sync.rs` (the copies of agents' settings in sandboxes and the worker that brings their changes to the host after asking), `cpu.rs` (CPU modes: the per-machine cap, the weight of agents in hidden terminals and the cores of hidden and shown ones, with what `cpu_mode` in the settings changes about them, applied by a worker thread when the mode or a terminal's visibility changes; the status bar, the Agents settings page and the palette switch them), `memory.rs` (memory pressure from `/proc/meminfo` and `/proc/pressure/memory`, which makes idle agents freeze sooner), `taskview.rs` (the tasks panel, a debug view of every worktree's panels and whether they are shown, hidden, frozen or exited, of the extension tasks and of the app's threads from `/proc/self/task`, drawn like the monitor panel in `monitor.rs`), `fix.rs` (the `fix` subcommand: the preferred agent in a PTY relayed to the user's terminal, with a prompt typed but not sent), `notifications.rs` (the notification list, update checks for agent integrations, npm or mise agent installs and the app itself), `forges.rs` (forge accounts from `term.forge`, tokens in the system's secret storage, `term.http` requests, the merge checks behind the rail's Agents tab, which `ui/agents.rs` builds, with their `git ls-remote` gate, and the discussion view), `discussion.rs` (renders a change request's discussion, placing code comments on the worktree's head through renames and force pushes with `git diff`, the reflog and `range-diff`), `vcs_worker.rs`, `terminal_panel.rs`, `browser.rs`, `debug_browser.rs` (the panel on the `devtools` debug browser). |

The subsystems are modules of `crates/cmdterm/src/`. Each was its own crate before the
crates were consolidated:

| Module | Owns |
| --- | --- |
| `container/` | Devcontainers: `devcontainer.json` (JSONC), the devcontainer CLI when installed, else a built-in subset on docker or podman. Produces a `ContainerTarget`. Also starts the engine sidecars of sandboxes (`sidecar.rs`). (was `cmdterm-container`) |
| `devtools/` | The debug browser: one Chrome (DevTools protocol on a pipe, `cdp.rs`) or Firefox (WebDriver BiDi, `bidi.rs`, over the small WebSocket client in `ws.rs`) for the whole app, started through `cmdterm-env`. `Hub` streams the shown tab's frames to the panels and takes their input; `mcp.rs` answers the MCP tools agents reach through `POST /v1/mcp`, with stateless sessions an agent names by id; `page.js` builds the accessibility snapshot and keeps element refs. (was `cmdterm-devtools`) |
| `ext/` | Lua extension host on Oximond, with the `manifest.toml` permission model (`manifest.rs`). Its `README.md` is the extension author reference. (was `cmdterm-ext`) |
| `forge/` | GitLab, Azure DevOps, Forgejo REST: remote URL detection, change requests, pipelines. (was `cmdterm-forge`) |
| `state/` | `state.json`: projects, worktrees, saved panel layout per worktree. `settings.json`: font, color scheme, language, turned-off extensions. (was `cmdterm-state`) |
| `term/` | PTY sessions on the `cmdterm-vt` terminal model, with the child started through `cmdterm_env::pty`, key translation to its key codes, OSC 7501 status reports from its passthrough events, row sync into the SchnellUI grid. The model's lock never covers a PTY write: replies, keys, pastes and mouse reports are encoded under it and written after. (was `cmdterm-term`) |
| `vcs/` | Git, Mercurial and, where the `jj` setting or a project turns it on, Jujutsu: discovery, cloning, worktrees (hg shares, jj workspaces), status, remotes, branches, file lists. (was `cmdterm-vcs`) |

`extensions/` holds extensions shipped as examples (`claude-code`, `dotfiles`,
`blocklist` for blocked hosts in sandboxes, and `npm`, `cargo`, `nuget`, `pypi`,
`git-credentials` for registry logins in sandboxes, `forge-clis` for the logins of
`fj`, `tea`, `glab` and `gh` in sandboxes, `toolchains` for rustup, nvm, npx,
dotnet tools and other toolchain managers in sandboxes, `aspnet-devcerts`, off by
default, for this machine's ASP.NET Core dev certificate in devcontainers,
`github`, `gitlab`, `bitbucket`, `forgejo`, `gitea` and `azure-devops` for the Agents tab's merge checks and change request discussions);
tests in `crates/cmdterm/src/ext` load them. `cmdterm-remote image alpine|ubuntu` builds a VM image without root
(`crates/cmdterm-remote/src/image.rs`).

## External dependencies

- `schnellui` (`https://github.com/codefionn/schnellui`) is a git dependency that
  scriptschnell-term changes when it needs to (SOUL principle 6). Its `SOUL.md` and
  `docs/architecture.md` apply to every change there, including its allocation
  budgets, which only go down. Add a budget test or bench row for every hot path
  you add to it. Override it with a local Cargo `[patch]` when editing `../schnellui`
  alongside this checkout.
- `oximond` (`https://github.com/codefionn/oximond`) is a git dependency used as
  published. Another session develops it. Do not edit it; report gaps with a repro
  instead (`crates/cmdterm/src/ext` lists known ones).
- No crate depends on wezterm-term, termwiz or portable-pty. The emulator is `crates/cmdterm-vt` and
  PTYs come from `cmdterm_env::pty`. `crates/cmdterm-vt/COMPAT.md` keeps the differences from wezterm-term
  that the former differential tests found.

## Commands

Test single crates with `-p`.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
# Headless screenshot plus accessibility tree, against a scratch config:
target/debug/scriptschnell-term --config-dir "$SCRATCH/cfg" --screenshot "$SCRATCH/shot.png" \
  --dump-a11y "$SCRATCH/a11y.json" /path/to/repo
```

Always pass `--config-dir` to a scratch directory when you run the app. The default is
the user's real directories (`~/.config/scriptschnell-term`, `~/.local/share/scriptschnell-term`,
`~/.local/state/scriptschnell-term`, `~/.cache/scriptschnell-term`); `--config-dir` puts all of them into one. A fresh config opens the first-start setup
over everything; put `{"sandbox": {"hosts_chosen": true}}` in the scratch `settings.json`
to skip it.

To drive a live instance, build with `--features debug-instrumentation` and run with
`SCHNELLUI_HEADLESS_WINDOWED=1`; `../schnellui/docs/debugging/DEBUG_INSTRUMENTATION.md`
covers the endpoint. The `browser` feature embeds Servo; its first build takes tens of
minutes, so leave it off unless you work on browser panels.

## Conventions

- **Processes.** Only `cmdterm-env` constructs `std::process::Command`. Everything
  else calls `Environment::command(..)` or `Environment::shell(..)`, so remote
  projects keep working.
- **Hot paths.** These run per frame or per PTY chunk and must not allocate in steady
  state: the `TerminalGrid::patched` closures and `GridSync::sync`, `Workspace::poll`,
  key and pointer input in `TerminalPanel::input`, `Keymap::press`. Mounting views, subtree
  replacement, VCS work, and saving state are cold and may allocate.
- **Dirty flags.** A change sets the narrowest `Dirty` flag in `workspace.rs`:
  `tabs` for a title, `status` for a notice, `main` for the active panel, `sidebar`
  for VCS data, `settings` for the settings overlay. `full` remounts the whole app
  and is reserved for changes that affect every region, such as a font or color
  scheme change.
- **Colors.** Views read colors from `inner.scheme` (`ui/theme.rs`), never from
  literals, so every scheme applies to them. A new scheme needs all tokens and
  the 16 ANSI colors.
- **Threads.** Worker threads (PTY readers, the VCS worker) never touch the UI. They
  bump a revision or send a reply, then call the redraw signal.
- **State.** Persist through `Workspace::save` after structural changes, never per
  frame. New state belongs to a worktree unless it clearly belongs to the app.
- **Accessibility.** Every interactive widget has a role and a name. Terminals in the
  first pane are named `Terminal`, the grids of later panes `Terminal (pane 2)` up to
  `Terminal (pane 8)`, numbered in layout order. Browser surfaces use `Web content` and
  `Web content (pane N)`, debug browser panels `Debug browser` and
  `Debug browser (pane N)`. `build_app` registers one focused-input handler per name, and
  each routes to its own pane's panel, so a tab shows at most `MAX_PANES` panes.
- **Languages.** The UI speaks English and German. Views read every string from
  `inner.tr` (`i18n.rs`), never from literals; a new string needs a field in
  `Strings` and an entry in both `EN` and `DE`. Names that code finds widgets by
  (terminal and web content panes, `PALETTE_INPUT`, `PALETTE_BRANCH_INPUT`) stay
  English. Tests always run in English. Run the app with `{"language": "en"}` in
  the scratch `settings.json` when you drive it by accessible names.
- **Shipped docs.** The app is named scriptschnell-term; the crates keep their
  `cmdterm-*` names, and so do ids that settings or other machines store (the `cmdterm`
  keymap, `cmdterm.` guard ids, `cmdterm://` URLs). `bundle.rs` writes these files into
  the user's configuration directory on every start, and the agent skill sends agents to
  them, so they are user documentation: `crates/cmdterm/CONFIGURATION.md` (every
  `settings.json` key, flags, variables), `crates/cmdterm/src/ext/README.md` (the `term` Lua
  API), `crates/cmdterm/src/ext/types/term.lua` and `term.d.tl` (its LuaLS and Teal types),
  and `extensions/<name>/README.md`. A change to a setting, the Lua API or a shipped
  extension updates the matching file in the same change. A new shipped extension needs
  `init.lua`, `manifest.toml` and `README.md`, and an entry in `bundle::EXTENSIONS`.
- **Writing.** Comments, docs, and commit messages use plain, specific language: no
  em dashes, no filler, sentence-case headings.
