//! Variables from the app's own environment that matter inside the
//! environments it starts terminals in.
//!
//! OpenTelemetry variables (`OTEL_*`, `CLAUDE_CODE_ENABLE_TELEMETRY`) tell
//! agents such as Claude Code where to send telemetry. Sandboxes inherit
//! them; devcontainers get them set, and a sandbox with a restricted network
//! may reach the collector's host.
//!
//! CA variables (`SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS` and
//! the others in [`CA_FILE_VARS`]) name certificate authorities the user's
//! tools trust, such as a company's TLS inspection CA. The sandbox proxy
//! trusts them for its upstream connections and puts them in the bundle it
//! hands to sandboxes, sandboxes may read the files, and devcontainers get
//! them in their trust store.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Variables that name a PEM file of trusted certificates.
pub const CA_FILE_VARS: [&str; 7] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
    "DENO_CERT",
];

/// OpenSSL's directory of trusted certificates, a path list.
pub const CA_DIR_VAR: &str = "SSL_CERT_DIR";

/// The variables OpenTelemetry exporters read their endpoint from.
const OTLP_ENDPOINTS: [&str; 4] = [
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
];

/// True for a variable that configures telemetry.
pub fn is_telemetry_var(name: &str) -> bool {
    name.starts_with("OTEL_")
        || name.starts_with("CLAUDE_CODE_OTEL_")
        || name == "CLAUDE_CODE_ENABLE_TELEMETRY"
}

/// The app's telemetry variables, sorted by name.
pub fn telemetry_vars() -> Vec<(String, String)> {
    telemetry_vars_in(std::env::vars_os())
}

fn telemetry_vars_in(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vars
        .into_iter()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| is_telemetry_var(key))
        .collect();
    out.sort();
    out
}

/// The hosts of the app's OTLP endpoints, without this machine's own names,
/// which a sandbox reaches without the proxy or not at all.
pub fn telemetry_hosts() -> Vec<String> {
    telemetry_hosts_in(|name| std::env::var(name).ok())
}

fn telemetry_hosts_in(var: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for value in OTLP_ENDPOINTS.iter().filter_map(|name| var(name)) {
        // OTEL_EXPORTER_OTLP_ENDPOINT may also be a plain `host:port`.
        let rest = value.split_once("://").map_or(value.as_str(), |(_, r)| r);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = match host_port.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => host_port.split(':').next().unwrap_or_default(),
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let local = host == "localhost" || host == "::1" || host.starts_with("127.");
        if !host.is_empty() && !local && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

/// The files and directories the app's CA variables name that exist.
pub fn ca_paths() -> Vec<PathBuf> {
    ca_paths_in(|name| std::env::var_os(name))
}

fn ca_paths_in(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let files = CA_FILE_VARS
        .iter()
        .filter_map(|name| var(name))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let dirs = var(CA_DIR_VAR)
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty() && path.is_dir());
    for path in files.chain(dirs) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// The certificates (DER) in the files and directories the app's CA
/// variables name, each once. Unreadable files and other PEM blocks are
/// skipped.
pub fn ca_certificates() -> Vec<Vec<u8>> {
    certificates_in(&ca_paths())
}

fn certificates_in(paths: &[PathBuf]) -> Vec<Vec<u8>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut add = |file: &Path| {
        let Ok(text) = std::fs::read(file) else {
            return;
        };
        for block in pem::parse_many(&text).unwrap_or_default() {
            if block.tag() == "CERTIFICATE" && seen.insert(block.contents().to_vec()) {
                out.push(block.into_contents());
            }
        }
    };
    for path in paths {
        if path.is_dir() {
            let Ok(entries) = std::fs::read_dir(path) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            for file in files.iter().filter(|f| f.is_file()) {
                add(file);
            }
        } else {
            add(path);
        }
    }
    out
}

/// `certs` as one PEM text.
pub fn to_pem(certs: &[Vec<u8>]) -> String {
    certs
        .iter()
        .map(|der| pem::encode(&pem::Pem::new("CERTIFICATE", der.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_variables_are_picked_by_name() {
        let vars = [
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
            ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
            ("CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS", "1000"),
            ("HOME", "/home/me"),
            ("MY_OTEL_THING", "x"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let names: Vec<String> = telemetry_vars_in(vars)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            names,
            [
                "CLAUDE_CODE_ENABLE_TELEMETRY",
                "CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS",
                "OTEL_EXPORTER_OTLP_ENDPOINT",
            ]
        );
    }

    #[test]
    fn otlp_endpoint_hosts_skip_this_machine() {
        let hosts = telemetry_hosts_in(|name| {
            match name {
                "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("https://user:pw@OTel.Example.com:4318/v1"),
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => Some("http://localhost:4318/v1/traces"),
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT" => Some("collector.internal:4317"),
                "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT" => Some("http://[2001:db8::1]:4317"),
                _ => None,
            }
            .map(str::to_owned)
        });
        assert_eq!(
            hosts,
            ["otel.example.com", "collector.internal", "2001:db8::1"]
        );
    }

    #[test]
    fn ca_files_and_directories_are_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = pem::encode(&pem::Pem::new("CERTIFICATE", b"a".to_vec()));
        let b = pem::encode(&pem::Pem::new("CERTIFICATE", b"b".to_vec()));
        let key = pem::encode(&pem::Pem::new("PRIVATE KEY", b"k".to_vec()));
        let file = dir.path().join("corp.pem");
        std::fs::write(&file, format!("{a}{key}")).unwrap();
        let certs = dir.path().join("certs");
        std::fs::create_dir(&certs).unwrap();
        std::fs::write(certs.join("1.pem"), format!("{a}{b}")).unwrap();
        let file_var = file.clone().into_os_string();
        let dir_var = certs.clone().into_os_string();
        let paths = ca_paths_in(|name| match name {
            "NODE_EXTRA_CA_CERTS" | "REQUESTS_CA_BUNDLE" => Some(file_var.clone()),
            "SSL_CERT_DIR" => Some(dir_var.clone()),
            "CURL_CA_BUNDLE" => Some(dir.path().join("missing.pem").into_os_string()),
            _ => None,
        });
        assert_eq!(paths, [file, certs]);
        assert_eq!(certificates_in(&paths), [b"a".to_vec(), b"b".to_vec()]);
    }
}
