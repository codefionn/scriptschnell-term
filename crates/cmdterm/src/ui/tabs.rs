//! The tab strip above the panel. Each tab owns its own split panes and is
//! named after its focused pane, with `+N` for the other panes, after an
//! icon of that pane's kind. The shown tab
//! is underlined in the active worktree's lane color. Tabs reorder by dragging
//! along the strip, and a tab dropped on a pane edge joins the shown tab as a
//! split (see `panel.rs`). A tab whose terminal waits for the user is
//! written, and underlined while shown, in that wait's attention color. The
//! `+` after the last tab opens the palette's
//! list of panels to open; the command button at the right end opens the
//! whole palette. While the app draws the window's title bar, the window's
//! minimize, maximize and close buttons end the strip, and a press on its
//! empty part moves the window.

use crate::term::Attention;
use schnellui::layout::EdgeInsets;
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, ContextMenu, ContextMenuItem, DropTip, Justify, Pad,
    Row, Stack, Tab, TabAppearance, TabBar, Text, ThemeProvider, View,
};

use schnellui_icons_md::{MdIcon, outlined};

use super::theme::active_lane;
use super::{Fill, TAB_HEIGHT};
use crate::workspace::{LivePanel, Workspace};

/// Side of the square `+` button after the last tab.
const ADD_SIZE: f32 = 24.0;
/// Side of the square command button at the right end.
const COMMAND_SIZE: f32 = 26.0;
/// Width of each of the window's buttons at the strip's right end.
const WINDOW_BUTTON: f32 = 40.0;
/// Below this strip width the command button shows only its magnifier.
const COMPACT_COMMANDS: f32 = 800.0;
/// Where a tab's icon starts, from the tab's left edge.
const ICON_LEFT: f32 = 8.0;
/// Side of a tab's icon.
const ICON_SIZE: f32 = 14.0;
/// Share of the lane tint in the shown tab's fill over the raised
/// surface, out of 256.
const ACTIVE_FILL_SHARE: u16 = 170;
/// Thickness of the active tab's underline.
const UNDERLINE: f32 = 2.0;

/// A title that starts with a path, as shells set it, with the path cut to
/// its last folder: `~/src/app - fish` becomes `app - fish`. The worktree's
/// path is in the sidebar and the status bar already.
fn short_title(title: &str) -> String {
    if !(title.starts_with("~/") || title.starts_with('/')) {
        return title.to_owned();
    }
    let end = title.find(' ').unwrap_or(title.len());
    let (path, rest) = title.split_at(end);
    match path.trim_end_matches('/').rsplit('/').next() {
        Some(last) if !last.is_empty() => format!("{last}{rest}"),
        _ => title.to_owned(),
    }
}

/// The icon of a panel's kind, shown before its tab's title.
fn kind_icon(panel: &LivePanel) -> (&'static str, &'static str) {
    match panel {
        LivePanel::Terminal(_) => ("terminal", outlined::ICON_TERMINAL),
        LivePanel::Files(_) => ("folder", outlined::ICON_FOLDER),
        // Each mode has its own icon, so a History and a Review tab side
        // by side read apart.
        LivePanel::Explore(explore) => match explore.mode() {
            crate::explore::Mode::History => ("history", outlined::ICON_HISTORY),
            crate::explore::Mode::Review => ("difference", outlined::ICON_DIFFERENCE),
            crate::explore::Mode::Change => ("merge", outlined::ICON_MERGE),
        },
        LivePanel::Monitor(_) => ("speed", outlined::ICON_SPEED),
        LivePanel::Tasks(_) => ("checklist", outlined::ICON_CHECKLIST),
        LivePanel::Browser(_) => ("public", outlined::ICON_PUBLIC),
        LivePanel::DebugBrowser(_) => ("bug_report", outlined::ICON_BUG_REPORT),
    }
}

/// Room a tab takes besides its title: the icon's lead, the padding, the
/// close button and the gap to the next tab. A cut tab measures 69 to 75 px
/// besides its title; any less pushes `+` and the command button off the
/// strip.
const TAB_CHROME: f32 = 72.0;
/// Width of a title character at the tab's text size, about: titles
/// measure near 6.3 px a character, and this leaves room for wide capitals
/// and the ellipsis.
const TAB_CHAR: f32 = 6.6;
/// Fewest characters a shortened title keeps before the strip turns to
/// icons.
const MIN_TITLE_CHARS: usize = 6;

/// Characters an icon tab beside the shown one gets when the strip has room
/// left: enough for a short name and its number, as "cmdter… (6)".
const NEIGHBOR_TITLE_CHARS: usize = 11;
/// The place number and its ". " that start every titled tab in icon modes.
const PLACE_CHARS: usize = 3;

/// Characters the shown tab keeps before a crowded strip adds icon tabs
/// beside it.
const SHOWN_TITLE_CHARS: usize = 24;

/// Room an icon-only tab takes among `count` tabs: the icon's lead, the
/// padding and the gap, and its number, which has two digits from the
/// tenth tab on. A split tab's pane count takes a little of the slack.
fn icon_tab(count: usize) -> f32 {
    let digits = (count.min(9) + 2 * count.saturating_sub(9)) as f32 / count.max(1) as f32;
    46.0 + 7.0 * digits
}

/// How the tabs fit the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    /// Every title whole.
    Whole,
    /// Every title cut to this many characters.
    Cut(usize),
    /// The shown tab's title cut to `max` characters, the others icons
    /// only, as a browser does with many tabs.
    Icons(usize),
    /// Too many even for icons: only `shown` tabs around the active one,
    /// the active one's title cut to `max` characters and the others icons;
    /// a button after them lists every tab.
    Window { max: usize, shown: usize },
}

/// Room kept for the button after a strip with hidden tabs; the button
/// itself is as wide as its label.
const MORE_WIDTH: f32 = 70.0;

/// How `count` tabs whose titles are `lengths` long fit in `room`. Every
/// title is cut to the same most characters, so no tab vanishes off the end
/// of the strip; short titles stay whole and leave their room to the long
/// ones. When even short titles do not fit, the hidden tabs show their icons.
fn fit_tabs(room: f32, count: usize, lengths: impl Iterator<Item = usize>) -> Fit {
    let lengths: Vec<usize> = lengths.collect();
    let width = |most: usize| {
        lengths
            .iter()
            .map(|&length| length.min(most))
            .sum::<usize>() as f32
            * TAB_CHAR
            + count as f32 * TAB_CHROME
    };
    let longest = lengths.iter().copied().max().unwrap_or(0);
    if count == 0 || width(longest) <= room {
        return Fit::Whole;
    }
    // The most characters every title may keep: the widths grow with it,
    // so a binary search finds the largest that fits.
    let (mut fits, mut over) = (0, longest);
    while over - fits > 1 {
        let mid = (fits + over) / 2;
        if width(mid) <= room {
            fits = mid;
        } else {
            over = mid;
        }
    }
    let share = fits;
    if share >= MIN_TITLE_CHARS {
        return Fit::Cut(share);
    }
    icon_fit(room, count, longest)
}

/// The icon or window fit for `count` tabs in `room`, the longest title
/// `longest` characters: for a strip where cut titles no longer read.
fn icon_fit(room: f32, count: usize, longest: usize) -> Fit {
    // Icons for the others while the shown tab keeps a title that reads;
    // else a window of them.
    let left = room - (count - 1) as f32 * icon_tab(count) - TAB_CHROME;
    if left >= NEIGHBOR_TITLE_CHARS as f32 * TAB_CHAR {
        return Fit::Icons(((left / TAB_CHAR) as usize).min(40));
    }
    // As many icon tabs as fit beside the shown tab's short title, so the
    // strip fills before any tab hides behind the button.
    let room = room - MORE_WIDTH - 8.0;
    // The shown tab keeps its title, up to a length that reads, before
    // any more icons join it: one more icon tab is worth less than knowing
    // which tab is shown.
    let reserve = longest.clamp(NEIGHBOR_TITLE_CHARS, SHOWN_TITLE_CHARS);
    let active = TAB_CHROME + reserve as f32 * TAB_CHAR;
    let others = ((room - active) / icon_tab(count)).max(0.0) as usize;
    let shown = (others + 1).min(count);
    // The shown tab's title takes what the icons leave.
    let left = room - (shown - 1) as f32 * icon_tab(count) - TAB_CHROME;
    Fit::Window {
        max: ((left / TAB_CHAR) as usize).clamp(MIN_TITLE_CHARS, 40),
        shown,
    }
}

/// Whether two tabs other than the shown one (which keeps its whole title
/// when `active_whole`) read the same once cut to `max` characters.
fn cut_alike(names: &[Option<String>], active: usize, max: usize, active_whole: bool) -> bool {
    let mut seen = std::collections::HashSet::new();
    names.iter().enumerate().any(|(index, title)| {
        if active_whole && index == active {
            return false;
        }
        let Some(title) = title else {
            return false;
        };
        !seen.insert(truncate_keeping_suffix(title, max))
    })
}

/// The most characters the other tabs may keep when the tab at `active`
/// keeps its whole title, or `None` when that would cut them below
/// [`MIN_TITLE_CHARS`].
fn cut_around(room: f32, lengths: &[usize], active: usize) -> Option<usize> {
    let whole = *lengths.get(active)?;
    let left = room - whole as f32 * TAB_CHAR - TAB_CHROME;
    let others = lengths
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != active)
        .map(|(_, length)| *length);
    match fit_tabs(left, lengths.len() - 1, others) {
        Fit::Whole => Some(usize::MAX),
        Fit::Cut(max) => Some(max),
        _ => None,
    }
}

/// Cuts a tab title to `max` characters from the middle of its name, so
/// what tells alike tabs apart at its end stays: " · 3 panes", " (2)".
/// When the whole end does not fit, its last part (the number) still does.
fn truncate_keeping_suffix(title: &str, max: usize) -> String {
    if title.chars().count() <= max {
        return title.to_owned();
    }
    // What may stay whole after the cut name, longest first: the number
    // with the pane count, the number with the count as "·3", the count's
    // words, the count as "·3", and the number alone.
    let mut tails: Vec<(&str, String)> = Vec::new();
    let pane_at = title.rfind(" · ");
    let number_at = title.rfind(" (");
    let count: String = pane_at
        .map(|at| {
            title[at + " · ".len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect()
        })
        .unwrap_or_default();
    match (number_at, pane_at) {
        // "name (2) · 3 panes"
        (Some(number), Some(panes)) if number < panes => {
            tails.push((&title[..number], title[number..].to_owned()));
            if !count.is_empty() {
                tails.push((
                    &title[..number],
                    format!("{} · {count}", &title[number..panes]),
                ));
            }
            tails.push((&title[..panes], title[panes..].to_owned()));
            if !count.is_empty() {
                tails.push((&title[..panes], format!(" · {count}")));
            }
        }
        // "name · 2 panes (2)", or a number alone
        (Some(number), _) => {
            if let Some(panes) = pane_at {
                tails.push((&title[..panes], title[panes..].to_owned()));
            }
            tails.push((&title[..number], title[number..].to_owned()));
        }
        (None, Some(panes)) => {
            tails.push((&title[..panes], title[panes..].to_owned()));
            if !count.is_empty() {
                tails.push((&title[..panes], format!(" · {count}")));
            }
        }
        (None, None) => {}
    }
    let last = tails.len().saturating_sub(1);
    for (index, (head, tail)) in tails.into_iter().enumerate() {
        let tail_len = tail.chars().count();
        // The head keeps three characters and the ellipsis while a shorter
        // tail is left to try, and at least one at the end or to keep the
        // number, which tells alike tabs apart. The pane count's words give
        // way to its short form before they cut the name under seven
        // characters: the name tells tabs apart, the words do not.
        // A number keeps a name of six characters before it, or it goes:
        // the name tells tabs apart at a glance, the number only on a
        // closer look.
        let head_room = if index < last && tail.contains(" panes") {
            8
        } else if tail.contains(" (") {
            7
        } else if tail.starts_with(" · ") {
            // A pane count alone gives way before the name falls under
            // five characters: "cm… · 4" names no tab.
            6
        } else if index < last {
            4
        } else {
            2
        };
        if tail_len + head_room <= max {
            let mut out = super::truncate(head, max - tail_len);
            out.push_str(&tail);
            return out;
        }
    }
    super::truncate(title, max)
}

/// `title` without the " (2)" that tells it from alike titles, keeping a
/// pane count after it: "fish (2) · 3 panes" becomes "fish · 3 panes".
fn strip_number(title: &str) -> String {
    let Some(at) = title.rfind(" (") else {
        return title.to_owned();
    };
    let rest = &title[at + 2..];
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || !rest[digits..].starts_with(')') {
        return title.to_owned();
    }
    format!("{}{}", &title[..at], &rest[digits + 1..])
}

/// The first of `shown` tabs out of `count` in a window that keeps
/// `active` near its middle.
fn window_start(count: usize, shown: usize, active: usize) -> usize {
    active
        .saturating_sub(shown.saturating_sub(1) / 2)
        .min(count.saturating_sub(shown))
}

/// The window's first tab: where it was while that still shows `active`
/// whole, so a tab chosen inside the window does not slide the strip under
/// the pointer, else one that keeps `active` near its middle.
fn keep_window(previous: Option<usize>, count: usize, shown: usize, active: usize) -> usize {
    match previous {
        Some(start) if start <= active && active < start + shown && start + shown <= count => start,
        _ => window_start(count, shown, active),
    }
}

pub fn tabs(ws: &Workspace, width: f32) -> impl View + use<> {
    let lane = active_lane(&ws.borrow());
    let scheme = ws.borrow().scheme;
    let tr = ws.tr();
    let active_ref = ws.regions().active_tab;
    let palette_keys = ws
        .borrow()
        .keymap
        .keys_for(&crate::keymap::Action::Palette)
        .map(|keys| keys.to_string())
        .unwrap_or_default();
    // Below this width the command button keeps only its magnifier, so the
    // tabs keep their room.
    let compact = width < COMPACT_COMMANDS;
    // Icon, the word and the keys, with gaps, at about 6.5 px a character.
    let command_width = if compact {
        COMMAND_SIZE
    } else {
        6.0 + 18.0
            + 6.0
            + tr.commands.chars().count() as f32 * 7.0
            + 6.0
            + palette_keys.chars().count() as f32 * 6.5
            + 8.0
            + 10.0
    };
    let command_tooltip = if compact {
        ws.borrow()
            .keymap
            .hint(tr.command_palette, &crate::keymap::Action::Palette)
    } else {
        String::new()
    };
    let rail_hidden = ws.borrow().rail_hidden;
    let show_hint = ws
        .borrow()
        .keymap
        .hint(tr.show_sidebar, &crate::keymap::Action::ToggleSidebar);
    // The window's buttons, while the app draws the title bar.
    let controls = window_controls(ws);
    let controls_width = if ws.draws_title_bar() {
        WINDOW_BUTTON * 3.0
    } else {
        0.0
    };
    // What the strip leaves for tabs: the command button, the `+`, the
    // button that shows a hidden rail, the window's buttons and their
    // margins.
    let room = width
        - controls_width
        - command_width
        - 16.0
        - ADD_SIZE
        - 8.0
        - if rail_hidden {
            (30.0 + 8.0) * 2.0 + super::rail::BELL_WIDTH + 8.0
        } else {
            0.0
        };
    // The tabs shown, how many are hidden before and after them, and the
    // titles of all of them for the menus that list them.
    let (tab_views, hidden_before, hidden_after, tab_titles): (
        Vec<ThemeProvider>,
        usize,
        usize,
        Vec<String>,
    ) = ws.with_active(|live| {
        let Some(live) = live else {
            return (Vec::new(), 0, 0, Vec::new());
        };
        let base_title = |tab: &crate::workspace::LiveTab| -> Option<String> {
            let (_, panel) = live.title_panel(tab)?;
            Some(short_title(&panel.title()))
        };
        // Tabs with the same title are told apart by a number from the
        // second on, so the strip, the menu and a screen reader never
        // offer two alike. The number goes by the age of the tab's oldest
        // panel, not by place, so moving a tab does not renumber it. It
        // goes by the naming pane's title alone (a titled agent's, else the
        // focused pane's), so a tab keeps its number when a pane joins it.
        let bases: Vec<Option<String>> = live.tabs.iter().map(base_title).collect();
        let age = |tab: &crate::workspace::LiveTab| {
            (0..tab.layout.len())
                .filter_map(|pane| tab.layout.get(pane))
                .min()
                .unwrap_or(u64::MAX)
        };
        let names: Vec<Option<String>> = live
            .tabs
            .iter()
            .zip(&bases)
            .map(|(tab, base)| {
                let title = base.clone()?;
                let older = live
                    .tabs
                    .iter()
                    .zip(&bases)
                    .filter(|(other, other_base)| {
                        other_base.as_deref() == Some(title.as_str()) && age(other) < age(tab)
                    })
                    .count();
                let title = if older == 0 {
                    title
                } else {
                    format!("{title} ({})", older + 1)
                };
                let panes = tab.layout.len();
                Some(if panes > 1 {
                    (ws.tr().tab_with_panes)(&title, panes)
                } else {
                    title
                })
            })
            .collect();
        let title_of = |tab: &crate::workspace::LiveTab| -> Option<String> {
            let index = live.tabs.iter().position(|t| std::ptr::eq(t, tab))?;
            names[index].clone()
        };
        let lengths: Vec<usize> = live
            .tabs
            .iter()
            .map(|tab| title_of(tab).map_or(0, |title| title.chars().count()))
            .collect();
        let fit = match fit_tabs(room, live.tabs.len(), lengths.iter().copied()) {
            // The room kept for the shown title that it does not need goes
            // to more icon tabs, so the strip never ends in a gap while
            // tabs hide behind the button.
            Fit::Window { max, shown } => {
                let count = live.tabs.len();
                let active_len = lengths
                    .get(live.active)
                    .map_or(0, |&length| length + PLACE_CHARS)
                    .min(max);
                let slack = (max - active_len) as f32 * TAB_CHAR;
                let extra = (slack / icon_tab(count)) as usize;
                Fit::Window {
                    max,
                    shown: (shown + extra).min(count),
                }
            }
            fit => fit,
        };
        // A cut strip keeps the shown tab's title whole when the others can
        // give it the room and still read.
        let (fit, active_whole) = match fit {
            Fit::Cut(_) => match cut_around(room, &lengths, live.active) {
                Some(others) => (Fit::Cut(others), true),
                None => (fit, false),
            },
            _ => (fit, false),
        };
        // Cut titles that come out alike tell no tab apart: place numbers
        // do, so the strip turns to icons sooner.
        let (fit, active_whole) = match fit {
            Fit::Cut(max) if cut_alike(&names, live.active, max, active_whole) => {
                let longest = lengths.iter().copied().max().unwrap_or(0);
                (icon_fit(room, live.tabs.len(), longest), false)
            }
            _ => (fit, active_whole),
        };
        let count = live.tabs.len();
        // Icon tabs nearest the shown one get short titles back while the
        // strip has room for them, so a strip of icons never sits beside
        // empty space.
        let shown = match fit {
            Fit::Window { shown, .. } => shown,
            _ => count,
        };
        let start = match fit {
            Fit::Window { .. } => {
                let previous = ws.borrow().tab_window.get();
                let start = keep_window(previous, count, shown, live.active);
                ws.borrow().tab_window.set(Some(start));
                start
            }
            _ => {
                ws.borrow().tab_window.set(None);
                window_start(count, shown, live.active)
            }
        };
        let mut titled = vec![false; count];
        if let Fit::Icons(max) | Fit::Window { max, .. } = fit {
            let room = match fit {
                Fit::Window { .. } => room - MORE_WIDTH - 8.0,
                _ => room,
            };
            // The room of the widest title any tab in the window would
            // take when shown, so the titled set below does not depend on
            // which tab is shown.
            let active_len = (start..start + shown)
                .filter_map(|index| lengths.get(index))
                .map(|&length| (length + PLACE_CHARS).min(max))
                .max()
                .unwrap_or(0);
            let mut left = room
                - TAB_CHROME
                - active_len as f32 * TAB_CHAR
                - shown.saturating_sub(1) as f32 * icon_tab(count);
            // A title too short to tell the tab from its alike neighbours
            // is no better than the number, so it gets enough to read.
            let upgrade = TAB_CHROME + NEIGHBOR_TITLE_CHARS as f32 * TAB_CHAR - icon_tab(count);
            // In strip order, not outward from the shown tab: which tabs
            // keep a title then stays put when another tab is chosen, so
            // the tabs do not jump under the pointer.
            // The shown tab takes its slot in this order too, though its
            // own title is wider, so the set is the same whichever is shown.
            for index in start..start + shown {
                let cost = upgrade.min(
                    TAB_CHROME + (lengths[index] + PLACE_CHARS) as f32 * TAB_CHAR - icon_tab(count),
                );
                if left >= cost {
                    left -= cost;
                    titled[index] = true;
                }
            }
        }
        let views = live
            .tabs
            .iter()
            .enumerate()
            .skip(start)
            .take(shown)
            .filter_map(|(index, tab)| {
                let id = tab.focused_panel()?;
                let (_, panel) = live.title_panel(tab)?;
                let panes = tab.layout.len();
                let (icon_name, icon_svg) = kind_icon(panel);
                let title = title_of(tab)?;
                // A full strip shortens every title alike; the tab keeps
                // its whole title as its name.
                let active = index == live.active;
                let budget = match fit {
                    Fit::Whole => None,
                    Fit::Cut(_) if active && active_whole => None,
                    Fit::Cut(max) => Some(max),
                    Fit::Icons(max) | Fit::Window { max, .. } if active => Some(max),
                    Fit::Icons(_) | Fit::Window { .. } if titled[index] => {
                        Some(NEIGHBOR_TITLE_CHARS)
                    }
                    Fit::Icons(_) | Fit::Window { .. } => Some(0),
                };
                let icon_only = budget == Some(0);
                let icons = matches!(fit, Fit::Icons(_) | Fit::Window { .. });
                // An icon-only tab shows its number, so it still reads
                // apart from its neighbors without hovering it.
                let shown = if icons {
                    // Beside icon tabs numbered by place, every tab starts
                    // with its place, so titles cut alike still read apart.
                    // A number that tells alike titles apart would read as
                    // a place too, so the title leaves it out. The tooltip
                    // and name keep the pane count an icon tab drops.
                    let place = index + 1;
                    Some(match budget {
                        Some(0) => place.to_string(),
                        Some(max) => {
                            let digits = place.to_string().len() + 2;
                            let plain = strip_number(&title);
                            format!(
                                "{place}. {}",
                                truncate_keeping_suffix(
                                    &plain,
                                    max.saturating_sub(digits).max(MIN_TITLE_CHARS)
                                )
                            )
                        }
                        None => format!("{place}. {}", strip_number(&title)),
                    })
                } else {
                    budget
                        .filter(|&max| title.chars().count() > max)
                        .map(|max| truncate_keeping_suffix(&title, max))
                };
                // The most urgent wait among the tab's panes.
                let attention = (0..panes)
                    .filter_map(|pane| live.panel(tab.layout.get(pane)?))
                    .map(|(_, panel)| match panel {
                        LivePanel::Terminal(terminal) => terminal.session.attention(),
                        _ => Attention::None,
                    })
                    .max()
                    .unwrap_or_default();
                let (theme, indicator) = if attention.wants_user() {
                    let color = scheme.attention_color(attention);
                    let mut theme = scheme.theme();
                    theme.text = color;
                    theme.text_muted = color;
                    (theme, color)
                } else if index == live.active {
                    (scheme.theme(), lane)
                } else {
                    // Tabs not shown are in the muted color, as the
                    // rail's tabs are, so the underline is not the only
                    // thing that marks the shown one.
                    (scheme.muted_theme(), lane)
                };
                let select = ws.clone();
                let close = ws.clone();
                let drag = ws.clone();
                let drag_end = ws.clone();
                // An icon before the title says what the naming pane is.
                let icon_color = if index == live.active {
                    indicator
                } else {
                    scheme.muted
                };
                Some(ThemeProvider::new(
                    scheme.tab_theme(theme),
                    // The tab leaves room before its title, and the icon
                    // sits in it under the tab, so hover, selection and the
                    // underline take in the icon too.
                    // A row of no width holds it, so the tab starts where
                    // the icon does.
                    Row::new()
                        .align(Align::Center)
                        .child(
                            Row::new().width(0.0).height(ICON_SIZE).child(
                                Pad::insets(EdgeInsets {
                                    left: ICON_LEFT,
                                    ..EdgeInsets::default()
                                })
                                .child(
                                    MdIcon::outlined(icon_name, icon_svg)
                                        .size(ICON_SIZE)
                                        .color(icon_color),
                                ),
                            ),
                        )
                        .child({
                            // Beside icon tabs numbered by place, the name
                            // and tooltip say that place, not the title's
                            // own number, so sighted users and screen
                            // readers get the same number.
                            let (title, shown) = if icons {
                                let plain = strip_number(&title);
                                (format!("{}. {plain}", index + 1), shown)
                            } else {
                                (title, shown)
                            };
                            let tab = Tab::new(title.clone());
                            // A cut title, or an icon alone, names the tab
                            // in full under the pointer. Not the shown tab:
                            // a tooltip lifts its host over its siblings, and
                            // the shown tab's fill would hide its icon and
                            // close button.
                            let tab = match shown {
                                Some(shown) if !active => tab.display(shown).tooltip(title),
                                Some(shown) => tab.display(shown),
                                None => tab,
                            };
                            let tab = tab
                                .appearance(TabAppearance::Underline)
                                .text_size(13.0)
                                .padding_h(8.0)
                                .lead(ICON_LEFT + ICON_SIZE + 6.0 - 8.0)
                                .min_height(TAB_HEIGHT - 1.0)
                                .indicator(indicator, UNDERLINE)
                                // A fill as well as the line, so a
                                // hovered tab never reads as the shown one.
                                // The fill leans toward the tab's lane
                                // tint, like a chosen row, so it stays
                                // the stronger of the two.
                                .selected_fill(super::theme::mix(
                                    scheme.raised,
                                    scheme.tinted(indicator),
                                    ACTIVE_FILL_SHARE,
                                ))
                                .selected(index == live.active)
                                .on_select(move || select.select_tab(index))
                                .on_drag_start(move || drag.start_tab_drag(id))
                                .on_drag_end(move |_| drag_end.end_tab_drag())
                                .context_menu(tab_menu(ws, tr, index, count, panes));
                            // A pane dragged by its title onto another tab
                            // joins it. The shown tab already holds it.
                            let tab = if index == live.active {
                                tab
                            } else {
                                let drop = ws.clone();
                                tab.on_drop(move || drop.drop_pane_on_tab(index))
                            };
                            // An icon-only tab has no room for its close
                            // button; it closes once shown.
                            let tab = if icon_only {
                                tab
                            } else {
                                tab.on_close(move || close.close_tab(index))
                                    .close_in_tab_order(active)
                            };
                            // The shown tab takes focus for `focus_tabs`.
                            // Another tab names the drop a pane makes on it.
                            let tab: Box<dyn View> = if active {
                                Box::new(tab.with_ref(active_ref))
                            } else {
                                Box::new(DropTip::new(tr.drop_into_tab).child(tab))
                            };
                            tab
                        }),
                ))
            })
            .collect();
        let end = start + shown.min(count - start);
        // Every tab, numbered as in the strip, as a numbered list, so the
        // place does not read as part of the title.
        let titles = live
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                format!(
                    "{}. {}",
                    index + 1,
                    strip_number(&title_of(tab).unwrap_or_default())
                )
            })
            .collect();
        (views, start, count - end, titles)
    });
    let reorder = ws.clone();
    let bar = tab_views.into_iter().fold(
        // A gap between tabs keeps each close button nearer its own label
        // than the next tab's.
        TabBar::new().gap(8.0).on_reorder(move |from, to| {
            // The bar counts from the first tab it shows.
            reorder.reorder_tab(from + hidden_before, to + hidden_before)
        }),
        TabBar::child,
    );
    // One button after the shown tabs counts the hidden ones and opens a
    // menu of them, so any tab is two clicks away and the menu holds as
    // many as the button says.
    let more = |hidden: usize| {
        let text = format!("{} ▾", (tr.tabs_more)(hidden));
        // In the tabs' text size, with rows as tall as the palette's.
        let mut menu = ContextMenu::new().text_size(13.0).row_height(28.0);
        let shown = hidden_before..tab_titles.len() - hidden_after;
        for (index, title) in tab_titles.iter().enumerate() {
            if shown.contains(&index) {
                continue;
            }
            let select = ws.clone();
            menu = menu.item(
                ContextMenuItem::new(title.clone()).on_select(move || select.select_tab(index)),
            );
        }
        // In the full theme, so the menu it opens lists its tabs in ink.
        Button::new((tr.tabs_hidden)(hidden))
            .display(text)
            .appearance(ButtonAppearance::Outline)
            .height(ADD_SIZE - 2.0)
            .text_size(12.5)
            .tooltip((tr.tabs_hidden)(hidden))
            .menu(menu)
    };
    let mut strip = Row::new().align(Align::Center).gap(8.0);
    // With the rail hidden, the strip starts with the way back to it.
    if rail_hidden {
        let show = ws.clone();
        strip = strip.child(super::rail::icon_button(
            MdIcon::outlined("left_panel_open", outlined::ICON_LEFT_PANEL_OPEN).color(scheme.muted),
            Button::new(tr.show_sidebar)
                .tooltip(show_hint)
                .on_click(move || show.toggle_sidebar()),
        ));
        // The gear and the bell live in the rail's footer; with the rail
        // hidden they move here, so the settings stay a click away and
        // unread notices still show.
        let settings = ws.clone();
        let settings_hint = ws
            .borrow()
            .keymap
            .hint(tr.settings, &crate::keymap::Action::Settings);
        strip = strip.child(super::rail::icon_button(
            MdIcon::outlined("settings", outlined::ICON_SETTINGS).color(scheme.muted),
            Button::new(tr.settings)
                .tooltip(settings_hint)
                .on_click(move || settings.open_settings()),
        ));
        strip = strip.child(super::rail::bell(ws));
    }
    let mut strip = strip.child(bar);
    let hidden = hidden_before + hidden_after;
    if hidden > 0 {
        strip = strip.child(more(hidden));
    }
    let palette = ws.clone();
    let add = ws.clone();
    let new_panel_ref = ws.regions().new_panel;
    let has_worktree = ws.borrow().active_key().is_some();
    Column::new()
        .width(width)
        .child(
            Stack::new()
                .size(width, TAB_HEIGHT - 1.0)
                // On a light scheme the page and the rail are close; the
                // rail's shade sets the strip apart from the terminal under
                // it.
                .child(Fill::new(
                    width,
                    TAB_HEIGHT - 1.0,
                    if scheme.dark {
                        scheme.page
                    } else {
                        scheme.rail
                    },
                ))
                .child(
                    Row::new()
                        .align(Align::Center)
                        .child(
                            Row::new()
                                .width(width - controls_width)
                                .justify(Justify::SpaceBetween)
                                .align(Align::Center)
                                .child(
                                    strip.child(
                                        Pad::insets(EdgeInsets::symmetric(4.0, 0.0)).child(
                                            DropTip::new(tr.drop_new_tab).child(
                                                Button::new(tr.new_panel)
                                                    .display("+")
                                                    .appearance(ButtonAppearance::Ghost)
                                                    .width(ADD_SIZE)
                                                    .height(ADD_SIZE)
                                                    .text_size(16.0)
                                                    .disabled(!has_worktree)
                                                    .tooltip(tr.new_panel_tip)
                                                    .on_click(move || add.open_new_panel_from_add())
                                                    // A pane dragged by its title onto
                                                    // it becomes a tab of its own.
                                                    .on_drop({
                                                        let drop = ws.clone();
                                                        move || drop.drop_pane_on_new_tab()
                                                    })
                                                    .accepts_drop({
                                                        let ws = ws.clone();
                                                        move || ws.dragging_pane()
                                                    })
                                                    .with_ref(new_panel_ref),
                                            ),
                                        ),
                                    ),
                                )
                                .child(
                                    Pad::insets(EdgeInsets {
                                        top: 0.0,
                                        right: 8.0,
                                        bottom: 0.0,
                                        left: 8.0,
                                    })
                                    .child(
                                        Stack::new()
                                            .size(command_width, COMMAND_SIZE)
                                            // A framed field at rest, like a search
                                            // box, so it reads as a control before
                                            // the pointer finds it.
                                            .child(
                                                schnellui::widgets::Panel::new()
                                                    .width(command_width)
                                                    .height(COMMAND_SIZE)
                                                    .surface(scheme.raised)
                                                    .outline(scheme.line)
                                                    .radius(6.0),
                                            )
                                            .child(
                                                // A magnifier: ⌘ means a Mac key, and the
                                                // palette is where commands are searched.
                                                // Its keys sit beside it, so the button
                                                // also teaches the shortcut.
                                                Pad::insets(EdgeInsets {
                                                    left: if compact {
                                                        (COMMAND_SIZE - 18.0) / 2.0
                                                    } else {
                                                        6.0
                                                    },
                                                    top: (COMMAND_SIZE - 18.0) / 2.0,
                                                    ..EdgeInsets::default()
                                                })
                                                .child({
                                                    let row = Row::new()
                                                        .gap(6.0)
                                                        .align(Align::Center)
                                                        .child(
                                                            MdIcon::outlined(
                                                                "search",
                                                                outlined::ICON_SEARCH,
                                                            )
                                                            .size(18.0)
                                                            .color(scheme.muted),
                                                        );
                                                    if compact {
                                                        row
                                                    } else {
                                                        row.child(Text::new(tr.commands).size(12.5))
                                                            // On a keycap pill, as the
                                                            // palette's rows show keys.
                                                            .child({
                                                                let chip =
                                                                    palette_keys.chars().count()
                                                                        as f32
                                                                        * 6.5
                                                                        + 8.0;
                                                                Stack::new()
                                                            .size(chip, 18.0)
                                                            .align(Align::Center)
                                                            .justify(Justify::Center)
                                                            .child(
                                                                schnellui::widgets::Panel::new()
                                                                    .width(chip)
                                                                    .height(18.0)
                                                                    .surface(scheme.keycap())
                                                                    .no_outline()
                                                                    .radius(9.0),
                                                            )
                                                            // Ink, as on the
                                                            // palette's pills:
                                                            // muted text on the
                                                            // line color fades
                                                            // in light schemes.
                                                            .child(ThemeProvider::new(
                                                                scheme.theme(),
                                                                Text::new(palette_keys).size(12.0),
                                                            ))
                                                            })
                                                    }
                                                }),
                                            )
                                            // On top, so it takes the pointer; the icon only paints.
                                            .child(
                                                Button::new(tr.command_palette)
                                                    .icon_only()
                                                    .appearance(ButtonAppearance::Ghost)
                                                    .width(command_width)
                                                    .height(COMMAND_SIZE)
                                                    // A tooltip only when compact: the
                                                    // wide button already shows its
                                                    // name and keys.
                                                    .tooltip(command_tooltip)
                                                    .on_click(move || palette.open_palette(None)),
                                            ),
                                    ),
                                ),
                        )
                        .child(controls),
                ),
        )
        .child(Fill::new(width, 1.0, scheme.line))
}

/// Minimize, maximize or restore, and close, at the strip's right end while
/// the app draws the window's title bar (`window_frame` is `app`). Empty
/// while the system draws it.
fn window_controls(ws: &Workspace) -> Row {
    use schnellui::WindowRequest;
    let row = Row::new().align(Align::Center);
    if !ws.draws_title_bar() {
        return row;
    }
    let scheme = ws.borrow().scheme;
    let tr = ws.tr();
    let (resize_name, resize_icon, resize_svg) = if ws.window_maximized() {
        (tr.restore_window, "filter_none", outlined::ICON_FILTER_NONE)
    } else {
        (
            tr.maximize_window,
            "crop_square",
            outlined::ICON_CROP_SQUARE,
        )
    };
    [
        (
            tr.minimize_window,
            "remove",
            outlined::ICON_REMOVE,
            WindowRequest::Minimize,
        ),
        (
            resize_name,
            resize_icon,
            resize_svg,
            WindowRequest::ToggleMaximize,
        ),
        (
            tr.close_window,
            "close",
            outlined::ICON_CLOSE,
            WindowRequest::Close,
        ),
    ]
    .into_iter()
    .fold(row, |row, (name, icon, svg, request)| {
        let window = ws.clone();
        row.child(
            Stack::new()
                .size(WINDOW_BUTTON, TAB_HEIGHT - 1.0)
                .align(Align::Center)
                .justify(Justify::Center)
                .child(MdIcon::outlined(icon, svg).size(18.0).color(scheme.muted))
                // On top, so it takes the pointer; the icon only paints.
                .child(
                    Button::new(name)
                        .icon_only()
                        .appearance(ButtonAppearance::Ghost)
                        .width(WINDOW_BUTTON)
                        .height(TAB_HEIGHT - 1.0)
                        .on_click(move || window.request_window(request)),
                ),
        )
    })
}

/// The menu a tab opens under the pointer: what the palette does to the
/// shown tab, for this one.
fn tab_menu(
    ws: &Workspace,
    tr: &'static crate::i18n::Strings,
    index: usize,
    count: usize,
    panes: usize,
) -> ContextMenu {
    use crate::state::SplitAxis;
    let mut menu = ContextMenu::new().text_size(13.0).row_height(28.0);
    for (title, axis) in [
        (tr.split_side_by_side, SplitAxis::SideBySide),
        (tr.split_stacked, SplitAxis::Stacked),
    ] {
        let split = ws.clone();
        menu = menu.item(ContextMenuItem::new(title).on_select(move || {
            split.select_tab(index);
            split.split_pane(axis);
        }));
    }
    if panes > 1 {
        let out = ws.clone();
        menu = menu.item(
            ContextMenuItem::new(tr.move_pane_to_tab).on_select(move || {
                out.select_tab(index);
                out.move_pane_to_tab();
            }),
        );
    }
    if count > 1 {
        let others = ws.clone();
        menu = menu.item(
            ContextMenuItem::new(tr.close_other_tabs)
                .on_select(move || others.close_other_tabs(index)),
        );
    }
    let close = ws.clone();
    menu.item(ContextMenuItem::new(tr.close_tab).on_select(move || close.close_tab(index)))
}

#[cfg(test)]
mod short_title_tests {
    use super::{fit_tabs, short_title};

    #[test]
    fn the_number_of_alike_titles_comes_off_with_the_pane_count_kept() {
        assert_eq!(super::strip_number("fish (2) · 3 panes"), "fish · 3 panes");
        assert_eq!(super::strip_number("fish (12)"), "fish");
        assert_eq!(super::strip_number("fish (dev)"), "fish (dev)");
        assert_eq!(super::strip_number("fish"), "fish");
    }

    #[test]
    fn titles_shorten_alike_only_when_the_strip_is_full() {
        use super::Fit;
        assert_eq!(fit_tabs(1000.0, 3, [10, 12, 8].into_iter()), Fit::Whole);
        let Fit::Cut(budget) = fit_tabs(800.0, 6, [14; 6].into_iter()) else {
            panic!("six long tabs in 800 px should cut their titles");
        };
        assert!((super::MIN_TITLE_CHARS..14).contains(&budget), "{budget}");
        // Short titles stay whole and give their room to the long ones.
        let Fit::Cut(budget) = fit_tabs(780.0, 6, [4, 4, 4, 4, 4, 40].into_iter()) else {
            panic!("one long title among short ones should be cut");
        };
        assert!(budget > 20, "{budget}");
        assert!(matches!(
            fit_tabs(790.0, 12, [9; 12].into_iter()),
            Fit::Icons(max) if max >= super::MIN_TITLE_CHARS
        ));
        // Too many for icons: a window of icons around the active tab's title.
        let Fit::Window { shown, max } = fit_tabs(600.0, 30, [9; 30].into_iter()) else {
            panic!("thirty tabs in 600 px should show a window of them");
        };
        assert!(shown > 1 && shown < 30, "{shown}");
        // The icons and the active title take the room, and no more.
        let width = |shown: usize, max: usize| {
            (shown - 1) as f32 * super::icon_tab(30)
                + super::TAB_CHROME
                + max as f32 * super::TAB_CHAR
        };
        let room = 600.0 - super::MORE_WIDTH - 8.0;
        assert!(max >= super::MIN_TITLE_CHARS, "{max}");
        assert!(width(shown, max) <= room, "{shown} {max}");
        assert!(
            width(shown + 1, super::NEIGHBOR_TITLE_CHARS) > room,
            "{shown}"
        );
        assert_eq!(super::window_start(30, 9, 0), 0);
        assert_eq!(super::window_start(30, 9, 15), 11);
        assert_eq!(super::window_start(30, 9, 29), 21);
        assert_eq!(super::window_start(5, 5, 3), 0);
        // A tab chosen inside the window keeps it; one outside moves it.
        assert_eq!(super::keep_window(Some(4), 30, 6, 7), 4);
        assert_eq!(super::keep_window(Some(4), 30, 6, 10), 8);
        assert_eq!(super::keep_window(None, 30, 6, 10), 8);
    }

    #[test]
    fn a_cut_title_keeps_its_name_before_the_count_words() {
        use super::truncate_keeping_suffix as cut;
        assert_eq!(cut("cmdterm - fish · 2 panes", 18), "cmdterm… · 2 panes");
        assert_eq!(cut("cmdterm - fish · 2 panes", 14), "cmdterm… · 2");
        // A cut after a dash leaves the dash out.
        assert_eq!(cut("cmdterm - fish · 2 panes", 11), "cmdter… · 2");
    }

    #[test]
    fn a_cut_title_keeps_its_number() {
        use super::truncate_keeping_suffix as cut;
        assert_eq!(cut("schnellui - fish (2)", 11), "schnel… (2)");
        // The number goes before the name drops under six characters.
        assert_eq!(cut("schnellui - fish (2)", 6), "schne…");
        assert_eq!(cut("fish · 2 panes (2)", 8), "fish…");
        assert_eq!(cut("fish · 2 panes", 14), "fish · 2 panes");
        assert_eq!(cut("fish · 2 panes", 12), "fish · 2");
        assert_eq!(cut("fish", 3), "fi…");
        // Too short for the number, the pane count stays.
        assert_eq!(cut("cmdterm - fish (2) · 3 panes", 11), "cmdter… · 3");
        // The number outlasts the count's words.
        assert_eq!(cut("cmdterm - fish (9) · 3 panes", 15), "cmdter… (9) · 3");
        // Too short for a name beside the count, the name stays.
        assert_eq!(cut("cmdterm - fish · 3 panes", 7), "cmdter…");
    }

    #[test]
    fn a_leading_path_keeps_its_last_folder() {
        assert_eq!(
            short_title("~/Documents/prog/schnellui - fish"),
            "schnellui - fish"
        );
        assert_eq!(short_title("/srv/app"), "app");
        assert_eq!(short_title("/"), "/");
        assert_eq!(short_title("History"), "History");
        assert_eq!(short_title("vim ~/notes.md"), "vim ~/notes.md");
    }
}
