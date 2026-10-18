//! `cmdterm-remote`: server, client and VM guest agent for cmdterm remote
//! machines. See the crate README for setup.

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmdterm_remote::client::{self, ExecOptions, ExecTarget, TrustOutcome};
use cmdterm_remote::keys::{self, HostTrust};
use cmdterm_remote::proto::OpenSpec;
use cmdterm_remote::server::Server;
use cmdterm_remote::{DEFAULT_PORT, Paths};

#[derive(Parser)]
#[command(name = "cmdterm-remote", version, about)]
struct Cli {
    /// One directory for keys, `server.json`, images and VM disks, all in its
    /// `remote/`. Defaults to $SCRIPTSCHNELL_TERM_CONFIG_DIR, else keys and
    /// `server.json` go to the configuration directory (`~/.config/scriptschnell-term`
    /// on Linux) and images and disks to the data directory (`~/.local/share/scriptschnell-term`).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this machine's public key, creating the key pair if needed.
    Keygen,
    /// Serve this machine to authorized clients.
    Serve {
        /// Address to listen on.
        #[arg(long, default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
        listen: String,
    },
    /// Let a client key log in. Reads the key line from stdin when omitted.
    Authorize {
        /// The client's `cmdterm-hybrid-v1 ...` line from `keygen`.
        key: Option<String>,
        /// Allow only Firecracker VMs, never the host itself.
        #[arg(long, conflicts_with = "host")]
        vm_only: bool,
        /// Allow the host itself even when server.json's offer_host is false.
        #[arg(long)]
        host: bool,
        /// Share this machine's projects with the key even when server.json's
        /// share_projects is false.
        #[arg(long, conflicts_with = "no_projects")]
        projects: bool,
        /// Never share this machine's projects with the key.
        #[arg(long)]
        no_projects: bool,
    },
    /// Fetch a server's key and add it to known_hosts after you confirm it.
    Trust {
        /// host or host:port.
        address: String,
        /// Accept the key without asking if it has this fingerprint.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Run a command or shell on a server or a local VM. cmdterm runs this
    /// for remote and VM projects.
    Exec {
        /// host or host:port.
        #[arg(
            long,
            required_unless_present = "local_vm",
            conflicts_with = "local_vm"
        )]
        target: Option<String>,
        /// Run in this VM of the local VM daemon.
        #[arg(long)]
        local_vm: Option<String>,
        /// Run in this Firecracker VM on the server.
        #[arg(long)]
        vm: Option<String>,
        /// Allocate a PTY sized like this terminal.
        #[arg(long)]
        pty: bool,
        /// Run the login shell instead of a command.
        #[arg(long)]
        shell: bool,
        #[arg(long)]
        cwd: Option<OsString>,
        /// KEY=VALUE, repeatable.
        #[arg(long = "env")]
        vars: Vec<OsString>,
        /// INSIDE=LOCAL, repeatable: connections to the Unix socket INSIDE on
        /// the remote machine reach the Unix socket LOCAL here while the
        /// command runs. Servers without relays run the command without it.
        #[arg(long = "forward")]
        forwards: Vec<String>,
        /// Connect directly instead of sharing a master connection.
        #[arg(long)]
        direct: bool,
        /// Run in the session NAME, which keeps running when the connection
        /// drops or this client exits. The same name attaches to it again.
        /// Needs --pty. On a lost connection the client reconnects.
        #[arg(long, value_name = "NAME")]
        session: Option<String>,
        /// End the session named by --session instead of running anything.
        #[arg(long, requires = "session")]
        end: bool,
        /// Start the command or shell under this CPU limit, as cmdterm
        /// passes it: UNIT:RESERVE:PERCENT:WEIGHT. Needs --pty. Servers
        /// without CPU limits start it unlimited.
        #[arg(long, value_name = "LIMIT")]
        cpu: Option<String>,
        /// The program and its arguments. Like --cwd and --env they must
        /// be UTF-8, as the protocol carries text.
        #[arg(last = true)]
        command: Vec<OsString>,
    },
    /// Link this machine to a cmdterm-web server, so its web app reaches the
    /// machine. Every `serve`, and the desktop app while it serves this
    /// machine, then keeps a connection to it.
    Link {
        /// The web server, `https://web.example`.
        url: String,
        /// The pairing code the web app shows under Link a machine.
        code: String,
        /// How the web app names this machine. Defaults to the host name.
        #[arg(long)]
        name: Option<String>,
        /// Link only if the web server's key has this fingerprint.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Remove the link to a cmdterm-web server.
    Unlink { url: String },
    /// List the cmdterm-web servers this machine is linked to.
    Links,
    /// Hold a shared connection to a server (started by `exec`).
    #[command(hide = true)]
    Master {
        #[arg(long)]
        target: String,
    },
    /// Build a ready-made VM image (alpine or ubuntu) without root and add it
    /// to server.json. A VM named after the image, or the image name and a
    /// dash, starts from it.
    Image {
        /// alpine or ubuntu.
        name: String,
        /// podman, docker, or a path. Defaults to podman when installed.
        #[arg(long)]
        engine: Option<String>,
    },
    /// Run this machine's Firecracker VMs for its own user (started by
    /// `exec --local-vm`). Uses the `firecracker` section of server.json.
    Vmd,
    /// Run channels for the host, inside a Firecracker VM. Start this from
    /// the guest's init.
    Guest {
        /// vsock port to listen on.
        #[arg(long, default_value_t = 7723)]
        vsock_port: u32,
        /// Listen on this Unix socket instead, speaking Firecracker's host-side
        /// vsock handshake. For testing without a VM.
        #[arg(long)]
        unix: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    // The proxy bridge, which a server's sandboxes run to scope themselves
    // (see `cmdterm_env::SCOPE_EXE_VAR`). Before parsing, since it takes
    // the sandboxed command's arguments.
    #[cfg(unix)]
    {
        let mut args = std::env::args_os().skip(1);
        if args
            .next()
            .is_some_and(|first| first == cmdterm_env::BUILT_IN_BRIDGE_FLAG)
        {
            std::process::exit(cmdterm_env::proxy_bridge_main(args));
        }
        cmdterm_env::set_built_in_proxy_bridge(cmdterm_env::BUILT_IN_BRIDGE_FLAG);
    }
    let cli = Cli::parse();
    let dirs = cmdterm_remote::AppDirs::resolve(cli.config_dir.as_deref());
    dirs.migrate();
    let paths = Paths::of(&dirs);
    match run(&paths, cli.command) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("cmdterm-remote: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(paths: &Paths, command: Command) -> io::Result<u8> {
    match command {
        Command::Keygen => {
            let identity = paths.load_identity()?;
            if let Err(error) = paths.keep_identity() {
                eprintln!("cmdterm-remote: the key stays out of the keyring: {error}");
            }
            let mut line_path = paths.identity().into_os_string();
            line_path.push(".pub");
            let line = std::fs::read_to_string(&line_path)
                .unwrap_or_else(|_| identity.public().to_line(""));
            println!("{}", line.trim());
            eprintln!("fingerprint {}", identity.public().fingerprint());
            Ok(0)
        }
        Command::Serve { listen } => {
            let server = Server::new(paths.clone())?;
            let address = server.spawn(&listen)?;
            eprintln!(
                "cmdterm-remote: serving on {address}, host key {}",
                server.public_key().fingerprint()
            );
            eprintln!(
                "cmdterm-remote: authorized keys in {}",
                paths.authorized_keys().display()
            );
            wait_for_termination()?;
            server.shutdown();
            Ok(0)
        }
        Command::Link {
            url,
            code,
            name,
            fingerprint,
        } => {
            let name = name.unwrap_or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .map(|h| h.trim().to_owned())
                    .unwrap_or_else(|_| "machine".into())
            });
            let link =
                cmdterm_remote::link::redeem(paths, &url, &code, &name, fingerprint.as_deref())?;
            eprintln!(
                "cmdterm-remote: linked to {} as {name}; machines connect to {}",
                link.url, link.address
            );
            eprintln!(
                "cmdterm-remote: the web server's key is {}; compare it with the machine key \
                 the web app shows",
                link.key()?.fingerprint()
            );
            eprintln!(
                "cmdterm-remote: running servers pick the link up within seconds; \
                 start one with `cmdterm-remote serve` or by serving this machine in the app"
            );
            Ok(0)
        }
        Command::Unlink { url } => {
            if cmdterm_remote::link::remove(paths, &url)? {
                eprintln!("cmdterm-remote: unlinked {url}");
                Ok(0)
            } else {
                eprintln!("cmdterm-remote: not linked to {url}");
                Ok(1)
            }
        }
        Command::Links => {
            for link in cmdterm_remote::link::load(paths)? {
                println!("{}\t{}\t{}", link.url, link.address, link.machine);
            }
            Ok(0)
        }
        Command::Image { name, engine } => {
            let Some(preset) = cmdterm_remote::image::preset(&name) else {
                let names: Vec<&str> = cmdterm_remote::image::PRESETS
                    .iter()
                    .map(|p| p.name)
                    .collect();
                return Err(io::Error::other(format!(
                    "no image {name:?}; pick one of {}",
                    names.join(", ")
                )));
            };
            let missing = cmdterm_remote::image::missing_tools(engine.as_deref());
            if !missing.is_empty() {
                return Err(io::Error::other(format!(
                    "install {} first",
                    missing.join(", ")
                )));
            }
            let log = cmdterm_remote::image::images_dir(paths).join(format!("{name}.log"));
            let options = cmdterm_remote::image::BuildOptions {
                engine,
                guest: std::env::current_exe()?,
                log: log.clone(),
            };
            let built = cmdterm_remote::image::build(paths, preset, &options, |step| {
                eprintln!("{step}");
            })
            .map_err(|error| io::Error::other(format!("{error} (log: {})", log.display())))?;
            cmdterm_remote::image::configure(paths, preset)?;
            eprintln!(
                "built {} and added it to {}",
                built.display(),
                paths.server_config().display()
            );
            if !cmdterm_remote::image::user_network_available() {
                eprintln!("VMs have no network: install pasta (passt) and iproute2 for it");
            }
            Ok(0)
        }
        Command::Authorize {
            key,
            vm_only,
            host,
            projects,
            no_projects,
        } => {
            let line = match key {
                Some(key) => key,
                None => {
                    let mut line = String::new();
                    io::stdin().lock().read_line(&mut line)?;
                    line
                }
            };
            let public = keys::PublicKey::parse(line.trim())
                .ok_or_else(|| io::Error::other("not a cmdterm-hybrid-v1 public key line"))?;
            let options = keys::KeyOptions {
                host: (vm_only || host).then_some(host),
                projects: (projects || no_projects).then_some(projects),
            };
            let line = options.prefix(line.trim());
            keys::authorize(&paths.authorized_keys(), &line)?;
            eprintln!("authorized {}", public.fingerprint());
            Ok(0)
        }
        Command::Trust {
            address,
            fingerprint,
        } => {
            let address = cmdterm_env::RemoteTarget::new(&address).address;
            let outcome = client::trust(paths, &address, |key, state| {
                if state == HostTrust::Changed {
                    return false;
                }
                let seen = key.fingerprint();
                if let Some(expected) = &fingerprint {
                    return *expected == seen;
                }
                eprintln!("{address} has key {seen}");
                if !io::stdin().is_terminal() {
                    eprintln!("not a terminal; pass --fingerprint to confirm it");
                    return false;
                }
                eprint!("Trust it? Compare it with `cmdterm-remote keygen` on the server. [y/N] ");
                let _ = io::stderr().flush();
                let mut answer = String::new();
                let _ = io::stdin().lock().read_line(&mut answer);
                matches!(answer.trim(), "y" | "Y" | "yes")
            })?;
            match outcome {
                TrustOutcome::AlreadyTrusted => eprintln!("{address} is already trusted"),
                TrustOutcome::Added => eprintln!("trusted {address}"),
                TrustOutcome::Declined => {
                    eprintln!("not trusted; known_hosts unchanged");
                    return Ok(1);
                }
            }
            Ok(0)
        }
        Command::Exec {
            target,
            local_vm,
            vm,
            pty,
            shell,
            cwd,
            vars,
            forwards,
            direct,
            session,
            end,
            cpu,
            command,
        } => {
            let cwd = cwd.map(|cwd| utf8("--cwd", cwd)).transpose()?;
            let vars = (vars.into_iter())
                .map(|pair| utf8("--env", pair))
                .collect::<io::Result<Vec<_>>>()?;
            let command = (command.into_iter())
                .map(|arg| utf8("the command", arg))
                .collect::<io::Result<Vec<_>>>()?;
            let mut command = command.into_iter();
            if !shell && !end && command.len() == 0 {
                return Err(io::Error::other(
                    "nothing to run: pass --shell or -- PROGRAM",
                ));
            }
            let vars = vars
                .iter()
                .map(|pair| {
                    pair.split_once('=')
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .ok_or_else(|| {
                            io::Error::other(format!("--env {pair}: expected KEY=VALUE"))
                        })
                })
                .collect::<io::Result<_>>()?;
            let forwards = forwards
                .iter()
                .map(|pair| {
                    pair.split_once('=')
                        .map(|(inside, local)| (inside.to_owned(), PathBuf::from(local)))
                        .ok_or_else(|| {
                            io::Error::other(format!("--forward {pair}: expected INSIDE=LOCAL"))
                        })
                })
                .collect::<io::Result<_>>()?;
            let (target, vm) = match (target, local_vm) {
                (_, Some(name)) => (ExecTarget::LocalVm, name),
                (Some(target), None) => (
                    ExecTarget::Remote(cmdterm_env::RemoteTarget::new(&target).address),
                    vm.unwrap_or_default(),
                ),
                (None, None) => unreachable!("clap requires one"),
            };
            let spec = if end {
                OpenSpec {
                    vm,
                    program: cmdterm_remote::session::END_PROGRAM.into(),
                    session: session.unwrap_or_default(),
                    ..OpenSpec::default()
                }
            } else {
                OpenSpec {
                    vm,
                    pty: pty.then_some((80, 24)),
                    shell,
                    cwd: cwd.unwrap_or_default(),
                    program: command.next().unwrap_or_default(),
                    args: command.collect(),
                    vars,
                    // Only PTY channels keep sessions.
                    session: session.filter(|_| pty).unwrap_or_default(),
                    resume: None,
                    cpu: cpu.filter(|_| pty).unwrap_or_default(),
                    exit_if_ended: false,
                    run: None,
                }
            };
            let direct = direct || std::env::var_os("CMDTERM_REMOTE_DIRECT").is_some();
            let code = client::exec(
                paths,
                ExecOptions {
                    target,
                    spec,
                    forwards,
                    direct,
                },
            );
            Ok(exit_status(code))
        }
        Command::Master { target } => {
            client::run_master(paths, &target)?;
            Ok(0)
        }
        Command::Vmd => {
            cmdterm_remote::local::run_vmd(paths)?;
            Ok(0)
        }
        Command::Guest { vsock_port, unix } => {
            match unix {
                Some(path) => guest_unix(&path),
                None => guest_vsock(vsock_port),
            }?;
            Ok(0)
        }
    }
}

/// `value` as text, or an error naming `what` when it is not UTF-8: the
/// protocol carries strings, and replacing bytes would run something else.
fn utf8(what: &str, value: OsString) -> io::Result<String> {
    value.into_string().map_err(|value| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{what}: {} is not UTF-8, and remote machines take only UTF-8 arguments",
                value.to_string_lossy()
            ),
        )
    })
}

/// The exit status for a remote exit code. Codes outside 0 to 255, such as
/// the negative ones of crashed Windows processes, become 255, so that no
/// failure exits 0.
fn exit_status(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(u8::MAX)
}

/// Returns on SIGINT or SIGTERM. On Windows it never returns; Ctrl+C ends
/// the process.
fn wait_for_termination() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])?;
        signals.forever().next();
        Ok(())
    }
    #[cfg(windows)]
    loop {
        std::thread::park();
    }
}

#[cfg(not(target_os = "linux"))]
fn guest_vsock(_port: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the guest agent runs only in Linux VMs",
    ))
}

#[cfg(windows)]
fn guest_unix(path: &std::path::Path) -> io::Result<()> {
    let _ = path;
    guest_vsock(0)
}

/// Serves each vsock connection from the host on this machine. VM guests
/// run Linux; vsock sockets exist on no other Unix this builds for.
#[cfg(target_os = "linux")]
fn guest_vsock(port: u32) -> io::Result<()> {
    use cmdterm_remote::mux::{self, LocalBackend};
    use cmdterm_remote::transport::Plain;
    use nix::sys::socket::{
        AddressFamily, Backlog, SockFlag, SockType, VsockAddr, bind, listen, socket,
    };
    use std::os::fd::AsRawFd;
    const VMADDR_CID_ANY: u32 = u32::MAX;
    let listener = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    bind(listener.as_raw_fd(), &VsockAddr::new(VMADDR_CID_ANY, port))?;
    listen(&listener, Backlog::new(16)?)?;
    loop {
        let stream = std::fs::File::from(rustix::net::accept(&listener)?);
        std::thread::spawn(move || {
            if let Ok(read) = stream.try_clone() {
                let _ = mux::serve(Plain(read), Plain(stream), &LocalBackend);
            }
        });
    }
}

/// Like [`guest_vsock`], on a Unix socket that answers Firecracker's
/// `CONNECT <port>` line first.
#[cfg(unix)]
fn guest_unix(path: &std::path::Path) -> io::Result<()> {
    use cmdterm_remote::mux::{self, LocalBackend};
    use cmdterm_remote::transport::Plain;
    use std::io::Read;
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        std::thread::spawn(move || {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            if !line.starts_with(b"CONNECT ") || stream.write_all(b"OK 1073741824\n").is_err() {
                return;
            }
            if let Ok(read) = stream.try_clone() {
                let _ = mux::serve(Plain(read), Plain(stream), &LocalBackend);
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_exits_nonzero() {
        assert_eq!(exit_status(0), 0);
        assert_eq!(exit_status(3), 3);
        assert_eq!(exit_status(255), 255);
        assert_eq!(exit_status(256), 255);
        // STATUS_ACCESS_VIOLATION as a Windows server reports it.
        assert_eq!(exit_status(0xC000_0005_u32 as i32), 255);
        assert_eq!(exit_status(-1), 255);
    }

    #[cfg(unix)]
    #[test]
    fn arguments_that_are_not_utf8_are_refused() {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(utf8("--cwd", "/srv".into()).unwrap(), "/srv");
        let error = utf8("the command", OsString::from_vec(b"caf\xe9".to_vec())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().starts_with("the command: caf"), "{error}");
    }
}
