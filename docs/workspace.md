# Workspace, projects and worktrees

scriptschnell-term organizes terminal and browser panels by version-control project and
worktree. Switching worktrees restores that worktree's panels, while the panels of other
worktrees keep running in the background. State survives restarts.

## Layout

- **Sidebar rail** (left): lists your projects and the worktrees of the selected project,
  with branch names, status counts, open change requests and CI status. The rail also has
  an **Agents** tab showing worktrees where coding agents ran, and a bell icon in the
  bottom corner for status notices and update checks.
- **Lane colors**: every worktree gets a color from a fixed palette by its position in
  the project's worktree list. The color marks its row in the rail, the active tab's
  underline and the focused pane's border.
- **Tab strip** (top): each tab owns its own split panes. A tab with splits is named
  after its focused pane, with `+N` for the others.
- **Status bar** (bottom): shows the runtime environment, CPU mode, branch, working
  directory and grid size of the focused terminal, and the newest notice for eight
  seconds.

Press `F6` to cycle focus from the active panel to the tab strip, status bar, sidebar
search field and back (`Shift+F6` reverses the order; `Escape` returns to the panel).

## Opening and cloning projects

Pass project locations on the command line or open them from the command palette
(`Ctrl+Shift+P` or `+` in the rail):

- **Local path**: `~/src/myrepo`
- **SSH**: `host:/path` or `ssh://[user@]host[:port]/path`
- **Remote machine**: `cmdterm://host[:port]/path[?vm=name]`
- **Local VM**: `vm://name/path`
- **VPN peer**: `tailscale://`, `netbird://` or `nebula://` (see [Remote machines and VMs](remote.md))
- **Extension schemes**: e.g. `k8s://pod/path` and `coder://workspace/path` from the
  shipped `kubernetes` and `coder` extensions

A repository URL (`https://`, `git://`, `file://`, `git@host:org/repo.git`, or an
`ssh://` URL with user `git` or `hg` or a path ending in `.git`) clones instead. The
palette asks for the destination folder (default `~`) and where the clone goes: this
machine, a local VM, a trusted remote machine, or a VM on one. This machine and remote
machines also offer to run terminals in the checkout's devcontainer. Git is tried first,
then Mercurial. With the `jj` setting on, a new git clone also becomes a colocated jj
repository. A folder that already holds a checkout of that name opens as is.

Forgetting a project and removing a worktree live in the command palette.

### Temporary and global sessions

- **Temporary project**: open a project with "Open project temporarily..." in the palette
  or `--temporary` on the command line. It works like any project until you quit, without
  being written to `state.json`.
- **Global session**: open a plain folder without a repository (defaulting to the home
  directory) with "New global session..." or `--global`, for work on dotfiles or machine
  settings. Its terminals run on the host unless switched in the palette.

Both start temporary; "Keep session" in the palette saves one permanently, and "Close
session" stops its terminals and closes it.

## Worktrees and VCS support

scriptschnell-term supports Git and Mercurial out of the box, and Jujutsu (`jj`) once you
turn on `jj` in settings or per project via the palette. With `jj` enabled, worktrees are
jj workspaces and branches are bookmarks.

By default, a worktree is named in the sidebar after its last coding agent session title,
with the branch name shown underneath. Use "Name worktrees by branch" in the palette (or
set `"worktree_titles": "branch"` in `settings.json`) to always show the branch instead.

### Searching worktrees

Press `Ctrl+Shift+S` (or click the field at the top of the sidebar) to filter every
project's worktrees as you type. Search reads agent session titles, branches, project and
folder names, paths, change requests and what agents are waiting for. It understands
stems, typos, initials and coding synonyms, so "auth bug" finds a worktree whose agent
titled its session "Fix the broken sign-in form".

## Tabs and split panes

A new terminal (`Ctrl+Shift+T`) opens a new tab. Splitting (`Ctrl+Shift+D` side by side,
`Ctrl+Shift+E` stacked) starts a new shell beside the focused pane in the same tab.
Closing a pane (`Ctrl+Shift+X`) stops its shell; closing a tab (`Ctrl+Shift+W`) stops
every shell in it. Each tab holds up to 8 panes.

You can also rearrange tabs and panes with the mouse:

- Drag a tab along the strip to reorder tabs.
- Drag a tab onto a pane edge to join its panes into the shown tab as a split on that side.
- Drag a pane by its title onto another pane's edge to move it there, or onto its middle
  to swap the two panes.
- Drag a pane by its title onto `+` after the tabs to move it into its own tab, or onto
  another tab to move it beside that tab's focused pane.

See [Keybindings and keymaps](keybindings.md) for all keyboard shortcuts.

## Selection and clipboard

- **Text selection**: double-click selects a word (paths, URLs and `file.rs:12:3` count
  as one word); triple-click selects a whole wrapped line. Dragging afterwards extends by
  words or lines. `Shift+click` extends the selection, and bypasses mouse grabbing in
  programs that capture the mouse.
- **Copy and paste**: `Ctrl+Shift+C` copies the selection and `Ctrl+Shift+V` pastes in
  terminals, file and history panels and the debug browser. Without a selection,
  `Ctrl+Shift+C` does nothing and never sends `Ctrl+C` to the program.

### Pasting images

`Ctrl+Shift+V` with an image and no text on the clipboard saves the image as a PNG file
where the terminal runs and pastes its path. Coding agents attach a pasted image path as
the image. This works locally, on remote machines, in containers and in sandboxes. On
Windows the path uses forward slashes so tools like Claude Code read it cleanly.

Agents also read images directly from the clipboard on `Ctrl+V` (`Alt+V` on Windows). For
terminals without a desktop clipboard (SSH hosts, remote machines, VMs, containers and
Linux sandboxes), pressing `Ctrl+V` or `Alt+V` copies the image to
`~/.cache/scriptschnell-term-clipboard/clipboard.png` (or the app's cache directory in a
sandbox) before passing the key to the program. Shims for `xclip` and `wl-paste` return
that image for the next minute and pass other calls to the real tool:

- On remote machines and containers, the shims live in `~/.local/bin`, which must be on
  `PATH` ahead of the real tools (existing user files of that name are left untouched).
- In a bubblewrap sandbox, the shims are mounted over the host's own `xclip` and
  `wl-paste`, so one of them must be installed on the host.

Images only leave your machine when you press one of these paste keys, and pasted files
older than a day are deleted on the next paste.

### Copying from sandboxes and remote machines

Programs in any terminal can write text to your desktop clipboard with OSC 52 (reading the
clipboard via OSC 52 is refused). Programs inside a bubblewrap sandbox or on another
machine that call `wl-copy`, `xclip` or `xsel` are intercepted by the same shims, which
forward copied text to the terminal as OSC 52 instead. Image copies (`-t image/png`) pass
through to the real tool.
