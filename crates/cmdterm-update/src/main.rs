//! `cmdterm-release`: makes release keys and signs the update manifests
//! that `cmdterm-update` checks. The release workflow runs it; see the
//! crate README.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmdterm_update::{FileEntry, MANIFEST, Manifest, SIGNATURE, SigningKey, VerifyingKey};

/// The variable `sign` reads the secret key from.
const KEY_VARIABLE: &str = "SCRIPTSCHNELL_RELEASE_SIGNING_KEY";

#[derive(Parser)]
#[command(name = "cmdterm-release", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Writes a new secret key to OUT and its public key to OUT.pub.
    Keygen { out: PathBuf },
    /// Hashes every file in <PLATFORM_DIR>/<VERSION>/ and writes the signed
    /// manifest.json and manifest.json.sig into PLATFORM_DIR. Reads the
    /// secret key from SCRIPTSCHNELL_RELEASE_SIGNING_KEY.
    Sign {
        #[arg(long)]
        version: String,
        #[arg(long)]
        platform: String,
        platform_dir: PathBuf,
    },
    /// Checks the manifest in PLATFORM_DIR against PUBLIC_KEY and every file
    /// it lists against its hash.
    Verify {
        #[arg(long)]
        public_key: String,
        platform_dir: PathBuf,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Keygen { out } => keygen(&out),
        Command::Sign {
            version,
            platform,
            platform_dir,
        } => sign(&version, &platform, &platform_dir),
        Command::Verify {
            public_key,
            platform_dir,
        } => verify(&public_key, &platform_dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cmdterm-release: {error}");
            ExitCode::FAILURE
        }
    }
}

fn keygen(out: &Path) -> Result<(), String> {
    let key = SigningKey::generate().map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    writeln!(file, "{}", key.to_secret().as_str()).map_err(|e| e.to_string())?;
    let mut public = out.as_os_str().to_owned();
    public.push(".pub");
    let line = key.public().to_line("scriptschnell-term release");
    std::fs::write(&public, format!("{line}\n")).map_err(|e| e.to_string())?;
    println!("{line}");
    Ok(())
}

fn sign(version: &str, platform: &str, platform_dir: &Path) -> Result<(), String> {
    let secret = std::env::var(KEY_VARIABLE).map_err(|_| format!("{KEY_VARIABLE} is not set"))?;
    let key = SigningKey::from_secret(&secret).ok_or("the signing key is invalid")?;
    let release_dir = platform_dir.join(version);
    let mut names: Vec<String> = std::fs::read_dir(&release_dir)
        .map_err(|e| format!("{}: {e}", release_dir.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let file = std::fs::File::open(release_dir.join(&name)).map_err(|e| e.to_string())?;
        let (sha3_256, size) = cmdterm_update::hash(file, io::sink()).map_err(|e| e.to_string())?;
        files.push(FileEntry {
            path: format!("{version}/{name}"),
            name,
            size,
            sha3_256,
        });
    }
    let manifest = Manifest {
        version: version.to_owned(),
        platform: platform.to_owned(),
        files,
    };
    let mut json = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    json.push(b'\n');
    let signature = cmdterm_update::sign(&key, &json);
    std::fs::write(platform_dir.join(MANIFEST), &json).map_err(|e| e.to_string())?;
    std::fs::write(platform_dir.join(SIGNATURE), signature).map_err(|e| e.to_string())?;
    println!(
        "signed {} files of {platform} {version}",
        manifest.files.len()
    );
    Ok(())
}

fn verify(public_key: &str, platform_dir: &Path) -> Result<(), String> {
    let key = VerifyingKey::parse(public_key).ok_or("the public key is invalid")?;
    let json = std::fs::read(platform_dir.join(MANIFEST)).map_err(|e| e.to_string())?;
    let signature =
        std::fs::read_to_string(platform_dir.join(SIGNATURE)).map_err(|e| e.to_string())?;
    let manifest = cmdterm_update::verify(&key, &json, &signature).map_err(|e| e.to_string())?;
    for file in &manifest.files {
        let path = platform_dir.join(&file.path);
        let body = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (hex, size) = cmdterm_update::hash(body, io::sink()).map_err(|e| e.to_string())?;
        if hex != file.sha3_256 || size != file.size {
            return Err(format!("{} does not match the manifest", path.display()));
        }
    }
    println!(
        "{} {}: signature and {} files check out",
        manifest.platform,
        manifest.version,
        manifest.files.len()
    );
    Ok(())
}
