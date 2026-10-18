//! The rail's Projects and Agents tabs, and the Agents tab's list: one row
//! per agent, running or ended, from `Inner::agent_rows` (`forges.rs`).
//! Finished agents whose branch is not merged stay expanded on top, most
//! urgent first; working and merged ones, and the earlier sessions, last
//! first, sit in groups that start collapsed. A click on a running agent
//! shows its worktree with the agent's tab and pane focused; on an ended
//! session it resumes the session.

use schnellui::layout::EdgeInsets;
use schnellui::widgets::{
    Align, Button, Column, ListRow, Pad, Row, Tab, TabAppearance, TabBar, Text, ThemeProvider, View,
};
use schnellui_icons_md::{MdIcon, outlined};

use super::theme::Scheme;
use super::{Fill, truncate};
use crate::ext::BranchState;
use crate::forges::{AgentGroup, AgentRow};
use crate::i18n::Strings;
use crate::term::Attention;
use crate::workspace::{Inner, RailTab, Workspace};

/// The rail's width inside its right border.
fn rail_inner() -> f32 {
    super::rail_width() - 1.0
}
const PAD: f32 = 12.0;
const INDENT: f32 = 12.0;
/// Height of the tab strip, its bottom rule included.
pub const TABS_HEIGHT: f32 = 33.0;
/// Characters of an agent's title in a row.
const TITLE_CHARS: usize = 30;
/// Characters of an agent's message under a waiting row.
const MESSAGE_CHARS: usize = 40;
/// Width of the discussion button beside a row.
const DISCUSSION_WIDTH: f32 = 30.0;

/// The Projects and Agents tabs. The Agents tab counts finished agents.
pub fn tabs(ws: &Workspace) -> impl View + use<> {
    let inner = ws.borrow();
    let (scheme, tr) = (inner.scheme, inner.tr);
    let finished = inner
        .agent_rows()
        .iter()
        .filter(|row| row.group == AgentGroup::Finished)
        .count();
    let current = inner.rail_tab;
    drop(inner);
    let agents = if finished == 0 {
        tr.agents_tab.to_owned()
    } else {
        (tr.agents_tab_count)(finished)
    };
    // The tab not shown is in the muted color, so the underline is not
    // the only thing that tells the two apart.
    let tab = |title: String, tab: RailTab| {
        let pick = ws.clone();
        let theme = if current == tab {
            scheme.theme()
        } else {
            scheme.muted_theme()
        };
        ThemeProvider::new(
            theme,
            Tab::new(title)
                .appearance(TabAppearance::Underline)
                .text_size(13.0)
                .padding_h(12.0)
                .min_height(TABS_HEIGHT - 1.0)
                .indicator(scheme.lanes[0], 2.0)
                .selected(current == tab)
                .on_select(move || pick.set_rail_tab(tab)),
        )
    };
    Column::new()
        .width(rail_inner())
        .child(
            Pad::insets(EdgeInsets::symmetric(PAD - 12.0, 0.0)).child(
                TabBar::new()
                    .gap(0.0)
                    .child(tab(tr.projects.to_owned(), RailTab::Projects))
                    .child(tab(agents, RailTab::Agents)),
            ),
        )
        .child(Fill::new(rail_inner(), 1.0, scheme.line))
}

/// The Agents tab's rows, into `list`.
pub fn agents(mut list: Column, ws: &Workspace, inner: &Inner) -> Column {
    let (scheme, tr) = (inner.scheme, inner.tr);
    let mut rows = inner.agent_rows();
    if rows.is_empty() {
        // Say what fills the list and offer the way to fill it.
        let start = ws.clone();
        let has_worktree = inner.active_key().is_some();
        return list
            .child(
                Pad::insets(EdgeInsets {
                    left: PAD,
                    right: PAD,
                    top: 10.0,
                    bottom: 2.0,
                })
                .child(Text::new(tr.agents_none).size(13.0)),
            )
            .child(
                Pad::insets(EdgeInsets::symmetric(PAD, 2.0)).child(ThemeProvider::new(
                    scheme.muted_theme(),
                    Column::new().width(rail_inner() - PAD * 2.0).child(
                        Text::new(tr.agents_none_hint)
                            .size(12.0)
                            .wrap(schnellui::widgets::WrapMode::Word),
                    ),
                )),
            )
            .child(
                Pad::insets(EdgeInsets::symmetric(PAD, 8.0)).child(
                    // The page's one action, so it is the filled button.
                    Button::new(tr.agents_start)
                        .appearance(schnellui::widgets::ButtonAppearance::Solid)
                        .text_size(13.0)
                        .height(30.0)
                        .disabled(!has_worktree)
                        .on_click(move || start.open_agent_list()),
                ),
            );
    }
    // Most urgent first, earlier sessions last seen first; the sort is
    // stable, so project order stays.
    rows.sort_by_key(|row| {
        use std::cmp::Reverse;
        (row.group, Reverse(row.attention), Reverse(row.seen))
    });
    list = list.child(section(scheme, tr.agents_finished));
    let finished = rows.iter().filter(|r| r.group == AgentGroup::Finished);
    let mut any = false;
    for row in finished {
        any = true;
        list = list.child(agent_row(ws, inner, row));
    }
    if !any {
        list = list.child(note(scheme, tr.agents_none_finished.to_owned()));
    }
    for (index, group, title) in [
        (0, AgentGroup::Working, tr.agents_working),
        (1, AgentGroup::Merged, tr.agents_merged),
        (2, AgentGroup::Earlier, tr.agents_earlier),
    ] {
        let count = rows.iter().filter(|r| r.group == group).count();
        if count == 0 {
            continue;
        }
        let expanded = inner.agents_expanded[index];
        let toggle = ws.clone();
        list = list.child(
            Pad::insets(EdgeInsets {
                top: 8.0,
                ..EdgeInsets::default()
            })
            .child(
                ListRow::new(format!(
                    "{} {}",
                    if expanded { "▾" } else { "▸" },
                    title(count)
                ))
                .width(rail_inner())
                .height(28.0)
                .on_click(move || toggle.toggle_agent_group(index)),
            ),
        );
        if expanded {
            for row in rows.iter().filter(|r| r.group == group) {
                list = list.child(agent_row(ws, inner, row));
            }
        }
    }
    list
}

/// One agent: its title, the project and branch under it, and on
/// the right what the agent waits for or the change request's state. A
/// button beside it opens the change request's discussion when the forge
/// can show it.
fn agent_row(ws: &Workspace, inner: &Inner, row: &AgentRow) -> impl View + use<> {
    let (scheme, tr) = (inner.scheme, inner.tr);
    let (open, discuss) = (ws.clone(), ws.clone());
    let (id, path, resume, panel) = (row.project, row.path.clone(), row.resume.clone(), row.panel);
    let discuss_path = row.path.clone();
    let place = match &row.branch {
        Some(branch) => format!("{} · {branch}", row.project_name),
        None => row.project_name.clone(),
    };
    // Only a row for the shown worktree is marked, and of its agents the
    // one in the focused pane.
    let active = row.group != AgentGroup::Earlier
        && inner
            .state
            .active()
            .is_some_and(|p| p.id == row.project && p.current_worktree() == row.path)
        && row.panel.is_none_or(|panel| {
            inner
                .active_live()
                .and_then(|live| live.focused_panel())
                .is_some_and(|(focused, _)| *focused == panel)
        });
    let discussion = row.discussions && row.change.as_ref().is_some_and(|c| c.number.is_some());
    let width = if discussion {
        rail_inner() - INDENT - DISCUSSION_WIDTH
    } else {
        rail_inner() - INDENT
    };
    let mut item = ListRow::new(truncate(&row.title, TITLE_CHARS))
        .width(width)
        .meta_size(12.0)
        .selected(active)
        .selected_background(scheme.selected())
        .on_click(move || match (&resume, panel) {
            (Some(agent), _) => open.resume_agent_session(id, &path, agent),
            (None, Some(panel)) => open.focus_agent(id, &path, panel),
            (None, None) => open.activate_worktree(id, &path),
        });
    let change = change_label(tr, row);
    if row.attention.wants_user() && row.group != AgentGroup::Merged {
        let color = scheme.attention_color(row.attention);
        let subtitle = if row.message.is_empty() {
            place
        } else {
            truncate(&row.message, MESSAGE_CHARS)
        };
        item = item
            .dot(color)
            .trailing(tr.attention_label(row.attention))
            .trailing_pill(color)
            .subtitle(subtitle)
            .value(match change {
                Some(change) => format!("{} · {change}", tr.attention_description(row.attention)),
                None => tr.attention_description(row.attention).to_owned(),
            });
    } else {
        item = item.subtitle(place);
        let busy = (row.attention == Attention::Busy).then(|| tr.attention_label(row.attention));
        let trailing = match (busy, change) {
            (Some(busy), Some(change)) => format!("{busy} · {change}"),
            (Some(busy), None) => busy.to_owned(),
            (None, Some(change)) => change,
            (None, None) => String::new(),
        };
        if !trailing.is_empty() {
            item = item.trailing(trailing);
        }
    }
    let mut line = Row::new().align(Align::Center).child(item);
    if discussion {
        line = line.child(super::rail::icon_button(
            MdIcon::outlined("forum", outlined::ICON_FORUM).color(scheme.muted),
            Button::new((tr.discussion_open)(&row.title))
                .tooltip((tr.discussion_open)(&row.title))
                .on_click(move || discuss.open_discussion(id, &discuss_path)),
        ));
    }
    Pad::insets(EdgeInsets {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: INDENT,
    })
    .child(line)
}

/// `!12 open`, `!12 merged` or `!12 closed`; `None` without a change request.
fn change_label(tr: &Strings, row: &AgentRow) -> Option<String> {
    let change = row.change.as_ref()?;
    let state = match change.state {
        BranchState::None => return None,
        BranchState::Open => tr.change_open,
        BranchState::Merged => tr.change_merged,
        BranchState::Closed => tr.change_closed,
    };
    Some(match change.number {
        Some(number) => format!("!{number} {state}"),
        None => state.to_owned(),
    })
}

fn section(scheme: &Scheme, title: &'static str) -> impl View + use<> {
    Pad::insets(EdgeInsets {
        top: 12.0,
        right: PAD,
        bottom: 4.0,
        left: PAD,
    })
    .child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(title).size(12.0),
    ))
}

fn note(scheme: &Scheme, text: String) -> impl View + use<> {
    Pad::insets(EdgeInsets::symmetric(PAD, 6.0)).child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(12.0).ellipsis(),
    ))
}
