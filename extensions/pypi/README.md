# pypi

Gives sandboxed terminals your PyPI logins, without the real passwords.

## What it does

- Rewrites `~/.pypirc` with fake passwords; the rewritten file stands in for the real one
  inside the sandbox.
- Hands `TWINE_PASSWORD` to sandboxes as a fake for upload.pypi.org, or for the host of
  `TWINE_REPOSITORY_URL` when that is set.
- Adds a `pypi.upload` guard for your repositories other than PyPI, which already has the
  proxy's own guard.

pip's cache lives under `~/.cache`, which sandboxes may write by default.

It applies its rules when the app starts and again whenever a sandbox comes up. Press
Reload extensions after you change a login.
The Registry logins page under Accounts (Ctrl+, then Accounts, then Registry logins) lists where it
reads logins and the hosts it found logins for.

## Permissions

`env`, `filesystem`, `sandbox`, `secrets`, `guards`. A shipped copy has them from the start;
Revoke on its page of the settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description and permissions.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
