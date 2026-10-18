# Devcontainers

Starts a worktree's devcontainer and returns a `ContainerTarget`, so scriptschnell-term can
run the worktree's terminals inside the container while VCS commands stay on the
host. The host is any `Environment`: this machine, an SSH host, or a scriptschnell-term
remote machine. Every file read and engine call runs as a command on that host,
so a remote worktree works the same as a local one.

## Entry points

- `find(host, worktree)` looks for `.devcontainer/devcontainer.json`, then
  `.devcontainer.json`, then the first `.devcontainer/<name>/devcontainer.json`
  in name order, and parses it with variables substituted.
- `up(UpOptions { .. })` creates or starts the container and returns `Up`
  with the target, whether this call created the container, and the workspace
  folder inside it. `UpOptions::mounts` binds more host directories into a new
  container (`--mount` for both the CLI and the built-in support), such as the
  app's control socket directory; they also go into the target's `mounts`, so
  paths under them translate.
- `up_builtin(..)` does the same with the built-in support, even when the
  devcontainer CLI is installed.
- `remove(host, worktree, engine)` removes every container labeled
  `cmdterm.worktree=<worktree>`. Built images stay in the engine's cache.
- `container_name(worktree)` is `cmdterm-<basename>-<12 hex chars>`. The hex
  part is an FNV-1a hash of the full path, so it is the same on every run.
- `start_sidecar(SidecarOptions { .. })` runs podman in podman or docker in
  docker for the sandboxed worktrees of a project and waits until its engine
  listens on a socket in `socket_dir`. The sidecar is `cmdterm-sidecar-<12 hex
  chars>`, named after the project root, privileged,
  removed when it stops, and keeps its storage in a volume of the same name. It
  mounts the shared paths at their own place and, read-only, the host's registry
  certificate directories and CA bundle. `stop_sidecar(host, sidecar)` removes it.
  Linux only.

## Two strategies

If `command -v devcontainer` succeeds on the host, `up` runs
`devcontainer up --workspace-folder <worktree> --id-label cmdterm.worktree=<worktree>
--docker-path <engine>` (plus `--remove-existing-container` on rebuild) and reads
its JSON result. The CLI handles the whole spec, including features and Docker
Compose. Install it with `npm install -g @devcontainers/cli`.

Without the CLI, `up` uses the built-in support, which drives `docker` or
`podman` directly. The engine is the one passed in `UpOptions::engine`, else
`docker` if the host has it, else `podman`.

## What the built-in support reads

- Image: `image`, or `build.dockerfile` with `build.context` (default `.`),
  `build.args` and `build.target`. The legacy `dockerFile` and `context` keys
  work too. Build paths are relative to the directory of devcontainer.json.
  The image is tagged `cmdterm-dc-<hash>` and built each time the container is
  created, so the engine's layer cache decides how much work that is.
- Workspace: `workspaceFolder` (default `/workspaces/<basename>`) and
  `workspaceMount` (default a bind mount of the worktree on the workspace
  folder).
- Container: `containerEnv`, `containerUser`, `mounts` (strings or
  `{source, target, type}` objects; type defaults to `bind`), `runArgs`,
  `init`, `privileged`, `capAdd`, `securityOpt`, and `overrideCommand`
  (default true, which replaces the entrypoint with a `/bin/sh` sleep loop).
- Commands: `remoteUser` and `remoteEnv` apply to lifecycle commands and to
  every command scriptschnell-term later runs through the target. `remoteEnv` may refer
  to `${containerEnv:VAR}`; scriptschnell-term reads those from the running container.
- Lifecycle: `onCreateCommand`, `updateContentCommand`, `postCreateCommand`
  and `postStartCommand`. A string runs with `sh -c`, an array runs directly,
  and an object runs each named command one after another, sorted by name
  (the spec runs them in parallel). `postAttachCommand` and `forwardPorts` are
  ignored.
- Variables: `${localWorkspaceFolder}`, `${localWorkspaceFolderBasename}`,
  `${containerWorkspaceFolder}`, `${containerWorkspaceFolderBasename}`,
  `${localEnv:VAR}`, `${localEnv:VAR:default}` and `${devcontainerId}` (the
  hash part of the container name).

JSON comments (`//`, `/* */`) and trailing commas are allowed.

Containers carry the labels `cmdterm.worktree=<path>` and
`devcontainer.local_folder=<path>`. On `up`, a running container is reused, a
stopped one is started and gets `postStartCommand`, and a missing one is
created and gets every lifecycle command. `rebuild` removes the container first.

With podman, containers get `--userns=keep-id` so files in the bind-mounted
worktree keep the user's ownership. It is left out when `runArgs` already set
`--userns` or the host user is root.

## Limitations

- `features` and `dockerComposeFile` need the devcontainer CLI. The built-in
  support refuses such a config with an error that says so.
- `${localEnv:..}` comes from the environment of the scriptschnell-term process, not from
  the host. For an SSH or remote host that is the wrong machine, so a config
  that depends on the host's environment needs the CLI installed there.
- Lifecycle commands only run when scriptschnell-term creates or starts the container.
  Running `up` against a container that is already running does nothing.
- The CLI's result has no `remoteEnv`, so with the CLI scriptschnell-term reads it from
  devcontainer.json itself.
