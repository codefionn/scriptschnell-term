# Extensions

scriptschnell-term extensions are written in Lua 5.4 (running in isolated Oximond VMs) or
compiled to Lua from Teal. Extensions can add palette commands, keybindings, recurring
tasks, sandbox rules and guards, fake credentials, forge integrations, custom coding
agents, agent MCP tools and custom remote environment schemes.

## Where extensions live

- **Shipped extensions and docs**: written on every start to `extensions/` in the app's
  data directory (`~/.local/share/scriptschnell-term/extensions/` on Linux), alongside
  `README.md` (the Lua API reference) and editor type files (`types/term.lua` for
  lua-language-server and `types/term.d.tl` for Teal). Later releases update untouched
  shipped files automatically.
- **Your own extensions**: placed in `extensions/` in the configuration directory
  (`~/.config/scriptschnell-term/extensions/` on Linux), where the app also maintains
  `.luarc.json` and `tlconfig.lua` pointing at the type files. Putting an extension with
  the same name as a shipped one replaces the shipped copy.
- **Shipped-only builds**: compiling with `--no-default-features` disables the
  `third-party-extensions` feature so only the shipped extensions load and the control
  socket refuses extension changes from agents.

See [CONFIGURATION.md](../crates/cmdterm/CONFIGURATION.md) for macOS and Windows
directory paths.

## Permissions and settings

Each directory extension includes a `manifest.toml` declaring its title, description,
required permissions (such as `terminal`, `sandbox`, `secrets`, `guards`, `network`,
`agents`, `environments`, `filesystem`, `process`, `env`) and optional user settings.

Open **Settings > Extensions** (`Ctrl+,`) to:

- Turn extensions on or off (`disabled_extensions` / `enabled_extensions`).
- Grant or revoke individual permissions per extension (`extension_permissions`). Shipped
  extensions start with their shipped manifest permissions granted; third-party
  extensions wait until you grant their permissions.
- Edit each extension's settings (`extension_settings`) and review built-in guards.
- Press **Reload extensions** after editing Lua files.

An extension error is reported as a status notice and disables only that extension; it
never crashes the app or blocks UI frames.

## Shipped extensions

### Sandbox credentials, registries and toolchains

| Extension | Description | Default |
| --- | --- | --- |
| [`npm`](../extensions/npm/README.md) | npm cache and fake registry tokens for sandboxes, plus publish guard | On |
| [`cargo`](../extensions/cargo/README.md) | Cargo registry cache and fake tokens for sandboxes, plus publish guard | On |
| [`nuget`](../extensions/nuget/README.md) | NuGet cache and fake feed passwords for sandboxes, plus push guard | On |
| [`pypi`](../extensions/pypi/README.md) | PyPI fake upload credentials for sandboxes, plus upload guard | On |
| [`git-credentials`](../extensions/git-credentials/README.md) | Git over HTTPS fake logins for sandboxes | On |
| [`forge-clis`](../extensions/forge-clis/README.md) | Fake tokens for `fj`, `tea`, `glab` and `gh` inside sandboxes | On |
| [`toolchains`](../extensions/toolchains/README.md) | Writable toolchain managers in sandboxes (`rustup`, `nvm`, `fnm`, `volta`, `npx`, `pnpm`, `bun`, `deno`, `pyenv`, `uv`, `go`, `dotnet tool`, `mise`, `asdf`, `sdkman`) | On |
| [`blocklist`](../extensions/blocklist/README.md) | Blocks ad, tracker and malware hosts in the sandbox proxy using HaGeZi's DNS blocklist | On |
| [`aspnet-devcerts`](../extensions/aspnet-devcerts/README.md) | Shares this machine's ASP.NET Core HTTPS dev certificate with devcontainers | Off |

### Forges and change requests

These power merge checks in the rail's **Agents** tab, the explore panel's change request
view, agent `change_request` tools and sandbox guards on creating, commenting on or
merging change requests:

| Extension | Forge | Default |
| --- | --- | --- |
| [`github`](../extensions/github/README.md) | GitHub pull requests | On |
| [`gitlab`](../extensions/gitlab/README.md) | GitLab merge requests | On |
| [`bitbucket`](../extensions/bitbucket/README.md) | Bitbucket Cloud pull requests | On |
| [`forgejo`](../extensions/forgejo/README.md) | Forgejo pull requests | On |
| [`gitea`](../extensions/gitea/README.md) | Gitea pull requests | On |
| [`azure-devops`](../extensions/azure-devops/README.md) | Azure DevOps pull requests | On |

### Remote environments and agent setups

| Extension | Description | Default |
| --- | --- | --- |
| [`claude-code`](../extensions/claude-code/README.md) | Copies Claude Code setup/login into devcontainers, VMs and remote machines and installs the CLI | On |
| [`dotfiles`](../extensions/dotfiles/README.md) | Copies selected dotfiles into devcontainers, VMs and remote machines on start | On |
| [`knowledge`](../extensions/knowledge/README.md) | Exposes Markdown document collections as MCP/control tools for coding agents | On |
| [`kubernetes`](../extensions/kubernetes/README.md) | Opens projects in Kubernetes pods with `k8s://pod/path` via `kubectl exec` | Off |
| [`coder`](../extensions/coder/README.md) | Opens projects in Coder workspaces with `coder://workspace/path` via `coder ssh` | Off |

## Writing extensions

See [crates/cmdterm/src/ext/README.md](../crates/cmdterm/src/ext/README.md) for the full
`term` Lua API, events, manifest schema, permissions and Teal compilation workflow.
