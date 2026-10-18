# Terminal coding agents to support next

Research date: 2026-10-22. Popularity numbers are point-in-time readings from the GitHub
API, the npm downloads API (week 2026-10-12 to 2026-10-18), pypistats.org and the Homebrew
analytics API (30 days). Every agent fact links to the project's own docs, repo or package
page. Anything marked "unverified" had no primary source I could find.

## What "support" means in scriptschnell-term today

Four agents are supported: Claude Code, Codex, opencode and pi
(`Agent::ALL`, crates/cmdterm/src/agents.rs:800). Supporting an agent means these pieces,
all keyed on the `Agent` enum (agents.rs:754):

1. **State reporting into the terminal.** Every integration calls one shell helper,
   `~/.local/share/cmdterm/agent-state` (agents.rs:109). It writes the `cmdterm_agent`
   user variable as OSC 1337 SetUserVar, with a value such as `permission:Bash`. The
   terminal model turns it into an `Attention` state of busy, done, input or permission
   (crates/cmdterm/src/term/attention.rs:171, parsed at attention.rs:53), and the rail
   marks the worktree. The integration takes one of two forms (agents.rs:762):
   - `Integration::Hooks`: Claude Code style hook groups (`{"hooks":{Event:[{matcher,
     hooks:[{type:"command",command,timeout:5}]}]}}`) merged into a JSON file
     (`add_hooks`, agents.rs:1236). Claude Code uses `~/.claude/settings.json`
     (CLAUDE_HOOKS, agents.rs:770). Codex uses `~/.codex/hooks.json` (CODEX_HOOKS,
     agents.rs:788), and the settings page then tells the user to turn on or trust hooks
     (`follow_up`, agents.rs:1490).
   - `Integration::File`: a whole plugin file the app owns. opencode gets a JS plugin
     (agents.rs:621) and pi gets a TS extension (agents.rs:675).
2. **Session context and the agent bus.** At session start the helper's `context` mode
   prints Claude-style `hookSpecificOutput.additionalContext` that tells the agent it runs
   in scriptschnell-term. Where the control socket is reachable, it also joins the agent list
   (`hook-join`, agents.rs:200). `deliver` on prompt submit and tool use hands over
   messages from other agents. `stop` keeps the turn going while messages wait
   (`control/bus.rs`).
3. **Skills.** Two skills, `configure-scriptschnell-term` and `scriptschnell-term-sandbox`
   (SKILLS, agents.rs:741), go into each agent's own skills directory
   (`skills_dir`, agents.rs:843).
4. **Debug browser MCP.** A stdio MCP server script (`BROWSER_MCP`, agents.rs:62) is
   registered in each agent's MCP config (`mcp_config`, agents.rs:918). The entry is
   JSON for Claude Code and opencode and a marker-delimited TOML block for Codex. pi has
   no MCP.
5. **Sandbox paths.** `sandbox_writable` and `sandbox_read_only` (agents.rs:868, 881)
   tell bubblewrap which state directories the agent may write and which config files
   stay read-only, so a sandboxed agent cannot rewrite its own hooks. runtime.rs:675
   applies them. The keyring bus passes Secret Service calls into sandboxes and has a
   Codex-specific workaround (crates/cmdterm-env/src/keyring.rs:536). The default
   allowed hosts include an "ai" group (crates/cmdterm/src/state/mod.rs:654).
6. **Install and update checks.** notifications.rs maps each agent to npm packages
   (`npm_packages`, notifications.rs:483) and mise tool names (`mise_agent`,
   notifications.rs:497). It offers upgrades through `npm` or `mise upgrade`
   (notifications.rs:580, 615).
7. **Starting agents.** The control API and palette start an agent by program name
   (`parse_agents`, crates/cmdterm/src/control.rs:1465; the `control` script's agent list,
   agents.rs:415). A first prompt is passed as a positional argument
   (crates/cmdterm/src/ui/approval.rs:223).
8. **Fallback without integration.** attention.rs already reads BEL (attention.rs:144),
   OSC 9 and OSC 777 desktop notifications (worded text is classified at
   attention.rs:185) and OSC 9;4 progress. An agent that emits any of these gets coarse
   attention marks with no integration at all.
9. **Environment setup.** Only Claude Code has an extension that copies its config into
   devcontainers, VMs and remotes (`extensions/claude-code/`).

So a full integration needs hook events that map to busy, done, input and permission, a
way to inject session context, a skills directory, an MCP config file, known state
directories, and a package name for update checks.

## Recommendation

Fix one problem before adding anything. **Cursor CLI already runs the Claude Code hooks
scriptschnell-term installs.** Cursor loads `~/.claude/settings.json` hooks by default
("Third-Party Imports", on by default) and maps Claude event names
([Cursor third-party hooks](https://cursor.com/docs/reference/third-party-hooks)). A Cursor
session in a scriptschnell-term terminal therefore runs `agent-state context claude` and joins
the agent bus as `claude`. GitHub Copilot CLI does the same for project-level
`.claude/settings.json`, though not for the user file
([Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)).
The helper should check `CURSOR_VERSION`, which Cursor sets for hooks, and either report
under the right name or stay silent.

Then add agents in this order:

1. **GitHub Copilot CLI.** It has by far the largest install base of the candidates:
   1,135,549 npm downloads a week, about half of opencode's `opencode-ai` count and five
   times Gemini CLI's. It has the full event set
   scriptschnell-term needs: `permissionRequest`, `notification` with
   `permission_prompt` and `elicitation_dialog`, `agentStop` and `sessionStart` with
   `additionalContext`. With PascalCase event names it sends Claude-style snake_case
   payloads, so the helper's `field` parser works unchanged. Hooks live in drop-in files
   (`~/.copilot/hooks/*.json`), so this is an `Integration::File`. The catches: the
   format is flat, it is proprietary, a first prompt needs `-i PROMPT` rather than a
   positional argument, and command-hook output at prompt submit is dropped, so messages
   must arrive through `postToolUse`.
2. **Qwen Code.** It is the cheapest to add. Its hooks copy Claude Code's event names,
   nesting, `additionalContext`, `PermissionRequest` and `Notification` types, and it
   accepts Claude tool names as matchers. `CLAUDE_HOOKS` almost works as-is against
   `~/.qwen/settings.json`. MCP sits in the same file under `mcpServers`. It is
   Apache-2.0 and gets 45k npm downloads a week.
3. **OSC 7501 in the terminal, which covers Antigravity CLI, Goose and Mistral Vibe.**
   All three emit `ESC ] 7501 ; state=idle|working|done|error|blocked[:kind=permission|question]:app=NAME[:msg=BASE64] ESC \`.
   Antigravity CLI is Google's replacement for Gemini CLI for consumer users (8,469
   Homebrew installs in 30 days, releases almost daily). Its hooks have no prompt,
   permission or notification event, so OSC 7501 is the better signal for it anyway.
   Goose has 55.5k stars. Vibe and Antigravity probe with `ESC]7501;?ESC\` and only
   report when the terminal answers. One parser change gives all three attention marks
   with nothing installed in the user's home. The cost is in the terminal layer: the
   pinned wezterm revision drops unknown OSC codes
   (`OperatingSystemCommand::Unspecified` is only logged, term/src/terminalstate/performer.rs:776
   at rev e2a98e1), so it needs a wezterm patch or a scan in the PTY reader, plus a reply
   to the probe. Skills and MCP for Antigravity can follow later.
4. **Gemini CLI.** It has 107.5k stars and 227k npm downloads a week, and it is still
   released weekly (v0.65.0 on 2026-10-20). Its hooks are Claude-shaped and have
   `SessionStart`, `BeforeAgent`, `AfterAgent`, `SessionEnd`, `Notification` (type
   `ToolPermission`) and `additionalContext`. It is Apache-2.0. I rank it below
   Antigravity's signal because Google stopped serving consumer Gemini CLI on 2026-06-18
   and points everyone to Antigravity CLI. Paid API keys and Code Assist Standard or
   Enterprise users keep it, so its audience will shrink. Watch out: Gemini hook
   `timeout` is in milliseconds, so `add_hooks`' `"timeout": 5` would mean 5 ms.
5. **Kimi Code CLI.** It gets 51.5k npm downloads a week and is MIT. Its hooks use Claude
   event names, including `PermissionRequest` and `Notification`. It also sends OSC 9 or
   BEL natively. Hooks are `[[hooks]]` tables in `~/.kimi-code/config.toml`, so the
   integration needs a marker-delimited TOML block like Codex's MCP block. The old Kimi
   CLI (Python) is archived.

Next in line: **Factory droid**, whose hooks match Claude Code's (same events, including
`Notification` with `permission_prompt`) in `~/.factory/hooks.json`, but whose adoption
is modest (16k npm downloads a week across two packages). After that **Cursor CLI** with
its own `~/.cursor/hooks.json` and **Amp** with a TS plugin like pi's. Neither has a
permission event.

Skip for now:
- **Aider.** It is effectively unmaintained (last GitHub release 2025-08-09), has no
  hooks or MCP, and only one notification command.
- **Crush.** It has only a `PreToolUse` hook. Its OSC 777 notifications fire only when
  the terminal reports focus loss.
- **Cline CLI.** It has no permission or notification hook and turns hooks off under
  `--yolo`. It gives no BEL or OSC signal.
- **Kiro CLI.** It has no permission or notification hook and is closed source. Its
  `osc9` notification method already reaches attention.rs if the user turns it on.
- **Kilo Code CLI.** It is an opencode fork, but its plugin events (`session.idle`,
  `permission.asked`) follow the older opencode plugin API, not the
  `session.execution.*` events in `OPENCODE_PLUGIN`, so it would need its own plugin.
- **Auggie.** It is proprietary, has no prompt-submit or permission hook and only rings
  a bell.

### Cross-cutting changes worth making first

- **Install the skills once into `~/.agents/skills/`.** Gemini CLI, Qwen Code, Copilot
  CLI, Cursor, Amp, Factory droid, Kilo, Mistral Vibe, Kimi Code, Auggie, Goose, Crush and
  Cline all read user skills from there (sources in each section below). Kiro and
  Antigravity CLI do not. Many also read `~/.claude/skills/` (Cursor, Amp, Crush, Goose,
  Kilo, Auggie), so they already see the skills the Claude Code integration installs.
  `~/.agents/skills` would then need the same read-only treatment in sandboxes that
  `skills_dir` gets (runtime.rs:679).
- **Make the hook spec per agent.** It needs the timeout unit (Gemini: ms), the context
  output shape (Antigravity: `injectSteps`, Crush: `context`) and the format: nested
  groups for Claude, Codex, Gemini, Qwen and droid, flat entries for Copilot and Cursor,
  TOML for Kimi.
- **Make the prompt argument per agent.** Copilot needs `-i PROMPT`. Gemini takes a
  positional prompt and stays interactive in a TTY
  ([Gemini CLI reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md)).
- **Fix opencode's package drift.** The opencode npm package moved. The mise registry
  points versions 2.0.0 and later at `npm:@opencode/cli` (latest 2.1.3, 273k
  downloads a week), while `opencode-ai` stops at 1.19.4
  ([mise registry opencode.toml](https://github.com/jdx/mise/blob/main/registry/opencode.toml),
  [npm @opencode/cli](https://registry.npmjs.org/@opencode/cli)). `npm_packages` only
  lists `opencode-ai` (notifications.rs:487), so 2.x users installed through npm get no
  update notice.
- **Touch points for a new `Agent` variant:** the `match` arms in agents.rs,
  `npm_packages` and `mise_agent` in notifications.rs, the `control` script's agent case
  (agents.rs:415) and error text (control.rs:1474), the sandbox paths, and the state
  directory list in `crates/cmdterm/CONFIGURATION.md` (line 145), which is shipped
  user documentation. New hosts go into the default allowed hosts, for example
  `*.githubcopilot.com` for Copilot and `cloudcode-pa.googleapis.com` for Gemini.

## Comparison

Popularity: GitHub stars from the API on 2026-10-22, npm downloads for 2026-10-12 to
2026-10-18, PyPI from pypistats, Homebrew installs over 30 days.

| Agent | Stars | Weekly downloads | License | Hooks | Permission or notification hook | Context injection | User skills dir | MCP config | Native attention signal |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Copilot CLI | 11.3k (no source) | npm 1,135,549 | Proprietary | `~/.copilot/hooks/*.json`, flat | `permissionRequest`, `notification` | sessionStart, postToolUse | `~/.copilot/skills`, `~/.agents/skills` | `~/.copilot/mcp-config.json` | beep, OSC 9;4, opt-in OSC 777 |
| Gemini CLI | 107.5k | npm 227,214 | Apache-2.0 | `~/.gemini/settings.json` | `Notification` (ToolPermission) | SessionStart, BeforeAgent, AfterTool | `~/.gemini/skills`, `~/.agents/skills` | `~/.gemini/settings.json` | opt-in OSC 9/777/BEL, title |
| Antigravity CLI | 2.6k (no source) | brew 8,469/30d | Proprietary | `~/.gemini/config/hooks.json` | none | `injectSteps` | `~/.gemini/antigravity-cli/skills` | `~/.gemini/config/mcp_config.json` | OSC 7501, opt-in bell |
| Qwen Code | 28.7k | npm 45,040 | Apache-2.0 | `~/.qwen/settings.json` | `PermissionRequest`, `Notification` | yes, Claude shape | `~/.qwen/skills`, `~/.agents/skills` | `~/.qwen/settings.json` | opt-in BEL/OSC 9/777 |
| Kimi Code CLI | about 8k | npm 51,541 | MIT | `~/.kimi-code/config.toml` | `PermissionRequest`, `Notification` | UserPromptSubmit | `~/.kimi-code/skills`, `~/.agents/skills` | `~/.kimi-code/mcp.json` | OSC 9 or BEL, OSC 9;4 |
| Factory droid | n/a | npm 10,713 + 5,264 | Proprietary | `~/.factory/hooks.json` | `Notification` | SessionStart, UserPromptSubmit, PostToolUse | `~/.factory/skills`, `~/.agents/skills` | `~/.factory/mcp.json` | sounds or bell |
| Cursor CLI | n/a | brew 1,513/30d | Proprietary | `~/.cursor/hooks.json`, flat; also reads Claude's | none | sessionStart, postToolUse | `~/.cursor/skills`, `~/.agents/skills` | `~/.cursor/mcp.json` | OSC 9/777/99 only for known terminals |
| Amp | n/a | npm 16,214 + 14,892 | Proprietary | TS plugins, `~/.config/amp/plugins/` | none | `agent.start` | `~/.config/amp/skills`, `~/.agents/skills` | `~/.config/amp/settings.json` | sounds, BEL, OSC 777 |
| Goose | 55.5k | brew 2,756/30d | Apache-2.0 | plugin `hooks/hooks.json` | none | none | `~/.agents/skills` | `~/.config/goose/config.yaml` | OSC 7501, opt-in BEL |
| Mistral Vibe | 5.2k | PyPI 366,153 | Apache-2.0 | `~/.vibe/hooks.toml`, 3 events | none | post_tool | `~/.vibe/skills`, `~/.agents/skills` | `~/.vibe/config.toml` | OSC 7501 or BEL |
| Cline CLI | 70.7k (mostly VS Code) | npm 42,575 | Apache-2.0 | one file per event, `~/.cline/hooks/` | none | Pre/PostToolUse | `~/.cline/skills`, `~/.agents/skills` | `~/.cline/data/settings/cline_mcp_settings.json` | title only |
| Kilo Code CLI | 27.7k | npm 16,095 | MIT | opencode-style JS plugins | `permission.asked` event | experimental transforms | `~/.kilo/skills`, `~/.agents/skills` | `~/.config/kilo/kilo.json` | opt-in notifications |
| Crush | 28.8k | npm 3,655 | FSL-1.1-MIT | `PreToolUse` only | none | PreToolUse `context` | `~/.config/crush/skills`, `~/.agents/skills` | crushrc or `crush.json` | OSC 99/777 or BEL when unfocused |
| Kiro CLI | 4.4k (no source) | brew 1,712/30d | Proprietary | `~/.kiro/hooks/*.json` | none | SessionStart, UserPromptSubmit | `~/.kiro/skills` | `~/.kiro/settings/mcp.json` | opt-in BEL or OSC 9 |
| Auggie | 0.3k (no source) | npm 16,297 | Proprietary | `~/.augment/settings.json` | none | SessionStart, PostToolUse | `~/.augment/skills`, `~/.agents/skills` | `~/.augment/settings.json` | BEL, title |
| Aider | 49.7k | PyPI 68,221 | Apache-2.0 | none (`--notifications-command`) | none | none | none | none | opt-in BEL |

For scale, the supported agents: `@openai/codex` 13,153,561, `@anthropic-ai/claude-code`
6,419,887, `@earendil-works/pi-coding-agent` 2,715,594 and `opencode-ai` 2,123,309 npm
downloads in the same week.

Mistral Vibe's PyPI number looks inflated next to its 5.2k stars and 751 Homebrew installs
in 30 days. The 3.9M monthly figure points to CI or mirror traffic. I did not verify the
cause.

## Per-agent findings

Abbreviations: `GD` = https://docs.github.com/en/copilot, `GH` =
https://github.com/google-gemini/gemini-cli/blob/main, `QW` =
https://github.com/QwenLM/qwen-code/blob/main.

### GitHub Copilot CLI

- Popularity: npm `@github/copilot` 1,135,549 a week
  ([npm API](https://api.npmjs.org/downloads/point/last-week/@github/copilot)). Homebrew
  cask `copilot-cli` has 17,772 installs in 30 days
  ([brew](https://formulae.brew.sh/api/cask/copilot-cli.json)). The repo has 11.3k stars
  and holds no source. The latest version is 1.0.103 (2026-10-22), with releases almost
  daily ([changelog](https://github.com/github/copilot-cli/blob/main/changelog.md)).
- License: the proprietary "GitHub Copilot CLI License"
  ([LICENSE.md](https://github.com/github/copilot-cli/blob/main/LICENSE.md)).
- Install: `curl -fsSL https://gh.io/copilot-install | bash`, `brew install copilot-cli`,
  WinGet `GitHub.Copilot` or `npm i -g @github/copilot`
  ([README](https://github.com/github/copilot-cli/blob/main/README.md)). mise: `copilot`
  (alias `copilot-cli`), backends aqua, github and npm
  ([registry](https://github.com/jdx/mise/blob/main/registry/copilot.toml)). Updates:
  `copilot update`; `autoUpdate` defaults to true
  ([command reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference),
  [config dir reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference)).
- Hooks ([hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)):
  - Locations: `~/.copilot/hooks/*.json` (`$COPILOT_HOME/hooks`), repo
    `.github/hooks/*.json`, an inline `hooks` key in `~/.copilot/settings.json`, and
    project `.claude/settings.json` and `.claude/settings.local.json`.
  - Format: `{"version":1,"hooks":{"preToolUse":[{"type":"command","bash":"...","timeoutSec":30,"matcher":"..."}]}}`.
    Entries are flat, with no nested group.
  - Events: sessionStart, sessionEnd, userPromptSubmitted, preToolUse, postToolUse,
    postToolUseFailure, permissionRequest, notification, agentStop, subagentStart,
    subagentStop, errorOccurred, preCompact. Notification types are
    `permission_prompt`, `elicitation_dialog`, `agent_idle` and `agent_completed`, among
    others.
  - PascalCase names (`SessionStart`, `PreToolUse`, `Stop`, `PermissionRequest`,
    `Notification`) get Claude-style snake_case payloads (`session_id`, `tool_name`).
  - `additionalContext` works on sessionStart, postToolUse and notification. Output from
    command hooks on userPromptSubmitted is dropped.
- Instructions: `~/.copilot/copilot-instructions.md`, `.github/copilot-instructions.md`,
  `AGENTS.md`, `CLAUDE.md` and `GEMINI.md`
  ([custom instructions](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-custom-instructions)).
  Skills: `~/.copilot/skills/` or `~/.agents/skills/`
  ([skills](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-skills)).
- MCP: `~/.copilot/mcp-config.json`, key `mcpServers`, stdio entry
  `{"type":"local","command":...,"args":[...],"env":{},"tools":["*"]}`
  ([MCP](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers)).
- Auth: the token goes to the system credential store. Without one it goes in plain
  text to `~/.copilot/config.json`, and only when `storeTokenPlaintext` is set. Env vars
  are `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` and `GITHUB_TOKEN`. State lives in `~/.copilot`
  (`session-state/`, `logs/`, `mcp-secrets/`) and `~/.cache/copilot` (command and config
  dir references above).
- Hosts: `api.github.com/copilot_internal/*`, `*.githubcopilot.com`,
  `copilot-proxy.githubusercontent.com`, `copilot-telemetry.githubusercontent.com`,
  `collector.github.com` and `github.com/login/*`
  ([Copilot allowlist](https://docs.github.com/en/copilot/reference/copilot-allowlist-reference)).
  This list covers Copilot in general; a CLI-only list is unverified.
- Signals: `beep` (default true), `terminalProgress` OSC 9;4 (default true),
  `terminalNotifications` with OSC 777 on Ghostty and WezTerm (default false) and
  `updateTerminalTitle` (config dir reference above). Which setting turns on desktop
  notifications is unverified.
- Start with a prompt: `copilot -i PROMPT` (command reference above).

### Qwen Code

- Popularity: 28.7k stars, npm `@qwen-code/qwen-code` 45,040 a week, Homebrew 4,337 in
  30 days ([brew](https://formulae.brew.sh/api/formula/qwen-code.json)). The latest
  version is v0.26.0 (2026-10-21)
  ([release](https://github.com/QwenLM/qwen-code/releases/tag/v0.26.0)).
- License: Apache-2.0 ([LICENSE](https://github.com/QwenLM/qwen-code/blob/main/LICENSE)).
- Install: standalone curl script, `npm i -g @qwen-code/qwen-code` or
  `brew install qwen-code` ([README](https://github.com/QwenLM/qwen-code/blob/main/README.md)).
  mise: `qwen` (alias `qwen-code`) on `npm:@qwen-code/qwen-code`
  ([registry](https://github.com/jdx/mise/blob/main/registry/qwen.toml)).
  `general.enableAutoUpdate` controls update checks
  ([settings](https://github.com/QwenLM/qwen-code/blob/main/docs/users/configuration/settings.md)).
- Hooks ([hooks doc](https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/hooks.md)):
  - Location: `hooks` in `~/.qwen/settings.json` or `.qwen/settings.json`. Project hooks
    load only in trusted folders.
  - Format: the Claude Code nesting. `timeout` is in seconds.
  - Events: PreToolUse, PostToolUse, UserPromptSubmit, SessionStart, SessionEnd, Stop,
    Notification (`permission_prompt`, `idle_prompt`), PermissionRequest and more.
    Claude tool names work as matchers.
  - Output: `hookSpecificOutput.additionalContext` works on SessionStart,
    UserPromptSubmit and Stop. `CLAUDE_PROJECT_DIR` is set for hooks.
- Instructions: `QWEN.md` (`~/.qwen/QWEN.md`) and `AGENTS.md`
  ([memory](https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/memory.md)).
  Skills: `~/.qwen/skills/` and `~/.agents/skills/`
  ([skills](https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/skills.md)).
- MCP: `mcpServers` in `~/.qwen/settings.json`, Gemini schema (`command`, `args`, `env`,
  `cwd`, `trust`) ([MCP](https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/mcp.md)).
- Auth: `~/.qwen` (`QWEN_HOME`). Qwen OAuth tokens live in `~/.qwen/oauth_creds.json`
  ([qwenOAuth2.ts](https://github.com/QwenLM/qwen-code/blob/main/packages/core/src/qwen/qwenOAuth2.ts)).
  The free OAuth tier ended on 2026-04-15. API keys come from env vars or
  `~/.qwen/.env` ([auth](https://github.com/QwenLM/qwen-code/blob/main/docs/users/configuration/auth.md)).
  Keyring use: unverified.
- Hosts: `dashscope.aliyuncs.com`, `coding.dashscope.aliyuncs.com`,
  `coding-intl.dashscope.aliyuncs.com`, `maas.qwencloudapi.com` (auth doc above),
  `chat.qwen.ai` for OAuth, and telemetry to an Aliyun RUM host
  ([qwen-logger.ts](https://github.com/QwenLM/qwen-code/blob/main/packages/core/src/telemetry/qwen-logger/qwen-logger.ts)).
- Signals: `general.terminalBell` picks OSC 9 on iTerm2, OSC 99 on kitty, OSC 777 on
  Ghostty and BEL elsewhere
  ([notificationService.ts](https://github.com/QwenLM/qwen-code/blob/main/packages/cli/src/services/notificationService.ts)).
  `ui.showStatusInTitle` adds a status prefix to the title.

### Antigravity CLI (`agy`)

- Status: Google moved consumer Gemini CLI users to Antigravity CLI. Gemini CLI stopped
  serving Google AI Pro, Ultra and free Code Assist users on 2026-06-18. It stays
  available through paid API keys and Code Assist Standard or Enterprise
  ([Google blog, 2026-05-19](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli),
  [Gemini CLI README](https://github.com/google-gemini/gemini-cli/blob/main/README.md)).
- Popularity: the repo has 2,615 stars and holds only README, CHANGELOG and examples, no
  source. Homebrew cask `antigravity-cli` has 8,469 installs in 30 days
  ([brew](https://formulae.brew.sh/api/cask/antigravity-cli.json)). The latest version is
  1.4.5 (2026-10-22) ([release](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.4.5)).
- License: no open source license; use falls under Google's terms
  ([terms](https://antigravity.google/terms)). It is written in Go (blog above).
- Install: `curl -fsSL https://antigravity.google/cli/install.sh | bash`, or the Homebrew
  cask ([README](https://github.com/google-antigravity/antigravity-cli),
  [install](https://antigravity.google/docs/install/)). There is no official npm package.
  mise: `antigravity-cli` (alias `agy`) on aqua
  ([registry](https://github.com/jdx/mise/blob/main/registry/antigravity-cli.toml)). It
  updates itself in the background; `AGY_CLI_DISABLE_AUTO_UPDATE=true` turns that off
  ([troubleshooting](https://antigravity.google/docs/cli/troubleshooting/)).
- Hooks ([hooks](https://antigravity.google/docs/hooks/)):
  - Location: global `~/.gemini/config/hooks.json`, or inside
    `~/.gemini/antigravity-cli/settings.json`. Workspace hooks go in `.agents/hooks.json`.
  - Events: SessionStart, PreToolUse, PostToolUse, PreInvocation, PostInvocation, Stop.
    There is no prompt-submit, permission, notification or session-end event.
  - Payload and output: stdin is camelCase (`conversationId`, `toolCall{name,args}`).
    Context goes back as `injectSteps`, not `additionalContext`.
  - Whether it reads Gemini CLI hooks from `~/.gemini/settings.json` is unverified, and
    the migration guide does not say so
    ([migration](https://antigravity.google/docs/cli/gcli-migration/)).
- Instructions: `AGENTS.md` or `GEMINI.md`, globally `~/.gemini/AGENTS.md`
  ([rules](https://antigravity.google/docs/rules/)). Skills: user skills in
  `~/.gemini/antigravity-cli/skills/<name>/`, workspace skills in `.agents/skills/`
  ([skills](https://antigravity.google/docs/skills/)).
- MCP: `~/.gemini/config/mcp_config.json`, key `mcpServers`, stdio entry
  `{"command","args","env","cwd"}` ([MCP](https://antigravity.google/docs/mcp/)).
- Auth: the OS keyring (Secret Service on Linux), with file storage when no D-Bus
  session bus exists ([README](https://github.com/google-antigravity/antigravity-cli),
  [CHANGELOG](https://github.com/google-antigravity/antigravity-cli/blob/main/CHANGELOG.md)
  1.1.3 and 1.1.26). The fallback file path is unverified. Data lives in
  `~/.gemini/antigravity-cli/`.
- Hosts: `businessaicode.googleapis.com` and `cloudcode-pa.googleapis.com` are required,
  plus regional `aiplatform` and `discoveryengine` hosts
  ([enterprise](https://antigravity.google/docs/enterprise/)). Updates come from
  `storage.googleapis.com` and a Cloud Run updater host (install.sh). Consumer sign-in
  hosts are unverified.
- Signals: OSC 7501 since 1.4.0. It probes first; `AGY_CLI_OSC7501=on` forces it
  (CHANGELOG 1.4.0). The `notifications` setting (default false) adds a bell
  ([settings](https://antigravity.google/docs/settings/)). OSC 9 and OSC 777 are
  unverified.

### Gemini CLI

- Popularity: 107.5k stars, npm `@google/gemini-cli` 227,214 a week, Homebrew 7,977 in 30
  days ([brew](https://formulae.brew.sh/api/formula/gemini-cli.json)). The latest version
  is v0.65.0 (2026-10-20) ([releases](https://github.com/google-gemini/gemini-cli/releases)).
  For its narrowed audience, see the Antigravity section.
- License: Apache-2.0 ([LICENSE](https://github.com/google-gemini/gemini-cli/blob/main/LICENSE)).
- Install: `npm i -g @google/gemini-cli`, `brew install gemini-cli` or MacPorts
  ([installation](https://github.com/google-gemini/gemini-cli/blob/main/docs/get-started/installation.mdx)).
  mise: `gemini-cli` (alias `gemini`)
  ([registry](https://github.com/jdx/mise/blob/main/registry/gemini-cli.toml)). Auto-update
  is on by default (`general.enableAutoUpdate`)
  ([configuration](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md)).
  I found no `gemini update` command (unverified).
- Hooks ([index](https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/index.md),
  [reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md)):
  - Location: `hooks` in `~/.gemini/settings.json` or `.gemini/settings.json`.
  - Format: the Claude-style nesting. `timeout` is in ms, default 60000.
  - Events: SessionStart, SessionEnd, BeforeAgent (prompt submit), AfterAgent (turn
    end), BeforeTool, AfterTool, BeforeModel, AfterModel, BeforeToolSelection,
    PreCompress, Notification (`notification_type: "ToolPermission"`).
  - Payload: stdin has `session_id`, `hook_event_name`, `tool_name` and `message`.
  - Output: `hookSpecificOutput.additionalContext` works on SessionStart, BeforeAgent
    and AfterTool.
  - `CLAUDE_PROJECT_DIR` is set as an alias.
- Instructions: `GEMINI.md`, user file `~/.gemini/GEMINI.md`. `AGENTS.md` loads only when
  set through `context.fileName`
  ([gemini-md](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/gemini-md.md)).
  Skills: `~/.gemini/skills/` or `~/.agents/skills/`
  ([skills](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/skills.md)).
- MCP: `mcpServers` in `~/.gemini/settings.json`, entry `{"command","args","env","cwd","timeout","trust"}`
  (configuration doc above).
- Auth: `~/.gemini/oauth_creds.json` (mode 0600) by default. With
  `GEMINI_FORCE_ENCRYPTED_FILE_STORAGE=true` it uses the keychain through keytar
  ([oauth2.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/code_assist/oauth2.ts),
  [keychainService.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/services/keychainService.ts)).
  Env vars are `GEMINI_API_KEY` and `GOOGLE_API_KEY`.
- Hosts: `cloudcode-pa.googleapis.com`
  ([server.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/code_assist/server.ts)),
  `www.googleapis.com` and `codeassist.google.com` for OAuth (oauth2.ts), and
  `play.googleapis.com/log` for usage statistics
  ([clearcut-logger.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/telemetry/clearcut-logger/clearcut-logger.ts)).
  `generativelanguage.googleapis.com` in API-key mode is unverified here; the default
  allowed hosts already include it.
- Signals: `general.enableNotifications` (default false) with
  `general.notificationMethod` set to `auto`, `osc9`, `osc777` or `bell`
  ([notifications](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/notifications.md)).
  `ui.dynamicWindowTitle` (default true) shows Ready, Action Required or Working in the
  title ([windowTitle.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/utils/windowTitle.ts)).
- Start with a prompt: `gemini "query"` stays interactive in a TTY (CLI reference linked
  above).

### Kimi Code CLI

- Status: the Python Kimi CLI (`MoonshotAI/kimi-cli`, PyPI `kimi-cli`) is archived. Its
  README says it "is no longer maintained"
  ([README](https://github.com/MoonshotAI/kimi-cli/blob/main/README.md)). Its successor is
  Kimi Code CLI (`MoonshotAI/kimi-code`, npm `@moonshot-ai/kimi-code`, binary `kimi`).
- Popularity: about 8k stars, npm 51,541 a week, Homebrew 1,001 in 30 days
  ([brew](https://formulae.brew.sh/api/formula/kimi-code.json)). The latest version is
  2.3.0 (2026-10-21) ([npm](https://registry.npmjs.org/@moonshot-ai/kimi-code)). License:
  MIT ([LICENSE](https://github.com/MoonshotAI/kimi-code/blob/main/LICENSE)).
- Install: curl script, `npm install -g @moonshot-ai/kimi-code` or
  `brew install kimi-code`. Update with `kimi upgrade`
  ([getting started](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/guides/getting-started.md)).
  It has no mise entry.
- Hooks: `[[hooks]]` in `~/.kimi-code/config.toml` with `event`, `matcher`, `command` and
  `timeout`. Events include UserPromptSubmit, PreToolUse, PostToolUse, PermissionRequest,
  Stop, SessionStart, SessionEnd and Notification. Stdin carries `hook_event_name` and
  `session_id`. Only UserPromptSubmit stdout is documented as reaching the context
  ([hooks](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/customization/hooks.md)).
- Instructions: `AGENTS.md`, `~/.kimi-code/AGENTS.md`
  ([agents](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/customization/agents.md)).
  Skills: `~/.kimi-code/skills/` and `~/.agents/skills/`
  ([skills](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/customization/skills.md)).
- MCP: `~/.kimi-code/mcp.json`, key `mcpServers`
  ([MCP](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/customization/mcp.md)).
- Auth: `~/.kimi-code/credentials/<name>.json` (0600), root overridable with
  `KIMI_CODE_HOME`
  ([data locations](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/configuration/data-locations.md)).
- Hosts, read from source and not from a documented list: `auth.kimi.com`,
  `api.kimi.com`, `api.moonshot.ai`, `code.kimi.com`, `telemetry-logs.kimi.com`
  ([constants.ts](https://github.com/MoonshotAI/kimi-code/blob/main/packages/oauth/src/constants.ts)).
- Signals: OSC 9 on iTerm2, WezTerm, Ghostty, Warp and kitty, BEL elsewhere, plus OSC 9;4
  progress and an OSC 0 title
  ([terminal-notification.ts](https://github.com/MoonshotAI/kimi-code/blob/main/apps/kimi-code/src/tui/utils/terminal-notification.ts)).

### Factory droid

- Popularity: npm `@factory/cli` 10,713 and `droid` 5,264 a week, Homebrew cask 189 in 30
  days ([brew](https://formulae.brew.sh/api/cask/droid.json)). The latest version is
  v0.247.0 (2026-10-22)
  ([release notes](https://docs.factory.com/changelog/release-notes)). License:
  proprietary, "UNLICENSED" on npm ([npm](https://registry.npmjs.org/@factory/cli)).
- Install: `curl -fsSL https://app.factory.ai/cli | sh`, `brew install --cask droid` or
  `npm install -g droid` ([quickstart](https://docs.factory.com/droid-cli/quickstart)).
  `droid update --check` checks for updates
  ([CLI reference](https://docs.factory.com/droid-cli/cli-reference)). No mise entry.
- Hooks ([hooks](https://docs.factory.com/harness/hooks)):
  - Location: `~/.factory/hooks.json` or `.factory/hooks.json`.
  - Format: the Claude nesting.
  - Events: PreToolUse, PostToolUse, UserPromptSubmit, Notification
    (`permission_prompt`, `idle_prompt`, `elicitation_dialog`), Stop, SubagentStop,
    PreCompact, SessionStart, SessionEnd.
  - Output: `additionalContext` works on SessionStart, UserPromptSubmit and PostToolUse.
  - Droid snapshots hooks at startup.
- Instructions: `AGENTS.md`, also `CLAUDE.md`, with personal files in `~/.factory/`
  ([AGENTS.md](https://docs.factory.com/harness/agents-md)). Skills:
  `~/.factory/skills/` and `~/.agents/skills/` ([skills](https://docs.factory.com/harness/skills)).
- MCP: `~/.factory/mcp.json`, key `mcpServers` ([MCP](https://docs.factory.com/harness/mcp)).
- Auth: `FACTORY_API_KEY` or `/login`. Token storage is not documented; strings in the
  binary point to a keyring with a file fallback (unverified). Config is
  `~/.factory/settings.json` ([settings](https://docs.factory.com/droid-cli/settings)).
- Hosts: the docs say only `*.factory.ai`
  ([network](https://docs.factory.com/enterprise/network-and-deployment)).
- Signals: `completionSound` and `awaitingInputSound` accept `bell` (settings above).

### Cursor CLI (`agent`, `cursor-agent`)

- Popularity: no npm package. Homebrew cask `cursor-cli` has 1,513 installs in 30 days
  ([brew](https://formulae.brew.sh/api/cask/cursor-cli.json)). Most installs go through
  the curl script, so this undercounts. Closed source; the exact license is unverified.
- Install: `curl https://cursor.com/install -fsS | bash`. It auto-updates, and
  `agent update` updates by hand ([installation](https://cursor.com/docs/cli/installation)).
  mise: `cursor-agent` (alias `cursor-cli`) on the http backend
  ([registry](https://github.com/jdx/mise/blob/main/registry/cursor-agent.toml)).
- Hooks ([hooks](https://cursor.com/docs/hooks)):
  - Location: `~/.cursor/hooks.json`. Cursor also loads Claude Code hooks from
    `~/.claude/settings.json`, on by default
    ([third-party hooks](https://cursor.com/docs/reference/third-party-hooks)).
  - Format: flat entries.
  - Events: sessionStart, sessionEnd, beforeSubmitPrompt, preToolUse, postToolUse, stop
    and more. There is no notification or permission event.
  - Output: `additional_context` works on sessionStart and postToolUse.
  - Which events fire in the CLI and not only the IDE is only partly documented
    ([CLI changelog](https://cursor.com/docs/cli/changelog)).
- Skills: `~/.cursor/skills/`, `~/.agents/skills/`, and `~/.claude/skills/` for
  compatibility ([skills](https://cursor.com/docs/skills)). MCP: `~/.cursor/mcp.json`,
  key `mcpServers` ([MCP](https://cursor.com/docs/mcp)).
- Auth: `agent login` or `CURSOR_API_KEY`
  ([authentication](https://cursor.com/docs/cli/reference/authentication)). On Linux the
  file store at `~/.config/cursor/auth.json` comes from reading the bundle (unverified in
  the docs). Config: `~/.cursor/cli-config.json`
  ([configuration](https://cursor.com/docs/cli/reference/configuration)).
- Hosts: `*.cursor.sh`, `*.cursor-cdn.com`, `*.cursorapi.com`, `api2.cursor.sh`,
  `downloads.cursor.com` ([network](https://cursor.com/docs/enterprise/network-configuration)).
- Signals: `notifications` (default true) and `display.showStatusIndicators` (config doc
  above). From reading the bundle, which is unverified in the docs: it sends nothing to
  terminals it does not recognize, so scriptschnell-term gets no OSC from it today.

### Amp

- Popularity: npm `@ampcode/cli` 16,214 a week. `@sourcegraph/amp` (14,892) is a
  wrapper around it ([npm](https://registry.npmjs.org/@sourcegraph/amp)). License:
  proprietary ([npm @ampcode/plugin](https://registry.npmjs.org/@ampcode/plugin)).
- Install: `curl -fsSL https://ampcode.com/install.sh | bash`. Update with `amp update`
  ([CLI](https://ampcode.com/docs/cli)). mise: `amp` on `npm:@ampcode/cli`
  ([registry](https://github.com/jdx/mise/blob/main/registry/amp.toml)). Homebrew core
  `amp` is an unrelated editor.
- Hooks: there is no hook config. TS plugins in `~/.config/amp/plugins/` call
  `amp.on(event)` for session.start, agent.start, tool.call, tool.result and agent.end.
  There is no session end or permission event. `agent.start` can append hidden context
  ([plugins](https://ampcode.com/docs/customize/plugins),
  [plugin API](https://ampcode.com/docs/plugin-api)). The integration would look like
  pi's.
- Skills: `~/.config/amp/skills/`, `~/.agents/skills/` and `~/.claude/skills/`
  ([skills](https://ampcode.com/docs/customize/skills)). MCP: `amp.mcpServers` in
  `~/.config/amp/settings.json` ([MCP](https://ampcode.com/docs/customize/mcp)).
- Auth: `AMP_API_KEY` or `amp login` (CLI doc). The file store at
  `~/.local/share/amp/secrets.json` comes from the bundle (unverified in the docs).
- Signals: `amp.notifications.enabled` plays sounds, and sends BEL over SSH or with
  `AMP_FORCE_BEL` ([settings](https://ampcode.com/docs/cli/settings)).

### Goose

- Popularity: 55.5k stars. The repo moved from `block/goose` to `aaif-goose/goose`.
  Homebrew `block-goose-cli` has 2,756 installs in 30 days
  ([brew](https://formulae.brew.sh/api/formula/block-goose-cli.json)). The latest version
  is v1.55.0 (2026-10-16)
  ([release](https://github.com/aaif-goose/goose/releases/tag/v1.55.0)). License:
  Apache-2.0.
- Install: the `download_cli.sh` script or `brew install block-goose-cli`
  ([installation](https://goose-docs.ai/docs/getting-started/installation)). mise:
  `goose-agent` ([registry](https://github.com/jdx/mise/blob/main/registry/goose-agent.toml)).
  Update with `goose update` ([updating](https://goose-docs.ai/docs/guides/updating-goose)).
- Hooks: only in plugins, `~/.agents/plugins/<plugin>/hooks/hooks.json`, in the Claude
  nesting. Events: SessionStart, SessionEnd, Stop, UserPromptSubmit, PreToolUse,
  PostToolUse and file and shell events. There is no permission event and no context
  injection ([hooks](https://goose-docs.ai/docs/guides/context-engineering/hooks)).
- Skills: `~/.agents/skills/`
  ([skills](https://goose-docs.ai/docs/guides/context-engineering/using-skills)). MCP:
  `extensions:` in `~/.config/goose/config.yaml`
  ([extensions](https://goose-docs.ai/docs/getting-started/using-extensions)).
- Auth: the system keyring by default, `~/.config/goose/secrets.yaml` with
  `GOOSE_DISABLE_KEYRING` ([config files](https://goose-docs.ai/docs/guides/config-files)).
- Signals: OSC 7501 by default on a TTY, turned off with
  `GOOSE_CLI_PROGRAM_STATUS=false`. This is in the source and not in the docs
  ([program_status.rs](https://github.com/aaif-goose/goose/blob/main/crates/goose-cli/src/session/program_status.rs)).
  BEL is opt-in with `GOOSE_CLI_BELL`.

### Mistral Vibe

- Popularity: 5.2k stars. PyPI `mistral-vibe` 366,153 a week (see the note under the
  table). The latest version is 3.0.2 (2026-10-22)
  ([CHANGELOG](https://github.com/mistralai/mistral-vibe/blob/main/CHANGELOG.md)).
  License: Apache-2.0.
- Install: curl script, `uv tool install mistral-vibe` or Homebrew; check with
  `vibe --check-upgrade` ([README](https://github.com/mistralai/mistral-vibe/blob/main/README.md)).
- Hooks: `~/.vibe/hooks.toml` with pre_tool, post_tool and post_agent only (README).
- Skills: `~/.vibe/skills/` and `~/.agents/skills/`. MCP: `[[mcp_servers]]` in
  `~/.vibe/config.toml`. Auth: `MISTRAL_API_KEY` or `~/.vibe/.env` (README).
- Signals: OSC 7501 after a `ESC]7501;?ESC\` probe, BEL otherwise
  ([program_status.rs](https://github.com/mistralai/mistral-vibe/blob/main/vibe/cli-rust/src/program_status.rs),
  [detection.rs](https://github.com/mistralai/mistral-vibe/blob/main/vibe/cli-rust/src/program_status/detection.rs)).
  The reply check accepts anything that starts with `]7501;?` and cites "OSC 7501
  revision 0.3"
  ([terminal_input_filter.rs](https://github.com/mistralai/mistral-vibe/blob/main/vibe/cli-rust/src/terminal_input_filter.rs)).
  Where the spec itself is published is unverified.

### Cline CLI

- Popularity: 70.7k stars (the repo includes the VS Code extension), npm `cline` 42,575
  a week. The latest version is cli-v3.0.73 (2026-10-17)
  ([release](https://github.com/cline/cline/releases/tag/cli-v3.0.73)). License:
  Apache-2.0.
- Install: `npm install -g cline`, update with `cline update`
  ([CLI reference](https://docs.cline.bot/cli/cli-reference)).
- Hooks: one executable per event in `~/.cline/hooks/`. Events: TaskStart, TaskComplete,
  TaskCancel, PreToolUse, PostToolUse, UserPromptSubmit and SessionShutdown. There is no
  permission event. The CLI turns hooks off under `--yolo`
  ([hook-file-config.ts](https://github.com/cline/cline/blob/main/sdk/packages/core/src/hooks/hook-file-config.ts),
  [clineignore doc](https://docs.cline.bot/customization/clineignore)).
- Skills: `~/.cline/skills/` and `~/.agents/skills/`
  ([skills](https://docs.cline.bot/customization/skills)). MCP:
  `~/.cline/data/settings/cline_mcp_settings.json`
  ([MCP](https://docs.cline.bot/mcp/mcp-overview)).
- Auth: `~/.cline/data/settings/providers.json` ([config](https://docs.cline.bot/getting-started/config)).
- Signals: a title only. I found no BEL or OSC notification in `apps/cli/src`, and
  absence in the source is unverified as a guarantee.

### Kilo Code CLI

- Popularity: 27.7k stars, npm `@kilocode/cli` 16,095 a week, v7.8.15 (2026-10-19)
  ([npm](https://registry.npmjs.org/@kilocode/cli)). License: MIT.
- It is an opencode fork with JS plugins in `~/.config/kilo/plugin/`. Bus events include
  `session.idle` and `permission.asked`
  ([plugins](https://github.com/Kilo-Org/kilocode/blob/main/packages/kilo-docs/pages/automate/extending/plugins.md)).
- Skills: `~/.kilo/skills/`, `~/.agents/skills/` and `~/.claude/skills/`
  ([skills](https://github.com/Kilo-Org/kilocode/blob/main/packages/kilo-docs/pages/customize/skills.md)).
  MCP: `mcp` in `~/.config/kilo/kilo.json`, opencode format. Auth:
  `~/.local/share/kilo/auth.json`.

### Crush

- Popularity: 28.8k stars, npm `@charmland/crush` 3,655 a week, v0.98.1 (2026-10-09)
  ([release](https://github.com/charmbracelet/crush/releases/tag/v0.98.1)). License:
  FSL-1.1-MIT, which is source-available
  ([LICENSE](https://github.com/charmbracelet/crush/blob/main/LICENSE.md)).
- Hooks: `PreToolUse` only
  ([hooks README](https://github.com/charmbracelet/crush/blob/main/docs/hooks/README.md)).
- Skills: `~/.config/crush/skills/`, `~/.agents/skills/` and `~/.claude/skills/`
  ([README](https://github.com/charmbracelet/crush/blob/main/README.md#agent-skills)).
- Auth: keys sit in plain text in `~/.local/share/crush/crush.json`
  ([store.go](https://github.com/charmbracelet/crush/blob/main/internal/config/store.go)).
- Signals: OSC 99, falling back to OSC 777 or BEL, only when the terminal reports focus
  loss ([osc.go](https://github.com/charmbracelet/crush/blob/main/internal/ui/notification/osc.go)).

### Kiro CLI

- Popularity: Homebrew cask `kiro-cli` 1,712 in 30 days
  ([brew](https://formulae.brew.sh/api/cask/kiro-cli.json)). Closed source; the Amazon Q
  CLI repo now gets only security fixes
  ([README](https://github.com/aws/amazon-q-developer-cli/blob/main/README.md)).
- Hooks: `~/.kiro/hooks/*.json`. Events: SessionStart, SessionEnd, UserPromptSubmit,
  PreToolUse, PostToolUse, Stop and file events. There is no permission or
  notification event ([hook types](https://kiro.dev/docs/hooks/types)).
- Skills: `~/.kiro/skills/` ([skills](https://kiro.dev/docs/skills)). MCP:
  `~/.kiro/settings/mcp.json` ([MCP](https://kiro.dev/docs/mcp/configuration)).
- Hosts: a published firewall list
  ([firewalls](https://kiro.dev/docs/privacy-and-security/firewalls)).
- Signals: `chat.notificationMethod` set to `bel` or `osc9`
  ([settings](https://kiro.dev/docs/reference/settings)).

### Auggie

- npm `@augmentcode/auggie` 16,297 a week. Proprietary
  ([LICENSE](https://github.com/augmentcode/auggie/blob/main/LICENSE.md)).
- Hooks: in `~/.augment/settings.json`. Events: PreToolUse, PostToolUse, Stop,
  SessionStart and SessionEnd ([hooks](https://docs.augmentcode.com/cli/hooks)).
- Skills: `~/.augment/skills/`, `~/.claude/skills/` and `~/.agents/skills/`
  ([skills](https://docs.augmentcode.com/cli/skills)).
- Signals: a bell when a task completes ([reference](https://docs.augmentcode.com/cli/reference)).

### Aider

- 49.7k stars, PyPI `aider-chat` 68,221 a week. The last GitHub release is v0.86.0
  (2025-08-09) ([release](https://github.com/Aider-AI/aider/releases/tag/v0.86.0)), and
  PyPI has 0.86.2 from 2026-02-12 ([PyPI](https://pypi.org/pypi/aider-chat/json)).
  License: Apache-2.0.
- It has no hooks, plugins or MCP. `--notifications-command` runs one shell command when
  aider waits for input, with no payload
  ([notifications](https://aider.chat/docs/usage/notifications.html),
  [io.py](https://github.com/Aider-AI/aider/blob/main/aider/io.py)). With
  `--notifications` and no command found, it sends BEL. That could call
  `agent-state done`, and nothing more.

### Other popular agents seen

Hermes Agent (NousResearch/hermes-agent) reports about 256k stars, but it is a general
agent and not coding-first ([repo](https://github.com/NousResearch/hermes-agent)).
OpenHands has 91.6k stars, but its separate CLI repo is small
([OpenHands-CLI](https://github.com/OpenHands/OpenHands-CLI)). I did not research either
in depth.

## Unverified

- The license terms of Cursor CLI.
- Where Copilot CLI turns on desktop notifications, and a Copilot CLI-specific host list.
- Whether Antigravity CLI reads Gemini CLI hooks, its keyring fallback file, and its
  consumer sign-in hosts.
- Gemini CLI's `generativelanguage.googleapis.com` use in API-key mode, and whether it has
  a `gemini update` command.
- Qwen Code keyring use.
- Factory droid and Amp credential storage, and Cursor's Linux auth file. These come from
  reading the shipped bundles, not from docs.
- Cursor's terminal detection and its silence on unknown terminals (bundle reading).
- Which Cursor hook events fire in the CLI and not only in the IDE.
- The OSC 7501 specification document and the reply format beyond the `]7501;?` prefix.
- Cline file hooks under `--yolo` (documented, not traced in source), and the absence of
  BEL or OSC in its CLI.
- The cause of Mistral Vibe's high PyPI count.
- Kimi Code SessionStart context injection.
- Exact download counts for curl-script installs, which no registry reports. This
  affects Cursor, Antigravity, Factory droid, Kiro, Amp and Goose.

## Sources

Package and popularity data:
- GitHub REST API, `https://api.github.com/repos/<owner>/<repo>`, read 2026-10-22.
- npm downloads API, `https://api.npmjs.org/downloads/point/last-week/<package>`, and the
  registry, `https://registry.npmjs.org/<package>`.
- pypistats, `https://pypistats.org/api/packages/<package>/recent`.
- Homebrew API, `https://formulae.brew.sh/api/formula/<name>.json` and
  `https://formulae.brew.sh/api/cask/<name>.json`.
- mise registry, `https://github.com/jdx/mise/tree/main/registry`.

Agent docs and source are linked inline in each section above. The main entry points:
- Copilot CLI: https://docs.github.com/en/copilot/reference/hooks-reference
- Qwen Code: https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/hooks.md
- Antigravity CLI: https://antigravity.google/docs/hooks/ and
  https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli
- Gemini CLI: https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md
- Kimi Code CLI: https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/customization/hooks.md
- Factory droid: https://docs.factory.com/harness/hooks
- Cursor CLI: https://cursor.com/docs/hooks and https://cursor.com/docs/reference/third-party-hooks
- Amp: https://ampcode.com/docs/plugin-api
- Goose: https://goose-docs.ai/docs/guides/context-engineering/hooks
- Mistral Vibe: https://github.com/mistralai/mistral-vibe/blob/main/README.md
- Cline: https://docs.cline.bot/cli/cli-reference
- Kilo Code: https://github.com/Kilo-Org/kilocode/blob/main/packages/kilo-docs/pages/automate/extending/plugins.md
- Crush: https://github.com/charmbracelet/crush/blob/main/docs/hooks/README.md
- Kiro CLI: https://kiro.dev/docs/hooks/types
- Auggie: https://docs.augmentcode.com/cli/hooks
- Aider: https://aider.chat/docs/usage/notifications.html
- wezterm OSC handling at the pinned revision: `term/src/terminalstate/performer.rs`,
  rev e2a98e1b8af156988ad0ac778658ac27b60a55fb (local cargo git checkout).
