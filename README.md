# scriptschnell-term

scriptschnell-term is a GPU-rendered workspace of terminal and browser panels, organized by
version-control project and worktree. The rail on the left lists projects (Git or
Mercurial repositories, local or over SSH, and Jujutsu ones once you turn `jj` on) and
the worktrees of the selected one.
Switching worktrees brings back that worktree's panels, which keep running in the
background. GitLab, Azure DevOps and Forgejo remotes show open change requests and the
latest CI status next to their branches. The rail lists the requests (click one to
open it in a browser panel) when a token is set in `GITLAB_TOKEN`,
`AZURE_DEVOPS_EXT_PAT` or `FORGEJO_TOKEN`. Extensions are written in Lua.

Every worktree gets a lane color from a fixed palette, by its position in the
project's worktree list. The color marks the worktree's row in the rail, the active
tab's underline and the focused pane's border. The status bar under the panel shows
the environment, branch, working directory and grid size of the focused terminal, and
the newest notice for eight seconds.

It is built on [SchnellUI](../schnellui) (GPU UI), cmdterm-vt (its terminal emulator), Servo
(optional browser panels) and [Oximond](../oximond) (Lua). `SOUL.md` explains the
design rules; `AGENTS.md` has the commands and conventions.

## Run it

```sh
cargo run --release -p cmdterm -- ~/src/myrepo devbox:/srv/service
cargo run --release -p cmdterm --features browser    # with Servo browser panels
```

The command palette (Ctrl+Shift+P, or the `+` in the rail) is how you open projects and
URLs, create worktrees, switch to any worktree, split, close and run extension
commands. Removing a worktree and forgetting a project live there too. Type to filter,
Up and Down to move, Enter to run, Escape to go back or close.

The field at the top of the sidebar searches every project's worktrees by what happened
there: the titles agents gave their sessions, branches, change requests, folders. It
understands stems, typos and coding synonyms, so "auth bug" finds a worktree whose agent
titled its session "Fix the broken sign-in form". A worktree is named after its last agent
session, with the branch under it; "Name worktrees by branch" in the palette turns that off.

Locations are a local path, `host:/path`, or `ssh://[user@]host[:port]/path`. SSH uses
your `~/.ssh/config` and shares one connection per host. Set `SCRIPTSCHNELL_TERM_SSH_PROGRAM`
to run another executable than `ssh`, such as a wrapper script around `tsh ssh`.
`cmdterm://host[:port]/path[?vm=name]` opens a project on another machine that runs
`cmdterm-remote serve` or `cmdterm --serve`, and `vm://name/path` opens one inside a
Firecracker VM on this machine. `crates/cmdterm-remote/README.md` covers keys and VMs.
Extensions with the `environments` permission add their own schemes for other remote
coding environments, such as `k8s://pod/path` and `coder://workspace/path` from the
shipped `kubernetes` and `coder` extensions.

A repository URL (`https://`, `git://`, `file://`, `git@host:org/repo.git`, or an
`ssh://` URL with user `git` or `hg` or a path ending in `.git`) clones instead. The
palette asks for the folder (default `~`) and where the clone goes: this machine, a VM
on it, a remote machine you trust, or a VM on one. This machine and remote machines
also offer to run the terminals in the checkout's devcontainer. git is tried first,
then Mercurial. With the `jj` setting on, a new git clone also becomes a colocated jj
repository. A folder that already holds a checkout of that name is opened as is.

The settings have a page for each. Remote machines copies your key, turns serving this
machine on or off (it stays on across restarts), lists the keys allowed in and the
machines you trust, and adds a machine after showing its key fingerprint to compare.
Virtual machines sets up ready-made Alpine and Ubuntu VMs with one button (no root,
about half a minute, with internet through pasta), lists your VMs, and resets or stops
them. Advanced settings there edit `server.json` for images you made yourself. Sandboxing picks where the current worktree's terminals run, the
devcontainer engine, and the sandbox's network. Its Files and Hosts subsections edit the sandbox's paths and allowed hosts.

A worktree's terminals can run somewhere other than its git: in the worktree's
devcontainer, or in a sandbox that may write only the worktree and cannot read your
keys. Coding agents (Claude Code, Codex, opencode, pi, Copilot CLI, Qwen Code, Gemini
CLI, Antigravity CLI, Kimi Code, Mistral Vibe) may also write their login,
sessions and state in a sandbox, but not their settings, hooks, plugins and skills. Linux uses bubblewrap and macOS uses `sandbox-exec`, where `/tmp` stays shared. On
Windows the `cmdterm-sandbox` launcher, shipped next to `scriptschnell-term.exe`, runs terminals with a
write-restricted token: they can write only the worktree and the writable paths, but they
can still read your keys and reach the network. Each project has its own sandbox identity and temp
dir, so one project's sandbox cannot write another's files. Agent settings and hooks are
read-only there too. The launcher grants write access to the paths once and leaves the
grant in place, even after you remove a path from the settings. With the experimental
AppContainer, hidden paths inside the toolchain directories it may read stay hidden. Pick one in the palette ("Run terminals in the devcontainer", "... in a
sandbox", "... on the host"). The status bar shows the choice and the worktree keeps it.
A sandbox's network is on, off, or limited to allowed hosts (`restrict_network` and
`allowed_hosts`, such as `crates.io` or `*.github.com`). The default limits it, to forges,
search engines, search APIs such as Exa, AI model APIs, documentation sites and the Rust,
npm, PyPI and Go registries. The first start asks which of these groups to allow, and the
Sandboxing > Hosts page asks again from "Choose common hosts". Limited sandboxes on Linux and macOS
have no network of their own. They reach the allowed hosts through a filtering HTTP proxy
in scriptschnell-term that `HTTP_PROXY` and `HTTPS_PROXY` point to, and other hosts get `403`. On
Linux, `cmdterm-proxy-bridge` (built next to `scriptschnell-term`) carries the connections from the
sandbox to the proxy. Without one next to it, `scriptschnell-term` runs itself as the bridge. A remote project's sandbox cannot reach the proxy and gets no network.
On Linux a local sandbox with the whole network has a loopback of its own too, unless `host_network` is on:
servers in it bind no ports on your machine, only HTTP and HTTPS leave (through the same proxy, which then
allows every host when `restrict_network` is off), and the app forwards listening ports between the
terminals of a worktree and, when you expose one, to your machine (palette: "Expose port N", or
`control expose N`).
With `intercept_tls` ("Inspect HTTPS", on by default) the proxy also decrypts HTTPS. It creates two CAs per
run that exist only in memory. Hosts whose certificates verify against this machine's roots
get a certificate from the first CA, which sandboxes trust through `SSL_CERT_FILE` and the
matching variables of curl, git, cargo, Python and Node. Hosts with invalid certificates get
one from the second CA, which nothing trusts, so clients still reject them unless told
otherwise (`curl -k`).
Sandboxes use this machine's container engines directly by default (`engines`,
"Container engines" on the Sandboxing page): docker, podman, containerd and buildkit sockets,
WSL interop inside WSL, and relayed engine pipes on Windows. Those engines run outside the
sandbox, so the containers they start are not limited by it. Turn it off to hide them, or
pick "Sidecar per worktree": then each sandboxed worktree gets podman in podman (or docker in
docker) of its own, which sees the worktree and the host's registry certificates but no other
files. Sidecars need Linux and stop when the app quits.
The sandbox's extra writable and hidden paths are `sandbox` in `settings.json`, and
`container_engine` picks docker or podman; Sandboxing > Files and the Sandboxing page edit them. `crates/cmdterm/src/container/README.md` lists
what devcontainer.json support covers.

Every start writes the extensions in `extensions/` into
`~/.config/scriptschnell-term/extensions/`, together with the extension reference
(`README.md`), type files for lua-language-server and Teal (`types/term.lua`,
`types/term.d.tl`, wired up by `.luarc.json` and `tlconfig.lua`) and `CONFIGURATION.md`
next to `settings.json`. Later versions update these files unless you changed or deleted
them. Each shipped extension loads once you grant its permissions on the Extensions
page: `claude-code` copies your Claude Code setup into devcontainers, VMs and remote
machines and installs Claude Code there, `dotfiles` copies chosen dotfiles, and `npm`,
`cargo`, `nuget`, `pypi` and `git-credentials` hand registry logins to sandboxes as fakes,
and `forge-clis` does the same for the logins of `fj`, `tea`, `glab` and `gh`.
`toolchains` lets sandboxes install toolchains and global tools with rustup, nvm, npx,
`dotnet tool` and similar managers.
`aspnet-devcerts`, off until you turn it on, gives devcontainers this machine's ASP.NET
Core HTTPS dev certificate, so Kestrel inside serves the certificate your browser here
trusts and the container trusts it too, on Windows, macOS and Linux hosts alike.
`blocklist` makes the sandbox proxy refuse ad, tracker and malware hosts from HaGeZi's
DNS blocklist, downloaded from GitHub or its Codeberg mirror and cached for a week.

The Agents page of the settings hooks Claude Code, Codex, opencode, pi, Copilot CLI, Qwen
Code, Gemini CLI and Kimi Code into the rail's attention marks and installs the `configure-scriptschnell-term` skill for them. The skill
points the agent at `CONFIGURATION.md`, the extension reference and each extension's
`README.md`. For a project on another machine the install copies those files there too.
Sandboxes need no copy: they see this machine's skill and references read-only, although
the rest of the configuration directory stays hidden. Mistral Vibe, Goose and Antigravity
CLI report their state themselves through OSC 7501, so the rail marks them without hooks;
for Mistral Vibe and Antigravity CLI the Agents page installs only the skills and the
debug browser's server.

Every terminal gets `SCRIPTSCHNELL_TERM=1`, and Claude Code, Codex, pi, Copilot CLI, Qwen
Code and Gemini CLI tell the agent at session start that it runs inside the app. A second skill,
`scriptschnell-term-sandbox`, explains the sandbox to them. Terminals also get a token for
the app's control socket, which `~/.local/share/cmdterm/control` talks to. Through it an
agent reads and uploads extensions, looks up what its sandbox lets through and which
hosts the proxy refused, suggests a settings change such as a missing allowed host, or
asks for a new worktree with agents started in it. Each request waits in a dialog until
you allow it, so a sandboxed agent can fix its own setup without taking control from
you. Terminals on SSH hosts, remote machines, VMs and this machine's devcontainers reach
the same socket, so their changes land in the app you run, not on the remote machine.
The same socket lets the agents of all terminals find each other: `control who` lists
which agent works in which worktree, whether it is busy, and the note it left; `control
send` messages one agent, a worktree, a project or all of them, and the hooks hand the
messages to the agent; `control run --mutex cargo --max-ram 8GiB -- cargo test` makes
agents take turns on heavy jobs and caps their memory and CPUs.
`CONFIGURATION.md` describes the socket and its API.

Each tab owns its own split panes. Splitting starts a new shell beside the focused
one in the same tab, and a new terminal opens a new tab. Closing a pane stops its
shell; closing a tab stops every shell in it. A tab with splits is named after its
focused pane, with `+N` for the others.

These are the default keys (the `cmdterm` keymap):

| Keys | Action |
| --- | --- |
| Ctrl+Shift+P | Command palette |
| Ctrl+, | Settings |
| Ctrl+Shift+S | Search the worktrees in the sidebar |
| F6 | Move focus from the panel to the tab strip, where Tab reaches the rest of the chrome. Press it again for the status bar, then the sidebar's search field, then the panel. Escape goes back too |
| Shift+F6 | The same stops the other way round |
| Ctrl+Shift+T | New shell in a new tab |
| Ctrl+Shift+F | Browse the worktree's files in a new tab |
| Ctrl+Shift+H | The worktree's history in a new tab |
| Ctrl+Shift+R | Review the worktree's changes against its base branch |
| Ctrl+Shift+W | Close the tab |
| Ctrl+Shift+D | Split the focused pane side by side |
| Ctrl+Shift+E | Split the focused pane stacked |
| Ctrl+Shift+X | Close the focused pane |
| Ctrl+Shift+M | Move the focused pane to a new tab |
| Ctrl+Shift+O | Move focus to the next pane |
| Ctrl+Shift+I | Move focus to the previous pane |
| Ctrl+PageDown / PageUp | Next / previous tab |
| Ctrl+Shift+C / V | Copy the selection / paste, in terminals, file and history panels and the debug browser. Without a selection Ctrl+Shift+C does nothing; it never reaches the program as Ctrl+C |
| Drag a tab onto a pane edge | Its panes join the shown tab as a split on that side |
| Drag a tab along the strip | Reorder tabs |
| Drag a pane by its title onto another pane's edge | Move it to that side of the other pane |
| Drag a pane by its title onto another pane's middle | Swap the two panes |
| Drag a pane by its title onto the + after the tabs | Move it into a tab of its own |
| Drag a pane by its title onto another tab | Move it into that tab, beside its focused pane |
| Shift+PageUp / PageDown, wheel | Scroll back |
| Double-click, triple-click in a terminal | Select a word or a whole line, with the rows it wrapped onto. Dragging after it extends by words or lines. Paths, URLs and `file.rs:12:3` count as one word |
| Shift+click in a terminal | Extend the selection to the click. In a program that grabs the mouse, Shift makes any click or drag select instead |

### Pasting images

Ctrl+Shift+V with an image and no text on the clipboard saves the image as a PNG file
where the terminal runs and pastes its path. Claude Code and other coding agents attach
a pasted image path as the image. That works on every machine, in containers and in
sandboxes. On Windows the path has forward slashes, which Claude Code reads where it
would drop backslashes.

Agents also read images from the clipboard themselves on Ctrl+V (Alt+V on Windows).
That needs a clipboard where they run, which an SSH host, a remote machine, a VM, a
container or a Linux sandbox does not have. In those terminals Ctrl+V and Alt+V first
copy the clipboard's image there, to `~/.cache/scriptschnell-term-clipboard/clipboard.png`
(the app's cache directory for a sandbox), and then reach the program. Shims for `xclip`
and `wl-paste` hand that image to programs that ask for one during the next minute and
pass every other call to the real tool. On another machine and in a container the
shims go into `~/.local/bin`, which must be on `PATH` before the real tools; a file of
that name the user put there stays. A bubblewrap sandbox gets them mounted over the
host's own `xclip` and `wl-paste`, so it needs one of them installed.

The image only leaves this machine when you press one of these keys. Pasted files older
than a day are deleted on the next paste.

### Copying from sandboxes and other machines

A program in any terminal can put text on this machine's clipboard with OSC 52, which
is how agents copy over SSH. Reading the clipboard that way is refused. Programs in a
bubblewrap sandbox usually copy through `wl-copy`, `xclip` or `xsel`, which cannot
reach the desktop from there. The same shims stand in for those tools: they send the
copied text to the terminal as OSC 52 instead. The sandbox gets them mounted over the
host's own tools, so a copy works through any of them that is installed. Only text
copies this way; an image copied with `-t image/png` goes to the real tool. On another
machine the shims in `~/.local/bin` copy the same way.

### Files

The file panel (Ctrl+Shift+F, or Browse files in the palette) shows the worktree's
files on the left and the open file on the right, with line numbers and syntax
highlighting for about 25 languages. It lists what the VCS lists: tracked files plus
untracked ones that are not ignored. It only reads; open an editor in a terminal to
change a file. The open file comes back after a restart.

In the tree, Up and Down (or J and K) move, Enter opens a folder or a file, Right
shows a file while the tree keeps focus, Left closes a folder or goes to its parent.
Tab switches between the tree and the file. In the file, arrows and J/K/H/L scroll,
PageUp, PageDown and Space page, Home and End (or G) jump. Drag to select and
Ctrl+Shift+C copies. R or F5 reads the files again. Files over 16 MiB show their
first 16 MiB.

Markdown, pictures (PNG, JPEG, GIF, WebP, BMP, ICO), SVG and PDF files open rendered.
Markdown wraps to the panel's width. LaTeX math (`$..$`, `$$..$$`, `\(..\)`, `\[..\]`
and `math` code blocks) shows as Unicode text, with fractions stacked in display math.
`mermaid` code blocks show flowcharts and sequence diagrams as box drawings; other
diagram types show their source. A picture fits the panel and never grows past its
own size; SVG drawings and PDF pages grow to fill it. In a PDF, Up and Down, PageUp
and PageDown and the wheel turn pages, Home and End jump to the first and last page.
V, or a click on the file's header, switches Markdown and SVG files between rendered
and source. Pictures and PDFs are read up to 64 MiB.

### History, review and change request

The explore panel has three modes; V switches between them. History (Ctrl+Shift+H, or
Explore history in the palette) lists the commits reachable from the checked-out
revision and shows the selected one: its message, the files it changed and its diff.
Review (Ctrl+Shift+R, or Review changes) shows what a change request from the worktree
would show: the diff from the point where the worktree left its base branch, the
changed files on the left with their added and removed line counts, the whole diff on
the right with old and new line numbers and syntax highlighting. Change request (Show
the change request in the palette, or the button beside an agent's row in the Agents
tab) shows the change request of the worktree's branch as its forge extension reports
it, with native widgets instead of the terminal grid. A bar on top switches modes,
reloads the page and opens it on the forge. The list on the left has the overview
(state, draft, target branch, conflicts, each reviewer's verdict, the checks in short
and the description), the checks with failed ones first, the pushes, the conversation,
and every code thread, open ones first, placed on the current head. The right side
shows the picked entry as cards: a thread shows the highlighted code around its line
and its comments, and comments render their Markdown. Panels narrower than 640 px drop
the list and show every entry one after the other. The page is read-only.

The base is the project's reference branch, set on the Project page of the settings
(palette: Project settings). Without one it is git's `origin/HEAD`, else the first of
`origin/main`, `origin/master`, `main`, `master` and `trunk` that exists; Mercurial uses
`default` and jj `trunk()`. B, or Review changes
against a branch in the palette, names another branch or revision. W adds the changes
not committed yet. Mode, base and that choice come back after a restart.

Keys work as in the file panel: Up and Down move through the list, Tab or Enter
switches to the diff, arrows and J/K/H/L scroll it. N and P (or ] and [) jump to the
next and previous file. While you scroll the review, the list follows the file at the
top, file by file. On the change request page Up and Down (or J/K, N/P) pick the
entry, and the wheel scrolls it. R or F5 runs the query again, or asks the forge again
on the change request page. Diffs over 32 MiB show their first 32 MiB.

### Machine monitor

Monitor machine in the palette opens a panel with the load of the machine a project
lives on: the whole CPU and each core, memory, swap, network traffic, and the file
system that holds the worktree, plus uptime, load average and task counts. For an SSH
or remote project that is the remote machine. It runs one `sh` script through the
project's environment every second, only while the panel is shown, and reads Linux's
`/proc`, so other systems show a message instead. Up, Down, PageUp, PageDown and the
wheel scroll on machines with more cores than rows. The `monitor` action binds it to
a key.

### Keymaps

The Keyboard page of the settings switches between keymaps modeled on other programs
and lists every binding in effect: `cmdterm`, `tmux` (Ctrl+B prefix, Ctrl+B Ctrl+B
sends Ctrl+B), `vscode`, `cursor`, `windows-terminal` and `konsole`. Override single
bindings in `settings.json`. A key sequence maps to an action, and `null` removes a
binding:

```json
{
  "keymap": "tmux",
  "keybindings": {
    "ctrl+b |": "split_side_by_side",
    "ctrl+b %": null,
    "ctrl+alt+g": "command:git-helper.log"
  }
}
```

Actions are `palette`, `settings`, `new_terminal`, `files`, `close_pane`, `close_panel`,
`close_tab`, `move_pane_to_tab`, `split_side_by_side`, `split_stacked`,
`focus_next_pane`, `focus_previous_pane`, `next_tab`, `previous_tab`, `tab_1` to
`tab_9`, `pane_1` to `pane_8`, `send_text:<text>` and `command:<extension command>`.
A sequence has up to three chords separated by spaces. Modifiers are `ctrl`, `shift`,
`alt` and `super`; named keys include `enter`, `tab`, `space`, `escape`, `pageup`,
`left`, `f1` to `f12`, `plus` and `minus`. Extensions bind their own commands (see
the Lua API). Your bindings win over an extension's, and an extension's over the
keymap's.

Forge tokens come from `GITLAB_TOKEN`, `AZURE_DEVOPS_EXT_PAT` (or `AZURE_DEVOPS_PAT`),
and `FORGEJO_TOKEN` (or `CODEBERG_TOKEN`). On Linux preferences
live in `~/.config/scriptschnell-term/settings.json` and your own extensions in
`~/.config/scriptschnell-term/extensions/`; state in `~/.local/state/scriptschnell-term`,
the references and shipped extensions in `~/.local/share/scriptschnell-term` and caches in
`~/.cache/scriptschnell-term`. [CONFIGURATION.md](crates/cmdterm/CONFIGURATION.md)
lists the directories of macOS and Windows. See
[crates/cmdterm/src/ext/README.md](crates/cmdterm/src/ext/README.md) for the Lua API.

## License

MIT OR Apache-2.0.
