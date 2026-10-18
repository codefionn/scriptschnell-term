//! The status bar under the panel: environment, branch, working directory and
//! grid size of the focused terminal on the left, the newest notice on the
//! right.

use schnellui::layout::EdgeInsets;
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, Justify, Pad, Row, Stack, Text, ThemeProvider, View,
};
use schnellui_icons_md::{MdIcon, outlined};

use super::{Fill, Named, STATUS_HEIGHT, tail_path, truncate};

/// Characters of the working directory shown in the status bar.
const STATUS_PATH_CHARS: usize = 60;
use crate::i18n::Strings;
use crate::workspace::{LivePanel, LiveWorktree, Workspace};

/// Longest notice shown, in characters.
const NOTICE_CHARS: usize = 90;
/// Room on each side of a part, inside its button.
const SIDE: f32 = 3.0;

/// What a part of the status bar shows, which picks its icon.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    /// This machine.
    Local,
    /// Another machine: SSH, a remote machine, a VM.
    Machine,
    /// A sandbox, a devcontainer, or one being started.
    Runtime,
    /// A sandbox whose terminals reach the host's container engines, which
    /// start containers outside it.
    RuntimeOpen,
    /// The CPU mode where the project runs; a click switches it.
    Cpu,
    Branch,
    Directory,
    Grid,
}

impl Part {
    fn icon(self) -> (&'static str, &'static str) {
        match self {
            Part::Local => ("computer", outlined::ICON_COMPUTER),
            Part::Machine => ("dns", outlined::ICON_DNS),
            Part::Runtime => ("shield", outlined::ICON_SHIELD),
            Part::RuntimeOpen => ("gpp_maybe", outlined::ICON_GPP_MAYBE),
            Part::Cpu => ("speed", outlined::ICON_SPEED),
            Part::Branch => ("fork_right", outlined::ICON_FORK_RIGHT),
            Part::Directory => ("folder", outlined::ICON_FOLDER),
            Part::Grid => ("aspect_ratio", outlined::ICON_ASPECT_RATIO),
        }
    }
}

/// The left-hand parts: environment, branch, cwd and `columns×rows`.
#[cfg(test)]
pub fn status_parts(ws: &Workspace) -> Vec<String> {
    status_items(ws).into_iter().map(|(_, text)| text).collect()
}

fn status_items(ws: &Workspace) -> Vec<(Part, String)> {
    let inner = ws.borrow();
    let mut parts = Vec::with_capacity(6);
    let Some(project) = inner.state.active() else {
        return parts;
    };
    let machine = if matches!(project.env, cmdterm_env::Environment::Local) {
        Part::Local
    } else {
        Part::Machine
    };
    parts.push((machine, project.env.label().to_owned()));
    let tr = inner.tr;
    if inner.active_runtime_preparing() {
        parts.push((Part::Runtime, tr.runtime_starting.to_owned()));
    } else {
        match inner.active_runtime() {
            crate::state::Runtime::Host => {}
            crate::state::Runtime::Devcontainer => {
                parts.push((Part::Runtime, tr.runtime_devcontainer.to_owned()))
            }
            // Engine passthrough lets terminals start containers outside
            // the sandbox, so the item takes the warning color and icon, and
            // the item names the cause; its tooltip says more. Once the user
            // keeps passthrough on purpose, the item is plain again.
            crate::state::Runtime::Sandbox => {
                let sandbox = &inner.settings.sandbox;
                if sandbox.engines == crate::state::EngineAccess::Passthrough
                    && !sandbox.engines_acknowledged
                {
                    parts.push((Part::RuntimeOpen, tr.runtime_sandbox_open.to_owned()))
                } else {
                    parts.push((Part::Runtime, tr.runtime_sandbox.to_owned()))
                }
            }
        }
    }
    if cmdterm_env::cpu::applies(&project.env) {
        let modes = &inner.settings.cpu_mode;
        let mode = modes.of(crate::cpu::Place::of(&project.env));
        parts.push((Part::Cpu, (tr.cpu_status)(mode.name(tr))));
    }
    if let Some(branch) = inner.active_branch() {
        parts.push((Part::Branch, branch.to_owned()));
    }
    let terminal = inner
        .active_live()
        .and_then(LiveWorktree::focused_panel)
        .and_then(|(_, panel)| match panel {
            LivePanel::Terminal(terminal) => Some(terminal.clone()),
            LivePanel::Browser(_)
            | LivePanel::Files(_)
            | LivePanel::Explore(_)
            | LivePanel::Monitor(_)
            | LivePanel::DebugBrowser(_)
            | LivePanel::Tasks(_) => None,
        });
    match &terminal {
        Some(terminal) => {
            let cwd = terminal.current_dir().or_else(|| terminal.cwd.clone());
            parts.push((
                Part::Directory,
                tail_path(
                    cwd.as_deref().unwrap_or_else(|| project.current_worktree()),
                    STATUS_PATH_CHARS,
                ),
            ));
            let (columns, rows) = inner.pane_grid();
            parts.push((Part::Grid, format!("{columns}×{rows}")));
        }
        None => parts.push((
            Part::Directory,
            tail_path(project.current_worktree(), STATUS_PATH_CHARS),
        )),
    }
    parts
}

/// Rough width of one 12.5 px character, to size the notice to the space left.
const CHAR_WIDTH: f32 = 7.0;
const GAP: f32 = 16.0;
/// Height of the dividers between the parts.
const DIVIDER_HEIGHT: f32 = 12.0;
const PAD: f32 = 12.0;
/// Characters of a notice the parts make room for.
const NOTICE_LEAST: usize = 24;
/// Size of the icon before each part, and the space after it.
const ICON: f32 = 13.0;
const ICON_GAP: f32 = 4.0;
/// Room left and right of a warning's text on its pill.
const PILL_PAD: f32 = 8.0;
/// Share of the warning color in a warning pill's fill, out of 256, on
/// dark and on light schemes.
const PILL_SHARE: (u16, u16) = (40, 44);
/// Share of the ink in a warning's text, out of 256, on dark and on light
/// schemes: a step toward the ink keeps 4.5:1 on the tinted pill, and a
/// light scheme takes a longer one, so the text reads plainly.
const PILL_INK_SHARE: (u16, u16) = (112, 176);

/// The CPU part with a button over it that switches the mode where the
/// active project runs to the next one.
fn cpu_switch(ws: &Workspace, width: f32, content: impl View) -> Stack {
    let (place, hint, tip) = {
        let inner = ws.borrow();
        let tr = inner.tr;
        let place = inner
            .state
            .active()
            .map_or(crate::cpu::Place::Local, |project| {
                crate::cpu::Place::of(&project.env)
            });
        let modes = &inner.settings.cpu_mode;
        let mode = modes.of(place);
        // The name starts with the part's visible text.
        let status = (tr.cpu_status)(mode.name(tr));
        let hint = (tr.cpu_status_hint)(&status, mode.next().name(tr));
        let tip = (tr.cpu_status_tip)(mode.name(tr), mode.next().name(tr));
        (place, hint, tip)
    };
    let switch = ws.clone();
    Stack::new()
        .size(width, STATUS_HEIGHT - 4.0)
        .align(Align::Center)
        .justify(Justify::Start)
        .child(content)
        .child(
            Button::new(hint)
                .icon_only()
                .tooltip(tip)
                .appearance(ButtonAppearance::Ghost)
                .width(width)
                .height(STATUS_HEIGHT - 4.0)
                .on_click(move || {
                    let mode = switch.borrow().settings.cpu_mode.of(place);
                    switch.set_cpu_mode(place, mode.next());
                }),
        )
}

/// What a click on a part opens, with the button's name: the branch its
/// history, the directory its files, the machine its settings. The grid size
/// only informs.
/// A click on a status bar part.
type PartAction = fn(&Workspace);

fn part_action(kind: Part, tr: &'static Strings) -> Option<(&'static str, PartAction)> {
    match kind {
        Part::Branch => Some((tr.status_open_history, |ws| ws.open_explore(false))),
        Part::Directory => Some((tr.status_open_files, |ws| ws.open_files())),
        Part::Local | Part::Machine => Some((tr.status_open_machines, |ws| {
            ws.show_settings_section(super::SettingsSection::Remote);
            ws.open_settings();
        })),
        _ => None,
    }
}

/// Width a part takes in the bar, about, with its icon and the gap after it.
fn part_width(kind: Part, text: &str) -> f32 {
    let pill = if kind == Part::RuntimeOpen {
        PILL_PAD * 2.0
    } else {
        0.0
    };
    text.chars().count() as f32 * CHAR_WIDTH + GAP + ICON + ICON_GAP + pill + SIDE * 2.0
}

/// Drops or shortens parts until they fit in `room`, least useful first:
/// the directory cut to its last folder, the grid size, then the directory,
/// the local machine's label and the CPU mode. Where terminals run and
/// the branch always stay.
fn fit_parts(parts: &mut Vec<(Part, String)>, room: f32) {
    let used = |parts: &[(Part, String)]| -> f32 {
        parts
            .iter()
            .map(|(kind, text)| part_width(*kind, text))
            .sum()
    };
    for step in 0..5 {
        if used(parts) <= room {
            return;
        }
        match step {
            0 => {
                for (kind, text) in parts.iter_mut() {
                    if *kind == Part::Directory
                        && let Some(last) = text.trim_end_matches('/').rsplit('/').next()
                        && last.len() + 2 < text.len()
                    {
                        *text = format!("…/{last}");
                    }
                }
            }
            1 => parts.retain(|(kind, _)| *kind != Part::Grid),
            2 => parts.retain(|(kind, _)| *kind != Part::Directory),
            3 => parts.retain(|(kind, _)| *kind != Part::Local),
            _ => parts.retain(|(kind, _)| *kind != Part::Cpu),
        }
    }
}

pub fn status(ws: &Workspace, width: f32) -> impl View + use<> {
    let mut parts = status_items(ws);
    // A notice keeps room for its first words; the parts give way first.
    // A drag in progress says what it carries and where it may land, in
    // the place of a notice.
    let (message, dragging) = {
        let inner = ws.borrow();
        let hint = inner.drag_hint.clone();
        let dragging = hint.is_some();
        (
            hint.or_else(|| inner.notices.back().map(|notice| notice.text.clone())),
            dragging,
        )
    };
    // A drag's hint is short and only useful whole, so the parts make room
    // for all of it.
    let least = if dragging { NOTICE_CHARS } else { NOTICE_LEAST };
    let notice_room = message.as_ref().map_or(0.0, |text| {
        text.chars().count().min(least) as f32 * CHAR_WIDTH + GAP + ICON + ICON_GAP
    });
    fit_parts(&mut parts, width - PAD * 2.0 - notice_room);
    let scheme = ws.borrow().scheme;
    let tr = ws.tr();
    let used: f32 = parts
        .iter()
        .map(|(kind, part)| part_width(*kind, part))
        .sum();
    let room = ((width - PAD * 2.0 - used - GAP - ICON - ICON_GAP) / CHAR_WIDTH).max(0.0) as usize;
    let notice = message
        .as_deref()
        .map(|text| truncate(text, room.min(NOTICE_CHARS)))
        .filter(|text| text.chars().count() > 1)
        .unwrap_or_default();
    // Parts sit between thin dividers, each after an icon that says what it
    // is. Where terminals run (a sandbox or a devcontainer) is a safety fact,
    // so it takes the accent color.
    let mut left = Row::new().gap(GAP / 2.0).align(Align::Center);
    for (index, (kind, part)) in parts.into_iter().enumerate() {
        if index > 0 {
            left = left.child(Fill::new(1.0, DIVIDER_HEIGHT, scheme.line));
        }
        let color = match kind {
            Part::Runtime => scheme.lanes[0],
            Part::RuntimeOpen => super::theme::mix(
                scheme.lanes[1],
                scheme.ink,
                if scheme.dark {
                    PILL_INK_SHARE.0
                } else {
                    PILL_INK_SHARE.1
                },
            ),
            _ => scheme.muted,
        };
        let (name, svg) = kind.icon();
        // Room on both sides, so a focus ring never touches the icon or
        // the last letter.
        let width = part.chars().count() as f32 * CHAR_WIDTH + ICON + ICON_GAP + SIDE * 2.0;
        let content = Pad::insets(EdgeInsets {
            left: SIDE,
            ..EdgeInsets::default()
        })
        .child(
            Row::new()
                .gap(ICON_GAP)
                .align(Align::Center)
                .child(MdIcon::outlined(name, svg).size(ICON).color(color))
                .child(ThemeProvider::new(
                    schnellui::widgets::Theme {
                        text: color,
                        ..scheme.theme()
                    },
                    Text::new(part.clone()).size(12.5),
                )),
        );
        // Where terminals run opens the settings that decide it.
        if matches!(kind, Part::Runtime | Part::RuntimeOpen) {
            let open = ws.clone();
            // A weakened sandbox is a warning, so it sits on a pill of its
            // color and does not read as one more fact among the others.
            let warn = kind == Part::RuntimeOpen;
            let pad = if warn { PILL_PAD } else { 0.0 };
            let width = width + pad * 2.0;
            let mut stack = Stack::new()
                .size(width, STATUS_HEIGHT - 4.0)
                .align(Align::Center)
                .justify(Justify::Start);
            if warn {
                // A frame in the warning color as well as the tint, so the
                // pill reads as a warning on a light scheme too, where a tint
                // alone fades to grey, and apart from a lane-colored row.
                stack = stack.child(
                    schnellui::widgets::Panel::new()
                        .width(width)
                        .height(STATUS_HEIGHT - 6.0)
                        .surface(super::theme::mix(
                            scheme.rail,
                            scheme.lanes[1],
                            if scheme.dark {
                                PILL_SHARE.0
                            } else {
                                PILL_SHARE.1
                            },
                        ))
                        .outline(super::theme::mix(scheme.rail, scheme.lanes[1], 160))
                        .radius((STATUS_HEIGHT - 6.0) / 2.0),
                );
            }
            left = left.child(
                stack
                    .child(Pad::insets(EdgeInsets::symmetric(pad, 0.0)).child(content))
                    // On top, so it takes the pointer; the row only paints.
                    // Its hover wash and focus ring round off as the pill
                    // under it does.
                    .child(ThemeProvider::new(
                        {
                            let mut theme = scheme.theme();
                            theme.shape.roundness = 100.0;
                            // The plain wash hardly shows on the amber pill.
                            if warn {
                                let lane = scheme.lanes[1];
                                theme.interactions.hover =
                                    schnellui::widgets::InteractionStyle::background(
                                        schnellui::scene::Color::rgba(lane.r, lane.g, lane.b, 0x48),
                                    );
                            }
                            theme
                        },
                        Button::new(format!("{part}, {}", tr.open_sandboxing))
                            .icon_only()
                            .tooltip(if kind == Part::RuntimeOpen {
                                tr.runtime_sandbox_open_hint
                            } else {
                                tr.open_sandboxing
                            })
                            .appearance(ButtonAppearance::Ghost)
                            .width(width)
                            // As tall as the pill, so its wash covers the
                            // pill and nothing past it.
                            .height(if warn {
                                STATUS_HEIGHT - 6.0
                            } else {
                                STATUS_HEIGHT - 4.0
                            })
                            .on_click(move || {
                                open.show_settings_section(super::SettingsSection::Sandboxing);
                                open.open_settings();
                            }),
                    )),
            );
        } else if kind == Part::Cpu {
            left = left.child(cpu_switch(ws, width, content));
        } else if let Some((name, action)) = part_action(kind, tr) {
            let run = ws.clone();
            left = left.child(
                Stack::new()
                    .size(width, STATUS_HEIGHT - 4.0)
                    .align(Align::Center)
                    .justify(Justify::Start)
                    .child(content)
                    .child(
                        Button::new(format!("{part}, {name}"))
                            .icon_only()
                            .tooltip(name)
                            .appearance(ButtonAppearance::Ghost)
                            .width(width)
                            .height(STATUS_HEIGHT - 4.0)
                            .on_click(move || action(&run)),
                    ),
            );
        } else {
            left = left.child(content);
        }
    }
    let mut row = Row::new()
        .width(width - PAD * 2.0)
        .height(STATUS_HEIGHT - 1.0)
        .justify(Justify::SpaceBetween)
        .align(Align::Center)
        .child(left);
    // The notice opens the notification list, which keeps its full text
    // after the status bar lets it go.
    if !notice.is_empty() {
        let open = ws.clone();
        let width = notice.chars().count() as f32 * CHAR_WIDTH + ICON + ICON_GAP;
        let content = Row::new()
            .gap(ICON_GAP)
            .align(Align::Center)
            // A drag's hint is not a notification: it gets the drag icon.
            .child(
                if dragging {
                    MdIcon::outlined("drag_pan", outlined::ICON_DRAG_PAN)
                } else {
                    MdIcon::outlined("notifications", outlined::ICON_NOTIFICATIONS)
                }
                .size(ICON)
                .color(scheme.muted),
            )
            .child(Text::new(notice).size(12.5));
        row = row.child(
            Stack::new()
                .size(width, STATUS_HEIGHT - 4.0)
                .align(Align::Center)
                .justify(Justify::End)
                .child(content)
                .child(
                    Button::new(tr.notifications)
                        .icon_only()
                        .tooltip(tr.notifications)
                        .appearance(ButtonAppearance::Ghost)
                        .width(width)
                        .height(STATUS_HEIGHT - 4.0)
                        .on_click(move || open.toggle_notifications()),
                ),
        );
    }
    Named(
        tr.status_bar.into(),
        Column::new()
            .width(width)
            .child(Fill::new(width, 1.0, scheme.line))
            .child(
                Stack::new()
                    .size(width, STATUS_HEIGHT - 1.0)
                    .child(Fill::new(width, STATUS_HEIGHT - 1.0, scheme.rail))
                    .child(ThemeProvider::new(
                        scheme.muted_theme(),
                        Pad::insets(EdgeInsets::symmetric(PAD, 0.0)).child(row),
                    )),
            ),
    )
}
