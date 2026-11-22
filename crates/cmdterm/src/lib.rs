//! scriptschnell-term: terminal and browser panels organized by VCS project and
//! worktree. See SOUL.md for what it is and AGENTS.md for how to work on it.

mod agent_sync;
mod agents;
mod blocklists;
mod browser;
mod bundle;
mod clipboard;
pub mod container;
// The control socket needs Unix sockets, and `fix` relays a Unix PTY.
#[cfg_attr(not(unix), allow(dead_code))]
mod control;
mod cpu;
mod debug_browser;
pub mod devtools;
mod discussion;
mod explore;
pub mod ext;
mod files;
#[cfg_attr(not(unix), allow(dead_code))]
mod fix;
pub mod forge;
mod forges;
mod fusion;
mod i18n;
mod keymap;
mod machines;
mod memory;
mod monitor;
mod notifications;
mod project_env;
mod runtime;
mod search;
pub mod state;
mod tasks;
mod taskview;
pub mod term;
mod terminal_panel;
mod ui;
pub mod vcs;
mod vcs_worker;
mod vpn;
mod workspace;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::state::{SettingsStore, StateStore};
use clap::Parser;
use schnellui::a11y::Role;
use schnellui::scene::Size;
use schnellui::widgets::TerminalCellMetrics;
use schnellui::{
    App, FocusedInputEvent, FocusedInputResult, RawFocusEvent, Remount, Shortcut,
    SubtreeReplacement, UiKey, WindowUpdate,
};

use terminal_panel::TERMINAL_NAMES;
use workspace::Workspace;

#[derive(Parser)]
#[command(name = "scriptschnell-term", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Projects to add on start: a path, host:/path, ssh://[user@]host[:port]/path,
    /// cmdterm://host[:port]/path[?vm=name], vm://name/path for a local VM, or
    /// tailscale://[user@]peer/path (also netbird://, nebula://) for a VPN peer.
    locations: Vec<String>,
    /// Open LOCATIONS as local sessions: shown until the app quits, never
    /// saved to state.json.
    #[arg(long)]
    temporary: bool,
    /// Open a global session: a folder without a repository, for work on
    /// the machine itself. Defaults to this machine's home; `host:` is the
    /// home on host. It is temporary until kept from the palette.
    #[arg(long, value_name = "LOCATION", num_args = 0..=1, default_missing_value = "")]
    global: Option<String>,
    /// Serve this machine to other scriptschnell-term instances while the app runs, on
    /// this address (default 0.0.0.0:7722). Keys live in remote/ of the
    /// configuration directory.
    #[arg(long, num_args = 0..=1, default_missing_value = "0.0.0.0:7722")]
    serve: Option<String>,
    /// One directory for everything the app keeps: settings.json, state.json,
    /// extensions/, logs and caches. Defaults to $SCRIPTSCHNELL_TERM_CONFIG_DIR,
    /// else the platform's configuration, data, state and cache directories.
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// Logical window width.
    #[arg(long, default_value_t = 1280)]
    width: u32,
    /// Logical window height.
    #[arg(long, default_value_t = 800)]
    height: u32,
    /// Terminal font size in logical pixels for this run. Overrides the
    /// saved setting without changing it.
    #[arg(long)]
    font_size: Option<f32>,
    /// Render one frame to this PNG and exit, without a window.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// With --screenshot: how long to let terminals produce output first.
    #[arg(long, default_value_t = 800)]
    settle_ms: u64,
    /// With --screenshot: open the command palette filtered by this text.
    #[arg(long)]
    palette: Option<String>,
    /// With --screenshot: open the settings at this section (project, font,
    /// color-scheme, keyboard, extensions, accounts, registries, agents, remote, vpn, sandboxing,
    /// sandbox-files or sandbox-hosts), or at `extensions/<name>` for one
    /// extension's page.
    #[arg(long)]
    settings: Option<String>,
    /// With --screenshot: open the setup at this stage (appearance,
    /// runtime, agents or hosts).
    #[arg(long)]
    setup: Option<String>,
    /// With --screenshot: check for updates, wait up to a minute for the
    /// result, and open the notifications.
    #[arg(long)]
    notifications: bool,
    /// With --screenshot: also write the accessibility tree as JSON.
    #[arg(long)]
    dump_a11y: Option<PathBuf>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start a coding agent, without the app, to repair its configuration.
    /// The agent opens in the configuration directory with a prompt that
    /// names the app's files; add what broke and send it.
    Fix {
        /// The agent's program, such as claude or codex. Defaults to the
        /// first agent on PATH whose integration is installed.
        #[arg(long)]
        agent: Option<String>,
    },
}

/// First argument that runs this executable as `cmdterm-proxy-bridge`.
#[cfg(unix)]
const PROXY_BRIDGE_FLAG: &str = cmdterm_env::BUILT_IN_BRIDGE_FLAG;

pub fn main() -> ExitCode {
    #[cfg(unix)]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().is_some_and(|first| first == PROXY_BRIDGE_FLAG) {
            std::process::exit(cmdterm_env::proxy_bridge_main(args));
        }
        cmdterm_env::set_built_in_proxy_bridge(PROXY_BRIDGE_FLAG);
    }
    let cli = Cli::parse();
    if let Some(Command::Fix { agent }) = &cli.command {
        let dirs = cmdterm_env::AppDirs::resolve(cli.config_dir.as_deref());
        dirs.migrate();
        let helpers = appimage_helpers(&dirs);
        if let Some(bin) = &helpers {
            cmdterm_env::set_proxy_bridge(bin.join("cmdterm-proxy-bridge"));
        }
        // The references the prompt names, even when the app never started.
        for error in bundle::install(&dirs) {
            eprintln!("scriptschnell-term: {error}");
        }
        return fix::main(&dirs, agent.as_deref());
    }
    let result = run(cli);
    // Extension data written in the last moments reaches the disk.
    crate::ext::flush_stores();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("scriptschnell-term: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Where the helper programs are when the app runs from an AppImage: copied
/// out of its mount into the data directory, see `cmdterm_env::appimage`.
/// `None` for other installs, or when the copy failed; the app then runs
/// them from beside itself.
fn appimage_helpers(dirs: &cmdterm_env::AppDirs) -> Option<std::path::PathBuf> {
    cmdterm_env::appimage::running_from()?;
    match cmdterm_env::appimage::install_helpers(&dirs.data.join("bin")) {
        Ok(bin) => Some(bin),
        Err(error) => {
            eprintln!(
                "scriptschnell-term: cannot copy the helper programs out of the AppImage: {error}"
            );
            None
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = cmdterm_env::AppDirs::resolve(cli.config_dir.as_deref());
    dirs.migrate();
    notifications::remove_update_leftovers();
    let helpers = appimage_helpers(&dirs);
    let mut client = cmdterm_env::RemoteClient::discover();
    if let Some(bin) = &helpers {
        cmdterm_env::set_proxy_bridge(bin.join("cmdterm-proxy-bridge"));
        client.program = bin.join("cmdterm-remote");
    }
    // Remote machine clients use this run's keys, not the default directory's.
    cmdterm_env::set_remote_client(cmdterm_env::RemoteClient {
        config_dir: dirs.is_single().then(|| dirs.config.clone()),
        ..client
    });
    for error in bundle::install(&dirs) {
        eprintln!("scriptschnell-term: {error}");
    }
    let store = StateStore::new(dirs.state.join("state.json"));
    let settings = SettingsStore::new(dirs.config.join("settings.json"));
    let ws = Workspace::with_dirs(store, settings, &dirs);
    match &cli.serve {
        Some(address) => {
            let (bound, fingerprint) = ws.serve(address)?;
            eprintln!(
                "scriptschnell-term: serving this machine on {bound}, host key {fingerprint}"
            );
        }
        None => ws.serve_from_settings(),
    }
    ws.fuse_from_settings();
    if let Some(size) = cli.font_size {
        ws.override_font_size(size);
    }
    ws.follow_system_appearance();
    ws.open_setup_if_unanswered();
    for location in &cli.locations {
        if cli.temporary {
            ws.add_temporary_project(location);
        } else {
            ws.add_project(location);
        }
    }
    if let Some(location) = &cli.global {
        ws.open_global_session(location);
    }

    let viewport = Size {
        width: cli.width as f32,
        height: cli.height as f32,
    };
    ws.fit_rail_to_window(viewport.width);
    fit_grid(&ws, viewport);
    let mut app = build_app(&ws, viewport);
    ws.set_redraw_signal(app.redraw_signal());

    if let Some(path) = &cli.screenshot {
        // Let shells print their prompt, then mount what they produced.
        let deadline = Instant::now() + Duration::from_millis(cli.settle_ms);
        while Instant::now() < deadline {
            ws.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        let extra = Instant::now() + Duration::from_secs(10);
        while !ws.borrow().pending_projects.is_empty() && Instant::now() < extra {
            ws.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        ws.poll();
        if let Some(query) = &cli.palette {
            ws.open_palette(None);
            ws.set_palette_query(query);
        }
        if cli.notifications {
            ws.check_updates();
            let extra = Instant::now() + Duration::from_secs(60);
            while ws.borrow().notifications.checking() && Instant::now() < extra {
                ws.poll();
                std::thread::sleep(Duration::from_millis(50));
            }
            ws.poll();
            ws.toggle_notifications();
        }
        if let Some(section) = &cli.settings {
            ws.open_settings();
            let (section, extension) = match section.split_once('/') {
                Some((section, name)) => (section, Some(name.to_owned())),
                None => (section.as_str(), None),
            };
            ws.show_settings_section(match section {
                "project" => ui::SettingsSection::Project,
                "color-scheme" => ui::SettingsSection::ColorScheme,
                "keyboard" => ui::SettingsSection::Keyboard,
                "extensions" => ui::SettingsSection::Extensions,
                "accounts" => ui::SettingsSection::Accounts,
                "registries" => ui::SettingsSection::Registries,
                "agents" => ui::SettingsSection::Agents,
                "remote" => ui::SettingsSection::Remote,
                "vpn" => ui::SettingsSection::Vpn,
                "sandboxing" => ui::SettingsSection::Sandboxing,
                "sandbox-files" => ui::SettingsSection::SandboxFiles,
                "sandbox-hosts" => ui::SettingsSection::SandboxHosts,
                _ => ui::SettingsSection::Font,
            });
            if extension.is_some() {
                ws.show_extension_page(extension);
            }
            let extra = Instant::now() + Duration::from_secs(10);
            while ws.borrow().settings_ui.machines.vpn.get().scanning && Instant::now() < extra {
                std::thread::sleep(Duration::from_millis(20));
            }
            // The agents page shows what the checks found, not the checks.
            let checking = |ws: &Workspace| {
                (ws.borrow().settings_ui.agents.rows().iter())
                    .any(|row| matches!(row.status, crate::agents::Status::Checking))
            };
            while checking(&ws) && Instant::now() < extra {
                ws.poll();
                std::thread::sleep(Duration::from_millis(20));
            }
            ws.poll();
        }
        if let Some(stage) = &cli.setup {
            let wanted = match stage.as_str() {
                "runtime" => ui::SetupStage::Runtime,
                "agents" => ui::SetupStage::Agents,
                "hosts" => ui::SetupStage::Hosts,
                _ => ui::SetupStage::Appearance,
            };
            ws.open_setup();
            while ws.setup_stage().is_some_and(|stage| stage != wanted) {
                ws.setup_next();
            }
            // The agents stage checks every agent on a thread.
            let checking = || {
                let rows = ws.borrow().settings_ui.agents.rows();
                rows.iter()
                    .any(|row| matches!(row.status, agents::Status::Checking))
            };
            let extra = Instant::now() + Duration::from_secs(10);
            while checking() && Instant::now() < extra {
                ws.poll();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        fit_grid(&ws, viewport);
        ws.take_dirty();
        app = build_app(&ws, viewport);
        app.frame();
        if let Some(a11y) = &cli.dump_a11y {
            app.dump_a11y(a11y)?;
        }
        app.render_to_png(path)?;
        println!("wrote {}", path.display());
        crate::devtools::hub().shutdown();
        return Ok(());
    }

    let mut mounted = viewport;
    // Where the settings overlay's focus belonged at its last rebuild.
    let mut settings_key = ws.settings_focus_key();
    let serving = ws.clone();
    let updates = move |viewport: Size| -> Option<WindowUpdate> {
        ws.poll();
        if viewport != mounted {
            mounted = viewport;
            ws.fit_rail_to_window(viewport.width);
            fit_grid(&ws, viewport);
            ws.take_dirty();
            settings_key = ws.settings_focus_key();
            return Some(WindowUpdate::Remount(Box::new(Remount::new(
                build_app(&ws, viewport),
                "resize",
            ))));
        }
        let dirty = ws.take_dirty();
        if !dirty.any() {
            return None;
        }
        if dirty.main || dirty.full {
            ws.batch_hidden_sessions();
        }
        if dirty.full {
            // A font change alters the cell size, so the grid is refit first.
            fit_grid(&ws, viewport);
            ws.take_dirty();
            // The remount keeps focus on the same control. When the
            // settings asked for another, such as the row of a binding just
            // reset, the next update rebuilds them and moves it there.
            let key = ws.settings_focus_key();
            if key.is_some() && key != settings_key {
                ws.borrow_mut().dirty.settings = true;
                if let Some(signal) = ws.redraw_handle().get() {
                    signal.request_redraw();
                }
            } else {
                settings_key = key;
            }
            return Some(WindowUpdate::Remount(Box::new(Remount::new(
                build_app(&ws, viewport),
                "full",
            ))));
        }
        let regions = ws.regions();
        let mut replacements = Vec::with_capacity(4);
        // The rail's search field stays, so typing in it keeps its focus.
        // A hidden rail has no regions to rebuild.
        let rail_shown = !ws.borrow().rail_hidden;
        if dirty.sidebar && rail_shown {
            replacements.push(SubtreeReplacement::new(
                regions.sidebar_list,
                ui::sidebar_list(&ws, viewport.height),
                "sidebar",
            ));
            replacements.push(SubtreeReplacement::new(
                regions.sidebar_footer,
                ui::sidebar_footer(&ws),
                "sidebar footer",
            ));
            replacements.push(SubtreeReplacement::new(
                regions.rail_tabs,
                ui::rail_tabs(&ws),
                "rail tabs",
            ));
        }
        // With the rail hidden the bell is in the tab strip, which then
        // follows what would have rebuilt the rail's footer.
        // A drag holds a tab or a pane's title strip; rebuilding them would
        // end it. A changed title waits until the drag ends.
        let dragging = ws.is_dragging();
        if dirty.main
            || (dirty.tabs && !dragging)
            || (dirty.sidebar && !rail_shown)
            || dirty.focus_tabs
        {
            let tabs = SubtreeReplacement::new(
                regions.tabs,
                ui::tabs(&ws, ui::main_width(&ws, viewport)),
                "tabs",
            );
            // F6 is the keyboard's, so the ring shows after a click too.
            replacements.push(if dirty.focus_tabs {
                tabs.with_focus_after(regions.active_tab).with_focus_ring()
            } else {
                tabs
            });
        }
        if dirty.main || dirty.status || dirty.refocus || dirty.focus_status {
            let status = SubtreeReplacement::new(
                regions.status,
                ui::status(&ws, ui::main_width(&ws, viewport)),
                "status",
            );
            // A ref on the bar focuses its first control.
            replacements.push(if dirty.refocus {
                status.with_focus_after(regions.focus)
            } else if dirty.focus_status {
                status.with_focus_after(regions.status).with_focus_ring()
            } else {
                status
            });
        }
        if dirty.main {
            replacements.push(panel_replacement(&ws, viewport));
        } else if dirty.tabs && !dragging {
            // A panel's new title also names its split pane.
            for (pane, view) in ui::pane_titles(&ws, ui::panel_size(&ws, viewport)) {
                replacements.push(SubtreeReplacement::new(
                    regions.pane_titles[pane],
                    view,
                    "pane title",
                ));
            }
        }
        if dirty.search && rail_shown {
            let target = if ws.borrow().rail_search_focus {
                regions.rail_search
            } else {
                regions.focus
            };
            replacements.push(
                SubtreeReplacement::new(regions.rail_search, ui::rail_search(&ws), "rail search")
                    .with_focus_after(target),
            );
        }
        // Last, so focus lands in the rebuilt panel when the palette closes.
        if dirty.palette {
            let target = if ws.palette_is_open() {
                regions.palette_input
            } else if dirty.focus_tabs {
                regions.active_tab
            } else if dirty.new_panel {
                regions.new_panel
            } else if dirty.search && ws.borrow().rail_search_focus {
                regions.rail_search
            } else {
                regions.focus
            };
            replacements.push(
                SubtreeReplacement::new(regions.palette, ui::palette(&ws, viewport), "palette")
                    .with_focus_after(target),
            );
        } else if dirty.palette_list {
            replacements.push(SubtreeReplacement::new(
                regions.palette_list,
                ui::palette_list(&ws),
                "palette list",
            ));
        }
        if dirty.settings {
            let key = ws.settings_focus_key();
            let replacement =
                SubtreeReplacement::new(regions.settings, ui::settings(&ws, viewport), "settings");
            // A change inside the same page, such as a toggled switch or a
            // closed dropdown, keeps focus where it was: the rebuilt
            // overlay's control with the same role and name takes it.
            let kept = match (&key, &settings_key) {
                (Some(key), Some(before)) => key == before || key.closed_picker_of(before),
                _ => false,
            };
            replacements.push(if kept {
                replacement
            } else if key.is_some() {
                replacement.with_focus_after(regions.settings_focus)
            } else if settings_key.is_some()
                && ws.borrow().settings_ui.from_rail
                && !ws.borrow().rail_hidden
            {
                // Closed: focus goes back to the rail's Settings button
                // that opened it.
                replacement.with_focus_after(regions.settings_button)
            } else {
                replacement.with_focus_after(regions.focus)
            });
            settings_key = key;
        }
        if dirty.notifications {
            let target = if ws.notifications_is_open() {
                regions.notifications_focus
            } else if dirty.bell {
                regions.bell
            } else if ws.settings_is_open() {
                regions.settings_focus
            } else {
                regions.focus
            };
            replacements.push(
                SubtreeReplacement::new(
                    regions.notifications,
                    ui::notifications(&ws, viewport),
                    "notifications",
                )
                .with_focus_after(target),
            );
        }
        if dirty.setup {
            let target = if ws.setup_is_open() {
                regions.setup_focus
            } else if ws.settings_is_open() {
                regions.settings_focus
            } else {
                regions.focus
            };
            replacements.push(
                SubtreeReplacement::new(regions.setup, ui::setup(&ws, viewport), "setup")
                    .with_focus_after(target),
            );
        }
        if dirty.approval {
            let target = if ws.approval_is_open() {
                regions.approval_focus
            } else if ws.settings_is_open() {
                regions.settings_focus
            } else {
                regions.focus
            };
            replacements.push(
                SubtreeReplacement::new(regions.approval, ui::approval(&ws, viewport), "approval")
                    .with_focus_after(target),
            );
        }
        Some(WindowUpdate::Subtrees(replacements))
    };
    // SCHNELLUI_HEADLESS_WINDOWED=1 runs this same loop offscreen (see AGENTS.md).
    let result = app.run_windowed_with_viewport_updates("scriptschnell-term", updates);
    serving.stop_serving();
    serving.stop_sidecars(true);
    crate::devtools::hub().shutdown();
    result
}

/// Rebuilds the panel region. While the palette is open the panel must not take
/// focus, or keys would reach the terminal behind the modal.
fn panel_replacement(ws: &Workspace, viewport: Size) -> SubtreeReplacement {
    let regions = ws.regions();
    let replacement = SubtreeReplacement::new(
        regions.panel,
        ui::panel(ws, ui::panel_size(ws, viewport)),
        "panel",
    );
    if ws.palette_is_open()
        || ws.settings_is_open()
        || ws.notifications_is_open()
        || ws.setup_is_open()
    {
        replacement
    } else {
        replacement.with_focus_after(regions.focus)
    }
}

/// Sizes the terminal grid to the panel area.
fn fit_grid(ws: &Workspace, viewport: Size) {
    let metrics = ws.borrow().metrics;
    let area = ui::panel_size(ws, viewport);
    let cols = (area.width / metrics.width).floor().clamp(2.0, 1000.0) as u16;
    let rows = (area.height / metrics.height).floor().clamp(1.0, 1000.0) as u16;
    ws.set_panel_area(area, cols, rows);
}

fn build_app(ws: &Workspace, viewport: Size) -> App {
    let theme = ws.borrow().scheme.theme();
    let mut app = App::mount_with_theme_size_scaled(
        theme,
        ui::root(ws, viewport),
        viewport.width as u32,
        viewport.height as u32,
        1.0,
    );
    app.set_clear_color(theme.page);

    let metrics = ws.borrow().metrics;
    for (pane, name) in TERMINAL_NAMES.into_iter().enumerate() {
        let terminal = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            terminal_input(&terminal, pane, metrics, event)
        });
    }

    for (pane, name) in files::FILES_NAMES.into_iter().enumerate() {
        let panel = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            files_input(&panel, pane, metrics, event)
        });
        // Rows that open on a click show the pointing hand.
        let panel = ws.clone();
        app.register_named_cursor_provider(Role::ScrollView, Some(name), move || {
            panel
                .pane_files(pane)
                .map_or(schnellui::widgets::CursorIcon::Default, |files| {
                    files.cursor()
                })
        });
    }

    for (pane, name) in explore::EXPLORE_NAMES.into_iter().enumerate() {
        let panel = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            explore_input(&panel, pane, metrics, event)
        });
        let panel = ws.clone();
        app.register_named_cursor_provider(Role::ScrollView, Some(name), move || {
            panel
                .pane_explore(pane)
                .map_or(schnellui::widgets::CursorIcon::Default, |explore| {
                    explore.cursor()
                })
        });
    }

    for (pane, name) in monitor::MONITOR_NAMES.into_iter().enumerate() {
        let panel = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            monitor_input(&panel, pane, metrics, event)
        });
    }

    for (pane, name) in taskview::TASKS_NAMES.into_iter().enumerate() {
        let panel = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            tasks_input(&panel, pane, event)
        });
    }

    for (pane, name) in debug_browser::DEBUG_BROWSER_NAMES.into_iter().enumerate() {
        let panel = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            if let Some(result) = app_chord(&panel, pane, &event) {
                return result;
            }
            if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
                panel.focus_pane(pane);
            }
            panel.debug_browser_input(pane, event)
        });
    }

    #[cfg(feature = "browser")]
    for (pane, name) in browser::WEB_CONTENT_NAMES.into_iter().enumerate() {
        let web = ws.clone();
        app.register_focused_input_handler(Role::ScrollView, Some(name), move |event| {
            if let Some(result) = app_chord(&web, pane, &event) {
                return result;
            }
            if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
                web.focus_pane(pane);
            }
            web.browser_input(pane, event)
        });
    }

    // The Keyboard page's recorder takes every key while it records; leaving
    // it stops the recording.
    let recorder = ws.clone();
    app.register_focused_input_handler(Role::ScrollView, Some(ui::KEY_RECORDER), move |event| {
        match event {
            FocusedInputEvent::Key(key) => {
                recorder.record_key(&key);
                FocusedInputResult::Handled
            }
            FocusedInputEvent::Focus(RawFocusEvent::WidgetLost) => {
                if recorder.borrow().settings_ui.recording.is_some() {
                    recorder.record_binding(None);
                }
                FocusedInputResult::Handled
            }
            _ => FocusedInputResult::Ignored,
        }
    });

    // Escape on a control of the rail, the tab strip or the status bar
    // returns to the panel, the way out of the chrome back to typing.
    // Overlays keep their own Escape.
    for role in [Role::Button, Role::Tab] {
        let keys = ws.clone();
        app.register_focused_key_handler(role, None, move |key| {
            key == UiKey::Escape && keys.refocus_panel()
        });
    }
    let palette_fields = [ui::PALETTE_INPUT, ui::PALETTE_PANEL_INPUT]
        .into_iter()
        .chain(ui::InputMode::ALL.map(ui::InputMode::input_name));
    for name in palette_fields {
        let keys = ws.clone();
        app.register_focused_key_handler(Role::TextInput, Some(name), move |key| {
            match key {
                UiKey::Enter => keys.palette_enter(),
                UiKey::Escape => keys.palette_escape(),
                UiKey::Up { .. } => keys.move_palette_highlight(-1),
                UiKey::Down { .. } => keys.move_palette_highlight(1),
                // In a list, Tab moves the highlight as the arrows do, so focus
                // never lands on a row while another stays highlighted. An input
                // mode's Tab reaches its buttons.
                UiKey::Tab { shift } if keys.borrow().palette.input.is_none() => {
                    keys.move_palette_highlight(if shift { -1 } else { 1 })
                }
                UiKey::PageUp => keys.move_palette_highlight(-8),
                UiKey::PageDown => keys.move_palette_highlight(8),
                _ => return false,
            }
            true
        });
    }
    // The settings' find field: Escape empties it before it closes the
    // settings, the arrows step through the pages that match, and Enter
    // moves on to the shown page.
    let keys = ws.clone();
    app.register_focused_key_handler(Role::TextInput, Some(ui::SETTINGS_FIND_INPUT), move |key| {
        match key {
            UiKey::Escape => keys.escape_settings_query(),
            UiKey::Up { .. } => keys.step_settings_result(-1),
            UiKey::Down { .. } => keys.step_settings_result(1),
            UiKey::Enter => keys.leave_settings_query(),
            _ => false,
        }
    });
    // The settings' nav is one list: Up and Down step through its pages,
    // Home and End jump to the first and last, and Tab leaves for the page.
    // Both languages' titles, so the keys keep working after a switch.
    let titles = ui::SettingsSection::ALL
        .into_iter()
        .flat_map(|page| [page.title(&i18n::EN), page.title(&i18n::DE)]);
    for name in titles {
        let keys = ws.clone();
        app.register_focused_key_handler(Role::Button, Some(name), move |key| match key {
            UiKey::Up { .. } => keys.step_settings_nav(name, -1),
            UiKey::Down { .. } => keys.step_settings_nav(name, 1),
            UiKey::Home { .. } => {
                keys.step_settings_nav(name, -(ui::SettingsSection::ALL.len() as isize))
            }
            UiKey::End { .. } => {
                keys.step_settings_nav(name, ui::SettingsSection::ALL.len() as isize)
            }
            _ => false,
        });
    }
    let keys = ws.clone();
    app.register_focused_key_handler(Role::TextInput, Some(ui::RAIL_SEARCH_INPUT), move |key| {
        match key {
            UiKey::Enter => keys.open_rail_highlight(),
            UiKey::Escape => keys.escape_rail_search(),
            UiKey::Up { .. } => keys.move_rail_highlight(-1),
            UiKey::Down { .. } => keys.move_rail_highlight(1),
            _ => return false,
        }
        true
    });
    for name in [ui::PALETTE_BRANCH_INPUT, ui::PALETTE_VM_INPUT] {
        let keys = ws.clone();
        app.register_focused_key_handler(Role::TextInput, Some(name), move |key| {
            match key {
                UiKey::Enter => keys.palette_enter(),
                UiKey::Escape => keys.palette_escape(),
                _ => return false,
            }
            true
        });
    }
    // Bindings for while no terminal or web pane has focus. Changing the
    // keymap remounts, which registers them again.
    let shortcuts: Vec<(Shortcut, keymap::Action)> = ws
        .borrow()
        .keymap
        .shortcuts()
        .map(|(shortcut, action)| (shortcut, action.clone()))
        .collect();
    for (shortcut, action) in shortcuts {
        let target = ws.clone();
        app.register_shortcut(shortcut, move || {
            target.run_action(&action, target.focused_pane());
        });
    }

    // The mouse's back and forward buttons step through the settings pages.
    let navigation = ws.clone();
    app.register_navigation_handler(move |step| navigation.navigate_settings(step));

    // The window opens without the system frame unless the settings ask for
    // it; the tab strip then carries its buttons, and its empty part moves it.
    let frame = ws.clone();
    app.set_window_decorations_provider(move || !frame.draws_title_bar());
    app.set_window_move_area(|p| p.y < ui::TAB_HEIGHT);

    let title = ws.clone();
    app.set_window_title_provider(move || {
        let inner = title.borrow();
        match inner.state.active() {
            Some(project) => format!(
                "{} · {} · scriptschnell-term",
                project.name,
                project.current_worktree().display()
            ),
            None => "scriptschnell-term".to_owned(),
        }
    });

    if ws.palette_is_open()
        && let Some(input) = app.resolve_ref(ws.regions().palette_input)
    {
        app.focus(Some(input));
        return app;
    }
    if ws.settings_is_open()
        && let Some(row) = app.resolve_ref(ws.regions().settings_focus)
    {
        app.focus_without_ring(Some(row));
        return app;
    }
    if ws.notifications_is_open()
        && let Some(close) = app.resolve_ref(ws.regions().notifications_focus)
    {
        app.focus_without_ring(Some(close));
        return app;
    }
    if ws.setup_is_open()
        && let Some(done) = app.resolve_ref(ws.regions().setup_focus)
    {
        app.focus_without_ring(Some(done));
        return app;
    }
    let pane = ws.focused_pane().min(TERMINAL_NAMES.len() - 1);
    let names = [
        TERMINAL_NAMES[pane],
        files::FILES_NAMES[pane],
        explore::EXPLORE_NAMES[pane],
        monitor::MONITOR_NAMES[pane],
        taskview::TASKS_NAMES[pane],
        debug_browser::DEBUG_BROWSER_NAMES[pane],
        browser::WEB_CONTENT_NAMES[pane],
    ];
    if let Some(widget) = names
        .into_iter()
        .find_map(|name| app.find_widget(Role::ScrollView, Some(name)))
    {
        // The pane's border marks the focused pane; a ring around a whole
        // panel adds nothing.
        app.focus_without_ring(Some(widget));
    }
    app
}

/// App keybindings shared by every pane's handler. They win over the panel.
fn app_chord(ws: &Workspace, pane: usize, event: &FocusedInputEvent) -> Option<FocusedInputResult> {
    let FocusedInputEvent::Key(key) = event else {
        return None;
    };
    match ws.press_key(key) {
        keymap::Press::Ignored => return None,
        keymap::Press::Consumed => {}
        keymap::Press::Run(index) => ws.run_binding(index, pane),
    }
    Some(FocusedInputResult::Handled)
}

/// Focused input for the terminal grid of `pane`: app chords first, then the
/// pane's terminal.
fn terminal_input(
    ws: &Workspace,
    pane: usize,
    metrics: TerminalCellMetrics,
    event: FocusedInputEvent,
) -> FocusedInputResult {
    if let Some(result) = app_chord(ws, pane, &event) {
        return result;
    }
    if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
        ws.focus_pane(pane);
    }
    match ws.pane_terminal(pane) {
        Some(active) => {
            let result = active.input(event, metrics);
            if let Some(url) = active.take_link() {
                ws.open_browser(&url);
            }
            result
        }
        None => FocusedInputResult::Ignored,
    }
}

/// Focused input for the file panel of `pane`: app chords first, then the
/// panel. Opening another file saves the layout, so it comes back after a
/// restart.
fn files_input(
    ws: &Workspace,
    pane: usize,
    metrics: TerminalCellMetrics,
    event: FocusedInputEvent,
) -> FocusedInputResult {
    if let Some(result) = app_chord(ws, pane, &event) {
        return result;
    }
    if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
        ws.focus_pane(pane);
    }
    let Some(files) = ws.pane_files(pane) else {
        return FocusedInputResult::Ignored;
    };
    let result = files.input(event, metrics);
    if files.take_opened() {
        ws.save();
    }
    result
}

/// Focused input for the explore panel of `pane`: app chords first, then
/// the panel. A mode or review option change saves the layout; `b` opens
/// the palette to ask for a base branch.
fn explore_input(
    ws: &Workspace,
    pane: usize,
    metrics: TerminalCellMetrics,
    event: FocusedInputEvent,
) -> FocusedInputResult {
    if let Some(result) = app_chord(ws, pane, &event) {
        return result;
    }
    if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
        ws.focus_pane(pane);
    }
    let Some(explore) = ws.pane_explore(pane) else {
        return FocusedInputResult::Ignored;
    };
    let result = explore.input(event, metrics);
    if explore.take_changed() {
        ws.save();
    }
    if explore.take_ask_base() {
        ws.open_palette(Some(ui::InputMode::ReviewBase));
    }
    result
}

/// Focused input for the monitor panel of `pane`: app chords first, then
/// the panel.
fn monitor_input(
    ws: &Workspace,
    pane: usize,
    metrics: TerminalCellMetrics,
    event: FocusedInputEvent,
) -> FocusedInputResult {
    if let Some(result) = app_chord(ws, pane, &event) {
        return result;
    }
    if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
        ws.focus_pane(pane);
    }
    match ws.pane_monitor(pane) {
        Some(monitor) => monitor.input(event, metrics),
        None => FocusedInputResult::Ignored,
    }
}

/// Focused input for the tasks panel of `pane`: app chords first, then the
/// panel.
fn tasks_input(ws: &Workspace, pane: usize, event: FocusedInputEvent) -> FocusedInputResult {
    if let Some(result) = app_chord(ws, pane, &event) {
        return result;
    }
    if matches!(event, FocusedInputEvent::Focus(RawFocusEvent::WidgetGained)) {
        ws.focus_pane(pane);
    }
    let Some(tasks) = ws.pane_tasks(pane) else {
        return FocusedInputResult::Ignored;
    };
    tasks.input(event)
}
