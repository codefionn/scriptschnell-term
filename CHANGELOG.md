# Changelog

All notable changes to scriptschnell-term are in this file. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Settings, Project has two new pages, Allowed and Blocked, that list the guarded
  requests the active project answered for good, such as a `git push` to a host or a
  keyring secret. Remove takes an entry back, "Block instead" and "Allow instead" move
  it to the other page, and a form adds one for a guard and a host. The switch that lets
  agents open worktrees without asking moved to the Allowed page.
- The approval dialog offers "Always block in this project". The project keeps the
  answer in `state.json` (`always_blocked`), and later requests for that guard and host
  are refused without a question.
- Programs in a sandbox with a proxy open links in your browser outside it, after you
  allow it under the new `cmdterm.open-url` guard. `$BROWSER` points to a shim, which
  on Linux also stands in for `xdg-open`, and the shim sends `http` and `https` links
  through the proxy to the app. A localhost port the link names, such as an OAuth
  `redirect_uri`, is forwarded from this machine into the sandbox for 10 minutes, so
  logins like Claude Code's `/login` and `gh auth login --web` complete from a sandbox.

### Fixed

- Agents the app starts itself, from a launch, the palette or a saved layout, no
  longer exit at once with "exec: vibe: not found" when they live in a directory the
  app's `PATH` lacks, such as `~/.local/bin` for an app started from the desktop. An
  agent missing from `PATH` is looked up on the `PATH` of your login shell. This
  affected Mistral Vibe, Kimi Code and any agent installed with uv, pipx or a user npm
  prefix.
- Opening a dropdown near the lower edge of a scrolled settings page, such as the CPU
  dropdowns on the Agents page, scrolls the page to show its whole list instead of
  leaving it cut off at the dialog's edge.
- Claude Code in a sandbox keeps its login and can log in again. The proxy refused
  `platform.claude.com`, where Claude Code renews its token and finishes `/login`, so a
  sandboxed Claude Code lost its login when the access token expired, and `/login` there
  failed after you had signed in. Since every sandbox shares `~/.claude/.credentials.json`,
  the login then ended in all of them at once. Sandboxes now reach `platform.claude.com`
  and `console.anthropic.com` whenever `api.anthropic.com` is allowed, also with
  settings saved before this change, and a renewed or new login reaches running Claude
  Code sessions in the other sandboxes through that shared file.

## [0.1.2] - 2026-11-23

### Added

- Agents read the pull or merge request of a worktree's branch from the app's own
  forge checks with the `change_request` and `change_request_wait` tools of the
  `scriptschnell-term-browser` MCP server, `GET /v1/change-request` on the control
  socket, or `control change-request [--wait]`. They get its state, number and URL,
  and once the app has read the discussion, CI checks, reviews, comment counts and the
  head commit, all without asking the forge themselves. A wait returns when the state
  changes. While an agent watches a worktree, the app adds its branch to its checks,
  finds pushes and merges with `git ls-remote` within minutes, and reads the
  discussion again when refs move or 30 minutes have passed. Codex now gives the MCP
  server's tools 900 seconds so these waits can finish.

- Git over SSH works in sandboxes that reach the network through the app's proxy: local
  bubblewrap sandboxes with a network of their own and Seatbelt sandboxes on macOS.
  `GIT_SSH_COMMAND` there runs the proxy bridge in place of `ssh`, and the proxy runs
  this machine's `ssh` with your configuration, agent and known hosts, so `git clone`,
  `fetch` and `push` reach allowed hosts while your keys stay outside the sandbox. Only
  git's own remote commands pass. A push asks under `cmdterm.git-push` like one over
  HTTPS. SSH from a sandbox never reaches this machine, even with `localhost` allowed.
- `control unlock NAME` frees the holds the agent's own terminal has on a `control run
  --mutex` lock, such as one a killed command left. While `control run` waits, it prints
  which commands hold the lock and how long they have, and says when one of them came
  from the same terminal, as when an agent waits for a build it moved to the background.

### Fixed

- The clipboard shims no longer hand a call back and forth for ever when two of them
  are on `PATH`, such as the one in `~/.local/bin` and the one a sandbox binds over
  `wl-paste` or `xclip`. A shim now skips other shims when it looks for the real tool.
- Mistral Vibe starts in bubblewrap sandboxes. Its TUI reads its API key from the
  keyring before it draws more than its banner, and the keyring bus held that read
  for a dialog, so Vibe sat at "Initializing…" or failed when the read was refused.
  Vibe's keyring logins (`service` `ai.mistral.vibe` and `ai.mistral.vibe.oidc`)
  now pass without a question, like Kimi Code's. The default AI host group allows
  `auth.mistral.ai`, `console.mistral.ai` and `chat.mistral.ai`, which Vibe needs
  with a Mistral account login. Vibe gets `VIBE_HOME` pointing to a directory of
  its own with a synced copy of `~/.vibe/config.toml`, as Codex does with
  `CODEX_HOME`, because it saves its settings by renaming a new file over them,
  which the read-only bind refused with "Device or resource busy".
- Agents that were in the middle of a turn when the app closed now resume at start in
  every worktree, not only the one that shows. Before, an agent in another worktree
  waited until you opened that worktree. Idle agents still start once their worktree
  shows.
- Programs that read only the system's CA bundle, such as appimagetool with its
  static libcurl, now trust HTTPS through the proxy in bubblewrap sandboxes instead
  of failing with "status code 0". Sandboxes bind the proxy's bundle, which holds the
  system roots too, over `/etc/ssl/certs/ca-certificates.crt`, `/etc/ssl/cert.pem`
  and their Fedora and openSUSE counterparts.
- A `control run --mutex` lock no longer stays held for hours after the shell running it
  was killed with SIGKILL. The `curl` that held the request open survived its shell; the
  app now sends each lock request an empty line every 10 seconds, which ends such a
  `curl` and frees the lock.
- `scripts/build-appimage.sh` builds an AppImage with a PNG `.DirIcon`, which
  appimagetool now asks for, and caches the AppImage runtime next to appimagetool.

## [0.1.1] - 2026-11-22

### Added

- The context menu of a worktree in the rail fetches, pulls and pushes its branch,
  and pushes it to each remote by name when the repository has several. A project's
  own row fetches from every remote. The command palette has the same entries for
  the shown worktree. Git pulls only fast-forward and a first push sets the
  upstream; Mercurial pushes the checked-out revision with its active bookmark;
  Jujutsu fetches and pushes the workspace's bookmarks. A notice says when each run
  starts and how it ended.

### Fixed

- `ssh`, and with it `git push` and `git fetch` over SSH, works again in bubblewrap
  sandboxes instead of stopping at "Bad owner or permissions on
  /etc/ssh/ssh_config.d/...". Root's files show as nobody's inside, and ssh refuses
  included configuration files that neither root nor the user owns, so sandboxes now
  bind copies the user owns over the files in `/etc/ssh/ssh_config.d`.
- Terminals in bubblewrap sandboxes, coding agents included, no longer exit at once
  when the app runs from an AppImage. A sandbox gets a fresh /tmp, which hid the
  proxy bridge every sandboxed command starts through; sandboxes now bind the bridge
  at its own path.
- Sandboxes on a Linux machine reached over SSH from Windows also bind the proxy
  bridge. The app checked the bridge's path with Windows path rules, which do not
  count `/tmp/...` as absolute, so it left the bind out.

### Changed

- On every start from an AppImage the app copies `cmdterm-remote` and
  `cmdterm-proxy-bridge` into `bin/` of its data directory and runs those copies, so
  helpers that outlive the app keep their file after the AppImage mount goes away. A
  copy is replaced by a rename only when its size or modification time changed.
- Commands the app starts no longer inherit the AppImage runtime's `APPIMAGE`,
  `APPDIR`, `ARGV0` and `OWD` variables.

## [0.1.0] - 2026-11-22

Initial release.

### Added

- GPU-rendered workspace of terminal and browser panels, organized by version-control
  project and worktree. The rail lists Git, Mercurial and Jujutsu projects, local or
  over SSH, and their worktrees; switching worktrees brings back that worktree's
  panels, which keep running in the background.
- Terminal emulator core (`cmdterm-vt`) with PTYs and sandboxes through `cmdterm-env`:
  bubblewrap on Linux, Seatbelt on macOS, the `cmdterm-sandbox` launcher on Windows,
  with an allowlist proxy for sandboxes with a restricted network.
- Remote machines (`cmdterm-remote serve` or `cmdterm --serve`), Firecracker VMs on
  this machine and on remote machines, devcontainers, and the `cmdterm-web` server
  for browsers and phones.
- Lua extensions on Oximond with a `manifest.toml` permission model, plus shipped
  example extensions.
- Change requests and CI status for GitLab, Azure DevOps and Forgejo remotes.
- Self-updates from GitHub releases and the codefionn.eu mirror, with signed release
  manifests.

[Unreleased]: https://github.com/codefionn/scriptschnell-term/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/codefionn/scriptschnell-term/releases/tag/v0.1.2
[0.1.1]: https://github.com/codefionn/scriptschnell-term/releases/tag/v0.1.1
[0.1.0]: https://github.com/codefionn/scriptschnell-term/releases/tag/v0.1.0
