//! Peers of the mesh VPNs on this machine: Tailscale, NetBird and Nebula.
//!
//! Tailscale and NetBird report their peers as JSON (`tailscale status
//! --json`, `netbird status --json`). Nebula has no such command, so its
//! peers come from its configuration (the lighthouses and `static_host_map`)
//! and from `/etc/hosts` entries inside its network. Every scan starts
//! processes or reads files, so it runs on a thread.
//!
//! A peer is reached like any other machine: over SSH or as a cmdterm remote
//! machine, at its VPN address. `tailscale://box/~/src` and the other VPN
//! locations name the peer instead of the address; the VCS worker resolves
//! them with [`VpnLocation::resolve`] before it discovers the repository.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use cmdterm_env::{Environment, RemoteTarget, SshTarget};
use serde_json::Value;

/// A mesh VPN scriptschnell-term knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Vpn {
    Tailscale,
    Netbird,
    Nebula,
}

impl Vpn {
    pub const ALL: [Vpn; 3] = [Vpn::Tailscale, Vpn::Netbird, Vpn::Nebula];

    /// The scheme of its locations, such as `tailscale` in
    /// `tailscale://box/~/src`.
    pub fn scheme(self) -> &'static str {
        match self {
            Vpn::Tailscale => "tailscale",
            Vpn::Netbird => "netbird",
            Vpn::Nebula => "nebula",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Vpn::Tailscale => "Tailscale",
            Vpn::Netbird => "NetBird",
            Vpn::Nebula => "Nebula",
        }
    }
}

/// A machine on a VPN.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peer {
    /// Short host name, such as `box`.
    pub name: String,
    /// A DNS name the VPN resolves, such as `box.tailnet.ts.net`. Only set
    /// when the VPN's DNS is on, since only then does it work as an address.
    pub dns_name: Option<String>,
    pub ips: Vec<IpAddr>,
    /// `None` when the VPN does not say.
    pub online: Option<bool>,
    pub os: Option<String>,
}

impl Peer {
    /// The address to connect to: the DNS name, else the first IPv4
    /// address, else the first address.
    pub fn address(&self) -> Option<String> {
        if let Some(name) = &self.dns_name {
            return Some(name.clone());
        }
        let ip = self
            .ips
            .iter()
            .find(|ip| ip.is_ipv4())
            .or(self.ips.first())?;
        Some(ip.to_string())
    }

    /// How a VPN location names this peer: the first label of its DNS
    /// name, else its name when it is a plain host name, else its address.
    pub fn location_name(&self) -> Option<String> {
        if let Some(label) = self
            .dns_name
            .as_deref()
            .and_then(|dns| dns.split('.').next())
        {
            return Some(label.to_owned());
        }
        let plain = !self.name.is_empty()
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if plain {
            return Some(self.name.clone());
        }
        self.address()
    }

    /// How well `query` names this peer, lower is better: 0 for its DNS
    /// name or one of its addresses, 1 for the first label of its DNS name,
    /// 2 for its name. `None` when it does not. Case does not matter.
    /// The name is what the machine calls itself, so another peer can
    /// share it; DNS names are unique on the VPN.
    fn answers_to(&self, query: &str) -> Option<u8> {
        let ip = query
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>();
        if ip.is_ok_and(|ip| self.ips.contains(&ip)) {
            return Some(0);
        }
        if let Some(dns) = &self.dns_name {
            if dns.eq_ignore_ascii_case(query.trim_end_matches('.')) {
                return Some(0);
            }
            if dns
                .split('.')
                .next()
                .is_some_and(|label| label.eq_ignore_ascii_case(query))
            {
                return Some(1);
            }
        }
        self.name.eq_ignore_ascii_case(query).then_some(2)
    }
}

/// Whether a VPN is usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Its command line tool and configuration were not found.
    Missing,
    /// Installed but not connected, with why.
    Down(String),
    Up,
}

/// What a scan of one VPN found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub vpn: Vpn,
    pub state: State,
    /// This machine on the VPN.
    pub this: Option<Peer>,
    /// The other machines, online ones first, then by name.
    pub peers: Vec<Peer>,
}

impl Network {
    fn new(vpn: Vpn, state: State) -> Self {
        Self {
            vpn,
            state,
            this: None,
            peers: Vec::new(),
        }
    }

    fn sort(&mut self) {
        self.peers.sort_by(|a, b| {
            let a_on = a.online != Some(false);
            let b_on = b.online != Some(false);
            b_on.cmp(&a_on).then_with(|| a.name.cmp(&b.name))
        });
    }

    /// The peer `query` names best, see [`Peer::answers_to`]; the first
    /// one of those that name it equally well.
    pub fn find(&self, query: &str) -> Option<&Peer> {
        (self.peers.iter())
            .filter_map(|peer| Some((peer.answers_to(query)?, peer)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, peer)| peer)
    }
}

/// Where scans look, from `settings.json`'s `vpn` section.
#[derive(Clone, Debug, Default)]
pub struct ScanConfig {
    /// Nebula's configuration file or directory. `None` looks at the
    /// running `nebula` process, then the usual places.
    pub nebula_config: Option<PathBuf>,
}

/// Scans every VPN. Blocking.
pub fn scan_all(config: &ScanConfig) -> Vec<Network> {
    Vpn::ALL.iter().map(|&vpn| scan(vpn, config)).collect()
}

/// Scans one VPN. Blocking.
pub fn scan(vpn: Vpn, config: &ScanConfig) -> Network {
    let mut network = match vpn {
        Vpn::Tailscale => scan_tailscale(),
        Vpn::Netbird => scan_netbird(),
        Vpn::Nebula => scan_nebula(config.nebula_config.as_deref()),
    };
    network.sort();
    network
}

/// Runs the first of `programs` that exists with `args`. `None` when none
/// of them does.
fn run_first(programs: &[&str], args: &[&str]) -> Option<std::process::Output> {
    for program in programs {
        match Environment::Local.command(*program).args(args).output() {
            Ok(output) => return Some(output),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        }
    }
    None
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// The first non-empty line of a failed command's output.
fn first_line(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("not running")
        .to_owned()
}

// --- Tailscale -------------------------------------------------------------------

fn scan_tailscale() -> Network {
    // The macOS app keeps its command line tool inside the bundle.
    let programs = [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ];
    let Some(output) = run_first(&programs, &["status", "--json"]) else {
        return Network::new(Vpn::Tailscale, State::Missing);
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(json) => parse_tailscale(&json),
        Err(_) => Network::new(Vpn::Tailscale, State::Down(first_line(&output))),
    }
}

fn parse_tailscale(json: &Value) -> Network {
    let backend = json["BackendState"].as_str().unwrap_or_default();
    if backend != "Running" {
        let why = if backend.is_empty() {
            "not running"
        } else {
            backend
        };
        return Network::new(Vpn::Tailscale, State::Down(why.to_owned()));
    }
    let magic_dns = json["CurrentTailnet"]["MagicDNSEnabled"]
        .as_bool()
        .unwrap_or(false);
    let peer = |node: &Value| Peer {
        name: node["HostName"].as_str().unwrap_or_default().to_owned(),
        dns_name: node["DNSName"]
            .as_str()
            .map(|name| name.trim_end_matches('.'))
            .filter(|name| magic_dns && !name.is_empty())
            .map(str::to_owned),
        ips: node["TailscaleIPs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|ip| ip.as_str()?.parse().ok())
            .collect(),
        online: node["Online"].as_bool(),
        os: node["OS"]
            .as_str()
            .filter(|os| !os.is_empty())
            .map(str::to_owned),
    };
    let mut network = Network::new(Vpn::Tailscale, State::Up);
    network.this = json.get("Self").map(peer);
    if let Some(peers) = json["Peer"].as_object() {
        network.peers = peers.values().map(peer).collect();
    }
    network
}

// --- NetBird ---------------------------------------------------------------------

fn scan_netbird() -> Network {
    let Some(output) = run_first(&["netbird"], &["status", "--json"]) else {
        return Network::new(Vpn::Netbird, State::Missing);
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(json) if output.status.success() => parse_netbird(&json),
        _ => Network::new(Vpn::Netbird, State::Down(first_line(&output))),
    }
}

/// The address part of `100.64.0.1/16`.
fn without_prefix(ip: &str) -> Option<IpAddr> {
    ip.split('/').next()?.parse().ok()
}

fn parse_netbird(json: &Value) -> Network {
    let own_ip = json["netbirdIp"].as_str().and_then(without_prefix);
    let managed = json["management"]["connected"].as_bool().unwrap_or(false);
    let Some(own_ip) = own_ip.filter(|_| managed) else {
        return Network::new(Vpn::Netbird, State::Down("not connected".to_owned()));
    };
    let short = |fqdn: &str| fqdn.split('.').next().unwrap_or(fqdn).to_owned();
    let mut network = Network::new(Vpn::Netbird, State::Up);
    let own_fqdn = json["fqdn"].as_str().unwrap_or_default();
    network.this = Some(Peer {
        name: short(own_fqdn),
        ips: std::iter::once(own_ip)
            .chain(json["netbirdIpv6"].as_str().and_then(without_prefix))
            .collect(),
        online: Some(true),
        ..Peer::default()
    });
    // NetBird's DNS can be turned off per client, and the status does not
    // say whether it is, so peers are reached by address.
    network.peers = json["peers"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|peer| {
            let ip = peer["netbirdIp"].as_str().and_then(without_prefix)?;
            let ipv6 = peer["netbirdIpv6"].as_str().and_then(without_prefix);
            let fqdn = peer["fqdn"].as_str().unwrap_or_default();
            let status = peer["status"].as_str().unwrap_or_default();
            Some(Peer {
                name: if fqdn.is_empty() {
                    ip.to_string()
                } else {
                    short(fqdn)
                },
                dns_name: None,
                ips: std::iter::once(ip).chain(ipv6).collect(),
                // Idle peers connect on first use.
                online: Some(matches!(status, "Connected" | "Idle")),
                os: None,
            })
        })
        .collect();
    network
}

// --- Nebula ----------------------------------------------------------------------

/// What scriptschnell-term reads from a Nebula configuration.
#[derive(Debug, Default, PartialEq, Eq)]
struct NebulaConfig {
    /// `pki.cert`: a path, or the certificate itself.
    cert: Option<String>,
    /// `tun.dev`, the interface name.
    dev: Option<String>,
    /// `lighthouse.hosts`, overlay addresses.
    lighthouses: Vec<IpAddr>,
    /// Keys of `static_host_map`, overlay addresses.
    static_hosts: Vec<IpAddr>,
}

/// Where Nebula's packages and guides put the configuration.
const NEBULA_CONFIGS: [&str; 5] = [
    "/etc/nebula/config.yml",
    "/etc/nebula/config.yaml",
    "/etc/nebula",
    "/usr/local/etc/nebula/config.yml",
    "/opt/homebrew/etc/nebula/config.yml",
];

fn scan_nebula(configured: Option<&Path>) -> Network {
    let running = running_nebula_configs();
    let path = configured
        .map(Path::to_path_buf)
        .or_else(|| running.iter().flatten().next().cloned())
        .or_else(|| {
            NEBULA_CONFIGS
                .iter()
                .map(PathBuf::from)
                .find(|path| path.exists())
        });
    let config = path
        .as_deref()
        .and_then(read_config_text)
        .map(|text| NebulaConfig::parse(&text))
        .unwrap_or_default();
    let dev = config.dev.as_deref().unwrap_or("nebula1");
    let interface = interface_networks(dev);
    let cert = config.cert.as_deref().and_then(print_cert);
    let installed = on_path("nebula");
    if !installed && path.is_none() && interface.is_empty() && running.is_empty() {
        return Network::new(Vpn::Nebula, State::Missing);
    }
    // The certificate names this machine; the interface has the addresses
    // when the certificate cannot be read.
    let (own_name, own_networks) = match cert {
        Some((name, networks)) if !networks.is_empty() => (name, networks),
        Some((name, _)) => (name, interface.clone()),
        None => (String::new(), interface.clone()),
    };
    if interface.is_empty() && running.is_empty() {
        return Network::new(Vpn::Nebula, State::Down("not running".to_owned()));
    }
    let mut network = Network::new(Vpn::Nebula, State::Up);
    let own_ips: Vec<IpAddr> = own_networks.iter().map(|(ip, _)| *ip).collect();
    network.this = (!own_ips.is_empty()).then(|| Peer {
        name: if own_name.is_empty() {
            own_ips[0].to_string()
        } else {
            own_name
        },
        ips: own_ips.clone(),
        online: Some(true),
        ..Peer::default()
    });
    let hosts = std::fs::read_to_string(hosts_file()).unwrap_or_default();
    network.peers = nebula_peers(&config, &own_networks, &parse_hosts(&hosts));
    network
}

/// The configured hosts, lighthouses and `/etc/hosts` entries inside
/// `networks`, without this machine, named from `/etc/hosts` where it
/// names them.
fn nebula_peers(
    config: &NebulaConfig,
    networks: &[(IpAddr, u8)],
    hosts: &[(IpAddr, String)],
) -> Vec<Peer> {
    let own = |ip: &IpAddr| networks.iter().any(|(own, _)| own == ip);
    let name_of = |ip: IpAddr| {
        hosts
            .iter()
            .find(|(host, _)| *host == ip)
            .map(|(_, name)| name.clone())
    };
    let mut peers: Vec<Peer> = Vec::new();
    let mut add = |ip: IpAddr, name: Option<String>| {
        if own(&ip) || peers.iter().any(|peer| peer.ips.contains(&ip)) {
            return;
        }
        peers.push(Peer {
            name: name.unwrap_or_else(|| ip.to_string()),
            ips: vec![ip],
            ..Peer::default()
        });
    };
    for &ip in config.lighthouses.iter().chain(&config.static_hosts) {
        add(ip, name_of(ip));
    }
    for (ip, name) in hosts {
        if networks
            .iter()
            .any(|&(net, prefix)| in_network(*ip, net, prefix))
        {
            add(*ip, Some(name.clone()));
        }
    }
    peers
}

/// Whether `ip` is in the network of `net` with `prefix` bits.
fn in_network(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    let bits = |ip: IpAddr| -> (u128, u32) {
        match ip {
            IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
            IpAddr::V6(v6) => (u128::from(v6), 128),
        }
    };
    let ((a, width), (b, other)) = (bits(ip), bits(net));
    if width != other || u32::from(prefix) > width {
        return false;
    }
    let shift = width - u32::from(prefix);
    shift >= 128 || a >> shift == b >> shift
}

/// The configuration at `path`: a file, or every `.yml` and `.yaml` file
/// of a directory, as Nebula reads them.
fn read_config_text(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return std::fs::read_to_string(path).ok();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    files.sort();
    let mut text = String::new();
    for file in files {
        text.push_str(&std::fs::read_to_string(file).unwrap_or_default());
        text.push('\n');
    }
    Some(text)
}

/// The `-config` paths of running `nebula` processes; `None` for one whose
/// path could not be read. Empty when none runs or the platform has no
/// `/proc`.
fn running_nebula_configs() -> Vec<Option<PathBuf>> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        if comm.trim() != "nebula" {
            continue;
        }
        let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        let config = config_arg(&args).map(|config| {
            // Relative to the process's directory, which only root may
            // read for other users' processes.
            let config = PathBuf::from(config);
            match std::fs::read_link(dir.join("cwd")) {
                Ok(cwd) if config.is_relative() => cwd.join(config),
                _ => config,
            }
        });
        found.push(config);
    }
    found
}

/// The value of `-config` or `--config` in a command line.
fn config_arg(args: &[&[u8]]) -> Option<String> {
    let text = |arg: &[u8]| String::from_utf8_lossy(arg).into_owned();
    for (i, arg) in args.iter().enumerate() {
        for flag in [&b"-config"[..], b"--config"] {
            if *arg == flag {
                return args.get(i + 1).map(|value| text(value));
            }
            if let Some(value) = arg.strip_prefix(flag).and_then(|v| v.strip_prefix(b"=")) {
                return Some(text(value));
            }
        }
    }
    None
}

/// Addresses and prefix lengths of interface `dev`, from `ip -j addr`.
/// Empty when it does not exist or `ip` is missing (on macOS Nebula uses a
/// `utun` device whose name it picks itself).
fn interface_networks(dev: &str) -> Vec<(IpAddr, u8)> {
    let output = Environment::Local
        .command("ip")
        .args(["-j", "addr", "show", "dev", dev])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let json: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    parse_ip_addr(&json)
}

fn parse_ip_addr(json: &Value) -> Vec<(IpAddr, u8)> {
    json.as_array()
        .into_iter()
        .flatten()
        .flat_map(|link| link["addr_info"].as_array().into_iter().flatten())
        .filter_map(|addr| {
            let ip: IpAddr = addr["local"].as_str()?.parse().ok()?;
            let prefix = u8::try_from(addr["prefixlen"].as_u64()?).ok()?;
            // Link-local IPv6 addresses are not Nebula's.
            let link_local = matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80);
            (!link_local).then_some((ip, prefix))
        })
        .collect()
}

/// This machine's name and networks from its certificate, through
/// `nebula-cert print -json`. `None` when the certificate is inline in the
/// configuration, unreadable, or `nebula-cert` is missing.
fn print_cert(cert: &str) -> Option<(String, Vec<(IpAddr, u8)>)> {
    if cert.contains("-----BEGIN") {
        return None;
    }
    let output = Environment::Local
        .command("nebula-cert")
        .args(["print", "-json", "-path", cert])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let json: Value = serde_json::from_slice(&output.stdout).ok()?;
    parse_cert(&json)
}

/// Version 1 certificates print one object with `details.ips`; version 2
/// prints `networks`, and a bundle of several prints an array.
fn parse_cert(json: &Value) -> Option<(String, Vec<(IpAddr, u8)>)> {
    let cert = match json {
        Value::Array(certs) => certs.first()?,
        other => other,
    };
    let details = cert.get("details").unwrap_or(cert);
    let name = details["name"].as_str().unwrap_or_default().to_owned();
    let networks = details
        .get("ips")
        .or_else(|| details.get("networks"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|net| {
            let (ip, prefix) = net.as_str()?.split_once('/')?;
            Some((ip.parse().ok()?, prefix.parse().ok()?))
        })
        .collect();
    Some((name, networks))
}

fn hosts_file() -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32\\drivers\\etc\\hosts")
    } else {
        PathBuf::from("/etc/hosts")
    }
}

/// Address and first name of each `/etc/hosts` line.
fn parse_hosts(text: &str) -> Vec<(IpAddr, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next()?;
            let mut words = line.split_whitespace();
            let ip = words.next()?.parse().ok()?;
            Some((ip, words.next()?.to_owned()))
        })
        .collect()
}

/// A line of YAML block style, without its comment.
struct YamlLine<'a> {
    indent: usize,
    text: &'a str,
}

fn yaml_lines(text: &str) -> Vec<YamlLine<'_>> {
    text.lines()
        .filter_map(|line| {
            let line = strip_comment(line).trim_end();
            let text = line.trim_start();
            if text.is_empty() || text == "---" {
                return None;
            }
            Some(YamlLine {
                indent: line.len() - text.len(),
                text,
            })
        })
        .collect()
}

/// `line` up to a `#` that starts a comment: at the start or after a space,
/// outside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous = ' ';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, '#') if previous.is_whitespace() => return &line[..i],
            _ => {}
        }
        previous = c;
    }
    line
}

/// The key and value of `key: value`, with the key unquoted. Keys may be
/// quoted IPv6 addresses, which hold colons.
fn split_key(text: &str) -> Option<(&str, &str)> {
    if let Some(open) = text.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let close = text[1..].find(open)? + 1;
        let rest = text[close + 1..].trim_start().strip_prefix(':')?;
        return Some((&text[1..close], rest.trim()));
    }
    let (key, value) = match text.find(": ") {
        Some(at) => (&text[..at], &text[at + 2..]),
        None => (text.strip_suffix(':')?, ""),
    };
    Some((key.trim(), value.trim()))
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// The value of `key` among `lines` at the indentation of the first line,
/// and the lines nested under it.
fn yaml_child<'a, 'b>(
    lines: &'b [YamlLine<'a>],
    key: &str,
) -> Option<(&'a str, &'b [YamlLine<'a>])> {
    let level = lines.first()?.indent;
    for (i, line) in lines.iter().enumerate() {
        if line.indent != level {
            continue;
        }
        let Some((found, value)) = split_key(line.text) else {
            continue;
        };
        if found != key {
            continue;
        }
        let end = lines[i + 1..]
            .iter()
            .position(|next| next.indent <= level)
            .map_or(lines.len(), |n| i + 1 + n);
        return Some((value, &lines[i + 1..end]));
    }
    None
}

/// A list written as `[a, b]` or as `- a` lines.
fn yaml_list<'a>(value: &'a str, children: &[YamlLine<'a>]) -> Vec<&'a str> {
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        return inner
            .split(',')
            .map(unquote)
            .filter(|item| !item.is_empty())
            .collect();
    }
    children
        .iter()
        .filter_map(|line| line.text.strip_prefix('-'))
        .map(unquote)
        .collect()
}

impl NebulaConfig {
    /// Reads the keys scriptschnell-term needs from Nebula's YAML. Block
    /// style only, which is what Nebula's example configuration uses.
    fn parse(text: &str) -> Self {
        let lines = yaml_lines(text);
        let lines = &lines[..];
        let scalar = |section: &str, key: &str| {
            let (_, children) = yaml_child(lines, section)?;
            let (value, _) = yaml_child(children, key)?;
            let value = unquote(value);
            (!value.is_empty() && value != "|").then(|| value.to_owned())
        };
        let ips = |items: Vec<&str>| -> Vec<IpAddr> {
            items.iter().filter_map(|ip| ip.parse().ok()).collect()
        };
        let lighthouses = yaml_child(lines, "lighthouse")
            .and_then(|(_, children)| yaml_child(children, "hosts"))
            .map(|(value, children)| ips(yaml_list(value, children)))
            .unwrap_or_default();
        let static_hosts = yaml_child(lines, "static_host_map")
            .map(|(_, children)| {
                let level = children.first().map_or(0, |line| line.indent);
                ips(children
                    .iter()
                    .filter(|line| line.indent == level)
                    .filter_map(|line| split_key(line.text).map(|(key, _)| key))
                    .collect())
            })
            .unwrap_or_default();
        Self {
            cert: scalar("pki", "cert"),
            dev: scalar("tun", "dev"),
            lighthouses,
            static_hosts,
        }
    }
}

// --- locations -------------------------------------------------------------------

/// A project location on a VPN peer: `tailscale://[user@]peer/path`,
/// `netbird://…` or `nebula://…`. It opens over SSH; `?remote` opens it on
/// the peer as a cmdterm remote machine, and `?vm=name` in one of its VMs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VpnLocation {
    pub vpn: Vpn,
    pub user: Option<String>,
    pub peer: String,
    /// An explicit port, for SSH or the remote machine.
    pub port: Option<u16>,
    pub path: PathBuf,
    pub remote: bool,
    pub vm: Option<String>,
}

impl VpnLocation {
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let (vpn, rest) = Vpn::ALL.iter().find_map(|&vpn| {
            let rest = input.strip_prefix(vpn.scheme())?.strip_prefix("://")?;
            Some((vpn, rest))
        })?;
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, query),
            None => (rest, ""),
        };
        let mut remote = false;
        let mut vm = None;
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            match pair.split_once('=') {
                Some(("vm", name)) if !name.is_empty() => {
                    remote = true;
                    vm = Some(name.to_owned());
                }
                None if pair == "remote" => remote = true,
                _ => return None,
            }
        }
        let (authority, path) = rest.split_once('/')?;
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_owned()), host),
            None => (None, authority),
        };
        let (peer, port) = match host.rsplit_once(':') {
            Some((peer, port)) if !peer.contains(':') => (peer, Some(port.parse().ok()?)),
            _ => (host, None),
        };
        if peer.is_empty() || (remote && user.is_some()) {
            return None;
        }
        // A user or peer such as `-oProxyCommand=...` would be an ssh
        // option, and one with spaces is no name.
        let destination = match &user {
            Some(user) => format!("{user}@{peer}"),
            None => peer.to_owned(),
        };
        if !cmdterm_env::valid_ssh_destination(&destination) {
            return None;
        }
        // `tailscale://box/~/src` is relative to the peer's home.
        let path = if path.starts_with('~') {
            PathBuf::from(path)
        } else {
            PathBuf::from(format!("/{path}"))
        };
        Some(Self {
            vpn,
            user,
            peer: peer.to_owned(),
            port,
            path,
            remote,
            vm,
        })
    }

    /// Scans the VPN and finds the peer. Blocking.
    pub fn resolve(&self, config: &ScanConfig) -> Result<Environment, String> {
        let network = scan(self.vpn, config);
        match &network.state {
            State::Up => {}
            State::Missing => return Err(format!("{} is not installed", self.vpn.name())),
            State::Down(why) => return Err(format!("{} is down: {why}", self.vpn.name())),
        }
        let address = network
            .find(&self.peer)
            .and_then(Peer::address)
            .ok_or_else(|| format!("no {} peer named {}", self.vpn.name(), self.peer))?;
        Ok(self.environment(&address))
    }

    /// The environment at the peer's `address`.
    pub fn environment(&self, address: &str) -> Environment {
        if self.remote {
            let host = if address.contains(':') {
                format!("[{address}]")
            } else {
                address.to_owned()
            };
            let host = match self.port {
                Some(port) => format!("{host}:{port}"),
                None => host,
            };
            let mut target = RemoteTarget::new(&host);
            target.vm = self.vm.clone();
            return Environment::Remote(target);
        }
        let destination = match &self.user {
            Some(user) => format!("{user}@{address}"),
            None => address.to_owned(),
        };
        let mut target = SshTarget::new(destination);
        target.port = self.port;
        Environment::Ssh(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tailscale_status_lists_peers_with_magic_dns_names() {
        let status = json!({
            "BackendState": "Running",
            "CurrentTailnet": {"MagicDNSEnabled": true},
            "Self": {"HostName": "laptop", "DNSName": "laptop.tail1.ts.net.",
                     "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"], "Online": true, "OS": "linux"},
            "Peer": {
                "nodekey:a": {"HostName": "box", "DNSName": "box.tail1.ts.net.",
                              "TailscaleIPs": ["100.64.0.2"], "Online": false, "OS": "linux"},
                "nodekey:b": {"HostName": "ci", "DNSName": "ci.tail1.ts.net.",
                              "TailscaleIPs": ["fd7a:115c:a1e0::3", "100.64.0.3"], "Online": true, "OS": ""}
            }
        });
        let mut network = parse_tailscale(&status);
        network.sort();
        assert_eq!(network.state, State::Up);
        assert_eq!(network.this.as_ref().unwrap().name, "laptop");
        let names: Vec<&str> = network.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["ci", "box"]);
        assert_eq!(network.peers[0].address().unwrap(), "ci.tail1.ts.net");
        assert_eq!(network.peers[0].os, None);
        assert_eq!(network.find("BOX").unwrap().name, "box");
        assert_eq!(network.find("box.tail1.ts.net").unwrap().name, "box");
        assert_eq!(network.find("100.64.0.3").unwrap().name, "ci");
        assert!(network.find("nope").is_none());

        let mut plain = status.clone();
        plain["CurrentTailnet"]["MagicDNSEnabled"] = json!(false);
        let network = parse_tailscale(&plain);
        let ci = network.find("ci").unwrap();
        assert_eq!(ci.address().unwrap(), "100.64.0.3");

        let stopped = parse_tailscale(&json!({"BackendState": "Stopped"}));
        assert_eq!(stopped.state, State::Down("Stopped".into()));
    }

    #[test]
    fn dns_names_win_over_the_names_machines_give_themselves() {
        let peer = |name: &str, dns: &str, ip: &str| Peer {
            name: name.into(),
            dns_name: Some(dns.into()),
            ips: vec![ip.parse().unwrap()],
            ..Peer::default()
        };
        // A machine renamed in the admin console keeps its old HostName,
        // which another machine's DNS label now has.
        let network = Network {
            peers: vec![
                peer("box", "old-box.tail1.ts.net", "100.64.0.2"),
                peer("laptop", "box.tail1.ts.net", "100.64.0.3"),
                peer("ci", "ci.tail1.ts.net", "100.64.0.4"),
            ],
            ..Network::new(Vpn::Tailscale, State::Up)
        };
        assert_eq!(network.find("box").unwrap().name, "laptop");
        assert_eq!(network.find("box.tail1.ts.net.").unwrap().name, "laptop");
        assert_eq!(network.find("old-box").unwrap().name, "box");
        assert_eq!(network.find("ci").unwrap().name, "ci");
        assert_eq!(network.find("100.64.0.2").unwrap().name, "box");
    }

    #[test]
    fn netbird_status_lists_peers_by_address() {
        let status = json!({
            "netbirdIp": "100.90.0.1/16",
            "fqdn": "laptop.netbird.cloud",
            "management": {"connected": true},
            "peers": {"total": 2, "connected": 1, "details": [
                {"fqdn": "box.netbird.cloud", "netbirdIp": "100.90.0.2", "netbirdIpv6": "fd00::2", "status": "Connected"},
                {"fqdn": "old.netbird.cloud", "netbirdIp": "100.90.0.3", "status": "Disconnected"},
                {"fqdn": "", "netbirdIp": "", "status": "Idle"}
            ]}
        });
        let network = parse_netbird(&status);
        assert_eq!(network.state, State::Up);
        let this = network.this.as_ref().unwrap();
        assert_eq!(
            (this.name.as_str(), this.ips[0].to_string()),
            ("laptop", "100.90.0.1".into())
        );
        assert_eq!(network.peers.len(), 2);
        let box_ = network.find("box").unwrap();
        assert_eq!(box_.address().unwrap(), "100.90.0.2");
        assert_eq!(box_.online, Some(true));
        assert_eq!(network.find("old").unwrap().online, Some(false));

        let mut down = status;
        down["management"]["connected"] = json!(false);
        assert!(matches!(parse_netbird(&down).state, State::Down(_)));
    }

    const NEBULA: &str = r#"
pki:
  ca: /etc/nebula/ca.crt
  cert: "/etc/nebula/host.crt" # this machine
  key: /etc/nebula/host.key

static_host_map:
  "192.168.100.1": ["198.51.100.1:4242"]
  "fd00::1": ["198.51.100.2:4242"]

lighthouse:
  am_lighthouse: false
  hosts:
    - "192.168.100.1"

tun:
  disabled: false
  dev: nebula9
"#;

    #[test]
    fn nebula_config_names_cert_device_and_lighthouses() {
        let config = NebulaConfig::parse(NEBULA);
        assert_eq!(config.cert.as_deref(), Some("/etc/nebula/host.crt"));
        assert_eq!(config.dev.as_deref(), Some("nebula9"));
        assert_eq!(
            config.lighthouses,
            ["192.168.100.1".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            config.static_hosts,
            [
                "192.168.100.1".parse::<IpAddr>().unwrap(),
                "fd00::1".parse().unwrap()
            ]
        );
        let flow = NebulaConfig::parse("lighthouse:\n  hosts: ['10.0.0.1', \"10.0.0.2\"]\n");
        assert_eq!(flow.lighthouses.len(), 2);
        assert_eq!(
            NebulaConfig::parse("pki:\n  cert: |\n    -----BEGIN\n").cert,
            None
        );
    }

    #[test]
    fn nebula_peers_come_from_the_config_and_hosts_in_the_network() {
        let config = NebulaConfig::parse(NEBULA);
        let hosts = parse_hosts(
            "127.0.0.1 localhost\n192.168.100.1 lighthouse # nebula\n\
             192.168.100.7 box box.lan\n192.168.100.5 me\n10.1.1.1 elsewhere\n",
        );
        let own = [("192.168.100.5".parse().unwrap(), 24)];
        let peers = nebula_peers(&config, &own, &hosts);
        let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["lighthouse", "fd00::1", "box"]);
        assert_eq!(peers[2].address().unwrap(), "192.168.100.7");
    }

    #[test]
    fn interface_and_certificate_output_parse() {
        let ip = json!([{"ifname": "nebula1", "addr_info": [
            {"family": "inet", "local": "192.168.100.5", "prefixlen": 24},
            {"family": "inet6", "local": "fe80::1", "prefixlen": 64}
        ]}]);
        assert_eq!(parse_ip_addr(&ip), [("192.168.100.5".parse().unwrap(), 24)]);
        let v1 = json!({"details": {"name": "laptop", "ips": ["192.168.100.5/24"]}});
        let v2 = json!([{"details": {"name": "laptop", "networks": ["192.168.100.5/24"]}}]);
        for cert in [v1, v2] {
            let (name, networks) = parse_cert(&cert).unwrap();
            assert_eq!(name, "laptop");
            assert_eq!(networks, [("192.168.100.5".parse().unwrap(), 24)]);
        }
        assert!(in_network(
            "10.0.3.4".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
            16
        ));
        assert!(!in_network(
            "10.1.0.4".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
            16
        ));
        assert!(in_network(
            "fd00::9".parse().unwrap(),
            "fd00::1".parse().unwrap(),
            64
        ));
    }

    #[test]
    fn config_flag_of_a_nebula_command_line() {
        let args: [&[u8]; 3] = [b"/usr/bin/nebula", b"-config", b"/etc/nebula/config.yml"];
        assert_eq!(config_arg(&args).as_deref(), Some("/etc/nebula/config.yml"));
        let args: [&[u8]; 2] = [b"nebula", b"--config=cfg"];
        assert_eq!(config_arg(&args).as_deref(), Some("cfg"));
        let args: [&[u8]; 1] = [b"nebula"];
        assert_eq!(config_arg(&args), None);
    }

    #[test]
    fn vpn_locations_parse_and_pick_ssh_or_remote() {
        let location = VpnLocation::parse("tailscale://me@box/~/src").unwrap();
        assert_eq!(location.vpn, Vpn::Tailscale);
        assert_eq!(location.peer, "box");
        assert_eq!(location.path, PathBuf::from("~/src"));
        let Environment::Ssh(target) = location.environment("100.64.0.2") else {
            panic!();
        };
        assert_eq!(target.destination, "me@100.64.0.2");

        let location = VpnLocation::parse("nebula://box:2222/srv/app").unwrap();
        assert_eq!(location.path, PathBuf::from("/srv/app"));
        let Environment::Ssh(target) = location.environment("192.168.100.7") else {
            panic!();
        };
        assert_eq!(target.port, Some(2222));

        let location = VpnLocation::parse("netbird://box/~/?vm=ci").unwrap();
        let Environment::Remote(target) = location.environment("100.90.0.2") else {
            panic!();
        };
        assert_eq!(target.address, "100.90.0.2:7722");
        assert_eq!(target.vm.as_deref(), Some("ci"));

        let location = VpnLocation::parse("tailscale://box/~/?remote").unwrap();
        let Environment::Remote(target) = location.environment("fd7a::2") else {
            panic!();
        };
        assert_eq!(target.address, "[fd7a::2]:7722");

        assert_eq!(VpnLocation::parse("tailscale:///srv"), None);
        assert_eq!(VpnLocation::parse("tailscale://box"), None);
        assert_eq!(VpnLocation::parse("tailscale://box/~/?bogus"), None);
        assert_eq!(VpnLocation::parse("ssh://box/srv"), None);
        // Users and peers that ssh would read as options are refused.
        assert_eq!(VpnLocation::parse("tailscale://-oProxyCommand=x/p"), None);
        assert_eq!(
            VpnLocation::parse("tailscale://-oProxyCommand=x@box/p"),
            None
        );
        assert_eq!(VpnLocation::parse("tailscale://me x@box/p"), None);
        assert_eq!(VpnLocation::parse("tailscale://@box/p"), None);
    }
}
