//! The proxy for the app's own HTTP requests. The desktop's proxy setting
//! wins when it names a proxy server: GNOME and the desktops built on its
//! settings (through `gsettings`), KDE Plasma (`kioslaverc`) and Windows (the
//! Internet Settings in the registry). When the desktop has no proxy, uses a
//! PAC script, or is none of these, the `ALL_PROXY`, `HTTPS_PROXY`,
//! `HTTP_PROXY` and `NO_PROXY` variables apply.

#[cfg(all(unix, not(target_os = "macos")))]
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one answer is reused. Reading GNOME's settings starts a process.
const REUSE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxySettings {
    /// The proxy's URL, such as `http://proxy.internal:3128`.
    pub server: String,
    /// The hosts that bypass the proxy, comma-separated as in `NO_PROXY`.
    pub no_proxy: String,
}

/// The proxy to use now, or `None` to connect directly.
pub fn current() -> Option<ProxySettings> {
    static LAST: Mutex<Option<(Instant, Option<ProxySettings>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, settings)) = &*last
        && at.elapsed() < REUSE
    {
        return settings.clone();
    }
    let settings = match desktop() {
        Some(settings) => Some(settings),
        None => from_variables(|name| std::env::var(name).ok()),
    };
    *last = Some((Instant::now(), settings.clone()));
    settings
}

/// The proxy of `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY`, in that order,
/// upper case before lower case.
fn from_variables(var: impl Fn(&str) -> Option<String>) -> Option<ProxySettings> {
    let first = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| var(name).filter(|value| !value.trim().is_empty()))
    };
    let server = first(&[
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ])?;
    Some(ProxySettings {
        server: server.trim().to_owned(),
        no_proxy: first(&["NO_PROXY", "no_proxy"]).unwrap_or_default(),
    })
}

/// The desktop's proxy server, when the desktop names one.
#[cfg(windows)]
fn desktop() -> Option<ProxySettings> {
    windows::settings()
}

/// The desktop's proxy server, when the desktop names one.
#[cfg(all(unix, not(target_os = "macos")))]
fn desktop() -> Option<ProxySettings> {
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut desktops = desktops.split(':').map(str::to_ascii_lowercase);
    let desktop = desktops.find_map(|name| match name.as_str() {
        "kde" => Some(true),
        "gnome" | "unity" | "x-cinnamon" | "cinnamon" | "budgie" | "pantheon" | "mate" => {
            Some(false)
        }
        _ => None,
    })?;
    if desktop {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config"))
            })?;
        kde(&std::fs::read_to_string(config.join("kioslaverc")).ok()?)
    } else {
        let output = Command::new("gsettings")
            .args(["list-recursively", "org.gnome.system.proxy"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        gnome(&String::from_utf8_lossy(&output.stdout))
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
fn desktop() -> Option<ProxySettings> {
    None
}

/// GNOME's settings as `gsettings list-recursively org.gnome.system.proxy`
/// prints them. Only the `manual` mode names a server; `auto` is a PAC
/// script, which the app cannot run.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn gnome(listing: &str) -> Option<ProxySettings> {
    let value = |schema: &str, key: &str| {
        listing.lines().find_map(|line| {
            let rest = line.strip_prefix(schema)?.strip_prefix(' ')?;
            let rest = rest.strip_prefix(key)?.strip_prefix(' ')?;
            Some(rest.trim())
        })
    };
    let text = |raw: &str| raw.trim_matches('\'').to_owned();
    if value("org.gnome.system.proxy", "mode").map(text)? != "manual" {
        return None;
    }
    let server = ["https", "http"].iter().find_map(|kind| {
        let schema = format!("org.gnome.system.proxy.{kind}");
        let host = value(&schema, "host").map(text)?;
        let port = value(&schema, "port")?.parse::<u16>().ok()?;
        (!host.is_empty() && port != 0).then(|| format!("http://{host}:{port}"))
    })?;
    let hosts = value("org.gnome.system.proxy", "ignore-hosts").unwrap_or("[]");
    let no_proxy = hosts
        .trim_start_matches("@as ")
        .trim_matches(['[', ']'])
        .split(',')
        .map(|host| text(host.trim()))
        .filter(|host| !host.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    Some(ProxySettings { server, no_proxy })
}

/// KDE's settings from `kioslaverc`. Only `ProxyType=1` (manual) names a
/// server; KDE stores it as `http://host 8080` or `http://host:8080`. A
/// reversed exception list, which sends only the listed hosts through the
/// proxy, is left to the variables.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn kde(config: &str) -> Option<ProxySettings> {
    let mut in_section = false;
    let mut keys = std::collections::HashMap::new();
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line == "[Proxy Settings]";
        } else if in_section && let Some((key, value)) = line.split_once('=') {
            keys.insert(key.trim(), value.trim());
        }
    }
    if keys.get("ProxyType") != Some(&"1") || keys.get("ReversedException") == Some(&"true") {
        return None;
    }
    let server = ["httpsProxy", "httpProxy"].iter().find_map(|key| {
        let value = keys.get(key).filter(|value| !value.is_empty())?;
        let value = match value.rsplit_once(' ') {
            Some((host, port)) => format!("{}:{port}", host.trim()),
            None => (*value).to_owned(),
        };
        Some(if value.contains("://") {
            value
        } else {
            format!("http://{value}")
        })
    })?;
    let no_proxy = keys
        .get("NoProxyFor")
        .copied()
        .unwrap_or_default()
        .to_owned();
    Some(ProxySettings { server, no_proxy })
}

/// Windows' `ProxyServer` value: `host:port`, or per protocol
/// `http=host:port;https=host:port`. `ProxyOverride` separates hosts with
/// `;`; `<local>`, every host without a dot, has no `NO_PROXY` form.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_values(server: &str, overrides: &str) -> Option<ProxySettings> {
    let server = server.trim();
    let pick = if server.contains('=') {
        let by_protocol = |name: &str| {
            server.split(';').find_map(|part| {
                let (protocol, address) = part.split_once('=')?;
                (protocol.trim().eq_ignore_ascii_case(name) && !address.trim().is_empty())
                    .then(|| address.trim())
            })
        };
        by_protocol("https").or_else(|| by_protocol("http"))?
    } else {
        server
    };
    if pick.is_empty() {
        return None;
    }
    let server = if pick.contains("://") {
        pick.to_owned()
    } else {
        format!("http://{pick}")
    };
    let no_proxy = overrides
        .split(';')
        .map(str::trim)
        .filter(|host| !host.is_empty() && *host != "<local>")
        .collect::<Vec<_>>()
        .join(",");
    Some(ProxySettings { server, no_proxy })
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn dword(name: &str) -> Option<u32> {
        let (key, name) = (wide(KEY), wide(name));
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: the key and name are NUL-terminated and `value` holds `size` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&raw mut value).cast(),
                &mut size,
            )
        };
        (status == 0).then_some(value)
    }

    fn string(name: &str) -> Option<String> {
        let (key, name) = (wide(KEY), wide(name));
        let mut size = 0u32;
        // SAFETY: a null buffer asks only for the size in bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY: `buffer` holds `size` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..end]))
    }

    pub(super) fn settings() -> Option<super::ProxySettings> {
        if dword("ProxyEnable")? != 1 {
            return None;
        }
        super::windows_values(
            &string("ProxyServer")?,
            &string("ProxyOverride").unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(server: &str, no_proxy: &str) -> Option<ProxySettings> {
        Some(ProxySettings {
            server: server.into(),
            no_proxy: no_proxy.into(),
        })
    }

    #[test]
    fn variables_prefer_all_then_https_then_http() {
        let vars = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://secure:1".to_owned()),
            "http_proxy" => Some("http://plain:2".to_owned()),
            "no_proxy" => Some("example.com".to_owned()),
            _ => None,
        };
        assert_eq!(
            from_variables(vars),
            settings("http://secure:1", "example.com")
        );
        assert_eq!(from_variables(|_| None), None);
    }

    #[test]
    fn gnome_names_a_server_only_in_manual_mode() {
        let listing = "\
org.gnome.system.proxy autoconfig-url ''
org.gnome.system.proxy ignore-hosts ['localhost', '127.0.0.0/8', 'codefionn.eu']
org.gnome.system.proxy mode 'manual'
org.gnome.system.proxy.http host 'plain.internal'
org.gnome.system.proxy.http port 8080
org.gnome.system.proxy.https host ''
org.gnome.system.proxy.https port 0
";
        assert_eq!(
            gnome(listing),
            settings(
                "http://plain.internal:8080",
                "localhost,127.0.0.0/8,codefionn.eu"
            )
        );
        assert_eq!(gnome(&listing.replace("'manual'", "'none'")), None);
        assert_eq!(gnome(&listing.replace("'manual'", "'auto'")), None);
    }

    #[test]
    fn kde_names_a_server_only_when_manual() {
        let config = "\
[Proxy Settings]
NoProxyFor=localhost,codefionn.eu
ProxyType=1
httpProxy=http://plain.internal 8080
httpsProxy=http://secure.internal 3128

[Other]
ProxyType=0
";
        assert_eq!(
            kde(config),
            settings("http://secure.internal:3128", "localhost,codefionn.eu")
        );
        assert_eq!(kde(&config.replace("ProxyType=1", "ProxyType=0")), None);
        assert_eq!(kde(&config.replace("ProxyType=1", "ProxyType=4")), None);
        let reversed = config.replace("ProxyType=1", "ProxyType=1\nReversedException=true");
        assert_eq!(kde(&reversed), None);
    }

    #[test]
    fn windows_picks_the_https_server_and_drops_local() {
        assert_eq!(
            windows_values(
                "http=plain:80;https=secure:443",
                "*.corp;<local>;codefionn.eu"
            ),
            settings("http://secure:443", "*.corp,codefionn.eu")
        );
        assert_eq!(
            windows_values("proxy:3128", ""),
            settings("http://proxy:3128", "")
        );
        assert_eq!(windows_values("", ""), None);
    }
}
