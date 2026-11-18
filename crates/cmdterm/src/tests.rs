//! End-to-end checks of the app model against real git repos and shells,
//! without a window.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::state::{PaneLayout, SettingsStore, StateStore, TabConfig};
use schnellui::scene::Size;
use schnellui::{FocusedInputEvent, RawImeEvent};

use crate::workspace::Workspace;

// Podman shell scripts.
#[cfg(unix)]
mod agent_harness;

const VIEWPORT: Size = Size {
    width: 1000.0,
    height: 600.0,
};

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "README"]);
    git(&repo, &["commit", "-qm", "init"]);
    cmdterm_env::canonicalize(&repo).unwrap()
}

fn workspace(root: &Path) -> Workspace {
    // Test binaries have no `cmdterm-proxy-bridge` beside them, so their
    // sandboxes keep the host's network instead of the default allowlist
    // and their own namespace, which both need the bridge.
    let settings = SettingsStore::new(root.join("cfg/settings.json"));
    let mut saved = settings.load().unwrap();
    saved.sandbox.restrict_network = false;
    saved.sandbox.host_network = true;
    // Git for Windows' `sh`, which these tests type into, cannot start under
    // the restricted token while another MSYS program runs: it cannot open
    // the per-user memory section the others made. Tests that need the
    // sandbox there pick it themselves.
    if cfg!(windows) {
        saved.default_runtime = crate::state::Runtime::Host;
    }
    settings.save(&saved).unwrap();
    Workspace::new(
        StateStore::new(root.join("cfg/state.json")),
        settings,
        &root.join("cfg/extensions"),
    )
}

/// Writes extension `name` as a directory with a manifest declaring
/// `permissions`, and grants them in the scratch settings under `root`.
fn granted_extension(root: &Path, name: &str, permissions: &[&str], source: &str) {
    let dir = root.join("cfg/extensions").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let list = permissions
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(dir.join("manifest.toml"), format!("permissions = [{list}]")).unwrap();
    std::fs::write(dir.join("init.lua"), source).unwrap();
    let store = SettingsStore::new(root.join("cfg/settings.json"));
    let mut settings = store.load().unwrap();
    let names: Vec<String> = permissions.iter().map(|p| (*p).to_owned()).collect();
    settings.grant_extension(name, &names);
    store.save(&settings).unwrap();
}

#[track_caller]
fn wait_until(mut step: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !step() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Typed into `sh`: prints the directory, then `marker-42`. Git for
/// Windows' `sh` prints `/tmp/x` for a plain `pwd`, `C:/Users/.../x` for
/// `pwd -W`.
const PWD_AND_MARKER: &str = if cfg!(windows) {
    "pwd -W; echo marker-$((40+2))\r"
} else {
    "pwd; echo marker-$((40+2))\r"
};

/// `dir` as [`PWD_AND_MARKER`] prints it.
fn shell_dir(dir: &Path) -> String {
    let shown = dir.display().to_string();
    if cfg!(windows) {
        shown.replace('\\', "/")
    } else {
        shown
    }
}

/// Adds a project and polls until the worker has discovered it.
fn add_project_and_wait(ws: &Workspace, repo: &Path) {
    let before = ws.borrow().state.projects.len();
    ws.add_project(repo.to_str().unwrap());
    wait_until(|| {
        ws.poll();
        ws.borrow().state.projects.len() > before
    });
}

#[test]
fn adding_a_non_repository_notifies_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let ws = workspace(dir.path());
    ws.add_project(plain.to_str().unwrap());
    assert_eq!(ws.borrow().pending_projects.len(), 1);
    wait_until(|| {
        ws.poll();
        ws.borrow().pending_projects.is_empty()
    });
    let inner = ws.borrow();
    assert!(inner.state.projects.is_empty());
    let path = plain.to_str().unwrap();
    assert!(
        inner.notices.iter().any(|n| n.text.contains(path)),
        "{:?}",
        inner.notices
    );
}

#[test]
fn a_local_session_is_shown_but_not_saved_until_opened_for_good() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    ws.add_temporary_project(repo.to_str().unwrap());
    wait_until(|| {
        ws.poll();
        !ws.borrow().state.projects.is_empty()
    });
    let id = ws.borrow().state.projects[0].id;
    assert!(ws.borrow().state.projects[0].temporary);
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    assert!(saved.projects.is_empty(), "{saved:?}");
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(app.a11y_json().contains("temporary"));

    ws.add_project(repo.to_str().unwrap());
    wait_until(|| {
        ws.poll();
        ws.borrow().pending_projects.is_empty()
    });
    assert!(!ws.borrow().state.project(id).unwrap().temporary);
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    assert_eq!(saved.projects.len(), 1);
}

#[test]
fn a_global_session_runs_on_the_host_in_its_folder_and_is_saved_once_kept() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let home = cmdterm_env::canonicalize(&home).unwrap();
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    ws.open_global_session(home.to_str().unwrap());
    wait_until(|| {
        ws.poll();
        ws.borrow().state.projects.len() == 2
    });
    let id = {
        let inner = ws.borrow();
        let global = inner.state.projects.iter().find(|p| p.is_global()).unwrap();
        assert!(global.temporary);
        assert_eq!(global.root, home);
        assert_eq!(inner.state.active_project, Some(global.id));
        assert_eq!(
            global.worktree(&home).unwrap().runtime,
            crate::state::Runtime::Host
        );
        global.id
    };
    let store = StateStore::new(dir.path().join("cfg/state.json"));
    assert_eq!(store.load().unwrap().projects.len(), 1);

    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(
        PWD_AND_MARKER.into(),
    )));
    assert!(typed.is_handled(), "the terminal has focus");
    let expected_dir = shell_dir(&home);
    wait_until(|| {
        ws.poll();
        app.frame();
        let a11y = app.a11y_json();
        a11y.contains("marker-42") && a11y.contains(&expected_dir)
    });

    // The same folder again selects the session instead of adding one.
    ws.open_global_session(home.to_str().unwrap());
    wait_until(|| {
        ws.poll();
        ws.borrow().pending_projects.is_empty()
    });
    assert_eq!(ws.borrow().state.projects.len(), 2);

    ws.open_palette(None);
    ws.set_palette_query("Keep session");
    ws.palette_enter();
    assert!(!ws.borrow().state.project(id).unwrap().temporary);
    let saved = store.load().unwrap();
    let global = saved.project(id).unwrap();
    assert!(global.is_global());
    assert_eq!(saved.active_project, Some(id));
}

/// Replaces the worktree's default login shell with plain `sh`, so the test
/// does not depend on the user's shell configuration.
fn use_plain_sh(ws: &Workspace) {
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(Some(vec!["sh".into()]), Some("sh".into()));
}

#[test]
fn typed_text_runs_in_the_worktree_and_shows_in_the_grid() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);

    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(
        PWD_AND_MARKER.into(),
    )));
    assert!(typed.is_handled(), "the terminal has focus");
    let expected_dir = shell_dir(&repo);
    wait_until(|| {
        ws.poll();
        app.frame();
        let a11y = app.a11y_json();
        a11y.contains("marker-42") && a11y.contains(&expected_dir)
    });
}

#[test]
fn new_worktree_switches_and_restores_panels() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.open_browser("localhost:3000");
    let main_panels = ws.with_active(|live| live.unwrap().panels.len());
    assert_eq!(main_panels, 2);

    // The worker needs a snapshot before it can add a worktree.
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    ws.add_worktree("feature/x");
    let feature = repo.with_file_name("app-feature-x");
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .state
            .active()
            .unwrap()
            .active_worktree
            .as_deref()
            == Some(&feature)
    });
    // A fresh worktree starts with one shell.
    assert_eq!(ws.with_active(|live| live.unwrap().panels.len()), 1);
    wait_until(|| {
        ws.poll();
        let inner = ws.borrow();
        inner.snapshots.values().any(|snapshot| {
            snapshot
                .worktrees
                .iter()
                .any(|w| w.path == feature && w.branch.as_deref() == Some("feature/x"))
        })
    });

    // Switching back finds the running panels, then the layout is on disk.
    let id = ws.borrow().state.active_project.unwrap();
    ws.activate_worktree(id, &repo);
    assert_eq!(ws.with_active(|live| live.unwrap().panels.len()), 2);
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    let project = saved.project(id).unwrap();
    assert_eq!(project.worktree(&repo).unwrap().panels.len(), 2);
    assert_eq!(project.worktree(&feature).unwrap().panels.len(), 1);
}

#[test]
fn extension_requests_open_panels_and_notify() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    granted_extension(
        dir.path(),
        "hello",
        &["browser"],
        r#"
        cmdterm.on("worktree_activated", function(ev)
            cmdterm.notify("entered " .. ev.project)
        end)
        cmdterm.command{ name = "docs", title = "Open docs", run = function()
            cmdterm.open_browser("localhost:8000")
        end }
        "#,
    );
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    ws.poll();
    assert!(ws.borrow().notices.iter().any(|n| n.text == "entered app"));
    ws.run_command("docs");
    ws.poll();
    let titles: Vec<String> = ws.with_active(|live| {
        live.unwrap()
            .panels
            .iter()
            .map(|(_, p)| p.title())
            .collect()
    });
    assert!(titles.iter().any(|t| t == "localhost:8000"), "{titles:?}");
}

#[test]
fn extension_views_open_in_panels_that_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    granted_extension(
        dir.path(),
        "notes",
        &["storage"],
        r##"
        local count = term.store.get("count") or 0
        term.view{ name = "note", render = function(key)
          count = count + 1
          term.store.set("count", count)
          return { title = "Note " .. key, markdown = "# Heading\n\nrendered-" .. key .. " $\\alpha$" }
        end }
        "##,
    );
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);

    ws.open_view("notes.note".into(), "n1".into());
    ws.poll();
    let titles = || -> Vec<String> {
        ws.with_active(|live| {
            live.unwrap()
                .panels
                .iter()
                .map(|(_, p)| p.title())
                .collect()
        })
    };
    assert!(titles().iter().any(|t| t == "Note n1"), "{:?}", titles());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("rendered-n1 α"), "{json}");

    // Opening it again shows the same panel instead of a second one.
    ws.open_view("notes.note".into(), "n1".into());
    ws.poll();
    assert_eq!(titles().iter().filter(|t| *t == "Note n1").count(), 1);
    ws.save();
    drop(app);
    drop(ws);

    let ws = workspace(dir.path());
    ws.poll();
    let titles: Vec<String> = ws.with_active(|live| {
        live.unwrap()
            .panels
            .iter()
            .map(|(_, p)| p.title())
            .collect()
    });
    assert!(titles.iter().any(|t| t == "Note n1"), "{titles:?}");
}

/// The hot paths cmdterm owns, measured on the UI thread: an idle wake must
/// allocate nothing, and neither may a frame that patches one echoed row.
#[cfg(feature = "count-allocations")]
#[test]
fn idle_wake_and_one_row_echo_stay_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(Some(vec!["cat".into()]), Some("cat".into()));
    let mut app = crate::build_app(&ws, VIEWPORT);
    let terminal = ws.active_terminal().unwrap();

    let mut echo = |byte: &[u8]| {
        let before = terminal.session.revision();
        terminal.session.write_bytes(byte).unwrap();
        wait_until(|| terminal.session.revision() != before);
        // Let the echo settle so the measured frame sees one complete change.
        std::thread::sleep(Duration::from_millis(30));
    };
    for _ in 0..3 {
        echo(b"w");
        ws.poll();
        app.frame();
    }
    for _ in 0..3 {
        ws.poll();
        app.frame();
    }

    let idle = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    eprintln!("idle wake: {idle:?}");
    assert_eq!(idle.count_total, 0, "idle wake allocated: {idle:?}");

    echo(b"w");
    let row = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    eprintln!("one-row echo: {row:?}");
    assert_eq!(row.count_total, 0, "one-row echo allocated: {row:?}");
    // The measured frame did the work: the fourth echo reached the grid.
    assert!(app.a11y_json().contains("wwww"));
}

fn panel_titles(ws: &Workspace) -> Vec<String> {
    ws.with_active(|live| {
        live.map(|live| live.panels.iter().map(|(_, p)| p.title()).collect())
            .unwrap_or_default()
    })
}

/// A workspace with one project, a mounted app and no open palette.
fn palette_fixture(dir: &Path) -> (Workspace, schnellui::App) {
    let repo = repo(dir);
    let ws = workspace(dir);
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let app = crate::build_app(&ws, VIEWPORT);
    (ws, app)
}

fn palette_input_is_focused(app: &schnellui::App) -> bool {
    use crate::ui::{InputMode, PALETTE_INPUT, PALETTE_PANEL_INPUT};
    let focused = app.focused_widget();
    focused.is_some()
        && [PALETTE_INPUT, PALETTE_PANEL_INPUT]
            .into_iter()
            .chain(InputMode::ALL.map(InputMode::input_name))
            .any(|name| app.find_widget(schnellui::a11y::Role::TextInput, Some(name)) == focused)
}

#[test]
fn the_palette_opens_by_chord_and_shortcut_and_takes_focus() {
    use schnellui::{FocusedInputResult, Shortcut};
    let dir = tempfile::tempdir().unwrap();
    let (ws, mut app) = palette_fixture(dir.path());
    assert!(!ws.palette_is_open());
    assert_eq!(
        app.dispatch_focused_input(chord("p")),
        FocusedInputResult::Handled
    );
    assert!(ws.palette_is_open());
    ws.take_dirty();
    let app2 = crate::build_app(&ws, VIEWPORT);
    assert!(palette_input_is_focused(&app2));

    // The shortcut opens it when no pane has focus.
    ws.close_palette();
    app = crate::build_app(&ws, VIEWPORT);
    assert!(app.dispatch_shortcut(Shortcut::new('p', true, true, false)));
    assert!(ws.palette_is_open());
}

#[test]
fn the_rail_plus_button_opens_the_palette_in_open_project_mode() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, mut app) = palette_fixture(dir.path());
    let plus = app.find_widget(Role::Button, Some("Open project")).unwrap();
    app.dispatch_click(plus);
    assert!(ws.palette_is_open());
    assert_eq!(
        ws.borrow().palette.input,
        Some(crate::ui::InputMode::Project)
    );
}

#[test]
fn the_tab_plus_button_lists_the_panels_to_open() {
    use schnellui::UiKey;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, mut app) = palette_fixture(dir.path());
    let plus = app.find_widget(Role::Button, Some("New panel")).unwrap();
    app.dispatch_click(plus);
    assert!(ws.palette_is_open());
    assert!(ws.borrow().palette.new_panel);
    let labels: Vec<String> = crate::ui::palette_entries(&ws.borrow())
        .into_iter()
        .map(|entry| entry.label)
        .collect();
    assert_eq!(
        labels,
        [
            "New shell",
            "Browse files",
            "Explore history",
            "Review changes",
            "Show the change request",
            "Monitor machine",
            "Show tasks",
            "Debug browser",
            "Open URL…",
            "Open URL in debug browser…"
        ]
    );

    // The URL row asks for a URL; Escape goes back to the panel list.
    ws.set_palette_query("url");
    ws.palette_enter();
    assert_eq!(ws.borrow().palette.input, Some(crate::ui::InputMode::Url));
    ws.palette_escape();
    assert!(ws.palette_is_open());
    assert!(ws.borrow().palette.new_panel);
    assert_eq!(ws.borrow().palette.input, None);

    ws.take_dirty();
    app = crate::build_app(&ws, VIEWPORT);
    let before = panel_titles(&ws).len();
    assert!(app.dispatch_key(UiKey::Enter));
    assert!(!ws.palette_is_open());
    assert_eq!(panel_titles(&ws).len(), before + 1);
}

#[test]
fn configured_agents_can_be_started_from_both_palette_lists() {
    use crate::agents::Agent;
    use crate::ui::{PaletteAction, palette_entries};
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    let env = ws.borrow().state.active().unwrap().env.clone();
    (ws.borrow().settings_ui.agents).set_configured(env, vec![Agent::ClaudeCode, Agent::Pi]);
    let starts = |ws: &crate::workspace::Workspace| -> Vec<(String, PaletteAction)> {
        palette_entries(&ws.borrow())
            .into_iter()
            .filter(|entry| matches!(entry.action, PaletteAction::StartAgent(_)))
            .map(|entry| (entry.label, entry.action))
            .collect()
    };
    let expected = vec![
        (
            "Start Claude Code".to_owned(),
            PaletteAction::StartAgent(Agent::ClaudeCode),
        ),
        ("Start pi".to_owned(), PaletteAction::StartAgent(Agent::Pi)),
    ];
    ws.open_new_panel();
    assert_eq!(starts(&ws), expected);
    ws.close_palette();
    ws.open_palette(None);
    assert_eq!(starts(&ws), expected);
}

#[test]
fn the_new_panel_list_offers_the_listed_shells() {
    use crate::state::ShellChoice;
    use crate::ui::{PaletteAction, palette_entries};
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    // Windows has no `/bin/sh`; a bare `sh` is the one Git brings.
    let sh = if cfg!(windows) { "sh" } else { "/bin/sh" };
    {
        let mut inner = ws.borrow_mut();
        inner.shells = vec![cmdterm_env::ShellOption {
            id: "pwsh",
            name: "PowerShell",
            argv: vec!["pwsh".into(), "-NoLogo".into()],
        }];
        inner.settings.shells = vec![
            ShellChoice::Name("pwsh".into()),
            // Not installed here: left out.
            ShellChoice::Name("git-bash".into()),
            ShellChoice::Command(vec![sh.into(), "-i".into()]),
        ];
    }
    ws.open_new_panel();
    let shells: Vec<(String, PaletteAction)> = palette_entries(&ws.borrow())
        .into_iter()
        .filter(|entry| matches!(entry.action, PaletteAction::NewShellWith { .. }))
        .map(|entry| (entry.label, entry.action))
        .collect();
    assert_eq!(
        shells,
        [
            (
                "New shell: PowerShell".to_owned(),
                PaletteAction::NewShellWith {
                    name: "PowerShell".into(),
                    argv: vec!["pwsh".into(), "-NoLogo".into()],
                }
            ),
            (
                "New shell: sh".to_owned(),
                PaletteAction::NewShellWith {
                    name: "sh".into(),
                    argv: vec![sh.into(), "-i".into()],
                }
            ),
        ]
    );
    let before = panel_titles(&ws).len();
    ws.set_palette_query("new shell: sh");
    ws.palette_run(0);
    assert_eq!(panel_titles(&ws).len(), before + 1);
}

#[test]
fn enter_on_new_shell_adds_a_panel_and_closes_the_palette() {
    use schnellui::UiKey;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    let before = panel_titles(&ws).len();
    ws.open_palette(None);
    ws.set_palette_query("new shell");
    let mut app = crate::build_app(&ws, VIEWPORT);
    assert!(palette_input_is_focused(&app));
    assert!(app.dispatch_key(UiKey::Enter));
    assert!(!ws.palette_is_open());
    assert_eq!(panel_titles(&ws).len(), before + 1);
}

#[test]
fn typing_a_settings_page_name_opens_that_page() {
    use crate::ui::SettingsSection;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(None);
    // The idle list leaves the pages out.
    assert!(
        !crate::ui::palette_entries(&ws.borrow())
            .iter()
            .any(|e| e.label.starts_with("Settings › "))
    );
    ws.set_palette_query("settings › hosts");
    ws.palette_run(0);
    assert!(ws.settings_is_open());
    assert_eq!(
        ws.borrow().settings_ui.section,
        SettingsSection::SandboxHosts
    );
}

#[test]
fn the_command_button_opens_the_palette_with_key_hints() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, mut app) = palette_fixture(dir.path());
    let button = app
        .find_widget(Role::Button, Some("Command palette"))
        .unwrap();
    app.dispatch_click(button);
    assert!(ws.palette_is_open());
    ws.set_palette_query("next tab");
    ws.take_dirty();
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let keys = ws
        .borrow()
        .keymap
        .keys_for(&crate::keymap::Action::NextTab)
        .unwrap()
        .to_string();
    let json = app.a11y_json();
    assert!(json.contains("Next tab"), "{json}");
    assert!(json.contains(&keys), "{keys}\n{json}");
}

#[test]
fn up_and_down_move_the_highlight() {
    use schnellui::UiKey;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    assert!(app.dispatch_key(UiKey::Down { shift: false }));
    assert!(app.dispatch_key(UiKey::Down { shift: false }));
    assert_eq!(ws.borrow().palette.highlight, 2);
    assert!(app.dispatch_key(UiKey::Up { shift: false }));
    assert_eq!(ws.borrow().palette.highlight, 1);
    // Up from the first row wraps to the last.
    ws.set_palette_query("split");
    assert!(app.dispatch_key(UiKey::Up { shift: false }));
    assert_eq!(ws.borrow().palette.highlight, 1);
    // Row 1 of "split" is the stacked split.
    app.dispatch_key(UiKey::Enter);
    assert!(!ws.palette_is_open());
}

#[test]
fn the_open_url_input_mode_opens_a_browser_panel() {
    use schnellui::UiKey;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(None);
    ws.set_palette_query("open url");
    let mut app = crate::build_app(&ws, VIEWPORT);
    assert!(app.dispatch_key(UiKey::Enter));
    assert!(ws.palette_is_open());
    assert_eq!(ws.borrow().palette.input, Some(crate::ui::InputMode::Url));
    assert!(ws.borrow().palette.query.is_empty());
    app = crate::build_app(&ws, VIEWPORT);
    assert!(palette_input_is_focused(&app));
    // An empty submit stays in the mode.
    assert!(app.dispatch_key(UiKey::Enter));
    assert!(ws.palette_is_open());
    ws.set_palette_query("localhost:4000");
    assert!(app.dispatch_key(UiKey::Enter));
    assert!(!ws.palette_is_open());
    let titles = panel_titles(&ws);
    assert!(titles.iter().any(|t| t == "localhost:4000"), "{titles:?}");
}

#[test]
fn input_modes_for_project_and_worktree_submit_through_the_workspace() {
    use crate::ui::InputMode;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    let second = dir.path().join("other");
    std::fs::create_dir_all(&second).unwrap();
    git(&second, &["init", "-q"]);
    std::fs::write(second.join("f"), "x").unwrap();
    git(&second, &["add", "f"]);
    git(&second, &["commit", "-qm", "init"]);
    ws.open_palette(Some(InputMode::Project));
    ws.set_palette_query(second.to_str().unwrap());
    ws.palette_enter();
    wait_until(|| {
        ws.poll();
        ws.borrow().state.projects.len() == 2
    });
    assert!(!ws.palette_is_open());

    let active = ws.borrow().state.active_project.unwrap();
    wait_until(|| {
        ws.poll();
        ws.borrow().snapshots.contains_key(&active)
    });
    ws.open_palette(Some(InputMode::Worktree));
    ws.set_palette_query("feature-x");
    ws.palette_enter();
    wait_until(|| {
        ws.poll();
        let inner = ws.borrow();
        inner
            .snapshots
            .get(&active)
            .is_some_and(|s| s.worktrees.len() == 2)
    });
}

#[test]
fn the_project_input_has_an_open_button_and_takes_the_chosen_folder() {
    use crate::ui::InputMode;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(Some(InputMode::Project));
    ws.set_palette_query("typed so far");
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(
        app.find_widget(Role::Button, Some("Choose folder"))
            .is_some()
    );
    assert!(
        app.find_widget(Role::TextInput, Some("Project folder"))
            .is_some()
    );

    ws.borrow_mut().dirty = Default::default();
    let sender = ws.borrow().picked_folders.0.clone();
    sender
        .send((
            crate::workspace::FolderTarget::Palette,
            "/srv/picked".into(),
        ))
        .unwrap();
    ws.poll();
    assert_eq!(ws.borrow().palette.query, "/srv/picked");
    assert!(ws.borrow().dirty.palette);

    // A folder that arrives after the input closed is dropped.
    ws.close_palette();
    sender
        .send((crate::workspace::FolderTarget::Palette, "/srv/late".into()))
        .unwrap();
    ws.poll();
    assert!(ws.borrow().palette.query.is_empty());
}

#[test]
fn the_open_button_is_only_in_the_project_input() {
    use crate::ui::InputMode;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(Some(InputMode::Url));
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(
        app.find_widget(Role::Button, Some("Choose folder"))
            .is_none()
    );
}

#[test]
fn escape_leaves_an_input_mode_then_closes_the_palette() {
    use crate::ui::InputMode;
    use schnellui::UiKey;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(Some(InputMode::Url));
    let mut app = crate::build_app(&ws, VIEWPORT);
    assert!(app.dispatch_key(UiKey::Escape));
    assert!(ws.palette_is_open());
    assert_eq!(ws.borrow().palette.input, None);
    app = crate::build_app(&ws, VIEWPORT);
    assert!(app.dispatch_key(UiKey::Escape));
    assert!(!ws.palette_is_open());
}

#[test]
fn clicking_outside_the_palette_closes_it() {
    use schnellui::scene::Point;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let outside = app
        .hit_test(Point { x: 20.0, y: 560.0 })
        .expect("the backdrop takes the click");
    app.dispatch_click(outside);
    assert!(!ws.palette_is_open());
}

#[test]
fn palette_keystrokes_never_reach_the_terminal() {
    use schnellui::UiKey;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_palette(None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    assert!(palette_input_is_focused(&app));
    // A terminal chord and a plain key go to the palette's input, which has
    // no terminal handler: the chord is not handled as an app chord either.
    let before = panel_titles(&ws).len();
    assert_eq!(
        app.dispatch_focused_input(chord("t")),
        schnellui::FocusedInputResult::Ignored
    );
    assert_eq!(panel_titles(&ws).len(), before);
    assert!(app.dispatch_key(UiKey::Down { shift: false }));
}

#[test]
fn the_palette_lists_switch_and_destructive_entries() {
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    let labels: Vec<String> = crate::ui::palette_entries(&ws.borrow())
        .into_iter()
        .map(|e| e.label)
        .collect();
    assert!(
        labels.iter().any(|l| l == "Switch to app / main"),
        "{labels:?}"
    );
    assert!(labels.iter().any(|l| l == "Forget project app"));
    assert!(labels.iter().any(|l| l == "Open project…"));
    // The main worktree cannot be removed.
    assert!(!labels.iter().any(|l| l.starts_with("Remove worktree")));
}

#[test]
fn forgetting_a_project_from_the_palette_takes_a_second_enter() {
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    let before = ws.borrow().state.projects.len();
    ws.open_palette(None);
    ws.set_palette_query("forget project app");
    ws.palette_run(0);
    assert!(ws.palette_is_open());
    assert_eq!(ws.borrow().palette.armed, Some(0));
    assert_eq!(ws.borrow().state.projects.len(), before);
    // Typing disarms it again.
    ws.set_palette_query("forget project ap");
    assert_eq!(ws.borrow().palette.armed, None);
    ws.palette_run(0);
    ws.palette_run(0);
    assert!(!ws.palette_is_open());
    assert_eq!(ws.borrow().state.projects.len(), before - 1);
}

#[cfg(unix)]
#[test]
fn the_palette_exposes_and_stops_exposing_the_ports_of_the_active_worktree() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    let labels = |ws: &Workspace| -> Vec<String> {
        crate::ui::palette_entries(&ws.borrow())
            .into_iter()
            .map(|e| e.label)
            .filter(|l| l.contains("port"))
            .collect()
    };
    assert!(labels(&ws).is_empty());
    let (id, path) = ws.borrow().active_key().unwrap();
    let hub = std::sync::Arc::new(cmdterm_env::PortHub::start(&dir.path().join("p.sock")).unwrap());
    ws.borrow().runtimes.ports.insert(id, &path, hub.clone());
    // A fake bridge reports a server on port 4321.
    let mut bridge = std::os::unix::net::UnixStream::connect(dir.path().join("p.sock")).unwrap();
    writeln!(bridge, "HELLO t\nLISTEN 4321").unwrap();
    wait_until(|| !hub.ports().is_empty());
    assert_eq!(labels(&ws), ["Expose port 4321"]);

    // The hub's revision moves, so the next poll refreshes an open list.
    ws.open_palette(None);
    ws.poll();
    ws.borrow_mut().dirty.palette_list = false;
    ws.expose_port(4321, true);
    ws.poll();
    assert!(ws.borrow().dirty.palette_list);
    assert!(hub.ports()[0].exposed.is_some());
    assert_eq!(labels(&ws), ["Stop exposing port 4321"]);
    ws.expose_port(4321, false);
    assert!(hub.ports()[0].exposed.is_none());
    assert_eq!(labels(&ws), ["Expose port 4321"]);

    assert_eq!((crate::i18n::DE.expose_port)(4321), "Port 4321 freigeben");
    assert_eq!(
        (crate::i18n::DE.stop_exposing_port)(4321),
        "Port 4321 nicht mehr freigeben"
    );
    drop(bridge);
}

#[cfg(unix)]
#[test]
fn the_browser_proxy_of_a_worktree_reaches_a_port_inside_its_sandbox() {
    browser_proxy_reaches_the_sandbox(false);
}

/// Chrome and Firefox answer the hub's 407 themselves. They start only on
/// request: `cargo test -p cmdterm -- --ignored browsers_pass_the_browser_proxy`.
#[cfg(unix)]
#[test]
#[ignore = "starts real browsers"]
fn browsers_pass_the_browser_proxy_password() {
    browser_proxy_reaches_the_sandbox(true);
}

#[cfg(unix)]
fn browser_proxy_reaches_the_sandbox(browsers: bool) {
    use crate::control::PortAccess;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::net::UnixStream;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("p.sock");
    let hub = std::sync::Arc::new(cmdterm_env::PortHub::start(&socket).unwrap());
    let hubs = crate::runtime::PortHubs::default();
    assert_eq!(hubs.http_proxy(1, Path::new("/w")), None);
    hubs.insert(1, Path::new("/w"), hub.clone());
    let url = hubs.http_proxy(1, Path::new("/w")).unwrap();
    assert_eq!(hubs.http_proxy(2, Path::new("/w")), None);
    let (proxy, token) = hub.http_proxy().unwrap();
    assert_eq!(url, token.url(proxy));
    let password = url
        .strip_prefix("http://cmdterm:")
        .and_then(|rest| rest.split_once('@'))
        .unwrap()
        .0;
    let authorization = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("cmdterm:{password}"),
    );

    // The "sandbox" server: answers every connection with a fixed body.
    let server = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in server.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ninside",
            );
        }
    });
    // A fake bridge that dials `port` whenever the hub opens a connection.
    let control = UnixStream::connect(&socket).unwrap();
    writeln!(&control, "HELLO t").unwrap();
    let reader = control.try_clone().unwrap();
    let sock = socket.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("OPEN ") {
                let conn = rest.split_once(' ').unwrap().0.to_owned();
                let sock = sock.clone();
                std::thread::spawn(move || {
                    let mut data = UnixStream::connect(&sock).unwrap();
                    writeln!(data, "DATA {conn}").unwrap();
                    let inside = TcpStream::connect(("127.0.0.1", port)).unwrap();
                    let (mut a, mut b) = (data.try_clone().unwrap(), inside.try_clone().unwrap());
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut a, &mut b);
                    });
                    let (mut data, mut inside) = (data, inside);
                    let _ = std::io::copy(&mut inside, &mut data);
                    let _ = data.shutdown(std::net::Shutdown::Both);
                });
            }
        }
    });
    // The hub learns of the port, as a bridge reports it.
    writeln!(&control, "LISTEN {port}").unwrap();
    wait_until(|| !hub.ports().is_empty());

    let ask = |authorization: &str| {
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).unwrap();
        write!(
            client,
            "GET http://localhost:{port}/ HTTP/1.1\r\nHost: localhost:{port}\r\n\
             Proxy-Authorization: Basic {authorization}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        answer
    };
    let answer = ask(&authorization);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("inside"), "{answer}");
    // Other programs on this machine do not know the password.
    let answer = ask("Y21kdGVybTp3cm9uZw==");
    assert!(answer.starts_with("HTTP/1.1 407"), "{answer}");

    if browsers {
        for kind in [
            crate::devtools::Choice::Chrome,
            crate::devtools::Choice::Firefox,
        ] {
            let profiles = dir.path().join("browsers");
            let page = format!("http://localhost:{port}/");
            let text = crate::devtools::text_through_proxy(kind, &profiles, &url, &page);
            assert_eq!(text.as_deref(), Ok("inside"), "{kind:?}");
        }
    }
}

#[test]
fn the_rail_and_tab_strip_have_no_text_inputs_but_the_search_or_destructive_buttons() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert_eq!(
        app.find_widget(Role::TextInput, None),
        app.find_widget(Role::TextInput, Some(crate::ui::RAIL_SEARCH_INPUT)),
        "{json}"
    );
    assert_eq!(json.matches("\"text_input\"").count(), 1, "{json}");
    for gone in [
        "Remove worktree",
        "Forget project",
        "Add project",
        "Open URL",
    ] {
        assert!(!json.contains(&format!("\"{gone}\"")), "{gone}");
    }
}

#[test]
fn new_shell_starts_in_the_directory_the_active_shell_reported() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_terminal_in(
        None,
        Some(vec![
            "sh".into(),
            "-c".into(),
            r#"printf "\033]7;file://some-host/tmp\007"; sleep 30"#.into(),
        ]),
        Some("osc7".into()),
    );
    let terminal = ws.active_terminal().unwrap();
    wait_until(|| terminal.current_dir() == Some("/tmp".into()));

    ws.open_terminal(None, None);
    ws.save();
    let state = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    let config = state.projects[0].worktree(&repo).expect("saved worktree");
    let last = config.panels.last().unwrap();
    match last {
        crate::state::PanelSpec::Terminal { cwd, .. } => {
            assert_eq!(cwd.as_deref(), Some(Path::new("/tmp")));
        }
        other => panic!("unexpected panel {other:?}"),
    }
}

#[test]
fn ctrl_click_on_a_url_opens_a_browser_panel_without_selecting() {
    use schnellui::scene::Point;
    use schnellui::{RawInputState, RawModifiers, RawPointerAction, RawPointerButton};

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(
        Some(vec![
            "sh".into(),
            "-c".into(),
            "printf 'https://example.com/x\\n'; sleep 30".into(),
        ]),
        Some("url".into()),
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("https://example.com/x")
    });
    let metrics = ws.borrow().metrics;
    let position = Point {
        x: metrics.width * 3.5,
        y: metrics.height * 0.5,
    };
    let click = |control: bool, state| {
        FocusedInputEvent::Pointer(schnellui::RawPointerEvent {
            position,
            window_position: position,
            modifiers: RawModifiers {
                control,
                ..RawModifiers::default()
            },
            action: RawPointerAction::Button {
                button: RawPointerButton::Left,
                state,
            },
        })
    };
    // A plain click opens nothing.
    app.dispatch_focused_input(click(false, RawInputState::Pressed));
    app.dispatch_focused_input(click(false, RawInputState::Released));
    assert!(ws.active_terminal().unwrap().take_link().is_none());

    let result = app.dispatch_focused_input(click(true, RawInputState::Pressed));
    assert!(result.is_handled());
    let titles: Vec<String> = ws.with_active(|live| {
        live.unwrap()
            .panels
            .iter()
            .map(|(_, p)| p.title())
            .collect()
    });
    assert!(
        titles.iter().any(|t| t.contains("example.com")),
        "{titles:?}"
    );
}

/// The saved tabs of `repo`, with panel indices in their panes, and the
/// shown tab.
fn saved_tabs(dir: &Path, repo: &Path, ws: &Workspace) -> (Vec<TabConfig>, usize) {
    let id = ws.borrow().state.active_project.unwrap();
    let state = StateStore::new(dir.join("cfg/state.json")).load().unwrap();
    let config = state.project(id).unwrap().worktree(repo).unwrap();
    (config.tabs.clone(), config.active_tab)
}

/// The panel ids shown in the panes of the shown tab, in pane order.
fn shown(ws: &Workspace) -> Vec<u64> {
    ws.with_active(|live| {
        let mut ids = Vec::new();
        live.unwrap().layout().for_each(&mut |id| ids.push(id));
        ids
    })
}

/// Number of tabs of the active worktree.
fn tab_count(ws: &Workspace) -> usize {
    ws.with_active(|live| live.unwrap().tabs.len())
}

fn panel_ids(ws: &Workspace) -> Vec<u64> {
    ws.with_active(|live| live.unwrap().panels.iter().map(|(id, _)| *id).collect())
}

fn first_terminal_cols(ws: &Workspace) -> Vec<u16> {
    ws.with_active(|live| {
        live.unwrap()
            .panels
            .iter()
            .filter_map(|(_, panel)| match panel {
                crate::workspace::LivePanel::Terminal(t) => Some(t.session.size().cols),
                _ => None,
            })
            .collect()
    })
}

#[test]
fn splitting_opens_a_shell_per_pane_nests_and_saves() {
    use crate::state::SplitAxis;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let full = first_terminal_cols(&ws)[0];
    let full_rows = ws.borrow().pane_grid().1;

    ws.split_pane(SplitAxis::SideBySide);
    assert_eq!(panel_ids(&ws).len(), 2);
    assert_eq!(shown(&ws), panel_ids(&ws));
    assert_eq!((tab_count(&ws), ws.focused_pane()), (1, 1));
    let mut expected = PaneLayout::pane(0);
    expected.split(0, SplitAxis::SideBySide, 1);
    let (tabs, active) = saved_tabs(dir.path(), &repo, &ws);
    assert_eq!((tabs.len(), active), (1, 0));
    assert_eq!((&tabs[0].layout, tabs[0].focused), (&expected, 1));
    for cols in first_terminal_cols(&ws) {
        assert!(
            cols < full * 6 / 10 && cols > full * 4 / 10,
            "{cols} of {full}"
        );
    }

    // Splitting the right pane stacks a third shell under it; the left pane
    // keeps its half of the width and the right ones get half the height.
    ws.split_pane(SplitAxis::Stacked);
    assert_eq!(panel_ids(&ws).len(), 3);
    assert_eq!(ws.focused_pane(), 2);
    let sizes = ws.borrow().pane_sizes.clone();
    assert_eq!(sizes.len(), 3);
    assert_eq!(sizes[1].width, sizes[2].width);
    assert!(sizes[0].height > sizes[1].height * 1.9, "{sizes:?}");
    assert!(ws.borrow().pane_grid().1 < full_rows * 6 / 10);
    expected.split(1, SplitAxis::Stacked, 2);
    assert_eq!(saved_tabs(dir.path(), &repo, &ws).0[0].layout, expected);

    // Growing the bottom pane moves the edge it shares with the pane above,
    // and dragging the line between the columns moves that one; both stick.
    ws.resize_pane(crate::keymap::PANE_STEP);
    let sizes = ws.borrow().pane_sizes.clone();
    assert!(sizes[2].height > sizes[1].height, "{sizes:?}");
    ws.set_pane_share(0, 300);
    let sizes = ws.borrow().pane_sizes.clone();
    assert!(sizes[0].width * 2.0 < sizes[1].width, "{sizes:?}");
    expected.resize(2, crate::keymap::PANE_STEP);
    expected.set_share(0, 300);
    assert_eq!(saved_tabs(dir.path(), &repo, &ws).0[0].layout, expected);

    // Closing a pane stops its shell and gives the room back.
    ws.close_pane();
    ws.focus_next_pane();
    assert_eq!(ws.focused_pane(), 0);
    ws.close_pane();
    assert_eq!(panel_ids(&ws).len(), 1);
    assert_eq!(shown(&ws).len(), 1);
    assert_eq!(tab_count(&ws), 1);
    let (tabs, _) = saved_tabs(dir.path(), &repo, &ws);
    assert_eq!(tabs[0].layout, PaneLayout::pane(0));
    assert_eq!(first_terminal_cols(&ws), [full]);
}

#[test]
fn each_tab_keeps_its_own_panes() {
    use crate::state::SplitAxis;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let full = first_terminal_cols(&ws)[0];
    ws.split_pane(SplitAxis::SideBySide);
    let first_tab = shown(&ws);

    // A new terminal is a new tab with one pane, at the full width, while
    // the split tab keeps both of its panes behind it.
    ws.open_terminal(Some(vec!["sh".into()]), Some("two".into()));
    assert_eq!(tab_count(&ws), 2);
    let second = shown(&ws);
    assert_eq!(second.len(), 1);
    assert_eq!(ws.focused_pane(), 0);
    let cols = |id: u64| {
        ws.with_active(|live| match &live.unwrap().panel(id).unwrap().1 {
            crate::workspace::LivePanel::Terminal(t) => t.session.size().cols,
            _ => unreachable!(),
        })
    };
    assert_eq!(cols(second[0]), full);
    assert!(
        cols(first_tab[0]) < full * 6 / 10,
        "hidden split keeps its size"
    );

    // Splitting the second tab leaves the first alone.
    ws.split_pane(SplitAxis::Stacked);
    assert_eq!(shown(&ws).len(), 2);
    ws.select_tab(0);
    assert_eq!(shown(&ws), first_tab);
    assert_eq!(ws.focused_pane(), 1);

    // Closing a tab stops every shell in it.
    ws.close_tab(1);
    assert_eq!(tab_count(&ws), 1);
    assert_eq!(panel_ids(&ws), first_tab);

    // Moving a pane out gives it a tab of its own after the shown one.
    ws.move_pane_to_tab();
    assert_eq!(tab_count(&ws), 2);
    assert_eq!(shown(&ws), [first_tab[1]]);
    ws.select_tab(0);
    assert_eq!(shown(&ws), [first_tab[0]]);
    let (tabs, active) = saved_tabs(dir.path(), &repo, &ws);
    assert_eq!((tabs.len(), active), (2, 0));

    // Closing the other tabs from the second tab's menu leaves it, shown.
    ws.close_other_tabs(1);
    assert_eq!(tab_count(&ws), 1);
    assert_eq!(panel_ids(&ws), [first_tab[1]]);
    assert_eq!(shown(&ws), [first_tab[1]]);
}

#[test]
fn splitting_stops_at_the_pane_limit() {
    use crate::state::SplitAxis;
    use crate::workspace::MAX_PANES;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    for _ in 1..MAX_PANES {
        ws.split_pane(SplitAxis::Stacked);
    }
    assert_eq!(shown(&ws).len(), MAX_PANES);
    ws.take_dirty();
    ws.split_pane(SplitAxis::Stacked);
    assert_eq!(shown(&ws).len(), MAX_PANES);
    assert_eq!(panel_ids(&ws).len(), MAX_PANES);
    assert!(ws.take_dirty().status);
}

#[test]
fn closing_keeps_the_layout_valid() {
    use crate::state::SplitAxis;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.split_pane(SplitAxis::SideBySide);
    ws.split_pane(SplitAxis::Stacked);
    let ids = panel_ids(&ws);
    assert_eq!(shown(&ws), ids);

    // Closing a panel removes its pane; the sibling takes over and focus
    // stays on the same panel.
    ws.close_panel(ids[1]);
    assert_eq!(shown(&ws), [ids[0], ids[2]]);
    assert_eq!(ws.focused_pane(), 1);
    assert_eq!(ws.pane_panel_id(1), Some(ids[2]));

    // The last panel of the last tab leaves an empty worktree.
    ws.close_pane();
    assert_eq!(shown(&ws), [ids[0]]);
    ws.close_panel(ids[0]);
    assert!(panel_ids(&ws).is_empty());
    assert_eq!(tab_count(&ws), 1);
    assert!(saved_tabs(dir.path(), &repo, &ws).0.is_empty());

    // A new terminal fills the empty pane instead of adding a tab.
    ws.open_terminal(None, None);
    assert_eq!((tab_count(&ws), shown(&ws).len()), (1, 1));
}

#[test]
fn a_saved_layout_comes_back_after_a_restart() {
    use crate::state::SplitAxis;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        crate::fit_grid(&ws, VIEWPORT);
        ws.split_pane(SplitAxis::Stacked);
        ws.split_pane(SplitAxis::SideBySide);
        ws.focus_pane(1);
        ws.open_terminal(None, None);
        ws.select_tab(0);
        ws.save();
    }
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    ws.with_active(|live| {
        let live = live.unwrap();
        assert_eq!(live.panels.len(), 4);
        assert_eq!((live.tabs.len(), live.active), (2, 0));
        assert_eq!(live.layout().len(), 3);
        assert_eq!(live.focused(), 1);
        assert_eq!(live.layout().span(SplitAxis::Stacked), 2);
    });
}

#[test]
fn text_in_a_later_pane_runs_in_that_pane_s_terminal() {
    use crate::state::SplitAxis;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.split_pane(SplitAxis::SideBySide);
    ws.split_pane(SplitAxis::Stacked);

    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    for name in ["Terminal", "Terminal (pane 2)"] {
        assert!(app.find_widget(Role::ScrollView, Some(name)).is_some());
    }
    let third = app
        .find_widget(Role::ScrollView, Some("Terminal (pane 3)"))
        .expect("third pane grid");
    app.focus(Some(third));
    let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(
        "printf 'marker-%s\\n' 43\r".into(),
    )));
    assert!(typed.is_handled());
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("marker-43")
    });
    // Only the third shell reads the input: exiting it leaves the others running.
    let terminals: Vec<_> = (0..3).map(|pane| ws.pane_terminal(pane).unwrap()).collect();
    app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit("exit\r".into())));
    wait_until(|| terminals[2].session.exit_code().is_some());
    assert!(
        terminals[..2]
            .iter()
            .all(|t| t.session.exit_code().is_none())
    );
    // The exited shell's pane closes on the next poll.
    ws.poll();
    assert!(ws.pane_terminal(2).is_none());
    assert_eq!(ws.with_active(|live| live.unwrap().layout().len()), 2);
    assert!(ws.borrow().dirty.main);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(
        app.find_widget(Role::ScrollView, Some("Terminal (pane 3)"))
            .is_none()
    );
}

#[test]
fn a_terminal_that_fails_to_start_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let before = panel_ids(&ws).len();
    ws.open_terminal(
        Some(vec![
            "sh".into(),
            "-c".into(),
            "echo 'execvp missing-program: No such file' >&2; exit 3".into(),
        ]),
        None,
    );
    let notice = "terminal exited right after it started (code 3): \
                  execvp missing-program: No such file";
    wait_until(|| {
        ws.poll();
        ws.borrow().notices.iter().any(|n| n.text == notice)
    });
    assert_eq!(panel_ids(&ws).len(), before);
}

#[test]
fn forge_details_show_in_the_sidebar_and_change_requests_open_browser_panels() {
    use crate::forge::{
        ChangeRequest, ChangeState, ForgeKind, ForgeRepo, Pipeline, PipelineStatus,
    };
    use schnellui::a11y::Role;
    use schnellui::template::DriveAction;

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let project = ws.borrow().state.active_project.unwrap();
    wait_until(|| {
        ws.poll();
        ws.borrow().snapshots.contains_key(&project)
    });

    let request = |number: u64, title: &str| ChangeRequest {
        number,
        title: title.to_owned(),
        author: "a".to_owned(),
        source_branch: "main".to_owned(),
        target_branch: "main".to_owned(),
        url: format!("https://gitlab.example.com/g/r/-/merge_requests/{number}"),
        draft: false,
        state: ChangeState::Open,
    };
    let mut snapshot = ws.borrow().snapshots[&project].clone();
    snapshot.forge = Some(ForgeRepo {
        kind: ForgeKind::GitLab,
        host: "gitlab.example.com".to_owned(),
        path: "g/r".to_owned(),
    });
    snapshot.change_requests = vec![request(12, "Fix the thing"), request(13, "Other")];
    snapshot.pipelines = snapshot
        .worktrees
        .iter()
        .map(|_| {
            Some(Pipeline {
                status: PipelineStatus::Failed,
                url: None,
            })
        })
        .collect();
    snapshot.forge_url = Some("https://gitlab.example.com/g/r".to_owned());

    crate::ui::TOKEN_OVERRIDE.with(|cell| cell.set(Some(false)));
    ws.apply_snapshot_for_test(project, snapshot.clone());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("ci failed"), "{json}");
    assert!(json.contains("Open gitlab.example.com"));
    assert!(json.contains("Set GITLAB_TOKEN to list change requests"));
    assert!(!json.contains("!12 Fix the thing"));

    crate::ui::TOKEN_OVERRIDE.with(|cell| cell.set(Some(true)));
    ws.apply_snapshot_for_test(project, snapshot);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("!12 Fix the thing"), "{json}");
    assert!(!json.contains("Set GITLAB_TOKEN"));

    assert!(app.drive_action(&DriveAction::click(Role::Button, "!12 Fix the thing")));
    let titles: Vec<String> = ws.with_active(|live| {
        live.unwrap()
            .panels
            .iter()
            .filter_map(|(_, p)| match p {
                crate::workspace::LivePanel::Browser(browser) => Some(browser.url()),
                _ => None,
            })
            .collect()
    });
    assert!(
        titles
            .iter()
            .any(|u| u == "https://gitlab.example.com/g/r/-/merge_requests/12"),
        "{titles:?}"
    );
}

/// A stand-in for `ssh`: skips the options and the destination, then runs the
/// final argument with `sh -c`, the way a remote login shell would.
#[cfg(unix)]
fn write_ssh_shim(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-ssh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-p|-i|-l|-F|-J|-L|-R|-D) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
shift
exec sh -c "$1"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_owned()
}

#[test]
#[cfg(unix)]
fn remote_project_works_end_to_end_through_an_ssh_shim() {
    use cmdterm_env::Environment;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let shim = write_ssh_shim(dir.path());
    let ws = workspace(dir.path());
    ws.set_ssh_program(Some(shim.clone()));

    ws.add_project(&format!("fakehost:{}", repo.display()));
    wait_until(|| {
        ws.poll();
        !ws.borrow().state.projects.is_empty()
    });
    {
        let inner = ws.borrow();
        let project = inner.state.active().unwrap();
        assert_eq!(project.env.label(), "fakehost");
        let Environment::Ssh(target) = &project.env else {
            panic!("expected ssh");
        };
        assert_eq!(target.program.as_deref(), Some(shim.as_str()));
        assert_eq!(project.root, repo);
    }
    // The worktree list arrives over the shim.
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .snapshots
            .values()
            .any(|s| s.worktrees.iter().any(|w| w.path == repo))
    });

    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(
        PWD_AND_MARKER.into(),
    )));
    assert!(typed.is_handled());
    let expected_dir = shell_dir(&repo);
    wait_until(|| {
        ws.poll();
        app.frame();
        let a11y = app.a11y_json();
        a11y.contains("marker-42") && a11y.contains(&expected_dir)
    });

    ws.add_worktree("remote/x");
    let feature = repo.with_file_name("app-remote-x");
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .snapshots
            .values()
            .any(|s| s.worktrees.iter().any(|w| w.path == feature))
    });
    assert!(feature.is_dir());
}

/// An SSH host gets a copy of the sandbox bridge once and keeps reusing
/// it. Its sandboxes, which keep the host's network, start under it, but
/// not under a copy that changed since.
#[test]
#[cfg(target_os = "linux")]
fn ssh_hosts_get_the_sandbox_bridge_once_and_never_run_a_changed_one() {
    use cmdterm_env::{Environment, HostBridge, SandboxKind, SandboxPolicy, SandboxTarget};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // The far side gets a home of its own, so nothing lands in the real
    // cache.
    let home = dir.path().join("far-home");
    std::fs::create_dir(&home).unwrap();
    let shim = write_ssh_shim(dir.path());
    let script = std::fs::read_to_string(&shim).unwrap().replace(
        "exec sh -c \"$1\"",
        &format!(
            "exec env -u XDG_CACHE_HOME HOME='{}' sh -c \"$1\"",
            home.display()
        ),
    );
    let shim = dir.path().join("home-ssh");
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let host: Environment = serde_json::from_value(serde_json::json!({
        "kind": "ssh", "destination": "fakehost", "program": shim,
    }))
    .unwrap();
    // A stand-in for the bridge that says when it runs.
    let local = dir.path().join("bin/cmdterm-proxy-bridge");
    std::fs::create_dir(local.parent().unwrap()).unwrap();
    std::fs::write(
        &local,
        "#!/bin/sh\n\
         [ \"$1\" = --cmdterm-proxy-bridge ] && shift\n\
         while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
         shift\n\
         echo bridge-ran >&2\n\
         exec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755)).unwrap();

    let copied = cmdterm_env::install_host_bridge(&host, &local).unwrap();
    let HostBridge::Copied(path) = copied else {
        panic!("expected a copy, got {copied:?}");
    };
    let cache = home.join(".cache/scriptschnell-term/bin");
    assert!(Path::new(&path).starts_with(&cache), "{path}");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(Path::new(&path)), 0o700);
    assert_eq!(mode(Path::new(&path).parent().unwrap()), 0o700);
    // Asked again, the host keeps its copy.
    assert_eq!(
        cmdterm_env::install_host_bridge(&host, &local),
        Ok(HostBridge::Reused(path.clone()))
    );

    if !bwrap_works() {
        eprintln!("bwrap cannot create namespaces here; the sandbox part is skipped");
        return;
    }
    let mut policy = SandboxPolicy::local(vec![], &[], true);
    policy.kind = SandboxKind::Bwrap;
    policy.host_network = true;
    let sandbox = Environment::Sandbox(Box::new(SandboxTarget::new(host.clone(), policy)));
    let run = || {
        let out = sandbox
            .command("sh")
            .args(["-c", "echo inside"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        (stdout, String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (stdout, stderr) = run();
    assert_eq!(stdout, "inside\n", "{stderr}");
    assert!(stderr.contains("bridge-ran"), "{stderr}");

    // A changed copy is not run; the sandbox starts without it.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, b"echo changed >&2\n").unwrap();
    drop(file);
    let (stdout, stderr) = run();
    assert_eq!(stdout, "inside\n", "{stderr}");
    assert!(!stderr.contains("bridge-ran"), "{stderr}");
    assert!(
        stderr.contains("changed since the app copied it"),
        "{stderr}"
    );

    // The next install replaces it.
    assert_eq!(
        cmdterm_env::install_host_bridge(&host, &local),
        Ok(HostBridge::Copied(path))
    );
    assert!(run().1.contains("bridge-ran"));
}

/// A worktree's variables reach the shell on an SSH host, not only the
/// local ssh process. This shim starts the far side with an empty
/// environment, as a real host would.
#[test]
#[cfg(unix)]
fn worktree_variables_reach_shells_on_ssh_hosts() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let shim = write_ssh_shim(dir.path());
    let script = std::fs::read_to_string(&shim).unwrap().replace(
        "exec sh -c \"$1\"",
        "exec env -i PATH=\"$PATH\" HOME=\"$HOME\" sh -c \"$1\"",
    );
    let shim = dir.path().join("clean-ssh");
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ws = workspace(dir.path());
    ws.set_ssh_program(Some(shim.to_str().unwrap().to_owned()));
    ws.add_project(&format!("fakehost:{}", repo.display()));
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .snapshots
            .values()
            .any(|s| s.worktrees.iter().any(|w| w.path == repo))
    });
    {
        let mut inner = ws.borrow_mut();
        let id = inner.state.active().unwrap().id;
        let config = inner.state.project_mut(id).unwrap().worktree_mut(&repo);
        config
            .env_vars
            .push(("CMDTERM_TEST_WORKTREE_VAR".into(), "far side".into()));
    }
    crate::fit_grid(&ws, VIEWPORT);
    let type_and_wait = |ws: &Workspace, marker: &str| {
        let mut app = crate::build_app(ws, VIEWPORT);
        app.frame();
        let line = format!("echo {marker}-\"$CMDTERM_TEST_WORKTREE_VAR\"-end\r");
        let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(line)));
        assert!(typed.is_handled());
        let expected = format!("{marker}-far side-end");
        wait_until(|| {
            ws.poll();
            app.frame();
            app.a11y_json().contains(&expected)
        });
    };
    // A command, as agents start.
    use_plain_sh(&ws);
    type_and_wait(&ws, "command");
    // The login shell.
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(None, None);
    type_and_wait(&ws, "shell");
}

fn ctrl_click(position: schnellui::scene::Point) -> FocusedInputEvent {
    use schnellui::{RawInputState, RawModifiers, RawPointerAction, RawPointerButton};
    FocusedInputEvent::Pointer(schnellui::RawPointerEvent {
        position,
        window_position: position,
        modifiers: RawModifiers {
            control: true,
            ..RawModifiers::default()
        },
        action: RawPointerAction::Button {
            button: RawPointerButton::Left,
            state: RawInputState::Pressed,
        },
    })
}

#[test]
fn ctrl_click_on_an_osc8_link_opens_its_target_and_selects_nothing() {
    use schnellui::scene::Point;
    use schnellui::{FocusedClipboardEvent, FocusedInputResult};

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(
        Some(vec![
            "sh".into(),
            "-c".into(),
            "printf '\\033]8;;https://target.example/page\\033\\\\click here\\033]8;;\\033\\\\\\n'; sleep 30"
                .into(),
        ]),
        Some("osc8".into()),
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("click here")
    });
    let metrics = ws.borrow().metrics;
    let position = Point {
        x: metrics.width * 3.5,
        y: metrics.height * 0.5,
    };
    let result = app.dispatch_focused_input(ctrl_click(position));
    assert!(result.is_handled());
    let titles: Vec<String> = ws.with_active(|live| {
        live.unwrap()
            .panels
            .iter()
            .map(|(_, p)| p.title())
            .collect()
    });
    assert!(
        titles.iter().any(|t| t.contains("target.example")),
        "{titles:?}"
    );
    // The click started no selection, so there is nothing to copy.
    let copy =
        app.dispatch_focused_input(FocusedInputEvent::Clipboard(FocusedClipboardEvent::Copy));
    assert_eq!(copy, FocusedInputResult::Ignored);
}

#[test]
fn ctrl_shift_c_copies_a_dragged_selection() {
    use schnellui::raw_keyboard::{Key, KeyCode, KeyLocation, NamedKey, PhysicalKey};
    use schnellui::scene::Point;
    use schnellui::{
        FocusedClipboardEvent, FocusedInputResult, RawInputState, RawKeyEvent, RawModifiers,
        RawPointerAction, RawPointerButton,
    };

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(
        Some(vec![
            "sh".into(),
            "-c".into(),
            "printf 'hello world\\n'; sleep 30".into(),
        ]),
        Some("copy".into()),
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("hello world")
    });
    let metrics = ws.borrow().metrics;
    let at = |column: f32| Point {
        x: metrics.width * (column + 0.5),
        y: metrics.height * 0.5,
    };
    let pointer = |position: Point, action: RawPointerAction| {
        FocusedInputEvent::Pointer(schnellui::RawPointerEvent {
            position,
            window_position: position,
            modifiers: RawModifiers::default(),
            action,
        })
    };
    let button = |state| RawPointerAction::Button {
        button: RawPointerButton::Left,
        state,
    };
    app.dispatch_focused_input(pointer(at(0.0), button(RawInputState::Pressed)));
    app.dispatch_focused_input(pointer(at(4.0), RawPointerAction::Move));
    app.dispatch_focused_input(pointer(at(4.0), button(RawInputState::Released)));

    // The window system sends Ctrl and Shift as keys before the chord.
    for (named, modifiers) in [
        (
            NamedKey::Control,
            RawModifiers {
                control: true,
                ..RawModifiers::default()
            },
        ),
        (
            NamedKey::Shift,
            RawModifiers {
                control: true,
                shift: true,
                ..RawModifiers::default()
            },
        ),
    ] {
        app.dispatch_focused_input(FocusedInputEvent::Key(RawKeyEvent {
            logical_key: Key::Named(named),
            physical_key: PhysicalKey::Code(KeyCode::ControlLeft),
            key_without_modifiers: Key::Named(named),
            location: KeyLocation::Left,
            modifiers,
            state: RawInputState::Pressed,
            repeat: false,
            text: None,
            text_with_all_modifiers: None,
        }));
    }
    let copy =
        app.dispatch_focused_input(FocusedInputEvent::Clipboard(FocusedClipboardEvent::Copy));
    assert_eq!(copy, FocusedInputResult::CopyText("hello".into()));
}

#[test]
fn clipboard_chords_never_reach_the_program_as_ctrl_c() {
    use schnellui::raw_keyboard::{Key, KeyCode, KeyLocation, PhysicalKey};
    use schnellui::{RawInputState, RawKeyEvent, RawModifiers};

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    ws.open_terminal(
        Some(vec![
            "sh".into(),
            "-c".into(),
            "stty -icanon -isig -echo; echo ready; \
             while :; do echo \"got$(dd bs=1 count=1 2>/dev/null | od -An -tx1)\"; done"
                .into(),
        ]),
        Some("bytes".into()),
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("ready")
    });
    // Copy found no selection, or paste found an empty clipboard, so the
    // window passes the chord on as a key, held down it repeats too.
    for letter in ["c", "v"] {
        let FocusedInputEvent::Key(mut key) = chord(letter) else {
            unreachable!()
        };
        app.dispatch_focused_input(FocusedInputEvent::Key(key.clone()));
        key.repeat = true;
        app.dispatch_focused_input(FocusedInputEvent::Key(key));
    }
    app.dispatch_focused_input(FocusedInputEvent::Key(RawKeyEvent {
        logical_key: Key::Character("c".into()),
        physical_key: PhysicalKey::Code(KeyCode::KeyC),
        key_without_modifiers: Key::Character("c".into()),
        location: KeyLocation::Standard,
        modifiers: RawModifiers {
            control: true,
            ..RawModifiers::default()
        },
        state: RawInputState::Pressed,
        repeat: false,
        text: None,
        text_with_all_modifiers: None,
    }));
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("got 03")
    });
    assert_eq!(app.a11y_json().matches("got").count(), 1);
}

#[test]
fn pasting_an_image_pastes_the_path_of_a_png_file() {
    use schnellui::FocusedClipboardEvent;
    use schnellui::platform::ClipboardImage;

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    let pasted = repo.join("pasted");
    // The pasted line names the file; the script copies it next to the test.
    ws.open_terminal(
        Some(vec![
            "sh".into(),
            "-c".into(),
            format!(
                "echo ready; read -r path; cp \"$path\" '{0}.tmp' && mv '{0}.tmp' '{0}'; sleep 30",
                pasted.display()
            ),
        ]),
        Some("paste".into()),
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains("ready")
    });
    let image = ClipboardImage {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 255, 255],
    };
    let result = app.dispatch_focused_input(FocusedInputEvent::Clipboard(
        FocusedClipboardEvent::PasteImage(image),
    ));
    assert!(result.is_handled());
    // A worker thread stages the file, then the path echoes.
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains(".png")
    });
    let terminal = ws.pane_terminal(0).unwrap();
    terminal.session.write_bytes(b"\r").unwrap();
    wait_until(|| pasted.exists());
    assert!(std::fs::read(&pasted).unwrap().starts_with(b"\x89PNG"));
}

fn chord(letter: &str) -> FocusedInputEvent {
    use schnellui::raw_keyboard::{Key, KeyCode, KeyLocation, PhysicalKey};
    use schnellui::{RawInputState, RawKeyEvent, RawModifiers};
    FocusedInputEvent::Key(RawKeyEvent {
        logical_key: Key::Character(letter.to_uppercase().into()),
        physical_key: PhysicalKey::Code(KeyCode::KeyD),
        key_without_modifiers: Key::Character(letter.into()),
        location: KeyLocation::Standard,
        modifiers: RawModifiers {
            control: true,
            shift: true,
            ..RawModifiers::default()
        },
        state: RawInputState::Pressed,
        repeat: false,
        text: None,
        text_with_all_modifiers: None,
    })
}

#[test]
fn app_chords_split_focus_and_close_through_the_focused_handler() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let panes = |ws: &Workspace| ws.with_active(|l| l.unwrap().layout().len());
    // Each chord changes the model; the app is rebuilt so the focused
    // handler is the one of the pane focused afterwards.
    let press = |ws: &Workspace, letter: &str| {
        let mut app = crate::build_app(ws, VIEWPORT);
        app.frame();
        assert!(app.dispatch_focused_input(chord(letter)).is_handled());
    };

    press(&ws, "d");
    assert_eq!((panes(&ws), ws.focused_pane()), (2, 1));
    press(&ws, "e");
    assert_eq!((panes(&ws), ws.focused_pane()), (3, 2));
    press(&ws, "o");
    assert_eq!(ws.focused_pane(), 0);

    // Closing the focused pane stops its shell.
    press(&ws, "x");
    assert_eq!(panes(&ws), 2);
    assert_eq!(ws.with_active(|l| l.unwrap().panels.len()), 2);

    // Ctrl+Shift+W closes the whole tab, leaving an empty worktree.
    press(&ws, "w");
    assert_eq!(ws.with_active(|l| l.unwrap().panels.len()), 0);
}

#[test]
fn a_focused_browser_pane_carries_the_focus_ref() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_browser("localhost:3000");
    assert!(ws.with_active(|l| matches!(
        l.unwrap().focused_panel().unwrap().1,
        crate::workspace::LivePanel::Browser(_)
    )));
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let replacement = schnellui::SubtreeReplacement::new(
        ws.regions().panel,
        crate::ui::panel(&ws, crate::ui::panel_size(&ws, VIEWPORT)),
        "panel",
    )
    .with_focus_after(ws.regions().focus);
    app.apply_subtree_replacement(replacement).unwrap();
    assert!(app.resolve_ref(ws.regions().focus).is_some());
}

#[test]
fn notices_expire_after_eight_seconds_and_touch_only_the_status_bar() {
    use crate::workspace::NOTICE_TTL;
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    ws.take_dirty();

    ws.notify("first");
    let dirty = ws.take_dirty();
    assert!(dirty.status);
    assert!(!dirty.sidebar && !dirty.main && !dirty.tabs && !dirty.full);

    // Not yet due: nothing changes.
    ws.expire_notices(Instant::now() + NOTICE_TTL - Duration::from_secs(2));
    assert_eq!(ws.borrow().notices.len(), 1);
    assert!(!ws.take_dirty().any());

    // A newer notice replaces the shown one and restarts the clock.
    ws.notify("second");
    assert_eq!(ws.borrow().notices.back().unwrap().text, "second");
    ws.take_dirty();
    ws.expire_notices(Instant::now() + NOTICE_TTL + Duration::from_secs(1));
    assert!(ws.borrow().notices.is_empty());
    let dirty = ws.take_dirty();
    assert!(dirty.status && !dirty.sidebar && !dirty.main);

    // Nothing to expire: no dirty flag.
    ws.expire_notices(Instant::now() + NOTICE_TTL * 2);
    assert!(!ws.take_dirty().any());
}

#[test]
fn the_status_bar_shows_environment_branch_grid_and_the_newest_notice() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    use_plain_sh(&ws);
    wait_until(|| {
        ws.poll();
        ws.borrow().active_branch() == Some("main")
    });
    crate::fit_grid(&ws, VIEWPORT);
    let (columns, rows) = ws.borrow().pane_grid();

    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Status bar"), "{json}");
    assert!(json.contains("\"local\""), "{json}");
    assert!(json.contains("\"main\""), "{json}");
    assert!(json.contains(&format!("{columns}×{rows}")), "{json}");
    assert!(!json.contains("a notice for the bar"));

    // The notice takes the room the parts leave; at 1000 px the CPU
    // mode leaves it a few characters.
    ws.notify("a notice for the bar");
    let wide = Size {
        width: 1200.0,
        ..VIEWPORT
    };
    let mut app = crate::build_app(&ws, wide);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("a notice for the bar"), "{json}");
}

#[test]
fn the_rail_uses_sentence_case_titles_and_named_rows() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    for title in ["Projects", "Open project", "Command palette"] {
        assert!(json.contains(&format!("\"{title}\"")), "{title}\n{json}");
    }
    for loud in [
        "PROJECTS",
        "WORKTREES",
        "CHANGE REQUESTS",
        "COMMANDS",
        "NOTICES",
    ] {
        assert!(!json.contains(loud), "{loud}");
    }
    use schnellui::a11y::Role;
    assert!(app.find_widget(Role::Button, Some("app")).is_some());
    assert!(app.find_widget(Role::Button, Some("main")).is_some());
    assert!(
        app.find_widget(Role::Button, Some("Open project"))
            .is_some()
    );
}

#[test]
fn the_rail_nests_every_projects_worktrees_under_it() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let app_repo = repo(dir.path());
    let lib = dir.path().join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    git(&lib, &["init", "-q", "-b", "trunk"]);
    std::fs::write(lib.join("README"), "hi\n").unwrap();
    git(&lib, &["add", "README"]);
    git(&lib, &["commit", "-qm", "init"]);
    let lib = cmdterm_env::canonicalize(&lib).unwrap();
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &app_repo);
        add_project_and_wait(&ws, &lib);
    }

    // After a restart only `lib` is watched; `app` still gets one refresh.
    let ws = workspace(dir.path());
    wait_until(|| {
        ws.poll();
        ws.borrow().snapshots.len() == 2
    });
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    for name in ["app", "main", "lib", "trunk"] {
        assert!(
            app.find_widget(Role::Button, Some(name)).is_some(),
            "{name}"
        );
    }

    let app_id = ws.borrow().state.projects[0].id;
    ws.activate_worktree(app_id, &app_repo);
    assert_eq!(ws.borrow().state.active_project, Some(app_id));
}

#[test]
fn each_worktree_takes_the_lane_color_of_its_position() {
    use crate::ui::active_lane;
    let lane_color = |index| crate::ui::SCHEMES[0].lane(index);
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    use_plain_sh(&ws);
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    assert_eq!(active_lane(&ws.borrow()), lane_color(0));

    ws.add_worktree("feature/x");
    let feature = repo.with_file_name("app-feature-x");
    wait_until(|| {
        ws.poll();
        let inner = ws.borrow();
        inner.state.active().unwrap().active_worktree.as_deref() == Some(&feature)
            && inner
                .snapshots
                .values()
                .any(|s| s.worktrees.iter().any(|w| w.path == feature))
    });
    let position = ws
        .borrow()
        .snapshots
        .values()
        .next()
        .unwrap()
        .worktrees
        .iter()
        .position(|w| w.path == feature)
        .unwrap();
    assert!(position > 0);
    assert_eq!(active_lane(&ws.borrow()), lane_color(position));

    let id = ws.borrow().state.active_project.unwrap();
    ws.activate_worktree(id, &repo);
    assert_eq!(active_lane(&ws.borrow()), lane_color(0));
}

#[test]
fn clicking_into_the_other_pane_marks_the_panel_dirty_once() {
    use crate::state::SplitAxis;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.split_pane(SplitAxis::SideBySide);
    ws.focus_next_pane();
    assert_eq!(ws.focused_pane(), 0);
    ws.take_dirty();

    ws.focus_pane(1);
    assert_eq!(ws.focused_pane(), 1);
    assert!(ws.take_dirty().main, "a changed focus rebuilds the panel");
    // The focus event after the rebuild repeats the same pane and must not loop.
    ws.focus_pane(1);
    assert!(!ws.take_dirty().main);
    // A pane that does not exist changes nothing.
    ws.focus_pane(5);
    assert_eq!(ws.focused_pane(), 1);
}

#[test]
fn status_bar_follows_the_directory_the_shell_reports() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    ws.take_dirty();
    let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(
        "printf '\\033]7;file://host/osc-moved-here\\007'\r".into(),
    )));
    assert!(typed.is_handled());
    wait_until(|| {
        ws.poll();
        crate::ui::status_parts(&ws)
            .iter()
            .any(|part| part == "/osc-moved-here")
    });
    // The directory can change between the poll and the check above.
    ws.poll();
    assert!(ws.take_dirty().status, "the status bar refreshes alone");
}

#[test]
fn the_status_bar_switches_the_cpu_mode_where_the_project_runs() {
    use crate::state::CpuMode;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let parts = crate::ui::status_parts(&ws);
    assert!(parts.iter().any(|part| part == "CPU balanced"), "{parts:?}");
    ws.take_dirty();

    let en = &crate::i18n::EN;
    let hint = (en.cpu_status_hint)("CPU balanced", en.cpu_quiet);
    let switch = app.find_widget(Role::Button, Some(&hint)).unwrap();
    app.dispatch_click(switch);
    assert_eq!(ws.borrow().settings.cpu_mode.local, CpuMode::Quiet);
    assert_eq!(ws.borrow().settings.cpu_mode.remote, CpuMode::Off);
    assert!(ws.take_dirty().status);
    let parts = crate::ui::status_parts(&ws);
    assert!(parts.iter().any(|part| part == "CPU quiet"), "{parts:?}");
}

#[test]
fn notices_pushed_internally_also_expire() {
    use crate::workspace::NOTICE_TTL;
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    ws.take_dirty();
    ws.push_internal_notice("terminal failed: boom");
    assert!(ws.take_dirty().status);
    ws.expire_notices(Instant::now() + NOTICE_TTL + Duration::from_secs(1));
    assert!(ws.borrow().notices.is_empty());
}

#[test]
fn a_panel_rebuild_does_not_steal_focus_from_the_open_palette() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    use_plain_sh(&ws);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_palette(None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(palette_input_is_focused(&app));
    ws.take_dirty();
    ws.open_terminal(None, None);
    assert!(ws.take_dirty().main);
    app.apply_subtree_replacement(crate::panel_replacement(&ws, VIEWPORT))
        .unwrap();
    assert!(palette_input_is_focused(&app));
}

#[test]
fn the_closed_palette_takes_no_pointer_hits() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    ws.open_terminal(None, None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let terminal = app.find_widget(Role::ScrollView, Some(crate::TERMINAL_NAMES[0]));
    let tab = app.find_widget(Role::Tab, None);
    for widget in [terminal, tab] {
        let rect = app.scene().layout(widget.unwrap()).unwrap().rect;
        let center = schnellui::scene::Point {
            x: rect.x + rect.width / 2.0,
            y: rect.y + rect.height / 2.0,
        };
        assert_eq!(app.hit_test(center), widget);
    }
}

#[test]
fn the_rail_settings_button_and_ctrl_comma_open_the_settings() {
    use schnellui::FocusedInputResult;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, mut app) = palette_fixture(dir.path());
    let button = app.find_widget(Role::Button, Some("Settings")).unwrap();
    app.dispatch_click(button);
    assert!(ws.settings_is_open());
    assert!(ws.take_dirty().settings);

    // Rebuilt with the overlay open, focus lands in it, not in the terminal.
    let mut app = crate::build_app(&ws, VIEWPORT);
    let focused = app.focused_widget();
    assert!(focused.is_some());
    assert_eq!(focused, app.resolve_ref(ws.regions().settings_focus));
    let close = app
        .find_widget(Role::Button, Some("Close settings"))
        .unwrap();
    app.dispatch_click(close);
    assert!(!ws.settings_is_open());

    let mut app = crate::build_app(&ws, VIEWPORT);
    let mut comma = chord("p");
    if let FocusedInputEvent::Key(key) = &mut comma {
        key.modifiers.shift = false;
        key.key_without_modifiers = schnellui::raw_keyboard::Key::Character(",".into());
        key.logical_key = schnellui::raw_keyboard::Key::Character(",".into());
    }
    assert_eq!(
        app.dispatch_focused_input(comma),
        FocusedInputResult::Handled
    );
    assert!(ws.settings_is_open());
}

#[test]
fn picking_a_section_rebuilds_only_the_overlay() {
    use crate::ui::SettingsSection;
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    ws.open_settings();
    ws.take_dirty();
    ws.show_settings_section(SettingsSection::Extensions);
    let dirty = ws.take_dirty();
    assert!(dirty.settings);
    assert!(!dirty.full && !dirty.main && !dirty.sidebar);
    ws.show_settings_section(SettingsSection::Extensions);
    assert!(!ws.take_dirty().any());
}

#[test]
fn font_size_changes_the_cell_saves_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    let before = ws.borrow().metrics;
    ws.take_dirty();
    ws.step_font_size(4.0);
    assert!(ws.take_dirty().full);
    let after = ws.borrow().metrics;
    assert!(after.width > before.width && after.height > before.height);
    assert_eq!(ws.borrow().settings.font_size, 18.0);
    drop(ws);

    let again = workspace(dir.path());
    assert_eq!(again.borrow().settings.font_size, 18.0);
    assert_eq!(again.borrow().metrics, after);

    // The command-line override does not touch the saved value.
    again.override_font_size(9.0);
    drop(again);
    assert_eq!(workspace(dir.path()).borrow().settings.font_size, 18.0);
}

#[test]
fn the_color_scheme_follows_the_setting_and_the_desktop() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    use_plain_sh(&ws);
    assert_eq!(ws.borrow().scheme.id, "spruce");
    ws.take_dirty();

    // System mode: a desktop switch to light swaps the scheme on the next poll.
    ws.set_system_dark(false);
    ws.poll();
    assert_eq!(ws.borrow().scheme.id, "spruce-light");
    assert!(ws.take_dirty().full);
    let terminal = ws.active_terminal().unwrap();
    let background = terminal.session.lock().palette().bg;
    let (r, g, b) = (background.r, background.g, background.b);
    let page = crate::ui::SCHEMES[1].page;
    assert_eq!((r, g, b), (page.r, page.g, page.b));

    // A fixed scheme ignores the desktop and is saved.
    ws.set_color_scheme("nord");
    assert_eq!(ws.borrow().scheme.id, "nord");
    ws.set_system_dark(true);
    ws.take_dirty();
    ws.poll();
    assert!(!ws.take_dirty().full);
    drop(ws);
    assert_eq!(workspace(dir.path()).borrow().scheme.id, "nord");
}

#[test]
fn turning_an_extension_off_unloads_it_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let extensions = dir.path().join("cfg/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(
        extensions.join("hello.lua"),
        "cmdterm.command{ name = 'hello', title = 'Say hello', run = function() end }",
    )
    .unwrap();
    let ws = workspace(dir.path());
    assert_eq!(ws.borrow().ext.commands().len(), 1);

    ws.set_extension_enabled("hello", false);
    assert!(ws.borrow().ext.commands().is_empty());
    let info: Vec<_> = ws
        .borrow()
        .ext
        .extensions()
        .map(|ext| (ext.name.clone(), ext.turned_off))
        .collect();
    assert_eq!(info, [("hello".to_owned(), true)]);
    drop(ws);

    let again = workspace(dir.path());
    assert!(again.borrow().ext.commands().is_empty());
    again.set_extension_enabled("hello", true);
    assert_eq!(again.borrow().ext.commands().len(), 1);
}

#[test]
fn the_worktree_plus_button_adds_a_detached_or_branched_worktree_from_that_row() {
    use crate::ui::{InputMode, PALETTE_BRANCH_INPUT};
    use schnellui::UiKey;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    let id = ws.borrow().state.active_project.unwrap();
    wait_until(|| {
        ws.poll();
        ws.borrow().snapshots.contains_key(&id)
    });
    let main = ws.borrow().snapshots[&id].worktrees[0].clone();
    let worktree = |path: &Path| {
        let inner = ws.borrow();
        inner.snapshots[&id]
            .worktrees
            .iter()
            .find(|w| w.path == path)
            .cloned()
    };

    // Empty branch: detached at main's commit.
    let mut app = crate::build_app(&ws, VIEWPORT);
    let plus = app
        .find_widget(Role::Button, Some("New worktree from main"))
        .unwrap();
    app.dispatch_click(plus);
    assert_eq!(ws.borrow().palette.input, Some(InputMode::WorktreeFrom));
    ws.set_palette_query("loose");
    ws.palette_enter();
    assert!(!ws.palette_is_open());
    let loose = main.path.with_file_name("app-loose");
    wait_until(|| {
        ws.poll();
        worktree(&loose).is_some()
    });
    let detached = worktree(&loose).unwrap();
    assert_eq!((detached.branch, detached.head), (None, main.head.clone()));

    // A branch typed into the second input; Enter there submits.
    ws.open_new_worktree_from(id, &loose);
    ws.take_dirty();
    let mut app = crate::build_app(&ws, VIEWPORT);
    ws.set_palette_query("topic");
    let branch = app
        .find_widget(Role::TextInput, Some(PALETTE_BRANCH_INPUT))
        .unwrap();
    // The name input has focus; Tab moves to the branch input.
    assert!(palette_input_is_focused(&app));
    assert!(app.dispatch_key(UiKey::Tab { shift: false }));
    assert_eq!(app.focused_widget(), Some(branch));
    ws.set_palette_branch("feature/topic");
    assert!(app.dispatch_key(UiKey::Enter));
    let topic = main.path.with_file_name("app-topic");
    wait_until(|| {
        ws.poll();
        worktree(&topic).is_some_and(|w| w.branch.as_deref() == Some("feature/topic"))
    });
    assert_eq!(worktree(&topic).unwrap().head, main.head);

    // Escape closes instead of falling back to the action list.
    ws.open_new_worktree_from(id, &main.path);
    ws.palette_escape();
    assert!(!ws.palette_is_open());
    ws.open_new_worktree_from(id, &main.path);
    ws.set_palette_query("a/b");
    // A name with a slash says why under the field; Enter keeps it open.
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(app.a11y_json().contains("A folder name: no /"));
    ws.palette_enter();
    assert!(ws.palette_is_open());
}

/// Drags the tab named `tab` to the point at the given fractions of the
/// widget `target` (role and name) and releases it there.
fn drag_tab(
    app: &mut schnellui::App,
    tab: &str,
    target: (schnellui::a11y::Role, &str),
    x: f32,
    y: f32,
) {
    drag_onto(app, (schnellui::a11y::Role::Tab, tab), target, x, y);
}

/// Drags `source` by its middle onto `target` at fractions `x`, `y` of its
/// box and asserts the target took it.
fn drag_onto(
    app: &mut schnellui::App,
    source: (schnellui::a11y::Role, &str),
    target: (schnellui::a11y::Role, &str),
    x: f32,
    y: f32,
) {
    use schnellui::a11y::Role;
    let point = |app: &schnellui::App, role: Role, name: &str, x: f32, y: f32| {
        let id = app
            .find_widget(role, Some(name))
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"));
        let rect = app.scene().layout(id).unwrap().rect;
        schnellui::scene::Point {
            x: rect.x + rect.width * x,
            y: rect.y + rect.height * y,
        }
    };
    let tab = source.1;
    let from = point(app, source.0, source.1, 0.3, 0.5);
    let to = point(app, target.0, target.1, x, y);
    assert!(app.begin_drag(from), "{tab} did not start a drag");
    assert!(app.update_drag(to), "{target:?} is no drop target");
    assert!(
        matches!(
            app.end_drag(to),
            schnellui::widgets::DragRelease::Drop { accepted: true }
        ),
        "{target:?} did not accept {tab}"
    );
}

#[test]
fn dragging_a_tab_onto_a_pane_edge_joins_it_to_the_shown_tab() {
    use crate::state::SplitAxis;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.open_terminal(Some(vec!["sh".into()]), Some("two".into()));
    let ids = panel_ids(&ws);
    let mounted = |ws: &Workspace| {
        let mut app = crate::build_app(ws, VIEWPORT);
        app.frame();
        app
    };

    // The other tab dropped on the right edge becomes a pane beside the
    // shown one, and its tab goes away.
    let mut app = mounted(&ws);
    drag_tab(&mut app, "sh", (Role::Group, "Pane 1"), 0.95, 0.5);
    assert_eq!(shown(&ws), [ids[1], ids[0]]);
    assert_eq!((tab_count(&ws), ws.focused_pane()), (1, 1));
    let horizontal = ws.with_active(|l| l.unwrap().layout().span(SplitAxis::SideBySide));
    assert_eq!(horizontal, 2);
    let (saved, _) = saved_tabs(dir.path(), &repo, &ws);
    assert_eq!(saved[0].layout.span(SplitAxis::SideBySide), 2);

    // The shown tab over its own panes finds no target: the drop would
    // change nothing, so none is offered.
    let app = mounted(&ws);
    let tab = app.find_widget(Role::Tab, Some("sh · 2 panes")).unwrap();
    let pane = app.find_widget(Role::Group, Some("Pane 1")).unwrap();
    let (from, to) = (
        app.scene().layout(tab).unwrap().rect,
        app.scene().layout(pane).unwrap().rect,
    );
    let mut app = app;
    let from = schnellui::scene::Point {
        x: from.x + from.width * 0.3,
        y: from.y + from.height * 0.5,
    };
    let to = schnellui::scene::Point {
        x: to.x + to.width * 0.05,
        y: to.y + to.height * 0.5,
    };
    assert!(app.begin_drag(from));
    let _ = app.update_drag(to);
    // Unaccepted, the tab's release is a click on it, as off any target.
    assert!(matches!(
        app.end_drag(to),
        schnellui::widgets::DragRelease::Click(_)
    ));
    ws.end_tab_drag();
    assert_eq!(shown(&ws), [ids[1], ids[0]]);

    // The middle of a pane takes no tab: there is nothing to swap it with.
    ws.move_pane_to_tab();
    ws.select_tab(0);
    let mut app = mounted(&ws);
    let tab = app.find_widget(Role::Tab, Some("sh")).unwrap();
    let pane = app.find_widget(Role::Group, Some("Pane 1")).unwrap();
    let (from, to) = (
        app.scene().layout(tab).unwrap().rect,
        app.scene().layout(pane).unwrap().rect,
    );
    assert!(app.begin_drag(schnellui::scene::Point {
        x: from.x + from.width * 0.3,
        y: from.y + from.height * 0.5,
    }));
    let middle = schnellui::scene::Point {
        x: to.x + to.width * 0.5,
        y: to.y + to.height * 0.5,
    };
    let _ = app.update_drag(middle);
    assert!(!matches!(
        app.end_drag(middle),
        schnellui::widgets::DragRelease::Drop { accepted: true }
    ));
    ws.end_tab_drag();
    assert_eq!(shown(&ws), [ids[1]]);
    assert_eq!(tab_count(&ws), 2);
}

#[test]
fn a_pane_dragged_by_its_title_moves_swaps_or_becomes_a_tab() {
    use crate::state::SplitAxis;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.split_pane(SplitAxis::SideBySide);
    ws.split_pane(SplitAxis::SideBySide);
    let ids = panel_ids(&ws);
    assert_eq!(shown(&ws).len(), 3);
    let mounted = |ws: &Workspace| {
        let mut app = crate::build_app(ws, VIEWPORT);
        app.frame();
        app
    };
    let before = shown(&ws);

    // The middle of another pane swaps the two.
    let mut app = mounted(&ws);
    drag_onto(
        &mut app,
        (Role::Group, "Drag to move pane 1"),
        (Role::Group, "Pane 3"),
        0.5,
        0.5,
    );
    assert_eq!(shown(&ws), [before[2], before[1], before[0]]);
    assert_eq!(ws.focused_pane(), 2);

    // An edge moves it there: pane 1 under pane 3.
    let mut app = mounted(&ws);
    drag_onto(
        &mut app,
        (Role::Group, "Drag to move pane 1"),
        (Role::Group, "Pane 3"),
        0.5,
        0.95,
    );
    assert_eq!(shown(&ws), [before[1], before[0], before[2]]);

    // The new tab button takes it out into a tab of its own.
    let mut app = mounted(&ws);
    drag_onto(
        &mut app,
        (Role::Group, "Drag to move pane 1"),
        (Role::Button, "New panel"),
        0.5,
        0.5,
    );
    assert_eq!(tab_count(&ws), 2);
    assert_eq!(shown(&ws), [before[1]]);
    assert_eq!(ids.len(), 3);
}

#[test]
fn dragging_a_tab_along_the_strip_reorders_it() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    use_plain_sh(&ws);
    ws.open_terminal(Some(vec!["sh".into()]), Some("two".into()));
    let ids = panel_ids(&ws);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    drag_tab(&mut app, "sh", (Role::Tab, "two"), 0.75, 0.5);
    let order = ws.with_active(|l| {
        let live = l.unwrap();
        let firsts: Vec<_> = live
            .tabs
            .iter()
            .map(|tab| tab.layout.get(0).unwrap())
            .collect();
        (firsts, live.active)
    });
    assert_eq!(
        order,
        (vec![ids[1], ids[0]], 0),
        "the shown tab moves along"
    );
    assert!(ws.take_dirty().tabs);
}

#[test]
fn a_waiting_agent_marks_its_worktree_until_the_user_answers() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    // What the agent-state helper writes for "permission:Bash wants to run".
    let script = "printf '\\033]1337;SetUserVar=cmdterm_agent=%s\\007' \
                  cGVybWlzc2lvbjpCYXNoIHdhbnRzIHRvIHJ1bg==; read line; sleep 30";
    ws.open_terminal(
        Some(vec!["sh".into(), "-c".into(), script.into()]),
        Some("agent".into()),
    );

    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        app.a11y_json()
            .contains("An agent waits for permission (Bash wants to run)")
    });
    let json = app.a11y_json();
    assert!(json.contains("● agent"), "the tab is marked too: {json}");
    assert!(
        json.contains("An agent waits for permission (1 worktrees)")
            || json.contains("\"An agent waits for permission\""),
        "the project row says so too: {json}"
    );

    let typed =
        app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit("y\r".into())));
    assert!(typed.is_handled());
    wait_until(|| {
        ws.poll();
        app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        !app.a11y_json().contains("An agent waits for permission")
    });
}

#[test]
fn the_rail_search_finds_older_agent_sessions_by_their_titles() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    {
        let mut inner = ws.borrow_mut();
        let id = inner.state.projects[0].id;
        let config = inner.state.project_mut(id).unwrap().worktree_mut(&repo);
        let agent = crate::state::AgentSession {
            agent: "claude".into(),
            session: Some("3f2a-11".into()),
            working: false,
        };
        config.note_agent_session(&agent, Some("Migrate the billing exporter"), 1);
        // Newer sessions pushed it out of the worktree's titles.
        for n in 0..crate::state::MAX_AGENT_TITLES {
            config.note_agent_title(&format!("task {n}"));
        }
    }
    let hits = crate::search::rail_hits(&ws.borrow(), "billing exporter");
    assert_eq!(
        hits.iter()
            .map(|hit| hit.path.as_path())
            .collect::<Vec<_>>(),
        [repo.as_path()]
    );
}

#[test]
fn an_agent_session_names_its_worktree_and_the_rail_search_finds_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    // A shell title before the session does not count; then the hooks
    // report busy and the agent titles its task behind a spinner.
    let script = "printf '\\033]0;user@host\\007'; \
                  printf '\\033]1337;SetUserVar=cmdterm_agent=%s\\007' YnVzeQ==; \
                  printf '\\033]0;\\342\\234\\263 Fix the broken sign-in form\\007'; sleep 30";
    ws.open_terminal(
        Some(vec!["sh".into(), "-c".into(), script.into()]),
        Some("agent".into()),
    );
    let title = "\"Fix the broken sign-in form\"";
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_until(|| {
        ws.poll();
        app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        app.a11y_json().contains(title)
    });
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    let config = saved.projects[0].worktree(&repo).unwrap();
    assert_eq!(config.agent_titles, ["Fix the broken sign-in form"]);

    // No word of the query is in the title or the branch.
    ws.set_rail_query("auth bug".into());
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains(title), "{json}");
    ws.set_rail_query("kubernetes".into());
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(
        json.contains("No worktree matches") && !json.contains(title),
        "{json}"
    );
    ws.clear_rail_search();
    assert!(ws.borrow().rail_query.is_empty());

    ws.set_worktree_titles(crate::state::WorktreeTitles::Branch);
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(!app.a11y_json().contains(title));
    let settings = SettingsStore::new(dir.path().join("cfg/settings.json"))
        .load()
        .unwrap();
    assert_eq!(
        settings.worktree_titles,
        crate::state::WorktreeTitles::Branch
    );
}

#[test]
fn jj_is_opt_in_per_user_and_per_project() {
    if std::process::Command::new("jj")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("jj not installed; skipping");
        return;
    }
    use crate::vcs::VcsKind;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let status = std::process::Command::new("jj")
        .args(["git", "init", "--colocate", "--quiet"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(status.success());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let id = ws.borrow().state.projects[0].id;
    let kind = |ws: &Workspace| ws.borrow().state.project(id).unwrap().vcs;
    let wait_for = |kind_now: VcsKind| {
        wait_until(|| {
            ws.poll();
            kind(&ws) == Some(kind_now)
        })
    };
    // Colocated, so git until jj is turned on.
    assert_eq!(kind(&ws), Some(VcsKind::Git));

    ws.set_project_jj(id, true);
    wait_for(VcsKind::Jujutsu);
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    assert_eq!(saved.projects[0].jj, Some(true));

    // Turning it on for everyone makes the project's own choice redundant.
    ws.set_jj(true);
    ws.set_project_jj(id, false);
    wait_for(VcsKind::Git);
    assert_eq!(ws.borrow().state.project(id).unwrap().jj, Some(false));
    ws.set_project_jj(id, true);
    wait_for(VcsKind::Jujutsu);
    assert_eq!(ws.borrow().state.project(id).unwrap().jj, None);
    let settings = SettingsStore::new(dir.path().join("cfg/settings.json"))
        .load()
        .unwrap();
    assert!(settings.jj);
}

#[test]
fn escape_in_a_filled_rail_search_clears_it_before_leaving() {
    let dir = tempfile::tempdir().unwrap();
    let (ws, _app) = palette_fixture(dir.path());
    ws.set_rail_query("zzz".into());
    ws.escape_rail_search();
    assert!(ws.borrow().rail_query.is_empty());
    assert!(ws.borrow().rail_search_focus);
    ws.escape_rail_search();
    assert!(!ws.borrow().rail_search_focus);
}

#[test]
fn the_search_shortcut_focuses_the_rail_search_and_escape_gives_it_back() {
    use schnellui::FocusedInputResult;
    let dir = tempfile::tempdir().unwrap();
    let (ws, mut app) = palette_fixture(dir.path());
    assert_eq!(
        app.dispatch_focused_input(chord("s")),
        FocusedInputResult::Handled
    );
    let dirty = ws.take_dirty();
    assert!(dirty.search && ws.borrow().rail_search_focus);
    // The branch name comes with the worker's first snapshot.
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    ws.set_rail_query("main".into());
    ws.open_rail_highlight();
    assert!(ws.borrow().rail_query.is_empty());
    assert!(!ws.borrow().rail_search_focus);
}

fn bwrap_works() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--unshare-all", "true"])
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn sandboxed_worktrees_write_only_the_worktree() {
    if !bwrap_works() {
        eprintln!("bwrap cannot create namespaces here; skipped");
        return;
    }
    // Not under $TMPDIR: when it points into a cache directory the sandbox
    // keeps writable, such as ~/.cache, `outside` would be writable too.
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let dir = tempfile::tempdir_in(&scratch).unwrap();
    let repo = repo(dir.path());
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    granted_extension(
        dir.path(),
        "writer",
        &["run"],
        &format!(
            r#"cmdterm.command{{ name = "write", run = function()
                cmdterm.run{{ cmd = {{ "sh", "-c", "echo in > inside; echo h > \"$HISTFILE\"; echo out > {}/x" }} }}
            end }}"#,
            outside.display()
        ),
    );
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    ws.set_runtime(crate::state::Runtime::Sandbox);
    assert_eq!(ws.borrow().active_runtime(), crate::state::Runtime::Sandbox);
    assert_eq!(
        ws.borrow().ext_context_for_tests().runtime,
        "sandbox",
        "extensions see the runtime"
    );
    ws.run_command("write");
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .notices
            .iter()
            .any(|n| n.text.starts_with("command failed"))
    });
    assert_eq!(
        std::fs::read_to_string(repo.join("inside")).unwrap(),
        "in\n"
    );
    assert!(!outside.join("x").exists());

    // The runtime is saved with the worktree.
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    let config = saved.projects[0].worktree(&repo).unwrap();
    assert_eq!(config.runtime, crate::state::Runtime::Sandbox);

    // Shell history goes to the project's own directory under the config.
    let shells = dir
        .path()
        .join("cfg/sandbox")
        .join(saved.projects[0].id.to_string());
    assert_eq!(
        std::fs::read_to_string(shells.join("history")).unwrap(),
        "h\n"
    );
    ws.remove_project(saved.projects[0].id);
    assert!(!shells.exists(), "removing the project deletes its history");
}

#[test]
fn extensions_copy_files_into_the_terminal_environment() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let source = dir.path().join("dotfiles/tool");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("config"), "x=1").unwrap();
    std::fs::write(source.join("debug.log"), "noise").unwrap();
    let dest = dir.path().join("home/.config");
    granted_extension(
        dir.path(),
        "copier",
        &["copy"],
        &format!(
            // Quoted with escapes, so Windows' backslashes stay in the Lua
            // string.
            r#"cmdterm.command{{ name = "copy", run = function()
                cmdterm.copy{{ from = {:?}, to = {:?}, exclude = {{ "*.log" }} }}
            end }}"#,
            source.display().to_string(),
            dest.display().to_string()
        ),
    );
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    // A sandbox could not write the destination outside the worktree.
    ws.set_runtime(crate::state::Runtime::Host);
    ws.run_command("copy");
    wait_until(|| {
        ws.poll();
        dest.join("tool/config").exists()
    });
    assert_eq!(
        std::fs::read_to_string(dest.join("tool/config")).unwrap(),
        "x=1"
    );
    assert!(!dest.join("tool/debug.log").exists());
}

#[test]
fn new_worktrees_take_the_default_runtime() {
    use crate::state::Runtime;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    ws.set_default_runtime(Runtime::Host);
    add_project_and_wait(&ws, &repo);
    assert_eq!(ws.borrow().active_runtime(), Runtime::Host);

    // A devcontainer default needs a devcontainer.json.
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    ws.set_default_runtime(Runtime::Devcontainer);
    add_project_and_wait(&ws, &other);
    assert_eq!(ws.borrow().active_runtime(), Runtime::Sandbox);
}

#[test]
fn devcontainer_worktrees_run_terminals_in_the_container() {
    let podman_works = std::process::Command::new("podman")
        .args(["image", "exists", "docker.io/library/alpine:3.20"])
        .status()
        .is_ok_and(|status| status.success());
    if !podman_works {
        eprintln!("podman or the alpine:3.20 image is missing; skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    std::fs::create_dir_all(repo.join(".devcontainer")).unwrap();
    std::fs::write(
        repo.join(".devcontainer/devcontainer.json"),
        r#"{
            // JSONC, as editors write it.
            "image": "docker.io/library/alpine:3.20",
            "remoteEnv": { "INSIDE": "yes" },
        }"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
    std::fs::write(
        dir.path().join("cfg/settings.json"),
        r#"{"container_engine": "podman"}"#,
    )
    .unwrap();
    granted_extension(
        dir.path(),
        "probe",
        &["run"],
        r#"cmdterm.on("environment_ready", function(e)
            cmdterm.notify("ready " .. e.kind .. " " .. tostring(e.fresh))
            cmdterm.run{ cmd = { "sh", "-c", "cat /etc/alpine-release > from-container; echo $INSIDE >> from-container" },
                         worktree = e.worktree }
        end)"#,
    );
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    ws.set_runtime(crate::state::Runtime::Devcontainer);
    let deadline = Instant::now() + Duration::from_secs(120);
    while !repo.join("from-container").exists() {
        ws.poll();
        let failed = ws
            .borrow()
            .notices
            .iter()
            .find(|n| n.text.contains("failed"))
            .map(|n| n.text.clone());
        if let Some(failed) = failed {
            let _ =
                crate::container::remove(&cmdterm_env::Environment::Local, &repo, Some("podman"));
            panic!("{failed}");
        }
        assert!(Instant::now() < deadline, "devcontainer did not come up");
        std::thread::sleep(Duration::from_millis(50));
    }
    wait_until(|| {
        ws.poll();
        std::fs::read_to_string(repo.join("from-container")).is_ok_and(|t| t.ends_with("yes\n"))
    });
    assert!(
        ws.borrow()
            .notices
            .iter()
            .any(|n| n.text == "ready devcontainer true")
    );
    crate::container::remove(&cmdterm_env::Environment::Local, &repo, Some("podman")).unwrap();
}

fn named_key(key: schnellui::raw_keyboard::NamedKey) -> FocusedInputEvent {
    use schnellui::raw_keyboard::{Key, KeyCode, KeyLocation, PhysicalKey};
    use schnellui::{RawInputState, RawKeyEvent, RawModifiers};
    FocusedInputEvent::Key(RawKeyEvent {
        logical_key: Key::Named(key),
        physical_key: PhysicalKey::Code(KeyCode::Enter),
        key_without_modifiers: Key::Named(key),
        location: KeyLocation::Standard,
        modifiers: RawModifiers::default(),
        state: RawInputState::Pressed,
        repeat: false,
        text: None,
        text_with_all_modifiers: None,
    })
}

/// A repository with a nested Rust file next to the README.
fn files_repo(root: &Path) -> std::path::PathBuf {
    let repo = repo(root);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// Doc.\npub fn answer() -> u32 {\n    42 // the answer\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("notes.bin"), b"\0\x01\x02").unwrap();
    git(&repo, &["add", "src/lib.rs"]);
    repo
}

/// Pumps the workspace and the app until the accessibility tree has `text`.
/// Like [`wait_for_text`], and mounts the app again when the main region
/// asks for it, as the window does: the tasks page is widgets.
fn wait_for_mounted_text(ws: &Workspace, app: &mut schnellui::App, text: &str) {
    wait_until(|| {
        ws.poll();
        if ws.take_dirty().main {
            *app = crate::build_app(ws, VIEWPORT);
        }
        app.frame();
        app.a11y_json().contains(text)
    });
}

#[track_caller]
fn wait_for_text(ws: &Workspace, app: &mut schnellui::App, text: &str) {
    wait_until(|| {
        ws.poll();
        app.frame();
        app.a11y_json().contains(text)
    });
}

#[test]
fn the_file_panel_lists_opens_and_highlights_and_survives_a_restart() {
    use schnellui::raw_keyboard::NamedKey;
    let dir = tempfile::tempdir().unwrap();
    let repo = files_repo(dir.path());
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        crate::fit_grid(&ws, VIEWPORT);
        ws.open_files();
        let mut app = crate::build_app(&ws, VIEWPORT);
        let grid = app.find_widget(schnellui::a11y::Role::ScrollView, Some("Files"));
        assert!(grid.is_some() && app.focused_widget() == grid);
        // Folders first, untracked files the ignore rules allow included.
        wait_for_text(&ws, &mut app, "3 files");
        let a11y = app.a11y_json();
        assert!(
            a11y.contains("▸ src") && a11y.contains("notes.bin"),
            "{a11y}"
        );

        // Enter on the folder expands it, Down then Enter opens the file.
        for key in [NamedKey::Enter, NamedKey::ArrowDown, NamedKey::Enter] {
            assert!(app.dispatch_focused_input(named_key(key)).is_handled());
        }
        wait_for_text(&ws, &mut app, "pub fn answer()");
        let a11y = app.a11y_json();
        let header = format!("src{}lib.rs · Rust · 4 lines", std::path::MAIN_SEPARATOR);
        // The a11y JSON escapes Windows' backslash.
        let header = header.replace('\\', r"\\");
        assert!(a11y.contains(&header), "{a11y}");
        assert!(a11y.contains("3     42 // the answer"), "{a11y}");
        ws.poll();
        assert!(panel_titles(&ws).iter().any(|t| t == "lib.rs"));
    }
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "pub fn answer()");
    // The reopened file is revealed in the tree.
    wait_for_text(&ws, &mut app, "▾ src");
}

/// Moving the selection in the tree rewrites only the two rows it touches,
/// and an idle frame with the panel shown allocates nothing.
#[cfg(feature = "count-allocations")]
#[test]
fn the_file_panel_stays_within_budget() {
    use schnellui::raw_keyboard::NamedKey;
    let dir = tempfile::tempdir().unwrap();
    let repo = files_repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_files();
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "3 files");
    for _ in 0..3 {
        ws.poll();
        app.frame();
    }
    let idle = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    assert_eq!(idle.count_total, 0, "idle frame allocated: {idle:?}");

    // The first moves grow retained buffers once; the key events themselves
    // are dispatched outside the measurement.
    for key in [NamedKey::ArrowDown, NamedKey::ArrowUp] {
        app.dispatch_focused_input(named_key(key));
        ws.poll();
        app.frame();
    }
    app.dispatch_focused_input(named_key(NamedKey::ArrowDown));
    let step = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    eprintln!("selection step: {step:?}");
    assert_eq!(step.count_total, 0, "selection step allocated: {step:?}");
}

fn char_key(text: &str) -> FocusedInputEvent {
    use schnellui::raw_keyboard::{Key, KeyCode, KeyLocation, PhysicalKey};
    use schnellui::{RawInputState, RawKeyEvent, RawModifiers};
    FocusedInputEvent::Key(RawKeyEvent {
        logical_key: Key::Character(text.into()),
        physical_key: PhysicalKey::Code(KeyCode::KeyV),
        key_without_modifiers: Key::Character(text.into()),
        location: KeyLocation::Standard,
        modifiers: RawModifiers::default(),
        state: RawInputState::Pressed,
        repeat: false,
        text: Some(text.into()),
        text_with_all_modifiers: Some(text.into()),
    })
}

/// `main` with the README, then `feature` checked out with one more commit.
fn review_repo(root: &Path) -> std::path::PathBuf {
    let repo = repo(root);
    git(&repo, &["checkout", "-qb", "feature"]);
    std::fs::write(repo.join("README"), "hi\nbye\n").unwrap();
    git(&repo, &["commit", "-qam", "say bye"]);
    repo
}

#[test]
fn the_explore_panel_shows_history_and_a_review_that_survives_a_restart() {
    use schnellui::raw_keyboard::NamedKey;
    let dir = tempfile::tempdir().unwrap();
    let repo = review_repo(dir.path());
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        crate::fit_grid(&ws, VIEWPORT);
        ws.open_explore(false);
        let mut app = crate::build_app(&ws, VIEWPORT);
        let grid = app.find_widget(schnellui::a11y::Role::ScrollView, Some("Explore"));
        assert!(grid.is_some() && app.focused_widget() == grid);
        // The newest commit is selected and shown with its diff.
        wait_for_text(&ws, &mut app, "+ bye");
        let a11y = app.a11y_json();
        assert!(a11y.contains("say bye"), "{a11y}");
        assert!(a11y.contains("Refs:   HEAD -> feature"), "{a11y}");
        assert!(a11y.contains("│ Refs:   HEAD"), "{a11y}");
        assert!(a11y.contains("init (main)"), "{a11y}");
        // Down shows the older commit.
        app.dispatch_focused_input(named_key(NamedKey::ArrowDown));
        wait_for_text(&ws, &mut app, "+ hi");

        // v switches to the review against the detected base.
        assert!(app.dispatch_focused_input(char_key("v")).is_handled());
        wait_for_text(&ws, &mut app, "against main");
        let a11y = app.a11y_json();
        // Uncommitted changes come before the counts, which a narrow
        // reader cuts.
        assert!(
            a11y.contains("against main · with uncommitted changes · 1 file"),
            "{a11y}"
        );
        assert!(a11y.contains("M README"), "{a11y}");
        ws.poll();
        assert!(panel_titles(&ws).iter().any(|t| t == "Review"));

        // The grid modes keep the change request page's bar, whose tabs
        // switch the mode. The panel's own tab is named "Review" now.
        let mut app = crate::build_app(&ws, VIEWPORT);
        let history = app
            .find_widget(schnellui::a11y::Role::Tab, Some("History"))
            .expect("the bar shows above the grid");
        app.dispatch_click(history);
        let explore = ws.pane_explore(0).unwrap();
        assert_eq!(explore.mode(), crate::explore::Mode::History);
        ws.poll();
        assert!(ws.take_dirty().main, "the bar marks the new mode");
        // The app remounts the panel and focuses its grid again.
        let mut app = crate::build_app(&ws, VIEWPORT);
        // History keeps the older commit picked above.
        wait_for_text(&ws, &mut app, "+ hi");
        assert!(
            app.find_widget(schnellui::a11y::Role::Tab, Some("Change request"))
                .is_some()
        );
        app.dispatch_focused_input(char_key("v"));
        wait_for_text(&ws, &mut app, "against main");
    }
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "against main");

    // The palette asks for another base and the open panel takes it.
    ws.open_palette(Some(crate::ui::InputMode::ReviewBase));
    ws.set_palette_query("feature");
    ws.palette_enter();
    assert_eq!(
        ws.pane_explore(0)
            .and_then(|explore| explore.base())
            .as_deref(),
        Some("feature")
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "No changes against feature");
}

#[test]
fn a_project_reference_branch_sets_the_review_base() {
    let dir = tempfile::tempdir().unwrap();
    let repo = review_repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let id = ws.borrow().state.projects[0].id;
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_explore(true);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "against main");

    // The open review follows the project's reference branch and keeps no
    // base of its own.
    ws.set_project_base(id, " feature ");
    wait_for_text(&ws, &mut app, "No changes against feature");
    assert_eq!(ws.pane_explore(0).and_then(|e| e.base()), None);
    let saved = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    assert_eq!(saved.projects[0].base.as_deref(), Some("feature"));

    // A base named in the palette still wins.
    ws.review_against("main");
    wait_for_text(&ws, &mut app, "against main");
    ws.review_against("");
    wait_for_text(&ws, &mut app, "No changes against feature");

    // Empty detects the base again.
    ws.set_project_base(id, "");
    wait_for_text(&ws, &mut app, "against main");
    assert_eq!(ws.borrow().state.project(id).unwrap().base, None);
}

#[test]
fn the_review_shows_staged_unstaged_and_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = review_repo(dir.path());
    std::fs::write(repo.join("README"), "hi\nbye\nagain\n").unwrap();
    std::fs::write(repo.join("STAGED"), "added\n").unwrap();
    git(&repo, &["add", "STAGED"]);
    std::fs::write(repo.join("LOOSE"), "loose\n").unwrap();
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_explore(true);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "A LOOSE");
    let a11y = app.a11y_json();
    for line in ["M README", "A STAGED", "A LOOSE", "+ again"] {
        assert!(a11y.contains(line), "{line}: {a11y}");
    }

    // A file added later shows once the worktree's status picks it up.
    std::fs::write(repo.join("LATER"), "later\n").unwrap();
    wait_for_text(&ws, &mut app, "A LATER");

    // w leaves out what is not committed.
    assert!(app.dispatch_focused_input(char_key("w")).is_handled());
    wait_for_text(&ws, &mut app, "against main · 1 file · +1 −0 · 1 commit");
}

/// Moving the review selection rewrites the rows it touches without
/// allocating, and an idle frame with the panel shown allocates nothing.
#[cfg(feature = "count-allocations")]
#[test]
fn the_explore_panel_stays_within_budget() {
    use schnellui::raw_keyboard::NamedKey;
    let dir = tempfile::tempdir().unwrap();
    let repo = review_repo(dir.path());
    std::fs::write(repo.join("NOTES"), "one\n").unwrap();
    git(&repo, &["add", "NOTES"]);
    git(&repo, &["commit", "-qm", "notes"]);
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_explore(true);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "2 files");
    for _ in 0..3 {
        ws.poll();
        app.frame();
    }
    let idle = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    assert_eq!(idle.count_total, 0, "idle frame allocated: {idle:?}");

    for key in [NamedKey::ArrowDown, NamedKey::ArrowUp] {
        app.dispatch_focused_input(named_key(key));
        ws.poll();
        app.frame();
    }
    app.dispatch_focused_input(named_key(NamedKey::ArrowDown));
    let step = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    assert_eq!(step.count_total, 0, "selection step allocated: {step:?}");
}

#[test]
fn a_repository_url_asks_where_to_clone_and_adds_the_clone() {
    use crate::ui::{CloneTarget, InputMode};

    let dir = tempfile::tempdir().unwrap();
    let upstream = repo(dir.path());
    let ws = workspace(dir.path());
    let url = format!("file://{}", upstream.display());

    ws.open_palette(Some(InputMode::Project));
    ws.set_palette_query(&url);
    ws.palette_enter();
    {
        let palette = &ws.borrow().palette;
        assert_eq!(palette.input, Some(InputMode::Clone));
        assert_eq!(palette.clone_url, url);
        assert_eq!(
            palette.clone_targets[..2],
            [
                CloneTarget::Here {
                    devcontainer: false
                },
                CloneTarget::Here { devcontainer: true },
            ]
        );
    }
    // Escape goes back to the project input with the URL in it.
    ws.palette_escape();
    assert_eq!(ws.borrow().palette.input, Some(InputMode::Project));
    assert_eq!(ws.borrow().palette.query, url);
    ws.palette_enter();

    let parent = dir.path().join("clones");
    ws.set_palette_query(parent.to_str().unwrap());
    ws.palette_enter();
    assert!(!ws.palette_is_open());
    assert_eq!(ws.borrow().pending_projects, std::slice::from_ref(&url));
    wait_until(|| {
        ws.poll();
        ws.borrow().pending_projects.is_empty()
    });
    let inner = ws.borrow();
    let project = inner.state.active().expect("clone added");
    assert_eq!(
        project.root,
        cmdterm_env::canonicalize(&parent).unwrap().join("app")
    );
    assert!(project.root.join("README").is_file());
}

#[test]
fn the_bell_counts_unread_notifications_and_opens_them() {
    use crate::notifications::{Manager, Topic};
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let (ws, _) = palette_fixture(dir.path());
    // Notices are kept but arrive read.
    ws.push_internal_notice("hello");
    ws.borrow().notifications.push(
        Topic::AgentUpdate {
            agent: crate::agents::Agent::Codex,
            env: cmdterm_env::Environment::Local,
            manager: Manager::Npm,
            current: "1.0.0".into(),
            latest: "1.1.0".into(),
            command: vec!["true".into()],
        },
        false,
    );
    ws.poll();
    assert!(ws.take_dirty().sidebar);
    let mut app = crate::build_app(&ws, VIEWPORT);
    let bell = app
        .find_widget(Role::Button, Some("Notifications, 1 unread"))
        .unwrap();
    app.dispatch_click(bell);
    assert!(ws.notifications_is_open());
    assert_eq!(ws.borrow().notifications.unread(), 0);

    ws.take_dirty();
    let app = crate::build_app(&ws, VIEWPORT);
    let json = app.a11y_json();
    assert!(json.contains("Codex 1.1.0 is available"), "{json}");
    assert!(json.contains("hello"));
    assert!(
        app.find_widget(Role::Button, Some("Notifications"))
            .is_some()
    );
    let close = app.resolve_ref(ws.regions().notifications_focus);
    assert!(close.is_some() && app.focused_widget() == close);

    // Update runs the command and reports the result.
    let id = ws
        .borrow()
        .notifications
        .items()
        .iter()
        .find(|item| item.topic.actionable())
        .unwrap()
        .id;
    ws.run_notification(id);
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .notifications
            .items()
            .iter()
            .any(|item| matches!(item.topic, Topic::Updated { .. }))
    });
    // The result arrived while the overlay was open, so the badge stays
    // empty, and closing it keeps it that way.
    assert_eq!(ws.borrow().notifications.unread(), 0);
    ws.close_notifications();
    assert!(!ws.notifications_is_open());
    assert_eq!(ws.borrow().notifications.unread(), 0);
    assert!(ws.take_dirty().sidebar);
}

#[test]
fn the_monitor_panel_shows_the_machine_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        crate::fit_grid(&ws, VIEWPORT);
        ws.open_monitor();
        let mut app = crate::build_app(&ws, VIEWPORT);
        let grid = app.find_widget(schnellui::a11y::Role::ScrollView, Some("Monitor"));
        assert!(grid.is_some() && app.focused_widget() == grid);
        // The first sample needs two readings; the header then names the
        // machine's load and the gauges follow.
        wait_for_text(&ws, &mut app, " · load ");
        // The labels are widgets, mounted again once the sample places them.
        wait_for_mounted_text(&ws, &mut app, "Memory");
        let a11y = app.a11y_json();
        for text in ["CPU", "Memory", "%", "GiB"] {
            assert!(a11y.contains(text), "{text}: {a11y}");
        }
        ws.poll();
        assert!(panel_titles(&ws).iter().any(|t| t == "Monitor"));
    }
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, " · load ");
}

#[test]
fn the_tasks_panel_lists_panels_of_every_worktree_and_the_threads() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        crate::fit_grid(&ws, VIEWPORT);
        ws.open_terminal(None, None);
        ws.open_tasks();
        let mut app = crate::build_app(&ws, VIEWPORT);
        let grid = app.find_widget(schnellui::a11y::Role::ScrollView, Some("Tasks"));
        assert!(grid.is_some() && app.focused_widget() == grid);
        wait_for_mounted_text(&ws, &mut app, "1 worktree");
        let tasks = ws.pane_tasks(0).unwrap();
        let snapshot = tasks.snapshot();
        assert_eq!(snapshot.worktrees.len(), 1);
        let worktree = &snapshot.worktrees[0];
        assert!(worktree.shown);
        let kinds: Vec<_> = worktree
            .panels
            .iter()
            .map(|panel| (panel.kind, panel.status, panel.tab))
            .collect();
        use crate::taskview::{Kind, Status};
        // The default shell and the one opened here, then the tasks tab.
        assert_eq!(
            kinds,
            [
                (Kind::Terminal, Status::Hidden, 0),
                (Kind::Terminal, Status::Hidden, 1),
                (Kind::Tasks, Status::Shown, 2),
            ]
        );
        // The thread sampler reports its threads; idle ones fold into a count.
        // Without `/proc/self/task` the panel says so instead.
        let threads = if cfg!(target_os = "linux") {
            "idle threads"
        } else {
            "no /proc/self/task"
        };
        wait_for_mounted_text(&ws, &mut app, threads);
        let text = tasks.text();
        assert!(text.contains("in background"), "{text}");
        assert!(text.contains("Extension tasks"), "{text}");
        // A terminal that exits closes, and leaves the list on the next look.
        ws.with_active(|live| {
            for (_, panel) in &live.unwrap().panels {
                if let crate::workspace::LivePanel::Terminal(terminal) = panel {
                    // Enter as a keyboard sends it, which `cmd.exe` needs.
                    let _ = terminal.session.write_bytes(b"exit\r");
                }
            }
        });
        wait_until(|| {
            ws.poll();
            tasks.refresh();
            ws.poll();
            !tasks.snapshot().worktrees[0]
                .panels
                .iter()
                .any(|panel| panel.kind == Kind::Terminal)
        });
        assert!(panel_titles(&ws).iter().any(|t| t == "Tasks"));
    }
    // The panel comes back after a restart.
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_mounted_text(&ws, &mut app, "1 worktree");
}

/// Once the tasks panel has drawn what it lists, an idle frame with it shown
/// allocates nothing.
#[cfg(feature = "count-allocations")]
#[test]
fn the_tasks_panel_stays_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_tasks();
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_mounted_text(&ws, &mut app, "idle threads");
    // Let the per-second look run and the rows settle.
    std::thread::sleep(Duration::from_millis(1100));
    ws.poll();
    app.frame();
    let idle = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    assert_eq!(idle.count_total, 0, "idle frame allocated: {idle:?}");
    // A look at an unchanged workspace builds nothing. The minute the
    // ages count in can turn over during one try, so it gets three.
    let looks: Vec<_> = (0..3)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(1100));
            allocation_counter::measure(|| ws.poll()).count_total
        })
        .collect();
    assert!(looks.contains(&0), "every look allocated: {looks:?}");
}

#[test]
fn the_debug_browser_panel_reports_a_browser_that_does_not_start() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let mut settings = ws.borrow().settings.clone();
    settings.debug_browser.browser = crate::state::DebugBrowserKind::Chrome;
    settings.debug_browser.executable = Some(dir.path().join("no-chrome").display().to_string());
    crate::debug_browser::configure(&settings, &ws.borrow().dirs);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_debug_browser();
    let app = crate::build_app(&ws, VIEWPORT);
    let surface = app.find_widget(schnellui::a11y::Role::ScrollView, Some("Debug browser"));
    assert!(surface.is_some() && app.focused_widget() == surface);
    assert!(panel_titles(&ws).iter().any(|t| t == "Debug browser"));
    wait_until(|| {
        ws.poll();
        matches!(
            crate::devtools::hub().status(),
            crate::devtools::Status::Failed(_)
        )
    });
    // The status can turn between the poll and the check above.
    ws.poll();
    let app = crate::build_app(&ws, VIEWPORT);
    let a11y = app.a11y_json();
    assert!(a11y.contains("The debug browser did not start"), "{a11y}");
    assert!(a11y.contains("no-chrome"), "{a11y}");
    assert!(
        app.find_widget(schnellui::a11y::Role::Button, Some("Try again"))
            .is_some()
    );
}

/// A frame that takes a new sample rewrites the rows whose numbers changed
/// without allocating, and an idle frame allocates nothing.
#[cfg(feature = "count-allocations")]
#[test]
fn the_monitor_panel_stays_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_monitor();
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, " · load ");
    let monitor = ws.pane_monitor(0).unwrap();
    // Let a few samples grow the retained rows and glyph caches.
    for _ in 0..3 {
        let revision = monitor.revision();
        wait_until(|| monitor.revision() != revision);
        ws.poll();
        app.frame();
    }
    let idle = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    assert_eq!(idle.count_total, 0, "idle frame allocated: {idle:?}");

    let revision = monitor.revision();
    wait_until(|| monitor.revision() != revision);
    let sample = allocation_counter::measure(|| {
        ws.poll();
        app.frame();
    });
    eprintln!("sample frame: {sample:?}");
    assert_eq!(sample.count_total, 0, "sample frame allocated: {sample:?}");
}

#[test]
fn extension_tasks_run_once_per_period_and_remember_it_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let extensions = dir.path().join("cfg/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(
        extensions.join("cleanup.lua"),
        r#"
        term.task{ name = "prune", title = "Prune", every = "1d", projects = { "app" },
          run = function(e)
            term.notify("pruned " .. e.project .. " " .. #e.worktrees .. " " .. e.worktrees[1].runtime)
          end }
        term.task{ name = "elsewhere", every = "1d", projects = { "/nowhere/*" },
          run = function() term.notify("wrong project") end }
        term.task{ name = "global", every = 3600, run = function(e) term.notify("ran " .. e.task) end }
        "#,
    )
    .unwrap();
    let count = |ws: &Workspace, text: &str| {
        ws.borrow()
            .notices
            .iter()
            .filter(|n| n.text.starts_with(text))
            .count()
    };
    let ws = workspace(dir.path());
    ws.poll();
    assert_eq!(count(&ws, "ran cleanup.global"), 1);
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        count(&ws, "pruned app 1 ") == 1
    });
    for _ in 0..3 {
        ws.poll();
    }
    assert_eq!(count(&ws, "pruned app"), 1);
    assert_eq!(count(&ws, "ran cleanup.global"), 1);
    assert_eq!(count(&ws, "wrong project"), 0);
    let runs = ws.borrow().state.task_runs.clone();
    let id = ws.borrow().state.projects[0].id;
    assert_eq!(
        runs.keys().cloned().collect::<Vec<_>>(),
        ["cleanup.global".to_owned(), format!("cleanup.prune@{id}")]
    );
    drop(ws);

    let again = workspace(dir.path());
    for _ in 0..3 {
        again.poll();
    }
    assert_eq!(count(&again, "pruned app"), 0, "the day has not passed");
    assert_eq!(count(&again, "ran cleanup.global"), 0);
    wait_until(|| {
        again.poll();
        !again.borrow().snapshots.is_empty()
    });
    assert_eq!(
        count(&again, "pruned app"),
        0,
        "a snapshot does not make it due"
    );
    again.open_palette(None);
    again.set_palette_query("run task: prune");
    again.palette_run(0);
    again.poll();
    assert_eq!(count(&again, "pruned app"), 1);
}

/// A terminal that attaches again to a remote session after a restart
/// keeps the socket name and the token its shell was started with, so the
/// shell still reaches the control socket. Saved values that could not
/// have come from the app are replaced.
#[cfg(unix)]
#[test]
fn remote_sessions_keep_their_control_socket_and_token() {
    use crate::control::{VAR_SOCKET, VAR_TOKEN, call};
    use crate::state::SessionControl;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    ws.pick_agent_control(crate::state::AgentControl::On);
    let env = cmdterm_env::Environment::Remote(cmdterm_env::RemoteTarget::new("box"));
    let var = |vars: &[(std::ffi::OsString, std::ffi::OsString)], key: &str| {
        (vars.iter())
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .unwrap()
    };
    let (first, vars, sockets, seat) = ws.borrow_mut().terminal_control(&env, &repo, None);
    let seat = seat.unwrap();
    assert_eq!(first.as_ref().unwrap().token(), seat.token);
    assert_eq!(var(&vars, VAR_SOCKET), format!("/tmp/{}.sock", seat.socket));
    drop(first);

    let (again, vars, _, kept) = ws.borrow_mut().terminal_control(&env, &repo, Some(&seat));
    assert_eq!(kept.as_ref(), Some(&seat));
    assert_eq!(var(&vars, VAR_TOKEN), seat.token);
    assert_eq!(var(&vars, VAR_SOCKET), format!("/tmp/{}.sock", seat.socket));
    // The kept token opens the socket again.
    let (status, body) = call(&sockets[0].0, "GET", "/v1/info", &seat.token, b"");
    assert_eq!(status, 200, "{body}");
    drop(again);

    let forged = SessionControl {
        socket: "../../etc/x".into(),
        token: "short".into(),
    };
    let (_reg, vars, _, fresh) = ws.borrow_mut().terminal_control(&env, &repo, Some(&forged));
    let fresh = fresh.unwrap();
    assert!(crate::control::valid_socket_name(&fresh.socket));
    assert_eq!(fresh.token.len(), 64);
    assert_eq!(
        var(&vars, VAR_SOCKET),
        format!("/tmp/{}.sock", fresh.socket)
    );
}

/// A terminal that waits for its runtime, such as a devcontainer on a
/// remote machine, keeps the saved session's socket and token with its
/// name, so a save before the runtime is ready does not lose them.
#[cfg(unix)]
#[test]
fn a_terminal_waiting_for_its_runtime_keeps_its_session_control() {
    use crate::state::{PanelSpec, SessionControl};
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    let log = dir.path().join("runtime.log");
    std::fs::write(&log, "").unwrap();
    let spec = PanelSpec::Terminal {
        title: None,
        command: None,
        cwd: None,
        session: Some("0123456789abcdef".into()),
        control: Some(SessionControl {
            socket: crate::control::socket_name(),
            token: "a".repeat(64),
        }),
        agent: None,
        once: Vec::new(),
    };
    let waiting = crate::runtime::TermEnv::Waiting(log);
    let (_, panel) = (ws.borrow_mut())
        .open_panel(&waiting, &repo, &spec, &[])
        .unwrap();
    assert_eq!(panel.spec(), spec);
}

/// Restarting a worktree's terminals with new sessions forgets the saved
/// sockets and tokens with the session names, so new sessions get their
/// own instead of the paths the old relays are removing.
#[test]
fn new_sessions_forget_the_saved_control() {
    use crate::state::{PanelSpec, SessionControl};
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let id = ws.borrow().state.projects[0].id;
    // A worktree that is not shown, so the restart opens nothing.
    let other = dir.path().join("other");
    let control = SessionControl {
        socket: crate::control::socket_name(),
        token: "a".repeat(64),
    };
    let terminal = |session: Option<&str>| PanelSpec::Terminal {
        title: None,
        command: None,
        cwd: None,
        session: session.map(str::to_owned),
        control: session.map(|_| control.clone()),
        agent: None,
        once: Vec::new(),
    };
    {
        let mut inner = ws.borrow_mut();
        let config = inner.state.project_mut(id).unwrap().worktree_mut(&other);
        config.panels = vec![terminal(Some("0123456789abcdef"))];
    }
    ws.restart_terminals(id, &other, false);
    let panels = |ws: &Workspace| {
        let inner = ws.borrow();
        let project = inner.state.project(id).unwrap();
        (project.worktrees.iter())
            .find(|w| w.path == other)
            .unwrap()
            .panels
            .clone()
    };
    assert_eq!(panels(&ws), [terminal(Some("0123456789abcdef"))]);
    ws.restart_terminals(id, &other, true);
    assert_eq!(panels(&ws), [terminal(None)]);
}

/// A terminal gets a token for the control socket and the variables that
/// name the terminal, also in a sandbox; an extension change it asks for
/// waits in the approval overlay, Allow writes and loads it, and the token
/// dies with the terminal.
#[cfg(unix)]
#[test]
fn agents_change_extensions_through_the_control_socket_with_approval() {
    use crate::control::call;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    // New worktrees run in a sandbox, whose /tmp is its own; the worktree
    // is shared. Sandboxed terminals get the socket only when asked for.
    ws.pick_agent_control(crate::state::AgentControl::Local);
    let vars = repo.join("vars");
    let script = format!(
        "printf '%s\\n%s\\n%s\\n%s\\n%s\\n' \"$SCRIPTSCHNELL_TERM\" \"$SCRIPTSCHNELL_TERM_SOCKET\" \
         \"$SCRIPTSCHNELL_TERM_TOKEN\" \"$COLORTERM\" \"$TERM_PROGRAM\" > {}.tmp && mv {0}.tmp {0}; \
         exec sleep 60",
        vars.display()
    );
    ws.open_terminal(Some(vec!["sh".into(), "-c".into(), script]), None);
    wait_until(|| vars.is_file());
    let text = std::fs::read_to_string(&vars).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let [marker, socket, token, colors, program] = lines[..] else {
        panic!("{text:?}")
    };
    assert_eq!(marker, "1");
    assert_eq!((colors, program), ("truecolor", "scriptschnell-term"));
    let socket = Path::new(socket).to_path_buf();
    let token = token.to_owned();
    let (status, body) = call(&socket, "GET", "/v1/info", &token, b"");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&repo.display().to_string()), "{body}");

    let source = b"term.command{ name = \"hello.hi\", title = \"Hello\", run = function() end }\n";
    let (status, body) = call(
        &socket,
        "PUT",
        "/v1/extensions/hello/files/init.lua",
        &token,
        source,
    );
    assert_eq!(status, 200, "{body}");
    let apply = {
        let (socket, token) = (socket.clone(), token.clone());
        std::thread::spawn(move || call(&socket, "POST", "/v1/extensions/hello/apply", &token, b""))
    };
    wait_until(|| {
        ws.poll();
        ws.approval_is_open()
    });
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(app.a11y_json().contains("Extension change"));
    let change = ws.borrow().pending_change().unwrap();
    assert!(change.new);
    ws.answer_change(change.id, true);
    let (status, body) = apply.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"loaded\": true"), "{body}");
    assert!(dir.path().join("cfg/extensions/hello/init.lua").is_file());
    assert!(!ws.approval_is_open());

    let id = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(id);
    // The token goes with the panel's last reference, which the built
    // views hold until they are rebuilt.
    drop(app);
    assert_eq!(call(&socket, "GET", "/v1/info", &token, b"").0, 401);
}

/// An agent asks for a new worktree with an agent in it; Allow creates
/// the worktree, switches to it and starts the agent there.
#[cfg(unix)]
#[test]
fn agents_start_worktrees_with_agents_through_the_control_socket() {
    use crate::control::call;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    let control = ws.borrow_mut().control().unwrap();
    let project_id = ws.borrow().state.active_project;
    let token = control.register(crate::control::Terminal {
        project_id,
        project: "app".into(),
        worktree: repo.clone(),
        machine: "local".into(),
        kind: "local",
    });
    let socket = control.socket().to_path_buf();
    // Posts `body` to `path` and returns the answer once the user allowed it.
    let allowed = |path: &'static str, body: &'static [u8]| {
        let (socket, token) = (socket.clone(), token.clone());
        let request = std::thread::spawn(move || call(&socket, "POST", path, &token, body));
        wait_until(|| {
            ws.poll();
            ws.approval_is_open()
        });
        let mut app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        assert!(app.a11y_json().contains("Start agents"));
        let launch = ws.borrow().pending_launch().unwrap();
        ws.answer_launch(launch.id, true);
        wait_until(|| {
            ws.poll();
            request.is_finished()
        });
        request.join().unwrap()
    };
    let (status, body) = allowed(
        "/v1/worktrees",
        br#"{"branch": "feature/y", "agents": [{"agent": "codex", "args": ["hi"]}]}"#,
    );
    let feature = repo.with_file_name("app-feature-y");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("app-feature-y"), "{body}");
    assert!(body.contains(r#""vcs": "git""#), "{body}");
    let active = ws.borrow().state.active().unwrap().active_worktree.clone();
    assert_eq!(active.as_deref(), Some(feature.as_path()));
    // The fresh worktree's shell, then the agent. Its prompt runs this time
    // only, so the saved command, which a restart runs, leaves it out.
    let commands = ws.with_active(|live| {
        (live.unwrap().panels.iter())
            .filter_map(|(_, panel)| match panel {
                crate::workspace::LivePanel::Terminal(t) => t.command.clone(),
                _ => None,
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(commands, [vec!["codex".to_owned()]]);

    // A detached worktree at a revision, with the directory named.
    let (status, body) = allowed(
        "/v1/worktrees",
        br#"{"detach": true, "from": "HEAD", "name": "review", "agents": ["claude"]}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("app-review"), "{body}");
    // An existing worktree, named by its branch once the snapshot lists it.
    wait_until(|| {
        ws.poll();
        (ws.borrow().snapshots.values()).any(|s| {
            s.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("feature/y"))
        })
    });
    let (status, body) = allowed(
        "/v1/agents",
        br#"{"worktree": "feature/y", "agents": ["claude"]}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("app-feature-y"), "{body}");
    let active = ws.borrow().state.active().unwrap().active_worktree.clone();
    assert_eq!(active.as_deref(), Some(feature.as_path()));
    let (status, body) = allowed(
        "/v1/agents",
        br#"{"worktree": "nope", "agents": ["claude"]}"#,
    );
    assert_eq!(status, 500, "{body}");
}

/// "Allow in this session" lets the project's later launches pass without
/// a question; "Always allow in this project" also keeps that in the state.
#[cfg(unix)]
#[test]
fn launches_can_be_allowed_for_the_session_or_the_project() {
    use crate::control::call;
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    let control = ws.borrow_mut().control().unwrap();
    let project_id = ws.borrow().state.active_project;
    let token = control.register(crate::control::Terminal {
        project_id,
        project: "app".into(),
        worktree: repo.clone(),
        machine: "local".into(),
        kind: "local",
    });
    let socket = control.socket().to_path_buf();
    let start = || {
        let (socket, token) = (socket.clone(), token.clone());
        std::thread::spawn(move || {
            call(
                &socket,
                "POST",
                "/v1/agents",
                &token,
                br#"{"agents": ["codex"]}"#,
            )
        })
    };
    let click = |name: &str| {
        wait_until(|| {
            ws.poll();
            ws.approval_is_open()
        });
        ws.take_dirty();
        let mut app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        let button = app.find_widget(Role::Button, Some(name)).unwrap();
        app.dispatch_click(button);
    };
    let finish = |request: std::thread::JoinHandle<(u16, String)>| {
        wait_until(|| {
            ws.poll();
            request.is_finished()
        });
        let (status, body) = request.join().unwrap();
        assert_eq!(status, 200, "{body}");
    };

    let request = start();
    click(crate::i18n::EN.control_allow_session);
    finish(request);
    finish(start());
    let saved = std::fs::read_to_string(dir.path().join("cfg/state.json")).unwrap();
    assert!(!saved.contains("allows_launches"), "{saved}");

    ws.borrow_mut().launches_allowed.clear();
    let request = start();
    click(crate::i18n::EN.approval_always);
    finish(request);
    finish(start());
    let saved = std::fs::read_to_string(dir.path().join("cfg/state.json")).unwrap();
    assert!(saved.contains(r#""allows_launches": true"#), "{saved}");

    ws.revoke_launches(project_id.unwrap());
    let request = start();
    wait_until(|| {
        ws.poll();
        ws.approval_is_open()
    });
    let launch = ws.borrow().pending_launch().unwrap();
    ws.answer_launch(launch.id, false);
    assert_eq!(request.join().unwrap().0, 403);
}

/// An agent suggests an allowed host; Allow puts it in the settings, on
/// disk and in what the sandbox report shows.
#[cfg(unix)]
#[test]
fn agents_suggest_settings_through_the_control_socket() {
    use crate::control::call;
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    let control = ws.borrow_mut().control().unwrap();
    let token = control.register(crate::control::Terminal {
        project_id: None,
        project: String::new(),
        worktree: dir.path().to_path_buf(),
        machine: "local".into(),
        kind: "sandbox",
    });
    let socket = control.socket().to_path_buf();
    let (status, report) = call(&socket, "GET", "/v1/sandbox", &token, b"");
    assert_eq!(status, 200, "{report}");
    assert!(!report.contains("pypi.example"), "{report}");
    let request = {
        let (socket, token) = (socket.clone(), token.clone());
        std::thread::spawn(move || {
            let body =
                br#"{"reason": "pip install", "add": {"sandbox.allowed_hosts": ["pypi.example"]}}"#;
            call(&socket, "POST", "/v1/settings", &token, body)
        })
    };
    wait_until(|| {
        ws.poll();
        ws.approval_is_open()
    });
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let a11y = app.a11y_json();
    assert!(a11y.contains("Settings change"), "{a11y}");
    assert!(
        a11y.contains("sandbox.allowed_hosts: add pypi.example"),
        "{a11y}"
    );
    let suggestion = ws.borrow().pending_suggestion().unwrap();
    ws.answer_suggestion(suggestion.id, true);
    let (status, body) = request.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(
        ws.borrow()
            .settings
            .sandbox
            .allowed_hosts
            .iter()
            .any(|h| h == "pypi.example")
    );
    let saved = SettingsStore::new(dir.path().join("cfg/settings.json"))
        .load()
        .unwrap();
    assert!(
        saved
            .sandbox
            .allowed_hosts
            .iter()
            .any(|h| h == "pypi.example")
    );
    let (_, report) = call(&socket, "GET", "/v1/sandbox", &token, b"");
    assert!(report.contains("pypi.example"), "{report}");
}

/// Starts the installed Chrome and Firefox, so it only runs on request:
/// `cargo test -p cmdterm -- --ignored debug_browser_panel_streams`.
#[test]
#[ignore = "starts real browsers"]
fn the_debug_browser_panel_streams_the_page_and_takes_clicks() {
    for kind in [
        crate::state::DebugBrowserKind::Chrome,
        crate::state::DebugBrowserKind::Firefox,
    ] {
        panel_streams_and_takes_clicks(kind);
    }
}

fn panel_streams_and_takes_clicks(kind: crate::state::DebugBrowserKind) {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    let mut settings = ws.borrow().settings.clone();
    settings.debug_browser.browser = kind;
    crate::debug_browser::configure(&settings, &ws.borrow().dirs);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_in_debug_browser(
        "data:text/html,<title>Red</title><body style='margin:0;background:%23f00'><script>\
         addEventListener('click', () => { document.body.style.background = '%2300f'; \
         document.title = 'Blue' })</script>",
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    let hub = crate::devtools::hub();
    let pixel = || {
        let frame = hub.frame()?;
        let at = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
        Some((frame.pixels[at], frame.pixels[at + 1], frame.pixels[at + 2]))
    };
    let red = |p: Option<(u8, u8, u8)>| p.is_some_and(|(r, g, b)| r > 200 && g < 50 && b < 50);
    wait_until(|| {
        ws.poll();
        app.frame();
        red(pixel())
    });
    wait_until(|| {
        ws.poll();
        panel_titles(&ws).iter().any(|t| t == "Red")
    });
    let surface = app.find_widget(schnellui::a11y::Role::ScrollView, Some("Debug browser"));
    assert!(surface.is_some() && app.focused_widget() == surface);
    let click = |state| {
        schnellui::FocusedInputEvent::Pointer(schnellui::RawPointerEvent {
            position: schnellui::scene::Point { x: 100.0, y: 100.0 },
            window_position: schnellui::scene::Point { x: 400.0, y: 140.0 },
            modifiers: schnellui::RawModifiers::default(),
            action: schnellui::RawPointerAction::Button {
                button: schnellui::RawPointerButton::Left,
                state,
            },
        })
    };
    for state in [
        schnellui::RawInputState::Pressed,
        schnellui::RawInputState::Released,
    ] {
        assert!(app.dispatch_focused_input(click(state)).is_handled());
    }
    wait_until(|| {
        ws.poll();
        app.frame();
        pixel().is_some_and(|(r, _, b)| r < 50 && b > 200)
    });
    wait_until(|| {
        ws.poll();
        panel_titles(&ws).iter().any(|t| t == "Blue")
    });
    hub.shutdown();
}

#[test]
fn project_stores_follow_the_project_and_reach_tools() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    granted_extension(
        dir.path(),
        "visits",
        &["project_storage"],
        r#"
        term.on("worktree_activated", function(e)
          local data = term.project_store(e.project_id)
          data.set("visits", (data.get("visits") or 0) + 1)
          term.notify("visit " .. e.project_id .. " " .. data.get("visits")
            .. " " .. tostring(term.context().project_id == e.project_id))
        end)
        "#,
    );
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    ws.poll();
    let id = {
        let inner = ws.borrow();
        let project = inner.state.active().unwrap();
        crate::workspace::project_storage_id(&project.name, &project.env, &project.root)
    };
    assert!(
        id.starts_with("app-") && id.len() == "app-".len() + 16,
        "{id}"
    );
    assert!(
        ws.borrow()
            .notices
            .iter()
            .any(|n| n.text == format!("visit {id} 1 true")),
        "{:?}",
        ws.borrow().notices
    );
    crate::ext::flush_stores();
    assert!(
        dir.path()
            .join("cfg/project-data")
            .join(&id)
            .join("visits/visits.json")
            .is_file()
    );
}

#[test]
#[cfg(unix)]
fn an_agent_session_in_a_terminal_resumes_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    // Names its session the way the hooks do, then prints its arguments.
    // In the worktree, which the sandbox shows.
    let claude = repo.join("bin/cmdterm-test-agent");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(
        &claude,
        "#!/bin/sh\n\
         printf '\\033]1337;SetUserVar=cmdterm_session=%s\\007' \"$(printf test-agent:3f2a-11 | base64)\"\n\
         printf '\\033]1337;SetUserVar=cmdterm_agent=%s\\007' \"$(printf busy | base64)\"\n\
         sleep 0.3\n\
         [ \"$1\" = --resume ] || printf '\\033]0;Fix the login form\\007'\n\
         echo \"args:$*:end\"\n\
         read line; echo \"got:$line\"\n\
         sleep 30\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&claude).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&claude, permissions).unwrap();
    let saved = |root: &Path| {
        let state = StateStore::new(root.join("cfg/state.json")).load().unwrap();
        let config = state.projects[0].worktree(&repo).cloned().unwrap();
        config.panels.into_iter().find_map(|panel| match panel {
            crate::state::PanelSpec::Terminal { agent, .. } => agent,
            _ => None,
        })
    };
    let saved_title = |root: &Path| {
        let state = StateStore::new(root.join("cfg/state.json")).load().unwrap();
        let config = state.projects[0].worktree(&repo).cloned().unwrap();
        config
            .agent_sessions
            .into_iter()
            .find_map(|record| record.title)
    };
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        crate::fit_grid(&ws, VIEWPORT);
        // Started the way a launch with a prompt starts it, labeled with
        // the agent's name.
        let argv = vec![claude.to_str().unwrap().into()];
        ws.open_terminal_once(argv, vec!["fix the login form".into()], "Claude".into());
        let mut app = crate::build_app(&ws, VIEWPORT);
        wait_for_text(&ws, &mut app, "args:fix the login form:end");
        wait_until(|| {
            ws.poll();
            saved(dir.path()).is_some_and(|agent| agent.working)
                && saved_title(dir.path()).is_some()
        });
    }
    let agent = saved(dir.path()).unwrap();
    assert_eq!(
        (agent.agent.as_str(), agent.session.as_deref()),
        ("test-agent", Some("3f2a-11"))
    );
    // The prompt is not saved, so it never runs a second time.
    let state = StateStore::new(dir.path().join("cfg/state.json"))
        .load()
        .unwrap();
    let saved_json = serde_json::to_string(&state).unwrap();
    assert!(!saved_json.contains("fix the login form"), "{saved_json}");

    {
        let ws = workspace(dir.path());
        crate::fit_grid(&ws, VIEWPORT);
        let mut app = crate::build_app(&ws, VIEWPORT);
        // Resumed without the prompt, which would run again.
        wait_for_text(&ws, &mut app, "args:--resume 3f2a-11:end");
        // It was working when the app stopped, so it is told to go on.
        wait_for_text(&ws, &mut app, "got:scriptschnell-term was closed");
        // The resumed session set no title yet. Its tab has the saved one
        // instead of the launch's label.
        let terminal = ws.active_terminal().unwrap();
        assert_eq!(terminal.title(), "Fix the login form");
        assert_eq!(terminal.agent_title(), "Fix the login form");
    }

    // A shell the agent ran in gets the command typed once it is ready.
    let store = StateStore::new(dir.path().join("cfg/state.json"));
    let mut state = store.load().unwrap();
    for panel in &mut state.projects[0].worktree_mut(&repo).panels {
        if let crate::state::PanelSpec::Terminal { command, agent, .. } = panel {
            *command = None;
            if let Some(agent) = agent {
                agent.working = false;
            }
        }
    }
    store.save(&state).unwrap();
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "cmdterm-test-agent --resume 3f2a-11");
}

#[test]
#[cfg(unix)]
fn a_saved_agent_session_that_is_gone_opens_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    // Names its session the way the hooks do, then draws its screen: the
    // app types into an agent only once its screen changed after the name
    // and then stayed still, and waits 10 seconds for that otherwise.
    let agent = repo.join("bin/cmdterm-test-agent");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(
        &agent,
        "#!/bin/sh\n\
         printf '\\033]1337;SetUserVar=cmdterm_session=%s\\007' \"$(printf test-agent:3f2a-11 | base64)\"\n\
         sleep 0.3\n\
         echo \"args:$*:end\"\n\
         read line; echo \"got:$line\"\n\
         sleep 30\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&agent).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&agent, permissions).unwrap();
    // Where the test agent keeps its sessions: empty, so 3f2a-11 is gone.
    // In the worktree, which the sandbox shows.
    let sessions = repo.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    {
        let ws = workspace(dir.path());
        add_project_and_wait(&ws, &repo);
        {
            let mut inner = ws.borrow_mut();
            let id = inner.state.projects[0].id;
            let path = format!(
                "{}:{}",
                agent.parent().unwrap().display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let config = inner.state.project_mut(id).unwrap().worktree_mut(&repo);
            config.env_vars.push(("PATH".into(), path));
            config.env_vars.push((
                "CMDTERM_TEST_AGENT_SESSIONS".into(),
                sessions.to_str().unwrap().into(),
            ));
        }
        crate::fit_grid(&ws, VIEWPORT);
        ws.open_terminal(Some(vec![agent.to_str().unwrap().into()]), None);
        let mut app = crate::build_app(&ws, VIEWPORT);
        wait_for_text(&ws, &mut app, "args::end");
        let saved = || {
            let state = StateStore::new(dir.path().join("cfg/state.json"))
                .load()
                .unwrap();
            let config = state.projects[0].worktree(&repo).cloned().unwrap();
            config.panels.into_iter().any(|panel| {
                matches!(panel, crate::state::PanelSpec::Terminal { agent: Some(agent), .. }
                    if agent.session.as_deref() == Some("3f2a-11"))
            })
        };
        wait_until(|| {
            ws.poll();
            ws.save();
            saved()
        });
    }
    let mark_working = || {
        let store = StateStore::new(dir.path().join("cfg/state.json"));
        let mut state = store.load().unwrap();
        for panel in &mut state.projects[0].worktree_mut(&repo).panels {
            if let crate::state::PanelSpec::Terminal {
                agent: Some(agent), ..
            } = panel
            {
                agent.working = true;
            }
        }
        store.save(&state).unwrap();
    };
    mark_working();
    {
        // Run directly, it starts without --resume and gets no prompt to
        // go on.
        let ws = workspace(dir.path());
        crate::fit_grid(&ws, VIEWPORT);
        let mut app = crate::build_app(&ws, VIEWPORT);
        wait_for_text(&ws, &mut app, "args::end");
        std::thread::sleep(Duration::from_secs(3));
        ws.poll();
        app.frame();
        let text = app.a11y_json();
        assert!(!text.contains("--resume"), "{text}");
        assert!(!text.contains("got:"), "{text}");
    }
    // Once the session exists again, it resumes.
    std::fs::write(sessions.join("3f2a-11.jsonl"), "").unwrap();
    mark_working();
    {
        let ws = workspace(dir.path());
        crate::fit_grid(&ws, VIEWPORT);
        let mut app = crate::build_app(&ws, VIEWPORT);
        wait_for_text(&ws, &mut app, "args:--resume 3f2a-11:end");
        wait_for_text(&ws, &mut app, "got:scriptschnell-term was closed");
    }
    // In a shell, the line typed starts a new session.
    std::fs::remove_file(sessions.join("3f2a-11.jsonl")).unwrap();
    let store = StateStore::new(dir.path().join("cfg/state.json"));
    let mut state = store.load().unwrap();
    for panel in &mut state.projects[0].worktree_mut(&repo).panels {
        if let crate::state::PanelSpec::Terminal { command, .. } = panel {
            *command = None;
        }
    }
    store.save(&state).unwrap();
    let ws = workspace(dir.path());
    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "args::end");
    let text = app.a11y_json();
    assert!(!text.contains("--resume"), "{text}");
}

/// A launch's first prompt that the agent takes typed waits until the agent
/// named its session.
#[test]
#[cfg(unix)]
fn a_first_prompt_is_typed_once_the_agent_is_ready() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let agent = repo.join("bin/claude");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(
        &agent,
        "#!/bin/sh\n\
         sleep 1\n\
         echo starting\n\
         printf '\\033]1337;SetUserVar=cmdterm_session=%s\\007' \"$(printf claude:abc-1 | base64)\"\n\
         read line; echo \"got:$line\"\n\
         sleep 30\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&agent).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&agent, permissions).unwrap();
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let terminal = ws
        .open_terminal_once(
            vec![agent.to_str().unwrap().into()],
            Vec::new(),
            "Claude".into(),
        )
        .unwrap();
    crate::agents::paste_when_ready(
        terminal.session.input(),
        crate::agents::Agent::ClaudeCode,
        "fix the login form".into(),
    );
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "got:fix the login form");
    let text = app.a11y_json();
    assert!(
        text.find("starting").unwrap() < text.find("fix the login form").unwrap(),
        "{text}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn agents_run_directly_get_bash_as_their_shell() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(bash) = std::env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .find(|bash| bash.is_file())
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    // Named like Claude Code, it reports the shell it got.
    let agent = repo.join("bin/claude");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(&agent, "#!/bin/sh\necho \"shell=[$SHELL]\"\nsleep 30\n").unwrap();
    let mut permissions = std::fs::metadata(&agent).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&agent, permissions).unwrap();
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_terminal(Some(vec![agent.to_str().unwrap().into()]), None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, &format!("shell=[{}]", bash.display()));
    // Turned off, the agent keeps the user's shell.
    ws.borrow_mut().settings.agents_use_bash = false;
    ws.open_terminal(Some(vec![agent.to_str().unwrap().into()]), None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    let user = std::env::var("SHELL").unwrap_or_default();
    if user != bash.to_str().unwrap() {
        wait_for_text(&ws, &mut app, &format!("shell=[{user}]"));
    }
}

#[test]
#[cfg(unix)]
fn an_ended_agent_session_stays_in_the_agents_tab_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    // Names its session, reports, titles its terminal, then ends the
    // session.
    let agent = repo.join("bin/cmdterm-test-agent");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(
        &agent,
        "#!/bin/sh\n\
         printf '\\033]1337;SetUserVar=cmdterm_session=%s\\007' \"$(printf test-agent:3f2a-11 | base64)\"\n\
         printf '\\033]1337;SetUserVar=cmdterm_agent=%s\\007' \"$(printf done | base64)\"\n\
         printf '\\033]0;Fix the login form\\007'\n\
         read line\n\
         printf '\\033]1337;SetUserVar=cmdterm_session=\\007'\n\
         echo ended\n\
         sleep 30\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&agent).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&agent, permissions).unwrap();
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_terminal(Some(vec![agent.to_str().unwrap().into()]), None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    let earlier = |ws: &Workspace| {
        ws.borrow()
            .agent_rows()
            .into_iter()
            .filter(|r| r.group == crate::forges::AgentGroup::Earlier)
            .map(|r| (r.title, r.resume.and_then(|a| a.session)))
            .collect::<Vec<_>>()
    };
    wait_until(|| {
        ws.poll();
        ws.borrow().state.projects[0]
            .worktree(&repo)
            .is_some_and(|w| w.agent_sessions.iter().any(|r| r.title.is_some()))
    });
    // Running, the session is no earlier one.
    assert_eq!(earlier(&ws), []);
    ws.active_terminal()
        .unwrap()
        .session
        .write_bytes(b"\r")
        .unwrap();
    wait_for_text(&ws, &mut app, "ended");
    wait_until(|| {
        ws.poll();
        !earlier(&ws).is_empty()
    });
    // In the main worktree an ended session is an earlier one.
    assert_eq!(
        earlier(&ws),
        [("Fix the login form".to_owned(), Some("3f2a-11".to_owned()))]
    );
    // A click resumes it in a new shell.
    let row = (ws.borrow().agent_rows().into_iter())
        .find(|r| r.group == crate::forges::AgentGroup::Earlier)
        .unwrap();
    ws.resume_agent_session(row.project, &row.path, row.resume.as_ref().unwrap());
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_text(&ws, &mut app, "cmdterm-test-agent --resume 3f2a-11");
}

/// An agent idle in a hidden shell stops, and resumes in that shell when
/// the terminal shows again; what was typed meanwhile reaches the resumed
/// agent.
#[cfg(target_os = "linux")]
#[test]
fn an_idle_agent_in_a_hidden_terminal_freezes_and_resumes_when_shown() {
    use crate::terminal_panel::Freeze;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    // Names its session, reports its turn done and echoes what it reads.
    let claude = repo.join("bin/cmdterm-test-agent");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::fs::write(
        &claude,
        "#!/bin/sh\n\
         printf '\\033]1337;SetUserVar=cmdterm_session=%s\\007' \"$(printf test-agent:3f2a-11 | base64)\"\n\
         printf '\\033]1337;SetUserVar=cmdterm_agent=%s\\007' \"$(printf done | base64)\"\n\
         echo \"agent-up:$*:end\"\n\
         while read line; do echo \"got:$line\"; done\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&claude).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&claude, permissions).unwrap();

    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    // `claude` resolves through PATH, as the resume command does.
    {
        let mut inner = ws.borrow_mut();
        let id = inner.state.projects[0].id;
        let path = format!(
            "{}:{}",
            claude.parent().unwrap().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let config = inner.state.project_mut(id).unwrap().worktree_mut(&repo);
        config.env_vars.push(("PATH".into(), path));
    }
    let first = ws.with_active(|live| live.unwrap().focused_panel().unwrap().0);
    ws.close_panel(first);
    ws.open_terminal(None, None);
    let mut app = crate::build_app(&ws, VIEWPORT);
    let agent = ws.active_terminal().unwrap();
    assert!(agent.freezable);
    // Typed once the shell drew its prompt, as on a restore.
    crate::agents::resume_in(
        agent.session.input(),
        "test-agent",
        Some("cmdterm-test-agent".into()),
        false,
        None,
    );
    wait_until(|| {
        ws.poll();
        app.frame();
        agent.session.attention() == crate::term::Attention::Done
            && agent.agent_session.borrow().is_some()
    });

    // Shown, it keeps running.
    ws.freeze_idle_agents(Instant::now(), Duration::ZERO);
    assert_eq!(agent.freeze_state(), Freeze::Live);
    ws.open_terminal(None, None);
    app.frame();
    ws.freeze_idle_agents(Instant::now(), Duration::ZERO);
    assert_eq!(agent.freeze_state(), Freeze::Stopped);
    // The shell is in the foreground again once the agent ended.
    let group = agent.session.foreground_group();
    wait_until(|| agent.session.foreground_group() != group);

    ws.select_tab(0);
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert_eq!(agent.freeze_state(), Freeze::Thawing);
    let typed = app.dispatch_focused_input(FocusedInputEvent::Ime(RawImeEvent::Commit(
        "held-input\r".into(),
    )));
    assert!(typed.is_handled());
    wait_for_text(&ws, &mut app, "agent-up:--resume 3f2a-11:end");
    wait_for_text(&ws, &mut app, "got:held-input");
    assert_eq!(agent.freeze_state(), Freeze::Live);
}

/// A forge extension that notes what `check` gets and reports the branch
/// `merged-work` merged and every other one open.
const HUB_FORGE: &str = r#"
term.forge{
  name = "hub",
  title = "Hub",
  accounts = { default = { "git.example.com" } },
  check = function(items)
    local results = {}
    for _, item in ipairs(items) do
      term.notify("check " .. item.account .. " " .. item.path .. " " .. table.concat(item.branches, ",")
        .. " " .. tostring(item.signed_in))
      for _, branch in ipairs(item.branches) do
        results[#results + 1] = {
          account = item.account, path = item.path, branch = branch,
          state = branch == "merged-work" and "merged" or "open", number = 3,
        }
      end
    end
    term.forge_report("hub", results)
  end,
  discussion = function(request)
    term.forge_discussion("hub", request.id, {
      title = "Polish " .. request.branch, state = "open", draft = true, target = "main",
      body = "Makes it **shine**.\n\n- one\n- two\n\n```lua\nlocal x = 1\n```",
      reviews = { { author = "cy", state = "approved" } },
      checks = { { name = "build", state = "success" }, { name = "lint", state = "failed", url = "https://ci/lint" } },
      comments = { { author = "ann", body = "Nice" } },
      threads = { { path = "README", line = 1, comments = { { author = "bob", body = "Say hello" } } } },
    })
  end,
}
"#;

#[test]
fn the_agents_tab_keeps_finished_agents_open_and_merged_ones_collapsed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@git.example.com:team/app.git",
        ],
    );
    let mut paths = Vec::new();
    for branch in ["merged-work", "open-work"] {
        let path = dir.path().join(branch);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
        paths.push(cmdterm_env::canonicalize(&path).unwrap());
    }
    granted_extension(dir.path(), "hub", &["network"], HUB_FORGE);
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        let inner = ws.borrow();
        inner
            .snapshots
            .values()
            .any(|s| s.worktrees.len() == 3 && s.remote_url.is_some())
    });
    {
        let mut inner = ws.borrow_mut();
        let id = inner.state.projects[0].id;
        let project = inner.state.project_mut(id).unwrap();
        project
            .worktree_mut(&paths[0])
            .note_agent_title("Ship the merged thing");
        project
            .worktree_mut(&paths[1])
            .note_agent_title("Polish the open thing");
        // The main worktree's old title does not make it an agent row.
        project
            .worktree_mut(&repo)
            .note_agent_title("Old main task");
    }

    ws.check_forges_now();
    ws.poll();
    let checked: Vec<String> = ws
        .borrow()
        .notices
        .iter()
        .map(|n| n.text.clone())
        .filter(|t| t.starts_with("check "))
        .collect();
    assert_eq!(
        checked,
        ["check git.example.com team/app merged-work,open-work false"]
    );
    let rows = ws.borrow().agent_rows();
    let groups: Vec<_> = rows.iter().map(|r| (r.title.as_str(), r.group)).collect();
    assert_eq!(
        groups,
        [
            ("Ship the merged thing", crate::forges::AgentGroup::Merged),
            ("Polish the open thing", crate::forges::AgentGroup::Finished),
        ]
    );

    // A merged branch is not asked about again.
    ws.check_forges_now();
    ws.poll();
    assert!(
        ws.borrow()
            .notices
            .iter()
            .any(|n| n.text == "check git.example.com team/app open-work false")
    );

    ws.set_rail_tab(crate::workspace::RailTab::Agents);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Agents (1)"), "{json}");
    assert!(json.contains("Finished, not merged"), "{json}");
    assert!(json.contains("Polish the open thing"), "{json}");
    assert!(json.contains("!3 open"), "{json}");
    assert!(json.contains("Merged (1)"), "{json}");
    assert!(!json.contains("Ship the merged thing"), "{json}");
    assert!(!json.contains("Old main task"), "{json}");

    ws.toggle_agent_group(1);
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Ship the merged thing"), "{json}");
    assert!(json.contains("!3 merged"), "{json}");
    assert!(
        json.contains("Discussion of Polish the open thing"),
        "{json}"
    );

    // The discussion opens as a panel, its thread placed on the head.
    let id = ws.borrow().state.projects[0].id;
    ws.open_discussion(id, &paths[1]);
    let key = crate::forges::discussion_key(id, &paths[1]);
    let mut page = String::new();
    wait_until(|| {
        ws.poll();
        page = ws.discussion_for_test(&key).unwrap_or_default();
        page.starts_with('#')
    });
    assert!(page.contains("# !3 Polish open-work"), "{page}");
    assert!(page.contains("Say hello"), "{page}");
    assert!(page.contains(">    1  hi"), "{page}");
    assert!(
        page.contains("`open-work` → `main` · open · draft"),
        "{page}"
    );
    assert!(page.contains("✗ [lint](https://ci/lint)"), "{page}");
    assert!(page.contains("**cy** approved"), "{page}");

    // On the change request page of an explore panel of that worktree.
    let explore = ws.pane_explore(ws.focused_pane()).expect("explore panel");
    assert_eq!(explore.mode(), crate::explore::Mode::Change);
    wait_until(|| {
        ws.poll();
        explore.change_labels_for_test().len() > 1
    });
    assert_eq!(
        explore.change_labels_for_test(),
        [
            "Overview",
            "Checks 1/2",
            "Pushes (1)",
            "Conversation (1)",
            "README:1"
        ]
    );
    // The page is built from widgets: its list, the overview's cards, and
    // a thread picked from the list.
    ws.poll();
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Change request sections"), "{json}");
    assert!(json.contains("Polish open-work !3"), "{json}");
    assert!(
        json.contains("Open lint") || json.contains("Checks"),
        "{json}"
    );
    explore.select_section(4);
    ws.poll();
    assert!(ws.take_dirty().main);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Say hello"), "{json}");
    // `v` turns the panel back to its grid.
    let typed = app.dispatch_focused_input(char_key("v"));
    assert!(typed.is_handled());
    assert_eq!(explore.mode(), crate::explore::Mode::History);
    ws.poll();
    assert!(ws.take_dirty().main);
    explore.show_change();

    // The saved layout keeps the page.
    let spec = ws.with_active(|live| {
        live.unwrap().panels.iter().find_map(|(_, panel)| {
            matches!(panel, crate::workspace::LivePanel::Explore(_)).then(|| panel.spec())
        })
    });
    assert!(
        matches!(
            spec,
            Some(crate::state::PanelSpec::Explore { change: true, .. })
        ),
        "{spec:?}"
    );
}

#[test]
fn change_request_pages_of_worktrees_without_agents_stop_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@git.example.com:team/app.git",
        ],
    );
    let feature = dir.path().join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().unwrap(),
        ],
    );
    let feature = cmdterm_env::canonicalize(&feature).unwrap();
    // Only `feature` has a change request.
    let forge = HUB_FORGE.replace(
        r#"state = branch == "merged-work" and "merged" or "open", number = 3,"#,
        r#"state = branch == "feature" and "open" or "none",
          number = branch == "feature" and 3 or nil,"#,
    );
    assert_ne!(forge, HUB_FORGE);
    granted_extension(dir.path(), "hub", &["network"], &forge);
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        let inner = ws.borrow();
        inner
            .snapshots
            .values()
            .any(|s| s.worktrees.len() == 2 && s.remote_url.is_some())
    });
    // Neither worktree is an agent row, so no regular check asks about them.
    assert!(ws.borrow().agent_rows().is_empty());

    let id = ws.borrow().state.projects[0].id;
    let main = cmdterm_env::canonicalize(&repo).unwrap();
    for path in [&feature, &main] {
        ws.open_discussion(id, path);
    }
    let feature_key = crate::forges::discussion_key(id, &feature);
    let main_key = crate::forges::discussion_key(id, &main);
    assert_eq!(ws.discussion_for_test(&feature_key).unwrap(), "Waiting");
    ws.check_forges_now();
    let (mut shown, mut none) = (String::new(), String::new());
    wait_until(|| {
        ws.poll();
        shown = ws.discussion_for_test(&feature_key).unwrap_or_default();
        none = ws.discussion_for_test(&main_key).unwrap_or_default();
        shown.starts_with('#') && none != "Waiting"
    });
    assert!(shown.contains("# !3 Polish feature"), "{shown}");
    assert_eq!(none, "NoChange");

    // After a restart the saved page shows before the forge is asked.
    drop(ws);
    let ws = workspace(dir.path());
    let id = ws.borrow().state.projects[0].id;
    ws.open_discussion(id, &feature);
    let saved = ws.discussion_for_test(&feature_key).unwrap_or_default();
    assert!(saved.contains("# !3 Polish feature"), "{saved}");
}

#[test]
fn repositories_take_the_forge_account_of_their_owner() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@git.example.com:Team/app.git",
        ],
    );
    let path = dir.path().join("work");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "work",
            path.to_str().unwrap(),
        ],
    );
    let path = cmdterm_env::canonicalize(&path).unwrap();
    let source = HUB_FORGE.replace(
        r#"default = { "git.example.com" }"#,
        r#"default = { "git.example.com", "git.example.com/team" }"#,
    );
    granted_extension(dir.path(), "hub", &["network"], &source);
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        let inner = ws.borrow();
        inner
            .snapshots
            .values()
            .any(|s| s.worktrees.len() == 2 && s.remote_url.is_some())
    });
    {
        let mut inner = ws.borrow_mut();
        let id = inner.state.projects[0].id;
        let project = inner.state.project_mut(id).unwrap();
        project.worktree_mut(&path).note_agent_title("Team work");
    }
    let checks = |ws: &Workspace| -> Vec<String> {
        ws.check_forges_now();
        ws.poll();
        let mut inner = ws.borrow_mut();
        let checks = (inner.notices.iter())
            .map(|n| n.text.clone())
            .filter(|t| t.starts_with("check "))
            .collect();
        inner.notices.clear();
        checks
    };
    // Signed in to the host only: the team's account has no token.
    ws.sign_in_forge_for_test("hub.hub", "git.example.com", "host", "me");
    assert_eq!(
        checks(&ws),
        ["check git.example.com/team Team/app work false"]
    );
    ws.sign_in_forge_for_test("hub.hub", "git.example.com/team", "team", "bot");
    assert_eq!(
        checks(&ws),
        ["check git.example.com/team Team/app work true"]
    );
    assert_eq!(
        ws.forge_auth_for_test("hub.hub", "git.example.com/team"),
        Some(("Authorization".into(), "Bearer team".into()))
    );

    // Each account has its own row on the Accounts page.
    let json = accounts_page_json(&ws);
    assert!(json.contains("Signed in as me"), "{json}");
    assert!(json.contains("Signed in as bot"), "{json}");
    assert!(json.contains("Sign out of git.example.com/team"), "{json}");
}

/// Adds a `[[settings]]` block to the manifest `granted_extension` wrote.
fn add_manifest_setting(root: &Path, name: &str, block: &str) {
    let path = root.join("cfg/extensions").join(name).join("manifest.toml");
    let mut manifest = std::fs::read_to_string(&path).unwrap();
    manifest.push('\n');
    manifest.push_str(block);
    std::fs::write(path, manifest).unwrap();
}

fn extension_page_json(ws: &Workspace, name: &str) -> String {
    ws.open_settings();
    ws.show_settings_section(crate::ui::SettingsSection::Extensions);
    ws.show_extension_page(Some(name.into()));
    let mut app = crate::build_app(ws, VIEWPORT);
    app.frame();
    app.a11y_json()
}

#[test]
fn list_settings_add_and_remove_one_entry_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    granted_extension(
        dir.path(),
        "lists",
        &[],
        r#"term.notify("files " .. table.concat(term.settings().files, "|"))"#,
    );
    add_manifest_setting(
        dir.path(),
        "lists",
        "[[settings]]\nkey = \"files\"\ntitle = \"Files\"\ntype = \"list\"\ndefault = [\".gitconfig\"]\n",
    );
    let ws = workspace(dir.path());
    let json = extension_page_json(&ws, "lists");
    assert!(json.contains("Remove .gitconfig from Files"), "{json}");
    assert!(json.contains("New entry for Files"), "{json}");
    assert!(!json.contains("commas"), "{json}");

    // An entry with a comma stays one entry.
    ws.set_list_draft("lists", "files", "  .config/a,b  ");
    ws.add_list_entry("lists", "files");
    let files = |ws: &Workspace| ws.borrow().list_setting("lists", "files").unwrap();
    assert_eq!(files(&ws), [".gitconfig", ".config/a,b"]);
    ws.poll();
    let notices: Vec<String> = ws.borrow().notices.iter().map(|n| n.text.clone()).collect();
    assert!(
        notices.iter().any(|n| n == "files .gitconfig|.config/a,b"),
        "{notices:?}"
    );

    ws.set_list_draft("lists", "files", ".gitconfig");
    ws.add_list_entry("lists", "files");
    assert_eq!(files(&ws).len(), 2);
    let json = extension_page_json(&ws, "lists");
    assert!(json.contains(".gitconfig is in the list already"), "{json}");

    ws.remove_list_entry("lists", "files", 0);
    assert_eq!(files(&ws), [".config/a,b"]);
    let json = extension_page_json(&ws, "lists");
    assert!(!json.contains("Remove .gitconfig from Files"), "{json}");
    assert!(!json.contains("in the list already"), "{json}");
}

fn accounts_page_json(ws: &Workspace) -> String {
    settings_page_json(ws, crate::ui::SettingsSection::Accounts)
}

fn settings_page_json(ws: &Workspace, section: crate::ui::SettingsSection) -> String {
    ws.open_settings();
    ws.show_settings_section(section);
    let mut app = crate::build_app(ws, VIEWPORT);
    app.frame();
    app.a11y_json()
}

#[test]
fn sign_in_turns_on_once_a_token_is_typed_and_the_field_keeps_focus() {
    use schnellui::a11y::{Role, StateFlags};
    let dir = tempfile::tempdir().unwrap();
    granted_extension(dir.path(), "hub", &["network"], HUB_FORGE);
    let ws = workspace(dir.path());
    ws.open_settings();
    ws.show_settings_section(crate::ui::SettingsSection::Accounts);
    ws.toggle_account_form(&crate::forges::account_key("hub.hub", "git.example.com"));
    let disabled = |app: &schnellui::App| {
        let id = app
            .find_widget(Role::Button, Some("Sign in to git.example.com"))
            .unwrap();
        StateFlags(app.scene().a11y(id).unwrap().state).contains(StateFlags::DISABLED)
    };
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(disabled(&app));
    let _ = ws.take_dirty();
    ws.set_forge_token_input("hub.hub", "git.example.com", "t");
    assert!(ws.take_dirty().settings);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(!disabled(&app));
    // A second character changes nothing the form shows.
    ws.set_forge_token_input("hub.hub", "git.example.com", "to");
    assert!(!ws.take_dirty().settings);
}

#[test]
fn forge_accounts_are_added_and_removed_on_the_accounts_page() {
    let dir = tempfile::tempdir().unwrap();
    let source = HUB_FORGE.replace(
        r#"accounts = { default = { "git.example.com" } },"#,
        r#"accounts = { default = { "git.example.com" }, example = "git.example.com/team", hint = "A host or host/owner." },"#,
    );
    granted_extension(dir.path(), "hub", &["network"], &source);
    let ws = workspace(dir.path());
    // The add form opens under the forge's title.
    ws.toggle_account_form("hub.hub+add");
    let json = accounts_page_json(&ws);
    // A lone account never signed in has nothing to remove.
    assert!(!json.contains("Remove account git.example.com"), "{json}");
    assert!(
        json.contains("New Hub account (Such as git.example.com/team)"),
        "{json}"
    );
    assert!(json.contains("A host or host/owner."), "{json}");
    // The extension's page points to the Accounts page.
    let json = extension_page_json(&ws, "hub");
    assert!(json.contains("Open Accounts"), "{json}");
    assert!(!json.contains("Sign in to git.example.com"), "{json}");

    ws.set_forge_account_draft("hub.hub", "not a host!");
    ws.add_forge_account("hub.hub");
    let json = accounts_page_json(&ws);
    assert!(json.contains("not a host! is not a host"), "{json}");

    ws.set_forge_account_draft("hub.hub", " Git.Example.com/Team/ ");
    ws.add_forge_account("hub.hub");
    let hosts = |ws: &Workspace| ws.borrow().ext.forges()[0].hosts.clone();
    assert_eq!(hosts(&ws), ["git.example.com", "git.example.com/team"]);
    ws.toggle_account_form("hub.hub/git.example.com/team");
    let json = accounts_page_json(&ws);
    assert!(
        json.contains("Remove account git.example.com/team"),
        "{json}"
    );
    assert!(json.contains("Remove account git.example.com\""), "{json}");
    assert!(json.contains("Sign in to git.example.com/team"), "{json}");
    let saved = || {
        SettingsStore::new(dir.path().join("cfg/settings.json"))
            .load()
            .unwrap()
            .forge_hosts
    };
    assert_eq!(
        saved().get("hub.hub").map(Vec::as_slice),
        Some(
            &[
                "git.example.com".to_owned(),
                "git.example.com/team".to_owned()
            ][..]
        )
    );

    // Removing an account forgets its token too, and back at the
    // extension's accounts the settings keep no list.
    ws.sign_in_forge_for_test("hub.hub", "git.example.com/team", "tok", "bot");
    ws.remove_forge_account("hub.hub", "git.example.com/team");
    assert_eq!(hosts(&ws), ["git.example.com"]);
    assert!(
        !ws.borrow()
            .settings
            .forge_accounts
            .contains_key("hub.hub/git.example.com/team")
    );
    assert_eq!(
        ws.forge_auth_for_test("hub.hub", "git.example.com/team"),
        None
    );
    assert!(saved().is_empty());

    // The accounts survive a reload of the extensions.
    ws.set_forge_account_draft("hub.hub", "other.example");
    ws.add_forge_account("hub.hub");
    ws.reload_extensions();
    assert_eq!(hosts(&ws), ["git.example.com", "other.example"]);
}

#[test]
fn registry_logins_have_their_own_page_under_accounts() {
    let dir = tempfile::tempdir().unwrap();
    granted_extension(
        dir.path(),
        "reg",
        &["secrets"],
        r#"term.secret{ value = "real", hosts = { "registry.example" } }"#,
    );
    add_manifest_setting(
        dir.path(),
        "reg",
        "[logins]\ntitle = \"Reg\"\nsources = [\"~/.regrc\", \"REG_TOKEN\"]\n",
    );
    let ws = workspace(dir.path());
    // The Accounts page keeps to forge accounts and lists the subpage.
    let json = accounts_page_json(&ws);
    assert!(!json.contains("Reads file ~/.regrc"), "{json}");
    assert!(json.contains("Registry logins"), "{json}");
    let json = settings_page_json(&ws, crate::ui::SettingsSection::Registries);
    assert!(json.contains("Reads file ~/.regrc"), "{json}");
    assert!(json.contains("Reads variable REG_TOKEN"), "{json}");
    assert!(json.contains("Logins for registry.example"), "{json}");
    // Each registry opens the page of the extension that reads it.
    ws.open_settings();
    ws.show_settings_section(crate::ui::SettingsSection::Registries);
    let mut app = crate::build_app(&ws, VIEWPORT);
    let open = app
        .find_widget(
            schnellui::a11y::Role::Button,
            Some(&(crate::i18n::EN.logins_extension_for)("reg")),
        )
        .unwrap();
    app.dispatch_click(open);
    assert_eq!(
        ws.borrow().settings_ui.section,
        crate::ui::SettingsSection::Extensions
    );
    assert_eq!(ws.borrow().settings_ui.extension.as_deref(), Some("reg"));
}

#[test]
fn a_refused_sign_in_shows_why_and_keeps_the_app_running() {
    let dir = tempfile::tempdir().unwrap();
    granted_extension(dir.path(), "hub", &["network"], HUB_FORGE);
    let ws = workspace(dir.path());
    // What a whoami answer outside 2xx, or an unreachable host, ends in.
    ws.fail_forge_sign_in_for_test("hub.hub", "git.example.com", "HTTP 404");
    ws.poll();
    let notices: Vec<String> = ws.borrow().notices.iter().map(|n| n.text.clone()).collect();
    assert!(
        !notices.iter().any(|n| n.contains("carried on")),
        "{notices:?}"
    );
    let json = accounts_page_json(&ws);
    assert!(json.contains("Sign-in failed: HTTP 404"), "{json}");
    assert_eq!(ws.forge_auth_for_test("hub.hub", "git.example.com"), None);
}

#[test]
fn account_actions_end_in_a_toast_in_the_settings_header() {
    let dir = tempfile::tempdir().unwrap();
    granted_extension(dir.path(), "hub", &["network"], HUB_FORGE);
    let ws = workspace(dir.path());
    accounts_page_json(&ws);

    ws.set_forge_account_draft("hub.hub", "git.example.com/team");
    ws.add_forge_account("hub.hub");
    assert_eq!(
        ws.toast_for_test(),
        Some((false, "Added git.example.com/team".into()))
    );
    let json = accounts_page_json(&ws);
    assert!(json.contains("Added git.example.com/team"), "{json}");
    assert!(json.contains("\"Done\""), "{json}");

    ws.set_forge_account_draft("hub.hub", "not a host!");
    ws.add_forge_account("hub.hub");
    assert!(matches!(ws.toast_for_test(), Some((true, text)) if text.contains("is not a host")));

    ws.fail_forge_sign_in_for_test("hub.hub", "git.example.com", "HTTP 401");
    ws.poll();
    assert_eq!(
        ws.toast_for_test(),
        Some((true, "Sign-in to git.example.com failed: HTTP 401".into()))
    );
    let json = accounts_page_json(&ws);
    assert!(json.contains("\"Failed\""), "{json}");

    ws.sign_in_forge_for_test("hub.hub", "git.example.com", "tok", "me");
    assert_eq!(
        ws.toast_for_test(),
        Some((false, "Signed in to git.example.com as me".into()))
    );
    ws.forge_sign_out("hub.hub", "git.example.com");
    assert_eq!(
        ws.toast_for_test(),
        Some((false, "Signed out of git.example.com".into()))
    );
    ws.remove_forge_account("hub.hub", "git.example.com/team");
    assert_eq!(
        ws.toast_for_test(),
        Some((false, "Removed git.example.com/team".into()))
    );

    // The toast goes after its time, and with the overlay.
    let later = std::time::Instant::now() + crate::ui::TOAST_TTL;
    ws.borrow_mut().expire_toast(later);
    assert_eq!(ws.toast_for_test(), None);
    ws.forge_sign_out("hub.hub", "git.example.com");
    ws.close_settings();
    assert_eq!(ws.toast_for_test(), None);

    // A sign-in that ends after the overlay closed becomes a notice.
    ws.fail_forge_sign_in_for_test("hub.hub", "git.example.com", "HTTP 500");
    ws.poll();
    assert_eq!(ws.toast_for_test(), None);
    let notices: Vec<String> = ws.borrow().notices.iter().map(|n| n.text.clone()).collect();
    assert!(
        notices.contains(&"Sign-in to git.example.com failed: HTTP 500".to_owned()),
        "{notices:?}"
    );
}

#[test]
fn a_panic_in_work_for_an_extension_becomes_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    ws.contain(|| {
        let _held = ws.borrow_mut();
        panic!("boom");
    });
    // The borrow the panic held is released.
    ws.borrow_mut().dirty.status = true;
    let notices: Vec<String> = ws.borrow().notices.iter().map(|n| n.text.clone()).collect();
    assert!(notices.iter().any(|n| n.ends_with(": boom")), "{notices:?}");
}

#[test]
fn forge_sign_ins_keep_the_account_and_scope_the_token() {
    let dir = tempfile::tempdir().unwrap();
    granted_extension(dir.path(), "hub", &["network"], HUB_FORGE);
    let ws = workspace(dir.path());
    assert_eq!(ws.forge_auth_for_test("hub.hub", "git.example.com"), None);
    ws.sign_in_forge_for_test("hub.hub", "git.example.com", "tok", "me");
    assert_eq!(
        ws.forge_auth_for_test("hub.hub", "git.example.com"),
        Some(("Authorization".into(), "Bearer tok".into()))
    );
    let settings = SettingsStore::new(dir.path().join("cfg/settings.json"))
        .load()
        .unwrap();
    assert_eq!(
        settings
            .forge_accounts
            .get("hub.hub/git.example.com")
            .map(String::as_str),
        Some("me")
    );

    // The Accounts page shows the account, signed in.
    ws.open_settings();
    ws.show_settings_section(crate::ui::SettingsSection::Accounts);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Signed in as me"), "{json}");
    assert!(json.contains("Sign out of git.example.com"), "{json}");

    ws.forge_sign_out("hub.hub", "git.example.com");
    assert_eq!(ws.forge_auth_for_test("hub.hub", "git.example.com"), None);
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    assert!(
        app.a11y_json()
            .contains("Open the sign-in for git.example.com")
    );
    ws.toggle_account_form("hub.hub/git.example.com");
    app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains("Token for git.example.com"), "{json}");
    assert!(json.contains("Sign in to git.example.com"), "{json}");
}

#[test]
fn rail_context_menus_open_project_settings_and_remove_worktrees() {
    use crate::i18n::EN;
    use crate::ui::SettingsSection;
    use schnellui::a11y::Role;
    use schnellui::a11y::accesskit_reexport::{Action, ActionRequest, TreeId};
    use schnellui::a11y::to_access_id;

    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    wait_until(|| {
        ws.poll();
        !ws.borrow().snapshots.is_empty()
    });
    ws.add_worktree("feature/x");
    let feature = repo.with_file_name("app-feature-x");
    wait_until(|| {
        ws.poll();
        ws.borrow()
            .snapshots
            .values()
            .any(|snapshot| snapshot.worktrees.iter().any(|w| w.path == feature))
    });
    let id = ws.borrow().state.active_project.unwrap();
    assert_eq!(
        ws.borrow().state.active().unwrap().current_worktree(),
        feature
    );
    let main_branch = {
        let inner = ws.borrow();
        let snapshot = &inner.snapshots[&id];
        crate::ui::branch_name(snapshot.worktrees.iter().find(|w| w.is_main).unwrap())
    };

    crate::fit_grid(&ws, VIEWPORT);
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let open_menu = |app: &mut schnellui::App, name: &str| {
        let row = app.find_widget(Role::Button, Some(name)).unwrap();
        assert!(app.dispatch_action(&ActionRequest {
            action: Action::ShowContextMenu,
            target_tree: TreeId::ROOT,
            target_node: to_access_id(row),
            data: None,
        }));
        assert!(app.context_menu_is_open());
    };
    let remove = (EN.remove_worktree)("feature/x");

    // The main worktree cannot be removed.
    open_menu(&mut app, &main_branch);
    assert!(
        app.find_widget(Role::MenuItem, Some(EN.project_settings))
            .is_some()
    );
    assert!(app.find_widget(Role::MenuItem, Some(&remove)).is_none());
    app.dismiss_context_menu();

    // Removing the shown worktree switches to the main one first.
    open_menu(&mut app, "feature/x");
    let item = app.find_widget(Role::MenuItem, Some(&remove)).unwrap();
    app.activate_context_menu_item(item).unwrap();
    assert_eq!(ws.borrow().state.active().unwrap().current_worktree(), repo);
    wait_until(|| {
        ws.poll();
        !feature.exists()
            && ws.borrow().snapshots[&id]
                .worktrees
                .iter()
                .all(|w| w.path != feature)
    });

    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    open_menu(&mut app, "app");
    let item = app
        .find_widget(Role::MenuItem, Some(EN.project_settings))
        .unwrap();
    app.activate_context_menu_item(item).unwrap();
    assert!(ws.settings_is_open());
    assert_eq!(ws.borrow().settings_ui.section, SettingsSection::Project);
}

/// The Agents tab lists each agent terminal, not the worktree, and a
/// click on one shows the tab it is in and focuses its pane.
#[test]
fn agent_rows_are_agents_and_a_click_focuses_the_agents_pane() {
    use crate::state::SplitAxis;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    let id = ws.borrow().state.projects[0].id;
    let mark = |session: &str| {
        *ws.active_terminal().unwrap().agent_session.borrow_mut() =
            Some(crate::state::AgentSession {
                agent: "claude".into(),
                session: Some(session.into()),
                working: false,
            });
    };
    // Tab 0: a plain shell. Tab 1: an agent beside a shell. Tab 2: an agent.
    ws.open_terminal(None, None);
    ws.open_terminal(None, None);
    mark("first");
    ws.split_pane(SplitAxis::SideBySide);
    ws.open_terminal(None, None);
    mark("second");
    let rows = ws.borrow().agent_rows();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r.panel.is_some() && r.resume.is_none()));

    ws.select_tab(0);
    let first = rows[0].panel.unwrap();
    ws.focus_agent(id, &rows[0].path, first);
    ws.with_active(|live| {
        let live = live.unwrap();
        assert_eq!(Some(live.active), live.tab_of(first));
        assert_eq!(live.focused_panel().map(|p| p.0), Some(first));
    });
}

#[test]
fn clicking_a_binding_records_its_new_key_into_settings() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    ws.open_settings();
    ws.show_settings_section(crate::ui::SettingsSection::Keyboard);
    ws.record_binding(Some(crate::keymap::Action::Files));
    let mut app = crate::build_app(&ws, VIEWPORT);
    app.frame();
    let json = app.a11y_json();
    assert!(json.contains(crate::ui::KEY_RECORDER), "{json}");

    // The recorder has focus, so the chord binds instead of running.
    app.dispatch_focused_input(chord("b"));
    let settings = ws.borrow().settings.keybindings.clone();
    assert_eq!(
        settings
            .values()
            .filter(|v| v.as_deref() == Some("files"))
            .count(),
        1,
        "{settings:?}"
    );
    assert!(settings.values().any(Option::is_none), "{settings:?}");
    assert!(ws.borrow().settings_ui.recording.is_none());
    let crate::keymap::Press::Run(index) = ws.press_key(match &chord("b") {
        FocusedInputEvent::Key(key) => key,
        _ => unreachable!(),
    }) else {
        panic!("Ctrl+Shift+B runs nothing");
    };
    assert_eq!(
        ws.borrow().keymap.bindings[index].action,
        crate::keymap::Action::Files
    );
}

#[test]
fn a_taken_chord_moves_to_the_new_binding_and_reset_restores_both() {
    use crate::keymap::Action;
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path());
    let keys_of = |ws: &crate::workspace::Workspace, action: &Action| -> Vec<String> {
        ws.borrow()
            .keymap
            .bindings
            .iter()
            .filter(|b| &b.action == action)
            .map(|b| b.keys.to_settings())
            .collect()
    };
    let explore = keys_of(&ws, &Action::Explore);
    let files = keys_of(&ws, &Action::Files);
    assert_eq!(explore.len(), 1);
    ws.open_settings();
    ws.show_settings_section(crate::ui::SettingsSection::Keyboard);
    ws.record_binding(Some(Action::Files));
    let FocusedInputEvent::Key(key) = chord("h") else {
        unreachable!()
    };
    ws.record_key(&key);
    // Files took Explore's chord; Explore has none, and the toast says so.
    assert_eq!(keys_of(&ws, &Action::Files), explore);
    assert!(keys_of(&ws, &Action::Explore).is_empty());
    let toast = ws.borrow().settings_ui.toast.clone().unwrap();
    assert!(toast.text.contains("Explore history"), "{}", toast.text);
    assert!(ws.binding_changed(&Action::Files));
    assert!(ws.binding_changed(&Action::Explore));
    ws.reset_binding(&Action::Files);
    ws.reset_binding(&Action::Explore);
    assert_eq!(keys_of(&ws, &Action::Files), files);
    assert_eq!(keys_of(&ws, &Action::Explore), explore);
    assert!(ws.borrow().settings.keybindings.is_empty());
}

#[test]
fn clicking_a_panel_in_the_tasks_list_opens_its_tab() {
    use schnellui::a11y::Role;
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let ws = workspace(dir.path());
    add_project_and_wait(&ws, &repo);
    crate::fit_grid(&ws, VIEWPORT);
    ws.open_tasks();
    let mut app = crate::build_app(&ws, VIEWPORT);
    wait_for_mounted_text(&ws, &mut app, "1 worktree");
    let active = |ws: &crate::workspace::Workspace| ws.with_active(|live| live.unwrap().active);
    assert_eq!(active(&ws), 1);
    // Each panel's row is a button named after the panel and where it is.
    let json: serde_json::Value = serde_json::from_str(&app.a11y_json()).unwrap();
    fn find(node: &serde_json::Value, out: &mut Vec<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if map.get("role").and_then(|r| r.as_str()) == Some("button")
                    && let Some(name) = map.get("name").and_then(|n| n.as_str())
                {
                    out.push(name.to_owned());
                }
                map.values().for_each(|v| find(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| find(v, out)),
            _ => {}
        }
    }
    let mut names = Vec::new();
    find(&json, &mut names);
    let name = names
        .into_iter()
        .find(|name| name.contains("tab 1"))
        .expect("a row for the terminal in tab 1");
    let row = app.find_widget(Role::Button, Some(&name)).unwrap();
    app.dispatch_click(row);
    assert_eq!(active(&ws), 0);
}

/// A line dragged far to one side in a wide window does not leave that
/// pane a sliver once the window shrinks: it keeps an even split's room
/// or 160 px, whichever is less.
#[test]
fn a_dragged_split_keeps_its_panes_usable_in_a_small_window() {
    use crate::state::SplitAxis;
    use crate::workspace::split_sizes;
    let small = schnellui::scene::Size {
        width: 500.0,
        height: 300.0,
    };
    let (first, second) = split_sizes(small, SplitAxis::SideBySide, (1, 3), 100);
    let (even, _) = split_sizes(small, SplitAxis::SideBySide, (1, 3), 0);
    assert!(first.width >= even.width.floor(), "{first:?} vs {even:?}");
    let (first, second_wide) = split_sizes(small, SplitAxis::SideBySide, (1, 1), 950);
    assert!(second_wide.width >= 159.0, "{second_wide:?}");
    assert!(first.width + second.width > 0.0);
    // A wide window keeps the share as dragged.
    let wide = schnellui::scene::Size {
        width: 2000.0,
        height: 300.0,
    };
    let (first, _) = split_sizes(wide, SplitAxis::SideBySide, (1, 1), 200);
    assert!((first.width - 398.0).abs() < 4.0, "{first:?}");
}
