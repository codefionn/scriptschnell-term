//! Remote machines, VMs and containers as the settings overlay manages them:
//! serving this machine from the app, the servers it trusts and the keys it
//! lets in, the `firecracker` section of `server.json`, local VMs, and the
//! devcontainer engine and sandbox lists in `settings.json`, and the peers
//! of the VPNs this machine is on.
//!
//! Everything here reads small local files, so it runs on the UI thread,
//! except fetching a server's key, building ready-made VM images and
//! scanning VPNs, which run on threads.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::state::Runtime;
use cmdterm_remote::keys::{self, AuthorizedKey, HostTrust, PublicKey};
use cmdterm_remote::local::{self, LocalVm};
use cmdterm_remote::server::{Server, ServerConfig};
use cmdterm_remote::{Paths, image};
use schnellui::RedrawSignal;
use serde_json::Value;

use crate::ui::{CloneTarget, InputMode};
use crate::workspace::Workspace;

/// What the pages show, read again when a page opens and after each change.
#[derive(Clone, Default)]
pub struct MachineFiles {
    /// This machine's key, created on first read.
    pub own_key: Option<PublicKey>,
    pub known_hosts: Vec<(String, PublicKey)>,
    pub allowed: Vec<AuthorizedKey>,
    pub config: ServerConfig,
    /// Why `server.json` did not load, if it did not.
    pub config_error: Option<String>,
    pub vms: Vec<LocalVm>,
    /// `/dev/kvm` opens for reading and writing.
    pub kvm: bool,
    /// Ready-made images that are built, by name.
    pub built: Vec<&'static str>,
    /// Programs a ready-made image build needs that are missing.
    pub missing_tools: Vec<String>,
    /// pasta is installed, so VMs get a network without root.
    pub user_network: bool,
}

/// A field of the Firecracker form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmField {
    Binary,
    Kernel,
    Rootfs,
    StateDir,
    Vcpus,
    MemMib,
    IdleSecs,
}

impl VmField {
    pub const ALL: [VmField; 7] = [
        VmField::Binary,
        VmField::Kernel,
        VmField::Rootfs,
        VmField::StateDir,
        VmField::Vcpus,
        VmField::MemMib,
        VmField::IdleSecs,
    ];

    /// The key in `server.json`'s `firecracker` section.
    pub fn key(self) -> &'static str {
        match self {
            VmField::Binary => "binary",
            VmField::Kernel => "kernel",
            VmField::Rootfs => "rootfs",
            VmField::StateDir => "state_dir",
            VmField::Vcpus => "vcpus",
            VmField::MemMib => "mem_mib",
            VmField::IdleSecs => "idle_secs",
        }
    }

    fn numeric(self) -> bool {
        matches!(self, VmField::Vcpus | VmField::MemMib | VmField::IdleSecs)
    }
}

/// A text field on the pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineText {
    Address,
    KeyLine,
    VmName,
    SandboxPath,
    SandboxHost,
    /// A folder for the active project's sandboxes, on the Project page.
    ProjectPath,
    /// The name of a per-project environment variable, on the Project page.
    ProjectEnvName,
    /// The value of a per-project environment variable, on the Project page.
    ProjectEnvValue,
    Vm(VmField),
}

/// What the user typed, kept across overlay rebuilds.
#[derive(Default)]
pub struct MachineForm {
    pub address: String,
    pub key_line: String,
    pub vm_only: bool,
    pub vm_name: String,
    pub sandbox_path: String,
    pub sandbox_host: String,
    /// Whether the hosts field is being typed in: each key rebuilds the
    /// page to filter the list, and focus stays in the field.
    pub filtering_hosts: bool,
    /// The host removed last, which the hosts page offers to put back.
    pub removed_host: Option<String>,
    /// Whether the allowed-key field is being typed in: the page rebuilds
    /// when it turns empty or not, so the Allow buttons follow, and focus
    /// stays in the field.
    pub typing_key: bool,
    /// The same for the address of a machine to connect to, whose Add
    /// button is off while the field is empty.
    pub typing_address: bool,
    /// Host groups on the Hosts page whose open state the user flipped, by
    /// their index in `HOST_GROUPS` (one past the end is the user's own).
    /// The common groups start closed and the user's own starts open.
    pub flipped_host_groups: Vec<usize>,
    pub project_path: String,
    /// The reference branch typed for a project, by its id, until it is set.
    pub project_base: Option<(u64, String)>,
    /// Name and value typed for a new project environment variable.
    pub project_env_name: String,
    pub project_env_value: String,
    /// Why the last project environment variable Add was refused.
    pub project_env_error: Option<String>,
    /// The Firecracker fields in [`VmField::ALL`] order. Empty until the
    /// first read of `server.json` fills them.
    pub vm: Vec<String>,
}

impl MachineForm {
    fn text_mut(&mut self, field: MachineText) -> &mut String {
        match field {
            MachineText::Address => &mut self.address,
            MachineText::KeyLine => &mut self.key_line,
            MachineText::VmName => &mut self.vm_name,
            MachineText::SandboxPath => &mut self.sandbox_path,
            MachineText::SandboxHost => &mut self.sandbox_host,
            MachineText::ProjectPath => &mut self.project_path,
            MachineText::ProjectEnvName => &mut self.project_env_name,
            MachineText::ProjectEnvValue => &mut self.project_env_value,
            MachineText::Vm(field) => {
                if self.vm.len() < VmField::ALL.len() {
                    self.vm.resize(VmField::ALL.len(), String::new());
                }
                &mut self.vm[field as usize]
            }
        }
    }

    pub fn vm_text(&self, field: VmField) -> &str {
        self.vm.get(field as usize).map_or("", String::as_str)
    }
}

/// A server key fetched to be trusted.
#[derive(Clone, Debug, Default)]
pub enum Probe {
    #[default]
    Idle,
    Fetching(String),
    /// Not in `known_hosts` yet: shown with its fingerprint to compare.
    Unknown {
        address: String,
        key: Box<PublicKey>,
    },
    Trusted(String),
    /// `known_hosts` lists the address with another key.
    Changed {
        address: String,
        fingerprint: String,
    },
    Failed {
        address: String,
        error: String,
    },
}

/// A ready-made image build.
#[derive(Clone, Debug)]
pub enum ImageJob {
    /// The step it is on.
    Running(String),
    Failed(String),
    Done,
}

/// State shared with a worker thread. The thread bumps `revision` and wakes
/// the UI, which rebuilds the overlay.
#[derive(Default)]
pub struct Shared<T> {
    value: Arc<Mutex<T>>,
    revision: Arc<AtomicU64>,
    shown: Arc<AtomicU64>,
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            revision: self.revision.clone(),
            shown: self.shown.clone(),
        }
    }
}

impl<T: Clone + Default> Shared<T> {
    pub fn get(&self) -> T {
        self.value.lock().map(|v| v.clone()).unwrap_or_default()
    }

    fn update(&self, change: impl FnOnce(&mut T)) {
        if let Ok(mut value) = self.value.lock() {
            change(&mut value);
        }
        self.revision.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn set(&self, value: T) {
        self.update(|slot| *slot = value);
    }

    /// True once after the thread changed it. Allocation-free, called per
    /// poll.
    pub fn changed(&self) -> bool {
        let revision = self.revision.load(Ordering::Acquire);
        self.shown.swap(revision, Ordering::AcqRel) != revision
    }
}

/// What the last VPN scan found.
#[derive(Clone, Default)]
pub struct VpnScan {
    pub scanning: bool,
    /// Every VPN scriptschnell-term knows, in [`Vpn::ALL`] order. Empty
    /// before the first scan.
    ///
    /// [`Vpn::ALL`]: crate::vpn::Vpn::ALL
    pub networks: Vec<crate::vpn::Network>,
}

/// How long after Set up turns into Start a press on it counts as the end of
/// the same double click.
const ARM_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// The settings overlay's remote, VM and container state.
#[derive(Default)]
pub struct MachinesUi {
    pub files: MachineFiles,
    pub form: MachineForm,
    pub probe: Shared<Probe>,
    pub vpn: Shared<VpnScan>,
    /// Ready-made image builds, by image name.
    pub jobs: Shared<BTreeMap<&'static str, ImageJob>>,
    /// Finished builds already applied to the server and the page.
    applied: Vec<&'static str>,
    /// Whether the VM page shows the hand-edited Firecracker settings.
    pub advanced: bool,
    /// The ready-made image whose Set up was pressed once: its row says
    /// what setting it up downloads, and a second press starts it.
    pub armed: Option<&'static str>,
    /// When `armed` was set: a second press sooner than
    /// [`ARM_SETTLE`] after it is the end of a double click, not a choice.
    armed_at: Option<std::time::Instant>,
    /// Kept open so the copied key stays on the clipboard.
    clipboard: schnellui::platform::SystemClipboard,
}

/// This machine serving others from inside the app.
pub struct Serving {
    pub server: Arc<Server>,
    pub bound: SocketAddr,
    /// The address asked for, to bind again after a config change.
    pub address: String,
}

fn read_files(paths: &Paths, engine: Option<&str>) -> MachineFiles {
    let own_key = paths.load_identity().ok().map(|id| id.public().clone());
    // Keys made before the keyring held them go there once per run, on a
    // thread, since a locked keyring waits for its password.
    static KEPT: std::sync::Once = std::sync::Once::new();
    KEPT.call_once(|| {
        let paths = paths.clone();
        std::thread::spawn(move || paths.keep_identity());
    });
    let (config, config_error) = match ServerConfig::load(paths) {
        Ok(config) => (config, None),
        Err(error) => (ServerConfig::default(), Some(error.to_string())),
    };
    let vms = config
        .firecracker
        .as_ref()
        .map(local::local_vms)
        .unwrap_or_default();
    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    let built = image::PRESETS
        .iter()
        .filter(|preset| image::rootfs_path(paths, preset).is_file())
        .map(|preset| preset.name)
        .collect();
    MachineFiles {
        built,
        missing_tools: image::missing_tools(engine),
        user_network: image::user_network_available(),
        own_key,
        known_hosts: keys::known_hosts_list(&paths.known_hosts()).unwrap_or_default(),
        allowed: keys::authorized_list(&paths.authorized_keys()).unwrap_or_default(),
        config,
        config_error,
        vms,
        kvm,
    }
}

/// The Firecracker form filled from `server.json`, or with defaults that
/// keep VM disks under the config directory.
fn vm_form(paths: &Paths, config: &ServerConfig) -> Vec<String> {
    match &config.firecracker {
        Some(fc) => vec![
            fc.binary.clone(),
            fc.kernel.display().to_string(),
            fc.rootfs
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            fc.state_dir.display().to_string(),
            fc.vcpus.to_string(),
            fc.mem_mib.to_string(),
            fc.idle_secs.to_string(),
        ],
        None => vec![
            "firecracker".into(),
            String::new(),
            String::new(),
            paths.data.join("vms").display().to_string(),
            "2".into(),
            "1024".into(),
            "1800".into(),
        ],
    }
}

fn wake_up(wake: &Arc<OnceLock<RedrawSignal>>) {
    if let Some(signal) = wake.get() {
        signal.request_redraw();
    }
}

impl Workspace {
    /// This run's remote configuration: `remote/` of the configuration
    /// and data directories.
    pub fn remote_paths(&self) -> Paths {
        Paths::of(&self.borrow().dirs)
    }

    /// Reads keys, hosts, `server.json` and local VMs again for the pages.
    pub fn refresh_machines(&self) {
        let paths = self.remote_paths();
        let engine = self.borrow().settings.container_engine.clone();
        let files = read_files(&paths, engine.as_deref());
        let mut inner = self.borrow_mut();
        let ui = &mut inner.settings_ui.machines;
        if ui.form.vm.is_empty() {
            ui.form.vm = vm_form(&paths, &files.config);
        }
        ui.files = files;
        inner.dirty.settings = true;
    }

    /// Mirrors a text field. The field keeps its own text, so this sets no
    /// dirty flag.
    pub fn set_machine_text(&self, field: MachineText, value: &str) {
        let mut inner = self.borrow_mut();
        let text = inner.settings_ui.machines.form.text_mut(field);
        let was_empty = text.trim().is_empty();
        text.clear();
        text.push_str(value);
        // The Allow and Add buttons are off while their field is empty.
        if was_empty != value.trim().is_empty() {
            match field {
                MachineText::KeyLine => inner.settings_ui.machines.form.typing_key = true,
                MachineText::Address => inner.settings_ui.machines.form.typing_address = true,
                _ => {}
            }
            if matches!(field, MachineText::KeyLine | MachineText::Address) {
                inner.dirty.settings = true;
            }
        }
        // A failed connection's error goes once the address changes, so it
        // never names an address the field no longer holds.
        if field == MachineText::Address
            && matches!(inner.settings_ui.machines.probe.get(), Probe::Failed { .. })
        {
            inner.settings_ui.machines.probe.set(Probe::Idle);
            inner.settings_ui.machines.form.typing_address = true;
            inner.dirty.settings = true;
        }
        // The hosts field also filters the list under it.
        if field == MachineText::SandboxHost {
            inner.settings_ui.machines.form.filtering_hosts = true;
            inner.dirty.settings = true;
        }
        // Only one field holds the settings' focus ref: the one typed in.
        if inner.dirty.settings {
            inner.settings_ui.finding_page = false;
            inner.settings_ui.finding_extension = false;
        }
    }

    /// Puts this machine's public key line on the clipboard, to paste into
    /// a server's allowed keys.
    pub fn copy_own_key(&self) {
        let paths = self.remote_paths();
        let line = match std::fs::read_to_string(paths.identity().with_extension("pub")) {
            Ok(line) => line.trim().to_owned(),
            Err(error) => {
                self.toast_failed((self.tr().error)(&error.to_string()));
                return;
            }
        };
        let copied = self
            .borrow_mut()
            .settings_ui
            .machines
            .clipboard
            .write_text(line);
        match copied {
            Ok(()) => self.toast_done(self.tr().key_copied),
            Err(error) => self.toast_failed((self.tr().error)(&error.to_string())),
        }
    }

    /// Puts a shell command on the clipboard, such as the one that
    /// installs a missing agent.
    pub fn copy_command(&self, command: String) {
        let copied = self
            .borrow_mut()
            .settings_ui
            .machines
            .clipboard
            .write_text(command);
        match copied {
            Ok(()) => self.toast_done(self.tr().command_copied),
            Err(error) => self.toast_failed((self.tr().error)(&error.to_string())),
        }
    }

    // --- serving ---------------------------------------------------------------

    /// Serves this machine on `address` until [`Workspace::stop_serving`].
    /// Returns the bound address and the host key fingerprint.
    pub fn serve(&self, address: &str) -> std::io::Result<(SocketAddr, String)> {
        self.stop_serving();
        let server = Server::new(self.remote_paths())?;
        let bound = server.spawn(address)?;
        let fingerprint = server.public_key().fingerprint();
        self.borrow_mut().serving = Some(Serving {
            server,
            bound,
            address: address.to_owned(),
        });
        Ok((bound, fingerprint))
    }

    pub fn stop_serving(&self) {
        let serving = self.borrow_mut().serving.take();
        if let Some(serving) = serving {
            serving.server.stop();
        }
    }

    /// Where this machine is served, if it is.
    #[cfg(test)]
    pub fn serving_on(&self) -> Option<SocketAddr> {
        self.borrow().serving.as_ref().map(|s| s.bound)
    }

    /// Starts serving when the settings say so. Called once at startup.
    pub fn serve_from_settings(&self) {
        let (serve, listen) = {
            let inner = self.borrow();
            (
                inner.settings.remote.serve,
                inner.settings.remote.listen.clone(),
            )
        };
        if serve && let Err(error) = self.serve(&listen) {
            self.notify((self.tr().serve_failed)(&error.to_string()));
        }
    }

    /// The Serve switch: starts or stops serving and remembers it.
    pub fn set_serving(&self, on: bool) {
        if on {
            let listen = self.borrow().settings.remote.listen.clone();
            match self.serve(&listen) {
                Ok((bound, _)) => self.toast_done((self.tr().serving_on)(&bound.to_string())),
                Err(error) => {
                    self.toast_failed((self.tr().serve_failed)(&error.to_string()));
                    self.refresh_machines();
                    return;
                }
            }
        } else {
            self.stop_serving();
            self.toast_done(self.tr().toast_stopped_serving);
        }
        self.borrow_mut().settings.remote.serve = on;
        self.save_settings();
        self.refresh_machines();
    }

    /// Serves again with a changed `server.json`, if serving.
    fn reload_server(&self) {
        let address = self.borrow().serving.as_ref().map(|s| s.address.clone());
        if let Some(address) = address
            && let Err(error) = self.serve(&address)
        {
            self.toast_failed((self.tr().serve_failed)(&error.to_string()));
        }
    }

    /// Edits `server.json`, reports a bad result, and serves again with it.
    fn edit_server_config(&self, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> bool {
        let result = ServerConfig::edit(&self.remote_paths(), edit);
        let ok = match result {
            Ok(_) => {
                self.reload_server();
                true
            }
            Err(error) => {
                self.toast_failed((self.tr().config_not_saved)(&error.to_string()));
                false
            }
        };
        self.refresh_machines();
        ok
    }

    /// Whether clients may run commands on this machine itself, or only in
    /// its VMs.
    pub fn set_offer_host(&self, on: bool) {
        let saved = self.edit_server_config(|map| {
            map.insert("offer_host".into(), on.into());
        });
        if saved {
            self.toast_done(self.tr().toast_saved);
        }
    }

    /// Whether clients may ask which repositories this machine has, to show
    /// them in their sidebars.
    pub fn set_share_projects(&self, on: bool) {
        let saved = self.edit_server_config(|map| {
            map.insert("share_projects".into(), on.into());
        });
        if saved {
            self.toast_done(self.tr().toast_saved);
        }
    }

    /// Whether web servers this machine is linked to (cmdterm-web) may run
    /// channels on this machine itself, or only in its VMs.
    pub fn set_web_links_vm_only(&self, on: bool) {
        let saved = self.edit_server_config(|map| {
            map.insert("web_links_vm_only".into(), on.into());
        });
        if saved {
            self.toast_done(self.tr().toast_saved);
        }
    }

    // --- machines this one connects to -----------------------------------------

    /// Fetches the key of the server typed into the address field, on a
    /// thread, to show its fingerprint before trusting it.
    pub fn probe_machine(&self) {
        let typed = self
            .borrow()
            .settings_ui
            .machines
            .form
            .address
            .trim()
            .to_owned();
        if typed.is_empty() {
            return;
        }
        let address = cmdterm_env::RemoteTarget::new(&typed).address;
        let paths = self.remote_paths();
        let slot = self.borrow().settings_ui.machines.probe.clone();
        let wake = self.redraw_handle();
        slot.set(Probe::Fetching(address.clone()));
        self.borrow_mut().dirty.settings = true;
        std::thread::spawn(move || {
            let probe = match cmdterm_remote::client::fetch_host_key(&paths, &address) {
                Ok((_, HostTrust::Trusted)) => Probe::Trusted(address),
                Ok((key, HostTrust::Unknown)) => Probe::Unknown {
                    address,
                    key: Box::new(key),
                },
                Ok((key, HostTrust::Changed)) => Probe::Changed {
                    address,
                    fingerprint: key.fingerprint(),
                },
                Err(error) => Probe::Failed {
                    address,
                    error: error.to_string(),
                },
            };
            slot.set(probe);
            wake_up(&wake);
        });
    }

    /// Trusts the key the probe fetched, after the user compared it.
    pub fn trust_probed(&self) {
        let slot = self.borrow().settings_ui.machines.probe.clone();
        if let Probe::Unknown { address, key } = slot.get() {
            let paths = self.remote_paths();
            match keys::trust_host(&paths.known_hosts(), &address, &key) {
                Ok(()) => {
                    slot.set(Probe::Idle);
                    self.borrow_mut().settings_ui.machines.form.address.clear();
                    self.toast_done((self.tr().toast_trusted)(&address));
                }
                Err(error) => self.toast_failed((self.tr().config_not_saved)(&error.to_string())),
            }
        }
        self.refresh_machines();
    }

    pub fn dismiss_probe(&self) {
        self.borrow().settings_ui.machines.probe.set(Probe::Idle);
        self.borrow_mut().dirty.settings = true;
    }

    /// Removes a server from `known_hosts`. Its key has to be trusted again.
    pub fn forget_machine(&self, address: &str) {
        let paths = self.remote_paths();
        match keys::forget_host(&paths.known_hosts(), address) {
            Ok(()) => self.toast_done((self.tr().toast_forgotten)(address)),
            Err(error) => self.toast_failed((self.tr().config_not_saved)(&error.to_string())),
        }
        self.refresh_machines();
    }

    /// Closes the settings and asks for a project path, starting with
    /// `location` (such as `cmdterm://box:7722/~/`).
    pub fn open_project_at(&self, location: &str) {
        self.close_settings();
        self.open_palette(Some(InputMode::Project));
        self.set_palette_query(location);
    }

    /// Where a repository can be cloned to: this machine, its VMs (built
    /// images start one on first use) and the remote machines it trusts.
    pub fn clone_targets(&self) -> Vec<CloneTarget> {
        let paths = self.remote_paths();
        let mut vms: Vec<String> = ServerConfig::load(&paths)
            .ok()
            .and_then(|config| config.firecracker)
            .map(|fc| {
                local::local_vms(&fc)
                    .into_iter()
                    .map(|vm| vm.name)
                    .collect()
            })
            .unwrap_or_default();
        for preset in &image::PRESETS {
            if image::rootfs_path(&paths, preset).is_file() && !vms.iter().any(|n| n == preset.name)
            {
                vms.push(preset.name.to_owned());
            }
        }
        let hosts = keys::known_hosts_list(&paths.known_hosts()).unwrap_or_default();
        let mut targets = vec![
            CloneTarget::Here {
                devcontainer: false,
            },
            CloneTarget::Here { devcontainer: true },
        ];
        targets.extend(vms.into_iter().map(CloneTarget::Vm));
        for (address, _) in hosts {
            targets.push(CloneTarget::Machine {
                address: address.clone(),
                devcontainer: false,
            });
            targets.push(CloneTarget::Machine {
                address: address.clone(),
                devcontainer: true,
            });
            targets.push(CloneTarget::MachineVm(address));
        }
        targets
    }

    // --- VPN peers ----------------------------------------------------------------

    /// Scans Tailscale, NetBird and Nebula again, on a thread. Does nothing
    /// while a scan runs.
    pub fn refresh_vpn(&self) {
        let slot = self.borrow().settings_ui.machines.vpn.clone();
        if slot.get().scanning {
            return;
        }
        let config = crate::vpn::ScanConfig {
            nebula_config: self
                .borrow()
                .settings
                .vpn
                .nebula_config
                .clone()
                .map(std::path::PathBuf::from),
        };
        slot.update(|scan| scan.scanning = true);
        self.borrow_mut().dirty.settings = true;
        let wake = self.redraw_handle();
        std::thread::spawn(move || {
            let networks = crate::vpn::scan_all(&config);
            slot.set(VpnScan {
                scanning: false,
                networks,
            });
            wake_up(&wake);
        });
    }

    /// Serves this machine on its address `ip` of a VPN, at the port of
    /// the `remote.listen` setting, so only machines on the VPN reach it.
    pub fn serve_on_vpn(&self, ip: std::net::IpAddr) {
        let port = {
            let inner = self.borrow();
            inner
                .settings
                .remote
                .listen
                .parse::<SocketAddr>()
                .map_or(cmdterm_env::REMOTE_DEFAULT_PORT, |address| address.port())
        };
        self.borrow_mut().settings.remote.listen = SocketAddr::new(ip, port).to_string();
        self.set_serving(true);
    }

    /// Fetches the key of a VPN peer's cmdterm remote server, to trust it as
    /// the Remote machines page does.
    pub fn trust_vpn_peer(&self, address: &str) {
        self.borrow_mut().settings_ui.machines.form.address = address.to_owned();
        self.probe_machine();
    }

    // --- machines that connect to this one -------------------------------------

    /// Adds the typed public key line to `authorized_keys`.
    pub fn allow_key(&self) {
        let (line, vm_only) = {
            let inner = self.borrow();
            let form = &inner.settings_ui.machines.form;
            (form.key_line.trim().to_owned(), form.vm_only)
        };
        if keys::PublicKey::parse(&line).is_none() {
            self.toast_failed(self.tr().not_a_key);
            return;
        }
        let options = keys::KeyOptions {
            host: vm_only.then_some(false),
            projects: None,
        };
        let line = options.prefix(&line);
        match keys::authorize(&self.remote_paths().authorized_keys(), &line) {
            Ok(()) => {
                let mut inner = self.borrow_mut();
                inner.settings_ui.machines.form.key_line.clear();
                inner.settings_ui.machines.form.vm_only = false;
                drop(inner);
                self.toast_done(self.tr().toast_key_allowed);
            }
            Err(error) => self.toast_failed((self.tr().config_not_saved)(&error.to_string())),
        }
        self.refresh_machines();
    }

    pub fn set_allow_vm_only(&self, on: bool) {
        let mut inner = self.borrow_mut();
        inner.settings_ui.machines.form.vm_only = on;
        inner.dirty.settings = true;
    }

    /// Gives `key` its own host access in `authorized_keys`, whatever
    /// `offer_host` says. Taking it away limits the key to VMs.
    pub fn set_key_host(&self, key: &PublicKey, on: bool) {
        self.edit_key_options(key, |options| options.host = Some(on));
    }

    /// Gives `key` its own answer to whether it sees this machine's
    /// projects, whatever `share_projects` says. Projects need the host, so
    /// sharing them with a key that has no host access grants it too.
    pub fn set_key_projects(&self, key: &PublicKey, on: bool) {
        let config = self.borrow().settings_ui.machines.files.config.clone();
        self.edit_key_options(key, |options| {
            options.projects = Some(on);
            if on && !config.host_for(options) {
                options.host = Some(true);
            }
        });
    }

    /// Lets `key` follow `server.json` again.
    pub fn reset_key_options(&self, key: &PublicKey) {
        self.edit_key_options(key, |options| *options = keys::KeyOptions::default());
    }

    /// Changes the options of `key` in `authorized_keys`. The server reads
    /// them on every login, so it needs no restart; logged-in sessions keep
    /// what they had.
    fn edit_key_options(&self, key: &PublicKey, edit: impl FnOnce(&mut keys::KeyOptions)) {
        let path = self.remote_paths().authorized_keys();
        let mut options = self
            .borrow()
            .settings_ui
            .machines
            .files
            .allowed
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| entry.options)
            .unwrap_or_default();
        edit(&mut options);
        match keys::set_options(&path, key, options) {
            Ok(()) => self.toast_done(self.tr().toast_saved),
            Err(error) => self.toast_failed((self.tr().config_not_saved)(&error.to_string())),
        }
        self.refresh_machines();
    }

    /// Removes a key from `authorized_keys`. Its sessions that are logged in
    /// keep running until they end.
    pub fn remove_allowed(&self, key: &PublicKey) {
        match keys::unauthorize(&self.remote_paths().authorized_keys(), key) {
            Ok(_) => self.toast_done(self.tr().toast_key_removed),
            Err(error) => self.toast_failed((self.tr().config_not_saved)(&error.to_string())),
        }
        self.refresh_machines();
    }

    // --- VMs -------------------------------------------------------------------

    /// Writes the Firecracker form to `server.json`. An empty field removes
    /// its setting. Without kernel, root filesystem and images the section
    /// goes, which turns VMs off.
    pub fn save_vm_settings(&self) {
        let form = self.borrow().settings_ui.machines.form.vm.clone();
        let text = |field: VmField| form.get(field as usize).map_or("", |s| s.trim());
        let mut values = Vec::new();
        for field in VmField::ALL {
            let value = text(field);
            let value = if value.is_empty() {
                None
            } else if field.numeric() {
                let Ok(number) = value.parse::<u64>() else {
                    self.toast_failed((self.tr().not_a_number)(value));
                    return;
                };
                Some(Value::from(number))
            } else {
                Some(Value::from(value))
            };
            values.push((field.key(), value));
        }
        let saved = self.edit_server_config(move |map| {
            let entry = map
                .entry("firecracker")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(serde_json::Map::new());
            }
            let Value::Object(fc) = entry else { return };
            // Fields the form does not show, such as images, stay.
            for (key, value) in values {
                match value {
                    Some(value) => {
                        fc.insert(key.into(), value);
                    }
                    None => {
                        fc.remove(key);
                    }
                }
            }
            let images = fc
                .get("images")
                .and_then(Value::as_object)
                .is_some_and(|images| !images.is_empty());
            if !fc.contains_key("kernel") && !fc.contains_key("rootfs") && !images {
                map.remove("firecracker");
            }
        });
        if saved {
            self.toast_done(self.tr().vm_settings_saved);
        }
    }

    /// Set up or Rebuild on a ready-made image: the first press says what
    /// it downloads and builds, the second starts it.
    pub fn arm_or_build_image(&self, name: &'static str) {
        let (armed, armed_at) = {
            let ui = &self.borrow().settings_ui.machines;
            (ui.armed, ui.armed_at)
        };
        if armed == Some(name) {
            if armed_at.is_some_and(|at| at.elapsed() < ARM_SETTLE) {
                return;
            }
            self.borrow_mut().settings_ui.machines.armed = None;
            self.build_image(name);
        } else {
            let mut inner = self.borrow_mut();
            inner.settings_ui.machines.armed = Some(name);
            inner.settings_ui.machines.armed_at = Some(std::time::Instant::now());
            inner.dirty.settings = true;
        }
    }

    /// Cancel beside an armed Start: the row goes back to Set up.
    pub fn disarm_image(&self) {
        let mut inner = self.borrow_mut();
        inner.settings_ui.machines.armed = None;
        inner.dirty.settings = true;
    }

    /// Builds a ready-made image on a thread and adds it to `server.json`.
    /// The page shows each step; the full output goes to a log file.
    pub fn build_image(&self, name: &str) {
        let Some(preset) = image::preset(name) else {
            return;
        };
        let jobs = self.borrow().settings_ui.machines.jobs.clone();
        if matches!(jobs.get().get(preset.name), Some(ImageJob::Running(_))) {
            return;
        }
        let paths = self.remote_paths();
        let options = image::BuildOptions {
            engine: self.borrow().settings.container_engine.clone(),
            guest: cmdterm_env::remote_client().program.clone(),
            log: (self.borrow().dirs.logs).join(format!("vm-image-{}.log", preset.name)),
        };
        let wake = self.redraw_handle();
        let tr = self.tr();
        jobs.update(|jobs| {
            jobs.insert(preset.name, ImageJob::Running(tr.preparing.to_owned()));
        });
        self.borrow_mut()
            .settings_ui
            .machines
            .applied
            .retain(|n| *n != preset.name);
        self.borrow_mut().dirty.settings = true;
        std::thread::spawn(move || {
            let progress_jobs = jobs.clone();
            let progress_wake = wake.clone();
            let built = image::build(&paths, preset, &options, move |step| {
                progress_jobs.update(|jobs| {
                    jobs.insert(preset.name, ImageJob::Running(step.to_owned()));
                });
                wake_up(&progress_wake);
            })
            .and_then(|_| image::configure(&paths, preset));
            let job = match built {
                Ok(_) => ImageJob::Done,
                Err(error) => ImageJob::Failed(error.to_string()),
            };
            jobs.update(|jobs| {
                jobs.insert(preset.name, job);
            });
            wake_up(&wake);
        });
    }

    /// Applies finished image builds: reads `server.json` again and serves
    /// with it. Called from `poll` when a build changed.
    pub(crate) fn image_jobs_changed(&self) {
        let finished = {
            let mut inner = self.borrow_mut();
            inner.dirty.settings = true;
            let tr = inner.tr;
            let ui = &mut inner.settings_ui.machines;
            let mut finished = false;
            let mut toasts = Vec::new();
            for (name, job) in ui.jobs.get() {
                if !matches!(job, ImageJob::Running(_)) && !ui.applied.contains(&name) {
                    ui.applied.push(name);
                    finished = true;
                    toasts.push(match job {
                        ImageJob::Failed(error) => (true, (tr.toast_image_failed)(name, &error)),
                        _ => (false, (tr.toast_image_ready)(name)),
                    });
                }
            }
            for (failed, text) in toasts {
                inner.toast(failed, text);
            }
            finished
        };
        if finished {
            self.reload_server();
            self.refresh_machines();
        }
    }

    pub fn toggle_advanced_vm_settings(&self) {
        let mut inner = self.borrow_mut();
        let ui = &mut inner.settings_ui.machines;
        ui.advanced = !ui.advanced;
        if ui.advanced {
            // Show what server.json holds now, images may have changed it.
            ui.form.vm.clear();
        }
        drop(inner);
        self.refresh_machines();
    }

    /// Stops the local VM daemon and its VMs.
    pub fn stop_local_vms(&self) {
        match local::stop_vmd(&self.remote_paths()) {
            Ok(true) => self.toast_done(self.tr().vms_stopping),
            Ok(false) => {}
            Err(error) => self.toast_failed((self.tr().error)(&error.to_string())),
        }
        self.refresh_machines();
    }

    /// Deletes a stopped local VM's disk.
    pub fn delete_local_vm(&self, name: &str) {
        let config = self
            .borrow()
            .settings_ui
            .machines
            .files
            .config
            .firecracker
            .clone();
        if let Some(config) = config {
            match local::delete_local_vm(&config, name) {
                Ok(_) => self.toast_done((self.tr().toast_disk_deleted)(name)),
                Err(error) => self.toast_failed((self.tr().error)(&error.to_string())),
            }
        }
        self.refresh_machines();
    }

    /// Asks for a project path in the VM typed into the name field.
    pub fn open_new_vm(&self) {
        let name = self
            .borrow()
            .settings_ui
            .machines
            .form
            .vm_name
            .trim()
            .to_owned();
        if !cmdterm_remote::vm::valid_name(&name) {
            self.toast_failed(self.tr().bad_vm_name);
            return;
        }
        self.open_project_at(&format!("vm://{name}/~/"));
    }

    // --- containers and sandbox ------------------------------------------------

    /// `None` picks docker when the host has it, else podman.
    pub fn set_container_engine(&self, engine: Option<&str>) {
        {
            let mut inner = self.borrow_mut();
            let engine = engine.map(str::to_owned);
            if inner.settings.container_engine == engine {
                return;
            }
            inner.settings.container_engine = engine;
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    pub fn set_sandbox_network(&self, on: bool) {
        self.borrow_mut().settings.sandbox.network = on;
        self.borrow_mut().dirty.settings = true;
        self.save_settings();
        self.warn_unscoped_host_network();
    }

    pub fn set_sandbox_restrict_network(&self, on: bool) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            inner.settings.sandbox.restrict_network = on;
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.settings = true;
        }
        self.save_settings();
        self.warn_unscoped_host_network();
    }

    /// Sets whether local sandboxes keep the host's network. Running
    /// sandboxes keep what they started with.
    pub fn set_sandbox_host_network(&self, on: bool) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            inner.settings.sandbox.host_network = on;
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.settings = true;
        }
        self.save_settings();
        self.warn_unscoped_host_network();
    }

    /// Tells the user once per run when local bubblewrap sandboxes keep the
    /// host's network on a kernel that cannot keep them from this machine's
    /// abstract Unix sockets, X11's among them (see
    /// `cmdterm_env::landlock_scopes`).
    pub(crate) fn warn_unscoped_host_network(&self) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let shares = !crate::runtime::isolates(&self.borrow().settings.sandbox);
        if !shares
            || cmdterm_env::SandboxKind::native() != cmdterm_env::SandboxKind::Bwrap
            || cmdterm_env::landlock_scopes()
            || WARNED.swap(true, Ordering::Relaxed)
        {
            return;
        }
        self.notify(self.tr().sandbox_host_network_unscoped);
    }

    /// Sets what happens to sandbox requests that extension guard `id`
    /// matches.
    pub fn set_guard_mode(&self, id: &str, mode: cmdterm_env::GuardMode) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let kind = (crate::runtime::all_guards(inner.ext.sandbox_rules()).into_iter())
                .find(|guard| guard.id == id)
                .and_then(|guard| guard.kind);
            if !inner.settings.sandbox.set_guard_mode(id, kind, mode) {
                return;
            }
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Sets what happens to sandbox requests on change requests of `kind`,
    /// on every forge, for guards without a mode of their own.
    pub fn set_change_request_mode(
        &self,
        kind: cmdterm_env::GuardKind,
        mode: cmdterm_env::GuardMode,
    ) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            if inner.settings.sandbox.change_requests.get(kind) == mode {
                return;
            }
            inner.settings.sandbox.change_requests.set(kind, mode);
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Sets how sandboxes reach container engines. Leaving sidecars stops
    /// the running ones.
    pub fn set_sandbox_engines(&self, engines: crate::state::EngineAccess) {
        self.close_picker();
        let left_sidecars = {
            let mut inner = self.borrow_mut();
            let before = std::mem::replace(&mut inner.settings.sandbox.engines, engines);
            // Picking passthrough is a choice the status bar need not
            // warn about.
            if engines == crate::state::EngineAccess::Passthrough {
                inner.settings.sandbox.engines_acknowledged = true;
                inner.dirty.status = true;
            }
            inner.dirty.settings = true;
            before == crate::state::EngineAccess::Sidecar && before != engines
        };
        if left_sidecars {
            self.stop_sidecars(false);
        }
        self.save_settings();
    }

    /// Keeps engine passthrough and stops the status bar's warning about it.
    pub fn acknowledge_sandbox_engines(&self) {
        {
            let mut inner = self.borrow_mut();
            inner.settings.sandbox.engines_acknowledged = true;
            inner.dirty.settings = true;
            inner.dirty.status = true;
        }
        self.save_settings();
    }

    /// Picks the engine that runs inside sidecars.
    /// Running sidecars of the other engine stop, so the next terminals start
    /// the new one.
    pub fn set_sidecar_engine(&self, engine: crate::state::SidecarEngine) {
        self.close_picker();
        let changed = {
            let mut inner = self.borrow_mut();
            inner.dirty.settings = true;
            std::mem::replace(&mut inner.settings.sandbox.sidecar.engine, engine) != engine
        };
        if changed {
            self.stop_sidecars(false);
        }
        self.save_settings();
    }

    pub fn set_sandbox_intercept_tls(&self, on: bool) {
        self.borrow_mut().settings.sandbox.intercept_tls = on;
        self.borrow_mut().dirty.settings = true;
        self.save_settings();
    }

    /// Adds the typed host to the sandbox's allowed hosts.
    pub fn add_sandbox_host(&self) {
        {
            let mut inner = self.borrow_mut();
            let host = inner
                .settings_ui
                .machines
                .form
                .sandbox_host
                .trim()
                .to_owned();
            if host.is_empty() {
                return;
            }
            if !cmdterm_env::valid_host_pattern(&host) {
                drop(inner);
                self.toast_failed(self.tr().bad_host);
                return;
            }
            let hosts = &mut inner.settings.sandbox.allowed_hosts;
            if !hosts.contains(&host) {
                hosts.push(host.clone());
            }
            let inner = &mut *inner;
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.settings_ui.machines.form.sandbox_host.clear();
            inner.dirty.settings = true;
            inner.toast(false, (inner.tr.toast_host_added)(&host));
        }
        self.save_settings();
    }

    /// Opens or closes a group of hosts on the Hosts page.
    pub fn toggle_host_group(&self, group: usize) {
        let mut inner = self.borrow_mut();
        let flipped = &mut inner.settings_ui.machines.form.flipped_host_groups;
        match flipped.iter().position(|&g| g == group) {
            Some(at) => {
                flipped.swap_remove(at);
            }
            None => flipped.push(group),
        }
        inner.dirty.settings = true;
    }

    pub fn remove_sandbox_host(&self, host: &str) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            inner.settings.sandbox.allowed_hosts.retain(|h| h != host);
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.settings_ui.machines.form.removed_host = Some(host.to_owned());
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Puts the host removed last back in the allowed hosts.
    pub fn undo_remove_sandbox_host(&self) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let Some(host) = inner.settings_ui.machines.form.removed_host.take() else {
                return;
            };
            if !inner.settings.sandbox.allowed_hosts.contains(&host) {
                inner.settings.sandbox.allowed_hosts.push(host);
            }
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Adds the typed path to the sandbox's writable or hidden list.
    pub fn add_sandbox_path(&self, writable: bool) {
        {
            let mut inner = self.borrow_mut();
            let path = inner
                .settings_ui
                .machines
                .form
                .sandbox_path
                .trim()
                .to_owned();
            if path.is_empty() {
                return;
            }
            let sandbox = &mut inner.settings.sandbox;
            let list = if writable {
                &mut sandbox.writable
            } else {
                &mut sandbox.hidden
            };
            if !list.contains(&path) {
                list.push(path.clone());
            }
            inner.settings_ui.machines.form.sandbox_path.clear();
            inner.dirty.settings = true;
            let added = (inner.tr.toast_path_added)(&path);
            inner.toast(false, added);
        }
        self.save_settings();
    }

    pub fn remove_sandbox_path(&self, writable: bool, path: &str) {
        {
            let mut inner = self.borrow_mut();
            let sandbox = &mut inner.settings.sandbox;
            let list = if writable {
                &mut sandbox.writable
            } else {
                &mut sandbox.hidden
            };
            list.retain(|p| p != path);
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Adds or updates the typed environment variable on the active project.
    pub fn add_project_env_var(&self) {
        let (id, name, value) = {
            let mut inner = self.borrow_mut();
            let tr = inner.tr;
            let Some(id) = inner.state.active_project else {
                return;
            };
            let name = inner
                .settings_ui
                .machines
                .form
                .project_env_name
                .trim()
                .to_owned();
            if !crate::project_env::valid_var_name(&name) {
                inner.settings_ui.machines.form.project_env_error =
                    Some(tr.project_env_invalid_name.to_owned());
                inner.dirty.settings = true;
                return;
            }
            let value = std::mem::take(&mut inner.settings_ui.machines.form.project_env_value);
            if value.contains('\0') {
                inner.settings_ui.machines.form.project_env_error =
                    Some(tr.project_env_invalid_value.to_owned());
                inner.dirty.settings = true;
                return;
            }
            inner.settings_ui.machines.form.project_env_name.clear();
            inner.settings_ui.machines.form.project_env_error = None;
            (id, name, value)
        };
        self.set_project_env_var(id, &name, &value);
    }

    /// Adds the typed folder to the active project's writable or read-only
    /// sandbox folders.
    pub fn add_project_sandbox_path(&self, writable: bool) {
        {
            let mut inner = self.borrow_mut();
            let path = inner
                .settings_ui
                .machines
                .form
                .project_path
                .trim()
                .to_owned();
            let Some(id) = inner.state.active_project else {
                return;
            };
            let Some(project) = inner.state.project_mut(id).filter(|_| !path.is_empty()) else {
                return;
            };
            let list = project.sandbox.list_mut(writable);
            if !list.contains(&path) {
                list.push(path.clone());
            }
            inner.settings_ui.machines.form.project_path.clear();
            inner.dirty.settings = true;
            let added = (inner.tr.toast_path_added)(&path);
            inner.toast(false, added);
        }
        self.save();
    }

    pub fn remove_project_sandbox_path(&self, writable: bool, path: &str) {
        {
            let mut inner = self.borrow_mut();
            let Some(id) = inner.state.active_project else {
                return;
            };
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            project.sandbox.list_mut(writable).retain(|p| p != path);
            inner.dirty.settings = true;
        }
        self.save();
    }

    /// Keeps the reference branch typed for project `id`.
    pub fn set_project_base_draft(&self, id: u64, typed: &str) {
        self.borrow_mut().settings_ui.machines.form.project_base = Some((id, typed.to_owned()));
    }

    /// Sets the reference branch typed for project `id`, if any was typed.
    pub fn apply_project_base_draft(&self, id: u64) {
        let draft = self
            .borrow_mut()
            .settings_ui
            .machines
            .form
            .project_base
            .take();
        let Some((_, typed)) = draft.filter(|(draft_id, _)| *draft_id == id) else {
            return;
        };
        self.set_project_base(id, &typed);
        let mut inner = self.borrow_mut();
        inner.dirty.settings = true;
        let base = inner.state.project(id).and_then(|p| p.base.clone());
        let done = (inner.tr.toast_project_base)(base.as_deref());
        inner.toast(false, done);
    }

    /// Whether agents in project `id` may create worktrees and start agents
    /// there without asking, as the launch dialog's "Always allow" does.
    pub fn set_project_launches(&self, id: u64, on: bool) {
        if !on {
            self.revoke_launches(id);
            return;
        }
        {
            let mut inner = self.borrow_mut();
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            project.allows_launches = true;
            inner.dirty.settings = true;
        }
        self.save();
    }

    /// The runtime rows of the Containers page: where the active worktree's
    /// terminals run.
    /// Picks a container engine from the settings dropdown and closes it.
    pub fn pick_container_engine(&self, engine: Option<&str>) {
        self.close_picker();
        self.set_container_engine(engine);
    }

    /// Picks where the active worktree's terminals run from the settings
    /// dropdown and closes it.
    pub fn pick_runtime(&self, runtime: Runtime) {
        self.close_picker();
        self.set_runtime(runtime);
        self.save();
        self.borrow_mut().dirty.settings = true;
    }

    /// Starts the active worktree's terminals again, so they run with the
    /// sandbox settings as they are now. Their programs end.
    pub fn restart_active_terminals(&self) {
        let Some((id, path)) = self.borrow().active_key() else {
            return;
        };
        self.restart_terminals(id, &path, true);
        let mut inner = self.borrow_mut();
        inner.settings_ui.sandbox_applied = Some(inner.settings.sandbox.clone());
        inner.dirty.settings = true;
    }

    pub fn rebuild_devcontainer_from_settings(&self) {
        self.rebuild_devcontainer();
        self.borrow_mut().dirty.settings = true;
    }
}

#[cfg(test)]
mod tests {
    use crate::state::{SettingsStore, StateStore};

    use super::*;

    fn workspace(root: &std::path::Path) -> Workspace {
        Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        )
    }

    #[test]
    fn trusting_a_served_machine_and_forgetting_it() {
        let dir = tempfile::tempdir().unwrap();
        let server = workspace(&dir.path().join("server"));
        let (bound, _) = server.serve("127.0.0.1:0").unwrap();
        let client = workspace(&dir.path().join("client"));
        client.refresh_machines();
        client.set_machine_text(MachineText::Address, &bound.to_string());
        client.probe_machine();
        let slot = client.borrow().settings_ui.machines.probe.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(slot.get(), Probe::Fetching(_)) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let Probe::Unknown { key, .. } = slot.get() else {
            panic!("{:?}", slot.get());
        };
        server.refresh_machines();
        assert_eq!(
            server.borrow().settings_ui.machines.files.own_key,
            Some(*key)
        );
        client.trust_probed();
        let files = client.borrow().settings_ui.machines.files.clone();
        assert_eq!(files.known_hosts.len(), 1);
        assert_eq!(files.known_hosts[0].0, bound.to_string());
        client.forget_machine(&bound.to_string());
        assert!(
            client
                .borrow()
                .settings_ui
                .machines
                .files
                .known_hosts
                .is_empty()
        );
        server.stop_serving();
        assert!(server.serving_on().is_none());
    }

    #[test]
    fn allowed_keys_and_vm_settings_round_trip_through_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        ws.refresh_machines();
        let line = keys::Identity::from_seed(&[5; 64])
            .public()
            .to_line("laptop");
        ws.set_machine_text(MachineText::KeyLine, &line);
        ws.set_allow_vm_only(true);
        ws.allow_key();
        let allowed = ws.borrow().settings_ui.machines.files.allowed.clone();
        assert_eq!(allowed.len(), 1);
        assert!(allowed[0].options.vm_only());
        assert_eq!(allowed[0].comment, "laptop");

        // A key's own settings beat server.json's, until reset.
        let key = allowed[0].key.clone();
        ws.set_offer_host(false);
        ws.set_share_projects(false);
        ws.set_key_projects(&key, true);
        let options = ws.borrow().settings_ui.machines.files.allowed[0].options;
        assert_eq!(
            options,
            keys::KeyOptions {
                host: Some(true),
                projects: Some(true)
            },
            "projects bring host access along"
        );
        let config = ws.borrow().settings_ui.machines.files.config.clone();
        assert!(config.host_for(&options) && config.projects_for(&options));
        ws.set_key_host(&key, false);
        let options = ws.borrow().settings_ui.machines.files.allowed[0].options;
        assert!(options.vm_only() && !config.projects_for(&options));
        ws.reset_key_options(&key);
        let allowed = ws.borrow().settings_ui.machines.files.allowed.clone();
        assert_eq!(allowed[0].options, keys::KeyOptions::default());
        assert_eq!(allowed[0].comment, "laptop");
        ws.remove_allowed(&allowed[0].key);
        assert!(ws.borrow().settings_ui.machines.files.allowed.is_empty());

        // Defaults until saved; saving writes the section.
        assert!(
            ws.borrow()
                .settings_ui
                .machines
                .files
                .config
                .firecracker
                .is_none()
        );
        ws.set_machine_text(MachineText::Vm(VmField::Kernel), "/k/vmlinux");
        ws.set_machine_text(MachineText::Vm(VmField::Rootfs), "/k/rootfs.ext4");
        ws.set_machine_text(MachineText::Vm(VmField::MemMib), "2048");
        ws.save_vm_settings();
        let fc = ws
            .borrow()
            .settings_ui
            .machines
            .files
            .config
            .firecracker
            .clone()
            .unwrap();
        assert_eq!(fc.mem_mib, 2048);
        assert_eq!(fc.kernel, std::path::Path::new("/k/vmlinux"));
        assert!(fc.state_dir.ends_with("cfg/remote/vms"));

        ws.set_machine_text(MachineText::Vm(VmField::Vcpus), "many");
        ws.save_vm_settings();
        assert_eq!(
            ws.borrow()
                .settings_ui
                .machines
                .files
                .config
                .firecracker
                .as_ref()
                .unwrap()
                .vcpus,
            2
        );

        ws.set_machine_text(MachineText::Vm(VmField::Vcpus), "2");
        ws.set_machine_text(MachineText::Vm(VmField::Kernel), "");
        ws.set_machine_text(MachineText::Vm(VmField::Rootfs), "");
        ws.save_vm_settings();
        assert!(
            ws.borrow()
                .settings_ui
                .machines
                .files
                .config
                .firecracker
                .is_none()
        );
    }

    /// Builds the Alpine image with podman or docker, boots a VM from it and
    /// reaches the internet from inside. Needs KVM, network access and
    /// `cargo build -p cmdterm-remote`; run with `--ignored`.
    #[test]
    #[ignore]
    fn a_ready_made_image_builds_and_boots_with_network() {
        let dir =
            tempfile::tempdir_in(std::env::var_os("TMPDIR").unwrap_or("/var/tmp".into())).unwrap();
        let remote = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("cmdterm-remote");
        assert!(remote.is_file(), "build cmdterm-remote first");
        cmdterm_env::set_remote_client(cmdterm_env::RemoteClient {
            program: remote,
            config_dir: Some(dir.path().join("cfg")),
        });
        let ws = workspace(dir.path());
        ws.refresh_machines();
        assert!(
            ws.borrow()
                .settings_ui
                .machines
                .files
                .missing_tools
                .is_empty()
        );
        ws.build_image("alpine");
        let jobs = ws.borrow().settings_ui.machines.jobs.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while matches!(jobs.get().get("alpine"), Some(ImageJob::Running(_))) {
            assert!(std::time::Instant::now() < deadline, "build timed out");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(
            matches!(jobs.get()["alpine"], ImageJob::Done),
            "{:?}",
            jobs.get()
        );
        ws.poll();
        assert_eq!(ws.borrow().settings_ui.machines.files.built, ["alpine"]);

        let vm = cmdterm_env::Environment::Vm(cmdterm_env::VmTarget {
            name: "alpine-test".into(),
        });
        let out = vm
            .command("sh")
            .args(["-c", "cat /etc/alpine-release && curl -fsS -o /dev/null https://github.com && echo online"])
            .stdout()
            .unwrap();
        let out = String::from_utf8_lossy(&out);
        assert!(out.starts_with("3.22"), "{out}");
        assert!(out.contains("online"), "{out}");
        ws.refresh_machines();
        let vms = ws.borrow().settings_ui.machines.files.vms.clone();
        assert_eq!(vms[0].image.as_deref(), Some("alpine"));
        ws.stop_local_vms();
    }

    #[test]
    fn sandbox_lists_and_engine_save_to_settings() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        ws.set_machine_text(MachineText::SandboxPath, "~/.npm");
        ws.add_sandbox_path(true);
        ws.remove_sandbox_path(false, "~/.ssh");
        ws.set_sandbox_network(false);
        ws.set_sandbox_restrict_network(false);
        ws.set_sandbox_host_network(true);
        ws.set_sandbox_intercept_tls(false);
        ws.set_sandbox_engines(crate::state::EngineAccess::Off);
        ws.set_machine_text(MachineText::SandboxHost, "*.example.com");
        ws.add_sandbox_host();
        ws.set_machine_text(MachineText::SandboxHost, "not a host");
        ws.add_sandbox_host();
        ws.remove_sandbox_host("github.com");
        ws.set_container_engine(Some("podman"));
        let saved = SettingsStore::new(dir.path().join("cfg/settings.json"))
            .load()
            .unwrap();
        assert!(saved.sandbox.writable.contains(&"~/.npm".to_owned()));
        assert!(!saved.sandbox.hidden.contains(&"~/.ssh".to_owned()));
        assert!(!saved.sandbox.network);
        assert!(!saved.sandbox.restrict_network);
        assert!(saved.sandbox.host_network);
        assert!(!saved.sandbox.intercept_tls);
        assert_eq!(saved.sandbox.engines, crate::state::EngineAccess::Off);
        let hosts = &saved.sandbox.allowed_hosts;
        assert!(hosts.contains(&"*.example.com".to_owned()));
        assert!(!hosts.contains(&"not a host".to_owned()));
        assert!(!hosts.contains(&"github.com".to_owned()));
        assert_eq!(saved.container_engine.as_deref(), Some("podman"));
    }
}
