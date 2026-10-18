//! View construction. Each region (rail, tab strip, panel, status bar) is
//! built by its own function so a change rebuilds only that region through a
//! `SubtreeReplacement`; a full remount happens only on resize.

mod agents;
mod approval;
mod change;
mod machines;
mod notifications;
mod palette;
mod panel;
mod rail;
mod settings;
mod setup;
mod status;
mod tabs;
mod tasks;
mod theme;

use schnellui::a11y::Role;
use schnellui::layout::EdgeInsets;
use schnellui::scene::{Color, Primitive, Rect, Size, WidgetId, WidgetKind};
use schnellui::widgets::{BuildCtx, Column, Pad, Row, Stack, View, node_rect};

use crate::workspace::Workspace;

pub use agents::tabs as rail_tabs;
pub use approval::approval;
pub use change::BAR as EXPLORE_BAR;
pub use change::GRID_TOP as EXPLORE_GRID_TOP;
pub use notifications::notifications;
#[cfg(test)]
pub use palette::{Action as PaletteAction, entries as palette_entries};
pub use palette::{
    CloneTarget, InputMode, PALETTE_BRANCH_INPUT, PALETTE_INPUT, PALETTE_PANEL_INPUT,
    PALETTE_VM_INPUT, PaletteState, list as palette_list, palette,
};
pub use panel::{pane_titles, panel};
#[cfg(test)]
pub use rail::TOKEN_OVERRIDE;
pub use rail::{
    RAIL_SEARCH_INPUT, branch_name, footer as sidebar_footer, list as sidebar_list,
    search as rail_search, sidebar,
};
#[cfg(test)]
pub use settings::TOAST_TTL;
pub use settings::{KEY_RECORDER, SETTINGS_FIND_INPUT, SettingsSection, SettingsState, settings};
pub use setup::{Setup, SetupStage, setup};
pub use status::status;
#[cfg(test)]
pub use status::status_parts;
pub use tabs::tabs;
pub use theme::{ReaderColors, Scheme, resolve as resolve_scheme};
#[cfg(test)]
pub use theme::{SCHEMES, active_lane};

pub const RAIL_WIDTH: f32 = 264.0;
/// The rail's width in a window narrower than [`NARROW_WINDOW`], so the
/// panels keep room for a split.
pub const NARROW_RAIL_WIDTH: f32 = 216.0;
/// Below this window width the rail narrows.
pub const NARROW_WINDOW: f32 = 1100.0;
/// The rail's width in a window narrower than [`SMALL_WINDOW`].
pub const SMALL_RAIL_WIDTH: f32 = 184.0;
/// Below this window width the rail narrows further.
pub const SMALL_WINDOW: f32 = 820.0;
/// Below this window width the rail hides itself, so the tabs, the panes
/// and the status bar keep their room; Ctrl+Shift+B still shows it.
pub const RAIL_HIDES_BELOW: f32 = 760.0;

thread_local! {
    /// The rail's width for the window last laid out. Every view that takes
    /// the window's size sets it, so the rail and the space beside it agree.
    static RAIL: std::cell::Cell<f32> = const { std::cell::Cell::new(RAIL_WIDTH) };
}

/// Sets the rail's width for a window `width` wide.
fn fit_rail(width: f32) {
    RAIL.with(|rail| {
        rail.set(if width < SMALL_WINDOW {
            SMALL_RAIL_WIDTH
        } else if width < NARROW_WINDOW {
            NARROW_RAIL_WIDTH
        } else {
            RAIL_WIDTH
        })
    });
}

/// The rail's width, hidden or not, for the window last laid out.
pub fn rail_width() -> f32 {
    RAIL.with(std::cell::Cell::get)
}
pub const TAB_HEIGHT: f32 = 34.0;
pub const STATUS_HEIGHT: f32 = 24.0;
/// Space around the panes inside the panel area.
pub const PANEL_INSET: f32 = 8.0;
/// Width of the slot each pane's border occupies on every side. The focused
/// pane draws 2 px of it in its lane color, the other pane 1 px in `LINE`.
pub const PANE_BORDER: f32 = 2.0;
/// Thickness of the line between the two halves of a split.
pub const DIVIDER: f32 = 2.0;
/// Gap on either side of a divider, which the pointer can grab as well.
pub const DIVIDER_SPACE: f32 = 2.0;
/// Space between the border slots of two neighboring panes.
pub const PANE_SEPARATOR: f32 = DIVIDER + DIVIDER_SPACE * 2.0;
/// Space a split pane's border slots take along each axis.
pub const PANE_FRAME: f32 = PANE_BORDER * 2.0;
/// Room between a split pane's border and its panel on the sides and at
/// the bottom, so text never touches the border.
pub const PANE_PAD: f32 = 4.0;
/// Height of the strip over each split pane that names its panel.
pub const PANE_TITLE: f32 = 27.0;

/// The whole window: rail on the left, tab strip, panel and status bar on the
/// right.
pub fn root(ws: &Workspace, viewport: Size) -> impl View + use<> {
    fit_rail(viewport.width);
    let main_width = main_width(ws, viewport);
    let mut row = Row::new().size(viewport.width, viewport.height);
    // A hidden rail is not mounted, so Tab never reaches its controls.
    if !ws.borrow().rail_hidden {
        row = row.child(sidebar(ws, viewport.height).with_ref(ws.regions().sidebar));
    }
    Stack::new()
        .size(viewport.width, viewport.height)
        .child(
            row.child(
                Column::new()
                    .size(main_width, viewport.height)
                    .child(tabs(ws, main_width).with_ref(ws.regions().tabs))
                    .child(below_tabs(ws, viewport, main_width)),
            ),
        )
        .child(palette(ws, viewport).with_ref(ws.regions().palette))
        .child(settings(ws, viewport).with_ref(ws.regions().settings))
        .child(notifications(ws, viewport).with_ref(ws.regions().notifications))
        .child(setup(ws, viewport).with_ref(ws.regions().setup))
        .child(approval(ws, viewport).with_ref(ws.regions().approval))
}

/// The panel with the status bar under it, in that order: Tab reaches the
/// terminal right after the tab strip, as it sits on screen. The terminal
/// keeps Tab for the shell, so the status bar's buttons are one Shift+Tab
/// back from the rail's first control, where focus wraps around.
fn below_tabs(ws: &Workspace, viewport: Size, main_width: f32) -> impl View + use<> {
    let height = (viewport.height - TAB_HEIGHT).max(STATUS_HEIGHT);
    Stack::new()
        .size(main_width, height)
        .child(panel(ws, panel_size(ws, viewport)).with_ref(ws.regions().panel))
        .child(
            Pad::insets(EdgeInsets {
                top: height - STATUS_HEIGHT,
                ..EdgeInsets::default()
            })
            .child(status(ws, main_width).with_ref(ws.regions().status)),
        )
}

/// Width of the area right of the rail.
pub fn main_width(ws: &Workspace, viewport: Size) -> f32 {
    fit_rail(viewport.width);
    (viewport.width - ws.rail_width()).max(200.0)
}

/// Space available to the active panel's content.
pub fn panel_size(ws: &Workspace, viewport: Size) -> Size {
    fit_rail(viewport.width);
    Size {
        width: (viewport.width - ws.rail_width() - PANEL_INSET * 2.0).max(100.0),
        height: (viewport.height - TAB_HEIGHT - STATUS_HEIGHT - PANEL_INSET * 2.0).max(60.0),
    }
}

/// Replaces the home directory prefix with `~`.
pub fn short_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text,
    }
}

/// [`short_path`] cut to its last `max` characters at a path separator, so
/// the end of a long path (the part that differs between worktrees) stays
/// visible: `…/src/web-search`.
pub fn tail_path(path: &std::path::Path, max: usize) -> String {
    let text = short_path(path);
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let skip = count - max.saturating_sub(1);
    let tail: String = text.chars().skip(skip).collect();
    match tail.find('/') {
        Some(slash) if slash + 1 < tail.len() => format!("…{}", &tail[slash..]),
        _ => format!("…{tail}"),
    }
}

/// Fits a panel title such as `~/src/app - fish` into `max` characters.
/// The program after the last ` - ` stays whole and the directory before it
/// loses its start, since its end tells panes apart. Other titles lose their
/// end.
pub fn fit_title(title: &str, max: usize) -> String {
    let count = title.chars().count();
    if count <= max {
        return title.to_owned();
    }
    if let Some((place, program)) = title.rsplit_once(" - ") {
        let keep = max.saturating_sub(program.chars().count() + 3 + 1);
        if keep >= 6 {
            let skip = place.chars().count().saturating_sub(keep);
            let tail: String = place.chars().skip(skip).collect();
            let tail = match tail.find('/') {
                Some(slash) if slash + 1 < tail.len() => &tail[slash..],
                _ => tail.as_str(),
            };
            return format!("…{tail} - {program}");
        }
    }
    truncate(title, max)
}

/// Cuts `text` to `max` characters, ending in an ellipsis when it was longer.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    // A cut at a space or a separator leaves none of it before the
    // ellipsis: "cmdterm - fish" cut reads "cmdterm…", not "cmdterm - …".
    let kept = out
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '·' | ','))
        .len();
    if kept > 0 {
        out.truncate(kept);
    }
    out.push('…');
    out
}

/// Width of a button sized to its `text` at `size` px: the label with the
/// same 16 px on either side the fixed-width buttons beside it get, about.
/// SchnellUI's own fit leaves 7 px, which reads cramped next to them.
pub fn text_button_width(text: &str, size: f32) -> f32 {
    (text.chars().count() as f32 * size * 0.55 + 32.0).ceil()
}

// --- primitives --------------------------------------------------------------

/// Gives a widget an accessible name. Text inputs show their label as a
/// placeholder only; the name is what the key handlers match.
pub struct Named<V>(pub std::borrow::Cow<'static, str>, pub V);

impl<V: View + 'static> View for Named<V> {
    fn build(self: Box<Self>, ctx: &mut BuildCtx, parent: Option<WidgetId>) -> WidgetId {
        let Named(name, view) = *self;
        let id = Box::new(view).build(ctx, parent);
        ctx.scene.a11y_mut(id).name = Some(name.into_owned());
        id
    }
}

/// Stands in for a closed overlay (the palette, the settings). The root
/// `Stack` stretches it over the whole window, so it is hidden or it would
/// take every pointer hit.
pub struct Closed;

impl View for Closed {
    fn build(self: Box<Self>, ctx: &mut BuildCtx, parent: Option<WidgetId>) -> WidgetId {
        let id = Box::new(Fill::new(0.0, 0.0, Color::TRANSPARENT)).build(ctx, parent);
        ctx.scene.set_visible(id, false);
        id
    }
}

/// A solid rectangle behind a region. Semantically transparent.
pub struct Fill {
    size: Size,
    color: Color,
}

impl Fill {
    pub fn new(width: f32, height: f32, color: Color) -> Self {
        Self {
            size: Size { width, height },
            color,
        }
    }
}

impl View for Fill {
    fn build(self: Box<Self>, ctx: &mut BuildCtx, parent: Option<WidgetId>) -> WidgetId {
        let this = *self;
        let id = ctx.scene.insert(WidgetKind::Image, parent);
        ctx.scene.a11y_mut(id).role = Role::Group.as_u16();
        let rect: Rect = node_rect(ctx.scene, id, this.size);
        ctx.scene
            .paint_mut(id)
            .primitives
            .push(Primitive::SolidRect {
                rect,
                color: this.color,
                corner_radius: 0.0,
            });
        ctx.layout
            .set_measure(id, Box::new(move |_available| this.size));
        id
    }
}

#[cfg(test)]
mod path_tests {
    use super::tail_path;
    use std::path::Path;

    #[test]
    fn titles_keep_their_program() {
        use super::fit_title;
        assert_eq!(fit_title("~/a - fish", 20), "~/a - fish");
        assert_eq!(
            fit_title("~/Documents/prog/cmdterm-uirevopus - fish", 30),
            "…/cmdterm-uirevopus - fish"
        );
        assert_eq!(
            fit_title("a very long title without dash", 10),
            "a very lo…"
        );
    }

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(tail_path(Path::new("/srv/app"), 20), "/srv/app");
        let cut = tail_path(
            Path::new("/tmp/aaaaaaaaaaaaaaaaaaaa/bbbbbbbb/src/web-search"),
            20,
        );
        assert_eq!(cut, "…/src/web-search");
        assert!(cut.chars().count() <= 20);
    }
}
