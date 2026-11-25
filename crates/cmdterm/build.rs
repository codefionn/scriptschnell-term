//! Lists the third-party crates linked into the app, with their licenses and
//! license files, for the Licenses page of the settings. It asks
//! `cargo metadata` for the dependency graph of this crate on the target
//! platform with the enabled features, from `Cargo.lock`, so the list
//! matches what the build links. It asks offline first, and online when
//! that fails: `cargo metadata` needs the manifest of every package in the
//! graph, dev dependencies such as `proptest`'s included, which a plain
//! `cargo build` never downloads. Without the graph the list is empty and
//! the page says so.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// The files a crate keeps its license and notices in, by the start of their
/// upper-cased name.
const LICENSE_FILES: [&str; 6] = [
    "LICENSE",
    "LICENCE",
    "COPYING",
    "NOTICE",
    "UNLICENSE",
    "COPYRIGHT",
];

struct Crate {
    name: String,
    version: String,
    license: String,
    repository: String,
    texts: Vec<usize>,
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir.join("../..");
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("Cargo.lock").display()
    );
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let mut texts = Vec::new();
    let crates = third_party(&manifest_dir, &mut texts).unwrap_or_else(|err| {
        println!("cargo:warning=the Licenses page lists no crates: {err}");
        texts.clear();
        Vec::new()
    });

    let mut out =
        String::from("/// License files, each once.\npub static LICENSE_TEXTS: &[&str] = &[\n");
    for text in &texts {
        writeln!(out, "    {text:?},").unwrap();
    }
    out.push_str("];\n\n/// Third-party crates linked into the app, by name.\npub static CRATES: &[ThirdParty] = &[\n");
    for c in &crates {
        writeln!(
            out,
            "    ThirdParty {{ name: {:?}, version: {:?}, license: {:?}, repository: {:?}, texts: &{:?} }},",
            c.name, c.version, c.license, c.repository, c.texts
        )
        .unwrap();
    }
    out.push_str("];\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("licenses.rs");
    std::fs::write(dest, out).unwrap();
}

/// The crates this one links, apart from the workspace's own, sorted by
/// name and version. `texts` collects their license files.
fn third_party(manifest_dir: &Path, texts: &mut Vec<String>) -> Result<Vec<Crate>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let target = std::env::var("TARGET").map_err(|err| err.to_string())?;
    // The enabled features, from their variables: `CARGO_FEATURE_DEBUG_INSTRUMENTATION`
    // is `debug-instrumentation`.
    let features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| {
            key.strip_prefix("CARGO_FEATURE_")
                .map(|name| name.to_lowercase().replace('_', "-"))
        })
        .filter(|name| name != "default")
        .collect();
    let metadata = |offline: bool| -> Result<Vec<u8>, String> {
        let mut command = std::process::Command::new(&cargo);
        command
            .arg("metadata")
            .args(["--format-version", "1", "--locked"])
            .args(["--filter-platform", &target])
            .arg("--manifest-path")
            .arg(manifest_dir.join("Cargo.toml"));
        if offline {
            command.arg("--offline");
        }
        if !features.is_empty() {
            command.args(["--features", &features.join(",")]);
        }
        let output = command.output().map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        Ok(output.stdout)
    };
    let stdout = metadata(true).or_else(|offline| {
        metadata(false).map_err(|online| format!("{offline}; online: {online}"))
    })?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&stdout).map_err(|err| err.to_string())?;

    let packages: HashMap<&str, &serde_json::Value> = metadata["packages"]
        .as_array()
        .ok_or("no packages")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    let members: HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .ok_or("no workspace members")?
        .iter()
        .filter_map(|id| id.as_str())
        .collect();
    let nodes: HashMap<&str, &serde_json::Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("no dependency graph")?
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    let manifest = manifest_dir.join("Cargo.toml");
    let root = packages
        .iter()
        .find(|(_, p)| {
            p["name"] == "cmdterm"
                && p["manifest_path"]
                    .as_str()
                    .is_some_and(|path| same_file(Path::new(path), &manifest))
        })
        .map(|(id, _)| *id)
        .ok_or("this crate is not in the graph")?;

    // Normal dependencies only: build and dev dependencies are not in the
    // binary.
    let mut seen = HashSet::from([root]);
    let mut queue = vec![root];
    while let Some(id) = queue.pop() {
        let Some(node) = nodes.get(id) else { continue };
        for dep in node["deps"].as_array().into_iter().flatten() {
            let normal = dep["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind["kind"].is_null());
            if let (true, Some(pkg)) = (normal, dep["pkg"].as_str())
                && seen.insert(pkg)
            {
                queue.push(pkg);
            }
        }
    }

    let mut known: BTreeMap<String, usize> = BTreeMap::new();
    let mut crates = Vec::new();
    for id in seen {
        if members.contains(id) {
            continue;
        }
        let Some(package) = packages.get(id) else {
            continue;
        };
        let text = |key: &str| package[key].as_str().unwrap_or_default().to_owned();
        let mut files = Vec::new();
        if let Some(dir) = package["manifest_path"]
            .as_str()
            .and_then(|path| Path::new(path).parent())
        {
            if let Some(file) = package["license_file"].as_str() {
                files.push(dir.join(file));
            }
            let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path.file_name().is_some_and(|name| {
                            let name = name.to_string_lossy().to_uppercase();
                            LICENSE_FILES.iter().any(|start| name.starts_with(start))
                        })
                })
                .collect();
            found.sort();
            files.extend(found);
        }
        let mut indexes = Vec::new();
        for file in files {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let content = content.replace("\r\n", "\n").trim().to_owned();
            if content.is_empty() {
                continue;
            }
            let index = *known.entry(content.clone()).or_insert_with(|| {
                texts.push(content);
                texts.len() - 1
            });
            if !indexes.contains(&index) {
                indexes.push(index);
            }
        }
        crates.push(Crate {
            name: text("name"),
            version: text("version"),
            license: text("license"),
            repository: text("repository"),
            texts: indexes,
        });
    }
    crates.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    Ok(crates)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
