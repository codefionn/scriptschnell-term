# forge-clis

Gives sandboxed terminals the logins of the forge command-line tools, without the real
tokens: `fj` (forgejo-cli), `tea`, `glab` and `gh`.

## What it does

Each tool's login file reaches the sandbox with fake tokens in place of the real ones.
The proxy puts the real token into requests to the host it belongs to. A fake sent
anywhere else stays a fake.

| Tool | File | Notes |
| --- | --- | --- |
| `fj` | `$XDG_DATA_HOME/forgejo-cli/keys.json` | OAuth logins never expire in the sandbox's copy |
| `tea` | `$XDG_CONFIG_HOME/tea/config.yml` | OAuth logins never expire in the sandbox's copy |
| `glab` | `$GLAB_CONFIG_DIR/config.yml` or `$XDG_CONFIG_HOME/glab-cli/config.yml` | OAuth logins never expire in the sandbox's copy |
| `gh` | `$GH_CONFIG_DIR/hosts.yml` or `$XDG_CONFIG_HOME/gh/hosts.yml` | Tokens from the keyring come from `gh auth token` |

It also allows the hosts of these logins when the sandbox network is limited to allowed
hosts.

Git credential helpers that read these tools' logins work in the sandbox too, such as
`gh auth git-credential` or a helper that runs `fj whoami` and reads `keys.json`. They
hand git the fake, and the proxy swaps it on the way out. `git push` still waits for you
through the proxy's own `cmdterm.git-push` guard.

### OAuth logins

An OAuth access token lasts about an hour, and a refresh in the sandbox would bring the
new real token into it. So the sandbox's copies say that OAuth logins of `fj`, `glab` and
`tea` never expire, and they hold no refresh token. This machine refreshes them instead:
when a sandbox comes up, and every 10 minutes while the active project is on this
machine, the extension refreshes every login that expires within 20 minutes.

The tools refresh a login only once it has expired (`fj`) or is within 5 minutes of it
(`glab`, `tea`). So the extension first marks the login expired in the tool's file on
this machine, then runs the tool here, which refreshes it and writes the new token:

| Tool | Marked expired | Then runs |
| --- | --- | --- |
| `fj` | `expires_at` in `keys.json` | `fj whoami --host https://<host>` |
| `glab` | `oauth2_expiry_date` in `config.yml` | `glab api user --hostname <host>` |
| `tea` | `token_expiry` in `config.yml` | `tea repos list --login <name> --limit 1 --output simple` |

The fake stays the same, so terminals that already run keep working with the new token.
If a tool cannot refresh a login, a notice names the command that logs in again:
`fj auth login --host <host>`, `glab auth login --hostname <host>` or
`tea login oauth-refresh <name>`.

Newer `tea` versions keep OAuth logins in `$XDG_CONFIG_HOME/tea/credentials.json` or in
the system's keyring instead of `config.yml`. A fake cannot stand in for those, so the
sandbox does not see `credentials.json`, and such logins do not work there.

### gh

`gh` usually keeps its tokens in the system's keyring, and `~/.config/gh` is hidden in
sandboxes by default. The extension runs `gh auth token --hostname <host>` on this
machine for every host in `hosts.yml` and writes the fake into the sandbox's copy of
`hosts.yml` as `oauth_token`. The first terminal of a sandbox may start before that
finished; later ones get it.

## Permissions

`env`, `filesystem`, `sandbox`, `secrets`, `run` and `process` (to run `fj`, `glab`, `tea` and `gh` on
this machine). A shipped copy has them from the start; Revoke on its page of the
settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description and permissions.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
