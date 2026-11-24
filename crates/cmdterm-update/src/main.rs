//! `scriptschnell-term-release`: makes release keys and signs the update manifests
//! that `cmdterm-update` checks. The release workflow runs it; see the
//! crate README.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmdterm_update::{FileEntry, Manifest, SIGNATURE_SUFFIX, SigningKey, VerifyingKey};

/// The variable `sign` reads the secret key from.
const KEY_VARIABLE: &str = "SCRIPTSCHNELL_RELEASE_SIGNING_KEY";

#[derive(Parser)]
#[command(name = "scriptschnell-term-release", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Writes a new secret key to OUT and its public key to OUT.pub.
    Keygen { out: PathBuf },
    /// Hashes every file in FILES_DIR (the files as installed, such as
    /// scriptschnell-term.exe), copies each into OUT as a release asset
    /// named <PLATFORM>-<name>, and writes the signed
    /// manifest-<PLATFORM>.json and its .sig into OUT. Reads the secret key
    /// from SCRIPTSCHNELL_RELEASE_SIGNING_KEY.
    Sign {
        #[arg(long)]
        version: String,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        out: PathBuf,
        files_dir: PathBuf,
    },
    /// Checks manifest-<PLATFORM>.json in ASSETS_DIR against PUBLIC_KEY and
    /// every asset it lists against its hash.
    Verify {
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        platform: String,
        assets_dir: PathBuf,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Keygen { out } => keygen(&out),
        Command::Sign {
            version,
            platform,
            out,
            files_dir,
        } => sign(&version, &platform, &files_dir, &out),
        Command::Verify {
            public_key,
            platform,
            assets_dir,
        } => verify(&public_key, &platform, &assets_dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("scriptschnell-term-release: {error}");
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

fn sign(version: &str, platform: &str, files_dir: &Path, out: &Path) -> Result<(), String> {
    let secret = std::env::var(KEY_VARIABLE).map_err(|_| format!("{KEY_VARIABLE} is not set"))?;
    let key = SigningKey::from_secret(&secret).ok_or("the signing key is invalid")?;
    let mut names: Vec<String> = std::fs::read_dir(files_dir)
        .map_err(|e| format!("{}: {e}", files_dir.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let asset = cmdterm_update::file_asset(platform, &name);
        let source = std::fs::File::open(files_dir.join(&name)).map_err(|e| e.to_string())?;
        let target = out.join(&asset);
        let copy =
            std::fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        let (sha3_256, size) = cmdterm_update::hash(source, copy).map_err(|e| e.to_string())?;
        files.push(FileEntry {
            path: asset,
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
    // Checks names, paths and the version as the app will.
    let signature = cmdterm_update::sign(&key, &json);
    cmdterm_update::verify(&key.public(), &json, &signature).map_err(|e| e.to_string())?;
    let manifest_path = out.join(cmdterm_update::manifest_asset(platform));
    std::fs::write(&manifest_path, &json).map_err(|e| e.to_string())?;
    std::fs::write(signature_path(&manifest_path), signature).map_err(|e| e.to_string())?;
    println!(
        "signed {} files of {platform} {version}",
        manifest.files.len()
    );
    Ok(())
}

fn signature_path(manifest: &Path) -> PathBuf {
    let mut path = manifest.as_os_str().to_owned();
    path.push(SIGNATURE_SUFFIX);
    path.into()
}

fn verify(public_key: &str, platform: &str, assets_dir: &Path) -> Result<(), String> {
    let key = VerifyingKey::parse(public_key).ok_or("the public key is invalid")?;
    let manifest_path = assets_dir.join(cmdterm_update::manifest_asset(platform));
    let json =
        std::fs::read(&manifest_path).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let signature =
        std::fs::read_to_string(signature_path(&manifest_path)).map_err(|e| e.to_string())?;
    let manifest = cmdterm_update::verify(&key, &json, &signature).map_err(|e| e.to_string())?;
    if manifest.platform != platform {
        return Err(format!("the manifest is for {}", manifest.platform));
    }
    for file in &manifest.files {
        let path = assets_dir.join(&file.path);
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
