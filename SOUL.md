# scriptschnell-term SOUL

This document is the constitution of **scriptschnell-term**. It is written for the humans and AI agents
who build it. When a design decision is unclear, this file is the tiebreaker. Code that
contradicts this document is wrong, even if it works.

## Identity

scriptschnell-term is a native, GPU-rendered workspace of terminal and browser panels, organized by
version-control project and worktree. The left sidebar lists projects (a repository, local or
on a host reached over SSH) and the worktrees of the selected project. Picking a worktree
brings back that worktree's panels: its shells, its running commands, its preview pages.
It is a terminal you command, scoped to the code you are working on.

It is written in Rust, rendered by SchnellUI on the GPU, emulates terminals with its own
emulator, cmdterm-vt, embeds Servo for browser panels, and runs extensions in Lua on Oximond.

## Why it exists

Working on several branches at once means a pile of terminal tabs and browser windows that
know nothing about which branch they belong to. Terminal emulators are fast but blind to the
repository. IDEs know the repository but bury the terminal and cost a gigabyte of RAM to show
a prompt. Electron terminals pay a browser engine for every glyph. Remote work is bolted on
everywhere: a different window, a different plugin, a different set of features.

scriptschnell-term closes that gap. The worktree is the unit of work, the terminal is the primary
surface, the remote host is just another place commands run, and the whole thing stays as
cheap as a plain terminal emulator while it idles.

## Core principles

These are decision rules, not aspirations. Apply them literally. When two conflict, the lower
number wins.

1. **Allocate little, and prove it.** An idle frame allocates nothing. A keystroke echo
   rewrites one terminal row and uploads one row of instances. Terminal output costs work
   proportional to the rows that changed, never to the scrollback or the window. Hot paths
   reuse buffers they own. SchnellUI's allocation covenant applies to every view scriptschnell-term
   builds, and scriptschnell-term adds its own budget tests for the paths it owns (PTY to grid sync,
   panel switching, sidebar refresh). A budget only goes down.
2. **The GPU draws; the CPU decides what changed.** All pixels come from SchnellUI's
   retained GPU scene. The CPU's job is to find the changed rows, cells, or labels and patch
   exactly those. Full rebuilds happen on structural change (a panel opens, the worktree
   switches), never on content change.
3. **The worktree is the unit of work.** A project owns its worktrees. Each worktree owns its
   panel layout, its terminals' working directories, its environment variables, and its
   preview URLs. Switching worktrees saves the one you leave and restores the one you enter.
   State survives restarts.
4. **Remote is not a mode.** Every process scriptschnell-term starts, VCS query or terminal shell, goes
   through one `Environment` abstraction (`crates/cmdterm-env`). A project on an SSH host has
   the same sidebar, the same worktree switching, and the same extension API as a local one.
   SSH connections are multiplexed, so a background poll costs a channel, not a handshake.
5. **Speak each VCS in its own tongue.** Git and Mercurial are driven through their own
   command-line tools with machine-readable output (`-z`, porcelain v2, templates), which
   works identically over SSH and never drifts from the user's installed version. Hosted
   forges (GitLab, Azure DevOps, Forgejo) are reached through their REST APIs. Parsing reads
   borrowed bytes; it does not build a string per line.
6. **Fix the framework, not around it.** SchnellUI is a sibling checkout that scriptschnell-term is free
   to change. When it is slow or missing a capability, the fix lands in SchnellUI, with its
   own tests and budget rows, instead of a workaround in cmdterm. Oximond is used as
   published; gaps are reported to its maintainers with a precise repro.
7. **Extensions cannot break the host.** Extensions are Lua, one VM each, talking to scriptschnell-term
   through a request queue. An extension error is reported and disables that extension; it
   never panics the app, blocks a frame, or corrupts another extension.
8. **Legible to agents.** scriptschnell-term runs headless with SchnellUI's instrumentation: an agent can
   start it against a scratch config directory, drive it through the accessibility tree,
   screenshot it, and quit. Every panel and sidebar row carries a role and a name.

## Non-goals

- **Not an editor or an IDE.** scriptschnell-term hosts editors in terminals. It does not grow a text
  editor, a language server client, or a debugger UI.
- **Not a web shell.** The chrome is native SchnellUI. Servo renders page content inside a
  panel; it never renders scriptschnell-term itself. The one exception is `scriptschnell-term-web`, a
  separate server for the devices that cannot run the app, such as a phone: it lists your
  machines and projects and attaches to their terminal sessions in a browser. It never
  grows into a second app, and the native app never depends on it.
- **Not a terminal multiplexer.** No panes inside a terminal, no prefix keys, no session
  manager of its own. Local and SSH sessions belong to the running app; on an SSH host,
  persistence is the job of the host's own tools. The one exception is a scriptschnell-term
  remote machine or VM: its server keeps a terminal's program running across lost
  connections and app restarts, because it is the only program on that machine that
  could.
- **Not a Git GUI.** The sidebar shows worktrees, branches, status counts, and change
  requests so you can pick where to work. The explore panel reads history and diffs a
  worktree against its base for review; it never writes. Staging, committing, and
  rebasing happen in the terminal, with the tools you already use.
- **Not a reimplementation of Git.** No libgit2 or gitoxide clone of repository internals;
  that would fork behavior from the user's git and fail on remote hosts.

## Decision filter

When in doubt:

- Choose the path that touches fewer rows, nodes, or bytes on the frame that follows.
- Choose the design that works the same over SSH as it does locally.
- Choose a change in SchnellUI over a workaround in cmdterm.
- Choose a measured number over a belief about performance.
- Choose the worktree as the scope for any new piece of state.

## Evolution

SOUL.md should change rarely. Update it when the project learns something that changes its
identity-level principles, not when an implementation detail changes. Operational guidance
(commands, layout, conventions) lives in `AGENTS.md`.
