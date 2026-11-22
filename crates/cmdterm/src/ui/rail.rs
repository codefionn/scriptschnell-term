//! The rail on the left: every project with its worktrees nested under it,
//! and the active project's change requests. Every row is a left-aligned `ListRow`.
//! A local or global session not kept yet says `temporary` beside its
//! machine; a global session has one row for its folder instead of worktrees.
//!
//! A worktree row shows the title of the last agent session in its
//! terminals, with the branch under it, unless the settings name worktrees
//! by branch. The search field above the list filters it in place: only
//! the projects and worktrees that match stay, best first (`search.rs`).
//! The field stays mounted while the list under it rebuilds, so typing
//! keeps its focus.
//!
//! A secondary click on a project or worktree row opens its context menu:
//! the project's settings and, where the repository has remotes, a fetch.
//! A worktree's menu also pulls and pushes its branch, pushes it to each
//! remote by name when there are several, and for a worktree other than
//! the main one, removes it.

use std::path::Path;

use crate::forge::{ForgeKind, Pipeline};
use crate::state::Project;
use crate::term::Attention;
use crate::vcs::{Status, Worktree};
use crate::vcs_worker::{RemoteOp, Snapshot};
use schnellui::layout::EdgeInsets;
use schnellui::widgets::{
    Align, Badge, Button, ButtonAppearance, Column, ContextMenu, ContextMenuItem, Justify, ListRow,
    Pad, Row, Scroll, Stack, Text, TextInput, ThemeProvider, View,
};
use schnellui_icons_md::{MdIcon, outlined};

use super::theme::Scheme;
use super::{Fill, InputMode, Named, tail_path, truncate};

/// Characters of a worktree path shown under its branch name.
const RAIL_PATH_CHARS: usize = 31;
use crate::i18n::Strings;
use crate::workspace::{Inner, Workspace};

/// Width of the rail's content; the last pixel is the rule on its right edge.
/// The rail's width inside its right border.
fn rail_inner() -> f32 {
    super::rail_width() - 1.0
}
const ROW_HEIGHT: f32 = 28.0;
const SECTION_GAP: f32 = 16.0;
const PAD: f32 = 12.0;
/// Left inset of worktree rows under their project.
const INDENT: f32 = 12.0;
/// Width of the `+` button beside each worktree row.
const ADD_WIDTH: f32 = 24.0;
/// Height of a worktree row's title line, the list row's default.
const ROW_TITLE_HEIGHT: f32 = 28.0;
/// Height of the footer with the settings button, its top rule included.
const FOOTER_HEIGHT: f32 = 41.0;
/// Characters of an agent's message shown under a waiting worktree.
const RAIL_MESSAGE_CHARS: usize = 40;
/// Characters of an agent session's title in a worktree row.
const RAIL_TITLE_CHARS: usize = 30;
/// Accessible name of the rail's search field, which its key handler
/// finds it by. Stays English.
pub const RAIL_SEARCH_INPUT: &str = "Search worktrees";
/// Height of the header with the app's name and the `+`.
const HEADER_HEIGHT: f32 = 36.0;
/// Height of the slot the search field sits in.
const SEARCH_HEIGHT: f32 = 46.0;

/// The whole rail: header, the Projects and Agents tabs, search field, the
/// list and the footer. Only a full remount builds it; the tabs, [`list`]
/// and [`footer`] rebuild on their own.
pub fn sidebar(ws: &Workspace, height: f32) -> impl View + use<> {
    let (scheme, regions) = {
        let inner = ws.borrow();
        (inner.scheme, inner.regions)
    };
    Stack::new()
        .size(super::rail_width(), height)
        .child(Fill::new(super::rail_width(), height, scheme.rail))
        .child(
            Row::new()
                .child(
                    Column::new()
                        .width(rail_inner())
                        .child(header(ws))
                        .child(super::agents::tabs(ws).with_ref(regions.rail_tabs))
                        .child(
                            Stack::new()
                                .size(rail_inner(), SEARCH_HEIGHT)
                                .justify(Justify::Center)
                                .child(
                                    Pad::insets(EdgeInsets::symmetric(PAD, 4.0))
                                        .child(search(ws).with_ref(regions.rail_search)),
                                ),
                        )
                        .child(list(ws, height).with_ref(regions.sidebar_list))
                        .child(Fill::new(rail_inner(), 1.0, scheme.line))
                        .child(footer(ws).with_ref(regions.sidebar_footer)),
                )
                .child(Fill::new(1.0, height, scheme.line)),
        )
}

/// The search field. Typing filters the list under it.
pub fn search(ws: &Workspace) -> impl View + use<> {
    let typed = ws.clone();
    let inner = ws.borrow();
    // The hint is dimmer than typed text, as in the palette.
    ThemeProvider::new(
        inner.scheme.field_theme(),
        Named(
            RAIL_SEARCH_INPUT.into(),
            TextInput::new(inner.rail_query.clone())
                .text_size(13.5)
                // A plain hint keeps the field one line, as tall as the
                // palette's.
                .hint(inner.tr.search_worktrees)
                .height(32.0)
                .width(rail_inner() - PAD * 2.0)
                .on_input(move |value| typed.set_rail_query(value.to_owned())),
        ),
    )
}

/// The scrolling list of a rail `height` tall: every project with its
/// worktrees. While the search field has words, only the projects and
/// worktrees that match, best first.
pub fn list(ws: &Workspace, height: f32) -> impl View + use<> {
    let inner = ws.borrow();
    let tr = inner.tr;
    let hits = crate::search::Query::parse(&inner.rail_query)
        .map(|_| crate::search::rail_hits(&inner, &inner.rail_query));
    let matches = hits.as_deref().map(|hits| Matches {
        hits,
        highlight: hits
            .get(inner.rail_highlight.min(hits.len().saturating_sub(1)))
            .map(|hit| (hit.project, hit.path.as_path())),
    });
    let list = if matches.is_none() && inner.rail_tab == crate::workspace::RailTab::Agents {
        super::agents::agents(Column::new().width(rail_inner()), ws, &inner)
    } else {
        projects(
            Column::new().width(rail_inner()),
            ws,
            &inner,
            matches.as_ref(),
        )
    };
    let list_height =
        (height - HEADER_HEIGHT - super::agents::TABS_HEIGHT - SEARCH_HEIGHT - FOOTER_HEIGHT)
            .max(0.0);
    Scroll::new()
        .label(tr.sidebar)
        .restoration_key(tr.sidebar)
        .size(rail_inner(), list_height)
        .child(list)
}

/// What the search found: the worktrees, best first, and the one Enter
/// opens.
struct Matches<'a> {
    hits: &'a [crate::search::Hit],
    highlight: Option<(u64, &'a Path)>,
}

impl Matches<'_> {
    fn highlighted(&self, project: u64, path: &Path) -> bool {
        self.highlight == Some((project, path))
    }
}

/// Every project with its worktrees and the active project's change
/// requests. With `matches`, only the projects with a
/// match, in the order of their best one, and nothing else.
fn projects(mut list: Column, ws: &Workspace, inner: &Inner, matches: Option<&Matches>) -> Column {
    let scheme = inner.scheme;
    let tr = inner.tr;
    // The Projects tab above names the list; a heading would repeat it.
    let mut shown: Vec<&Project> = Vec::new();
    let projects: &[Project] = match matches {
        Some(matches) => {
            for hit in matches.hits {
                if !shown.iter().any(|p| p.id == hit.project)
                    && let Some(project) = inner.state.project(hit.project)
                {
                    shown.push(project);
                }
            }
            if shown.is_empty() {
                return list.child(note(scheme, tr.search_no_match.to_owned()));
            }
            &[]
        }
        None => &inner.state.projects,
    };
    for project in shown.iter().copied().chain(projects) {
        let active = inner.state.active_project == Some(project.id);
        let id = project.id;
        let select = ws.clone();
        let (attention, waiting) = inner.project_attention(id);
        let trailing = if project.temporary {
            format!("{} · {}", project.env.label(), tr.temporary)
        } else {
            project.env.label().to_owned()
        };
        let mut row = ListRow::new(project.name.clone())
            .trailing(trailing)
            .trailing_after_title()
            .title_size(15.0)
            .meta_size(12.0)
            .width(rail_inner())
            .selected(active)
            // A quiet fill: the project is a heading, and the worktree row
            // under it carries the strong mark of what is shown.
            .selected_background(scheme.raised)
            .on_click(move || select.select_project(id))
            .context_menu(project_menu(ws, inner, project));
        // Its worktrees show what they wait for; the project only shows
        // that one of them does, also while it scrolled out of view.
        if attention != Attention::None {
            row = row
                .dot(scheme.attention_color(attention))
                .value(project_attention_name(tr, attention, waiting));
        }
        list = list.child(row);
        if let Some(error) = inner.errors.get(&id) {
            list = list.child(note(scheme, truncate(error, 60)));
        }
        list = worktree_rows(list, ws, inner, project, active, matches);
    }
    if matches.is_some() {
        return list;
    }
    for input in &inner.pending_projects {
        list = list.child(note(scheme, (tr.adding)(input)));
    }
    // Under the last project, the way to the next one.
    let open = ws.clone();
    list = list.child(ThemeProvider::new(
        scheme.link_theme(),
        row(tr.open_project_ask.to_owned())
            .on_click(move || open.open_palette(Some(InputMode::Project))),
    ));
    if let Some(project) = inner.state.active()
        && let Some(snapshot) = inner.snapshots.get(&project.id)
        && let Some(forge) = &snapshot.forge
    {
        list = list.child(section(scheme, tr.change_requests, true));
        if has_token(forge.kind, &forge.host) {
            for request in &snapshot.change_requests {
                let open = ws.clone();
                let url = request.url.clone();
                let title = format!("!{} {}", request.number, truncate(&request.title, 40));
                list = list.child(row(title).on_click(move || open.open_browser(&url)));
            }
        } else {
            list = list.child(note(scheme, (tr.set_token)(token_hint(forge.kind))));
        }
        if let Some(url) = &snapshot.forge_url {
            let open = ws.clone();
            let url = url.clone();
            list = list.child(muted(
                scheme,
                row((tr.open_host)(&forge.host)).on_click(move || open.open_browser(&url)),
            ));
        }
    }
    list
}

/// The bar pinned to the bottom of the rail, under the scrolling list: a
/// gear button that opens the settings on the left, and on the right a
/// bell that opens the notifications, with a badge counting unread ones.
pub fn footer(ws: &Workspace) -> impl View + use<> {
    let open = ws.clone();
    let inner = ws.borrow();
    let (scheme, tr) = (inner.scheme, inner.tr);
    let settings_ref = inner.regions.settings_button;
    let settings_hint = inner
        .keymap
        .hint(tr.settings, &crate::keymap::Action::Settings);
    drop(inner);
    // Settings is named beside its icon; the footer has room for it.
    let settings = labeled_icon_button(
        MdIcon::outlined("settings", outlined::ICON_SETTINGS).color(scheme.muted),
        tr.settings,
        scheme,
        Button::new(tr.settings)
            .tooltip(settings_hint)
            .on_click(move || open.open_settings_from_rail()),
        Some(settings_ref),
    );
    Pad::insets(EdgeInsets::symmetric(8.0, 5.0)).child(
        Row::new()
            .width(rail_inner() - 16.0)
            .justify(Justify::SpaceBetween)
            .align(Align::Center)
            .child(settings)
            .child(bell(ws)),
    )
}

/// The bell that opens the notifications, in the rail's footer or, while
/// the rail is hidden, in the tab strip. Only one of them is mounted, so
/// both carry the ref focus comes back to.
pub(super) fn bell(ws: &Workspace) -> impl View + use<> {
    let bell = ws.clone();
    let inner = ws.borrow();
    let (scheme, tr) = (inner.scheme, inner.tr);
    let unread = inner.notifications.unread();
    let bell_open = inner.notifications_open;
    let bell_ref = inner.regions.bell;
    drop(inner);
    let (icon, name) = if unread == 0 {
        (
            MdIcon::outlined("notifications", outlined::ICON_NOTIFICATIONS).color(scheme.muted),
            std::borrow::Cow::Borrowed(tr.notifications),
        )
    } else {
        (
            // Ink, so the amber count on its corner stands apart from it.
            MdIcon::outlined("notifications_active", outlined::ICON_NOTIFICATIONS_ACTIVE)
                .color(scheme.ink),
            std::borrow::Cow::Owned((tr.notifications_unread)(unread)),
        )
    };
    let tip = name.clone();
    // The unread count sits on the bell's top right corner, inside the
    // button, so it takes the same clicks and the bell never moves when it
    // comes or goes. While the popover is open the bell sits on the
    // selection fill, so it reads as the button the popover belongs to.
    let mut bell_button = Stack::new()
        .size(BELL_WIDTH, 30.0)
        .child(
            schnellui::widgets::Panel::new()
                .width(BELL_WIDTH)
                .height(30.0)
                .surface(if bell_open {
                    scheme.selected()
                } else {
                    schnellui::scene::Color::TRANSPARENT
                })
                .no_outline()
                .radius(6.0),
        )
        .child(
            Stack::new()
                .size(BELL_WIDTH, 30.0)
                .align(Align::Center)
                .justify(Justify::Center)
                .child(icon.size(20.0)),
        );
    if unread > 0 {
        bell_button = bell_button.child(
            Pad::insets(EdgeInsets {
                left: BELL_WIDTH / 2.0 + 5.0,
                top: 0.0,
                ..EdgeInsets::default()
            })
            .child(ThemeProvider::new(
                scheme.attention_theme(Attention::Input),
                Badge::new(unread.min(99).to_string()),
            )),
        );
    }
    // On top, so it takes the pointer. Escape in the popover brings focus
    // back here.
    bell_button.child(
        Button::new(name)
            .tooltip(tip)
            .icon_only()
            .appearance(ButtonAppearance::Ghost)
            .width(BELL_WIDTH)
            .height(30.0)
            .on_click(move || bell.toggle_notifications())
            .with_ref(bell_ref),
    )
}

/// Width of the bell's button, with room for the unread count on its corner.
pub(super) const BELL_WIDTH: f32 = 38.0;

/// A ghost `button` over `icon` and its visible `label`, 30 px tall.
fn labeled_icon_button(
    icon: MdIcon,
    label: &'static str,
    scheme: &Scheme,
    button: Button,
    focus_ref: Option<schnellui::scene::ComponentRef>,
) -> impl View + use<> {
    let width = LABELED_ICON_WIDTH;
    let button = button
        .icon_only()
        .appearance(ButtonAppearance::Ghost)
        .width(width)
        .height(30.0);
    let button: Box<dyn View> = match focus_ref {
        Some(focus_ref) => Box::new(button.with_ref(focus_ref)),
        None => Box::new(button),
    };
    Stack::new()
        .size(width, 30.0)
        .child(
            Pad::insets(EdgeInsets {
                left: 5.0,
                top: 5.0,
                ..EdgeInsets::default()
            })
            .child(
                Row::new()
                    .gap(8.0)
                    .align(Align::Center)
                    .child(icon.size(20.0))
                    .child(ThemeProvider::new(
                        scheme.muted_theme(),
                        Text::new(label).size(13.0),
                    )),
            ),
        )
        .child(button)
}

/// Width of a footer button with a label beside its icon.
const LABELED_ICON_WIDTH: f32 = 112.0;

/// A 30 px ghost `button` over `icon`.
pub(super) fn icon_button(icon: MdIcon, button: Button) -> impl View + use<> {
    Stack::new()
        .size(30.0, 30.0)
        .align(Align::Center)
        .justify(Justify::Center)
        .child(icon.size(20.0))
        // On top, so it takes the pointer; the icon only paints.
        .child(
            button
                .icon_only()
                .appearance(ButtonAppearance::Ghost)
                .width(30.0)
                .height(30.0),
        )
}

/// The worktrees of `project`, indented under its row: the main worktree
/// first, then the others in the order the VCS reports them. Only the shown
/// worktree of the active project is selected. A `+` beside each row adds a
/// worktree at that row's revision.
fn worktree_rows(
    mut list: Column,
    ws: &Workspace,
    inner: &Inner,
    project: &Project,
    active: bool,
    matches: Option<&Matches>,
) -> Column {
    let tr = inner.tr;
    if project.is_global() {
        let highlighted = matches.is_some_and(|m| m.highlighted(project.id, &project.root));
        // While searching, only the row Enter opens is marked.
        let current = active && matches.is_none();
        return global_row(list, ws, inner, project, current, highlighted);
    }
    let current = project.current_worktree();
    let snapshot = inner.snapshots.get(&project.id);
    let fallback = [Worktree {
        path: project.root.clone(),
        branch: None,
        head: None,
        is_main: true,
        locked: false,
        prunable: false,
        bare: false,
    }];
    let (worktrees, statuses): (&[Worktree], &[Option<Status>]) = match snapshot {
        Some(snapshot) => (&snapshot.worktrees, &snapshot.status),
        None => (&fallback, &[]),
    };
    // The lane follows the position in the full list, so skipped bare
    // entries still take an index.
    let main = worktrees.iter().enumerate().filter(|(_, w)| w.is_main);
    let rest = worktrees.iter().enumerate().filter(|(_, w)| !w.is_main);
    // While searching, the matching worktrees, best first.
    let found = matches.map(|matches| {
        matches
            .hits
            .iter()
            .filter(|hit| hit.project == project.id)
            .filter_map(|hit| {
                let index = worktrees.iter().position(|w| w.path == hit.path)?;
                Some((index, &worktrees[index]))
            })
    });
    let mut all = main.chain(rest);
    let mut found = found.into_iter().flatten();
    let order: &mut dyn Iterator<Item = (usize, &Worktree)> = if matches.is_some() {
        &mut found
    } else {
        &mut all
    };
    for (index, worktree) in order {
        if worktree.bare {
            continue;
        }
        let status = statuses.get(index).and_then(Option::as_ref);
        let change = snapshot.and_then(|snapshot| {
            let branch = worktree.branch.as_deref()?;
            snapshot
                .change_requests
                .iter()
                .find(|request| request.source_branch == branch)
        });
        let pipeline = snapshot
            .and_then(|snapshot| snapshot.pipelines.get(index))
            .and_then(Option::as_ref);
        let meta = worktree_meta(
            tr,
            status,
            change.map(|c| c.number),
            pipeline,
            worktree.locked,
            worktree.prunable,
        );
        let lane = inner.scheme.lane(index);
        let selected = active && worktree.path == current;
        let highlighted = matches.is_some_and(|m| m.highlighted(project.id, &worktree.path));
        let path = worktree.path.clone();
        let id = project.id;
        let activate = ws.clone();
        let branch = branch_name(worktree);
        let agent =
            crate::search::agent_title(inner.settings.worktree_titles, project, &worktree.path);
        let title = agent.map_or_else(|| branch.clone(), |t| truncate(t, RAIL_TITLE_CHARS));
        let add = ws.clone();
        let source = worktree.path.clone();
        // The tooltip says what the name says, so the pointer and a screen
        // reader hear the same branch.
        let add_name = (tr.new_worktree_from)(&branch);
        let add_button = Button::new(add_name.clone())
            .display("+")
            .appearance(ButtonAppearance::Ghost)
            .width(ADD_WIDTH)
            .height(ADD_WIDTH)
            .text_size(16.0)
            .tooltip(add_name)
            .on_click(move || add.open_new_worktree_from(id, &source));
        let (attention, message) = inner.worktree_attention(project.id, &worktree.path);
        let mut row = ListRow::new(title)
            .width(rail_inner() - INDENT)
            .meta_size(12.0)
            .end_room(ADD_WIDTH)
            .dot(lane)
            // While searching, only the row Enter opens takes the chosen
            // fill, so it never reads as the worktree already shown.
            .selected(if matches.is_some() {
                highlighted
            } else {
                selected
            })
            .selected_background(inner.scheme.tinted(lane))
            .on_click(move || activate.activate_worktree(id, &path))
            .context_menu(worktree_menu(ws, tr, snapshot, id, worktree, &branch));
        row = if attention.wants_user() {
            // A pill in the attention color takes the place of the VCS
            // summary, and the agent's message, which says what it waits
            // for, the place of the path, until the user answers.
            let color = inner.scheme.attention_color(attention);
            let description = tr.attention_description(attention);
            let subtitle = if message.is_empty() {
                description.to_owned()
            } else {
                truncate(&message, RAIL_MESSAGE_CHARS)
            };
            row.trailing(tr.attention_label(attention))
                .trailing_pill(color)
                .subtitle(subtitle)
                .subtitle_color(color)
                .value(if message.is_empty() {
                    description.to_owned()
                } else {
                    format!("{description} ({message})")
                })
        } else {
            // Under an agent session's title, the branch says where it ran.
            let subtitle = if agent.is_some() {
                branch
            } else {
                tail_path(&worktree.path, RAIL_PATH_CHARS)
            };
            if attention == Attention::Busy {
                let busy = tr.attention_label(attention);
                let trailing = if meta.is_empty() {
                    busy.to_owned()
                } else {
                    format!("{busy} · {meta}")
                };
                row.trailing(trailing).subtitle(subtitle)
            } else {
                row.trailing(meta).subtitle(subtitle)
            }
        };
        // While searching, the bar marks the worktree Enter opens.
        if (matches.is_some() && highlighted) || (matches.is_none() && selected) {
            row = row.accent_bar(lane);
        }
        list = list.child(
            Pad::insets(EdgeInsets {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: INDENT,
            })
            // The "+" sits inside the row, at the end of its title line, so
            // it reads as part of the worktree it starts from.
            .child(
                Row::new()
                    .gap(-(ADD_WIDTH + 4.0))
                    .align(Align::Start)
                    .child(row)
                    .child(
                        Pad::insets(EdgeInsets {
                            top: (ROW_TITLE_HEIGHT - ADD_WIDTH) / 2.0,
                            ..EdgeInsets::default()
                        })
                        .child(add_button),
                    ),
            ),
        );
    }
    if snapshot.is_none() && !inner.errors.contains_key(&project.id) {
        list = list.child(note(inner.scheme, tr.loading.to_owned()));
    }
    list
}

/// The context menu of a project row: the project's settings, and a
/// fetch from every remote when it has any.
fn project_menu(ws: &Workspace, inner: &Inner, project: &Project) -> ContextMenu {
    let tr = inner.tr;
    let id = project.id;
    let menu = ContextMenu::new()
        .text_size(13.0)
        .row_height(28.0)
        .item(project_settings_item(ws, tr, id));
    match inner.snapshots.get(&id) {
        Some(snapshot) if !snapshot.remotes.is_empty() => {
            menu.item(ContextMenuItem::separator()).item(remote_item(
                ws,
                tr.vcs_fetch.to_owned(),
                id,
                &project.root,
                RemoteOp::Fetch,
            ))
        }
        _ => menu,
    }
}

/// The context menu of a worktree row: the project's settings, fetch,
/// pull and push where the repository has remotes, a push to each remote
/// when there are several, and removing the worktree unless it is the
/// main one.
fn worktree_menu(
    ws: &Workspace,
    tr: &'static Strings,
    snapshot: Option<&Snapshot>,
    id: u64,
    worktree: &Worktree,
    branch: &str,
) -> ContextMenu {
    let mut menu = ContextMenu::new()
        .text_size(13.0)
        .row_height(28.0)
        .item(project_settings_item(ws, tr, id));
    let remotes = snapshot.map_or(&[][..], |snapshot| &snapshot.remotes[..]);
    if !remotes.is_empty() && !worktree.prunable {
        let path = &worktree.path;
        menu = menu.item(ContextMenuItem::separator()).item(remote_item(
            ws,
            tr.vcs_fetch.to_owned(),
            id,
            path,
            RemoteOp::Fetch,
        ));
        if worktree.branch.is_some() {
            if snapshot.is_some_and(|snapshot| snapshot.repo.can_pull()) {
                let label = (tr.vcs_pull)(branch);
                menu = menu.item(remote_item(ws, label, id, path, RemoteOp::Pull));
            }
            let label = (tr.vcs_push)(branch);
            menu = menu.item(remote_item(ws, label, id, path, RemoteOp::Push(None)));
            if remotes.len() > 1 {
                for remote in remotes {
                    let label = (tr.vcs_push_to)(branch, remote);
                    let op = RemoteOp::Push(Some(remote.clone()));
                    menu = menu.item(remote_item(ws, label, id, path, op));
                }
            }
        }
    }
    if worktree.is_main {
        return menu;
    }
    let menu = menu.item(ContextMenuItem::separator());
    let remove = ws.clone();
    let path = worktree.path.clone();
    menu.item(
        ContextMenuItem::new((tr.remove_worktree)(branch))
            .on_select(move || remove.remove_worktree(id, &path)),
    )
}

/// A menu item that runs `op` in worktree `path` of project `id`.
fn remote_item(
    ws: &Workspace,
    label: String,
    id: u64,
    path: &Path,
    op: RemoteOp,
) -> ContextMenuItem {
    let run = ws.clone();
    let path = path.to_path_buf();
    ContextMenuItem::new(label).on_select(move || run.run_remote(id, &path, op.clone()))
}

/// Opens the settings on the page of project `id`, which it selects first
/// since that page edits the active project.
fn project_settings_item(ws: &Workspace, tr: &'static Strings, id: u64) -> ContextMenuItem {
    let open = ws.clone();
    ContextMenuItem::new(tr.project_settings).on_select(move || {
        if open.borrow().state.active_project != Some(id) {
            open.select_project(id);
        }
        open.open_project_settings();
    })
}

/// The one row of a global session, for its folder: no branch, no VCS
/// summary and no `+`, since there is no repository.
fn global_row(
    list: Column,
    ws: &Workspace,
    inner: &Inner,
    project: &Project,
    active: bool,
    highlighted: bool,
) -> Column {
    let tr = inner.tr;
    let lane = inner.scheme.lane(0);
    let (id, path) = (project.id, project.root.clone());
    let activate = ws.clone();
    let (attention, message) = inner.worktree_attention(id, &project.root);
    let title =
        match crate::search::agent_title(inner.settings.worktree_titles, project, &project.root) {
            Some(agent) => truncate(agent, RAIL_TITLE_CHARS),
            None => crate::search::folder_name(&project.root),
        };
    let mut row = ListRow::new(title)
        .width(rail_inner() - INDENT)
        .meta_size(12.0)
        .dot(lane)
        .selected(active || highlighted)
        .selected_background(inner.scheme.tinted(lane))
        .subtitle(tail_path(&project.root, RAIL_PATH_CHARS))
        .on_click(move || activate.activate_worktree(id, &path));
    if attention.wants_user() {
        let color = inner.scheme.attention_color(attention);
        row = row
            .trailing(tr.attention_label(attention))
            .trailing_pill(color)
            .value(if message.is_empty() {
                tr.attention_description(attention).to_owned()
            } else {
                truncate(&message, RAIL_MESSAGE_CHARS)
            });
    } else if attention == Attention::Busy {
        row = row.trailing(tr.attention_label(attention));
    }
    if active || highlighted {
        row = row.accent_bar(lane);
    }
    list.child(
        Pad::insets(EdgeInsets {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: INDENT,
        })
        .child(row),
    )
}

/// Accessible value of a project row while one of its worktrees waits.
fn project_attention_name(tr: &Strings, attention: Attention, count: usize) -> String {
    let description = tr.attention_description(attention);
    match count {
        0 | 1 => description.to_owned(),
        _ => (tr.worktree_count)(description, count),
    }
}

fn header(ws: &Workspace) -> impl View + use<> {
    let (open, hide) = (ws.clone(), ws.clone());
    let tr = ws.tr();
    let (scheme, hide_hint) = {
        let inner = ws.borrow();
        let hint = inner
            .keymap
            .hint(tr.hide_sidebar, &crate::keymap::Action::ToggleSidebar);
        (inner.scheme, hint)
    };
    let small = super::rail_width() <= super::SMALL_RAIL_WIDTH;
    // Exactly as tall as the list's height leaves for it, with the row
    // centered in it, so the footer under the list stays in the window.
    Stack::new()
        .size(rail_inner(), HEADER_HEIGHT)
        .align(Align::Center)
        .justify(Justify::Start)
        .child(
            Pad::insets(EdgeInsets {
                top: 0.0,
                right: 8.0,
                bottom: 0.0,
                left: PAD,
            })
            .child(
                Row::new()
                    .width(rail_inner() - PAD - 8.0)
                    .justify(Justify::SpaceBetween)
                    .align(Align::Center)
                    .child(Text::new("scriptschnell-term").size(if small { 14.0 } else { 15.0 }))
                    .child({
                        let buttons = Row::new().gap(2.0).align(Align::Center);
                        // A small rail has no room for the name and both buttons;
                        // "Open project…" under the list does the same.
                        if small {
                            buttons
                        } else {
                            buttons.child(
                                Button::new(tr.open_project)
                                    .display("+")
                                    .appearance(ButtonAppearance::Ghost)
                                    .width(28.0)
                                    .height(28.0)
                                    .text_size(18.0)
                                    .tooltip(tr.open_project)
                                    .on_click(move || open.open_palette(Some(InputMode::Project))),
                            )
                        }
                        // Hides the rail, so a narrow window gives the panel
                        // its whole width; the tab strip has the way back.
                        .child(icon_button(
                            MdIcon::outlined("left_panel_close", outlined::ICON_LEFT_PANEL_CLOSE)
                                .color(scheme.muted),
                            Button::new(tr.hide_sidebar)
                                .tooltip(hide_hint)
                                .on_click(move || hide.toggle_sidebar()),
                        ))
                    }),
            ),
        )
}

fn section(scheme: &Scheme, title: &'static str, spaced: bool) -> impl View + use<> {
    Pad::insets(EdgeInsets {
        top: if spaced { SECTION_GAP } else { 12.0 },
        right: PAD,
        bottom: 4.0,
        left: PAD,
    })
    .child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(title).size(12.0),
    ))
}

fn row(title: String) -> ListRow {
    // The path and counts under a worktree at 12px, the least that reads
    // at a glance beside the 13px branch.
    ListRow::new(title)
        .width(rail_inner())
        .height(ROW_HEIGHT)
        .meta_size(12.0)
}

fn muted(scheme: &Scheme, view: impl View + 'static) -> impl View {
    ThemeProvider::new(scheme.muted_theme(), view)
}

fn note(scheme: &Scheme, text: String) -> impl View + use<> {
    Pad::insets(EdgeInsets::symmetric(PAD, 2.0)).child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(12.0).ellipsis(),
    ))
}

#[cfg(test)]
thread_local! {
    /// Lets a test decide whether a token counts as set without touching the
    /// process environment.
    pub static TOKEN_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn has_token(kind: ForgeKind, host: &str) -> bool {
    #[cfg(test)]
    if let Some(forced) = TOKEN_OVERRIDE.with(|cell| cell.get()) {
        return forced;
    }
    crate::forge::token_from_env(kind, host).is_some()
}

/// The environment variable that holds the access token for `kind`.
pub fn token_hint(kind: ForgeKind) -> &'static str {
    match kind {
        ForgeKind::GitLab => "GITLAB_TOKEN",
        ForgeKind::AzureDevOps => "AZURE_DEVOPS_EXT_PAT",
        ForgeKind::Forgejo => "FORGEJO_TOKEN",
    }
}

/// The row title of a worktree: its branch, else the short head.
pub fn branch_name(worktree: &Worktree) -> String {
    match &worktree.branch {
        Some(branch) => branch.clone(),
        None => match &worktree.head {
            Some(head) => format!("({})", &head[..head.len().min(8)]),
            None => "(detached)".to_owned(),
        },
    }
}

/// The right-aligned meta text of a worktree row: dirty count `3 changed`,
/// untracked `2 new`, ahead `↑1`, behind `↓2`, the CI label, the change request
/// `!12`, then `locked` and `missing`. Parts that do not apply are left out.
pub fn worktree_meta(
    tr: &Strings,
    status: Option<&Status>,
    change: Option<u64>,
    pipeline: Option<&Pipeline>,
    locked: bool,
    prunable: bool,
) -> String {
    use std::fmt::Write;
    let mut meta = String::new();
    let part = |meta: &mut String, args: std::fmt::Arguments| {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        let _ = meta.write_fmt(args);
    };
    if let Some(status) = status {
        let dirty = status.staged + status.modified + status.conflicted;
        if dirty > 0 {
            part(&mut meta, format_args!("{dirty} {}", tr.meta_changed));
        }
        if status.untracked > 0 {
            part(
                &mut meta,
                format_args!("{} {}", status.untracked, tr.meta_new),
            );
        }
        if status.ahead > 0 {
            part(&mut meta, format_args!("↑{}", status.ahead));
        }
        if status.behind > 0 {
            part(&mut meta, format_args!("↓{}", status.behind));
        }
    }
    let ci = tr.pipeline_label(pipeline.map(|pipeline| pipeline.status));
    if !ci.is_empty() {
        part(&mut meta, format_args!("{ci}"));
    }
    if let Some(number) = change {
        part(&mut meta, format_args!("!{number}"));
    }
    if locked {
        part(&mut meta, format_args!("{}", tr.locked));
    }
    if prunable {
        part(&mut meta, format_args!("{}", tr.missing));
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::PipelineStatus;
    use crate::i18n::{DE, EN};

    fn pipeline(status: PipelineStatus) -> Pipeline {
        Pipeline { status, url: None }
    }

    fn status(staged: u32, modified: u32, untracked: u32, ahead: u32, behind: u32) -> Status {
        Status {
            staged,
            modified,
            untracked,
            ahead,
            behind,
            ..Status::default()
        }
    }

    #[test]
    fn pipeline_labels() {
        let label = |status| EN.pipeline_label(Some(status));
        assert_eq!(label(PipelineStatus::Success), "ci ok");
        assert_eq!(label(PipelineStatus::Failed), "ci failed");
        assert_eq!(label(PipelineStatus::Running), "ci running");
        assert_eq!(label(PipelineStatus::Pending), "ci pending");
        assert_eq!(label(PipelineStatus::Canceled), "ci canceled");
        assert_eq!(label(PipelineStatus::Unknown), "");
        assert_eq!(EN.pipeline_label(None), "");
        assert_eq!(DE.pipeline_label(Some(PipelineStatus::Running)), "CI läuft");
    }

    #[test]
    fn token_hints_name_the_variable() {
        assert_eq!(token_hint(ForgeKind::GitLab), "GITLAB_TOKEN");
        assert_eq!(token_hint(ForgeKind::AzureDevOps), "AZURE_DEVOPS_EXT_PAT");
        assert_eq!(token_hint(ForgeKind::Forgejo), "FORGEJO_TOKEN");
    }

    #[test]
    fn long_titles_are_cut_with_an_ellipsis() {
        assert_eq!(truncate("short", 40), "short");
        let cut = truncate(&"x".repeat(50), 40);
        assert_eq!(cut.chars().count(), 40);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn meta_lists_counts_in_a_fixed_order() {
        let s = status(1, 1, 2, 1, 2);
        assert_eq!(
            worktree_meta(
                &EN,
                Some(&s),
                Some(12),
                Some(&pipeline(PipelineStatus::Failed)),
                false,
                false
            ),
            "2 changed · 2 new · ↑1 · ↓2 · ci failed · !12"
        );
    }

    #[test]
    fn meta_leaves_out_what_does_not_apply() {
        assert_eq!(worktree_meta(&EN, None, None, None, false, false), "");
        let clean = status(0, 0, 0, 0, 0);
        assert_eq!(
            worktree_meta(&EN, Some(&clean), None, None, false, false),
            ""
        );
        let s = status(0, 3, 0, 0, 0);
        assert_eq!(
            worktree_meta(&EN, Some(&s), None, None, false, false),
            "3 changed"
        );
        assert_eq!(
            worktree_meta(&EN, None, None, None, true, true),
            "locked · missing"
        );
        assert_eq!(
            worktree_meta(&DE, None, None, None, true, true),
            "gesperrt · fehlt"
        );
        assert_eq!(worktree_meta(&EN, None, Some(7), None, false, false), "!7");
    }

    #[test]
    fn dirty_counts_staged_modified_and_conflicted() {
        let mut s = status(1, 1, 0, 0, 0);
        s.conflicted = 1;
        assert_eq!(
            worktree_meta(&EN, Some(&s), None, None, false, false),
            "3 changed"
        );
    }

    #[test]
    fn branch_names_fall_back_to_the_head() {
        let mut worktree = Worktree {
            path: "/r".into(),
            branch: Some("main".into()),
            head: Some("0123456789abcdef".into()),
            is_main: true,
            locked: false,
            prunable: false,
            bare: false,
        };
        assert_eq!(branch_name(&worktree), "main");
        worktree.branch = None;
        assert_eq!(branch_name(&worktree), "(01234567)");
        worktree.head = None;
        assert_eq!(branch_name(&worktree), "(detached)");
    }
}
