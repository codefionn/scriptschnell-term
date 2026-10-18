# toolchains

Lets sandboxed terminals install and run toolchains and global tools. Without it a
sandbox sees your home read-only, so `rustup` cannot add the toolchain a
`rust-toolchain.toml` asks for, `nvm install` fails, and `npx`, `pnpm dlx` or
`dotnet tool install` have nowhere to put what they download.

## What it does

For each tool below it makes the tool's directories writable in sandboxes and allows its
download hosts when the sandbox network is limited to allowed hosts. A variable moves a
directory: with `RUSTUP_HOME` set, that directory becomes writable instead of `~/.rustup`.

| Tool | Writable | Hosts |
| --- | --- | --- |
| `rustup` | `$RUSTUP_HOME` (`~/.rustup`) | static.rust-lang.org |
| `cargo` | `bin`, `.crates.toml`, `.crates2.json` and the cache locks in `$CARGO_HOME` (`~/.cargo`) | crates.io |
| `nvm` | `$NVM_DIR` (`~/.nvm`) | nodejs.org |
| `fnm` | `$FNM_DIR` (`~/.local/share/fnm`), `~/.fnm`, `~/.local/state/fnm_multishells` | nodejs.org |
| `volta` | `$VOLTA_HOME` (`~/.volta`) | nodejs.org, registry.npmjs.org |
| `npm` | `$npm_config_cache` (`~/.npm`, where `npx` keeps packages), `$NPM_CONFIG_PREFIX` (`~/.npm-global`) | registry.npmjs.org |
| `pnpm` | `$PNPM_HOME` (`~/.local/share/pnpm`) | registry.npmjs.org |
| `yarn` | `~/.yarn`, `~/.config/yarn` | registry.npmjs.org, registry.yarnpkg.com, repo.yarnpkg.com |
| `bun` | `$BUN_INSTALL` (`~/.bun`) | registry.npmjs.org, bun.sh |
| `deno` | `$DENO_INSTALL_ROOT` (`~/.deno`), `$DENO_DIR` (`~/.cache/deno`) | deno.land, dl.deno.land, jsr.io, registry.npmjs.org |
| `pyenv` | `$PYENV_ROOT` (`~/.pyenv`) | www.python.org |
| `uv` | `$UV_PYTHON_INSTALL_DIR` (`~/.local/share/uv/python`), `$UV_TOOL_DIR` (`~/.local/share/uv/tools`) | pypi.org, files.pythonhosted.org |
| `pipx` | `$PIPX_HOME` (`~/.local/share/pipx`) | pypi.org, files.pythonhosted.org |
| `go` | `$GOMODCACHE` (`~/go/pkg/mod`), `$GOBIN` (`~/go/bin`) | proxy.golang.org, sum.golang.org, go.dev, dl.google.com |
| `dotnet` | `$DOTNET_CLI_HOME/.dotnet` (`~/.dotnet`, with global tools in `tools`), `$NUGET_PACKAGES` (`~/.nuget/packages`), `~/.local/share/NuGet` | the nuget.org hosts and the .NET download hosts |
| `mise` | `$MISE_DATA_DIR` (`~/.local/share/mise`), `$MISE_STATE_DIR` (`~/.local/state/mise`) | mise.jdx.dev, mise-versions.jdx.dev, nodejs.org, static.rust-lang.org |
| `asdf` | `$ASDF_DATA_DIR` (`~/.asdf`) | |
| `sdkman` | `$SDKMAN_DIR` (`~/.sdkman`) | api.sdkman.io, broker.sdkman.io |
| `rbenv` | `$RBENV_ROOT` (`~/.rbenv`) | cache.ruby-lang.org |
| `gem` | `$GEM_HOME` (`~/.gem`), `~/.local/share/gem` | rubygems.org, index.rubygems.org |

Many tools fetch releases from GitHub, which the default allowed hosts already cover.
Directories that do not exist when a sandbox starts stay missing inside it, so install a
manager once outside the sandbox. `~/.local/bin`, where `uv tool` and `pipx` link their
commands, stays read-only: add it under More writable paths if you want those links.

It applies its rules when the app starts and again whenever a sandbox comes up.

## Trade-off

The host runs what a sandbox writes into these directories: a toolchain, a global tool,
an `npx` package. A process in the sandbox could replace one of them with something
else, and you would run it outside the sandbox later. Turn a tool off to keep its
directories read-only.

## Settings

On the extension's page of the settings (Ctrl+,):

- Turned off: tools by the names in the table, such as `cargo` or `dotnet`. Their
  directories stay read-only and their hosts are not added.
- More writable paths: further directories sandboxes may write. Paths may start with `~/`.

## Permissions

`env` and `sandbox`. A shipped copy has them from the start; Revoke on its page of the
settings (Ctrl+,) takes them away.

## Files

- `init.lua`: the extension.
- `manifest.toml`: its description, permissions and settings.

scriptschnell-term replaces these files when a new version ships, unless you changed them.
To change the behavior, copy the directory under another name and turn this one off.
