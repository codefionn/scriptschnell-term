//! Firecracker microVMs.
//!
//! A server with a `firecracker` section in `server.json` offers VMs. A
//! client names one (`cmdterm://host/path?vm=ci`), and the server starts it
//! on first use. Every owner gets its own VMs: the same name from two client
//! keys means two machines. The local VM daemon (`cmdterm-remote vmd`) runs
//! VMs from the same configuration for this machine's own user, under the
//! owner `local`. A VM keeps its writable root disk between runs,
//! so clones and builds inside it survive. It stops after `idle_secs`
//! without channels.
//!
//! The root filesystem image must start `cmdterm-remote guest` at boot (see
//! the crate README). The server reaches that agent through Firecracker's
//! vsock device: it connects to the VM's vsock Unix socket and sends
//! `CONNECT <port>`. Channels then use the same messages as over TCP, without
//! sealing, because only this host can reach the socket.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cmdterm_env::Environment;
use serde::Deserialize;

use crate::exec::{self, Channel, Outbox};
use crate::proto::{Msg, OpenSpec};
use crate::sys::UnixStream;
use crate::transport::{FrameRead, FrameWrite, Plain};

/// The `firecracker` section of `server.json`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirecrackerConfig {
    /// The firecracker executable.
    #[serde(default = "default_binary")]
    pub binary: String,
    /// Uncompressed guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// Root filesystem image for VMs no other image names. Each VM starts
    /// from a copy.
    #[serde(default)]
    pub rootfs: Option<PathBuf>,
    /// Named root filesystems, such as the ready-made `alpine` and `ubuntu`
    /// (see [`crate::image`]). A new VM called `ubuntu` or `ubuntu-<anything>`
    /// starts from the `ubuntu` image.
    #[serde(default)]
    pub images: BTreeMap<String, PathBuf>,
    /// The image for VMs whose name matches none, instead of `rootfs`.
    #[serde(default)]
    pub default_image: Option<String>,
    /// Kernel command line. `ip=` from a network slot is appended.
    #[serde(default = "default_boot_args")]
    pub boot_args: String,
    #[serde(default = "default_vcpus")]
    pub vcpus: u8,
    #[serde(default = "default_mem")]
    pub mem_mib: u32,
    /// Where VM disks, sockets and logs live.
    pub state_dir: PathBuf,
    /// vsock port the guest agent listens on.
    #[serde(default = "default_guest_port")]
    pub guest_port: u32,
    /// Stop a VM after this many seconds without channels.
    #[serde(default = "default_idle")]
    pub idle_secs: u64,
    /// How long a VM may take to boot until its agent answers.
    #[serde(default = "default_boot_timeout")]
    pub boot_timeout_secs: u64,
    /// Most VMs running at once.
    #[serde(default = "default_max_vms")]
    pub max_vms: usize,
    /// Tap devices for guest networking, created by the administrator. A VM
    /// takes a free one; without any, VMs have no network.
    #[serde(default)]
    pub network: Vec<NetworkSlot>,
    /// Give VMs without a tap device a network through pasta, which needs
    /// no root: each VM's Firecracker runs in its own user and network
    /// namespace with a NAT to the host's connection. The VM cannot reach
    /// services that listen only on this machine's loopback.
    #[serde(default)]
    pub user_network: bool,
    #[serde(default = "default_pasta")]
    pub pasta: String,
}

/// Addresses of the user network inside each VM's namespace.
const USER_TAP: &str = "fc0";
const USER_GUEST_IP: &str = "172.16.0.2::172.16.0.1:255.255.255.252::eth0:off";
/// pasta forwards DNS sent here to the host's resolver.
const USER_DNS: &str = "169.254.1.1";
/// Runs in the namespace pasta created: a tap for the VM, forwarding and a
/// NAT out of pasta's interface, then Firecracker.
const USER_NET_SETUP: &str = r#"set -e
ip tuntap add dev fc0 mode tap
ip addr add 172.16.0.1/30 dev fc0
ip link set fc0 up
echo 1 > /proc/sys/net/ipv4/ip_forward
if command -v nft >/dev/null; then
  nft add table ip cmdterm
  nft add chain ip cmdterm post '{ type nat hook postrouting priority 100; }'
  nft add rule ip cmdterm post oifname != fc0 masquerade
else
  iptables -t nat -A POSTROUTING ! -o fc0 -j MASQUERADE
fi
exec "$@""#;

/// One tap device a VM can use.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSlot {
    pub tap: String,
    /// Appended to the kernel command line as `ip=<value>`, for example
    /// `172.16.0.2::172.16.0.1:255.255.255.0::eth0:off`.
    #[serde(default)]
    pub guest_ip: Option<String>,
}

fn default_binary() -> String {
    "firecracker".into()
}
fn default_pasta() -> String {
    "pasta".into()
}
fn default_boot_args() -> String {
    "console=ttyS0 reboot=k panic=1 pci=off".into()
}
fn default_vcpus() -> u8 {
    2
}
fn default_mem() -> u32 {
    1024
}
fn default_guest_port() -> u32 {
    7723
}
fn default_idle() -> u64 {
    1800
}
fn default_boot_timeout() -> u64 {
    30
}
fn default_max_vms() -> usize {
    8
}

impl FirecrackerConfig {
    /// The image a new VM called `name` starts from, and its name: an image
    /// named `name` or a prefix of it up to a `-`, else the default image,
    /// else `rootfs`.
    pub fn image_for(&self, name: &str) -> Result<(String, PathBuf), String> {
        let named = self
            .images
            .iter()
            .filter(|(image, _)| {
                name == image.as_str()
                    || name
                        .strip_prefix(image.as_str())
                        .is_some_and(|rest| rest.starts_with('-'))
            })
            .max_by_key(|(image, _)| image.len());
        if let Some((image, path)) = named {
            return Ok((image.clone(), path.clone()));
        }
        if let Some(image) = &self.default_image {
            return match self.images.get(image) {
                Some(path) => Ok((image.clone(), path.clone())),
                None => Err(format!("the default image {image:?} is not in images")),
            };
        }
        match &self.rootfs {
            Some(path) => Ok(("rootfs".into(), path.clone())),
            None => Err("no VM image set up: add rootfs or images to server.json".into()),
        }
    }
}

/// True for names that are safe as one path component.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !name.starts_with('-')
}

/// Starts, tracks and stops VMs.
pub struct VmManager {
    config: FirecrackerConfig,
    vms: Mutex<HashMap<(String, String), Arc<Vm>>>,
}

struct Vm {
    dir: PathBuf,
    state: Mutex<VmState>,
    channels: AtomicUsize,
}

#[derive(Default)]
struct VmState {
    child: Option<Child>,
    network: Option<usize>,
    idle_since: Option<Instant>,
}

impl VmState {
    fn running(&mut self) -> bool {
        match &mut self.child {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }
}

/// Keeps a VM marked busy while a channel uses it.
pub struct Lease {
    vm: Arc<Vm>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.vm.channels.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.vm.state.lock().unwrap().idle_since = Some(Instant::now());
        }
    }
}

impl VmManager {
    /// Creates the manager and its idle reaper thread.
    pub fn new(config: FirecrackerConfig) -> Arc<Self> {
        let manager = Arc::new(Self {
            config,
            vms: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&manager);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(15));
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                manager.stop_idle();
            }
        });
        manager
    }

    /// Starts the VM `name` of `owner` if needed and opens a stream to its
    /// guest agent. `owner` is a client key's short id, or `local`.
    pub fn connect(&self, owner: &str, name: &str) -> Result<(UnixStream, Lease), String> {
        if !valid_name(name) {
            return Err(format!("invalid VM name {name:?}"));
        }
        let vm = {
            let mut vms = self.vms.lock().unwrap();
            vms.entry((owner.to_owned(), name.to_owned()))
                .or_insert_with(|| {
                    Arc::new(Vm {
                        dir: self.config.state_dir.join(owner).join(name),
                        state: Mutex::new(VmState::default()),
                        channels: AtomicUsize::new(0),
                    })
                })
                .clone()
        };
        vm.channels.fetch_add(1, Ordering::SeqCst);
        let lease = Lease { vm: vm.clone() };
        {
            let mut state = vm.state.lock().unwrap();
            state.idle_since = None;
            if !state.running() {
                let network = self.free_network(&vm)?;
                self.start(&vm.dir, network)?;
                let child = self.spawn(&vm.dir)?;
                state.child = Some(child);
                state.network = network;
            }
        }
        let stream = self.wait_for_agent(&vm)?;
        Ok((stream, lease))
    }

    /// Picks a network slot no running VM uses, if any are configured.
    fn free_network(&self, this: &Arc<Vm>) -> Result<Option<usize>, String> {
        let vms = self.vms.lock().unwrap();
        let mut running = 0;
        let mut used = vec![false; self.config.network.len()];
        for vm in vms.values() {
            if Arc::ptr_eq(vm, this) {
                continue;
            }
            let Ok(mut state) = vm.state.try_lock() else {
                // Locked means it is starting or busy; count it as running.
                running += 1;
                continue;
            };
            if state.running() {
                running += 1;
                if let Some(slot) = state.network {
                    used[slot] = true;
                }
            }
        }
        if running >= self.config.max_vms {
            return Err(format!("this server already runs {running} VMs, its limit"));
        }
        if self.config.network.is_empty() {
            return Ok(None);
        }
        match used.iter().position(|used| !used) {
            Some(slot) => Ok(Some(slot)),
            None => Err("no free network device for another VM".into()),
        }
    }

    /// Prepares the VM directory: disk copy and Firecracker config file.
    fn start(&self, dir: &Path, network: Option<usize>) -> Result<(), String> {
        let disk = dir.join("rootfs.ext4");
        if !disk.exists() {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let (image, source) = self.config.image_for(&name)?;
            crate::create_private_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let partial = dir.join("rootfs.ext4.partial");
            Environment::Local
                .command("cp")
                .args(["--reflink=auto", "--sparse=always"])
                .arg(&source)
                .arg(&partial)
                .stdout()
                .map_err(|e| format!("could not copy the VM disk: {e}"))?;
            std::fs::rename(&partial, &disk).map_err(|e| e.to_string())?;
            // Which image the disk came from, for listings.
            let _ = std::fs::write(dir.join("image"), format!("{image}\n"));
        }
        for socket in ["api.sock", "vsock.sock"] {
            let _ = std::fs::remove_file(dir.join(socket));
        }
        let config = self.vm_config(dir, network);
        std::fs::write(dir.join("vm.json"), config.to_string()).map_err(|e| e.to_string())
    }

    fn vm_config(&self, dir: &Path, network: Option<usize>) -> serde_json::Value {
        let mut boot_args = self.config.boot_args.clone();
        let mut interfaces = Vec::new();
        if let Some(slot) = network.and_then(|i| self.config.network.get(i)) {
            if let Some(ip) = &slot.guest_ip {
                boot_args.push_str(" ip=");
                boot_args.push_str(ip);
            }
            interfaces.push(serde_json::json!({
                "iface_id": "eth0",
                "guest_mac": format!("06:00:ac:10:00:{:02x}", network.unwrap_or(0) + 2),
                "host_dev_name": slot.tap,
            }));
        } else if self.config.user_network {
            boot_args.push_str(&format!(" ip={USER_GUEST_IP} cmdterm.dns={USER_DNS}"));
            interfaces.push(serde_json::json!({
                "iface_id": "eth0",
                "guest_mac": "06:00:ac:10:00:02",
                "host_dev_name": USER_TAP,
            }));
        }
        serde_json::json!({
            "boot-source": {
                "kernel_image_path": self.config.kernel,
                "boot_args": boot_args,
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": dir.join("rootfs.ext4"),
                "is_root_device": true,
                "is_read_only": false,
            }],
            "machine-config": {
                "vcpu_count": self.config.vcpus,
                "mem_size_mib": self.config.mem_mib,
            },
            "vsock": {
                "guest_cid": 3,
                "uds_path": dir.join("vsock.sock"),
            },
            "network-interfaces": interfaces,
        })
    }

    fn spawn(&self, dir: &Path) -> Result<Child, String> {
        let log = std::fs::File::create(dir.join("firecracker.log")).map_err(|e| e.to_string())?;
        let user_network = self.config.user_network && self.config.network.is_empty();
        let launcher = if user_network {
            Environment::Local.command(&self.config.pasta).args([
                "--config-net",
                "--quiet",
                "--no-map-gw",
                "--dns-forward",
                USER_DNS,
                "-t",
                "none",
                "-u",
                "none",
                "-T",
                "none",
                "-U",
                "none",
                "--",
                "sh",
                "-c",
                USER_NET_SETUP,
                "sh",
            ])
        } else {
            Environment::Local.command(&self.config.binary)
        };
        let launcher = if user_network {
            launcher.arg(&self.config.binary)
        } else {
            launcher
        };
        let mut command = launcher
            .arg("--api-sock")
            .arg(dir.join("api.sock"))
            .arg("--config-file")
            .arg(dir.join("vm.json"))
            .cwd(dir)
            .to_std();
        command
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        // Keep terminal signals aimed at the server away from VMs.
        crate::sys::own_process_group(&mut command);
        command.spawn().map_err(|e| {
            let program = if user_network {
                &self.config.pasta
            } else {
                &self.config.binary
            };
            format!("could not start {program}: {e}")
        })
    }

    /// Connects to the guest agent, retrying while the VM boots.
    fn wait_for_agent(&self, vm: &Vm) -> Result<UnixStream, String> {
        let deadline = Instant::now() + Duration::from_secs(self.config.boot_timeout_secs);
        let socket = vm.dir.join("vsock.sock");
        let mut last_error = String::from("VM did not start");
        while Instant::now() < deadline {
            if !vm.state.lock().unwrap().running() {
                return Err(format!(
                    "VM stopped while booting: {}",
                    log_tail(&vm.dir.join("firecracker.log"))
                ));
            }
            match vsock_connect(&socket, self.config.guest_port) {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = error.to_string(),
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(format!("VM guest agent did not answer: {last_error}"))
    }

    fn stop_idle(&self) {
        let idle = Duration::from_secs(self.config.idle_secs);
        let vms: Vec<Arc<Vm>> = self.vms.lock().unwrap().values().cloned().collect();
        for vm in vms {
            let mut state = vm.state.lock().unwrap();
            let expired = state
                .idle_since
                .is_some_and(|since| since.elapsed() >= idle);
            if expired && vm.channels.load(Ordering::SeqCst) == 0 && state.running() {
                stop(&vm.dir, &mut state);
            }
        }
    }

    /// Number of VMs running now.
    pub fn running(&self) -> usize {
        let vms: Vec<Arc<Vm>> = self.vms.lock().unwrap().values().cloned().collect();
        vms.iter()
            .filter(|vm| vm.state.lock().unwrap().running())
            .count()
    }

    /// Stops every VM, for server shutdown.
    pub fn stop_all(&self) {
        let vms: Vec<Arc<Vm>> = self.vms.lock().unwrap().values().cloned().collect();
        for vm in vms {
            let mut state = vm.state.lock().unwrap();
            if state.running() {
                stop(&vm.dir, &mut state);
            }
        }
    }
}

/// Asks the guest to reboot (which a Firecracker guest turns into exit),
/// then kills the VM if it is still there after ten seconds.
fn stop(dir: &Path, state: &mut VmState) {
    let asked = api_put(
        &dir.join("api.sock"),
        "/actions",
        r#"{"action_type":"SendCtrlAltDel"}"#,
    );
    if asked.is_ok() {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && state.running() {
            thread::sleep(Duration::from_millis(100));
        }
    }
    if let Some(mut child) = state.child.take() {
        // The whole group: with user networking the child is pasta, and
        // Firecracker runs under it.
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    state.network = None;
}

/// One request to the Firecracker API socket.
fn api_put(socket: &Path, path: &str, body: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "PUT {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status)?;
    if status
        .split_whitespace()
        .nth(1)
        .is_some_and(|c| c.starts_with('2'))
    {
        Ok(())
    } else {
        Err(io::Error::other(status.trim().to_owned()))
    }
}

/// Opens a stream to `port` in the guest through Firecracker's host-side
/// vsock socket.
pub fn vsock_connect(socket: &Path, port: u32) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "CONNECT {port}")?;
    // Read the answer byte by byte so no channel bytes are consumed.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 64 {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    if !line.starts_with(b"OK ") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "guest agent is not listening yet",
        ));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(5).collect();
    let tail: Vec<&str> = lines.into_iter().rev().collect();
    if tail.is_empty() {
        "no log output".into()
    } else {
        tail.join(" | ")
    }
}

/// A channel that runs in a VM: a proxy to one guest agent connection.
pub struct VmChannel {
    id: u32,
    /// Messages for the guest, at most [`exec::INPUT_LIMIT`] bytes of them.
    tx: crate::queue::Sender<Msg>,
    out: Outbox,
    /// Input for a PTY never waits for room; see [`exec::INPUT_WAIT`].
    pty: bool,
    eof_sent: bool,
    closed: bool,
}

impl VmChannel {
    /// Returns at once; the VM boots in the background so other channels on
    /// the connection are not held up. Input sent meanwhile is queued, as
    /// much as a host channel would queue.
    pub fn open(
        manager: Arc<VmManager>,
        owner: String,
        id: u32,
        spec: &OpenSpec,
        out: Outbox,
    ) -> Box<dyn Channel> {
        let (tx, rx) = crate::queue::bounded::<Msg>(exec::INPUT_LIMIT);
        let pty = spec.pty.is_some();
        let note = out.clone();
        let mut guest_spec = spec.clone();
        let name = std::mem::take(&mut guest_spec.vm);
        thread::spawn(move || {
            let (stream, lease) = match manager.connect(&owner, &name) {
                Ok(connected) => connected,
                Err(message) => {
                    let _ = out.send(Msg::OpenFailed(id, message));
                    return;
                }
            };
            let Ok(read_half) = stream.try_clone() else {
                let _ = out.send(Msg::OpenFailed(id, "could not use the VM socket".into()));
                return;
            };
            let shutdown = stream.try_clone().ok();
            let mut writer = Plain(stream);
            let mut buf = Vec::new();
            Msg::Open(0, guest_spec).encode(&mut buf);
            if writer.write_frame(&buf).is_err() {
                let _ = out.send(Msg::OpenFailed(id, "VM agent closed the connection".into()));
                return;
            }
            thread::spawn(move || {
                let mut buf = Vec::new();
                while let Some(msg) = rx.recv() {
                    let close = matches!(msg, Msg::Close(_));
                    msg.with_channel(0).encode(&mut buf);
                    if writer.write_frame(&buf).is_err() || close {
                        break;
                    }
                }
                if let Some(stream) = shutdown {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            });
            let mut reader = Plain(read_half);
            let mut ended = false;
            while let Ok(true) = reader.read_frame(&mut buf) {
                let Ok(msg) = Msg::decode(&buf) else { break };
                ended |= matches!(msg, Msg::Exit(..) | Msg::OpenFailed(..));
                if out.send(msg.with_channel(id)).is_err() {
                    break;
                }
            }
            if !ended {
                let _ = out.send(Msg::OpenFailed(id, "VM connection lost".into()));
            }
            drop(lease);
        });
        Box::new(Self {
            id,
            tx,
            out: note,
            pty,
            eof_sent: false,
            closed: false,
        })
    }
}

impl Channel for VmChannel {
    /// Waits for room like a host channel (`exec::INPUT_WAIT`); a PTY's
    /// input that finds no room is dropped.
    fn input(&mut self, data: Vec<u8>) {
        if self.closed {
            return;
        }
        let cost = data.len();
        let wait = if self.pty {
            Duration::ZERO
        } else {
            exec::INPUT_WAIT
        };
        let sent = self.tx.send(Msg::Data(0, data), cost, Some(wait));
        if sent == Err(crate::queue::SendError::Full) && !self.pty {
            let note = format!(
                "cmdterm-remote: the VM read none of the command's input for {}s; ended it\n",
                wait.as_secs()
            );
            let _ = self.out.try_send(Msg::Stderr(self.id, note.into_bytes()));
            self.kill();
        }
    }

    fn close_input(&mut self) {
        if !self.eof_sent {
            self.eof_sent = true;
            let _ = self.tx.force(Msg::Eof(0));
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        // The newest size matters, so it never waits for room: it takes the
        // place of a size still queued, which keeps one at most.
        let _ = self.tx.force_replacing(Msg::Resize(0, cols, rows), |msg| {
            matches!(msg, Msg::Resize(..))
        });
    }

    fn kill(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.tx.force(Msg::Close(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_names_are_single_safe_components() {
        for good in ["ci", "build-1", "a_b"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "..", "a/b", "-x", "x y", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn config_fills_defaults_and_rejects_typos() {
        let config: FirecrackerConfig = serde_json::from_str(
            r#"{"kernel": "/k", "rootfs": "/r", "state_dir": "/s",
                "network": [{"tap": "fc0", "guest_ip": "10.0.0.2::10.0.0.1:255.255.255.0::eth0:off"}]}"#,
        )
        .unwrap();
        assert_eq!(config.binary, "firecracker");
        assert_eq!(config.guest_port, 7723);
        let manager = VmManager::new(config);
        let json = manager.vm_config(Path::new("/s/x/ci"), Some(0));
        assert_eq!(json["network-interfaces"][0]["host_dev_name"], "fc0");
        assert!(
            json["boot-source"]["boot_args"]
                .as_str()
                .unwrap()
                .ends_with(" ip=10.0.0.2::10.0.0.1:255.255.255.0::eth0:off")
        );
        assert_eq!(json["vsock"]["uds_path"], "/s/x/ci/vsock.sock");
        assert!(
            serde_json::from_str::<FirecrackerConfig>(
                r#"{"kernel": "/k", "rootfs": "/r", "state_dir": "/s", "vcpu": 4}"#
            )
            .is_err()
        );
    }

    #[test]
    fn vm_names_pick_their_image() {
        let mut config: FirecrackerConfig = serde_json::from_str(
            r#"{"kernel": "/k", "state_dir": "/s",
                "images": {"ubuntu": "/u.ext4", "alpine": "/a.ext4", "alpine-big": "/b.ext4"}}"#,
        )
        .unwrap();
        let image = |config: &FirecrackerConfig, name| config.image_for(name).map(|(i, _)| i);
        assert_eq!(image(&config, "ubuntu").unwrap(), "ubuntu");
        assert_eq!(image(&config, "ubuntu-work").unwrap(), "ubuntu");
        assert_eq!(image(&config, "alpine-big-2").unwrap(), "alpine-big");
        assert!(image(&config, "ubuntus").is_err(), "no default, no rootfs");
        config.rootfs = Some("/r".into());
        assert_eq!(image(&config, "dev").unwrap(), "rootfs");
        config.default_image = Some("alpine".into());
        assert_eq!(config.image_for("dev").unwrap().1, Path::new("/a.ext4"));
    }

    /// A resize gets through a full queue, and only the newest waits.
    #[test]
    fn the_newest_size_gets_through_a_full_queue() {
        let (tx, rx) = crate::queue::bounded::<Msg>(1024);
        let (out, _out_rx) = std::sync::mpsc::sync_channel(4);
        let mut channel = VmChannel {
            id: 1,
            tx,
            out,
            pty: true,
            eof_sent: false,
            closed: false,
        };
        channel.input(vec![b'x'; 2048]);
        channel.input(vec![b'y'; 16]);
        channel.resize(80, 24);
        channel.resize(100, 30);
        drop(channel);
        let mut got = Vec::new();
        while let Some(msg) = rx.recv() {
            got.push(msg);
        }
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(matches!(&got[0], Msg::Data(0, data) if data.len() == 2048));
        assert!(matches!(got[1], Msg::Resize(0, 100, 30)));
    }

    #[test]
    fn user_network_adds_a_tap_and_addresses() {
        let config: FirecrackerConfig = serde_json::from_str(
            r#"{"kernel": "/k", "rootfs": "/r", "state_dir": "/s", "user_network": true}"#,
        )
        .unwrap();
        let json = VmManager::new(config).vm_config(Path::new("/s/x/ci"), None);
        assert_eq!(json["network-interfaces"][0]["host_dev_name"], USER_TAP);
        let boot = json["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot.ends_with(&format!(" ip={USER_GUEST_IP} cmdterm.dns={USER_DNS}")));
    }
}
