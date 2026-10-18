//! The local VM daemon: Firecracker VMs for this machine's own user.
//!
//! `cmdterm-remote exec --local-vm NAME` (what `Environment::Vm` runs)
//! connects to the daemon's Unix socket in [`crate::runtime_dir`] and starts
//! the daemon when none listens, the same way `exec` starts a master. The
//! socket directory is mode 0700, so only this user reaches it; there is no
//! handshake. The daemon reads the `firecracker` section of `server.json`,
//! so local VMs and the VMs a server offers share one setup. Local VMs live
//! under `state_dir/local/`.
//!
//! Terminals keep running in sessions ([`crate::session`]) while no client
//! is connected. The daemon exits once no VM and no session has run and no
//! client has been connected for `PERSIST`. SIGTERM and SIGINT end its
//! sessions and stop its VMs first.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use sha3::{Digest, Sha3_256};

use crate::Paths;
use crate::exec::{Channel, Outbox};
use crate::mux::{self, Backend};
use crate::proto::OpenSpec;
use crate::server::ServerConfig;
use crate::session::{END_PROGRAM, Sessions};
use crate::sys::UnixStream;
use crate::transport::Plain;
use crate::vm::{FirecrackerConfig, VmChannel, VmManager};

/// VM owner name for this machine's user.
pub const LOCAL_OWNER: &str = "local";
const PERSIST: Duration = Duration::from_secs(600);

/// The daemon's socket for these paths.
pub fn vmd_socket(paths: &Paths) -> PathBuf {
    let digest = Sha3_256::digest(paths.dir.as_os_str().as_encoded_bytes());
    let name: String = digest[..10].iter().map(|b| format!("{b:02x}")).collect();
    crate::runtime_dir().join(format!("vmd-{name}.sock"))
}

/// Connects to the daemon, starting it if needed.
pub fn vmd_connection(paths: &Paths) -> io::Result<UnixStream> {
    crate::client::helper_connection(paths, &vmd_socket(paths), &["vmd"], "starting local VMs")
}

/// One of this machine's VMs, from its directory under `state_dir/local`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalVm {
    pub name: String,
    /// Its Firecracker process answers on the API socket.
    pub running: bool,
    /// The image its disk came from, if recorded.
    pub image: Option<String>,
}

/// This machine's VMs that have a disk, by name.
pub fn local_vms(config: &FirecrackerConfig) -> Vec<LocalVm> {
    let Ok(entries) = std::fs::read_dir(config.state_dir.join(LOCAL_OWNER)) else {
        return Vec::new();
    };
    let mut vms: Vec<LocalVm> = entries
        .flatten()
        .filter(|entry| entry.path().join("rootfs.ext4").exists())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let running = UnixStream::connect(entry.path().join("api.sock")).is_ok();
            let image = std::fs::read_to_string(entry.path().join("image"))
                .ok()
                .map(|image| image.trim().to_owned());
            Some(LocalVm {
                name,
                running,
                image,
            })
        })
        .collect();
    vms.sort_by(|a, b| a.name.cmp(&b.name));
    vms
}

/// Deletes a stopped VM's disk, so its next start begins from the image.
pub fn delete_local_vm(config: &FirecrackerConfig, name: &str) -> io::Result<()> {
    if !crate::vm::valid_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a VM name"));
    }
    let dir = config.state_dir.join(LOCAL_OWNER).join(name);
    if UnixStream::connect(dir.join("api.sock")).is_ok() {
        return Err(io::Error::other(format!(
            "{name} is running; stop VMs first"
        )));
    }
    std::fs::remove_dir_all(dir)
}

/// Stops the VM daemon and with it every local VM. Returns false when none
/// was running.
pub fn stop_vmd(paths: &Paths) -> io::Result<bool> {
    let socket = vmd_socket(paths);
    if UnixStream::connect(&socket).is_err() {
        return Ok(false);
    }
    let pid: i32 = std::fs::read_to_string(socket.with_extension("pid"))?
        .trim()
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad vmd pid file"))?;
    crate::sys::terminate(pid)?;
    Ok(true)
}

/// The daemon's VM manager, made again from `server.json` when the file
/// changed and no VM runs, so new images and settings apply without
/// restarting the daemon.
struct Managers {
    paths: Paths,
    current: Mutex<(Arc<VmManager>, Option<SystemTime>)>,
}

impl Managers {
    fn get(&self) -> Arc<VmManager> {
        let mut current = self.current.lock().unwrap();
        let modified = config_modified(&self.paths);
        if modified != current.1 && current.0.running() == 0 {
            if let Ok(ServerConfig {
                firecracker: Some(config),
                ..
            }) = ServerConfig::load(&self.paths)
            {
                current.0 = VmManager::new(config);
            }
            current.1 = modified;
        }
        current.0.clone()
    }

    fn running(&self) -> usize {
        self.current.lock().unwrap().0.running()
    }

    #[cfg(unix)]
    fn stop_all(&self) {
        let manager = self.current.lock().unwrap().0.clone();
        manager.stop_all();
    }
}

fn config_modified(paths: &Paths) -> Option<SystemTime> {
    std::fs::metadata(paths.server_config())
        .and_then(|m| m.modified())
        .ok()
}

/// Removes the daemon's socket and pid file before it exits.
fn remove_runtime_files(socket: &Path) {
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(socket.with_extension("pid"));
}

struct LocalBackend {
    managers: Arc<Managers>,
    sessions: Arc<Sessions>,
}

impl Backend for LocalBackend {
    fn open(&self, id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
        if !spec.session.is_empty() && (spec.pty.is_some() || spec.program == END_PROGRAM) {
            return self.sessions.open(LOCAL_OWNER, id, spec, out, |spec, out| {
                self.start(0, spec, out)
            });
        }
        self.start(id, spec, out)
    }
}

impl LocalBackend {
    fn start(&self, id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
        if spec.vm.is_empty() {
            return Err("the local VM daemon only runs VMs; name one".into());
        }
        Ok(VmChannel::open(
            self.managers.get(),
            LOCAL_OWNER.to_owned(),
            id,
            spec,
            out,
        ))
    }
}

/// Runs the daemon. Prints `ready` or `error: ...` once, like the master.
pub fn run_vmd(paths: &Paths) -> io::Result<()> {
    crate::sys::ignore_hangup();
    crate::create_runtime_dir()?;
    let socket = vmd_socket(paths);
    let lock = std::fs::File::create(socket.with_extension("lock"))?;
    lock.lock()?;
    if UnixStream::connect(&socket).is_ok() {
        println!("ready");
        return Ok(());
    }
    let (config, sessions) = match ServerConfig::load(paths) {
        Ok(
            server @ ServerConfig {
                firecracker: Some(_),
                ..
            },
        ) => (
            server.firecracker.clone().expect("matched above"),
            Sessions::new(server.detached_limit()),
        ),
        Ok(_) => {
            let message = format!(
                "no VMs set up: add a firecracker section to {}",
                paths.server_config().display()
            );
            println!("error: {message}");
            return Err(io::Error::other(message));
        }
        Err(error) => {
            println!("error: {error}");
            return Err(error);
        }
    };
    let managers = Arc::new(Managers {
        current: Mutex::new((VmManager::new(config), config_modified(paths))),
        paths: paths.clone(),
    });
    let _ = std::fs::remove_file(&socket);
    // Mode 0600 from creation, like the master's socket.
    let listener = crate::sys::bind_private_socket(&socket)?;
    std::fs::write(socket.with_extension("pid"), std::process::id().to_string())?;
    println!("ready");
    let _ = io::stdout().flush();

    #[cfg(unix)]
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ]) {
        let stop_managers = managers.clone();
        let stop_socket = socket.clone();
        let stop_sessions = sessions.clone();
        thread::spawn(move || {
            if signals.forever().next().is_some() {
                stop_sessions.stop_all();
                stop_managers.stop_all();
                remove_runtime_files(&stop_socket);
                std::process::exit(0);
            }
        });
    }

    let clients = Arc::new(AtomicUsize::new(0));
    let idle_clients = clients.clone();
    let idle_managers = managers.clone();
    let idle_socket = socket.clone();
    let idle_sessions = sessions.clone();
    let persist = std::env::var("CMDTERM_REMOTE_PERSIST_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(PERSIST, Duration::from_secs);
    thread::spawn(move || {
        let mut idle_since = Instant::now();
        loop {
            thread::sleep(Duration::from_secs(1).min(persist));
            if idle_clients.load(Ordering::SeqCst) > 0
                || idle_managers.running() > 0
                || !idle_sessions.is_empty()
            {
                idle_since = Instant::now();
            } else if idle_since.elapsed() >= persist {
                remove_runtime_files(&idle_socket);
                std::process::exit(0);
            }
        }
    });

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let managers = managers.clone();
        let sessions = sessions.clone();
        let clients = clients.clone();
        clients.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            if let Ok(read) = stream.try_clone() {
                let backend = LocalBackend { managers, sessions };
                let _ = mux::serve(Plain(read), Plain(stream), &backend);
            }
            clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}
