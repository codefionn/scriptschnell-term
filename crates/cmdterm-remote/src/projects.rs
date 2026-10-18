//! The projects a remote machine shares with its clients.
//!
//! A client opens a channel whose program is [`PROGRAM`]. The server runs no
//! process for it: it answers with the repositories in its own
//! scriptschnell-term `state.json` as one JSON array ([`SharedProject`]) and
//! exits with 0. Only repositories on the machine itself count, not those the
//! machine reaches over SSH or on other remote machines, so two machines that
//! show each other's projects do not show them back in a loop.
//!
//! The app shows the projects of the machines it is told to in its sidebar,
//! next to its own, so switching to a worktree on another machine is the same
//! as switching to a local one.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cmdterm_env::Environment;
use serde::{Deserialize, Serialize};

use crate::exec::{Channel, Outbox};
use crate::proto::Msg;

/// The program name that asks for the list.
pub const PROGRAM: &str = "cmdterm-remote:projects";

/// One repository on the serving machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedProject {
    pub name: String,
    /// The main worktree's root on the serving machine.
    pub root: PathBuf,
    /// `git`, `mercurial` or `jujutsu`, as `state.json` names it.
    pub vcs: String,
}

/// The repositories on this machine in `state`, a scriptschnell-term
/// `state.json`. A missing or unreadable file shares nothing.
pub fn list(state: &Path) -> Vec<SharedProject> {
    let Ok(bytes) = std::fs::read(state) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let Some(projects) = value.get("projects").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    projects
        .iter()
        .filter(|project| {
            // `state.json` leaves `env` out for this machine in older files.
            project
                .get("env")
                .is_none_or(|env| env.get("kind").and_then(|k| k.as_str()) == Some("local"))
        })
        .filter_map(|project| {
            Some(SharedProject {
                name: project.get("name")?.as_str()?.to_owned(),
                root: PathBuf::from(project.get("root")?.as_str()?),
                // Global sessions have no repository.
                vcs: project.get("vcs")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Answers channel `id` with `projects`.
pub fn open(id: u32, projects: &[SharedProject], out: Outbox) -> Box<dyn Channel> {
    let json = serde_json::to_vec(projects).unwrap_or_else(|_| b"[]".to_vec());
    let _ = out.send(Msg::Data(id, json));
    let _ = out.send(Msg::Exit(id, 0));
    Box::new(Answered)
}

/// Asks the remote machine `env` for the projects it shares. Fails when the
/// machine is unreachable, does not answer within `timeout`, shares none,
/// or runs a server from before shared projects.
pub fn fetch(env: &Environment, timeout: Duration) -> io::Result<Vec<SharedProject>> {
    let output = env.command(PROGRAM).output_within(timeout)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(io::Error::other(if message.is_empty() {
            "the machine shares no projects".to_owned()
        } else {
            message.to_owned()
        }));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// The channel of a [`PROGRAM`] open, which has already exited.
struct Answered;

impl Channel for Answered {
    fn input(&mut self, _data: Vec<u8>) {}
    fn close_input(&mut self) {}
    fn resize(&mut self, _cols: u16, _rows: u16) {}
    fn kill(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_repositories_on_this_machine_only() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        std::fs::write(
            &state,
            r#"{"version": 1, "projects": [
                {"id": 1, "name": "app", "env": {"kind": "local"}, "root": "/srv/app", "vcs": "git"},
                {"id": 2, "name": "old", "root": "/srv/old", "vcs": "mercurial"},
                {"id": 3, "name": "far", "env": {"kind": "ssh", "host": "x"}, "root": "/a", "vcs": "git"},
                {"id": 4, "name": "there", "env": {"kind": "remote", "address": "b:7722"}, "root": "/b", "vcs": "git"},
                {"id": 5, "name": "Home", "env": {"kind": "local"}, "root": "/home/me"}
            ]}"#,
        )
        .unwrap();
        let names: Vec<_> = list(&state).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["app", "old"]);
        assert_eq!(list(&dir.path().join("missing.json")), []);
    }
}
