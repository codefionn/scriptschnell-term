//! Environments that extensions define: a scheme such as `coder` and the
//! program that runs a command in one of its targets, such as
//! `coder ssh {target} -- {script}`. A project at `coder://ws/~/src` is
//! [`Environment::Custom`] with scheme `coder` and target `ws`.
//!
//! The project only stores the scheme and the target. The program comes
//! from the registry below each time a command is built, so an extension
//! that is turned off, or loses the `environments` permission, takes its
//! environments with it: their commands then fail with a message that
//! names the missing scheme instead of running anything.
//!
//! [`Environment::Custom`]: crate::Environment::Custom

use std::ffi::OsString;
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

/// A target of an extension's environment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CustomTarget {
    /// The scheme of the location, such as `coder`.
    pub scheme: String,
    /// What the location names after the scheme, such as a workspace.
    pub target: String,
}

/// How an extension's environment runs commands, from `term.environment`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomProvider {
    pub scheme: String,
    pub title: String,
    /// The extension that registered it.
    pub extension: String,
    /// The program on this machine that reaches a target.
    pub program: String,
    /// Arguments for commands without a terminal. `{target}` anywhere in an
    /// argument becomes the target; an argument that is exactly `{script}`
    /// becomes the POSIX shell script to run there. Without one the script
    /// goes last.
    pub args: Vec<String>,
    /// Arguments for commands in a terminal, the same way; `None` uses
    /// [`Self::args`].
    pub pty_args: Option<Vec<String>>,
}

/// Placeholder for the target in [`CustomProvider::args`].
pub const TARGET_PLACEHOLDER: &str = "{target}";
/// Placeholder for the script in [`CustomProvider::args`].
pub const SCRIPT_PLACEHOLDER: &str = "{script}";

/// Schemes the app parses itself, which extensions cannot take.
const RESERVED_SCHEMES: [&str; 12] = [
    "cmdterm",
    "file",
    "git",
    "http",
    "https",
    "local",
    "nebula",
    "netbird",
    "ssh",
    "tailscale",
    "vm",
    "wsl",
];

/// Whether `scheme` can name an extension's environment: a lowercase
/// letter, then up to 31 lowercase letters, digits and `-`, and not a
/// scheme the app parses itself.
pub fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && scheme.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !RESERVED_SCHEMES.contains(&scheme)
}

/// Whether `target` can be put into a provider's arguments: letters,
/// digits and `._@:+~-`, not starting with `-`, so a location can never
/// pass an option to the provider's program.
pub fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 255
        && !target.starts_with('-')
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@:+~-".contains(c))
}

/// Whether `args` uses the placeholders right: `{script}` at most once and
/// only as a whole argument.
pub fn valid_template(args: &[String]) -> bool {
    let whole = args.iter().filter(|a| *a == SCRIPT_PLACEHOLDER).count();
    let inside = args
        .iter()
        .filter(|a| a.contains(SCRIPT_PLACEHOLDER))
        .count();
    whole <= 1 && whole == inside
}

/// Every registered provider, with the id of whoever registered it.
static PROVIDERS: RwLock<Vec<(u64, Arc<CustomProvider>)>> = RwLock::new(Vec::new());

/// Replaces the providers `owner` registered with `providers`. An app has
/// one owner per extension host; a scheme another owner holds already
/// stays with that owner.
pub fn set_custom_environments(owner: u64, providers: Vec<CustomProvider>) {
    let mut list = PROVIDERS.write().unwrap_or_else(PoisonError::into_inner);
    list.retain(|(id, _)| *id != owner);
    for provider in providers {
        list.push((owner, Arc::new(provider)));
    }
}

/// The provider of `scheme`, if an extension registered one.
pub fn custom_environment(scheme: &str) -> Option<Arc<CustomProvider>> {
    let list = PROVIDERS.read().unwrap_or_else(PoisonError::into_inner);
    list.iter()
        .find(|(_, provider)| provider.scheme == scheme)
        .map(|(_, provider)| Arc::clone(provider))
}

impl CustomTarget {
    /// The program and arguments that run `script` in this target, or
    /// `None` when no extension provides the scheme.
    pub(crate) fn argv(&self, script: OsString, pty: bool) -> Option<(OsString, Vec<OsString>)> {
        let provider = custom_environment(&self.scheme)?;
        let template = match (&provider.pty_args, pty) {
            (Some(args), true) => args,
            _ => &provider.args,
        };
        let mut args = Vec::with_capacity(template.len() + 1);
        let mut script = Some(script);
        for arg in template {
            if arg == SCRIPT_PLACEHOLDER {
                args.push(script.take().unwrap_or_default());
            } else {
                args.push(arg.replace(TARGET_PLACEHOLDER, &self.target).into());
            }
        }
        if let Some(script) = script {
            args.push(script);
        }
        Some((provider.program.clone().into(), args))
    }

    /// A local command that prints why this target cannot run anything and
    /// fails, for when no extension provides its scheme.
    pub(crate) fn missing(&self) -> (OsString, Vec<OsString>) {
        let message = format!(
            "no extension provides {}:// environments; turn on the extension that does",
            self.scheme
        );
        if cfg!(windows) {
            let script = format!("echo {message} 1>&2 & exit /b 127");
            ("cmd.exe".into(), vec!["/C".into(), script.into()])
        } else {
            let script = format!("echo {} >&2; exit 127", shell_words::quote(&message));
            ("/bin/sh".into(), vec!["-c".into(), script.into()])
        }
    }

    /// [`Self::argv`], else [`Self::missing`].
    pub(crate) fn argv_or_missing(&self, script: OsString, pty: bool) -> (OsString, Vec<OsString>) {
        self.argv(script, pty).unwrap_or_else(|| self.missing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;

    fn provider(scheme: &str) -> CustomProvider {
        CustomProvider {
            scheme: scheme.into(),
            title: "Test".into(),
            extension: "test".into(),
            program: "tool".into(),
            args: vec!["ssh".into(), "--name={target}".into(), "--".into()],
            pty_args: Some(vec![
                "ssh".into(),
                "-t".into(),
                "{target}".into(),
                "sh".into(),
                "-c".into(),
                "{script}".into(),
            ]),
        }
    }

    fn argv(command: &std::process::Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn names_and_templates_are_checked() {
        assert!(valid_scheme("coder"));
        assert!(valid_scheme("k8s-pod"));
        assert!(!valid_scheme("ssh"));
        assert!(!valid_scheme("Coder"));
        assert!(!valid_scheme("1x"));
        assert!(!valid_scheme(""));
        assert!(valid_target("me@ws.main"));
        assert!(!valid_target("-oProxyCommand=x"));
        assert!(!valid_target("a b"));
        assert!(!valid_target("a/b"));
        assert!(!valid_target(""));
        assert!(valid_template(&["{script}".into()]));
        assert!(valid_template(&[]));
        assert!(!valid_template(&["{script}".into(), "{script}".into()]));
        assert!(!valid_template(&["-c={script}".into()]));
    }

    #[test]
    fn commands_run_through_the_provider_while_it_is_registered() {
        let owner = u64::MAX - 1;
        set_custom_environments(owner, vec![provider("custom-test-a")]);
        let env = Environment::Custom(CustomTarget {
            scheme: "custom-test-a".into(),
            target: "ws".into(),
        });
        assert_eq!(env.label(), "ws");
        let command = env.command("git").arg("status").cwd("/srv/a").var("K", "v");
        assert_eq!(
            argv(&command.to_std()),
            [
                "tool",
                "ssh",
                "--name=ws",
                "--",
                "cd /srv/a && K=v exec git status"
            ]
        );
        let pty = command.to_pty();
        let pty: Vec<_> = (pty.get_argv().iter())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            pty,
            [
                "tool",
                "ssh",
                "-t",
                "ws",
                "sh",
                "-c",
                "cd /srv/a && K=v exec git status"
            ]
        );
        let shell = env.shell(Some(std::path::Path::new("/srv/a")));
        assert_eq!(
            shell.get_argv().last().unwrap(),
            "cd /srv/a && \"${SHELL:-/bin/sh}\" -l"
        );
        assert!(!env.keeps_sessions());
        assert_eq!(env.forwarded_socket(std::path::Path::new("/s"), "x"), None);

        set_custom_environments(owner, Vec::new());
        let error = env.command("true").stdout().unwrap_err().to_string();
        assert!(
            error.contains("no extension provides custom-test-a:// environments"),
            "{error}"
        );
    }

    #[test]
    fn a_scheme_stays_with_its_first_owner() {
        set_custom_environments(u64::MAX - 2, vec![provider("custom-test-b")]);
        let mut other = provider("custom-test-b");
        other.program = "other".into();
        set_custom_environments(u64::MAX - 3, vec![other]);
        assert_eq!(custom_environment("custom-test-b").unwrap().program, "tool");
        set_custom_environments(u64::MAX - 2, Vec::new());
        assert_eq!(
            custom_environment("custom-test-b").unwrap().program,
            "other"
        );
        set_custom_environments(u64::MAX - 3, Vec::new());
        assert_eq!(custom_environment("custom-test-b"), None);
    }

    #[test]
    fn custom_targets_round_trip_as_json() {
        let env = Environment::Custom(CustomTarget {
            scheme: "coder".into(),
            target: "ws".into(),
        });
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"kind":"custom","scheme":"coder","target":"ws"}"#);
        assert_eq!(serde_json::from_str::<Environment>(&json).unwrap(), env);
    }
}
