# claude-code

Sets up Claude Code in devcontainers, VMs and remote machines.

## What it does

When a worktree's environment comes up (a devcontainer, VM or remote machine; SSH hosts
are off by default), it copies your Claude Code setup from `~/.claude` into the
environment's `~/.claude`: `settings.json`, `CLAUDE.md`, `agents`, `commands`, `skills`,
`hooks`, `output-styles` and `plugins`. Caches, logs and lock files stay behind. When
`claude` is missing there, it installs Claude Code with the official script.

The palette command "Claude Code: set up this environment" (`claude-code.setup`) does the
same for the active worktree on demand.

Your login is not copied unless you turn on "Copy the login". Copying it gives everything
that runs in the environment your Claude account. Without it, run `claude` once inside
and log in there.

## Settings

Change them on the extension's page of the settings (Ctrl+,, Extensions, claude-code).
They are saved under `extension_settings.claude-code` in `settings.json`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kinds` | list | `devcontainer`, `vm`, `remote` | Environment kinds to set up when they come up; `ssh` is the fourth |
| `copy` | list | `settings.json`, `CLAUDE.md`, `agents`, `commands`, `skills`, `hooks`, `output-styles`, `plugins` | Entries of `~/.claude` to copy |
| `credentials` | bool | false | Also copy `~/.claude/.credentials.json` |
| `install` | bool | true | Run the official install script where `claude` is missing |

## Permissions

`env`, `filesystem`, `copy`, `run`. A shipped copy has them from the
start; Revoke on its page of the settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description and permissions.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
