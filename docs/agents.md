# Coding agents

scriptschnell-term integrates coding agents into the sidebar, sandboxes, resource limits
and a shared control socket so multiple agents can work across worktrees without getting
in each other's way.

## Supported agents

- **Hook-integrated agents**: Claude Code (`claude`), Codex (`codex`), opencode
  (`opencode`), pi (`pi`), Copilot CLI (`copilot`), Qwen Code (`qwen`), Gemini CLI
  (`gemini`) and Kimi Code (`kimi`).
- **OSC 7501 agents**: Mistral Vibe (`vibe`), Goose and Antigravity CLI (`agy`) report
  their own busy/idle/attention state directly to the terminal without hooks.
- **Custom agents**: extensions can register additional coding agents with `term.agent`.

## Setup on Settings > Agents

Open **Settings > Agents** (`Ctrl+,`) to install integrations:

- **Hooks and attention marks**: connects agent lifecycle events so the rail highlights
  worktrees where an agent is busy, finished, waiting for input or asking for permission.
  For projects on another machine, installing copies the hooks and references there too.
- **Skills**: installs two agent skills:
  - `configure-scriptschnell-term`, pointing agents at `CONFIGURATION.md`, the extension
    reference and each shipped extension's `README.md`.
  - `scriptschnell-term-sandbox`, explaining how the sandbox works and how to inspect
    blocked hosts or suggest settings changes.
  Sandboxes see these skills and references read-only while the rest of your config
  directory stays hidden.
- **Default agent**: pick `default_agent` for the `Ctrl+Shift+A` (**New agent tab**)
  shortcut.
- **Agent shell**: on Linux, `agents_use_bash` (on by default) starts launched agents
  with `SHELL` pointing to `bash` so agent tool commands work reliably even when your
  interactive shell is fish or nushell.

For Mistral Vibe and Antigravity CLI, **Settings > Agents** installs the skills and the
debug browser MCP server; their state marks work automatically via OSC 7501.

## Agents tab and merge checks

The rail's **Agents** tab tracks worktrees where agent sessions have run: finished agents
whose branch is not yet merged appear at the top, with working and merged ones grouped
below. When forge accounts are configured on **Settings > Accounts**, scriptschnell-term
checks whether branches are merged and adds a button next to rows with a change request
to open its [discussion panel](panels.md#history-review-and-change-request).

## Session resume, idle freezing and CPU modes

- **Automatic session resume**: terminals running hook-integrated agents record their
  session id and automatically resume that session after an app restart (and prompt an
  interrupted turn to continue).
- **Freezing idle agents**: `freeze_idle_agents` (15 minutes by default; `0` disables)
  stops idle agents in hidden terminals while preserving their screen, then resumes them
  automatically when you switch back. On Linux, memory pressure (`/proc/meminfo` and
  `/proc/pressure/memory`) freezes idle agents sooner when RAM runs low.
- **CPU modes**: `cpu_mode.local` (default `balanced`) and `cpu_mode.remote` (default
  `off`) cap total terminal CPU usage and lower priority for agents in hidden terminals,
  placing hidden terminals on efficiency cores on hybrid CPUs. Click `CPU balanced` in the
  status bar, use **Settings > Agents**, or search "CPU" in the command palette to switch
  between `off`, `balanced` and `quiet` live.

## Control socket and the `control` CLI

Every terminal sets `SCRIPTSCHNELL_TERM=1`. Where `agent_control` allows it (`local` by
default, so also in sandboxes and devcontainers; `host`, `on` or `off` on **Settings > Agents**), terminals also receive
`SCRIPTSCHNELL_TERM_SOCKET` and a per-terminal `SCRIPTSCHNELL_TERM_TOKEN`. Terminals on
SSH hosts, remote machines, VMs and local devcontainers forward back to your local app's
socket automatically.

The agent integrations install a helper CLI at `~/.local/share/cmdterm/control` (requires
`curl`):

- `control sandbox`: prints the terminal's runtime, effective allowed hosts, guards and
  recently refused hosts.
- `control allow-host pypi.org 'pip needs it'`: suggests a settings change for your
  approval.
- `control push NAME DIR`: stages and proposes a Lua extension update.
- `control worktree feature/x --prompt 'fix the test' claude`: requests a new worktree
  with one or more agents started in it (`control agents --worktree ...` launches into an
  existing worktree).
- `control expose PORT`: exposes a port from a Linux sandbox to `127.0.0.1`.

Every extension change, settings suggestion and worktree/agent launch pauses in an
approval dialog showing the exact diff or command line before anything runs.

### Multi-agent coordination

Agents across terminals and projects can discover each other, exchange messages and share
heavy build locks without approval prompts:

- `control who`: lists active agents, their worktree, machine, state, notes and held locks.
- `control send ID TEXT` (or `--worktree`, `--project`, `--all`): sends a message to one
  or more agents. Hooks deliver messages into the next turn, or type a short inbox notice
  if the recipient is idle. `--untrusted-file PATH` wraps external content in an
  `<untrusted-...>` fence.
- `control inbox` and `control wait --timeout SECONDS`: read messages without hooks.
- `control run --mutex cargo --max-ram 8GiB -- cargo test`: queues heavy commands behind a
  named per-machine lock and caps RAM, CPU cores (`--cpus`), CPU quota (`--cpu-quota`)
  and task count (`--max-tasks`). `control locks` shows who holds or waits on locks.

## Debug browser, change request and extension tools

Installing an agent integration registers `~/.local/share/cmdterm/browser-mcp` as the
`scriptschnell-term-browser` MCP server (backed by `POST /v1/mcp` on the control socket).
It exposes:

- **Debug browser tools** (`debug_browser.agents`, default `local`): Playwright-style
  browser automation (`browser_session_new`, `browser_navigate`, `browser_snapshot`,
  `browser_click`, `browser_type`, `browser_take_screenshot`, `browser_console_messages`,
  `browser_evaluate`, `browser_tabs`, and more) isolated per session and respecting
  sandbox network rules.
- **Change request tools**: `change_request` and `change_request_wait` (also available as
  `control change-request [--wait]`) let agents read and wait on pull/merge request
  status, CI checks and review threads without calling forge CLIs themselves.
- **Extension tools**: tools registered by Lua extensions with `term.tool`, such as the
  shipped `knowledge` extension's Markdown document tools.

## Repairing a broken configuration

If a setting or extension stops scriptschnell-term from starting properly, run:

```sh
scriptschnell-term fix
scriptschnell-term fix --agent codex
```

`fix` launches your preferred coding agent unsandboxed in the configuration directory with
a pre-typed prompt referencing `settings.json`, `state.json`, your extensions, logs and
`CONFIGURATION.md`. Add what went wrong and press `Enter`.

For the full control socket HTTP API and every agent-related setting, see
[CONFIGURATION.md](../crates/cmdterm/CONFIGURATION.md#control-socket).
