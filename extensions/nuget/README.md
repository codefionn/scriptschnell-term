# nuget

Gives sandboxed terminals NuGet's package cache and your feed logins, without the real
passwords.

## What it does

- Makes `~/.nuget/packages` and `~/.local/share/NuGet` writable in sandboxes.
- Rewrites `NuGet.Config` (`~/.nuget/NuGet/NuGet.Config` or
  `~/.config/NuGet/NuGet.Config`) with fake `ClearTextPassword` values. Encrypted
  passwords stay as they are, since only Windows can decrypt them.
- The proxy puts the real password into requests to the feed it belongs to.
- Adds a `nuget.push` guard for your feeds other than nuget.org, which already has the
  proxy's own guard.

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
