//! `manifest.toml`: what a directory extension is and which permissions it
//! needs.
//!
//! ```toml
//! description = "npm logins for sandboxes"
//! permissions = ["env", "filesystem", "sandbox", "secrets"]
//! ```
//!
//! `enabled = false` keeps an extension off until the user turns it on.
//!
//! `[[settings]]` tables declare what the user can configure on the
//! extension's page of the settings; the extension reads the values with
//! `term.settings()`:
//!
//! ```toml
//! [[settings]]
//! key = "files"
//! title = "Files to copy"
//! description = "Paths under your home directory."
//! type = "list"
//! default = [".gitconfig"]
//! ```
//!
//! `hosts` lists what `term.http` and blocklist downloads may reach. The
//! accounts of the extension's forges and the values of its `hosts`
//! settings are allowed as well; every other host is refused:
//!
//! ```toml
//! hosts = ["api.bitbucket.org"]
//!
//! [[settings]]
//! key = "mirrors"
//! title = "Mirrors"
//! type = "hosts"
//! ```
//!
//! A `urls` setting holds `https://` URLs; their hosts are allowed too.
//!
//! A `[logins]` table says that the extension passes logins of a tool into
//! sandboxes, and where it reads them. The Accounts page of the settings
//! lists it with the hosts its `term.secret` values go to:
//!
//! ```toml
//! [logins]
//! title = "npm"
//! sources = ["~/.npmrc", "NPM_TOKEN", "NODE_AUTH_TOKEN"]
//! ```
//!
//! An extension gets only the permissions its manifest declares and the
//! user granted. One without a manifest (a single `.lua` file, or a
//! directory without `manifest.toml`) gets none. Without a permission the
//! matching `cmdterm` functions raise an error, and the Lua libraries
//! behind `filesystem`, `process` and `env` are removed from its VM.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Something an extension may do beyond registering handlers and commands,
/// notifying and reading `term.context()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// `term.open_terminal` and `term.send_text`.
    Terminal,
    /// `term.open_browser`.
    Browser,
    /// `term.switch_worktree`.
    Worktree,
    /// `term.copy`.
    Copy,
    /// `term.run`.
    Run,
    /// `term.trust`: certificates for the trust store of an environment.
    Trust,
    /// `term.sandbox`: hosts, paths, variables and files for sandboxes.
    Sandbox,
    /// `term.secret`: real credentials the proxy adds to requests.
    Secrets,
    /// `term.sandbox{ guards = ... }`: requests that publish.
    Guards,
    /// `term.sandbox{ blocklists = ... }`: lists the app downloads from
    /// the extension's URLs.
    Network,
    /// `term.tool`: tools agents call through the control socket.
    Agents,
    /// `term.environment`: places projects live, reached through a program
    /// of this machine the extension names.
    Environments,
    /// `term.store`: data the extension keeps for the whole app.
    Storage,
    /// `term.project_store`: data the extension keeps per project.
    ProjectStorage,
    /// Lua's `io` library, `dofile`, `loadfile`, `os.remove`, `os.rename`
    /// and `os.tmpname`, and `require` beyond the extension's directory.
    Filesystem,
    /// `os.execute`, `io.popen`, `package.loadlib` and the `debug` library.
    Process,
    /// `os.getenv`.
    Env,
}

impl Permission {
    pub const ALL: [Self; 17] = [
        Self::Terminal,
        Self::Browser,
        Self::Worktree,
        Self::Copy,
        Self::Run,
        Self::Trust,
        Self::Sandbox,
        Self::Secrets,
        Self::Guards,
        Self::Network,
        Self::Agents,
        Self::Environments,
        Self::Storage,
        Self::ProjectStorage,
        Self::Filesystem,
        Self::Process,
        Self::Env,
    ];

    /// The name in `manifest.toml` and `settings.json`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Browser => "browser",
            Self::Worktree => "worktree",
            Self::Copy => "copy",
            Self::Run => "run",
            Self::Trust => "trust",
            Self::Sandbox => "sandbox",
            Self::Secrets => "secrets",
            Self::Guards => "guards",
            Self::Network => "network",
            Self::Agents => "agents",
            Self::Environments => "environments",
            Self::Storage => "storage",
            Self::ProjectStorage => "project_storage",
            Self::Filesystem => "filesystem",
            Self::Process => "process",
            Self::Env => "env",
        }
    }

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Permission {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|p| p.name() == name)
            .ok_or_else(|| {
                let known: Vec<&str> = Self::ALL.iter().map(|p| p.name()).collect();
                format!(
                    "unknown permission '{name}' (expected one of {})",
                    known.join(", ")
                )
            })
    }
}

/// A set of [`Permission`]s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Permissions(u32);

impl Permissions {
    pub const NONE: Self = Self(0);

    pub fn all() -> Self {
        Self::ALL_BITS
    }

    const ALL_BITS: Self = Self((1 << Permission::ALL.len()) - 1);

    pub const fn contains(self, permission: Permission) -> bool {
        self.0 & permission.bit() != 0
    }

    pub fn insert(&mut self, permission: Permission) {
        self.0 |= permission.bit();
    }

    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The permissions of `self` that `other` lacks.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Permission> {
        Permission::ALL
            .into_iter()
            .filter(move |p| self.contains(*p))
    }

    /// The names, in [`Permission::ALL`] order.
    pub fn names(self) -> Vec<String> {
        self.iter().map(|p| p.name().to_owned()).collect()
    }

    /// The known names of `names`; unknown ones are skipped, so settings
    /// written by a newer cmdterm still load.
    pub fn from_names<S: AsRef<str>>(names: &[S]) -> Self {
        let mut set = Self::NONE;
        for name in names {
            if let Ok(permission) = name.as_ref().parse() {
                set.insert(permission);
            }
        }
        set
    }
}

impl FromIterator<Permission> for Permissions {
    fn from_iter<I: IntoIterator<Item = Permission>>(iter: I) -> Self {
        let mut set = Self::NONE;
        for permission in iter {
            set.insert(permission);
        }
        set
    }
}

/// The value of an extension setting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    Bool(bool),
    Integer(i64),
    String(String),
    List(Vec<String>),
}

/// What kind of value a setting holds, and so how the settings show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// A switch.
    Bool,
    Integer,
    String,
    /// Strings, edited as one comma-separated line.
    List,
    /// Host names or `*.domain` patterns. `term.http` may reach every one
    /// the user lists, as if the manifest's `hosts` named it.
    Hosts,
    /// `https://` URLs. `term.http` and blocklists may reach their hosts.
    Urls,
    /// One of the listed strings.
    Choice(Vec<String>),
}

/// The host of an `https://` URL whose host is a valid host name.
pub fn https_host(url: &str) -> Option<String> {
    let host = crate::ext::url_host(url).filter(|_| url.starts_with("https://"))?;
    (cmdterm_env::valid_host_pattern(&host) && !host.starts_with("*.")).then_some(host)
}

impl SettingKind {
    /// Whether the settings page edits it as a list.
    pub fn is_list(&self) -> bool {
        matches!(self, Self::List | Self::Hosts | Self::Urls)
    }

    /// True when `value` is a value of this kind.
    pub fn fits(&self, value: &SettingValue) -> bool {
        match (self, value) {
            (Self::Bool, SettingValue::Bool(_))
            | (Self::Integer, SettingValue::Integer(_))
            | (Self::String, SettingValue::String(_))
            | (Self::List, SettingValue::List(_)) => true,
            (Self::Hosts, SettingValue::List(hosts)) => hosts
                .iter()
                .all(|host| cmdterm_env::valid_host_pattern(host)),
            (Self::Urls, SettingValue::List(urls)) => {
                urls.iter().all(|url| https_host(url).is_some())
            }
            (Self::Choice(choices), SettingValue::String(value)) => choices.contains(value),
            _ => false,
        }
    }
}

/// One `[[settings]]` table of a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingSpec {
    /// What `term.settings()` calls it: lowercase letters, digits and `_`.
    pub key: String,
    /// What the settings page shows; defaults to the key.
    pub title: String,
    pub description: Option<String>,
    pub kind: SettingKind,
    /// The value while the user set none: the manifest's `default`, else
    /// false, 0, "", an empty list or the first choice.
    pub default: SettingValue,
}

/// The `[logins]` table of a manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logins {
    /// The tool the logins belong to, such as `npm`.
    pub title: String,
    /// Where the extension reads them: files and variables.
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    settings: Vec<RawSetting>,
    #[serde(default)]
    logins: Option<Logins>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSetting {
    key: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    default: Option<SettingValue>,
    #[serde(default)]
    choices: Vec<String>,
}

impl RawSetting {
    fn parse(self) -> Result<SettingSpec, String> {
        let key = self.key;
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(format!(
                "setting '{key}': keys are lowercase letters, digits and '_'"
            ));
        }
        let kind = match self.kind.as_str() {
            "bool" => SettingKind::Bool,
            "integer" => SettingKind::Integer,
            "string" => SettingKind::String,
            "list" => SettingKind::List,
            "hosts" => SettingKind::Hosts,
            "urls" => SettingKind::Urls,
            "choice" if self.choices.is_empty() => {
                return Err(format!("setting '{key}': a choice needs 'choices'"));
            }
            "choice" => SettingKind::Choice(self.choices.clone()),
            other => {
                return Err(format!(
                    "setting '{key}': unknown type '{other}' (expected bool, integer, \
                     string, list, hosts, urls or choice)"
                ));
            }
        };
        let default = match self.default {
            Some(value) if kind.fits(&value) => value,
            Some(_) => {
                return Err(format!(
                    "setting '{key}': the default does not fit its type"
                ));
            }
            None => match &kind {
                SettingKind::Bool => SettingValue::Bool(false),
                SettingKind::Integer => SettingValue::Integer(0),
                SettingKind::String => SettingValue::String(String::new()),
                SettingKind::List | SettingKind::Hosts | SettingKind::Urls => {
                    SettingValue::List(Vec::new())
                }
                SettingKind::Choice(choices) => SettingValue::String(choices[0].clone()),
            },
        };
        Ok(SettingSpec {
            title: self.title.unwrap_or_else(|| key.clone()),
            key,
            description: self.description,
            kind,
            default,
        })
    }
}

/// A parsed `manifest.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// What the settings show in place of the extension's directory name.
    pub title: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    /// From `enabled`, true unless it says `false`: then the extension stays
    /// off until the user turns it on.
    pub enabled: bool,
    pub permissions: Permissions,
    /// From `hosts`: what `term.http` and blocklist downloads may reach,
    /// besides the accounts of the extension's forges and the values of
    /// its `hosts` settings.
    pub hosts: Vec<String>,
    pub settings: Vec<SettingSpec>,
    pub logins: Option<Logins>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            title: None,
            description: None,
            version: None,
            enabled: true,
            permissions: Permissions::NONE,
            hosts: Vec::new(),
            settings: Vec::new(),
            logins: None,
        }
    }
}

impl Manifest {
    /// Parses manifest text. Unknown keys and permissions are errors.
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        let mut permissions = Permissions::NONE;
        for name in &raw.permissions {
            permissions.insert(name.parse()?);
        }
        let mut settings: Vec<SettingSpec> = Vec::new();
        for raw in raw.settings {
            let spec = raw.parse()?;
            if settings.iter().any(|s| s.key == spec.key) {
                return Err(format!("setting '{}' is declared twice", spec.key));
            }
            settings.push(spec);
        }
        let mut hosts = Vec::new();
        for host in raw.hosts {
            let host = host.trim().to_ascii_lowercase();
            if !cmdterm_env::valid_host_pattern(&host) {
                return Err(format!("hosts: '{host}' is not a host name or *.domain"));
            }
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        Ok(Self {
            title: raw.title.filter(|title| !title.trim().is_empty()),
            description: raw.description,
            version: raw.version,
            enabled: raw.enabled.unwrap_or(true),
            permissions,
            hosts,
            settings,
            logins: raw.logins,
        })
    }

    /// The manifest of the extension in `dir`, `None` without one.
    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(dir.join("manifest.toml")) {
            Ok(text) => Self::parse(&text)
                .map(Some)
                .map_err(|e| format!("manifest.toml: {e}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("cannot read manifest.toml: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_declare_known_permissions_only() {
        let manifest = Manifest::parse(
            r#"description = "npm logins"
               version = "1.0"
               permissions = ["env", "secrets", "sandbox"]"#,
        )
        .unwrap();
        assert_eq!(manifest.description.as_deref(), Some("npm logins"));
        assert_eq!(manifest.permissions.names(), ["sandbox", "secrets", "env"]);
        assert!(
            Manifest::parse(r#"permissions = ["root"]"#)
                .unwrap_err()
                .contains("unknown permission 'root'")
        );
        assert!(Manifest::parse(r#"permision = ["env"]"#).is_err());
        let configured = Manifest::parse(
            r#"
            [[settings]]
            key = "files"
            title = "Files to copy"
            description = "Under your home."
            type = "list"
            default = [".gitconfig"]

            [[settings]]
            key = "mode"
            type = "choice"
            choices = ["fast", "safe"]

            [[settings]]
            key = "retries"
            type = "integer"
            default = 3
            "#,
        )
        .unwrap();
        let keys: Vec<&str> = configured.settings.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["files", "mode", "retries"]);
        assert_eq!(configured.settings[0].title, "Files to copy");
        assert_eq!(
            configured.settings[0].default,
            SettingValue::List(vec![".gitconfig".into()])
        );
        assert_eq!(configured.settings[1].title, "mode");
        assert_eq!(
            configured.settings[1].default,
            SettingValue::String("fast".into())
        );
        assert_eq!(configured.settings[2].default, SettingValue::Integer(3));
        assert!(
            configured.settings[1]
                .kind
                .fits(&SettingValue::String("safe".into()))
        );
        assert!(
            !configured.settings[1]
                .kind
                .fits(&SettingValue::String("other".into()))
        );
        for bad in [
            "[[settings]]\nkey = \"A\"\ntype = \"bool\"",
            "[[settings]]\nkey = \"a\"\ntype = \"date\"",
            "[[settings]]\nkey = \"a\"\ntype = \"bool\"\ndefault = 1",
            "[[settings]]\nkey = \"a\"\ntype = \"choice\"",
            "[[settings]]\nkey = \"a\"\ntype = \"bool\"\n[[settings]]\nkey = \"a\"\ntype = \"bool\"",
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Manifest::parse("").unwrap().permissions, Permissions::NONE);
        assert!(Manifest::parse("").unwrap().enabled);
        assert!(!Manifest::parse("enabled = false").unwrap().enabled);
        let granted = Permissions::from_names(&["env", "future-thing"]);
        assert_eq!(granted.names(), ["env"]);
        assert_eq!(
            manifest.permissions.without(granted).names(),
            ["sandbox", "secrets"]
        );
    }
}
