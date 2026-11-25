# Remote machines, SSH, VMs and web access

Every process scriptschnell-term starts (shells, VCS polls, file reads, diffs, machine
monitor) runs through a single environment layer, so remote projects work the same way as
local ones.

## Location formats

Pass any of these on the command line or in the command palette (`Ctrl+Shift+P`):

| Location | Target |
| --- | --- |
| `/path/to/repo` | Local directory |
| `host:/path` or `ssh://[user@]host[:port]/path` | Project on an SSH host |
| `cmdterm://host[:port]/path` | Project on a `scriptschnell-term-remote` server |
| `cmdterm://host[:port]/path?vm=name` | Firecracker VM on a remote machine |
| `vm://name/path` | Local Firecracker VM on this machine |
| `tailscale://[user@]peer[:port]/path` | Tailscale peer over SSH (add `?remote` or `?vm=name` for remote server) |
| `netbird://[user@]peer[:port]/path` | NetBird peer over SSH (or `?remote`, `?vm=name`) |
| `nebula://[user@]peer[:port]/path` | Nebula peer over SSH (or `?remote`, `?vm=name`) |
| `k8s://pod/path`, `coder://workspace/path` | Custom schemes from extensions (`kubernetes`, `coder`) |

## SSH projects

SSH locations use your `~/.ssh/config` and share one multiplexed connection per host so
background VCS polls open a channel rather than a new handshake. Set
`SCRIPTSCHNELL_TERM_SSH_PROGRAM` to use another binary or wrapper instead of `ssh` (for
example a wrapper around `tsh ssh`).

## Remote machines (`cmdterm://`)

A remote machine runs either the standalone `scriptschnell-term-remote serve` server or
the desktop app with serving enabled (**Settings > Remote machines** or
`scriptschnell-term --serve`). Connections use a hybrid post-quantum handshake (Ed25519 +
ML-DSA-65 identities, X25519 + ML-KEM-768 key exchange).

- **Persistent sessions**: terminals on a remote machine or VM keep running across
  network drops and app restarts, and reattach automatically when you reopen the app.
- **Managing keys**: **Settings > Remote machines** copies your public key line, enables
  or disables serving this machine (`remote.serve`, `remote.listen`, default
  `0.0.0.0:7722`), lists authorized client keys with per-key **Shell** and **Projects**
  permissions, and adds trusted servers after showing their key fingerprint to verify.
- **Fused projects**: toggle the switch next to a trusted remote machine on **Settings >
  Remote machines** (`remote.fused`) to display that machine's shared projects directly
  in your local sidebar.

For CLI key commands, `authorized_keys` options, `server.json` and protocol details, see
[crates/cmdterm-remote/README.md](../crates/cmdterm-remote/README.md).

## Virtual machines (`vm://`)

**Settings > Virtual machines** manages local Firecracker VMs on Linux without root:

- **One-button setup**: builds ready-made Alpine or Ubuntu VM images in about half a
  minute, with internet access through `pasta`.
- **Lifecycle**: lists your VMs and lets you open, stop or reset them.
- **Custom images**: edit `server.json` under Advanced settings (or build images with
  `scriptschnell-term-remote image alpine|ubuntu`).

## VPN networks

**Settings > Remote machines > VPN networks** discovers peers on **Tailscale**
(`tailscale status --json`), **NetBird** (`netbird status --json`) and **Nebula**
(configured via `vpn.nebula_config` or `/etc/nebula/config.yml`). From any peer row you
can open a project over SSH or as a remote machine on port 7722, or click **Serve** to
bind this machine's remote server to its VPN address only.

## Web access from browsers and phones

`scriptschnell-term-web` is a standalone server and installable PWA for reaching your
machines when you only have a browser or phone (checking builds or answering an agent
away from your desk).

Machines dial out to `scriptschnell-term-web` (so they work behind NAT) after linking
with a one-time code:

```sh
scriptschnell-term-remote link https://web.example ABCD-EFGH-JK --name laptop
```

Both standalone `scriptschnell-term-remote serve` instances and desktop apps serving this
machine keep the link active. Terminals opened from the web app use persistent sessions.
For setup, password/OIDC authentication and the HTTP API, see
[crates/cmdterm-web/README.md](../crates/cmdterm-web/README.md).
