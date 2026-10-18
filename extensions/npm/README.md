# npm

Gives sandboxed terminals npm's cache and your npm logins, without the real tokens.

## What it does

- Allows `registry.npmjs.org` when the sandbox network is limited to allowed hosts, and
  makes `~/.npm` writable.
- Rewrites `~/.npmrc` (or the file `NPM_CONFIG_USERCONFIG` names) with fake tokens; the
  rewritten file stands in for the real one inside the sandbox.
- Hands `NPM_TOKEN` and `NODE_AUTH_TOKEN` to sandboxes as fakes for registry.npmjs.org.
- Adds an `npm.publish` guard for the other registries your `.npmrc` names.
  registry.npmjs.org already has the proxy's own guard.

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
