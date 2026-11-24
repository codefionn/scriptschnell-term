# cmdterm-remote

Remote machines for cmdterm. One machine serves, the others open projects on
it with `cmdterm://host[:port]/path`. The serving machine can be a dedicated
server running `scriptschnell-term-remote serve`, or a desktop running `scriptschnell-term --serve`.
It can offer its own environment, Firecracker VMs, or both.

The crate builds on Windows so the app can link it there. Keys, `known_hosts`
and the handshake work. `exec` and its master, `--forward`, the VM daemon and
Firecracker talk over Unix sockets and fail with "Unix sockets are not
supported on Windows" (`src/sys.rs`). `serve` starts, but its host channels
run `$SHELL` or `/bin/sh`, so serving a Windows machine is untested.

## Setup

On the client:

```sh
scriptschnell-term-remote keygen            # prints this machine's public key line
```

On the server:

```sh
scriptschnell-term-remote authorize 'cmdterm-hybrid-v1 AAAA... me@laptop'
scriptschnell-term-remote serve             # listens on 0.0.0.0:7722
scriptschnell-term-remote keygen            # shows the host key fingerprint to compare
```

Back on the client:

```sh
scriptschnell-term-remote trust server      # shows the fingerprint, asks before saving
scriptschnell-term cmdterm://server/srv/app
scriptschnell-term cmdterm://server:7722/~/src/app?vm=ci
```

The app's settings do the same on the Remote machines page: Copy puts your key
line on the clipboard, Add shows a server's fingerprint before trusting it, and a
switch serves this machine while the app runs (`remote.serve` and
`remote.listen` in `settings.json`). The Virtual machines page writes the
`firecracker` section below.

Keys and lists live in `remote/` of the configuration directory
(`~/.config/scriptschnell-term/remote/` on Linux): `id_hybrid` (mode 0600),
`id_hybrid.pub`, `authorized_keys`, `known_hosts`, and `server.json`. VM images and
disks go to `remote/` of the data directory (`~/.local/share/scriptschnell-term/remote/`).
`--config-dir` puts both into one directory, the same as for `scriptschnell-term`.

Where the session has secret storage, the private key also goes there: the Secret
Service through `secret-tool` (gnome-keyring, KWallet, KeePassXC), the login keychain
through `security` on macOS, or the Credential Manager on Windows. The entry's service is `scriptschnell-term remote key`, and
its account is the path of `id_hybrid`. A new key is stored right away. A key from before
goes there the next time the app starts, or on `scriptschnell-term-remote keygen`. When `id_hybrid`
is missing, the key comes back from the storage before a new one is made, and the
file is written again. With `--config-dir` the storage is left alone, so test runs leave
no keys in it. The file stays the key in use; the stored copy brings it back.

`authorized_keys` takes one key per line, optionally after a comma-separated
list of options. They give one machine its own access, whatever `server.json`
says for the rest:

- `vm-only` limits the key to VMs. `host` lets it run commands on the host
  even when `offer_host` is false.
- `projects` shares this machine's projects with the key even when
  `share_projects` is false. `no-projects` never shares them with it.
  Projects need host access.

A key without an option follows `server.json`. `scriptschnell-term-remote authorize`
takes the same as `--vm-only`, `--host`, `--projects` and `--no-projects`.
An unknown option, or two that contradict each other like `host,vm-only`,
rejects the key instead of being ignored. The server reads the file on every
login.

## Cryptography

Each side has a hybrid identity: Ed25519 plus ML-DSA-65 (FIPS 204). A
signature counts only when both halves verify.

The handshake (`src/handshake.rs`) takes three messages. The session key
comes from both an X25519 exchange and ML-KEM-768 (FIPS 203). It is SHA3-256
over both secrets and the transcript hash, as in X-Wing. The server signs the
transcript. The client checks that signature and `known_hosts` before it
sends its own identity, so a server it does not trust never learns who
connected. Then the client signs the transcript.

Traffic is ChaCha20-Poly1305 with one key per direction. The nonce is a
message counter and the length prefix is authenticated, so dropped,
replayed, reordered or altered frames end the connection.

The accept loops of `serve` and of cmdterm-web's link port cap connections
(`MAX_CONNECTIONS`/`MAX_LINKS`) and, before any handshake work is spent on
a socket, limit each peer address to 8 connection attempts per minute
(`src/rate.rs`), so unauthenticated callers cannot keep the ML-DSA
signatures busy. IPv6 addresses count per /64. A connection that logs in
with an authorized key gives its attempt back, so `exec --direct` can run
any number of commands. `serve` gives a client 15 seconds for the whole
handshake, however slowly it sends, and reads each handshake message only
up to the size it expects. One connection runs at most 1024 channels at once
(`src/mux.rs`); further opens fail instead of starting more processes.

This is a custom protocol built from standard parts (the RustCrypto
`ml-kem`, `ml-dsa`, `x25519-dalek`, `ed25519-dalek` and `chacha20poly1305`
crates). Nobody has reviewed it yet. Get it reviewed before you expose a
server to the internet.

## Connections

`cmdterm-env`'s `Environment::Remote` runs `scriptschnell-term-remote exec` for every
command and shell. `exec` talks to a per-server master over a Unix socket in
`$XDG_RUNTIME_DIR/cmdterm-remote/`, and starts the master if none runs. The
master holds the one TCP connection. It exits after 10 minutes without
clients (`CMDTERM_REMOTE_PERSIST_SECS` changes that), or when the server goes
away. `exec --direct` or `CMDTERM_REMOTE_DIRECT=1` skips the master.

`exec` exits with the remote command's exit code. A code outside 0 to 255, such
as the negative one of a crashed process on a Windows server, becomes 255. The
protocol carries text, so `exec` refuses a command, `--cwd` or `--env` that is
not UTF-8 instead of changing its bytes.

Channels have no flow control of their own. Up to 64 MiB of input waits
for a channel's program, also while its VM boots. When a program without a
PTY lets more wait, the server stops reading the connection, which holds up
its other channels, until the program reads again; one that reads nothing
for 120 seconds with 64 MiB waiting is ended with a note on its stderr.
Input for a PTY never waits: what goes beyond the 64 MiB is dropped. A
terminal's size never waits either, and only the newest one is kept. The
master keeps up to 16 MiB of output for each `exec`. A command's `exec`
that lets more wait holds up the connection the same way; a terminal's
`exec` is cut off instead, and reconnects to its session.

Clients never prompt. An unknown host key fails with a message that names
`scriptschnell-term-remote trust`.

`exec --forward INSIDE=LOCAL` carries connections to the Unix socket `INSIDE`
on the server (or in its VM) back to the Unix socket `LOCAL` on the client
while the command runs, like `ssh -R`. scriptschnell-term uses it to let
agents on a remote machine reach the app's control socket. `exec` opens a
second channel whose program is `cmdterm-remote:relay`; the server, or the
guest agent in a VM, runs no process for it but binds `INSIDE` with mode 0600
and carries each connection as frames on that channel (`src/relay.rs`). The
socket goes away with the channel. A server without relays fails that open,
and the command runs without the forward. Up to 64 MiB may wait for a
connection that reads slowly. A connection that lets more wait is shut
down, and its reader sees a plain end of the stream, as a Unix socket
cannot end with an error; a protocol with lengths, such as HTTP, notices
the missing bytes.

## Sessions

`exec --pty --session NAME` runs the command in a session. scriptschnell-term
starts every terminal on a remote machine or a local VM this way, with a random
name it saves with the panel. The program keeps running when the connection
drops, when `exec` exits, and when the app quits. Closing the panel ends it
with `exec --session NAME --end`, which exits with 0, or with 1 when no such
session runs.

The server keeps the newest 1 MiB of a session's output. When `exec` opens
the same name again, the server answers with the offset its replay starts at
(`Attached` in `src/proto.rs`), sends the output from there, and resizes the
PTY, so full-screen programs draw their screen again. A client that lost its
connection asks for the output after the last byte it showed, so it shows
nothing twice. A client that starts fresh, such as the app after a restart,
gets all output still kept. When the replay starts after output that is gone,
the server first sends the terminal modes the program set before (alternate
screen, mouse reporting, bracketed paste, cursor keys and visibility), and a
client that misses output in between gets a terminal reset first.

When the connection drops, `exec` says so in the terminal and reconnects,
waiting up to 15 seconds between attempts. Input typed meanwhile, up to
64 KiB, goes to the program once it is back. Both ends send TCP keepalives
after 15 seconds of silence, so a dead network ends the connection within
about half a minute. A new attach takes the session from the old one, so a
client that reconnects before the server noticed does not wait.

The app tells `exec` when a session's terminal is hidden (`SIGUSR1`) and when
it shows again (`SIGUSR2`): in another worktree, project or tab. `exec`
passes that on as `Batch`. For a hidden terminal the server holds output back
for up to 5 seconds, or until 128 KiB wait, and sends it as one message, so a
laptop or phone on a mobile network wakes its radio every few seconds for a
busy terminal in the background instead of for every line. Output that waits
goes out at once when the terminal shows again or the program exits. The
server announces that it takes `Batch` with a flag after the offset in
`Attached`; `exec` sends nothing to servers without it, which would end the
connection on a message they do not know.

A session belongs to the client key that started it. One key keeps at most
64; a 65th ends the one detached longest. An open attaches only when it
names the session's own VM, or no VM for a session on the host, and the key
may still use that place: a key made `vm-only` since cannot attach to its
host sessions, though `--end` still ends them.

An open that resumes a session that is gone starts nothing. When the server
still knows how the session ended, the client gets its exit; otherwise the
open fails with `session NAME no longer runs`. A client whose connection
dropped before the exit reached it then exits too, instead of running the
program a second time.

A client whose connection dropped before `Attached` arrived cannot tell
whether its open reached the server. It opens again with the `exit_if_ended`
flag, which follows everything older servers read. A running session attaches as
usual. Each `exec` picks a random run id and sends it with every open,
after the flag, and the server keeps the id with the session and its exit.
When the session ended and the server still knows the exit of the same run,
the client gets that exit and the server starts nothing. Otherwise the
program starts, also when the name ran an earlier program whose exit the
server still keeps, as when the app reuses a saved name after a restart.
Servers from before the flag skip it and start the program again when it
ended in between, and so does a master connection an older `exec` started,
which passes the open on without it. Opens without a run id, from clients
before it, behave as before.

A session ends when its program exits and the client saw the exit, on
`--end`, or after `detached_secs` in `server.json` without a client: seven
days by default, `0` for never. The local VM daemon keeps sessions the same
way and runs while any does. A server started with `scriptschnell-term
--serve` keeps them only while the app runs. A server from before sessions
runs the command as a plain channel, which ends with the connection.

A channel whose program is `cmdterm-remote:sessions` runs nothing either. The
server answers with the sessions of the client's key as one JSON array,
`[{"name": "t1", "attached": false, "exited": false}]`, and exits with 0.

## CPU limits

`exec --pty --cpu UNIT:RESERVE:PERCENT:WEIGHT` starts the command or shell under a
CPU limit of scriptschnell-term's CPU modes. The open message carries it after the
session name and resume offset, where older servers stop reading, so they start the
command unlimited. The server applies it as scriptschnell-term does for its own
terminals: a systemd scope in `app-scriptschnell_term.slice` on Linux, nested job
objects on Windows.

A channel whose program is `cmdterm-remote:cpu` runs nothing and changes the limits
in the server process, which holds the terminals' job objects on Windows:

- `topology` prints `online LIST` and `efficiency LIST` (or each core's
  `capacity N C` on Linux), in the kernel's CPU list format.
- `share RESERVE PERCENT [CORES]` moves the cap of all terminals, and their cores
  when CORES is given, empty for every core.
- `scope UNIT.scope WEIGHT [CORES]` sets one terminal's weight, 1 to 10000 with 100
  the default, and its cores.

It exits with 0 when the change applied and 1 when it could not.

## Sandboxes

A client's bubblewrap sandbox runs on the server with the server's network, and so
with its abstract Unix sockets, X11's among them. On Linux the server sets
`CMDTERM_SCOPE_EXE` for the commands it starts to the program that serves as its
proxy bridge: `scriptschnell-term-proxy-bridge` when it sits next to the server's executable,
else that executable. The sandbox starts with a short `sh` script that runs it as
`PROGRAM --cmdterm-proxy-bridge --try-scope -- COMMAND...`. Where the kernel has
Landlock ABI 6 (Linux 6.12), the bridge keeps the sandbox from abstract sockets bound
outside it and from signalling processes outside it, then becomes the command.
Without the variable, as from older servers, or on an older kernel, the command
starts as it is. `scriptschnell-term-remote --cmdterm-proxy-bridge ...` is that bridge mode; it
is not meant to be run by hand.

## Shared projects

A channel whose program is `cmdterm-remote:projects` runs nothing. The server
answers it with the repositories in the scriptschnell-term `state.json` of its
state directory as one JSON array, `[{"name": "app", "root": "/srv/app", "vcs":
"git"}]`, and exits with 0. Only projects on the machine itself count, not those it
reaches over SSH or on other remote machines, so two machines that show each
other's projects do not pass them back and forth. scriptschnell-term asks the
machines in `remote.fused` of its settings this way and shows their projects in its
sidebar (`src/projects.rs`).

The open fails, and the client learns nothing, when the key may not use the
host or has no share of the projects: by its `vm-only` or `no-projects` option,
or, without its own option, because `server.json` sets `offer_host` or
`share_projects` to false. A server from
before shared projects tries to run the program and fails too.

## Web links

`scriptschnell-term-remote link https://web.example CODE` links this machine to a
scriptschnell-term-web server (`crates/cmdterm-web`), whose web app then reaches it from
browsers and phones. The code comes from the web app. The machine sends its
public key with it over HTTPS and gets back the web server's key and the
address machines connect to; both go into `links.json` next to the keys, mode
0600. Plain `http://` is refused except to this machine. The command prints
the web server's key fingerprint after linking: compare it with the machine
key the web app shows, or pin it beforehand with
`--fingerprint SHA3-256:...`, which refuses the link when the answer carries
another key. `scriptschnell-term-remote links` lists the links and `scriptschnell-term-remote
unlink URL` removes one.

Every server on the machine, `serve` or the app's `--serve`, reads
`links.json` at start and every ten seconds after, and keeps one connection
per link. The machine dials the web server and runs the handshake as the
client, with its host key, and refuses a web server whose key differs from
the pinned one. Then the roles turn: the machine serves channels over that
connection like to any client, under the same rules (`offer_host`, VMs,
sessions owned by the web server's key). With `web_links_vm_only: true` in
`server.json` a web link instead gets `vm-only`: its browsers can only start
VMs here, never a host shell. The server logs a warning while it serves a
link with host access. A dropped link is dialed again after 1 second, then
twice as long each time up to a minute; one that stayed up for a minute
starts over at 1 second. Errors are logged once, not on every retry.

## Firecracker VMs

The quickest way to VMs is a ready-made image, built without root:

```sh
scriptschnell-term-remote image ubuntu      # or alpine
scriptschnell-term vm://ubuntu/~/           # a VM called ubuntu or ubuntu-<anything> uses it
```

The settings' Virtual machines page has the same as a Set up button per image.
`image` downloads the firecracker binary and a Firecracker CI kernel (6.1)
into `remote/images/` of the data directory once — both pinned to an exact
version in the source with a SHA-256 that is verified after the download and
before the binary is ever run, and the base container image pinned by digest —
builds the root filesystem from it with podman or docker (`--engine`), and packs it with
`fakeroot mkfs.ext4 -d`. The guest agent goes into `/opt/cmdterm` with the
C libraries it was linked against and runs through that dynamic loader, so
it works on Alpine's musl as well as on any glibc. The image's `/sbin/init`
mounts the basics, sets DNS from `cmdterm.dns=` on the kernel command line
and starts the agent. Then `image` adds the image to `server.json`: the
binary, kernel and a disk folder if none are set, `images.<name>`,
`default_image` unless a hand-made `rootfs` is the default, and
`user_network` when pasta is installed. The first build takes about a
minute, later ones about 20 to 30 seconds.

This was checked on a real machine: Alpine and Ubuntu VMs booted and ran
their first command in about a second, git and gcc ran inside, and
`curl https://github.com` worked through the user network.

### server.json

For your own images, write the `firecracker` section of `server.json` by hand:

```json
{
  "offer_host": false,
  "share_projects": true,
  "web_links_vm_only": true,
  "detached_secs": 86400,
  "firecracker": {
    "binary": "/usr/local/bin/firecracker",
    "kernel": "/var/lib/cmdterm/vmlinux",
    "rootfs": "/var/lib/cmdterm/rootfs.ext4",
    "state_dir": "/var/lib/cmdterm/vms",
    "vcpus": 2,
    "mem_mib": 2048,
    "idle_secs": 1800,
    "max_vms": 8,
    "network": [
      { "tap": "fc-tap0", "guest_ip": "172.16.0.2::172.16.0.1:255.255.255.0::eth0:off" }
    ]
  }
}
```

`images` maps names to more root filesystems. A new VM whose name is an
image name, or an image name and a dash, starts from that image, the longest
match winning. Other VMs start from `default_image` if set, else `rootfs`.
The VM records its image in `image` next to its disk.

`offer_host: false` turns the server into a pure compute provider. A client
selects a VM with `?vm=name`. `web_links_vm_only: true` does the same for the
web servers in `links.json`, whatever `offer_host` says: a linked scriptschnell-term-web
then serves its browsers only VMs. Each client key gets its own VMs under
`state_dir/<key id>/<name>/`, and each VM starts with a copy of `rootfs` that
it keeps between boots (reflinked where the filesystem allows). The server
boots a VM on first use. Concurrent channels share the VM, and it stops after
`idle_secs` without any. The server itself needs access to `/dev/kvm`.

VMs have no network unless one of two things is set. `network` lists tap
devices the administrator created, because creating taps needs
`CAP_NET_ADMIN`. `user_network: true` needs no root: each VM's Firecracker
runs under `pasta` (from passt) in its own user and network namespace, with
a tap and a NAT inside that namespace. The guest gets 172.16.0.2 and sends
DNS to 169.254.1.1, which pasta forwards to the host's resolver. pasta runs
with `--no-map-gw`, so a VM cannot reach services that listen only on the
host's loopback; it can reach the host's network like any other program.
`pasta` sets another path to the executable.

A hand-made guest image must run the agent at boot:

```sh
scriptschnell-term-remote guest --vsock-port 7723
```

The agent serves channels over vsock without encryption, because only the
host can reach that socket.

The server does not use Firecracker's `jailer`. Run the server as its own
unprivileged user if VMs run untrusted code.

## Local VMs

The same `firecracker` section gives this machine's own user VMs, without a
server. Open `vm://name/path` in scriptschnell-term, or run
`scriptschnell-term-remote exec --local-vm name -- cmd`. `exec` talks to the local VM
daemon (`scriptschnell-term-remote vmd`) over a Unix socket in
`$XDG_RUNTIME_DIR/cmdterm-remote/`, starting it when needed. Its VMs live
under `state_dir/local/<name>/` and stop after `idle_secs` without channels;
the daemon exits once none runs, and SIGTERM stops it with its VMs. When
`server.json` changes and no VM runs, the daemon reads it again, so a new
image works without a restart. The socket directory is mode 0700, which is
the only access check.

A VM keeps its disk, but it does not see your files. Copy a project in
(`tar -c app | scriptschnell-term-remote exec --local-vm dev -- tar -x -C /root/src`)
or clone it inside if the VM has a network. Extensions can copy setup
files in with `term.copy` when the VM's project first shows
(`extensions/claude-code` does this for Claude Code).

## Tests

`cargo test -p cmdterm-remote` runs a server in-process against the real
binary. A shell script stands in for Firecracker: it serves the guest agent
on the VM's vsock socket path (`guest --unix`), which covers the boot, the
proxy and the disk lifecycle without KVM. The suite does not boot a real VM, because
that needs KVM, a kernel and an image. `cargo test -p cmdterm -- --ignored
a_ready_made_image` does: it builds the Alpine image through the app, boots
a VM and fetches a page from inside.
