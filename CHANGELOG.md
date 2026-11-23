# Changelog

All notable changes to scriptschnell-term are in this file. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Git over SSH works in sandboxes that reach the network through the app's proxy: local
  bubblewrap sandboxes with a network of their own and Seatbelt sandboxes on macOS.
  `GIT_SSH_COMMAND` there runs the proxy bridge in place of `ssh`, and the proxy runs
  this machine's `ssh` with your configuration, agent and known hosts, so `git clone`,
  `fetch` and `push` reach allowed hosts while your keys stay outside the sandbox. Only
  git's own remote commands pass. A push asks under `cmdterm.git-push` like one over
  HTTPS. SSH from a sandbox never reaches this machine, even with `localhost` allowed.

### Fixed

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

[Unreleased]: https://github.com/codefionn/scriptschnell-term/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/codefionn/scriptschnell-term/releases/tag/v0.1.1
[0.1.0]: https://github.com/codefionn/scriptschnell-term/releases/tag/v0.1.0
