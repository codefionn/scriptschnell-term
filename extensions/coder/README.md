# coder

Opens projects in [Coder](https://coder.com) workspaces. Add a project as
`coder://workspace/path`, such as `coder://dev/~/src/app`, and the app runs its VCS
queries, terminals and extension jobs in the workspace through `coder ssh`. Name an
agent of a workspace with several as `coder://dev.main/~/src/app`.

It is off until you turn it on (Ctrl+,, Extensions, show all, coder). It needs the
`environments` permission, which lets it name the program, `coder`, that reaches the
workspaces. Install the `coder` CLI on this machine and sign in with `coder login`
first; the extension uses that login and that deployment.

## Settings

- Wait for startup script: `auto` follows the template, `yes` waits until the
  workspace's startup script finishes, `no` connects as soon as the agent is ready.
- Start stopped workspaces: on by default. A stopped workspace starts when the app
  first runs something in it, which can take a while the first time.
- Forward SSH agent: off by default. When on, programs in the workspace use the keys of
  this machine's SSH agent, such as for `git push`.

## Limits

Only your own workspaces: a location cannot hold the `/` of `owner/workspace`.
Terminals end when the connection drops; they do not keep running across restarts.
Agents in the workspace's terminals cannot reach the app's control socket, since
`coder ssh` forwards no Unix sockets for the app.
