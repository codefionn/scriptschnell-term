//! The setup overlay. The first start walks through its stages: language,
//! keymap and color scheme; where new worktrees run their terminals; which
//! coding agents get hooks; and, while sandboxes reach only allowed hosts,
//! which common hosts they may reach, one switch per [`HOST_GROUPS`] entry.
//! The containers page opens the hosts stage alone. Every choice applies at
//! once; Done and dismissing both finish the setup, so it does not come back.

use crate::state::{HOST_GROUPS, Runtime, SYSTEM_LANGUAGE, SYSTEM_SCHEME, Settings};
use cmdterm_env::Environment;
use schnellui::scene::{Color, Size};
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Checkbox, Column, Dialog, DialogPosition, Justify, ListRow,
    Row, Scroll, Switch, Text, ThemeProvider, View, WrapMode,
};

use super::settings::{SWATCHES_WIDTH, lane_swatches};
use super::theme::{SCHEMES, Scheme, resolve};
use super::{Closed, Fill, Named, truncate};
use crate::agents::{Agent, Status};
use crate::i18n::{Lang, Strings};
use crate::keymap::PRESETS;
use crate::workspace::Workspace;

const WIDTH: f32 = 600.0;
const PAD: f32 = 24.0;
/// Height of the choice buttons and the dialog's own buttons.
const BUTTON_HEIGHT: f32 = 30.0;
/// Space between two groups of choices.
const GROUP_GAP: f32 = 18.0;
const INNER: f32 = WIDTH - PAD * 2.0;
const MAX_LIST_HEIGHT: f32 = 380.0;
const CHOICE_GAP: f32 = 8.0;
/// Width of a choice button: three to a row.
const CHOICE_WIDTH: f32 = (INNER - CHOICE_GAP * 2.0) / 3.0;
/// An accent bar that does not show, so every row's title starts at the
/// same edge as the chosen row's.
const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
/// Room for the scroll bar right of the switches.
const ROW_WIDTH: f32 = INNER - super::settings::SCROLLBAR_ROOM;
const ROW_HEIGHT: f32 = 28.0;
/// An agent's row with its status line, and the gap below it.
const AGENT_ROW_HEIGHT: f32 = 48.0;

/// A page of the setup overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupStage {
    /// Language, keymap and color scheme.
    Appearance,
    /// Where new worktrees run their terminals.
    Runtime,
    /// Which coding agents get hooks.
    Agents,
    /// Which common hosts sandboxes may reach.
    Hosts,
}

/// The open setup overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// Every stage, as on the first start, rather than the hosts alone.
    full: bool,
    stage: SetupStage,
}

impl Setup {
    /// The stages this setup shows, in order. The hosts stage shows only
    /// when new worktrees run in sandboxes that reach allowed hosts only.
    fn stages(self, settings: &Settings) -> &'static [SetupStage] {
        use SetupStage::{Agents, Appearance, Hosts};
        if !self.full {
            return &[Hosts];
        }
        let sandbox = &settings.sandbox;
        if settings.default_runtime == Runtime::Sandbox
            && sandbox.network
            && sandbox.restrict_network
        {
            &[Appearance, SetupStage::Runtime, Agents, Hosts]
        } else {
            &[Appearance, SetupStage::Runtime, Agents]
        }
    }
}

impl Workspace {
    /// Opens every stage of the setup, starting with the first.
    pub fn open_setup(&self) {
        self.open_setup_at(Setup {
            full: true,
            stage: SetupStage::Appearance,
        });
    }

    /// Opens the hosts stage alone.
    pub fn open_host_setup(&self) {
        self.open_setup_at(Setup {
            full: false,
            stage: SetupStage::Hosts,
        });
    }

    fn open_setup_at(&self, setup: Setup) {
        let mut inner = self.borrow_mut();
        inner.setup = Some(setup);
        inner.dirty.setup = true;
    }

    /// Opens the setup unless the user already finished it.
    pub fn open_setup_if_unanswered(&self) {
        if !self.borrow().settings.sandbox.hosts_chosen {
            self.open_setup();
        }
    }

    /// Closes the setup and remembers that it was finished.
    pub fn finish_setup(&self) {
        {
            let mut inner = self.borrow_mut();
            if inner.setup.take().is_none() {
                return;
            }
            inner.settings.sandbox.hosts_chosen = true;
            inner.dirty.setup = true;
            // The containers page lists the hosts the switches changed.
            inner.dirty.settings |= inner.settings_ui.open;
        }
        self.save_settings();
    }

    pub fn setup_is_open(&self) -> bool {
        self.borrow().setup.is_some()
    }

    /// The stage the setup shows, if it is open.
    pub fn setup_stage(&self) -> Option<SetupStage> {
        self.borrow().setup.map(|setup| setup.stage)
    }

    /// Shows the next stage, or finishes the setup after the last one.
    pub fn setup_next(&self) {
        if !self.move_setup(1) {
            self.finish_setup();
        }
    }

    /// Shows the previous stage.
    pub fn setup_back(&self) {
        self.move_setup(-1);
    }

    /// Moves `delta` stages. Returns false when that leaves the stages.
    fn move_setup(&self, delta: isize) -> bool {
        let stage = {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let Some(setup) = inner.setup.as_mut() else {
                return true;
            };
            let stages = setup.stages(&inner.settings);
            let at = stages.iter().position(|s| *s == setup.stage).unwrap_or(0);
            let Some(&stage) = at.checked_add_signed(delta).and_then(|at| stages.get(at)) else {
                return false;
            };
            setup.stage = stage;
            inner.dirty.setup = true;
            stage
        };
        if stage == SetupStage::Agents {
            let setup = self.borrow().settings_ui.agents.clone();
            setup.refresh(vec![Environment::Local], self.redraw_handle());
        }
        true
    }

    /// Sets where the terminals of worktrees the app has not seen yet run.
    pub fn set_default_runtime(&self, runtime: Runtime) {
        {
            let mut inner = self.borrow_mut();
            if inner.settings.default_runtime == runtime {
                return;
            }
            inner.settings.default_runtime = runtime;
            inner.dirty.setup = true;
            inner.dirty.settings |= inner.settings_ui.open;
        }
        self.save_settings();
    }

    /// Runs `change`, then rebuilds the setup, whose selection or stage
    /// count `change` moved.
    fn in_setup(&self, change: impl FnOnce(&Self)) {
        change(self);
        self.borrow_mut().dirty.setup = true;
    }

    /// Allows or forbids the hosts of group `id`. The switch shows its new
    /// state on its own, so the overlay does not rebuild.
    pub fn allow_host_group(&self, id: &str, on: bool) {
        let Some(group) = HOST_GROUPS.iter().find(|g| g.id == id) else {
            return;
        };
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            if !inner.settings.sandbox.allow_group(group, on) {
                return;
            }
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
        }
        self.save_settings();
    }
}

/// The overlay: a dialog while open, an empty slot while closed.
pub fn setup(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let inner = ws.borrow();
    let Some(setup) = inner.setup else {
        return Box::new(Closed);
    };
    let (scheme, tr, focus) = (inner.scheme, inner.tr, inner.regions.setup_focus);
    let stages = setup.stages(&inner.settings);
    drop(inner);
    let at = stages.iter().position(|s| *s == setup.stage).unwrap_or(0);
    let last = at + 1 == stages.len();

    let (title, intro, body): (&str, &str, Box<dyn View>) = match setup.stage {
        SetupStage::Appearance => (
            tr.setup_appearance,
            tr.setup_appearance_intro,
            Box::new(appearance(ws, scheme, tr)),
        ),
        SetupStage::Runtime => (
            tr.setup_runtime,
            tr.setup_runtime_intro,
            Box::new(runtime(ws, scheme, tr)),
        ),
        SetupStage::Agents => (
            tr.setup_agents,
            tr.setup_agents_intro,
            Box::new(agents(ws, viewport, tr)),
        ),
        SetupStage::Hosts => (
            tr.host_setup_dialog,
            tr.host_setup_intro,
            Box::new(hosts(ws, scheme, viewport, tr)),
        ),
    };

    let mut header = Column::new().width(INNER).gap(6.0);
    if stages.len() > 1 {
        header = header.child(note(scheme, (tr.setup_step)(at + 1, stages.len())));
    }
    header = header
        .child(Text::new(title).size(18.0).bold())
        .child(note(scheme, intro));

    let mut buttons = Row::new().width(INNER).gap(8.0).justify(Justify::End);
    if at > 0 {
        let back = ws.clone();
        buttons = buttons.child(
            Button::new(tr.setup_back)
                .appearance(ButtonAppearance::Outline)
                .width(96.0)
                .height(BUTTON_HEIGHT)
                .text_size(13.0)
                .on_click(move || back.setup_back()),
        );
    }
    let next = ws.clone();
    buttons = buttons.child(
        Button::new(if last {
            tr.host_setup_done
        } else {
            tr.setup_next
        })
        .height(BUTTON_HEIGHT)
        .width(96.0)
        .text_size(13.0)
        .on_click(move || next.setup_next())
        .with_ref(focus),
    );

    let dismiss = ws.clone();
    Box::new(
        Dialog::new(if setup.full {
            tr.setup_dialog
        } else {
            tr.host_setup_dialog
        })
        .modal()
        .undecorated()
        .position(DialogPosition::Center)
        .width(WIDTH)
        .max_width(WIDTH)
        .backdrop(scheme.backdrop)
        .surface(scheme.rail)
        .padding(PAD)
        .gap(GROUP_GAP)
        .on_dismiss(move || dismiss.finish_setup())
        .child(header)
        .child(body)
        .child(buttons),
    )
}

/// Language, keymap and color scheme.
fn appearance(ws: &Workspace, scheme: &Scheme, tr: &'static Strings) -> impl View + use<> {
    let (language, keymap, color_scheme, system_dark) = {
        let inner = ws.borrow();
        (
            inner.settings.language.clone(),
            inner.settings.keymap.clone(),
            inner.settings.color_scheme.clone(),
            inner.system_dark(),
        )
    };

    let mut languages = Row::new().gap(CHOICE_GAP);
    let system_name = (tr.system_language)(Lang::system().name());
    for (id, name) in [(SYSTEM_LANGUAGE, system_name)]
        .into_iter()
        .chain(Lang::ALL.map(|lang| (lang.id(), lang.name().to_owned())))
    {
        let pick = ws.clone();
        languages = languages.child(choice(scheme, name, language == id, move || {
            pick.in_setup(|ws| ws.set_language(id))
        }));
    }

    // A grid of three columns, since six names do not fit in one row; the
    // second row starts at the same edge as the first.
    let mut keymaps = Column::new().gap(CHOICE_GAP);
    for presets in PRESETS.chunks(3) {
        let mut row = Row::new().gap(CHOICE_GAP);
        for preset in presets {
            let pick = ws.clone();
            row = row.child(choice(
                scheme,
                preset.name,
                keymap == preset.id,
                move || pick.in_setup(|ws| ws.set_keymap(preset.id)),
            ));
        }
        keymaps = keymaps.child(row);
    }
    let description = PRESETS
        .iter()
        .find(|preset| preset.id == keymap)
        .map_or("", |preset| (tr.preset_description)(preset));

    let following = resolve(SYSTEM_SCHEME, system_dark);
    let system = ws.clone();
    // Each row ends in the scheme's lane colors, as on the settings page,
    // so the choice is not made blind.
    let swatched = |row: ListRow, lanes: [Color; 6], chosen: bool| {
        Row::new()
            .width(INNER)
            .gap(-SWATCHES_WIDTH)
            .align(Align::Center)
            .child(
                row.width(INNER)
                    .end_room(SWATCHES_WIDTH)
                    .selected(chosen)
                    .selected_background(scheme.selected())
                    .accent_bar(if chosen { scheme.lanes[0] } else { TRANSPARENT }),
            )
            .child(lane_swatches(lanes))
    };
    let mut schemes = Column::new().width(INNER).child(swatched(
        ListRow::new(tr.system)
            .subtitle((tr.follows_desktop)(following.name))
            .dot(following.lanes[0])
            .on_click(move || system.in_setup(|ws| ws.set_color_scheme(SYSTEM_SCHEME))),
        following.lanes,
        color_scheme == SYSTEM_SCHEME,
    ));
    for option in SCHEMES {
        let pick = ws.clone();
        schemes = schemes.child(swatched(
            ListRow::new(option.name)
                .trailing(if option.dark { tr.dark } else { tr.light })
                .height(ROW_HEIGHT)
                .dot(option.lanes[0])
                .on_click(move || pick.in_setup(|ws| ws.set_color_scheme(option.id))),
            option.lanes,
            color_scheme == option.id,
        ));
    }

    Column::new()
        .width(INNER)
        .gap(GROUP_GAP)
        .child(group(tr.language).child(languages))
        .child(
            group(tr.keymap_picker)
                .child(keymaps)
                .child(note(scheme, description)),
        )
        .child(group(tr.section_color_scheme).child(schemes))
}

/// A titled group of choices: a bold label over its controls.
fn group(title: &'static str) -> Column {
    Column::new().width(INNER).gap(8.0).child(label(title))
}

/// What the sandbox option says: on Windows, why it is not recommended.
pub(crate) fn sandbox_note(tr: &'static Strings) -> &'static str {
    if cfg!(windows) {
        tr.setup_sandbox_note_windows
    } else {
        tr.setup_sandbox_note
    }
}

/// Where new worktrees run their terminals, and the sandbox's network.
fn runtime(ws: &Workspace, scheme: &Scheme, tr: &'static Strings) -> impl View + use<> {
    let (current, network, restrict) = {
        let inner = ws.borrow();
        let sandbox = &inner.settings.sandbox;
        (
            inner.settings.default_runtime,
            sandbox.network,
            sandbox.restrict_network,
        )
    };
    let mut list = Column::new().width(INNER);
    for (runtime, title, subtitle) in [
        (Runtime::Sandbox, tr.run_in_sandbox, sandbox_note(tr)),
        (
            Runtime::Devcontainer,
            tr.run_in_devcontainer,
            tr.setup_devcontainer_note,
        ),
        (Runtime::Host, tr.run_on_host, tr.setup_host_note),
    ] {
        let pick = ws.clone();
        // A dot in front of each option, filled in the accent for the
        // chosen one, so the three read as one choice.
        let mut row = ListRow::new(title)
            .subtitle(subtitle)
            .width(INNER)
            .dot(if current == runtime {
                scheme.lanes[0]
            } else {
                scheme.theme().media
            })
            .selected(current == runtime)
            .selected_background(scheme.selected())
            .accent_bar(if current == runtime {
                scheme.lanes[0]
            } else {
                TRANSPARENT
            })
            .on_click(move || pick.set_default_runtime(runtime));
        // The Windows sandbox breaks too many tools to recommend it.
        if runtime == Runtime::Sandbox && !cfg!(windows) {
            row = row
                .trailing(tr.setup_recommended)
                .trailing_pill(scheme.theme().accent);
        }
        list = list.child(row);
    }
    let mut column = Column::new().width(INNER).gap(6.0).child(list);
    if current == Runtime::Sandbox {
        let toggle = ws.clone();
        column = column.child(switch_row(
            scheme,
            16.0,
            tr.sandbox_network,
            network,
            move |on| toggle.in_setup(|ws| ws.set_sandbox_network(on)),
        ));
        if network {
            let toggle = ws.clone();
            column = column.child(switch_row(
                scheme,
                32.0,
                tr.sandbox_restrict,
                restrict,
                move |on| toggle.in_setup(|ws| ws.set_sandbox_restrict_network(on)),
            ));
        }
    }
    column.child(note(scheme, tr.setup_runtime_note))
}

/// A checkbox per coding agent on this machine. Checking one installs its
/// hooks, unchecking removes them.
fn agents(ws: &Workspace, viewport: Size, tr: &'static Strings) -> impl View + use<> {
    let rows: Vec<_> = (ws.borrow().settings_ui.agents.rows().into_iter())
        .filter(|row| row.env.is_local())
        .collect();
    let height = (rows.len() as f32 * AGENT_ROW_HEIGHT).min(list_height(viewport));
    let mut list = Column::new().width(ROW_WIDTH).gap(4.0);
    for row in rows {
        let name = row.agent.name();
        let status = if row.busy {
            tr.working.to_owned()
        } else {
            tr.agent_status(&row.status)
        };
        let ready = !row.busy && !matches!(row.status, Status::Checking | Status::NotFound);
        let mut line = Row::new().width(ROW_WIDTH).align(Align::Center).gap(8.0);
        if ready {
            let checked = matches!(row.status, Status::On | Status::Outdated);
            let change = ws.clone();
            let agent: Agent = row.agent;
            line =
                line.child(Checkbox::new(checked).name(name).on_toggle(move |on| {
                    change.set_agent_integration(agent, Environment::Local, on)
                }));
        }
        list = list.child(
            line.child(
                ListRow::new(name)
                    .subtitle(truncate(&status, 72))
                    .width(ROW_WIDTH - 32.0),
            ),
        );
    }
    Scroll::new()
        .label(tr.agent_list)
        .scrollbar(true)
        .size(INNER, height)
        .child(list)
}

/// A switch per [`HOST_GROUPS`] entry.
fn hosts(
    ws: &Workspace,
    scheme: &Scheme,
    viewport: Size,
    tr: &'static Strings,
) -> impl View + use<> {
    let allowed: Vec<bool> = {
        let inner = ws.borrow();
        HOST_GROUPS
            .iter()
            .map(|group| inner.settings.sandbox.group_allowed(group))
            .collect()
    };
    let mut list = Column::new().width(ROW_WIDTH).gap(4.0);
    for (group, on) in HOST_GROUPS.iter().zip(allowed) {
        let title = (tr.host_group)(group.id);
        let toggle = ws.clone();
        let id = group.id;
        list = list.child(
            Row::new()
                .width(ROW_WIDTH)
                .align(Align::Center)
                .child(
                    ListRow::new(title)
                        .subtitle(truncate(&group.hosts.join(", "), 72))
                        .width(ROW_WIDTH - 44.0),
                )
                .child(Named(
                    title.into(),
                    ThemeProvider::new(
                        scheme.switch_theme(),
                        Switch::new(on).on_toggle(move |on| toggle.allow_host_group(id, on)),
                    ),
                )),
        );
    }
    Column::new()
        .width(INNER)
        .gap(10.0)
        .child(
            Scroll::new()
                .label(tr.host_setup_dialog)
                .scrollbar(true)
                .size(INNER, list_height(viewport))
                .child(list),
        )
        .child(note(scheme, tr.host_setup_note))
}

fn list_height(viewport: Size) -> f32 {
    MAX_LIST_HEIGHT.min(viewport.height - 300.0).max(120.0)
}

/// One button of a group that picks one option, the current one marked.
fn choice(
    scheme: &Scheme,
    text: impl Into<std::borrow::Cow<'static, str>>,
    on: bool,
    pick: impl FnMut() + 'static,
) -> ThemeProvider {
    // Every option has a frame, so the ones not chosen still read as
    // buttons. The chosen one takes the selected rows' tint inside an
    // accent frame, so it never looks like the solid Next button.
    let mut theme = scheme.theme();
    let text = text.into();
    let mut button = Button::new(text.clone());
    if on {
        theme.surface = scheme.selected();
        theme.outline = scheme.lanes[0];
        // A check mark too, as on the settings' chosen segments, so the
        // pick does not rest on color alone.
        button = button.display(format!("✓ {text}"));
    }
    ThemeProvider::new(
        theme,
        button
            .appearance(ButtonAppearance::Outline)
            .width(CHOICE_WIDTH)
            .height(BUTTON_HEIGHT)
            .text_size(13.0)
            .on_click(pick),
    )
}

/// A title with a switch at its right.
/// `indent` moves the title right, under the switch it depends on.
fn switch_row(
    scheme: &Scheme,
    indent: f32,
    title: &'static str,
    on: bool,
    toggle: impl FnMut(bool) + 'static,
) -> Row {
    Row::new()
        .width(INNER)
        .align(Align::Center)
        .child(Fill::new(indent, 1.0, TRANSPARENT))
        .child(
            ListRow::new(title)
                .width(INNER - 44.0 - indent)
                .height(ROW_HEIGHT),
        )
        .child(Named(
            title.into(),
            ThemeProvider::new(scheme.switch_theme(), Switch::new(on).on_toggle(toggle)),
        ))
}

fn label(text: &'static str) -> impl View + use<> {
    Text::new(text).size(13.5).bold()
}

/// Muted text that wraps at the dialog's width.
fn note(scheme: &Scheme, text: impl Into<std::borrow::Cow<'static, str>>) -> impl View {
    Column::new().width(INNER).child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(12.0).wrap(WrapMode::Word),
    ))
}

#[cfg(test)]
mod tests {
    use crate::state::{SettingsStore, StateStore};
    use schnellui::a11y::Role;

    use super::*;
    use crate::i18n::EN;

    const VIEWPORT: Size = Size {
        width: 1000.0,
        height: 700.0,
    };

    /// Rebuilds the overlay the way the window loop does.
    fn rebuild(ws: &Workspace, app: &mut schnellui::App) {
        assert!(ws.take_dirty().setup);
        let regions = ws.regions();
        app.apply_subtree_replacement(
            schnellui::SubtreeReplacement::new(regions.setup, setup(ws, VIEWPORT), "setup")
                .with_focus_after(regions.setup_focus),
        )
        .unwrap();
        app.frame();
    }

    fn click(ws: &Workspace, app: &mut schnellui::App, role: Role, name: &str) {
        let widget = app.find_widget(role, Some(name)).unwrap();
        app.dispatch_click(widget);
        rebuild(ws, app);
    }

    #[test]
    fn the_first_start_walks_through_every_stage_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = || {
            Workspace::new(
                StateStore::new(root.join("cfg/state.json")),
                SettingsStore::new(root.join("cfg/settings.json")),
                &root.join("cfg/extensions"),
            )
        };
        let first = ws();
        // The hosts stage shows only for sandboxes, which Windows does not
        // pick by default.
        first.borrow_mut().settings.default_runtime = Runtime::Sandbox;
        first.open_setup_if_unanswered();
        assert!(first.setup_is_open());
        assert!(first.take_dirty().setup);
        let mut app = crate::build_app(&first, VIEWPORT);
        app.frame();
        let next = app.find_widget(Role::Button, Some(EN.setup_next)).unwrap();
        assert_eq!(app.focused_widget(), Some(next));
        assert!(app.find_widget(Role::Button, Some(EN.setup_back)).is_none());

        click(&first, &mut app, Role::Button, "tmux");
        assert_eq!(first.borrow().settings.keymap, "tmux");
        click(&first, &mut app, Role::Button, EN.setup_next);
        assert_eq!(first.setup_stage(), Some(SetupStage::Runtime));

        click(&first, &mut app, Role::Button, EN.setup_back);
        assert_eq!(first.setup_stage(), Some(SetupStage::Appearance));
        click(&first, &mut app, Role::Button, EN.setup_next);
        click(&first, &mut app, Role::Button, EN.setup_next);
        assert_eq!(first.setup_stage(), Some(SetupStage::Agents));
        assert!(
            app.find_widget(Role::ScrollView, Some(EN.agent_list))
                .is_some()
        );
        click(&first, &mut app, Role::Button, EN.setup_next);
        assert_eq!(first.setup_stage(), Some(SetupStage::Hosts));

        let jvm = (EN.host_group)("jvm");
        let switch = app.find_widget(Role::Switch, Some(jvm)).unwrap();
        app.dispatch_click(switch);
        let done = app
            .find_widget(Role::Button, Some(EN.host_setup_done))
            .unwrap();
        app.dispatch_click(done);
        assert!(!first.setup_is_open());

        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert!(saved.sandbox.hosts_chosen);
        assert_eq!(saved.keymap, "tmux");
        let hosts = &saved.sandbox.allowed_hosts;
        assert!(hosts.contains(&"repo1.maven.org".to_owned()));
        assert!(hosts.contains(&"api.exa.ai".to_owned()));
        assert!(hosts.contains(&"mcp.exa.ai".to_owned()));
        let second = ws();
        second.open_setup_if_unanswered();
        assert!(!second.setup_is_open());
    }

    #[test]
    fn running_new_worktrees_on_the_host_skips_the_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        // Windows starts on the host already, so start from the sandbox.
        ws.borrow_mut().settings.default_runtime = Runtime::Sandbox;
        ws.open_setup();
        ws.take_dirty();
        let mut app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        click(&ws, &mut app, Role::Button, EN.setup_next);
        let host = app.find_widget(Role::Button, Some(EN.run_on_host));
        app.dispatch_click(host.unwrap());
        rebuild(&ws, &mut app);
        assert_eq!(ws.borrow().settings.default_runtime, Runtime::Host);
        click(&ws, &mut app, Role::Button, EN.setup_next);
        assert_eq!(ws.setup_stage(), Some(SetupStage::Agents));
        assert!(
            app.find_widget(Role::Button, Some(EN.host_setup_done))
                .is_some(),
            "the agents stage is the last one"
        );
        click(&ws, &mut app, Role::Button, EN.host_setup_done);
        assert!(!ws.setup_is_open());
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.default_runtime, Runtime::Host);
    }

    #[test]
    fn the_containers_page_opens_the_hosts_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_host_setup();
        assert_eq!(ws.setup_stage(), Some(SetupStage::Hosts));
        ws.setup_next();
        assert!(!ws.setup_is_open());
    }

    #[test]
    fn resetting_settings_opens_the_setup_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.set_font_size(20.0);
        ws.open_setup();
        ws.finish_setup();
        ws.open_settings();
        // Reset sits at the end of the Project page.
        ws.show_settings_section(crate::ui::SettingsSection::Project);
        let mut app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        let reset = app
            .find_widget(Role::Button, Some(EN.reset_settings))
            .unwrap();
        app.dispatch_click(reset);
        assert!(ws.settings_is_open(), "the first click only asks");
        app.frame();
        let reset = app
            .find_widget(Role::Button, Some(EN.reset_settings))
            .unwrap();
        app.dispatch_click(reset);
        assert!(!ws.settings_is_open());
        assert_eq!(ws.setup_stage(), Some(SetupStage::Appearance));

        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.font_size, crate::state::Settings::default().font_size);
        assert!(!saved.sandbox.hosts_chosen);
        assert!(
            saved
                .sandbox
                .allowed_hosts
                .contains(&"localhost".to_owned())
        );
    }
}
