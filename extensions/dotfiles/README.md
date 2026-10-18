# dotfiles

Copies chosen dotfiles into devcontainers, VMs and remote machines when they come up, so
shells there feel like yours.

## What it does

When a worktree's environment comes up (a devcontainer, VM or remote machine; SSH hosts
are off by default), it copies each listed path under your home directory to the same
place under the environment's home. Missing paths are skipped. The default list is
`.gitconfig`, `.config/git`, `.tmux.conf`, `.inputrc`, `.config/nvim` and
`.config/starship.toml`.

The palette command "Dotfiles: copy into this environment" (`dotfiles.copy`) does the same
for the active worktree on demand.

## Settings

Change them on the extension's page of the settings (Ctrl+,, Extensions, dotfiles).
They are saved under `extension_settings.dotfiles` in `settings.json`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `files` | list | the list above | Paths under your home directory to copy |
| `kinds` | list | `devcontainer`, `vm`, `remote` | Environment kinds to copy into when they come up; `ssh` is the fourth |

## Permissions

`env`, `filesystem`, `copy`. A shipped copy has them from the
start; Revoke on its page of the settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description and permissions.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
