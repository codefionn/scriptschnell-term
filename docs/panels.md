# Built-in panels

Besides terminal panes, scriptschnell-term includes read-only panels for browsing files,
reviewing git/hg/jj history and change requests, inspecting a shared debug browser and
monitoring machine load.

## File browser

Open with `Ctrl+Shift+F` or **Browse files** in the command palette. The panel shows the
worktree's VCS-listed files (tracked files plus unignored untracked files) on the left and
the open file on the right, with line numbers and syntax highlighting for about 25
languages. The panel is read-only; open an editor in a terminal to edit files. The open
file is restored after a restart.

### Rendered files

Markdown, pictures (PNG, JPEG, GIF, WebP, BMP, ICO), SVG and PDF files open rendered:

- **Markdown** wraps to the panel width. LaTeX math (`$..$`, `$$..$$`, `\(..\)`, `\[..\]`
  and `math` code blocks) renders as Unicode text with stacked fractions in display math.
  `mermaid` code blocks render flowcharts and sequence diagrams as box drawings (other
  diagram types show their source).
- **Pictures** fit the panel without growing past their own size.
- **SVG drawings and PDF pages** scale to fill the panel.

Press `V` (or click the file's header) to toggle Markdown and SVG files between rendered
and source view. Text files are read up to 16 MiB; pictures and PDFs up to 64 MiB.

### File browser keys

| Keys | Action |
| --- | --- |
| `Up` / `Down`, `J` / `K` | Move in the tree or scroll the file |
| `Enter` | Open a folder or file |
| `Right` | Preview a file while keeping focus in the tree |
| `Left` | Close a folder or move to its parent |
| `Tab` | Switch focus between the tree and the file |
| `H` / `L`, arrows | Scroll horizontally or vertically in the file |
| `PageUp` / `PageDown`, `Space` | Page through the file (or turn PDF pages) |
| `Home` / `End`, `G` | Jump to top or bottom (first or last page in a PDF) |
| `V` | Toggle rendered / source view (Markdown and SVG) |
| `R` or `F5` | Reload the file tree and open file |
| Drag + `Ctrl+Shift+C` | Select and copy text |

## History, review and change request

The explore panel has three modes; press `V` (or use the top bar) to switch between them.
The panel is read-only. Mode, base branch and uncommitted-changes toggle are restored
after a restart.

- **History** (`Ctrl+Shift+H`, or **Explore history**): lists commits reachable from the
  checked-out revision and displays the selected commit's message, changed files and diff.
- **Review** (`Ctrl+Shift+R`, or **Review changes**): shows the diff from where the
  worktree left its base branch, with changed files and line counts on the left and a
  syntax-highlighted diff with old and new line numbers on the right.
- **Change request** (**Show the change request** in the palette, or the button beside a
  row in the rail's Agents tab): displays the branch's pull or merge request as reported
  by its forge extension. The left list groups the overview (state, draft, target branch,
  conflicts, reviewer verdicts, check summary, description), checks (failed first),
  pushes, conversation and code threads (open first, placed onto the current head even
  across renames and force pushes). Selecting an entry shows it as cards with highlighted
  code and rendered Markdown comments. Panels narrower than 640 px stack entries vertically.

### Base branch and uncommitted changes

Review compares against the project's reference branch configured on **Settings >
Project** (palette: **Project settings**). Without one, scriptschnell-term detects it
automatically: git's `origin/HEAD`, then the first of `origin/main`, `origin/master`,
`main`, `master` and `trunk` that exists; Mercurial uses `default`; jj uses `trunk()`.

Press `B` (or **Review changes against a branch** in the palette) to compare against
another branch or revision. Press `W` to include uncommitted changes in the review. Diffs
over 32 MiB show their first 32 MiB.

### Forge tokens and accounts

To show change requests and CI status in the rail and in the change request view, sign in
on **Settings > Accounts** (`Ctrl+,`) for the forge extensions (`github`, `gitlab`,
`bitbucket`, `forgejo`, `gitea`, `azure-devops`), or set `GITLAB_TOKEN`,
`AZURE_DEVOPS_EXT_PAT` (or `AZURE_DEVOPS_PAT`) or `FORGEJO_TOKEN` (or `CODEBERG_TOKEN`).

### Explore panel keys

| Keys | Action |
| --- | --- |
| `V` | Switch between History, Review and Change request modes |
| `B` | Pick another base branch or revision |
| `W` | Toggle uncommitted changes in Review mode |
| `Up` / `Down`, `J` / `K` | Move through commits, files or change request entries |
| `Tab` or `Enter` | Switch focus to the diff |
| `N` / `P` or `]` / `[` | Jump to the next or previous file (or change request entry) |
| `R` or `F5` | Refresh VCS data or re-query the forge |

While scrolling a review diff, the file list tracks the file currently at the top.

## Debug browser

Open with **Debug browser** in the command palette (or bind the `debug_browser` action),
or use **Open URL in debug browser** to load a specific URL.

scriptschnell-term manages one Chrome, Chromium or Firefox instance shared across
worktrees, with isolated profiles stored in the cache directory. It starts on demand
(headless by default) when you open a panel or when a coding agent calls a browser MCP
tool, and stops when the app quits. The panel displays the tab an agent used last and
forwards your clicks and keystrokes to it.

See [Coding agents](agents.md#debug-browser-change-request-and-extension-tools) and the Debug browser section of
[CONFIGURATION.md](../crates/cmdterm/CONFIGURATION.md#debug-browser) for MCP tools,
sandbox routing and configuration keys (`debug_browser.*`).

## Machine monitor

Open with **Monitor machine** in the command palette (or bind the `monitor` action). It
displays total and per-core CPU load, memory, swap, network traffic, filesystem usage for
the worktree, uptime, load average and task counts.

For an SSH or remote project, it monitors that remote machine. It queries Linux's `/proc`
once per second while the panel is visible (non-Linux hosts show a message instead). Use
`Up`, `Down`, `PageUp`, `PageDown` and the scroll wheel when a machine has more cores than
rows.

## Browser panels and tasks panel

- **Embedded browser panels**: available when built with `--features browser` (embeds
  Servo). Open URLs from the command palette or by clicking change requests in the rail.
- **Tasks panel**: open with the `tasks` action or the command palette to inspect every
  worktree's panels (shown, hidden, frozen or exited), extension tasks and app threads.
