# scriptschnell-term extensions

scriptschnell-term runs extensions written in Lua. Each extension gets its own Lua 5.4 VM
(Oximond) with the full standard library, so `io`, `os`, `require` and `debug`
all work. LuaJIT `ffi` is not available.

## Where extensions live

The app loads every `<dir>/<name>/init.lua` and every `<dir>/<name>.lua` of two
directories, in name order. The extension's name is the directory name or the file
stem. A directory extension can `require` modules that sit next to its `init.lua`.

The Extensions page of the settings (Ctrl+,) lists the extensions found there,
the active ones unless its filter shows all or the turned-off ones, and turns each
one on or off. A turned-off extension is not loaded at all. Its
name is saved under `disabled_extensions` in `settings.json`. Turning one on or
off, or pressing Reload extensions, drops every VM and loads the directories again,
so each extension sees `app_started` a second time.

Your own extensions go in `extensions/` of the configuration directory,
`~/.config/scriptschnell-term/extensions` on Linux. The shipped ones, this file and the
type files are in `extensions/` of the data directory, `~/.local/share/scriptschnell-term/extensions`
on Linux (`CONFIGURATION.md` lists the directories of every platform). One of yours
replaces a shipped extension of the same name. With `--config-dir` both are the same
`extensions/`. The app does not load `tlconfig.lua` or anything starting with a dot,
and a directory without `init.lua`, such as `types/`, is skipped.

### Shipped extensions

Every start writes the shipped extensions into the data directory: `aspnet-devcerts`, `azure-devops`, `bitbucket`, `blocklist`, `cargo`,
`claude-code`, `coder`, `dotfiles`, `forge-clis`, `forgejo`, `git-credentials`, `gitea`, `github`, `gitlab`, `knowledge`, `kubernetes`, `npm`, `nuget`, `pypi` and `toolchains`, each with a `README.md`
that says what it does. `aspnet-devcerts`, `coder` and `kubernetes` are off until you turn them on. They have the permissions their shipped `manifest.toml` declares without a grant,
so they load right away; Revoke on their page takes that away, and a manifest you widened
waits for a grant of the rest. A new version of the app updates their
files, unless you changed or deleted them; `.bundled.json` in the data directory
records what the app wrote. To change one, copy its directory into your own `extensions/`,
under its name to replace it or under a new name next to it, and edit the copy.

### Types for your editor

The app also keeps `types/term.lua` (LuaCATS annotations for lua-language-server) and
`types/term.d.tl` (Teal declarations) current in the data directory's `extensions/`, and
writes `.luarc.json` and `tlconfig.lua` pointing to them into your own `extensions/`.
Open your extensions directory as the workspace and lua-language-server completes and
checks the `term` API.

To write an extension in Teal, put `init.tl` in its directory and compile it from the
extensions directory:

```sh
cd ~/.config/scriptschnell-term/extensions
tl check my-ext/init.tl
tl gen my-ext/init.tl -o my-ext/init.lua
```

The app loads only `init.lua`. Event handlers take the matching record, such as
`Term.WorktreeActivated`:

```lua
-- my-ext/init.tl
term.on("worktree_activated", function(e: Term.WorktreeActivated)
   term.notify("on " .. (e.branch or "a detached head"))
end)
```

## Manifest and permissions

A directory extension declares what it is and what it needs in `manifest.toml`
next to its `init.lua`:

```toml
title = "npm"
description = "npm cache and logins for sandboxes"
version = "1.0"
permissions = ["env", "filesystem", "sandbox", "secrets", "guards"]
```

Every key is optional; unknown keys and permissions are errors. `title` is the name
the Extensions page shows; without it the page shows the directory name, which stays
the extension's id everywhere else. `enabled = false` keeps
the extension off until the user turns it on on the Extensions page; its name is then
saved under `enabled_extensions` in `settings.json`. An extension gets
only the permissions it declares, and only those the user granted: the Extensions
page lists each declared permission with a switch, and the extension runs with the
ones that are on. While none of the declared ones are on, it is listed with "needs
permission" and not loaded. Grants are saved per extension under
`extension_permissions` in `settings.json`, as the exact set that is on. A manifest
that later declares more waits for a grant of the new ones. Shipped extensions start
with a grant of what their shipped manifest declares. A single
`.lua` file, or a directory without a manifest, has no permissions.

| Permission | Allows |
| --- | --- |
| `terminal` | `term.open_terminal`, `term.send_text` |
| `browser` | `term.open_browser` |
| `worktree` | `term.switch_worktree` |
| `copy` | `term.copy` |
| `run` | `term.run` |
| `trust` | `term.trust` |
| `sandbox` | `term.sandbox` |
| `secrets` | `term.secret` |
| `guards` | the `guards` fields of `term.sandbox` and `term.forge` |
| `network` | the `blocklists` field of `term.sandbox`: the app downloads from the extension's URLs; `term.http`, `term.forge`, `term.forge_report` and `term.forge_discussion` |
| `agents` | `term.tool`: tools agents call through the control socket and its MCP server; `term.agent`: coding agents the app integrates like its own |
| `environments` | `term.environment`: location schemes whose commands run through a program of this machine the extension names. Grant it only to extensions you would let run programs |
| `storage` | `term.store`: data the extension keeps for the whole app |
| `project_storage` | `term.project_store`: data the extension keeps per project, apart from every other project |
| `filesystem` | Lua's `io`, `dofile`, `loadfile`, `os.remove`, `os.rename`, `os.tmpname`, and `require` beyond the extension's directory |
| `process` | `os.execute`, `io.popen`, `package.loadlib`, the `debug` library and `term.run{ host = true }` |
| `env` | `os.getenv` |

Registering handlers, commands, tasks and views, `term.notify`, `term.settings()` and
`term.context()` need no permission. A `term` function called without its permission
raises an error, which disables the extension like any other. Without `filesystem`,
`process` or `env` their functions are not in the VM at all.

### Settings

`[[settings]]` tables in the manifest declare what the user can change on the
extension's page of the settings (Ctrl+, then Extensions, then the extension):

```toml
[[settings]]
key = "files"
title = "Files"
description = "Paths under your home directory."
type = "list"
default = [".gitconfig", ".tmux.conf"]
```

- `key` is lowercase letters, digits and `_`. `title` defaults to the key, and the
  page shows `description` under it.
- `type` is `bool` (a switch), `integer`, `string`, `list` (strings, one row each
  with Remove, and a field with Add for a new one), `hosts` (a list of host names or
  `*.domain` patterns), `urls` (a list of `https://` URLs) or `choice` (one of
  `choices`, a list of strings). Hosts in a `hosts` setting and the hosts of the URLs
  in a `urls` setting count as hosts of the extension (see Hosts below).
- `default` must fit the type. Without one it is false, 0, "", an empty list or the
  first choice.

`term.settings()` returns a table from key to value, the user's value or else the
default. Values are saved under `extension_settings` in `settings.json`. A switch, a
choice, or adding or removing a list entry reloads the extensions at once; typed values
reload them when the user leaves the page or closes the settings. A saved value that no longer fits its
setting gives way to the default.

### Hosts

`term.http` and the `blocklists` of `term.sandbox` reach only the extension's hosts:

- the manifest's top-level `hosts`, host names such as `api.bitbucket.org` or
  `*.example.com` for every subdomain,
- the accounts of the extension's forges and their subdomains, such as
  `api.github.com` for the account `github.com`,
- the values of its `hosts` settings,
- the hosts of the values of its `urls` settings.

```toml
hosts = ["api.bitbucket.org"]

[[settings]]
key = "mirrors"
title = "Mirrors"
type = "hosts"
```

A request to any other host raises `term.http: '<host>' is not one of the extension's
hosts; list it in 'hosts' of manifest.toml`, and the app refuses a forge's `whoami` URL
outside them too. For an extension with the `network` permission the extension's page
of the settings lists these hosts.

### Logins

A `[logins]` table says that the extension passes the logins of a tool into sandboxes,
and where it reads them:

```toml
[logins]
title = "npm"
sources = ["~/.npmrc", "NPM_TOKEN", "NODE_AUTH_TOKEN"]
```

`title` names the tool and `sources` lists the files and variables the extension reads.
The Registry logins page under Accounts (Ctrl+, then Accounts, then Registry logins) lists every
such extension with its sources and the hosts its `term.secret` values go to.

## Errors and limits

If an extension raises an error, while loading or inside a handler or command,
scriptschnell-term reports the error and disables that extension. Its handlers and
commands are removed and the other extensions keep running.

Each call into an extension gets 500 ms of wall-clock time. Loading the file
counts as one call, and so does each handler or command run. When the budget
runs out, the VM raises `interrupted!` and the extension is disabled. Wrapping
the loop in `pcall` does not help, because `pcall`, `xpcall` and
`coroutine.resume` re-raise the error once the budget has expired. Keep
handlers short and leave slow work to the terminals you open.

`print` writes to scriptschnell-term's stderr, prefixed with `[ext:<name>]`. So do
`io.write` and `io.stderr:write`. `os.exit` does not quit the app. It counts as
an error and disables the extension.

## The `term` table

### Events

`term.on(event, fn)` registers a handler. Handlers run in load order, and
within one extension in the order they were registered. Each handler gets one
table. Its `event` field holds the event name, and paths arrive as strings.
Fields that have no value are `nil`.

| Event | Fields |
| --- | --- |
| `app_started` | none |
| `project_added` | `name`, `root`, `env`, `project_id` |
| `worktree_activated` | `project`, `project_id`, `path`, `branch` (nil when detached), `env` |
| `terminal_opened` | `id` (integer), `cwd` (may be nil) |
| `terminal_exited` | `id`, `code` (nil when the app has no exit code). The terminal's pane has already closed |
| `environment_ready` | `project`, `worktree`, `kind`, `env`, `host`, `fresh` |

`environment_ready` fires when a worktree's terminal environment comes up: a
devcontainer finished starting (`kind = "devcontainer"`), a sandbox was turned on
(`"sandbox"`), or a project on a VM, remote machine, SSH host or in an extension's
environment showed for the first time this run (`"vm"`, `"remote"`, `"ssh"`,
`"custom"`). `env` names the environment, such as the
container. `host` names the project's environment, the machine under a devcontainer or
sandbox: `"local"` when that is this machine. `fresh` is true only for a devcontainer
this start created, so it is empty. Handlers usually copy files in and install tools
with `term.copy` and `term.run`; both should be safe to repeat.

### Commands

`term.command{ name = "...", title = "...", run = fn }` adds an entry to the
command palette. `title` defaults to `name`. Calling it again with the same
name replaces your own command. A name that another extension already uses is
an error.

An optional `keys` field binds the command to a key sequence, written as in
`settings.json` (`"ctrl+alt+g"`, `"ctrl+b g"`). Give one string for every
keymap, or a table from keymap id to sequence, where `default` covers the
keymaps you do not name:

```lua
term.command{
  name = "git-helper.log",
  title = "Git: log",
  keys = { default = "ctrl+alt+g", tmux = "ctrl+b g" },
  run = function() term.open_terminal{ cmd = { "git", "log" } } end,
}
```

An extension's binding replaces the keymap's binding for the same keys. The
user's `keybindings` in `settings.json` replace both. When two extensions ask
for the same keys, the one loaded first keeps them and the other gets a
notice. A sequence scriptschnell-term cannot read is reported and skipped.

### Tasks

`term.task{ name, title, every, projects, run }` registers a recurring task, such as a
cleanup. The app calls `run` once per period, and once per project for a task with
`projects`. It saves the time of the last run in `state.json`, so a daily task runs once
a day across restarts. A task that has never run is due right away. Calling it again
with the same name replaces your own task.

- `name` is letters, digits, `-` and `_`. The task's id is `<extension>.<name>`.
- `title` names the task in the palette and defaults to `name`.
- `every` is seconds, or a count with `s`, `m`, `h`, `d` or `w`, such as `"30m"` or
  `"1d"`. The shortest period is 60 seconds.
- `projects` limits the task to projects. Each entry is a project name, such as
  `"cmdterm"`, or a path compared with the project's main worktree, such as
  `"~/work/*"` (`~` is this machine's home directory). `*` matches any run of
  characters, `/` included. Leave `projects` out for a task that belongs to no project.

`run` gets one table. `task` holds the id. For a project it also has `project`, `project_id`, `root`,
`env` and `worktrees`, a list of `{ path, branch, runtime }` (`branch` is nil when
detached, `runtime` is `host`, `devcontainer` or `sandbox`). The app waits with a project
until it has listed the project's worktrees.

`run` has the same 500 ms budget as a handler, so it only decides and queues: do the
work with `term.run`, which needs the `run` permission and runs in the worktree's
terminal environment. A worktree whose devcontainer is not running gives a notice
instead, so check `runtime` when that matters. A task that raises an error disables its
extension like a handler does; its run still counts, so it waits for its next period.

The palette lists every task as "Run task: <title>". It runs the task now for every
project it matches and starts its period again.

```lua
-- ~/.config/scriptschnell-term/extensions/cleanup/init.lua
-- manifest.toml: permissions = ["run"]
term.task{
  name = "prune",
  title = "Prune merged worktrees and old build output",
  every = "1d",
  projects = { "~/Documents/prog/*" },
  run = function(e)
    term.run{ cmd = { "git", "worktree", "prune" }, worktree = e.root }
    for _, w in ipairs(e.worktrees) do
      if w.runtime ~= "devcontainer" then
        term.run{ cmd = { "sh", "-c", "find target -maxdepth 2 -name incremental -mtime +7 -exec rm -rf {} +" }, worktree = w.path }
      end
    end
  end,
}
```

### Actions

The following functions queue a request. The app carries them out after the
current handler returns, in call order.

- `term.open_terminal{ cwd = "/path", cmd = { "prog", "arg" }, title = "..." }`.
  Every field is optional, and `term.open_terminal()` opens a default shell.
- `term.open_browser(url)`
- `term.notify(message [, level])`. The level is `"info"` (the default),
  `"warning"` or `"error"`.
- `term.send_text(text [, terminal_id])`. Without an id the text goes to the
  focused terminal.
- `term.switch_worktree(path)`
- `term.copy{ from = "/local/path", to = "~/dir", worktree = path, exclude = { "*.log" } }`
  copies a local file or directory into the directory `to` of the worktree's terminal
  environment (its devcontainer, VM, remote machine or SSH host), keeping its name.
  `to` may start with `~`. `worktree` defaults to the active one; `exclude` takes tar
  patterns. It runs in the background and reports failures as notices. A copy onto the
  same path of this machine is skipped.
- `term.run{ cmd = { "prog", "arg" }, worktree = path, cwd = "/dir" }` runs a command
  in the worktree's terminal environment in the background, from the worktree unless
  `cwd` says otherwise. A failure becomes a notice. Use `open_terminal` instead when the
  user should watch it. `host = true` runs it in the project's environment instead: the
  machine under the worktree's devcontainer or sandbox, which is this machine for a local
  project. It needs the `process` permission too, since it leaves the sandbox.
- `term.trust{ name = "my-dev-ca", path = "/local/ca.pem", worktree = path }` adds a PEM
  certificate of this machine to the system trust store of the worktree's environment: a
  devcontainer, VM, remote machine or SSH host. It knows the stores of Debian, Ubuntu and
  Alpine (`update-ca-certificates`), Fedora and RHEL (`update-ca-trust`) and Arch
  (`trust anchor`), and adds the certificate to `~/.pki/nssdb` for Chromium and Firefox
  when `certutil` is there. It runs as the environment's user and needs root or
  passwordless `sudo` there; most devcontainer images have one of them. `name` is
  letters, digits, `-`, `_` and `.` and names the file in the store, so a second call
  with the same name replaces it. Sandboxes and local projects are this machine, whose
  store the app does not change; the call fails for them. Needs the `trust` permission.

`term.copy`, `term.run` and `term.trust` take an optional `done` function. The app
calls it once the request finished, with `{ ok, code, stdout, stderr, error }`: `ok` is
true when the command exited with 0 or the copy or trust worked, `code` is the exit code
(nil when the command did not run), `stdout` and `stderr` hold up to 64 KiB of output,
and `error` says why a request that is not one command failed. For `term.trust`,
`stdout` is the path of the environment's CA bundle, such as
`/etc/ssl/certs/ca-certificates.crt`. A request with `done` reports nothing as a notice;
its function decides. It runs with the same 500 ms budget as a handler, so chain the
next step from it instead of waiting:

```lua
term.run{ cmd = { "sh", "-c", "command -v dotnet" }, worktree = w, done = function(r)
  if not r.ok then return end -- no .NET SDK in there
  term.run{ cmd = { "dotnet", "--version" }, host = true, worktree = w, done = function(v)
    term.notify("this machine has .NET " .. v.stdout)
  end }
end }
```

A `done` function of an extension that is reloaded or disabled before the request
finishes is not called.

### Sandboxes

`term.sandbox{ hosts, writable, hidden, redirects, vars, files, guards, blocked,
unblocked, blocklists }` adds to
what sandboxed terminals may reach, on top of the user's sandbox settings.
Every field is optional, and paths may start with `~/`.

- `hosts` are names such as `registry.npmjs.org`, or `*.example.com` for every
  subdomain. They count only when the user limits the network to allowed hosts.
- `writable` paths are bound read-write. Missing ones are skipped.
- `hidden` paths are hidden like the user's hidden paths.
- `redirects` is a list of `{ host = "~/.cache/my-ext/npm", sandbox = "~/.npm" }`: the
  host directory, created if needed, stands in for the sandbox path, which must
  exist. Only Linux applies redirects.

- `vars` is a table from variable name to value, set inside sandboxes. Use it to hand
  over a fake token in place of one the host's environment holds.
- `files` is a list of `{ sandbox = "~/.npmrc", content = "..." }`: the content stands
  in for the sandbox file, read-only. The file must exist on this machine. Linux binds
  the content over it; macOS cannot remount paths, so it hides the real file instead.
- `guards` is a list of `{ name, title, hosts, requests }` for requests that publish.
  See below.
- `blocked`, `unblocked` and `blocklists` refuse hosts. See Blocklists below.

Calling it again replaces your extension's rules, and they go away when the extension
is disabled or fails. The user's hidden paths win: a writable path or redirect that
overlaps one is dropped. A file may stand in for a hidden path, because its content
comes from you. Path, variable and file rules apply to projects on this machine only.
New hosts, secrets and guards reach the proxy right away; paths, variables and files
apply to terminals started afterwards.

```lua
term.sandbox{
  hosts = { "registry.npmjs.org" },
  writable = { "~/.npm" },
}
```

#### Credentials

`term.secret{ value = token, hosts = { "registry.npmjs.org" } }` returns a fake for
`token`. Put the fake into the sandbox through `vars` or `files`; the real value never
enters it. scriptschnell-term's proxy replaces the fake with the real value in requests to
the listed hosts, in the headers that carry credentials, also inside `Basic` credentials:
`Authorization`, `Cookie`, and headers whose name contains `auth`, `token`, `key`,
`secret`, `pass`, `credential` or `session`, such as `PRIVATE-TOKEN` or `X-NuGet-ApiKey`.
Other headers and the request target (path and query) keep the fake, because servers
echo them back in redirects, error pages and CORS headers, so a token that an API takes
in the query does not work. The proxy does not pass `TRACE` requests to these hosts. A fake sent anywhere
else stays a fake, and so do fakes sent to a server whose certificate the proxy could
not verify, and fakes sent over plain HTTP to a server outside this machine and the
local network (private and unique local addresses). The same value gives the same fake
for the whole run, and a later call adds its hosts. `hosts` is required and takes the
same patterns as `hosts` above, but not `*`.

The fake keeps the shape of the value where it can: its length, a prefix such as
`npm_` or `ghp_`, and which characters are digits or upper or lower case. Values with
fewer than 16 letters and digits get 32 random ones instead.

`name` ties the fake to a name instead of the value: a later call with the same name and
a new value returns the same fake, and the proxy sends the new value from then on. Use it
for tokens this machine refreshes, such as OAuth access tokens, so that files a sandbox
already read keep working. Names are per extension.

```lua
local fake = term.secret{ value = os.getenv("NPM_TOKEN"), hosts = { "registry.npmjs.org" } }
term.sandbox{ vars = { NPM_TOKEN = fake } }
```

#### Guards

A guard holds back requests that publish, such as `git push` or `cargo publish`:

```lua
term.sandbox{
  guards = {
    {
      name = "publish",
      title = "publish a crate",
      hosts = { "crates.io" },
      requests = { "PUT /api/v1/crates/new" },
    },
  },
}
```

- `name` is letters, digits, `-` and `_`. The guard's id is `<extension>.<name>`.
- `title` finishes the sentence "A sandbox wants to ...". It defaults to `name`.
- `hosts` takes host patterns, or `*` for every host.
- `requests` are `METHOD TARGET` patterns. `*` matches any run of characters, and a
  METHOD of `*` any method. TARGET is the path with its query, such as
  `GET */info/refs?*service=git-receive-pack*`. A TARGET without `?` also matches the
  path followed by any query. Both sides are compared without case, after decoding
  `%XX` escapes and resolving `.`, `..` and repeated slashes.
- A pattern may add a BODY after a second space, such as
  `POST */graphql *mutation*createPullRequest*`. The request then matches only when
  its body matches too, compared without case after decoding `%XX` and JSON's
  `\uXXXX` escapes. The proxy holds such a request until its body is in, up to 1 MiB.
  A body it cannot read, because it is larger, compressed or chunked, counts as
  matching. The dialog shows the start of a body it read.
- `kind` is `create`, `comment` or `merge` for a guard on change requests. See
  Guards on change requests under Forges.

The proxy has guards of its own, with ids starting with `cmdterm.`: `git push` and
`hg push` to any host, container image pushes to any registry, and publishing to
registry.npmjs.org, crates.io, nuget.org, PyPI and rubygems.org. Add guards for other
hosts, such as a company registry.

The Extensions page of the settings lists every guard with Ask, Block and Allow: an
extension's guards on its page, and a built-in guard for npm, Cargo, NuGet, PyPI or
`git push` on the page of the shipped extension for it while that is installed. Modes are saved
under `sandbox.guards` in `settings.json`. Ask is the default: a dialog shows the
request with Block, Allow and Allow in this session, and, for a request from a local
project, Always allow in this project. Allow holds for one minute for the guard, the
host and what the dialog showed: the same repository, package or image (the path
without its query, up to `/info/refs`, `/git-receive-pack`, `/blobs/` or `/manifests/`),
and the same body when the guard read one. A push that takes two requests asks once,
and a push to another repository asks again. A request that several guards match, such
as one GraphQL request that opens and merges a pull request, passes only when each of
them lets it. Allow in this session holds for
the guard and host in that project until the app quits. A request nobody answers is
refused after two minutes.

#### Blocklists

The proxy refuses blocked hosts with `403 Forbidden`, even when the allowed hosts or the
whole network would let them through:

```lua
term.sandbox{
  blocklists = {
    {
      name = "ads",
      urls = {
        "https://raw.githubusercontent.com/hagezi/dns-blocklists/main/wildcard/pro-onlydomains.txt",
        "https://codeberg.org/hagezi/mirror2/raw/branch/main/dns-blocklists/wildcard/pro-onlydomains.txt",
      },
      max_age = 7 * 24 * 60 * 60,
    },
  },
  blocked = { "telemetry.example.com" },
  unblocked = { "sentry.io" },
}
```

- `blocked` names hosts to refuse. `example.com` blocks it and every subdomain,
  `*.example.com` only the subdomains.
- `unblocked` names hosts no block applies to, in the same form. It wins over every
  list and every extension's `blocked`.
- `blocklists` is a list of `{ name, urls, max_age }` and needs the `network`
  permission. `name` is letters, digits, `-` and `_`; refusals name the list
  `<extension>.<name>`. `urls` are `https://` URLs of the same list, tried in order, so
  the ones after the first are mirrors. Each URL must be on one of the extension's hosts
  (see Hosts under Manifest and permissions). `max_age` is how many seconds the cached
  copy lasts before the app downloads it again, a week by default.

A list holds one entry per line: plain domains, wildcards (`*.ads.example`), hosts
files (`0.0.0.0 ads.example`) or Adblock domain rules (`||ads.example^`). Comments
(`#`, `!`), exceptions (`@@`) and rules with options (`$`) are skipped. Every entry blocks
its subdomains too. A download without a single domain, such as an error page, counts
as failed.

The app downloads a list once a sandbox uses the proxy, keeps it in `blocklists/` in
the cache directory and checks its age every hour. When no URL answers, the
cached copy keeps blocking, and a notice names the list. Blocked hosts alone make
sandboxes use the proxy, also with the whole network.

#### How the proxy applies them

Secrets and guards need scriptschnell-term's proxy to read the requests. Sandboxes on this
machine use it whenever the user limits the network to allowed hosts, and also when
there are secrets or guards that do not allow, which with the proxy's own guards is
the default. A sandbox with the whole
network then keeps it, and the proxy variables send HTTP clients through the proxy;
programs that ignore them go around it and reach the hosts with fakes only. Only a
restricted network enforces guards against every program.

For hosts with secrets or guards the proxy ends TLS itself, with a CA the sandbox
trusts through `SSL_CERT_FILE` and the other usual variables, and speaks HTTP/1.1 to
them. Plain TCP tunnels to such a host, such as SSH, are refused. A guard on `*`
therefore makes the proxy decrypt every HTTPS connection of sandboxes, and so do the
proxy's own `git push`, `hg push` and container guards until the user allows them.

The shipped `npm`, `cargo`, `nuget`, `pypi`, `git-credentials` and `forge-clis` extensions
use all of this for registry and forge logins, and the shipped `blocklist` extension blocks HaGeZi's list of
ad, tracker and malware hosts.

### Data

`term.store` keeps data across runs and reloads, separate for every extension. It
needs the `storage` permission:

- `term.store.get(key)` returns the value, or nil.
- `term.store.set(key, value)` stores a value; nil removes the key.
- `term.store.delete(key)` removes it.
- `term.store.keys(prefix)` lists the keys that start with `prefix`, sorted. Without
  a prefix it lists all of them.

Keys are 1 to 200 letters, digits, `.`, `_`, `-`, `/` and `:`, such as
`doc/notes/intro`. Values are what JSON holds: booleans, numbers, strings and
tables of them. A table with the keys `1..n` is a list, an empty table an empty
list, any other a table with string keys. Functions and tables that contain
themselves are errors. One value may take 4 MiB as JSON, and a store holds up to
20000 keys.

The store reads its directory once, on first use, and answers from memory. A
write changes memory at once and reaches the disk in the background, one file per
key under `extension-data/<name>` of the data directory. Reads and writes never wait
for the disk, so a handler can use them freely.

#### Project data

`term.project_store(project_id)` returns a store with the same `get`, `set`, `delete`
and `keys` that holds only what the extension keeps for one project, and `id`, the
project's id. It needs the `project_storage` permission, which is separate from
`storage`: an extension with only `project_storage` cannot keep anything outside a
project.

```lua
term.on("worktree_activated", function(e)
  local data = term.project_store(e.project_id)
  data.set("visits", (data.get("visits") or 0) + 1)
end)
```

`project_id` names the project: the `project_id` field of `term.context()`, of the
`project_added` and `worktree_activated` events, of a task's `run` table and of a tool's
caller. Without one it is the active project's, and with no project active the call
raises an error. The id is the project's name and a hash of its environment and root,
such as `cmdterm-3f1c09a2b4d5e6f7`, so it stays the same across restarts and differs
for two projects of the same name. Ids are letters, digits, `-` and `_`.

The data of a project stays on this machine, also for a project on an SSH host or a
remote machine, under `project-data/<project id>/<name>` of the data directory, one
directory per project, so removing that directory forgets everything extensions kept
for it.

`term.json_encode(value)` and `term.json_decode(text)` convert between the same
values and JSON text. `json_decode` returns nil and a message for invalid text.

### Views

A view is Markdown the app shows in a panel, laid out like a Markdown file in the
file panel, with code highlighting, tables, Mermaid diagrams and math.

```lua
term.view{
  name = "page",
  render = function(key)
    return { title = "Page " .. key, markdown = "# " .. key .. "\n\nSome text." }
  end,
}
term.open_view("page", "intro")
```

- `render(key)` returns `{ title, markdown }`, a Markdown string, or nil when there
  is nothing under `key` anymore. The app calls it when a panel opens, after
  `term.refresh_view`, after a reload, and when the app starts with a panel of the
  view saved in the worktree's layout.
- `term.open_view(name, key)` shows the view in the active worktree: in the panel
  that shows it with that key already, else in a new tab. `key` defaults to `""`.
  `name` may be `<extension>.<name>` to open another extension's view.
- `term.refresh_view(name, key)` renders the open panels of the view again, every
  one or only the one of `key`. Call it after the data behind it changed.

### Tools for agents

`term.tool{ name, description, input, run }` offers agents a tool. Agents in the
app's terminals call it through the control socket (`GET /v1/tools`,
`POST /v1/tools/<name>`) and through the MCP server the Agents page installs
(`scriptschnell-term-browser`), next to the debug browser's tools. It needs the
`agents` permission.

- `name` is letters, digits, `-` and `_`, unique across extensions. Start it with
  your extension's name, such as `knowledge_search`.
- `description` tells the agent what the tool does.
- `input` is the JSON schema of the arguments as a Lua table. Its `type` defaults
  to `"object"`.
- `run(args, caller)` gets the arguments as a table and the caller as
  `{ project, project_id, worktree, environment, agent }`, and returns what the agent gets:
  a string, or a table that the app sends as JSON. Return `nil, message` for an
  error the agent should see. Raising an error disables the extension like any
  other error, so check the arguments and return `nil, message` instead.

`run` has the same 500 ms budget as a handler, and the app calls it on its UI
thread, so keep it to reading and changing data.

```lua
term.tool{
  name = "notes_add",
  description = "Adds a note",
  input = { properties = { text = { type = "string" } }, required = { "text" } },
  run = function(args, caller)
    if args.text == "" then return nil, "text is empty" end
    -- manifest.toml: permissions = ["agents", "project_storage"]
    if not caller.project_id then return nil, "this terminal belongs to no project" end
    local data = term.project_store(caller.project_id)
    local notes = data.get("notes") or {}
    notes[#notes + 1] = { text = args.text, by = caller.agent }
    data.set("notes", notes)
    return { count = #notes }
  end,
}
```

### Agents

`term.agent(spec)` adds a coding agent the app does not know itself and treats it like
the ones it does: the Agents page (Ctrl+, then Agents) lists it in every environment and
installs its integration with the user's consent, the palette offers to start it, its
terminal shows when it is busy or waits for the user, saved sessions resume after a
restart, `control launch` and `POST /v1/launch` start it by name, sandboxes let it keep
its state, and update checks look for its npm packages. It needs the `agents`
permission; the `sandbox` field also needs `sandbox`.

```lua
-- manifest.toml: permissions = ["agents", "sandbox"]
term.agent{
  name = "my-agent",                    -- what its hooks pass to `agent-state context`
  title = "My Agent",
  program = "my-agent",
  config_dir = ".my-agent",             -- missing there: the agent is not installed
  skills_dir = ".my-agent/skills",
  integration = {
    file = ".my-agent/settings.json",
    hooks = {
      { event = "SessionStart", args = "context my-agent" },
      { event = "UserPromptSubmit", args = "deliver UserPromptSubmit" },
      { event = "PermissionRequest", args = "permission -" },
      { event = "Stop", args = "stop" },
      { event = "SessionEnd", args = "end" },
    },
  },
  mcp = { file = ".my-agent/mcp.json" },
  supports = { "state", "session", "resume", "messages", "prompt", "paste" },
  resume = { "--resume" },
  prompt = { "--prompt", "{prompt}" },
  sessions = { dir = ".my-agent/sessions" },
  sandbox = {
    writable = { ".my-agent" },
    read_only = { ".my-agent/settings.json", ".my-agent/skills" },
  },
  npm = { "@me/my-agent" },
}
```

Paths are relative to the home directory of the machine the agent runs on and may start
with `~/`. They hold letters, digits and `._-/+@`, and no `..`.

- `name` is a lowercase letter, then up to 31 lowercase letters, digits and `-`. The
  agent's hooks pass it to the helper's `context` and `session` commands, and the app
  finds the agent of a saved session by it. A name or program of an agent the app knows
  itself (`claude`, `codex`, `cursor` and so on) is refused, and so is one another
  extension registered first.
- `program` starts the agent, and the app types it into shells: letters, digits and
  `_.+-`, not starting with `-`. It defaults to `name`.
- `config_dir` is the agent's configuration directory. The Agents page shows "not
  found" where it is missing.
- `skills_dir` is where the app writes the `configure-scriptschnell-term` and
  `scriptschnell-term-sandbox` skills. Leave it out for an agent without skills.
- `integration` says how the agent reports what it does. Every form runs the helper
  `~/.local/share/cmdterm/agent-state`, which sets the terminal's state:
  - `hooks` adds hook entries to the JSON file `file`, the way Claude Code reads them:
    `hooks.<event>` is a list of `{ matcher, hooks = { { type = "command", command,
    timeout } } }`. The user's own hooks stay. `args` are the helper's arguments:
    `busy`, `done`, `input MESSAGE`, `permission MESSAGE` and `clear` set the state,
    and a MESSAGE of `-` is read from the `message` or `tool_name` field of the hook's
    JSON on stdin. `context NAME [SHAPE]` at session start names the session and joins
    the agent list, `deliver EVENT [SHAPE]` hands over other agents' messages, `stop`
    keeps the turn going while messages wait, and `end` leaves the list. SHAPE is how
    the agent reads hook output: Claude Code's `hookSpecificOutput` by default, `flat`
    for a top-level `additionalContext`, `text` for plain text. `args` hold letters,
    digits, spaces, `_.-` and single-quoted text. `timeout_unit` is 1000 for an agent
    that reads hook timeouts in milliseconds.
  - `content` writes `file` whole, such as a plugin the agent loads. `{helper}`,
    `{control}` and `{browser_mcp}` in it become the paths of the helpers relative to
    `$HOME`, such as `.local/share/cmdterm/agent-state`.
  - Without `integration` the agent reports its state itself, through the
    `cmdterm_agent` user variable (OSC 1337 SetUserVar) or OSC 7501.
- `mcp` registers the debug browser's MCP server, and with it the tools of
  `term.tool`, in the JSON file `file` under `key` (`mcpServers` by default). `style`
  is the shape of the entry: `stdio` (`type`, `command`, `args`, `env`), `command`
  (`command`, `args`) or `local` (`type = "local"`, `command` as a list, `enabled`).
- `resume` holds the arguments after `program` that open a session again, in front of
  its id, such as `{ "--resume" }`. The app types the command into the terminal's shell
  when it starts again. Leave it out for an agent whose sessions do not resume.
  `session_at_start = false` says the agent's session start hook runs only with the
  first prompt, as Codex's does; the app then waits for its screen instead of its hook
  before it types into a resumed session.
- `supports` lists what the agent can do with the app, and the app relies on nothing
  it leaves out. An extension opts in to everything its agent does and to nothing it
  does not; `term.agent` fails on a capability without what makes it work, and on a
  field or hook that makes one work without it in `supports`:

  | Capability | The agent | Needs |
  | --- | --- | --- |
  | `state` | reports whether it works, waits for the user or is done | with `hooks`, one that runs `busy`, `done` or `stop` |
  | `session` | names itself and its session in `cmdterm_session` through `context NAME` or `session NAME [ID]` | with `hooks`, a `context` or `session` hook |
  | `resume` | opens a saved session again after a restart | `resume`, `session` |
  | `messages` | joins the app's agent list and reads messages from other agents | `state`, `session`; with `hooks`, a `context` and a `deliver` hook |
  | `prompt` | starts its TUI with a first prompt in its arguments | `prompt` |
  | `paste` | takes a first prompt typed once it named its session at start | `session`, not `session_at_start = false` |

  Hooks that run `context` or `session` need `session`, `deliver` needs `messages`,
  and the state commands need `state`. An `integration` with `content` or none reports
  on its own, so the app takes its claims as made. Without `messages` the agent cannot
  join the agent list, not even with `control join`. Without `resume` its sessions are
  not saved for a restart.
- `prompt` holds the arguments after `program` that start the agent's TUI with a first
  prompt: up to 8 words of letters, digits and `_.+-=:/`, and `{prompt}` once, where the
  prompt goes. `control agents --prompt TEXT NAME`, `control worktree` and
  `POST /v1/agents` use it, or type the prompt for an agent with `paste` only.
- `paste` makes the app type a first prompt into the agent once it is ready, the way it
  does for Claude Code and Kimi Code. The agent is ready when it named its session after
  it started (its `context` hook ran, or it called the helper's `session` command) and
  its screen then stayed still for half a second. Call that only once the TUI reads
  input; a prompt typed earlier is lost. An agent that is not ready within a minute gets
  nothing.
- `sessions` says where the agent keeps its sessions, so the app can check before it
  resumes one that the session still exists. `dir` is the directory. `by = "name"`
  (the default) finds a session by a file or directory there whose name holds its id,
  `by = "contents"` by a file there that contains it. When `dir` exists but holds no
  trace of the session, the app starts the agent without it, as a new session, and
  types no prompt to continue. Without `sessions`, or when `dir` is missing, the app
  resumes without checking.
- `sandbox` lists what the agent may change in sandboxes. `writable` paths are bound
  read-write, unless the user hid them; `read_only` entries inside them stay
  read-only, so a sandbox cannot change hooks or plugins the agent runs outside it.
  `synced` files are settings the agent changes itself: a bubblewrap sandbox gets a copy
  of each, and the app brings the copy's changes to this machine after asking.
  `guarded` maps a synced file to the key paths whose changes ask, such as
  `{ [".my-agent/state.json"] = { { "projects", "*", "trusted" } } }`; changes elsewhere
  in that file come over at once, and a synced file without an entry asks for every
  change. `home_var = { "MY_AGENT_HOME", ".my-agent" }` is for an agent that renames new
  settings over old ones, which a file bound in place does not allow: its sandbox gets
  a directory of its own, named by the variable. `login_vars = { "MY_AGENT_API_KEY" }`
  names the variables the agent logs in with. Sandboxes on this machine and on others
  remove every inherited variable whose name looks like a credential (see
  `sandbox.pass_env` in `CONFIGURATION.md`), but keep these, so the agent can log in
  inside. Every command in every sandbox sees them, not only the agent, so name only
  what the agent needs. They also need the `env` permission, since the sandbox gets
  their values as `os.getenv` would.
- `npm` names the packages the agent is published as. The update check offers to
  update a global npm or mise install of one.

The agent's local configuration for scriptschnell-term is its `integration`, the skills
in `skills_dir` and the MCP server entry in `mcp`. The app installs nothing until the user
turns the agent on on the Agents page, and takes out what it added when the user turns it
off. A later version of the extension
that changes the integration shows it as outdated there, and the update check offers to
install the new one. When the extension is turned off or fails, its agent leaves the
Agents page and the palette; what it installed stays, and saved sessions of it no
longer resume. `scriptschnell-term fix` loads no extensions, so it knows only the agents
the app knows itself.

### HTTP

`term.http{ url, method, headers, body, account, forge, done }` sends a request in the
background. It needs the `network` permission.

- `url` starts with `https://` or `http://` and must be on one of the extension's hosts
  (see Hosts under Manifest and permissions). `method` is `GET` (the default), `POST` or
  `HEAD`. `headers` is a table from name to value, at most 32.
- `done(res)` gets `{ ok, status, headers, body, error }`: `ok` is true for a 2xx
  answer, `status` is 0 when no answer came, `headers` has lowercase names, and `body`
  holds up to 4 MiB.
- `account` names an account of one of the extension's forges, as `check` got it, and
  `forge` that forge when the extension has more than one. The app adds the token of that account, if it has
  one, as the forge's `auth` says. The token goes only to the account's host and its
  subdomains; a request to another host fails without being sent. The extension never
  sees the token.

At most 16 requests run at once across extensions.

### Forges

`term.forge{ name, title, accounts, aliases, auth, env, scopes, token_url, whoami, check, discussion, guards }`
registers a forge. The rail's Agents tab uses forges to tell whether the branch of a
finished agent has a change request and whether it is merged. It needs the `network`
permission.

The Accounts page of the settings (Ctrl+, then Accounts) lists every forge with its
accounts. The user adds and removes accounts and signs in to them there, not on the
extension's page. The app keeps each forge's list under `forge_hosts` in
`settings.json`; a forge without an entry has the accounts in `accounts.default`.

- `accounts = { default, example, hint }` describes the accounts the user signs in to,
  each with its own token. `default` lists the accounts before the user changes them,
  `example` is an account the add field shows, such as `github.com/acme`, and `hint`
  says under that field what an account is. The add field refuses an entry that is not
  a host or `host/path`. The old `hosts` and `setting` fields raise an error. An account
  is a host, such as `github.com`, or a host and a path, such as `github.com/acme`, for
  the repositories under that owner, group or organization. A remote belongs to the
  account on its host with the longest path its repository path starts with, whole
  segments and ignoring case: with `github.com` and `github.com/acme`, `acme/app` takes
  `github.com/acme` and `octo/app` takes `github.com`. A remote that matches no account
  does not belong to the forge.
- `aliases` maps other remote hosts to an account's host, such as
  `["ssh.dev.azure.com"] = "dev.azure.com"`. A pattern may end in a path the app cuts
  off the repository path, and the target may add a path in front of it, where `*`
  stands for the label a `*.domain` pattern matched:
  `["ssh.dev.azure.com/v3"] = "dev.azure.com"` turns `v3/org/project/repo` into
  `org/project/repo`, and `["*.visualstudio.com"] = "dev.azure.com/*"` turns
  `org.visualstudio.com` with `project/_git/repo` into `org/project/_git/repo`. The
  rewritten path then picks the account; the `path` `check` gets stays the remote's.
- `auth = { header, value }` says how requests carry the token: `value` holds `{token}`,
  `{basic}` for the Base64 of `:token`, or `{base64}` for the Base64 of the token as the
  user typed it, such as an `email:token` login. The default is
  `Authorization: Bearer {token}`.
- `env` lists variables the app takes a token from for the accounts on the host of the
  first account when the user has not signed in to them. The app reads them; the extension needs no `env` permission.
- `scopes` is shown under the token field.
- `token_url(host, account)` returns the page where the user creates a token for
  `account` on `host`. Create token on the Accounts page opens it in the system's
  browser.
- `whoami(host, account)` returns a URL that answers with JSON about the token's owner, and the
  dotted field that names them, such as `"login"`. The URL must be on one of the
  extension's hosts. Sign in on the Accounts page sends the token there; a
  2xx answer keeps the token in the system's secret storage (`secret-tool` or the macOS
  keychain) and the user name under `forge_accounts` in `settings.json`. Without a
  `whoami` the token is kept without a check.
- `check(items)` gets a list of `{ account, host, path, remote, branches, signed_in }`,
  one per repository, with every branch the app wants to know about. `account` is the
  account the repository belongs to, `host` the remote's own host. Build URLs from the
  account's host, `item.account:match("^[^/]+")`, and send them with
  `account = item.account`, so each repository gets its own account's token. Send as few
  requests as the forge allows, such as one GraphQL query for all of them, and answer
  with `term.forge_report`.

The app calls `check` only for worktrees whose agent finished and whose branch is not
known to be merged, at most every three minutes per forge and 10 seconds after an agent
finishes, never more than once in 30 seconds. When every such branch of a repository has
a change request already, the app first runs `git ls-remote` for the default branch and
those branches. While none of them moved, `check` does not get the repository, for up to
30 minutes.

`term.forge_report(name, results, opts)` tells the app what `check` found. Each result
is `{ account, path, branch, state, number, url, title }`, where `account`, `path` and
`branch` are those `check` got and `state` is `open`, `merged`, `closed` or `none`.
`opts.retry_after` pauses the forge's checks for that many seconds, such as after a rate
limit answer.

```lua
-- manifest.toml: permissions = ["network"]
term.forge{
  name = "hub",
  -- One token for git.example.com, another for the repositories under ops/.
  -- The user can change them on the Accounts page.
  accounts = {
    default = { "git.example.com", "git.example.com/ops" },
    example = "git.example.com/team",
    hint = "A host, or a host and a group for the repositories under it.",
  },
  token_url = function(host) return "https://" .. host .. "/settings/tokens" end,
  whoami = function(host) return "https://" .. host .. "/api/user", "login" end,
  check = function(items)
    for _, item in ipairs(items) do
      for _, branch in ipairs(item.branches) do
        term.http{
          url = ("https://%s/api/repos/%s/pulls?head=%s"):format(item.account:match("^[^/]+"), item.path, branch),
          account = item.account,
          done = function(res)
            local pulls = res.ok and term.json_decode(res.body) or {}
            local pull = pulls[1]
            term.forge_report("hub", { {
              account = item.account, path = item.path, branch = branch,
              state = pull and (pull.merged and "merged" or pull.state) or "none",
              number = pull and pull.number, url = pull and pull.html_url,
            } })
          end,
        }
      end
    end
  end,
}
```

#### Guards on change requests

`guards` lists guards like the ones of `term.sandbox`, for requests that open, comment
on or merge a change request, so sandboxed agents ask first:

```lua
term.forge{
  -- ...
  guards = {
    {
      name = "merge",
      kind = "merge",
      title = "merge a pull request",
      requests = {
        "PUT */repos/*/pulls/*/merge",
        "POST */graphql *mutation*mergePullRequest*",
      },
    },
  },
}
```

- `hosts` may be left out. The app adds the host of every account of the forge, its
  subdomains (`api.github.com` for `github.com`) and the hosts of `aliases`, and
  follows the accounts when the user changes them on the Accounts page.
- `kind` is `create` (opening one), `comment` (comments and reviews) or `merge`
  (merging or completing one). The Extensions page of the app itself has one mode per
  kind for every forge, saved under `sandbox.change_requests` in `settings.json`. A
  guard without a mode of its own on its extension's page follows the mode of its
  kind.
- `guards` needs the `guards` permission. Without it the forge registers without its
  guards, so its checks and discussions keep working.

The shipped `github`, `gitlab`, `bitbucket`, `forgejo`, `gitea` and `azure-devops`
extensions guard opening, commenting on and reviewing, and merging change requests
through their REST APIs, and GitHub's and GitLab's GraphQL mutations.

#### Discussions

A forge with a `discussion(request)` function shows a change request's status and
comments on the Change request page of the explore panel (the History and Review panel):
a button beside an agent's row in the Agents tab opens it, and `v` or a click on the
page's tab switches to it in any explore panel. `request` is
`{ id, account, host, path, remote, branch, number, signed_in }`; answer with
`term.forge_discussion(name, request.id, discussion)`, or
`term.forge_discussion(name, request.id, nil, error)` when the forge could not be read.
Reloading the page (`r` or F5) asks again.

`discussion` is
`{ title, url, state, author, body, draft, target, conflicts, reviews, checks, pushes,
comments, threads }`:

- `draft` and `conflicts` are booleans; `target` is the branch it merges into.
- `reviews` are `{ author, state }`, each reviewer's latest verdict, with `state` one of
  `approved`, `changes`, `commented` or `pending` (asked, no verdict yet).
- `checks` are the CI pipelines, jobs and status checks on the head: `{ name, state, url }`
  with `state` one of `success`, `failed`, `running`, `pending`, `skipped` or `canceled`.
  Without checks the page shows the pipeline the built-in GitLab, Azure DevOps and
  Forgejo client found for the branch, if any.
- `comments` are `{ author, body, at }` on the change request as a whole, with `at` in
  ISO 8601.
- `threads` are comments on code: `{ path, line, side, commit, hunk, resolved, outdated,
  comments }`. `line` is the line in `commit`'s version of the file, `side = "old"` marks
  a removed line of the base, and `hunk` is the diff hunk whose last line is the
  commented one.
- `pushes` are the heads the forge saw: `{ commit, at, before, force }`.

Give the commit and line a comment was written on, not where the forge shows it now. The
app places every thread on the worktree's head itself, so force pushes look the same on
every forge: with the commit in the repository, `git diff -M` from it to `HEAD` carries
the line through renames and edits; without it, the last line of `hunk` is looked up in
the file. Each thread says whether its line is unchanged, moved, changed or gone. The
pushes and the branch's reflog make one timeline, and a push that rewrote the branch
shows `git range-diff`'s summary. The page lists its sections on the left (overview,
checks, pushes, conversation and each thread, open ones first) and shows them on the
right. It is read-only; replies go through the forge's
page, which the title links to.

The shipped `github`, `gitlab`, `bitbucket`, `forgejo`, `gitea` and `azure-devops`
extensions are forges, all with discussions.

### Environments

`term.environment(spec)` registers a scheme for project locations in a remote coding
environment the app does not know itself, such as a Kubernetes pod, a Coder workspace or
a cloud VM. The shipped `kubernetes` and `coder` extensions are two examples. The extension names a program of this machine that runs a shell script in
one of the environment's targets; the app runs every command of a project there through
it, the same way it runs commands over SSH: VCS queries, terminals, `term.run`,
`term.copy` and `term.trust`. It needs the `environments` permission.

```lua
-- manifest.toml: permissions = ["environments"]
term.environment{
  name = "k8s",                -- locations are k8s://pod/path
  title = "Kubernetes pods",
  program = "kubectl",
  args = { "exec", "-i", "{target}", "--", "sh", "-c", "{script}" },
  pty_args = { "exec", "-it", "{target}", "--", "sh", "-c", "{script}" },
}
```

- `name` is the scheme: a lowercase letter, then up to 31 lowercase letters, digits and
  `-`. Schemes the app parses itself (`ssh`, `cmdterm`, `vm`, `tailscale`, `netbird`,
  `nebula`, `git`, `file`, `http`, `https`, `local`, `wsl`) are refused, and so is one
  another extension registered first.
- `program` runs on this machine with the app's environment variables.
- `args` are its arguments for commands without a terminal. `{target}` anywhere in an
  argument becomes the target of the location. An argument that is exactly `{script}`
  becomes the POSIX shell script that changes to the working directory, sets the
  variables and runs the command; without one the script goes last, as for `ssh`.
  `args` must hold `{target}`. Pass stdin through, since `term.copy` streams a tar
  archive into the script.
- `pty_args` are the arguments for terminals, the same way; `args` by default. Ask the
  program for a terminal here, such as `-t`.

A location is `<name>://<target>/<path>`. The target is letters, digits and `._@:+~-`
and never starts with `-`, so a location cannot pass an option to the program. The path
is absolute, or relative to the home there when it starts with `~`. The project stores
only the scheme and the target, not the program: while no loaded extension provides the
scheme, because the extension is off, failed or lost the permission, every command of
the project fails with "no extension provides <name>:// environments" and nothing runs.
`term.context().env` and the events name such a project's environment by its target,
and `environment_ready` fires with `kind = "custom"` the first time it shows.

Terminals there do not keep running across restarts, and the app's control socket does
not reach them, since the app cannot forward Unix sockets through an arbitrary program.

### State

- `term.context()` returns `{ project, project_id, worktree, branch, env, runtime }` for the
  active project. `runtime` is `host`, `devcontainer` or `sandbox`. Fields are `nil`
  when nothing is selected.
- `term.version` is the scriptschnell-term version string.
- `term.os` is this machine's operating system: `"linux"`, `"macos"` or `"windows"`.
  Paths of this machine use `\` on Windows; `package.config:sub(1, 1)` is the separator.
- `term.cache_dir()` returns the extension's own directory in the app's cache directory
  (`extensions/<name>`), creating it if needed. Keep files there that the app may delete,
  such as an export a later step copies into an environment.

The global was called `cmdterm` before the app was renamed. That name still works and
points to the same table.

## Example

`~/.config/scriptschnell-term/extensions/cargo-helper/manifest.toml`:

```toml
description = "Test runner and docs for Rust worktrees"
permissions = ["filesystem", "terminal", "browser"]
```

`~/.config/scriptschnell-term/extensions/cargo-helper/init.lua`:

```lua
local function in_rust_project(dir)
  local f = io.open(dir .. "/Cargo.toml")
  if f then f:close() return true end
  return false
end

-- Open a test runner next to every Rust worktree you switch to.
term.on("worktree_activated", function(e)
  if in_rust_project(e.path) then
    term.open_terminal{
      cwd = e.path,
      cmd = { "cargo", "watch", "-x", "test" },
      title = "tests: " .. (e.branch or "detached"),
    }
  end
end)

term.on("terminal_exited", function(e)
  if e.code and e.code ~= 0 then
    term.notify("terminal " .. e.id .. " exited with " .. e.code, "warning")
  end
end)

term.command{
  name = "cargo-helper.docs",
  title = "Cargo: open docs.rs for this crate",
  run = function()
    local ctx = term.context()
    if not ctx.project then
      term.notify("no project selected", "error")
      return
    end
    term.open_browser("https://docs.rs/" .. ctx.project)
  end,
}
```
