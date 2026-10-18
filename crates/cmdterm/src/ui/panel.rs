//! The panel area: the shown tab's one pane, or its tree of split panes with
//! a border each.
//! The focused pane gets a 2 px border in the active lane color and a raised
//! title strip with a lane-colored underline. The others keep a 1 px `LINE`
//! and a muted title. Every pane is a drop target for tabs dragged from the
//! strip.

use crate::state::{LEAST_SHARE, PaneLayout, SplitAxis};
use schnellui::layout::EdgeInsets;
use schnellui::scene::{Color, Size};
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, DockArea, DockPosition, DragSource, Justify, ListRow,
    Pad, Referenced, Row, Splitter, Stack, Text, ThemeProvider, View,
};

use super::theme::{Scheme, active_lane};
use super::{
    DIVIDER, Fill, PANE_BORDER, PANE_FRAME, PANE_PAD, PANE_SEPARATOR, PANE_TITLE, PANEL_INSET,
};
use crate::browser::WEB_CONTENT_NAMES;
use crate::debug_browser::DEBUG_BROWSER_NAMES;
use crate::explore::EXPLORE_NAMES;
use crate::files::FILES_NAMES;
use crate::keymap::Action;
use crate::monitor::MONITOR_NAMES;
use crate::taskview::TASKS_NAMES;
use crate::terminal_panel::{GridStyle, TERMINAL_NAMES};
use crate::workspace::{LivePanel, LiveWorktree, MAX_PANES, Workspace, split_sizes};

pub fn panel(ws: &Workspace, size: Size) -> impl View + use<> {
    let style = ws.borrow().grid_style();
    let lane = active_lane(&ws.borrow());
    let scheme = ws.borrow().scheme;
    let content = ws.with_active(|live| -> Box<dyn View> {
        let Some(live) = live else {
            return Box::new(empty_state(ws));
        };
        if live.layout().len() == 1 {
            let view = pane_view(ws, live.pane_panel(0), 0, size, &style, true);
            return Box::new(dock_target(ws, 0, size, view));
        }
        let mut panes = Panes {
            ws,
            live,
            style: &style,
            lane,
            scheme,
            next: 0,
        };
        panes.view(live.layout(), size)
    });
    let outer = Size {
        width: size.width + PANEL_INSET * 2.0,
        height: size.height + PANEL_INSET * 2.0,
    };
    // A pane shows its focus with its frame, and a lone pane with its
    // cursor or chosen row. SchnellUI's ring would draw 3 px around a whole
    // terminal, or around a scroll view's last painted descendant instead
    // of its viewport, so panels wear an invisible one, as the change
    // request page does.
    let content = ThemeProvider::new(scheme.panel_theme(), content);
    Stack::new()
        .size(outer.width, outer.height)
        .child(Fill::new(outer.width, outer.height, scheme.page))
        .child(Pad::all(PANEL_INSET).child(content))
}

/// Builds the views of a split layout, numbering panes in layout order.
struct Panes<'a> {
    ws: &'a Workspace,
    live: &'a LiveWorktree,
    style: &'a GridStyle,
    lane: Color,
    scheme: &'static Scheme,
    /// Position of the next pane to build.
    next: usize,
}

impl Panes<'_> {
    /// The subtree `layout` filling `outer`, borders included. Matches
    /// [`crate::workspace::pane_sizes`], which sizes the terminals.
    fn view(&mut self, layout: &PaneLayout<u64>, outer: Size) -> Box<dyn View> {
        match layout {
            PaneLayout::Pane { panel } => {
                let pane = self.next;
                self.next += 1;
                let focused = pane == self.live.focused();
                // Inside the border: the title strip across the whole
                // width, then the panel with a little room on its sides and
                // under it. Matches `pane_sizes`.
                let inner = Size {
                    width: (outer.width - PANE_FRAME).max(1.0),
                    height: (outer.height - PANE_FRAME - PANE_TITLE).max(1.0),
                };
                let content = Size {
                    width: (inner.width - PANE_PAD * 2.0).max(1.0),
                    height: (inner.height - PANE_PAD).max(1.0),
                };
                let entry = self.live.panel(*panel);
                let view = pane_view(self.ws, entry, pane, content, self.style, focused);
                let title = pane_title(
                    self.ws,
                    entry,
                    pane,
                    inner.width,
                    focused,
                    self.lane,
                    self.scheme,
                )
                .with_ref(self.ws.regions().pane_titles[pane.min(MAX_PANES - 1)]);
                let boxed = Size {
                    width: inner.width,
                    height: inner.height + PANE_TITLE,
                };
                let view: Box<dyn View> = Box::new(
                    Column::new().child(title).child(
                        Pad::insets(EdgeInsets {
                            left: PANE_PAD,
                            right: PANE_PAD,
                            bottom: PANE_PAD,
                            top: 0.0,
                        })
                        .child(view),
                    ),
                );
                let view = framed(view, boxed, focused, self.lane, self.scheme);
                Box::new(dock_target(self.ws, pane, outer, view))
            }
            PaneLayout::Split {
                axis,
                first,
                second,
                share,
            } => {
                let spans = (first.span(*axis), second.span(*axis));
                let (a, b) = split_sizes(outer, *axis, spans, *share);
                let first = self.view(first, a);
                // The line after the last pane of the first half.
                let boundary = self.next - 1;
                let second = self.view(second, b);
                let (vertical, before, after, size) = match axis {
                    SplitAxis::SideBySide => (
                        true,
                        a.width,
                        b.width,
                        Size {
                            width: PANE_SEPARATOR,
                            height: outer.height,
                        },
                    ),
                    SplitAxis::Stacked => (
                        false,
                        a.height,
                        b.height,
                        Size {
                            width: outer.width,
                            height: PANE_SEPARATOR,
                        },
                    ),
                };
                let room = before + after;
                // A pane stays wide enough for about 20 columns, or tall
                // enough for about 6 rows, and never under a tenth.
                let font = self.ws.borrow().settings.font_size;
                let usable = if vertical {
                    font * 0.6 * 20.0 + PANE_FRAME + PANE_PAD * 2.0
                } else {
                    font * 1.3 * 6.0 + PANE_FRAME + PANE_TITLE + PANE_PAD
                };
                let least = (room * f32::from(LEAST_SHARE) / 1000.0)
                    .max(usable)
                    .min(room / 2.0);
                let ws = self.ws.clone();
                let splitter = Splitter::new(self.ws.tr().resize_panes, size, vertical)
                    .line(DIVIDER, self.scheme.line, self.lane)
                    .grip(self.scheme.muted)
                    .range((before - least).max(0.0), (after - least).max(0.0))
                    .on_resize(move |moved| {
                        let share = (1000.0 * (before + moved) / room).round() as u16;
                        ws.set_pane_share(boundary, share);
                    });
                match axis {
                    SplitAxis::SideBySide => {
                        Box::new(Row::new().child(first).child(splitter).child(second))
                    }
                    SplitAxis::Stacked => {
                        Box::new(Column::new().child(first).child(splitter).child(second))
                    }
                }
            }
        }
    }
}

/// The title strips of the shown tab's split panes, for rebuilding them
/// alone when a title changes. Empty for a tab with one pane.
pub fn pane_titles(ws: &Workspace, size: Size) -> Vec<(usize, Box<dyn View>)> {
    let scheme = ws.borrow().scheme;
    let lane = active_lane(&ws.borrow());
    ws.with_active(|live| {
        let Some(live) = live else {
            return Vec::new();
        };
        if live.layout().len() < 2 {
            return Vec::new();
        }
        let mut sizes = Vec::new();
        crate::workspace::pane_sizes(live.layout(), size, &mut sizes);
        sizes
            .iter()
            .enumerate()
            .take(MAX_PANES)
            .map(|(pane, content)| {
                let focused = pane == live.focused();
                let view: Box<dyn View> = Box::new(pane_title(
                    ws,
                    live.pane_panel(pane),
                    pane,
                    content.width + PANE_PAD * 2.0,
                    focused,
                    lane,
                    scheme,
                ));
                (pane, view)
            })
            .collect()
    })
}

/// The menu a pane's title strip opens under the pointer: what the
/// palette does to the focused pane, for this one. Only split tabs have
/// strips, so moving the pane out always leaves a tab behind.
fn pane_menu(
    ws: &Workspace,
    tr: &'static crate::i18n::Strings,
    pane: usize,
    id: u64,
) -> schnellui::widgets::ContextMenu {
    use crate::state::SplitAxis;
    use schnellui::widgets::{ContextMenu, ContextMenuItem};
    let mut menu = ContextMenu::new()
        .text_size(13.0)
        .row_height(28.0)
        .name((tr.pane_menu)(pane + 1));
    for (title, axis) in [
        (tr.split_side_by_side, SplitAxis::SideBySide),
        (tr.split_stacked, SplitAxis::Stacked),
    ] {
        let split = ws.clone();
        menu = menu.item(ContextMenuItem::new(title).on_select(move || {
            split.focus_pane(pane);
            split.split_pane(axis);
        }));
    }
    let out = ws.clone();
    let close = ws.clone();
    menu.item(
        ContextMenuItem::new(tr.move_pane_to_tab).on_select(move || {
            out.focus_pane(pane);
            out.move_pane_to_tab();
        }),
    )
    .item(ContextMenuItem::new(tr.close_pane).on_select(move || close.close_panel(id)))
}

/// Share of the lane, out of 256, in the focused pane's title strip.
const FOCUSED_STRIP_SHARE: u16 = 36;

/// The strip over a split pane: its number, its panel's title and a button
/// that closes the panel. The focused pane's strip is raised, with ink text
/// and a lane-colored underline; the others are muted. The number tells
/// panes with the same title apart, in the order Ctrl+Shift+O visits them.
fn pane_title(
    ws: &Workspace,
    entry: Option<&(u64, LivePanel)>,
    pane: usize,
    width: f32,
    focused: bool,
    lane: Color,
    scheme: &'static Scheme,
) -> impl View + use<> {
    let tr = ws.tr();
    let (id, title) = match entry {
        Some((id, panel)) => (Some(*id), panel.title()),
        None => (None, String::new()),
    };
    let color = if focused { scheme.ink } else { scheme.muted };
    let label = Row::new()
        .size(width, PANE_TITLE - 1.0)
        .align(Align::Center)
        .child(
            Pad::insets(EdgeInsets {
                left: 8.0,
                ..EdgeInsets::default()
            })
            .child(
                Row::new()
                    .gap(6.0)
                    .align(Align::Center)
                    .child(ThemeProvider::new(
                        scheme.muted_theme(),
                        Text::new(format!("{}", pane + 1)).size(11.5).bold(),
                    ))
                    .child(ThemeProvider::new(
                        schnellui::widgets::Theme {
                            text: color,
                            ..scheme.theme()
                        },
                        Text::new(super::fit_title(
                            &title,
                            ((width - 72.0) / 7.6).max(4.0) as usize,
                        ))
                        .size(12.5),
                    )),
            ),
        );
    // The strip is the pane's handle: dragged onto another pane's edge it
    // moves there, onto its middle it swaps with it, onto the new tab
    // button it becomes a tab of its own.
    let strip = Stack::new()
        .size(width, PANE_TITLE - 1.0)
        .child(Fill::new(
            width,
            PANE_TITLE - 1.0,
            // A share of the lane, as the focused pane's border has, so the
            // strips read apart on a light scheme too, where the raised
            // surface and the rail are close.
            if focused {
                super::theme::mix(scheme.raised, lane, FOCUSED_STRIP_SHARE)
            } else {
                scheme.rail
            },
        ))
        .child(label);
    let mut top = Stack::new().size(width, PANE_TITLE - 1.0);
    top = match id {
        Some(id) => {
            let (drag, end, pick) = (ws.clone(), ws.clone(), ws.clone());
            top.child(
                DragSource::new((tr.move_pane)(pane + 1))
                    .on_drag_start(move || drag.start_pane_drag(id))
                    .on_drag_end(move |_| end.end_tab_drag())
                    // A click on the strip focuses its pane, so the keys
                    // never land nowhere.
                    .on_click(move || {
                        pick.focus_pane(pane);
                        pick.refocus_panel();
                    })
                    .context_menu(pane_menu(ws, tr, pane, id))
                    .child(strip),
            )
        }
        None => top.child(strip),
    };
    if let Some(id) = id {
        let close = ws.clone();
        // Over the handle, so a press on it closes instead of dragging.
        // Inset from the strip's edges, so its focus ring shows whole
        // instead of running into the pane's lane-colored border.
        top = top.child(
            Row::new()
                .size(width, PANE_TITLE - 1.0)
                .align(Align::Center)
                .justify(Justify::End)
                .child(
                    Pad::insets(EdgeInsets {
                        right: 6.0,
                        ..EdgeInsets::default()
                    })
                    .child(
                        Button::new((tr.close_panel_named)(pane + 1, &title))
                            .display("×")
                            .appearance(ButtonAppearance::Ghost)
                            .text_size(16.0)
                            .width(22.0)
                            .height(PANE_TITLE - 8.0)
                            .tooltip(tr.close_panel)
                            // A drag source clicks on release, so a press
                            // slid off the button closes nothing.
                            .on_drag_start(|| {})
                            .on_click(move || close.close_panel(id)),
                    ),
                ),
        );
    }
    Column::new().child(top).child(Fill::new(
        width,
        1.0,
        if focused { lane } else { scheme.line },
    ))
}

/// Makes pane `pane` a drop target for dragged tabs and panes: an edge
/// moves what is dragged into a split on that side, the middle swaps a
/// dragged pane with this one. A chip in each zone says which.
fn dock_target(ws: &Workspace, pane: usize, size: Size, view: impl View) -> DockArea {
    let (dock, accepts) = (ws.clone(), ws.clone());
    let tr = ws.tr();
    DockArea::new((tr.pane_n)(pane + 1))
        .size(size.width, size.height)
        .zone_label(DockPosition::Left, tr.zone_left)
        .zone_label(DockPosition::Right, tr.zone_right)
        .zone_label(DockPosition::Top, tr.zone_top)
        .zone_label(DockPosition::Bottom, tr.zone_bottom)
        .zone_label(DockPosition::Center, tr.zone_swap)
        .accepts(move |position| accepts.dock_accepts(pane, position))
        .on_dock(move |position| dock.dock_dragged(pane, position))
        .child(view)
}

/// Draws a border around a pane's `content` box. The border always takes the
/// full `PANE_BORDER` slot so focus changes do not move the content.
fn framed(
    content: Box<dyn View>,
    size: Size,
    focused: bool,
    lane: Color,
    scheme: &Scheme,
) -> impl View + use<> {
    let (thickness, color) = if focused {
        (PANE_BORDER, lane)
    } else {
        (1.0, scheme.pane_edge())
    };
    let outer = Size {
        width: size.width + PANE_BORDER * 2.0,
        height: size.height + PANE_BORDER * 2.0,
    };
    let inset = PANE_BORDER - thickness;
    let ring = Size {
        width: outer.width - inset * 2.0,
        height: outer.height - inset * 2.0,
    };
    Stack::new()
        .size(outer.width, outer.height)
        .child(Fill::new(outer.width, outer.height, scheme.page))
        .child(
            Pad::all(inset).child(
                Stack::new()
                    .size(ring.width, ring.height)
                    .child(Fill::new(ring.width, ring.height, color))
                    .child(
                        Pad::all(thickness).child(
                            // The grid covers whole cells only; the page color
                            // fills the rest so the ring stays thin.
                            Stack::new()
                                .size(size.width, size.height)
                                .child(Fill::new(size.width, size.height, scheme.page))
                                .child(content),
                        ),
                    ),
            ),
        )
}

/// The content of the pane at position `pane`. `focus` marks the pane that
/// takes focus after a rebuild.
fn pane_view(
    ws: &Workspace,
    panel: Option<&(u64, LivePanel)>,
    pane: usize,
    size: Size,
    style: &GridStyle,
    focus: bool,
) -> Box<dyn View> {
    let slot = pane.min(TERMINAL_NAMES.len() - 1);
    match panel {
        Some((_, LivePanel::Terminal(terminal))) => {
            terminal.mount_focused(focus);
            let grid = terminal.view(style, TERMINAL_NAMES[slot]);
            if focus {
                Box::new(grid.with_ref(ws.regions().focus))
            } else {
                Box::new(grid)
            }
        }
        Some((_, LivePanel::Files(files))) => {
            let colors = ws.borrow().scheme.reader_colors();
            let grid = files.view(style, colors, ws.tr(), FILES_NAMES[slot]);
            if focus {
                Box::new(grid.with_ref(ws.regions().focus))
            } else {
                Box::new(grid)
            }
        }
        Some((_, LivePanel::Explore(explore))) if explore.shows_change() => {
            super::change::page(ws, explore, EXPLORE_NAMES[slot], size, focus)
        }
        Some((_, LivePanel::Explore(explore))) => {
            let colors = ws.borrow().scheme.reader_colors();
            let grid = explore.view(style, colors, ws.tr(), EXPLORE_NAMES[slot]);
            let grid: Box<dyn View> = if focus {
                Box::new(grid.with_ref(ws.regions().focus))
            } else {
                Box::new(grid)
            };
            super::change::grid_page(ws, explore, grid, size)
        }
        Some((_, LivePanel::Monitor(monitor))) => {
            let colors = ws.borrow().scheme.reader_colors();
            let grid = monitor.view(style, colors, ws.tr(), MONITOR_NAMES[slot]);
            let grid: Box<dyn View> = if focus {
                Box::new(grid.with_ref(ws.regions().focus))
            } else {
                Box::new(grid)
            };
            let tr = ws.tr();
            // The row labels in the UI font, over the grid's empty label
            // column, so the panel reads like the rest of the app.
            let cell = style.metrics;
            let body = Size {
                width: size.width,
                height: (size.height - super::EXPLORE_BAR).max(0.0),
            };
            let mut page = Stack::new()
                .size(body.width, body.height)
                .align(Align::Start)
                .justify(Justify::Start)
                .child(grid);
            for (row, text) in monitor.labels(tr) {
                page = page.child(
                    Pad::insets(EdgeInsets {
                        left: cell.width,
                        // The text's line box sets its letters above the
                        // middle of a cell, so a nudge down centers them on
                        // the row's bar.
                        top: row as f32 * cell.height + LABEL_NUDGE,
                        ..EdgeInsets::default()
                    })
                    .child(
                        Stack::new()
                            .size(cell.width * 14.0, cell.height)
                            .align(Align::Center)
                            .justify(Justify::Start)
                            .child(ThemeProvider::new(
                                ws.borrow().scheme.muted_theme(),
                                Text::new(text).size(13.0),
                            )),
                    ),
                );
            }
            bar_page(
                ws,
                tr.monitor,
                tr.monitor_note,
                None,
                Box::new(page),
                size,
                f32::INFINITY,
            )
        }
        Some((_, LivePanel::Tasks(tasks))) => {
            let tr = ws.tr();
            let bar = super::EXPLORE_BAR;
            let body = Size {
                width: size.width,
                height: (size.height - bar).max(0.0),
            };
            let page = super::tasks::page(ws, tasks, TASKS_NAMES[slot], body, focus);
            let again = tasks.clone();
            let refresh = Button::new(tr.tasks_refresh)
                .appearance(ButtonAppearance::Outline)
                .text_size(12.5)
                .height(bar - 12.0)
                .tooltip("r")
                .on_click(move || again.refresh());
            // Reload sits at the bar's end, where the other panels keep
            // their actions.
            bar_page(
                ws,
                tr.tasks,
                tr.tasks_note,
                Some(refresh),
                page,
                size,
                f32::INFINITY,
            )
        }
        Some((_, LivePanel::DebugBrowser(debug))) => {
            let view = debug.view(size, DEBUG_BROWSER_NAMES[slot]);
            if focus {
                Box::new(Referenced::new(ws.regions().focus).child(view))
            } else {
                view
            }
        }
        Some((_, LivePanel::Browser(browser))) => {
            let view = browser.view(size.width, size.height, WEB_CONTENT_NAMES[slot]);
            if focus {
                Box::new(Referenced::new(ws.regions().focus).child(view))
            } else {
                view
            }
        }
        None => Box::new(empty_state(ws)),
    }
}

/// How far a monitor row's label sits below the top of its cell.
const LABEL_NUDGE: f32 = 2.0;

/// A grid panel under a bar like the explore panel's: its name and what it
/// shows on the left in the UI font, an action on the right.
fn bar_page(
    ws: &Workspace,
    title: &'static str,
    note: &'static str,
    action: Option<Button>,
    grid: Box<dyn View>,
    size: Size,
    max_width: f32,
) -> Box<dyn View> {
    let scheme = ws.borrow().scheme;
    let bar = super::EXPLORE_BAR;
    let left = Row::new()
        .gap(10.0)
        .align(Align::Center)
        .child(Text::new(title).size(13.5))
        .child(ThemeProvider::new(
            scheme.muted_theme(),
            Text::new(note).size(12.5),
        ));
    let mut row = Row::new()
        .size((size.width - 24.0).min(max_width), bar - 1.0)
        .justify(Justify::SpaceBetween)
        .align(Align::Center)
        .child(left);
    if let Some(action) = action {
        row = row.child(action);
    }
    Box::new(
        Column::new()
            .size(size.width, size.height)
            .child(Pad::insets(EdgeInsets::symmetric(12.0, 0.0)).child(row))
            .child(Fill::new(size.width, 1.0, scheme.line))
            .child(grid),
    )
}

fn empty_state(ws: &Workspace) -> impl View + use<> {
    let muted = ws.borrow().scheme.muted_theme();
    let tr = ws.tr();
    let press = |action: &Action, otherwise: &str| match ws.borrow().keymap.keys_for(action) {
        Some(keys) => (tr.press)(&keys.to_string()),
        None => otherwise.to_owned(),
    };
    let (title, message) = if ws.borrow().state.projects.is_empty() {
        (
            tr.open_a_project,
            (tr.then_open_project)(&press(&Action::Palette, tr.open_the_palette)),
        )
    } else if ws.borrow().active_key().is_some() {
        (
            tr.no_panels,
            (tr.no_panels_message)(&press(&Action::NewTerminal, tr.use_new_shell)),
        )
    } else {
        (tr.no_project, tr.select_project.to_owned())
    };
    // The ways out of an empty worktree, as rows the palette's look shares:
    // the action on the left, its keys on the right.
    let actions: &[Action] = if ws.borrow().state.projects.is_empty() {
        &[Action::Palette]
    } else if ws.borrow().active_key().is_some() {
        &[Action::NewTerminal, Action::Files, Action::Explore]
    } else {
        &[]
    };
    let mut column = Column::new()
        .gap(8.0)
        .child(Text::new(title).size(20.0))
        .child(ThemeProvider::new(muted, Text::new(message).size(13.0)));
    if !actions.is_empty() {
        let mut list = Column::new().gap(2.0);
        for action in actions {
            let run = ws.clone();
            let action = action.clone();
            let mut row = ListRow::new(tr.action(&action))
                .width(EMPTY_ACTIONS_WIDTH)
                .height(32.0);
            if let Some(keys) = ws.borrow().keymap.keys_for(&action) {
                row = row.trailing(keys.to_string());
            }
            list = list.child(row.on_click(move || run.run_action(&action, 0)));
        }
        column = column
            .child(Fill::new(1.0, 4.0, Color::TRANSPARENT))
            .child(list);
    }
    column
}

/// Width of the action rows of an empty worktree.
const EMPTY_ACTIONS_WIDTH: f32 = 320.0;
