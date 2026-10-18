# cargo

Gives sandboxed terminals Cargo's registry cache and your Cargo logins, without the real
tokens.

## What it does

- Makes `$CARGO_HOME/registry` and `$CARGO_HOME/git` writable in sandboxes (`CARGO_HOME`
  defaults to `~/.cargo`), so builds reuse the host's download cache.
- Rewrites `credentials.toml` (or the older `credentials`) with fake tokens and puts the
  rewritten file in place of the real one inside the sandbox.
- Hands `CARGO_REGISTRY_TOKEN` and `CARGO_REGISTRIES_<NAME>_TOKEN` to sandboxes as fakes.
  The proxy swaps the real token back in for requests to the registry the token belongs to.
  A token for a registry whose index host it cannot find reaches the sandbox empty.
- Adds a `cargo.publish` guard for every registry in `config.toml` other than crates.io.
  crates.io already has the proxy's own guard.

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
To change the behavior, copy the directory under another name and turn this one off.
