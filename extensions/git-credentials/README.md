# git-credentials

Gives sandboxed terminals your HTTPS logins for git hosts, without the real tokens.

## What it does

- Rewrites `~/.git-credentials` with fake tokens. The file is hidden in sandboxes by
  default; the rewritten copy stands in for it.
- Hands `GH_TOKEN` and `GITHUB_TOKEN` (for github.com and api.github.com) and
  `GITLAB_TOKEN` (for gitlab.com) to sandboxes as fakes.
- The proxy puts the real token into requests to the host it belongs to. A fake sent
  anywhere else stays a fake.

`git push` still waits for you: the proxy's own `cmdterm.git-push` guard asks before any
push leaves a sandbox, whatever the host.

It applies its rules when the app starts and again whenever a sandbox comes up. Press
Reload extensions after you change a login.
The Registry logins page under Accounts (Ctrl+, then Accounts, then Registry logins) lists where it
reads logins and the hosts it found logins for.

## Permissions

`env`, `filesystem`, `sandbox`, `secrets`. A shipped copy has them from the
start; Revoke on its page of the settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description and permissions.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
