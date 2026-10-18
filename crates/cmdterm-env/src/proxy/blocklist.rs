//! Hosts the proxy refuses even when the allowed hosts or the whole
//! network let them through: downloaded lists of ad, tracker and malware
//! domains, and single hosts extensions name.
//!
//! A [`Blocklist`] keeps a sorted hash per domain instead of the names, so
//! a list of 200,000 domains costs about 1.6 MB. A host is blocked when it
//! or one of its parent domains is listed.

use std::fmt;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::net::IpAddr;
use std::sync::Arc;

use super::{host_allowed, normalize};

/// One list of domains. A domain blocks its subdomains too.
#[derive(Clone, PartialEq, Eq)]
pub struct Blocklist {
    /// What refusals name, such as `blocklist.hagezi-pro`.
    name: String,
    /// The hashes of the domains, sorted and without duplicates.
    hashes: Vec<u64>,
}

impl fmt::Debug for Blocklist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Blocklist")
            .field("name", &self.name)
            .field("domains", &self.hashes.len())
            .finish()
    }
}

impl Blocklist {
    /// Reads a list in any of the common formats, one entry per line:
    /// plain domains, hosts files (`0.0.0.0 ads.example`), wildcards
    /// (`*.ads.example`) and Adblock domain rules (`||ads.example^`).
    /// Comments (`#`, `!`), exceptions (`@@`), rules with options (`$`)
    /// and lines that are no domain are skipped.
    pub fn parse(name: impl Into<String>, text: &str) -> Self {
        let mut hashes = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() || line.starts_with('!') || line.starts_with("@@") {
                continue;
            }
            if let Some(rule) = line.strip_prefix("||") {
                if let Some(domain) = rule.strip_suffix('^').filter(|d| !d.contains('$')) {
                    push_domain(&mut hashes, domain);
                }
                continue;
            }
            let mut words = line.split_whitespace();
            let first = words.next().unwrap_or_default();
            if first.parse::<IpAddr>().is_ok() {
                for domain in words {
                    push_domain(&mut hashes, domain);
                }
            } else if words.next().is_none() {
                push_domain(&mut hashes, first.strip_prefix("*.").unwrap_or(first));
            }
        }
        hashes.sort_unstable();
        hashes.dedup();
        Self {
            name: name.into(),
            hashes,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many domains it lists.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// True when `host` or one of its parent domains is listed. `host` is
    /// normalized already.
    fn contains(&self, host: &str) -> bool {
        let mut rest = host;
        loop {
            if self.hashes.binary_search(&hash(rest)).is_ok() {
                return true;
            }
            match rest.split_once('.') {
                Some((_, parent)) if !parent.is_empty() => rest = parent,
                _ => return false,
            }
        }
    }
}

/// Names that hosts files map to an address without blocking anything.
const NOT_BLOCKED: [&str; 5] = [
    "localhost",
    "localhost.localdomain",
    "local",
    "broadcasthost",
    "0.0.0.0",
];

fn push_domain(hashes: &mut Vec<u64>, domain: &str) {
    let domain = normalize(domain);
    let valid = !domain.is_empty()
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !domain.starts_with('.')
        && !NOT_BLOCKED.contains(&domain.as_str())
        && !domain.starts_with("ip6-");
    if valid {
        hashes.push(hash(&domain));
    }
}

fn hash(domain: &str) -> u64 {
    BuildHasherDefault::<DefaultHasher>::default().hash_one(domain)
}

/// Everything the proxy refuses besides hosts that are not allowed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blocking {
    pub lists: Vec<Arc<Blocklist>>,
    /// Hosts to refuse. `ads.example` blocks it and its subdomains,
    /// `*.ads.example` only the subdomains.
    pub hosts: Vec<String>,
    /// Hosts no block applies to, in the same form as [`Self::hosts`].
    pub exceptions: Vec<String>,
}

impl Blocking {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.lists.iter().all(|list| list.is_empty())
    }

    /// What blocks `host`: the name of a list, or the host pattern.
    pub fn blocked_by(&self, host: &str) -> Option<&str> {
        let host = normalize(host);
        if host.parse::<IpAddr>().is_ok() || domain_matches(&self.exceptions, &host) {
            return None;
        }
        if let Some(pattern) = self
            .hosts
            .iter()
            .find(|pattern| domain_matches(std::slice::from_ref(pattern), &host))
        {
            return Some(pattern);
        }
        self.lists
            .iter()
            .find(|list| list.contains(&host))
            .map(|list| list.name())
    }
}

/// True when one of `patterns` matches `host`: `*.example.com` as in the
/// allowed hosts, a plain name also every subdomain.
fn domain_matches(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = normalize(pattern);
        if pattern.starts_with("*.") {
            return host_allowed(std::slice::from_ref(&pattern), host);
        }
        host.strip_suffix(pattern.as_str())
            .is_some_and(|sub| sub.is_empty() || sub.ends_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
# Title: test
! Adblock comment
ads.example
0.0.0.0 tracker.example metrics.example # trailing comment
127.0.0.1 localhost
*.wild.example
||adblock.example^
||options.example^$third-party
@@||allowed.example^
not a domain
";

    #[test]
    fn lists_read_the_common_formats() {
        let list = Blocklist::parse("test", LIST);
        for host in [
            "ads.example",
            "a.b.ads.example",
            "tracker.example",
            "metrics.example",
            "wild.example",
            "x.wild.example",
            "adblock.example",
            "ADS.Example.",
        ] {
            assert!(list.contains(&normalize(host)), "{host}");
        }
        for host in [
            "example",
            "localhost",
            "options.example",
            "allowed.example",
            "badads.example",
        ] {
            assert!(!list.contains(host), "{host}");
        }
        assert_eq!(list.len(), 5);
    }

    #[test]
    fn exceptions_win_over_lists_and_hosts() {
        let blocking = Blocking {
            lists: vec![Arc::new(Blocklist::parse("test", LIST))],
            hosts: vec!["evil.example".into(), "*.sub.example".into()],
            exceptions: vec!["ok.ads.example".into()],
        };
        assert_eq!(blocking.blocked_by("cdn.ads.example"), Some("test"));
        assert_eq!(blocking.blocked_by("ok.ads.example"), None);
        assert_eq!(blocking.blocked_by("deeper.ok.ads.example"), None);
        assert_eq!(blocking.blocked_by("a.evil.example"), Some("evil.example"));
        assert_eq!(blocking.blocked_by("x.sub.example"), Some("*.sub.example"));
        assert_eq!(blocking.blocked_by("sub.example"), None);
        assert_eq!(blocking.blocked_by("crates.io"), None);
        assert_eq!(blocking.blocked_by("[::1]"), None);
        assert!(!blocking.is_empty());
        assert!(Blocking::default().is_empty());
    }
}
