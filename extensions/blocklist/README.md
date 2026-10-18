# blocklist

Blocks ad, tracker and malware hosts in sandboxed terminals.

## What it does

- Names [HaGeZi's DNS blocklist](https://github.com/hagezi/dns-blocklists) (Multi PRO by
  default) for the sandbox proxy. scriptschnell-term downloads it from GitHub, or from the
  Codeberg mirror when GitHub fails, and keeps it in `blocklists/` in the cache
  directory (`~/.cache/scriptschnell-term` on Linux).
- Downloads the list again once the copy is a week old. While a new copy cannot be
  downloaded, the old one keeps blocking, and a notice says why.
- The proxy refuses a listed host and its subdomains with `403 Forbidden`, whatever the
  allowed hosts say, also in sandboxes with the whole network.

Nothing is downloaded until a sandbox uses the proxy, so the first sandboxed terminal
may reach listed hosts for the few seconds the first download takes.

A sandbox with the whole network reaches the proxy through `HTTP_PROXY` and
`HTTPS_PROXY`. Programs that ignore those variables go around it and are not blocked.

## Settings

On the extension's page of the settings (Ctrl+,):

- List: `pro` (the default), `light`, `multi`, `pro.plus`, `ultimate`, `tif` (threat
  intelligence only) or `off`.
- Download again after (days): 7 by default.
- More lists: `https://` URLs of further lists. Plain domains, hosts files
  (`0.0.0.0 ads.example`) and Adblock domain rules (`||ads.example^`) work. The
  extension may download from the hosts of these URLs, besides raw.githubusercontent.com
  and codeberg.org, which its manifest lists.
- Blocked hosts: hosts to block as well. `example.com` blocks it and its subdomains,
  `*.example.com` only the subdomains.
- Never blocked: hosts no list blocks, in the same form. Use it when a list blocks a host
  you need, such as `sentry.io`.

## Permissions

`sandbox` and `network`. A shipped copy has them from the start; Revoke on its page of
the settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description, permissions and settings.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
