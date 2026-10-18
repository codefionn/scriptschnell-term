//! devcontainer.json: the keys the built-in support reads, defaults, and
//! variable substitution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::container::jsonc;

/// A parsed devcontainer.json with variables substituted. Keys cmdterm does
/// not read are ignored.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub name: Option<String>,
    pub image: Option<String>,
    pub build: Option<Build>,
    /// Legacy spelling of `build.dockerfile`.
    #[serde(rename = "dockerFile")]
    pub docker_file: Option<String>,
    /// Legacy spelling of `build.context`.
    pub context: Option<String>,
    /// Where the worktree appears inside the container. Always set after
    /// parsing: the default is `/workspaces/<worktree basename>`.
    pub workspace_folder: String,
    /// A `--mount` value for the worktree. `None` means a bind mount of the
    /// worktree on `workspace_folder`.
    pub workspace_mount: Option<String>,
    pub container_env: BTreeMap<String, String>,
    /// `None` values unset a variable; they are dropped.
    pub remote_env: BTreeMap<String, Option<String>>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    pub mounts: Vec<Mount>,
    pub run_args: Vec<String>,
    /// Defaults to true: the container runs a sleep loop instead of the
    /// image's command.
    pub override_command: Option<bool>,
    pub init: Option<bool>,
    pub privileged: Option<bool>,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub on_create_command: Option<Lifecycle>,
    pub update_content_command: Option<Lifecycle>,
    pub post_create_command: Option<Lifecycle>,
    pub post_start_command: Option<Lifecycle>,
    /// Only the devcontainer CLI handles compose setups.
    pub docker_compose_file: Option<Value>,
    /// Only the devcontainer CLI handles features.
    pub features: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Build {
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    pub args: BTreeMap<String, String>,
    pub target: Option<String>,
}

/// An entry of `mounts`: a `--mount` string or an object.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Mount {
    Spec(String),
    Object {
        #[serde(default)]
        source: Option<String>,
        target: String,
        #[serde(default, rename = "type")]
        kind: Option<String>,
    },
}

impl Mount {
    /// The value for `--mount`. Objects without a type are bind mounts.
    pub fn to_arg(&self) -> String {
        match self {
            Self::Spec(spec) => spec.clone(),
            Self::Object {
                source,
                target,
                kind,
            } => {
                let mut arg = format!("type={}", kind.as_deref().unwrap_or("bind"));
                if let Some(source) = source {
                    arg.push_str(",source=");
                    arg.push_str(source);
                }
                arg.push_str(",target=");
                arg.push_str(target);
                arg
            }
        }
    }
}

/// A lifecycle command such as `postCreateCommand`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Lifecycle {
    /// Run with `sh -c`.
    Shell(String),
    /// Run directly, without a shell.
    Exec(Vec<String>),
    /// Several named commands, run one after another in name order.
    Named(BTreeMap<String, Step>),
}

/// One named command inside [`Lifecycle::Named`].
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Shell(String),
    Exec(Vec<String>),
}

impl Step {
    pub fn argv(&self) -> Vec<String> {
        match self {
            Self::Shell(script) => vec!["sh".into(), "-c".into(), script.clone()],
            Self::Exec(argv) => argv.clone(),
        }
    }
}

impl Lifecycle {
    /// The commands to run, each with its name when it has one.
    pub fn commands(&self) -> Vec<(Option<&str>, Vec<String>)> {
        match self {
            Self::Shell(script) => vec![(None, Step::Shell(script.clone()).argv())],
            Self::Exec(argv) => vec![(None, argv.clone())],
            Self::Named(steps) => steps
                .iter()
                .map(|(name, step)| (Some(name.as_str()), step.argv()))
                .collect(),
        }
    }
}

/// Where the image comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageSource {
    Pull(String),
    Build {
        dockerfile: PathBuf,
        context: PathBuf,
        args: BTreeMap<String, String>,
        target: Option<String>,
    },
}

impl Config {
    /// Parses devcontainer.json text for `worktree`, reading `${localEnv:..}`
    /// from this process's environment.
    pub fn parse(text: &str, worktree: &Path, devcontainer_id: &str) -> Result<Self, String> {
        Self::parse_with(text, worktree, devcontainer_id, &|name| {
            std::env::var(name).ok()
        })
    }

    /// Like [`Config::parse`], with `env` answering `${localEnv:..}`.
    pub fn parse_with(
        text: &str,
        worktree: &Path,
        devcontainer_id: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut value: Value =
            serde_json::from_str(&jsonc::strip(text)).map_err(|e| e.to_string())?;
        if !value.is_object() {
            return Err("the top level is not an object".into());
        }
        let mut vars = Vars {
            local_folder: worktree,
            container_folder: None,
            devcontainer_id,
            env,
        };
        let folder = match value.get("workspaceFolder").and_then(Value::as_str) {
            Some(folder) => substitute(folder, &vars),
            None => format!("/workspaces/{}", basename(worktree)),
        };
        vars.container_folder = Some(&folder);
        substitute_all(&mut value, &vars);
        let mut config: Config = serde_json::from_value(value).map_err(|e| e.to_string())?;
        config.workspace_folder = folder.clone();
        Ok(config)
    }

    /// Keys present that only the devcontainer CLI supports.
    pub fn cli_only_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.docker_compose_file.is_some() {
            keys.push("dockerComposeFile");
        }
        if self.features.as_ref().is_some_and(|f| match f {
            Value::Object(map) => !map.is_empty(),
            Value::Null => false,
            _ => true,
        }) {
            keys.push("features");
        }
        keys
    }

    /// The image to run, or `None` when the config names neither an image
    /// nor a Dockerfile. Build paths are relative to `config_dir`, the
    /// directory of devcontainer.json.
    pub fn image_source(&self, config_dir: &Path) -> Option<ImageSource> {
        let build = self.build.clone().unwrap_or_default();
        let dockerfile = build.dockerfile.or_else(|| self.docker_file.clone());
        match dockerfile {
            Some(dockerfile) => {
                let context = build
                    .context
                    .or_else(|| self.context.clone())
                    .unwrap_or_else(|| ".".into());
                Some(ImageSource::Build {
                    dockerfile: config_dir.join(dockerfile),
                    context: config_dir.join(context),
                    args: build.args,
                    target: build.target,
                })
            }
            None => self.image.clone().map(ImageSource::Pull),
        }
    }

    /// The `--mount` value for the worktree.
    pub fn workspace_mount_arg(&self, worktree: &Path) -> String {
        match &self.workspace_mount {
            Some(mount) => mount.clone(),
            None => format!(
                "type=bind,source={},target={}",
                worktree.display(),
                self.workspace_folder
            ),
        }
    }

    /// The user commands run as: `remoteUser`, else `containerUser`.
    pub fn user(&self) -> Option<&str> {
        self.remote_user
            .as_deref()
            .or(self.container_user.as_deref())
    }
}

pub(crate) fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Values for `${..}` references.
pub(crate) struct Vars<'a> {
    pub local_folder: &'a Path,
    /// `None` while `workspaceFolder` itself is resolved.
    pub container_folder: Option<&'a str>,
    pub devcontainer_id: &'a str,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

impl Vars<'_> {
    fn lookup(&self, name: &str) -> Option<String> {
        match name {
            "localWorkspaceFolder" => Some(self.local_folder.display().to_string()),
            "localWorkspaceFolderBasename" => Some(basename(self.local_folder)),
            "containerWorkspaceFolder" => self.container_folder.map(str::to_owned),
            "containerWorkspaceFolderBasename" => {
                self.container_folder.map(|f| basename(Path::new(f)))
            }
            "devcontainerId" => Some(self.devcontainer_id.to_owned()),
            _ => {
                let rest = name
                    .strip_prefix("localEnv:")
                    .or_else(|| name.strip_prefix("env:"))?;
                let (var, default) = match rest.split_once(':') {
                    Some((var, default)) => (var, default),
                    None => (rest, ""),
                };
                Some((self.env)(var).unwrap_or_else(|| default.to_owned()))
            }
        }
    }
}

/// `text` with every known `${..}` reference replaced. Unknown references,
/// such as `${containerEnv:PATH}`, stay as they are.
pub(crate) fn substitute(text: &str, vars: &Vars) -> String {
    substitute_by(text, &|name| vars.lookup(name))
}

/// `text` with `${name}` replaced by `lookup(name)` where it answers.
pub(crate) fn substitute_by(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match lookup(name) {
            Some(value) => out.push_str(&value),
            None => {
                out.push_str("${");
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn substitute_all(value: &mut Value, vars: &Vars) {
    match value {
        Value::String(text) if text.contains("${") => *text = substitute(text, vars),
        Value::Array(items) => items.iter_mut().for_each(|v| substitute_all(v, vars)),
        Value::Object(map) => map.values_mut().for_each(|v| substitute_all(v, vars)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "HOME" => Some("/home/me".into()),
            _ => None,
        }
    }

    fn parse(text: &str) -> Config {
        Config::parse_with(text, Path::new("/src/my app"), "abc123", &env).unwrap()
    }

    #[test]
    fn defaults() {
        let config = parse(r#"{"image": "alpine"}"#);
        assert_eq!(config.workspace_folder, "/workspaces/my app");
        assert_eq!(
            config.workspace_mount_arg(Path::new("/src/my app")),
            "type=bind,source=/src/my app,target=/workspaces/my app"
        );
        assert_eq!(config.override_command, None);
        assert_eq!(
            config.image_source(Path::new("/src/my app/.devcontainer")),
            Some(ImageSource::Pull("alpine".into()))
        );
        assert!(config.cli_only_keys().is_empty());
    }

    #[test]
    fn reads_every_supported_key() {
        let config = parse(
            r#"{
            // comment
            "name": "dev",
            "build": {"dockerfile": "Dockerfile", "context": "..", "args": {"V": "1"}, "target": "dev"},
            "workspaceFolder": "/work",
            "containerEnv": {"A": "1"},
            "remoteEnv": {"B": "2", "C": null},
            "containerUser": "root",
            "remoteUser": "vscode",
            "mounts": ["type=volume,source=cache,target=/cache", {"source": "/tmp", "target": "/host-tmp"}],
            "runArgs": ["--network=host"],
            "overrideCommand": false,
            "init": true,
            "privileged": true,
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "onCreateCommand": "echo one",
            "updateContentCommand": ["echo", "two"],
            "postCreateCommand": {"b": "echo b", "a": ["echo", "a"]},
            "postStartCommand": "echo start",
            "postAttachCommand": "ignored",
            "forwardPorts": [3000],
        }"#,
        );
        assert_eq!(config.name.as_deref(), Some("dev"));
        assert_eq!(config.workspace_folder, "/work");
        assert_eq!(config.user(), Some("vscode"));
        assert_eq!(config.remote_env.get("C"), Some(&None));
        assert_eq!(
            config.mounts[1].to_arg(),
            "type=bind,source=/tmp,target=/host-tmp"
        );
        assert_eq!(
            config.image_source(Path::new("/src/my app/.devcontainer")),
            Some(ImageSource::Build {
                dockerfile: "/src/my app/.devcontainer/Dockerfile".into(),
                context: "/src/my app/.devcontainer/..".into(),
                args: [("V".to_owned(), "1".to_owned())].into(),
                target: Some("dev".into()),
            })
        );
        let post_create = config.post_create_command.as_ref().unwrap().commands();
        assert_eq!(
            post_create,
            [
                (Some("a"), vec!["echo".to_owned(), "a".to_owned()]),
                (
                    Some("b"),
                    vec!["sh".to_owned(), "-c".to_owned(), "echo b".to_owned()]
                ),
            ]
        );
        assert_eq!(config.override_command, Some(false));
        assert_eq!(config.cap_add, ["SYS_PTRACE"]);
    }

    #[test]
    fn legacy_dockerfile_key() {
        let config = parse(r#"{"dockerFile": "../Dockerfile"}"#);
        let Some(ImageSource::Build {
            dockerfile,
            context,
            ..
        }) = config.image_source(Path::new("/r/.devcontainer"))
        else {
            panic!("expected a build");
        };
        assert_eq!(dockerfile, Path::new("/r/.devcontainer/../Dockerfile"));
        assert_eq!(context, Path::new("/r/.devcontainer/."));
    }

    #[test]
    fn no_image() {
        assert_eq!(parse("{}").image_source(Path::new("/r")), None);
    }

    #[test]
    fn substitutes_variables() {
        let config = parse(
            r#"{
            "workspaceFolder": "/w/${localWorkspaceFolderBasename}",
            "containerEnv": {
                "LOCAL": "${localWorkspaceFolder}",
                "INSIDE": "${containerWorkspaceFolder}",
                "BASE": "${containerWorkspaceFolderBasename}",
                "HOME_DIR": "${localEnv:HOME}",
                "MISSING": "${localEnv:NOPE}",
                "DEFAULT": "${localEnv:NOPE:fallback:with:colons}",
                "ID": "vol-${devcontainerId}",
                "KEEP": "${containerEnv:PATH}:/x",
                "OPEN": "a ${unclosed"
            }
        }"#,
        );
        let env = &config.container_env;
        assert_eq!(config.workspace_folder, "/w/my app");
        assert_eq!(env["LOCAL"], "/src/my app");
        assert_eq!(env["INSIDE"], "/w/my app");
        assert_eq!(env["BASE"], "my app");
        assert_eq!(env["HOME_DIR"], "/home/me");
        assert_eq!(env["MISSING"], "");
        assert_eq!(env["DEFAULT"], "fallback:with:colons");
        assert_eq!(env["ID"], "vol-abc123");
        assert_eq!(env["KEEP"], "${containerEnv:PATH}:/x");
        assert_eq!(env["OPEN"], "a ${unclosed");
    }

    #[test]
    fn flags_cli_only_keys() {
        let config = parse(r#"{"dockerComposeFile": "c.yml", "features": {"x": {}}}"#);
        assert_eq!(config.cli_only_keys(), ["dockerComposeFile", "features"]);
        assert!(
            parse(r#"{"image": "a", "features": {}}"#)
                .cli_only_keys()
                .is_empty()
        );
    }

    #[test]
    fn rejects_bad_json() {
        let err = Config::parse_with("{\"a\": }", Path::new("/r"), "x", &env).unwrap_err();
        assert!(err.contains("line 1"), "{err}");
        assert!(Config::parse_with("[1]", Path::new("/r"), "x", &env).is_err());
    }
}
