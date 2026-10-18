//! The tasks panel as widgets: every worktree's panels, the extension tasks
//! and the app's threads in rows of the UI font. A panel's row is a button
//! that opens its tab. The lines come from [`crate::taskview::TasksPanel`],
//! whose sampler runs as before; new lines mount this page again.

use std::rc::Rc;

use schnellui::layout::EdgeInsets;
use schnellui::scene::{Color, Size};
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, Justify, Pad, Row, Scroll, Stack, Text, ThemeProvider,
    View,
};

use super::Fill;
use super::theme::Scheme;
use crate::taskview::{RowKind, RowTone, TaskRow, TasksPanel};
use crate::workspace::Workspace;

/// Widths of the state, kind and panel columns.
const STATE: f32 = 112.0;
const KIND: f32 = 92.0;
const PANEL: f32 = 300.0;
/// Height of an item row.
const ROW: f32 = 28.0;
/// Space left of the page and of the rows under a section.
const PAD: f32 = 16.0;
const INDENT: f32 = 12.0;
/// Widest the rows grow: about what the longest "where" needs, so a
/// row's chevron stays near its text.
const MAX_WIDTH: f32 = 1200.0;

/// The page of `tasks` in a pane of `size`, under the panel's bar.
pub fn page(
    ws: &Workspace,
    tasks: &Rc<TasksPanel>,
    label: &'static str,
    size: Size,
    focus: bool,
) -> Box<dyn View> {
    let scheme = ws.borrow().scheme;
    let tr = ws.tr();
    let (header, rows) = tasks.rows(tr);
    // Wide enough for every column and a short detail; wider, the open
    // arrow drifts away from the row it belongs to.
    let width = (size.width - PAD * 2.0 - 12.0).clamp(200.0, MAX_WIDTH);
    let mut column = Column::new().width(width).gap(2.0).child(
        Pad::insets(EdgeInsets {
            top: 10.0,
            bottom: 4.0,
            ..EdgeInsets::default()
        })
        .child(ThemeProvider::new(
            scheme.muted_theme(),
            Text::new(header).size(12.5),
        )),
    );
    for row in rows {
        column = column.child(row_view(ws, scheme, row, width));
    }
    let scroll = Scroll::new()
        .label(label)
        .scrollbar(true)
        .size(size.width, size.height)
        .child(Pad::insets(EdgeInsets::symmetric(PAD, 0.0)).child(column));
    if focus {
        Box::new(scroll.with_ref(ws.regions().focus))
    } else {
        Box::new(scroll)
    }
}

fn tone_color(scheme: &Scheme, tone: RowTone) -> Color {
    match tone {
        RowTone::Ink => scheme.ink,
        RowTone::Muted => scheme.muted,
        RowTone::Good => scheme.lanes[4],
        RowTone::Warn => scheme.lanes[1],
        RowTone::Bad => scheme.lanes[3],
        RowTone::Accent => scheme.lanes[2],
    }
}

fn colored(scheme: &Scheme, color: Color, view: impl View + 'static) -> ThemeProvider {
    ThemeProvider::new(
        schnellui::widgets::Theme {
            text: color,
            ..scheme.theme()
        },
        view,
    )
}

/// A fixed-width cell of a row, cut with an ellipsis when it is longer.
fn cell(scheme: &Scheme, text: impl Into<String>, width: f32, color: Color) -> impl View {
    // Cut by characters: an ellipsis text in a row gives way to its
    // neighbours and the columns stop lining up.
    // About the average advance of 13px text, so a cell fills its column.
    let chars = (width / 6.5) as usize;
    fixed(
        width,
        colored(
            scheme,
            color,
            Text::new(super::truncate(&text.into(), chars)).size(13.0),
        ),
    )
}

/// `view` in a box `width` wide, so the columns of every row line up.
fn fixed(width: f32, view: impl View + 'static) -> Stack {
    Stack::new()
        .size(width, 18.0)
        .align(Align::Center)
        .justify(Justify::Start)
        .child(view)
}

fn row_view(ws: &Workspace, scheme: &'static Scheme, row: TaskRow, width: f32) -> Box<dyn View> {
    match row.kind {
        RowKind::Blank => Box::new(Fill::new(width, 6.0, scheme.page)),
        RowKind::Section => Box::new(
            Column::new()
                .width(width)
                .child(
                    Pad::insets(EdgeInsets {
                        top: 8.0,
                        bottom: 4.0,
                        ..EdgeInsets::default()
                    })
                    .child(Text::new(row.text).size(14.0)),
                )
                .child(Fill::new(width, 1.0, scheme.line)),
        ),
        RowKind::Group => Box::new(
            Pad::insets(EdgeInsets {
                top: 6.0,
                left: 0.0,
                ..EdgeInsets::default()
            })
            .child(
                Row::new()
                    .gap(10.0)
                    .align(Align::Center)
                    .child(Text::new(row.text).size(13.5))
                    .child(colored(
                        scheme,
                        scheme.muted,
                        Text::new(row.detail).size(12.5),
                    )),
            ),
        ),
        RowKind::Columns => Box::new(
            Pad::insets(EdgeInsets {
                left: INDENT + 8.0,
                top: 2.0,
                ..EdgeInsets::default()
            })
            .child(columns(scheme, &row, scheme.muted, 12.0)),
        ),
        RowKind::Message => Box::new(
            Pad::insets(EdgeInsets {
                left: INDENT,
                top: 2.0,
                ..EdgeInsets::default()
            })
            .child(colored(
                scheme,
                scheme.muted,
                Text::new(row.text).size(12.5),
            )),
        ),
        // A hairline under each row keeps a long row on its line.
        RowKind::Item => Box::new(
            Column::new().child(item(ws, scheme, row, width)).child(
                Pad::insets(EdgeInsets {
                    left: INDENT,
                    ..EdgeInsets::default()
                })
                .child(Fill::new(width - INDENT, 1.0, scheme.raised)),
            ),
        ),
    }
}

/// The state, kind, panel and where columns of a row.
fn columns(scheme: &Scheme, row: &TaskRow, color: Color, size: f32) -> Row {
    let small = |text: &str| colored(scheme, color, Text::new(text.to_owned()).size(size));
    let mut line = Row::new().align(Align::Center);
    line = line.child(fixed(STATE, small(row.badge)));
    let text_width = if row.panel_kind.is_empty() {
        PANEL + KIND
    } else {
        line = line.child(fixed(KIND, small(row.panel_kind)));
        PANEL
    };
    line.child(fixed(text_width, small(&row.text)))
        .child(small(&row.detail))
}

/// A panel, task or thread row: its state in its color, the rest in ink or
/// muted. A row that opens a panel is a button with `›` at its end.
fn item(ws: &Workspace, scheme: &'static Scheme, row: TaskRow, width: f32) -> Box<dyn View> {
    let width = width - INDENT;
    let muted = row.tone == RowTone::Muted;
    let ink = if muted { scheme.muted } else { scheme.ink };
    // A dot in the state's color leads the state, so states read apart
    // without relying on the text's color alone.
    let tone = tone_color(scheme, row.tone);
    let state = fixed(
        STATE,
        Row::new()
            .gap(6.0)
            .align(Align::Center)
            // A drawn dot, centered on the text; a glyph sits high.
            .child(
                schnellui::widgets::Panel::new()
                    .width(7.0)
                    .height(7.0)
                    .surface(tone)
                    .no_outline()
                    .radius(3.5),
            )
            .child(colored(scheme, tone, Text::new(row.badge).size(13.0))),
    );
    let mut line = Row::new().align(Align::Center).child(state);
    if !row.panel_kind.is_empty() {
        line = line.child(cell(scheme, row.panel_kind, KIND, scheme.muted));
    }
    // A row without a kind gives its column to the text.
    let text_width = if row.panel_kind.is_empty() {
        PANEL + KIND
    } else {
        PANEL
    };
    line = line.child(cell(scheme, row.text.clone(), text_width, ink));
    let detail = colored(
        scheme,
        scheme.muted,
        Text::new(row.detail.clone()).size(12.5),
    );
    let content = Row::new()
        .size(width - 16.0, ROW)
        .justify(Justify::SpaceBetween)
        .align(Align::Center)
        .child(line.child(detail));
    let Some(jump) = row.jump else {
        return Box::new(
            Pad::insets(EdgeInsets {
                left: INDENT + 8.0,
                ..EdgeInsets::default()
            })
            .child(content),
        );
    };
    let content = content.child(
        schnellui_icons_md::MdIcon::outlined(
            "chevron_right",
            schnellui_icons_md::outlined::ICON_CHEVRON_RIGHT,
        )
        .size(18.0)
        .color(scheme.muted),
    );
    let open = ws.clone();
    let name = format!("{} {}", row.text, row.detail);
    let button = Button::new(name)
        .icon_only()
        .appearance(ButtonAppearance::Ghost)
        .width(width)
        .height(ROW)
        // No tooltip: the chevron says the row opens, and a tooltip would
        // hang over the row above.
        .on_click(move || {
            open.activate_worktree(jump.project, &jump.path);
            open.select_tab(jump.tab);
        });
    Box::new(
        Pad::insets(EdgeInsets {
            left: INDENT,
            ..EdgeInsets::default()
        })
        .child(
            Stack::new()
                .size(width, ROW)
                .child(
                    Pad::insets(EdgeInsets {
                        left: 8.0,
                        ..EdgeInsets::default()
                    })
                    .child(content),
                )
                // The button on top takes the pointer and lights up under
                // it; the text only paints.
                .child(button),
        ),
    )
}
