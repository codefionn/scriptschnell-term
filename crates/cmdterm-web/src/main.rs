//! `scriptschnell-term-web`: the web server, and the commands that manage its users.
//! See the crate README.

use std::io::{self, BufRead, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use cmdterm_remote::keys::Identity;
use cmdterm_web::config::Config;
use cmdterm_web::hub::Hub;
use cmdterm_web::store::{Store, User};
use cmdterm_web::{App, api, auth, now, random_id};

#[derive(Parser)]
#[command(name = "scriptschnell-term-web", version, about)]
struct Cli {
    /// Where `web.json`, `store.json` and the server key live. Defaults to
    /// `cmdterm-web` in the data directory (`~/.local/share` on Linux).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the web app and accept linked machines.
    Serve {
        /// Create a user `dev` and print a login link with a token that
        /// works until the server stops.
        #[arg(long)]
        dev: bool,
    },
    /// Manage the users who log in with a password.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Add a user. Asks for the password, or reads it from stdin.
    Add { name: String },
    /// Set a user's password.
    Passwd { name: String },
    /// Remove a user, their machines and their sessions.
    Remove { name: String },
    /// List the users.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dir = cli.data_dir.unwrap_or_else(default_dir);
    match run(&dir, cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("scriptschnell-term-web: {error}");
            ExitCode::FAILURE
        }
    }
}

fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cmdterm-web")
}

fn run(dir: &Path, command: Command) -> io::Result<()> {
    match command {
        Command::Serve { dev } => serve(dir, dev),
        Command::User { command } => user(dir, command),
    }
}

fn serve(dir: &Path, dev: bool) -> io::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = Config::load(dir)?;
    let store = Arc::new(Store::open(dir)?);
    let identity = Identity::load_or_generate(&dir.join("identity"))?;
    let runtime = tokio::runtime::Runtime::new()?;
    let hub = Hub::new(identity, store.clone(), runtime.handle().clone());
    let linked = hub.listen(&config.link_listen)?;
    let dev_token = dev.then(auth::random_token);
    let app = App::new(config.clone(), store, hub.clone(), dev_token.clone());
    let router = api::router(app).into_make_service_with_connect_info::<SocketAddr>();
    let address: SocketAddr = config
        .listen
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("listen: {e}")))?;
    eprintln!(
        "scriptschnell-term-web: serving {} on {address}",
        config.public_url()
    );
    if config.tls.is_none() && !address.ip().is_loopback() {
        eprintln!(
            "scriptschnell-term-web: plain HTTP on a non-loopback address: session cookies cross the \
             network unencrypted; set tls in web.json or bind localhost"
        );
    }
    let fingerprint = cmdterm_remote::keys::PublicKey::parse(&hub.key_line())
        .map(|k| k.fingerprint())
        .unwrap_or_default();
    eprintln!(
        "scriptschnell-term-web: machines connect on {linked} as {}, server key {fingerprint}",
        config.link_address()
    );
    if let Some(token) = &dev_token {
        // The token goes in the fragment, which the GET never sends, so it
        // stays out of server logs and history; the page POSTs it.
        eprintln!(
            "scriptschnell-term-web: dev login: {}/auth/token#token={token}",
            config.public_url()
        );
        if !address.ip().is_loopback() {
            eprintln!(
                "scriptschnell-term-web: the dev login works for anyone who reaches {address}"
            );
        }
    }
    runtime.block_on(async move {
        let handle = axum_server::Handle::new();
        let stop = handle.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        match &config.tls {
            Some(tls) => {
                let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                    .await?;
                axum_server::bind_rustls(address, tls)
                    .handle(handle)
                    .serve(router)
                    .await
            }
            None => {
                axum_server::bind(address)
                    .handle(handle)
                    .serve(router)
                    .await
            }
        }
    })
}

/// Reads a new password twice from the terminal, or once from stdin.
fn read_password() -> io::Result<String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_owned());
    }
    let first = prompt("password: ")?;
    let second = prompt("again: ")?;
    if first != second {
        return Err(io::Error::other("the passwords differ"));
    }
    Ok(first)
}

/// Asks with echo turned off. Windows consoles keep echoing.
fn prompt(label: &str) -> io::Result<String> {
    #[cfg(windows)]
    {
        eprint!("{label}");
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\n', '\r']).to_owned())
    }
    #[cfg(unix)]
    prompt_quietly(label)
}

#[cfg(unix)]
fn prompt_quietly(label: &str) -> io::Result<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    eprint!("{label}");
    io::stderr().flush()?;
    let stdin = io::stdin();
    let saved = tcgetattr(&stdin).ok();
    if let Some(saved) = &saved {
        let mut quiet = saved.clone();
        quiet.local_modes.remove(LocalModes::ECHO);
        let _ = tcsetattr(&stdin, OptionalActions::Now, &quiet);
    }
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    if let Some(saved) = &saved {
        let _ = tcsetattr(&stdin, OptionalActions::Now, saved);
    }
    eprintln!();
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

fn user(dir: &Path, command: UserCommand) -> io::Result<()> {
    let store = Store::open(dir)?;
    match command {
        UserCommand::Add { name } => {
            let name = name.trim().to_owned();
            if name.is_empty() || name.chars().count() > 64 {
                return Err(io::Error::other("user names have 1 to 64 characters"));
            }
            if store.user_by_name(&name).is_some() {
                return Err(io::Error::other(format!("there already is a user {name}")));
            }
            let password = read_password()?;
            auth::check_new_password(&password).map_err(io::Error::other)?;
            let hash = auth::hash_password(&password).map_err(io::Error::other)?;
            store.write(|d| {
                d.users.push(User {
                    id: random_id(),
                    name: name.clone(),
                    password: Some(hash),
                    identities: Vec::new(),
                    created: now(),
                })
            })?;
            eprintln!("scriptschnell-term-web: added {name}");
        }
        UserCommand::Passwd { name } => {
            let user = store
                .user_by_name(&name)
                .ok_or_else(|| io::Error::other(format!("no user {name}")))?;
            let password = read_password()?;
            auth::check_new_password(&password).map_err(io::Error::other)?;
            let hash = auth::hash_password(&password).map_err(io::Error::other)?;
            store.write(|d| {
                if let Some(u) = d.users.iter_mut().find(|u| u.id == user.id) {
                    u.password = Some(hash);
                }
                // Browsers logged in with the old password log in again.
                d.sessions.retain(|s| s.user != user.id);
            })?;
            eprintln!("scriptschnell-term-web: changed the password of {name}");
        }
        UserCommand::Remove { name } => {
            let user = store
                .user_by_name(&name)
                .ok_or_else(|| io::Error::other(format!("no user {name}")))?;
            store.write(|d| {
                d.users.retain(|u| u.id != user.id);
                d.machines.retain(|m| m.owner != user.id);
                d.sessions.retain(|s| s.user != user.id);
            })?;
            eprintln!(
                "scriptschnell-term-web: removed {name}; restart the server to drop their machines' connections"
            );
        }
        UserCommand::List => {
            store.read(|d| {
                for user in &d.users {
                    let machines = d.machines.iter().filter(|m| m.owner == user.id).count();
                    let ways: Vec<&str> = user
                        .password
                        .iter()
                        .map(|_| "password")
                        .chain(user.identities.iter().map(|i| i.provider.as_str()))
                        .collect();
                    println!("{}\t{} machines\t{}", user.name, machines, ways.join(","));
                }
            });
        }
    }
    Ok(())
}
