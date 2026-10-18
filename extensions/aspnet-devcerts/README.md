# aspnet-devcerts

Gives devcontainers this machine's ASP.NET Core HTTPS development certificate. Kestrel in
the container then serves the certificate your browser here trusts, and programs in the
container trust it too. It works the same on Windows, macOS and Linux hosts, with any
Linux devcontainer image that has the .NET SDK.

It is off until you turn it on (Ctrl+,, Extensions, show all, aspnet-devcerts).

## What it does

When a devcontainer of a project on this machine comes up, and the container has
`dotnet`, it:

1. Creates the dev certificate on this machine if there is none (`dotnet dev-certs https`)
   and trusts it here (`--trust`). Windows shows its certificate dialog once; on Linux
   `--trust` needs what `dotnet dev-certs` needs there, and a failure only gives a notice.
2. Exports it here as a PFX with a one-time password and as a PEM certificate, into the
   extension's directory in the app's cache.
3. Copies the PFX into the container's `~/.aspnet/https` and imports it with
   `dotnet dev-certs https --clean --import`. Kestrel uses it from then on. The copy is
   deleted after the import.
4. Adds the PEM certificate to the container's system trust store as `aspnetcore-dev`
   (`update-ca-certificates` on Debian, Ubuntu and Alpine, `update-ca-trust` on Fedora,
   RHEL and Arch) and to `~/.pki/nssdb` when `certutil` is there.

The exports on this machine are deleted as soon as the next step has them. Containers
without `dotnet` are left alone. Every step repeats safely, so it runs each time the
container comes up.

The palette command "ASP.NET Core: share the dev certificate with this devcontainer"
(`aspnet-devcerts.share`) does the same for the active worktree on demand.

Port forwarding is not part of it: open the forwarded `https://localhost:<port>` on this
machine and the browser sees the certificate it already trusts.

## Requirements

- The .NET SDK on this machine and in the container. Keep their major versions close:
  an SDK only imports dev certificates it considers current.
- Root or passwordless `sudo` in the container for step 4. The Microsoft devcontainer
  images have `sudo` for their `vscode` user. Without it the container still serves the
  certificate and a notice says that trusting it there failed.
- Projects on this machine. Devcontainers on SSH hosts, VMs and remote machines are
  skipped, since the certificate your browser trusts lives here.

## Settings

Change them on the extension's page of the settings (Ctrl+,, Extensions, aspnet-devcerts).
They are saved under `extension_settings.aspnet-devcerts` in `settings.json`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `trust_host` | bool | true | Run `dotnet dev-certs https --trust` on this machine |
| `trust_container` | bool | true | Add the certificate to the container's trust store |

## Permissions

`filesystem` (deleting the exports), `process` (running `dotnet dev-certs` on this machine
through `term.run{ host = true }`), `copy`, `run` and `trust`. A shipped copy has them from
the start; Revoke on its page of the settings (Ctrl+,) takes them away.

## Other ecosystems

The same pieces work for any development certificate: run the tool that makes it on this
machine with `term.run{ host = true }`, carry it over with `term.copy`, and trust it with
`term.trust`, chaining the steps through `done` functions. `extensions/README.md` in the
data directory describes them.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description, permissions and settings.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
