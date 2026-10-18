//! Ready-made VM images: Alpine and Ubuntu, built without root.
//!
//! [`build`] downloads the firecracker binary and a Firecracker CI kernel
//! once — both pinned to an exact version with a SHA-256 that is checked
//! before the file is used, so a compromised mirror cannot slip in a binary —
//! builds a root filesystem from a container image pinned by digest with
//! podman or docker, adds the guest agent with the C libraries it was linked against
//! (so it runs on any distribution's libc, or none), installs an init that
//! starts it, and packs everything with `fakeroot mkfs.ext4 -d`. [`configure`]
//! then adds the image to the `firecracker` section of `server.json`, so a VM
//! named after the image starts from it.
//!
//! Everything lives in `<config-dir>/remote/images/`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use cmdterm_env::Environment;

use crate::Paths;
use crate::server::ServerConfig;

/// A ready-made image.
#[derive(Debug)]
pub struct Preset {
    /// Image name in `server.json`, and the VM name that uses it.
    pub name: &'static str,
    pub title: &'static str,
    /// What it holds, for the settings page.
    pub summary: &'static str,
    /// Size of the ext4 file. It is sparse, so unused space costs nothing.
    size_mib: u32,
    containerfile: &'static str,
}

pub const PRESETS: [Preset; 2] = [
    Preset {
        name: "alpine",
        title: "Alpine 3.22",
        summary: "small, apk, bash, git, curl",
        size_mib: 2048,
        // Pinned by digest (manifest list, so each architecture resolves to
        // its own image): the tag alone can be moved by the registry.
        containerfile: "FROM docker.io/library/alpine:3.22@sha256:2e0b81d60597f449baa56dc25d355367869d2dca4cd57e866f1e2b52af3eb55f
RUN apk add --no-cache bash git curl ca-certificates less procps iproute2 \
    libgcc libstdc++ ripgrep openssh-client \
 && sed -i 's#^root:\\(.*\\):/bin/sh$#root:\\1:/bin/bash#' /etc/passwd
",
    },
    Preset {
        name: "ubuntu",
        title: "Ubuntu 24.04",
        summary: "apt, build tools, git, curl",
        size_mib: 4096,
        containerfile: "FROM docker.io/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    git curl ca-certificates bash less procps iproute2 build-essential \
    openssh-client ripgrep \
 && rm -rf /var/lib/apt/lists/*
",
    },
];

/// The preset called `name`.
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

const CI_KERNEL: &str = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12";
const KERNEL_VERSION: &str = "6.1.128";
/// The Firecracker release the image builds with, pinned instead of
/// "latest", and the SHA-256 of each release archive, checked after the
/// download and before the binary is ever run.
const FIRECRACKER_VERSION: &str = "v1.17.0";
const FIRECRACKER_SHA256: &[(&str, &str)] = &[
    (
        "x86_64",
        "06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558",
    ),
    (
        "aarch64",
        "e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256",
    ),
];
/// SHA-256 of the CI kernel per architecture, checked the same way.
const KERNEL_SHA256: &[(&str, &str)] = &[
    (
        "x86_64",
        "27a8310b9a727517e9eb02044524b6ceb77de5728e3491b6974d5c846227ecc8",
    ),
    (
        "aarch64",
        "cb1291c66bca75bc11cb9c8357fcef9965bb1786dffcb42a60923c3e0e49f319",
    ),
];

/// The pinned SHA-256 for `arch` in `table`.
fn pinned_sha256(arch: &str, table: &[(&'static str, &'static str)]) -> io::Result<&'static str> {
    table
        .iter()
        .find(|(a, _)| *a == arch)
        .map(|(_, hash)| *hash)
        .ok_or_else(|| io::Error::other(format!("no pinned SHA-256 for {arch}; add it")))
}

/// SHA-256 of `bytes`, hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// The vsock port the init starts the agent on, the config's default.
const GUEST_PORT: u32 = 7723;

/// Where images, the kernel and the firecracker binary are kept.
pub fn images_dir(paths: &Paths) -> PathBuf {
    paths.data.join("images")
}

/// The finished root filesystem of `preset`.
pub fn rootfs_path(paths: &Paths, preset: &Preset) -> PathBuf {
    images_dir(paths).join(format!("{}.ext4", preset.name))
}

/// How [`build`] runs.
pub struct BuildOptions {
    /// `podman`, `docker`, or a path. `None` prefers podman, which needs no
    /// daemon.
    pub engine: Option<String>,
    /// The `cmdterm-remote` executable to put into the image.
    pub guest: PathBuf,
    /// Full command output goes here.
    pub log: PathBuf,
}

fn has_command(name: &str) -> bool {
    Environment::Local
        .command("sh")
        .args(["-c", "command -v \"$1\" >/dev/null", "sh", name])
        .stdout()
        .is_ok()
}

/// The container engine `build` uses.
pub fn engine(choice: Option<&str>) -> String {
    match choice {
        Some(engine) => engine.to_owned(),
        None if has_command("podman") => "podman".into(),
        None => "docker".into(),
    }
}

/// Programs `build` needs that are not installed.
pub fn missing_tools(engine_choice: Option<&str>) -> Vec<String> {
    let engine = engine(engine_choice);
    [
        engine.as_str(),
        "curl",
        "tar",
        "fakeroot",
        "mkfs.ext4",
        "ldd",
    ]
    .into_iter()
    .filter(|tool| !has_command(tool))
    .map(str::to_owned)
    .collect()
}

/// True when VMs can get a network without root, through pasta.
pub fn user_network_available() -> bool {
    has_command("pasta") && has_command("ip")
}

/// A program name resolved through `PATH`, or the path as given.
pub fn resolve_program(program: &Path) -> PathBuf {
    if program.components().count() > 1 {
        return program.to_path_buf();
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
        .unwrap_or_else(|| program.to_path_buf())
}

struct Builder {
    log: std::fs::File,
    progress: Box<dyn FnMut(&str)>,
}

impl Builder {
    fn step(&mut self, text: &str) {
        let _ = writeln!(self.log, "== {text}");
        (self.progress)(text);
    }

    /// Runs `command`, logging its output. Fails with the last lines of
    /// stderr.
    fn run(&mut self, what: &str, command: cmdterm_env::EnvCommand) -> io::Result<Vec<u8>> {
        let output = command
            .output()
            .map_err(|error| io::Error::other(format!("{what}: {error}")))?;
        let _ = self.log.write_all(&output.stderr);
        if output.status.success() {
            return Ok(output.stdout);
        }
        let _ = self.log.write_all(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        Err(io::Error::other(format!(
            "{what} failed: {}",
            tail.join(" | ")
        )))
    }

    /// Downloads `url` to `to`, checking its SHA-256 against `expected`
    /// before the file is renamed into place. A mismatch deletes the
    /// download and fails, so a tampered or mirrored file is never used.
    fn download(&mut self, url: &str, to: &Path, expected: &str) -> io::Result<()> {
        let partial = to.with_extension("partial");
        self.run(
            &format!("downloading {url}"),
            Environment::Local
                .command("curl")
                .args(["-fsSL", "--retry", "2", "-o"])
                .arg(&partial)
                .arg(url),
        )?;
        let got = sha256_hex(&std::fs::read(&partial)?);
        if got != expected {
            let _ = std::fs::remove_file(&partial);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{url}: SHA-256 {got} does not match the pinned {expected}"),
            ));
        }
        std::fs::rename(partial, to)
    }
}

/// Builds `preset` into [`rootfs_path`], downloading the firecracker binary
/// and kernel first if they are missing. `progress` gets a short line per
/// step. Takes minutes on first use; later builds reuse the downloads.
pub fn build(
    paths: &Paths,
    preset: &Preset,
    options: &BuildOptions,
    progress: impl FnMut(&str) + 'static,
) -> io::Result<PathBuf> {
    let dir = images_dir(paths);
    crate::create_private_dir(&paths.dir)?;
    std::fs::create_dir_all(&dir)?;
    if let Some(parent) = options.log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut b = Builder {
        log: std::fs::File::create(&options.log)?,
        progress: Box::new(progress),
    };
    let work = dir.join(format!("{}.build", preset.name));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = build_in(&mut b, &dir, &work, preset, options);
    let _ = std::fs::remove_dir_all(&work);
    if let Err(error) = &result {
        let _ = writeln!(b.log, "error: {error}");
    }
    result
}

fn build_in(
    b: &mut Builder,
    dir: &Path,
    work: &Path,
    preset: &Preset,
    options: &BuildOptions,
) -> io::Result<PathBuf> {
    let arch = std::env::consts::ARCH;
    if !dir.join("firecracker").is_file() {
        let version = FIRECRACKER_VERSION;
        b.step(&format!("Downloading Firecracker {version} (pinned)"));
        let archive = work.join("firecracker.tgz");
        b.download(
            &format!(
                "https://github.com/firecracker-microvm/firecracker/releases/download/\
                 {version}/firecracker-{version}-{arch}.tgz"
            ),
            &archive,
            pinned_sha256(arch, FIRECRACKER_SHA256)?,
        )?;
        b.run(
            "unpacking Firecracker",
            Environment::Local
                .command("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(work),
        )?;
        let unpacked = work
            .join(format!("release-{version}-{arch}"))
            .join(format!("firecracker-{version}-{arch}"));
        let partial = dir.join("firecracker.partial");
        std::fs::copy(unpacked, &partial)?;
        set_mode(&partial, 0o755)?;
        std::fs::rename(partial, dir.join("firecracker"))?;
    }
    if !dir.join("vmlinux").is_file() {
        b.step("Downloading the kernel (pinned)");
        b.download(
            &format!("{CI_KERNEL}/{arch}/vmlinux-{KERNEL_VERSION}"),
            &dir.join("vmlinux"),
            pinned_sha256(arch, KERNEL_SHA256)?,
        )?;
    }

    let engine = engine(options.engine.as_deref());
    let tag = format!("localhost/cmdterm-vm-{}", preset.name);
    b.step(&format!(
        "Building the {} image with {engine}",
        preset.title
    ));
    std::fs::write(work.join("Containerfile"), preset.containerfile)?;
    b.run(
        "building the image",
        Environment::Local
            .command(&engine)
            .args(["build", "-t", &tag, "-f"])
            .arg(work.join("Containerfile"))
            .arg(work),
    )?;
    let id = b.run(
        "creating a container",
        Environment::Local
            .command(&engine)
            .args(["create", &tag, "/bin/true"]),
    )?;
    let id = String::from_utf8_lossy(&id).trim().to_owned();
    let exported = b.run(
        "exporting the root filesystem",
        Environment::Local
            .command(&engine)
            .args(["export", "-o"])
            .arg(work.join("rootfs.tar"))
            .arg(&id),
    );
    let _ = b.run(
        "removing the container",
        Environment::Local.command(&engine).args(["rm", &id]),
    );
    let _ = b.run(
        "removing the image",
        Environment::Local.command(&engine).args(["rmi", &tag]),
    );
    exported?;

    b.step("Adding the guest agent");
    let guest = resolve_program(&options.guest);
    let ldd = Environment::Local.command("ldd").arg(&guest).output()?;
    let libraries = parse_ldd(&String::from_utf8_lossy(&ldd.stdout));
    let bundle = work.join("cmdterm");
    std::fs::create_dir_all(bundle.join("bin"))?;
    std::fs::create_dir_all(bundle.join("lib"))?;
    std::fs::copy(&guest, bundle.join("bin/cmdterm-remote"))?;
    set_mode(&bundle.join("bin/cmdterm-remote"), 0o755)?;
    let mut loader = None;
    for library in &libraries {
        std::fs::copy(&library.path, bundle.join("lib").join(&library.name))?;
        if library.loader {
            loader = Some(library.name.clone());
        }
    }
    std::fs::write(work.join("init"), init_script(loader.as_deref()))?;

    b.step("Packing the disk");
    let partial = dir.join(format!("{}.ext4.partial", preset.name));
    let _ = std::fs::remove_file(&partial);
    b.run(
        "packing the disk",
        Environment::Local
            .command("fakeroot")
            .args([
                "sh",
                "-c",
                "set -e
                 mkdir rootfs
                 tar -xpf rootfs.tar -C rootfs
                 rm -f rootfs.tar
                 mkdir -p rootfs/opt
                 cp -a cmdterm rootfs/opt/cmdterm
                 rm -f rootfs/sbin/init
                 install -m 755 init rootfs/sbin/init
                 mkfs.ext4 -q -d rootfs -L rootfs \"$1\" \"$2\"M",
                "sh",
            ])
            .arg(&partial)
            .arg(preset.size_mib.to_string())
            .cwd(work),
    )?;
    let rootfs = dir.join(format!("{}.ext4", preset.name));
    std::fs::rename(partial, &rootfs)?;
    b.step("Done");
    Ok(rootfs)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    crate::sys::set_mode(path, mode)
}

/// A shared library the guest agent loads.
#[derive(Debug, PartialEq, Eq)]
struct Library {
    /// File name in the bundle: the soname.
    name: String,
    path: PathBuf,
    /// The dynamic loader, which runs the agent.
    loader: bool,
}

/// Libraries from `ldd` output. Empty for a static binary.
fn parse_ldd(output: &str) -> Vec<Library> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.split(" (0x").next()?;
            let (name, path) = match line.split_once(" => ") {
                Some((name, path)) => (name.trim(), path.trim()),
                None if line.starts_with('/') => (line, line),
                None => return None,
            };
            if !path.starts_with('/') {
                return None;
            }
            Some(Library {
                name: Path::new(name).file_name()?.to_string_lossy().into_owned(),
                path: PathBuf::from(path),
                loader: name.starts_with('/'),
            })
        })
        .collect()
}

/// PID 1 of the guest: mounts, DNS from the kernel command line, the agent,
/// and reaping orphans. With a `loader`, the agent runs through the bundled
/// dynamic loader and libraries, so the image's own libc does not matter.
fn init_script(loader: Option<&str>) -> String {
    let agent = match loader {
        Some(loader) => format!(
            "/opt/cmdterm/lib/{loader} --library-path /opt/cmdterm/lib \
             /opt/cmdterm/bin/cmdterm-remote"
        ),
        None => "/opt/cmdterm/bin/cmdterm-remote".to_owned(),
    };
    format!(
        r#"#!/bin/sh
# PID 1 of cmdterm VMs, written by cmdterm-remote image build.
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t tmpfs tmpfs /tmp
mount -t tmpfs tmpfs /run
mount -o remount,rw /
hostname cmdterm-vm
ip link set lo up 2>/dev/null
for word in $(cat /proc/cmdline); do
  case $word in cmdterm.dns=*) dns=${{word#cmdterm.dns=}} ;; esac
done
if [ -n "$dns" ]; then
  rm -f /etc/resolv.conf
  echo "nameserver $dns" > /etc/resolv.conf
fi
export HOME=/root USER=root LANG=C.UTF-8
export PATH=/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
if [ -x /bin/bash ]; then export SHELL=/bin/bash; else export SHELL=/bin/sh; fi
cd /root
{agent} guest --vsock-port {GUEST_PORT} &
while :; do wait; sleep 1; done
"#
    )
}

/// Adds a built image to `server.json`: the image under its name, and the
/// downloaded binary, kernel, a disk folder, a default image and user
/// networking where the section does not set them yet.
pub fn configure(paths: &Paths, preset: &Preset) -> io::Result<ServerConfig> {
    let dir = images_dir(paths);
    let rootfs = rootfs_path(paths, preset);
    let network = user_network_available();
    let state_dir = paths.data.join("vms");
    ServerConfig::edit(paths, |map| {
        let entry = map
            .entry("firecracker")
            .or_insert_with(|| serde_json::json!({}));
        if !entry.is_object() {
            *entry = serde_json::json!({});
        }
        let fc = entry.as_object_mut().expect("an object");
        for (key, value) in [
            ("binary", dir.join("firecracker")),
            ("kernel", dir.join("vmlinux")),
            ("state_dir", state_dir),
        ] {
            fc.entry(key.to_owned())
                .or_insert(value.display().to_string().into());
        }
        // An image the user set up by hand stays the default, and tap
        // devices they created stay the network.
        if !fc.contains_key("rootfs") {
            fc.entry("default_image".to_owned())
                .or_insert(preset.name.into());
        }
        if network && !fc.contains_key("network") {
            fc.entry("user_network".to_owned()).or_insert(true.into());
        }
        let images = fc
            .entry("images".to_owned())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(images) = images.as_object_mut() {
            images.insert(preset.name.into(), rootfs.display().to_string().into());
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_are_pinned_and_checked() {
        assert_eq!(
            pinned_sha256("x86_64", FIRECRACKER_SHA256).unwrap().len(),
            64
        );
        assert_eq!(pinned_sha256("aarch64", KERNEL_SHA256).unwrap().len(), 64);
        assert!(pinned_sha256("riscv64", FIRECRACKER_SHA256).is_err());
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(FIRECRACKER_VERSION.starts_with('v'));
        for preset in &PRESETS {
            assert!(
                preset.containerfile.contains("@sha256:"),
                "{} pins its base image by digest",
                preset.name
            );
        }
    }

    #[test]
    fn ldd_output_gives_libraries_and_the_loader() {
        let libraries = parse_ldd(
            "\tlinux-vdso.so.1 (0x00007f32ed810000)
\tlibgcc_s.so.1 => /usr/lib/libgcc_s.so.1 (0x00007f32ed3cb000)
\tlibc.so.6 => /usr/lib/libc.so.6 (0x00007f32ed000000)
\t/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x00007f32ed812000)
",
        );
        assert_eq!(libraries.len(), 3);
        assert_eq!(libraries[0].name, "libgcc_s.so.1");
        assert!(!libraries[0].loader);
        assert_eq!(libraries[2].name, "ld-linux-x86-64.so.2");
        assert_eq!(
            libraries[2].path,
            Path::new("/usr/lib64/ld-linux-x86-64.so.2")
        );
        assert!(libraries[2].loader);
        assert!(parse_ldd("\tnot a dynamic executable\n").is_empty());
        assert!(parse_ldd("\t/lib/ld-musl-x86_64.so.1 (0x7f00)\n")[0].loader);
    }

    #[test]
    fn init_runs_the_agent_through_the_bundled_loader() {
        let init = init_script(Some("ld-linux-x86-64.so.2"));
        assert!(init.contains(
            "/opt/cmdterm/lib/ld-linux-x86-64.so.2 --library-path /opt/cmdterm/lib \
             /opt/cmdterm/bin/cmdterm-remote guest --vsock-port 7723 &"
        ));
        assert!(init.contains("dns=${word#cmdterm.dns=}"));
        assert!(init_script(None).contains("\n/opt/cmdterm/bin/cmdterm-remote guest"));
    }

    #[test]
    fn configure_adds_the_image_and_keeps_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf()));
        crate::create_private_dir(&paths.dir).unwrap();
        std::fs::write(
            paths.server_config(),
            r#"{"firecracker": {"kernel": "/my/vmlinux", "rootfs": "/my/rootfs", "state_dir": "/vms"}}"#,
        )
        .unwrap();
        let config = configure(&paths, preset("alpine").unwrap()).unwrap();
        let config = configure(&paths, preset("ubuntu").unwrap())
            .map(|_| config)
            .unwrap();
        let fc = config.firecracker.unwrap();
        assert_eq!(fc.kernel, Path::new("/my/vmlinux"));
        assert_eq!(
            fc.default_image, None,
            "the hand-made rootfs stays the default"
        );
        let fc = ServerConfig::load(&paths).unwrap().firecracker.unwrap();
        assert_eq!(fc.images.len(), 2);
        assert!(fc.images["ubuntu"].ends_with("remote/images/ubuntu.ext4"));

        let fresh = Paths::new(Some(dir.path().join("fresh")));
        let fc = configure(&fresh, preset("ubuntu").unwrap())
            .unwrap()
            .firecracker
            .unwrap();
        assert_eq!(fc.default_image.as_deref(), Some("ubuntu"));
        assert!(fc.kernel.ends_with("fresh/remote/images/vmlinux"));
        assert!(fc.rootfs.is_none());
    }
}
