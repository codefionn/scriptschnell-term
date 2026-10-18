//! Remote URL parsing and URL component encoding.

use crate::forge::{ForgeHints, ForgeKind, ForgeRepo};

/// Host and repository path pulled out of a remote URL.
struct Remote<'a> {
    /// Host name without port or user, lowercase.
    hostname: String,
    /// `host:port` for http(s) URLs with an explicit port, else the hostname.
    /// The web UI and API live there.
    web_host: String,
    /// Path without leading or trailing slashes and without `.git`.
    path: &'a str,
}

fn split_remote(url: &str) -> Option<Remote<'_>> {
    let url = url.trim();
    let (hostname, port, path, web) = if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                (host, Some(port))
            }
            _ => (authority, None),
        };
        let web = matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https");
        (host, port, path, web)
    } else {
        // scp-like `[user@]host:path`. A slash before the colon means a
        // local path.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host, None, path, false)
    };
    if hostname.is_empty() {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    let hostname = hostname.to_ascii_lowercase();
    let web_host = match port {
        Some(port) if web => format!("{hostname}:{port}"),
        _ => hostname.clone(),
    };
    Some(Remote {
        hostname,
        web_host,
        path,
    })
}

pub(crate) fn split(url: &str) -> Option<(String, String)> {
    let remote = split_remote(url)?;
    Some((remote.hostname, remote.path.to_owned()))
}

pub(crate) fn detect(remote_url: &str, hints: &ForgeHints) -> Option<ForgeRepo> {
    let remote = split_remote(remote_url)?;
    let hinted = hints.hosts.iter().find_map(|(host, kind)| {
        (host.eq_ignore_ascii_case(&remote.hostname) || host.eq_ignore_ascii_case(&remote.web_host))
            .then_some(*kind)
    });
    let host = remote.hostname.as_str();
    let (kind, host, path) = match hinted {
        Some(kind) => {
            let path = match kind {
                ForgeKind::GitLab => gitlab_path(remote.path)?,
                ForgeKind::Forgejo => owner_repo(remote.path)?,
                ForgeKind::AzureDevOps => {
                    azure_git_path(None, remote.path).or_else(|| azure_v3_path(remote.path))?
                }
            };
            (kind, remote.web_host, path)
        }
        None if host == "gitlab.com" => (
            ForgeKind::GitLab,
            remote.web_host,
            gitlab_path(remote.path)?,
        ),
        None if host == "codeberg.org" => (
            ForgeKind::Forgejo,
            remote.web_host,
            owner_repo(remote.path)?,
        ),
        None if host == "dev.azure.com" => (
            ForgeKind::AzureDevOps,
            "dev.azure.com".to_owned(),
            azure_git_path(None, remote.path)?,
        ),
        None if host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" => (
            ForgeKind::AzureDevOps,
            "dev.azure.com".to_owned(),
            azure_v3_path(remote.path)?,
        ),
        None => {
            let org = host.strip_suffix(".visualstudio.com")?;
            if org.is_empty() || org.contains('.') {
                return None;
            }
            (
                ForgeKind::AzureDevOps,
                "dev.azure.com".to_owned(),
                azure_git_path(Some(org), remote.path)?,
            )
        }
    };
    Some(ForgeRepo { kind, host, path })
}

/// GitLab namespaces nest: keep the whole path, minus any `/-/...` suffix.
fn gitlab_path(path: &str) -> Option<String> {
    let path = path.split_once("/-/").map_or(path, |(head, _)| head);
    let decoded = decode(path);
    (decoded.split('/').filter(|s| !s.is_empty()).count() >= 2).then_some(decoded)
}

fn owner_repo(path: &str) -> Option<String> {
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    Some(format!("{}/{}", decode(owner), decode(repo)))
}

/// `[org/]...project/_git/repo` → `org/project/repo`. Without a host-given
/// org, every segment before the project is the org (on-premises servers use
/// `tfs/Collection`).
fn azure_git_path(org: Option<&str>, path: &str) -> Option<String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let git = segments.iter().position(|&s| s == "_git")?;
    let repo = segments.get(git + 1)?;
    let project = segments[..git].last()?;
    let org = match org {
        Some(org) => org.to_owned(),
        None => {
            let prefix = &segments[..git - 1];
            if prefix.is_empty() {
                return None;
            }
            prefix.join("/")
        }
    };
    Some(format!(
        "{}/{}/{}",
        decode(&org),
        decode(project),
        decode(repo)
    ))
}

/// SSH form `v3/org/project/repo`.
fn azure_v3_path(path: &str) -> Option<String> {
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    if segments.next()? != "v3" {
        return None;
    }
    let (org, project, repo) = (segments.next()?, segments.next()?, segments.next()?);
    Some(format!(
        "{}/{}/{}",
        decode(org),
        decode(project),
        decode(repo)
    ))
}

/// Percent-decodes `%XX` escapes; leaves malformed escapes as they are.
fn decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Appends `input` percent-encoded as one URL component (`/` included).
pub(crate) fn push_encoded(out: &mut String, input: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 15)] as char);
        }
    }
}

/// Appends each `/`-separated segment encoded, keeping the slashes.
pub(crate) fn push_encoded_path(out: &mut String, path: &str) {
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        push_encoded(out, segment);
    }
}

pub(crate) fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(kind: ForgeKind, host: &str, path: &str) -> Option<ForgeRepo> {
        Some(ForgeRepo {
            kind,
            host: host.to_owned(),
            path: path.to_owned(),
        })
    }

    #[test]
    fn detects_known_hosts() {
        use ForgeKind::*;
        let none = ForgeHints::default();
        let cases: &[(&str, Option<ForgeRepo>)] = &[
            (
                "https://gitlab.com/group/sub/repo.git",
                repo(GitLab, "gitlab.com", "group/sub/repo"),
            ),
            (
                "https://gitlab.com/group/repo",
                repo(GitLab, "gitlab.com", "group/repo"),
            ),
            (
                "git@gitlab.com:group/sub/repo.git",
                repo(GitLab, "gitlab.com", "group/sub/repo"),
            ),
            (
                "ssh://git@gitlab.com:22/group/repo.git",
                repo(GitLab, "gitlab.com", "group/repo"),
            ),
            (
                "https://oauth2:tok@GitLab.com/g/r.git/",
                repo(GitLab, "gitlab.com", "g/r"),
            ),
            ("https://gitlab.com/onlyone", None),
            (
                "https://codeberg.org/owner/repo.git",
                repo(Forgejo, "codeberg.org", "owner/repo"),
            ),
            (
                "git@codeberg.org:owner/repo.git",
                repo(Forgejo, "codeberg.org", "owner/repo"),
            ),
            (
                "https://org@dev.azure.com/org/My%20Project/_git/repo",
                repo(AzureDevOps, "dev.azure.com", "org/My Project/repo"),
            ),
            (
                "git@ssh.dev.azure.com:v3/org/proj/repo",
                repo(AzureDevOps, "dev.azure.com", "org/proj/repo"),
            ),
            (
                "ssh://git@ssh.dev.azure.com/v3/org/proj/repo",
                repo(AzureDevOps, "dev.azure.com", "org/proj/repo"),
            ),
            (
                "https://org.visualstudio.com/proj/_git/repo",
                repo(AzureDevOps, "dev.azure.com", "org/proj/repo"),
            ),
            (
                "https://org.visualstudio.com/DefaultCollection/proj/_git/repo",
                repo(AzureDevOps, "dev.azure.com", "org/proj/repo"),
            ),
            (
                "org@vs-ssh.visualstudio.com:v3/org/proj/repo",
                repo(AzureDevOps, "dev.azure.com", "org/proj/repo"),
            ),
            ("https://github.com/o/r.git", None),
            ("git@git.example.com:o/r.git", None),
            ("/srv/git/repo.git", None),
            ("./relative/path", None),
            ("", None),
        ];
        for (url, expected) in cases {
            assert_eq!(&detect(url, &none), expected, "{url}");
        }
    }

    #[test]
    fn detects_hinted_hosts() {
        use ForgeKind::*;
        let hints = ForgeHints {
            hosts: vec![
                ("git.example.com".into(), Forgejo),
                ("gl.corp".into(), GitLab),
                ("tfs.corp".into(), AzureDevOps),
            ],
        };
        let cases: &[(&str, Option<ForgeRepo>)] = &[
            (
                "git@git.example.com:o/r.git",
                repo(Forgejo, "git.example.com", "o/r"),
            ),
            (
                "https://git.example.com:3000/o/r",
                repo(Forgejo, "git.example.com:3000", "o/r"),
            ),
            (
                "ssh://git@git.example.com:2222/o/r.git",
                repo(Forgejo, "git.example.com", "o/r"),
            ),
            (
                "https://gl.corp/a/b/c.git",
                repo(GitLab, "gl.corp", "a/b/c"),
            ),
            (
                "https://tfs.corp/tfs/Coll/Proj/_git/Repo",
                repo(AzureDevOps, "tfs.corp", "tfs/Coll/Proj/Repo"),
            ),
            ("https://unknown.corp/o/r", None),
        ];
        for (url, expected) in cases {
            assert_eq!(&detect(url, &hints), expected, "{url}");
        }
    }

    #[test]
    fn encodes_components() {
        let mut out = String::new();
        push_encoded(&mut out, "group/sub repo");
        assert_eq!(out, "group%2Fsub%20repo");
        out.clear();
        push_encoded_path(&mut out, "a b/c");
        assert_eq!(out, "a%20b/c");
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
            (":pat", "OnBhdA=="),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }
}
