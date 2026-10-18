//! The `ureq` proxy for the app's requests.
//!
//! `ureq` matches a `NO_PROXY` entry such as `example.com` only against that
//! exact host, so `git.example.com` would still go through the proxy. curl,
//! git and most other tools let `example.com`, `.example.com` and
//! `*.example.com` all cover the domain and every subdomain. [`build`] adds
//! the entries with those rules.

use ureq::Proxy;

/// The proxy at `server` that leaves out the hosts of `no_proxy`, a
/// comma-separated list as in `NO_PROXY`. `None` when `server` is no valid
/// proxy URL.
pub fn build(server: &str, no_proxy: &str) -> Option<Proxy> {
    with_no_proxy(Proxy::new(server).ok()?, Some(no_proxy))
}

/// `proxy` with the entries of the comma-separated `no_proxy` list.
/// Entries with a port lose it; CIDR ranges are skipped, since `ureq` only
/// compares host names.
fn with_no_proxy(proxy: Proxy, no_proxy: Option<&str>) -> Option<Proxy> {
    let mut builder = Proxy::builder(proxy.protocol())
        .host(proxy.host())
        .port(proxy.port())
        .resolve_target(proxy.resolve_target());
    if let Some(username) = proxy.username() {
        builder = builder.username(username);
    }
    if let Some(password) = proxy.password() {
        builder = builder.password(password);
    }
    for entry in no_proxy.unwrap_or_default().split(',') {
        let entry = entry.trim();
        if entry == "*" {
            builder = builder.no_proxy("*");
            continue;
        }
        let host = entry.trim_start_matches('*').trim_start_matches('.');
        let host = strip_port(host);
        if host.is_empty() || host.contains('/') {
            continue;
        }
        builder = builder.no_proxy(host).no_proxy(&format!(".{host}"));
    }
    builder.build().ok()
}

/// `host` without a trailing `:port`. An IPv6 address keeps its colons.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.parse::<u16>().is_ok() => name,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bypassed(no_proxy: &str, url: &str) -> bool {
        let proxy = Proxy::new("http://proxy.internal:3128").unwrap();
        let proxy = with_no_proxy(proxy, Some(no_proxy)).unwrap();
        proxy.is_no_proxy(&url.parse().unwrap())
    }

    #[test]
    fn a_domain_covers_its_subdomains() {
        let list = "fionn-router.internal, codefionn.eu,192.168.26.0/24,localhost";
        assert!(bypassed(list, "https://git.codefionn.eu/api/v1/user"));
        assert!(bypassed(list, "https://codefionn.eu/"));
        assert!(bypassed(list, "https://a.b.fionn-router.internal/"));
        assert!(bypassed(list, "http://localhost:3000/"));
        assert!(!bypassed(list, "https://notcodefionn.eu/"));
        assert!(!bypassed(list, "https://codeberg.org/"));
    }

    #[test]
    fn dots_wildcards_and_ports_are_accepted() {
        assert!(bypassed(".example.com", "https://example.com/"));
        assert!(bypassed("*.example.com", "https://git.example.com/"));
        assert!(bypassed("example.com:443", "https://git.example.com/"));
        assert!(bypassed("*", "https://anything.org/"));
        assert!(!bypassed("", "https://example.com/"));
    }

    #[test]
    fn the_proxy_keeps_its_address_and_credentials() {
        let proxy = Proxy::new("http://user:secret@proxy.internal:3128").unwrap();
        let proxy = with_no_proxy(proxy, None).unwrap();
        assert_eq!(proxy.host(), "proxy.internal");
        assert_eq!(proxy.port(), 3128);
        assert_eq!(proxy.username(), Some("user"));
        assert_eq!(proxy.password(), Some("secret"));
    }
}
