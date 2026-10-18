//! The explore panel's change request page, built from native widgets: a
//! bar with the panel's modes and the page's actions, which the grid modes
//! show above their grid too so the panel keeps its look, the list of the
//! page's entries on the left, and the chosen entry on the right as cards.
//! A narrow panel drops the list and shows every entry one after the other.
//!
//! The page is cold: it is built when the panel turns to it, when the forge
//! answers, and when another entry is picked, never per frame. Its content
//! scroll view carries the panel's accessible name, so the explore panel's
//! key handler gets its keys: `v` switches the mode, `r` reloads, and the
//! arrow keys, `j`, `k`, `n` and `p` pick entries.

use std::path::Path;
use std::rc::Rc;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use schnellui::layout::EdgeInsets;
use schnellui::scene::{Color, Size};
use schnellui::widgets::{
    Align, Badge, Button, ButtonAppearance, Column, ComponentInteractions, ContextMenu,
    ContextMenuItem, InteractionStates, InteractionStyle, Justify, ListMarker, ListRow, Pad, Panel,
    Referenced, RichBlock, RichDoc, RichSpan, RichText, Row, Scroll, SpanStyle, Tab, TabAppearance,
    TabBar, Text, Theme, ThemeProvider, View, WrapMode,
};

use super::Fill;
use super::theme::{ReaderColors, Scheme};
use crate::discussion::{PlacedThread, PushEntry, Rendered, SectionKind, check_mark, short_time};
use crate::explore::{ChangePage, ExplorePanel, Mode};
use crate::ext::{BranchState, CheckState, Comment, Verdict};
use crate::files::highlight::{self, State};
use crate::forges::open_in_system_browser;
use crate::i18n::Strings;
use crate::workspace::Workspace;

/// Height of the bar above every mode of the explore panel, its bottom
/// rule included.
pub const BAR: f32 = 37.0;
/// Room between the bar's rule and a grid mode's first row.
const GRID_GAP: f32 = 4.0;
/// Everything above a grid mode's first row: the bar, its rule and the gap.
pub const GRID_TOP: f32 = BAR + 1.0 + GRID_GAP;
/// Panels narrower than this drop the list.
const NARROW: f32 = 640.0;
/// Space around the page's cards.
const GUTTER: f32 = 20.0;
/// The widest the cards get, so lines stay readable.
const MAX_CARDS: f32 = 920.0;
/// Room for the scroll bar on the right of a scroll view's rows.
const SCROLLBAR: f32 = 12.0;
const CARD_PAD: f32 = 14.0;
const BODY_SIZE: f32 = 13.5;
const META_SIZE: f32 = 12.0;

/// The change request page of `explore` filling `size`. Its content scroll
/// view is named `name`; `focus` puts the app's focus reference on it.
pub fn page(
    ws: &Workspace,
    explore: &Rc<ExplorePanel>,
    name: &'static str,
    size: Size,
    focus: bool,
) -> Box<dyn View> {
    let (scheme, tr) = {
        let inner = ws.borrow();
        (inner.scheme, inner.tr)
    };
    let shown = explore.change_page();
    let rendered = match &shown {
        ChangePage::Ready(rendered) => Some(rendered.as_ref()),
        ChangePage::Message(_) => None,
    };
    let body_height = (size.height - BAR).max(1.0);
    let wide = size.width >= NARROW && rendered.is_some_and(|r| r.sections.len() > 1);
    let list_width = if wide {
        (size.width * 0.26).clamp(200.0, 300.0)
    } else {
        0.0
    };
    let content_width = (size.width - list_width - if wide { 1.0 } else { 0.0 }).max(1.0);
    let cards_width = (content_width - GUTTER * 2.0 - SCROLLBAR).clamp(1.0, MAX_CARDS);

    let mut cards = Column::new().width(cards_width).gap(14.0);
    // Each entry keeps its own scroll position across rebuilds.
    let key = match &shown {
        ChangePage::Message(message) => {
            cards = cards.child(muted(scheme, message.clone(), BODY_SIZE));
            // The way on from a forge no extension serves: the extensions.
            if explore.change_unsupported() {
                let open = ws.clone();
                // A narrow pane shows the page's name, which fits, and the
                // button keeps its full name for screen readers.
                let full = super::text_button_width(tr.change_open_extensions, 12.5);
                let shown = if full <= cards_width {
                    tr.change_open_extensions
                } else {
                    tr.section_extensions
                };
                cards = cards.child(
                    Row::new().child(
                        Button::new(tr.change_open_extensions)
                            .display(shown)
                            .width(super::text_button_width(shown, 12.5).min(cards_width))
                            .appearance(ButtonAppearance::Outline)
                            .text_size(12.5)
                            .height(BAR - 12.0)
                            .on_click(move || {
                                open.show_settings_section(crate::ui::SettingsSection::Extensions);
                                open.open_settings();
                            }),
                    ),
                );
            }
            "message".to_owned()
        }
        ChangePage::Ready(rendered) if wide => {
            let section = explore.section().min(rendered.sections.len() - 1);
            cards = section_cards(cards, scheme, tr, rendered, section, cards_width);
            format!("{}:{section}", rendered.number)
        }
        ChangePage::Ready(rendered) => {
            for section in 0..rendered.sections.len() {
                cards = section_cards(cards, scheme, tr, rendered, section, cards_width);
            }
            format!("{}:all", rendered.number)
        }
    };
    let content = Scroll::new()
        .label(name)
        .restoration_key(format!("{name}:{key}"))
        .scrollbar(true)
        .size(content_width, body_height)
        .child(
            Pad::insets(EdgeInsets {
                left: GUTTER,
                right: GUTTER + SCROLLBAR,
                top: GUTTER,
                bottom: GUTTER,
            })
            .child(cards),
        );
    let content: Box<dyn View> = if focus {
        Box::new(Referenced::new(ws.regions().focus).child(content))
    } else {
        Box::new(content)
    };
    // The pane's frame shows its focus, as it does for a terminal. SchnellUI
    // draws a scroll view's focus ring around its last painted descendant
    // instead of its viewport, so the ring is turned invisible here.
    let content = ThemeProvider::new(
        Theme {
            component_interactions: ComponentInteractions {
                raw_surface: Some(InteractionStates {
                    hover: InteractionStyle::NONE,
                    focus: InteractionStyle::border(Color::TRANSPARENT),
                    active: InteractionStyle::NONE,
                }),
                ..ComponentInteractions::NONE
            },
            ..scheme.theme()
        },
        content,
    );

    let mut body = Row::new().size(size.width, body_height);
    if wide && let Some(rendered) = rendered {
        body = body
            .child(section_list(
                explore,
                scheme,
                tr,
                rendered,
                list_width,
                body_height,
            ))
            .child(Fill::new(1.0, body_height, scheme.line));
    }
    let body = body.child(content);

    Box::new(
        Column::new()
            .size(size.width, size.height)
            .child(bar(ws, explore, scheme, tr, rendered, size.width))
            .child(Fill::new(size.width, 1.0, scheme.line))
            .child(body),
    )
}

/// A grid mode of `explore` filling `size`: the bar the change request
/// page has, then `grid`, which the workspace sizes to the rows below it.
pub fn grid_page(
    ws: &Workspace,
    explore: &Rc<ExplorePanel>,
    grid: Box<dyn View>,
    size: Size,
) -> Box<dyn View> {
    let scheme = ws.borrow().scheme;
    let tr = ws.tr();
    Box::new(
        Column::new()
            .size(size.width, size.height)
            .child(bar(ws, explore, scheme, tr, None, size.width))
            .child(Fill::new(size.width, 1.0, scheme.line))
            // A little room under the rule, so the first row does not touch
            // it; the workspace leaves it out of the grid's rows.
            .child(Fill::new(size.width, GRID_GAP, scheme.page))
            .child(grid),
    )
}

/// Below this width the explore bar's switch tightens and Reload shows
/// only its symbol.
const NARROW_BAR: f32 = 380.0;
/// Below this width the explore bar's modes collapse into a menu button
/// that names the shown one.
const MENU_BAR: f32 = 300.0;
/// Below this width the explore bar has no room for Reload, even beside
/// the bare mode menu.
const NO_RELOAD_BAR: f32 = 90.0;
/// Room the bar takes besides the mode menu's label: its margins, the gap
/// and the Reload symbol.
const MENU_BAR_CHROME: f32 = 8.0 + 8.0 + 24.0 + 12.0;

/// Height of a mode segment in the bar.
const SEGMENT_HEIGHT: f32 = 26.0;

/// The bar above every mode of the explore panel: the modes on the left,
/// reload and, on the change request page, the forge's page on the right.
fn bar(
    ws: &Workspace,
    explore: &Rc<ExplorePanel>,
    scheme: &'static Scheme,
    tr: &'static Strings,
    rendered: Option<&Rendered>,
    width: f32,
) -> impl View + use<> {
    // A narrow pane tightens the switch and turns Reload into its symbol;
    // a narrower one shows the modes as a menu, and the narrowest leaves
    // Reload to its keys (r, F5), so the bar never runs past the pane.
    let narrow = width < NARROW_BAR;
    let as_menu = width < MENU_BAR;
    let reload_fits = width >= NO_RELOAD_BAR;
    let mut tabs = TabBar::new().gap(0.0);
    for (mode, title) in [
        (Mode::History, tr.explore_history),
        (Mode::Review, tr.explore_review),
        (Mode::Change, tr.explore_change),
    ] {
        let (pick, panel) = (ws.clone(), explore.clone());
        tabs = tabs.child(
            // Segments in a frame rather than underlined tabs, so the modes
            // read as a switch inside the panel and not as a second row of
            // the window's tabs, whose titles they share.
            Tab::new(title)
                .appearance(TabAppearance::Underline)
                .text_size(12.5)
                .padding_h(if narrow { 6.0 } else { 12.0 })
                .min_height(SEGMENT_HEIGHT)
                .indicator(scheme.lanes[0], 0.0)
                .selected_fill(scheme.selected())
                .selected(mode == explore.mode())
                .on_select(move || {
                    panel.switch_mode(mode);
                    if panel.take_changed() {
                        pick.save();
                    }
                }),
        );
    }
    let mut actions = Row::new().gap(4.0).align(Align::Center);
    // The forge's page is a link on the change request page too, so a
    // narrow bar leaves this button out.
    if let Some(url) = rendered
        .and_then(|r| r.discussion.url.clone())
        .filter(|_| !narrow)
    {
        actions = actions.child(
            Button::new(tr.change_open_on_forge)
                .appearance(ButtonAppearance::Outline)
                .text_size(12.5)
                .height(BAR - 12.0)
                .tooltip(url.clone())
                .on_click(move || open_in_system_browser(&url)),
        );
    }
    // Nothing to reload while no extension serves the forge; the page
    // offers the extensions instead.
    if reload_fits && !(explore.mode() == Mode::Change && explore.change_unsupported()) {
        let reload = explore.clone();
        // An outlined button, so it reads as an action next to the mode
        // tabs without the weight of a filled one.
        let button = Button::new(tr.change_reload)
            .appearance(ButtonAppearance::Outline)
            .text_size(12.5)
            .height(BAR - 12.0)
            .on_click(move || reload.reload());
        actions = actions.child(if narrow {
            button
                .display("↻")
                .width(BAR - 12.0)
                .tooltip(format!("{} (r)", tr.change_reload))
        } else {
            button.tooltip("r")
        });
    }
    Row::new()
        .size(width, BAR - 1.0)
        .justify(Justify::SpaceBetween)
        // In a narrow pane the modes and the actions still keep apart.
        .gap(8.0)
        .align(Align::Center)
        .child(
            Pad::insets(EdgeInsets {
                left: 8.0,
                ..EdgeInsets::default()
            })
            .child(if as_menu {
                Box::new(mode_menu(ws, explore, tr, width)) as Box<dyn View>
            } else {
                Box::new(
                    schnellui::widgets::Panel::new()
                        .surface(scheme.page)
                        .outline(scheme.line)
                        .radius(6.0)
                        .padding(2.0)
                        .child(tabs),
                )
            }),
        )
        // The same right margin as the tasks and monitor bars.
        .child(
            Pad::insets(EdgeInsets {
                right: 12.0,
                ..EdgeInsets::default()
            })
            .child(actions),
        )
}

/// The explore modes as one button that names the shown mode and opens a
/// menu of all three, for a pane too narrow for the switch. In a bar
/// `width` wide with no room for its label beside Reload, the button shows
/// only the arrow and names the mode in its tooltip.
fn mode_menu(
    ws: &Workspace,
    explore: &Rc<ExplorePanel>,
    tr: &'static Strings,
    width: f32,
) -> Button {
    let modes = [
        (Mode::History, tr.explore_history),
        (Mode::Review, tr.explore_review),
        (Mode::Change, tr.explore_change),
    ];
    let shown = modes
        .iter()
        .find(|(mode, _)| *mode == explore.mode())
        .map_or(tr.explore_history, |(_, title)| *title);
    let mut menu = ContextMenu::new().text_size(13.0).row_height(28.0);
    for (mode, title) in modes {
        let (pick, panel) = (ws.clone(), explore.clone());
        menu = menu.item(
            ContextMenuItem::new(title)
                .selected(mode == explore.mode())
                .on_select(move || {
                    panel.switch_mode(mode);
                    if panel.take_changed() {
                        pick.save();
                    }
                    pick.focus_explore_pane(&panel);
                }),
        );
    }
    // A bar too narrow for Reload keeps it here, so the pointer still
    // reaches it.
    if width < NO_RELOAD_BAR && !(explore.mode() == Mode::Change && explore.change_unsupported()) {
        let reload = explore.clone();
        menu = menu.item(
            ContextMenuItem::new(format!("{} (r)", tr.change_reload))
                .on_select(move || reload.reload()),
        );
    }
    // The label goes before Reload does: the arrow alone still opens the
    // modes, and the tooltip names the shown one.
    let label = format!("{shown} ▾");
    let bare = width < super::text_button_width(&label, 12.5) + MENU_BAR_CHROME;
    let button = Button::new(shown)
        .appearance(ButtonAppearance::Outline)
        .text_size(12.5)
        .height(SEGMENT_HEIGHT)
        .menu(menu);
    if bare {
        button
            .display("▾")
            .width(SEGMENT_HEIGHT + 4.0)
            .tooltip(shown)
    } else {
        button.display(label)
    }
}

/// The page's entries, the shown one selected.
fn section_list(
    explore: &Rc<ExplorePanel>,
    scheme: &'static Scheme,
    tr: &'static Strings,
    rendered: &Rendered,
    width: f32,
    height: f32,
) -> impl View + use<> {
    let shown = explore.section();
    let row_width = width - SCROLLBAR;
    let mut list = Column::new().width(row_width).gap(2.0);
    for (index, section) in rendered.sections.iter().enumerate() {
        let (mark, color) = section_mark(scheme, section.kind);
        let pick = explore.clone();
        let mut row = ListRow::new(section.label.clone())
            .width(row_width)
            .height(30.0)
            .padding(12.0)
            .title_size(13.0)
            .dot(color)
            .selected(index == shown)
            .on_click(move || pick.select_section(index));
        if let Some(mark) = mark {
            row = row.trailing(mark);
        }
        if let (SectionKind::Thread { .. }, Some(thread)) = (section.kind, rendered.thread(index))
            && thread.comments.len() > 1
        {
            row = row.trailing(thread.comments.len().to_string());
        }
        list = list.child(row);
    }
    Scroll::new()
        .label(tr.change_sections)
        .scrollbar(true)
        .size(width, height)
        .child(Pad::insets(EdgeInsets::symmetric(0.0, 8.0)).child(list))
}

/// The dot color of an entry, and a short mark after its label.
fn section_mark(scheme: &Scheme, kind: SectionKind) -> (Option<String>, Color) {
    match kind {
        SectionKind::Overview | SectionKind::Pushes | SectionKind::Conversation => {
            (None, scheme.muted)
        }
        SectionKind::Checks(state) => (
            Some(check_mark(state).to_string()),
            check_color(scheme, state),
        ),
        SectionKind::Thread { resolved: false } => (None, scheme.lanes[1]),
        SectionKind::Thread { resolved: true } => (Some("✓".to_owned()), scheme.line),
    }
}

/// Adds the cards of entry `index` to `cards`.
fn section_cards(
    cards: Column,
    scheme: &'static Scheme,
    tr: &'static Strings,
    rendered: &Rendered,
    index: usize,
    width: f32,
) -> Column {
    let colors = scheme.reader_colors();
    match rendered.sections[index].kind {
        SectionKind::Overview => overview(cards, scheme, tr, rendered, width, &colors),
        SectionKind::Checks(_) => cards.child(checks(scheme, tr, rendered, width)),
        SectionKind::Pushes => cards.child(pushes(scheme, tr, &rendered.pushes, width, &colors)),
        SectionKind::Conversation => {
            let mut cards = cards.child(heading(tr.discussion_conversation));
            for comment in &rendered.discussion.comments {
                cards = cards.child(comment_card(scheme, comment, width, &colors));
            }
            cards
        }
        SectionKind::Thread { .. } => match rendered.thread(index) {
            Some(thread) => cards.child(thread_card(scheme, tr, thread, width, &colors)),
            None => cards,
        },
    }
}

/// The title, state, branches and author; a conflict warning; the checks,
/// reviews and threads in short; the description.
fn overview(
    mut cards: Column,
    scheme: &'static Scheme,
    tr: &'static Strings,
    rendered: &Rendered,
    width: f32,
    colors: &ReaderColors,
) -> Column {
    let discussion = &rendered.discussion;
    let mut title = Column::new().width(width).gap(8.0).child(
        Text::new(format!("{} !{}", discussion.title, rendered.number))
            .size(22.0)
            .wrap(WrapMode::Word),
    );
    let mut meta = Row::new().gap(8.0).align(Align::Center).wrap().width(width);
    let state = match discussion.state {
        BranchState::Open => Some((tr.change_open, scheme.lanes[4])),
        BranchState::Merged => Some((tr.change_merged, scheme.lanes[5])),
        BranchState::Closed => Some((tr.change_closed, scheme.lanes[3])),
        BranchState::None => None,
    };
    if let Some((text, color)) = state {
        meta = meta.child(badge(scheme, text, color));
    }
    if discussion.draft {
        meta = meta.child(badge(scheme, tr.change_draft, scheme.muted));
    }
    let mut branches = vec![RichSpan::code(rendered.branch.clone())];
    if let Some(target) = &discussion.target {
        branches.push(RichSpan::new(" → ", tint(scheme.muted)));
        branches.push(RichSpan::code(target.clone()));
    }
    meta = meta.child(RichText::new(RichDoc::new().paragraph(branches)).size(META_SIZE + 0.5));
    if let Some(author) = &discussion.author {
        meta = meta.child(muted(scheme, (tr.discussion_by)(author), META_SIZE + 0.5));
    }
    title = title.child(meta);
    cards = cards.child(title);

    if discussion.conflicts {
        cards = cards.child(
            Panel::new()
                .width(width)
                .padding(CARD_PAD)
                .surface(scheme.warning_surface())
                .rail(scheme.lanes[1])
                .rail_width(3.0)
                .child(
                    Text::new(tr.change_conflicts)
                        .size(BODY_SIZE)
                        .wrap(WrapMode::Word),
                ),
        );
    }

    let mut summary = Column::new().width(width - CARD_PAD * 2.0).gap(10.0);
    if !rendered.checks.is_empty() {
        let (passed, failed, waiting) = rendered.tally();
        let worst = rendered.checks[0].state;
        summary = summary.child(
            Row::new()
                .gap(10.0)
                .align(Align::Center)
                .child(mark(scheme, check_mark(worst), check_color(scheme, worst)))
                .child(Text::new(tr.change_checks).size(BODY_SIZE))
                .child(muted(
                    scheme,
                    (tr.change_checks_summary)(passed, failed, waiting),
                    META_SIZE + 0.5,
                )),
        );
    }
    if !discussion.reviews.is_empty() {
        let mut reviews = Column::new()
            .gap(6.0)
            .child(label(scheme, tr.change_reviews));
        for review in &discussion.reviews {
            let (symbol, color, verdict) = match review.verdict {
                Verdict::Approved => ('✓', scheme.lanes[4], tr.change_approved),
                Verdict::Changes => ('✗', scheme.lanes[3], tr.change_requested_changes),
                Verdict::Commented => ('●', scheme.lanes[2], tr.change_commented),
                Verdict::Pending => ('○', scheme.muted, tr.change_review_pending),
            };
            reviews = reviews.child(
                Row::new()
                    .gap(10.0)
                    .align(Align::Center)
                    .child(mark(scheme, symbol, color))
                    .child(
                        RichText::new(RichDoc::new().paragraph([
                            RichSpan::bold(review.author.clone()),
                            RichSpan::new(format!("  {verdict}"), tint(scheme.muted)),
                        ]))
                        .size(BODY_SIZE),
                    ),
            );
        }
        summary = summary.child(reviews);
    }
    if !discussion.threads.is_empty() {
        let open = discussion.threads.iter().filter(|t| !t.resolved).count();
        let resolved = discussion.threads.len() - open;
        let color = if open > 0 {
            scheme.lanes[1]
        } else {
            scheme.lanes[4]
        };
        summary = summary.child(
            Row::new()
                .gap(10.0)
                .align(Align::Center)
                .child(mark(scheme, if open > 0 { '●' } else { '✓' }, color))
                .child(Text::new((tr.change_threads)(open, resolved)).size(BODY_SIZE)),
        );
    }
    if !rendered.checks.is_empty()
        || !discussion.reviews.is_empty()
        || !discussion.threads.is_empty()
    {
        cards = cards.child(card(scheme, width).child(summary));
    }

    let description = discussion.body.as_deref().filter(|b| !b.trim().is_empty());
    let inner = width - CARD_PAD * 2.0;
    let body: Box<dyn View> = match description {
        Some(body) => Box::new(
            Column::new()
                .width(inner)
                .child(RichText::new(markdown(body, scheme, colors)).size(BODY_SIZE)),
        ),
        None => Box::new(muted(scheme, tr.change_no_description, BODY_SIZE)),
    };
    cards
        .child(heading(tr.change_description))
        .child(card(scheme, width).child(body))
}

/// Every check, the worst first, with a button to its page.
fn checks(
    scheme: &'static Scheme,
    tr: &'static Strings,
    rendered: &Rendered,
    width: f32,
) -> impl View + use<> {
    let inner = width - CARD_PAD * 2.0;
    let (passed, failed, waiting) = rendered.tally();
    let mut list = Column::new().width(inner).gap(4.0).child(
        Pad::insets(EdgeInsets {
            bottom: 6.0,
            ..EdgeInsets::default()
        })
        .child(muted(
            scheme,
            (tr.change_checks_summary)(passed, failed, waiting),
            META_SIZE + 0.5,
        )),
    );
    for check in &rendered.checks {
        let mut row = Row::new()
            .width(inner)
            .height(30.0)
            .justify(Justify::SpaceBetween)
            .align(Align::Center)
            .child(
                Row::new()
                    .gap(10.0)
                    .align(Align::Center)
                    .child(mark(
                        scheme,
                        check_mark(check.state),
                        check_color(scheme, check.state),
                    ))
                    .child(Text::new(check.name.clone()).size(BODY_SIZE).ellipsis()),
            );
        if let Some(url) = check.url.clone() {
            row = row.child(
                Button::new((tr.change_open_check)(&check.name))
                    .display(tr.change_open_link)
                    .appearance(ButtonAppearance::Ghost)
                    .text_size(12.5)
                    .tooltip(url.clone())
                    .on_click(move || open_in_system_browser(&url)),
            );
        }
        list = list.child(row);
    }
    Column::new()
        .width(width)
        .gap(14.0)
        .child(heading(tr.change_checks))
        .child(card(scheme, width).child(list))
}

/// The heads of the branch, oldest first, each rewrite with what
/// `git range-diff` says of it.
fn pushes(
    scheme: &'static Scheme,
    tr: &'static Strings,
    pushes: &[PushEntry],
    width: f32,
    colors: &ReaderColors,
) -> impl View + use<> {
    let inner = width - CARD_PAD * 2.0;
    let mut list = Column::new().width(inner).gap(10.0);
    for push in pushes {
        let mut spans = Vec::new();
        if let Some(old) = &push.rewrote {
            spans.push(RichSpan::code(old.clone()));
            spans.push(RichSpan::new(" → ", tint(scheme.muted)));
        }
        spans.push(RichSpan::code(push.commit.clone()));
        spans.push(RichSpan::new(
            format!("  {}", push.what),
            tint(scheme.muted),
        ));
        let mut line = Row::new()
            .gap(10.0)
            .align(Align::Center)
            .wrap()
            .width(inner);
        let (symbol, color) = if push.rewrote.is_some() {
            ('↻', scheme.lanes[1])
        } else {
            ('↑', scheme.lanes[2])
        };
        line = line.child(mark(scheme, symbol, color));
        if push.rewrote.is_some() {
            line = line.child(badge(scheme, tr.discussion_rewritten, scheme.lanes[1]));
        }
        line = line.child(RichText::new(RichDoc::new().paragraph(spans)).size(BODY_SIZE));
        if !push.at.is_empty() {
            line = line.child(muted(scheme, push.at.clone(), META_SIZE));
        }
        let mut entry = Column::new().width(inner).gap(6.0).child(line);
        if !push.range_diff.is_empty() {
            let lines = push
                .range_diff
                .iter()
                .map(|line| vec![RichSpan::token(line.clone(), range_color(line, colors))]);
            entry = entry.child(
                Column::new()
                    .width(inner)
                    .child(RichText::new(RichDoc::new().code_block("", lines)).size(12.5)),
            );
        }
        list = list.child(entry);
    }
    Column::new()
        .width(width)
        .gap(14.0)
        .child(heading(tr.discussion_pushes))
        .child(card(scheme, width).child(list))
}

/// The color of a `git range-diff` summary line by what happened to the
/// commit: kept, changed, added or dropped.
fn range_color(line: &str, colors: &ReaderColors) -> Color {
    let status = line.split_whitespace().nth(2).unwrap_or("");
    match status {
        "=" => colors.muted,
        "!" => colors.hunk,
        ">" => colors.added,
        "<" => colors.removed,
        _ => colors.ink,
    }
}

/// A code thread: where it sits on the head, the code around its line, and
/// its comments.
fn thread_card(
    scheme: &'static Scheme,
    tr: &'static Strings,
    thread: &PlacedThread,
    width: f32,
    colors: &ReaderColors,
) -> impl View + use<> {
    let inner = width - CARD_PAD * 2.0;
    let mut head = Row::new().gap(8.0).align(Align::Center).wrap().width(inner);
    head = head.child(if thread.resolved {
        badge(scheme, tr.discussion_resolved, scheme.lanes[4])
    } else {
        badge(scheme, tr.change_open, scheme.lanes[1])
    });
    let mut place = vec![RichSpan::new(
        thread.path.clone(),
        SpanStyle {
            bold: true,
            ..SpanStyle::code()
        },
    )];
    if let Some(line) = thread.line {
        place.push(RichSpan::new(
            format!("  {}", (tr.change_line)(line)),
            tint(scheme.muted),
        ));
    }
    head = head.child(RichText::new(RichDoc::new().paragraph(place)).size(BODY_SIZE));
    if thread.outdated {
        head = head.child(badge(scheme, tr.discussion_outdated, scheme.muted));
    }
    let mut column = Column::new().width(inner).gap(10.0).child(head);
    if !thread.note.is_empty() {
        column = column.child(muted(scheme, thread.note.clone(), META_SIZE + 0.5));
    }
    if let Some(code) = snippet(scheme, thread, colors) {
        column = column.child(
            Column::new()
                .width(inner)
                .child(RichText::new(code).size(12.5)),
        );
    }
    for comment in &thread.comments {
        column = column
            .child(Fill::new(inner, 1.0, scheme.line))
            .child(comment_body(scheme, comment, inner, colors));
    }
    let rail = if thread.resolved {
        scheme.line
    } else {
        scheme.lanes[1]
    };
    card(scheme, width).rail(rail).rail_width(3.0).child(column)
}

/// The code around a thread's line on the head, highlighted, its line
/// marked; else the forge's own hunk.
fn snippet(scheme: &Scheme, thread: &PlacedThread, colors: &ReaderColors) -> Option<RichDoc> {
    if let Some(line) = thread.line
        && !thread.code.is_empty()
    {
        let first = thread.code[0].1.as_bytes();
        let language = highlight::detect(Path::new(&thread.path), first);
        let digits = thread.code.last().map_or(1, |(n, _)| n.to_string().len());
        let mut state = State::default();
        let lines = thread.code.iter().map(|(number, text)| {
            let marked = *number == line;
            let mut spans = vec![RichSpan::new(
                format!("{} {number:>digits$}  ", if marked { '▶' } else { ' ' }),
                SpanStyle {
                    bold: marked,
                    ..SpanStyle::token(if marked {
                        scheme.lanes[1]
                    } else {
                        colors.muted
                    })
                },
            )];
            highlighted(&mut spans, language, &mut state, text, colors);
            spans
        });
        let lines: Vec<_> = lines.collect();
        return Some(RichDoc::new().code_block("", lines));
    }
    let hunk = thread.hunk.as_deref()?;
    let mut state = State::default();
    let lines = hunk.lines().map(|text| {
        let mut spans = Vec::new();
        highlighted(&mut spans, Some(&highlight::DIFF), &mut state, text, colors);
        spans
    });
    let lines: Vec<_> = lines.collect();
    Some(RichDoc::new().code_block("diff", lines))
}

/// Appends `text` to `spans` as code, colored by `language` from `state`.
fn highlighted(
    spans: &mut Vec<RichSpan>,
    language: Option<&highlight::Language>,
    state: &mut State,
    text: &str,
    colors: &ReaderColors,
) {
    match language {
        Some(language) => highlight::highlight(language, state, text, |start, end, token| {
            spans.push(RichSpan::token(
                &text[start..end],
                colors.syntax[token as usize],
            ));
        }),
        None => spans.push(RichSpan::code(text)),
    }
}

/// A comment of the conversation on a card of its own.
fn comment_card(
    scheme: &'static Scheme,
    comment: &Comment,
    width: f32,
    colors: &ReaderColors,
) -> impl View + use<> {
    card(scheme, width).child(comment_body(
        scheme,
        comment,
        width - CARD_PAD * 2.0,
        colors,
    ))
}

/// The author and time of a comment over its text.
fn comment_body(
    scheme: &Scheme,
    comment: &Comment,
    width: f32,
    colors: &ReaderColors,
) -> impl View + use<> {
    let mut byline = vec![RichSpan::bold(comment.author.clone())];
    if let Some(at) = &comment.at {
        byline.push(RichSpan::new(
            format!("  {}", short_time(at)),
            tint(scheme.muted),
        ));
    }
    Column::new()
        .width(width)
        .gap(6.0)
        .child(RichText::new(RichDoc::new().paragraph(byline)).size(BODY_SIZE))
        .child(RichText::new(markdown(comment.body.trim(), scheme, colors)).size(BODY_SIZE))
}

// --- pieces ------------------------------------------------------------------

/// A card: the raised surface with a hairline around it.
fn card(scheme: &Scheme, width: f32) -> Panel {
    Panel::new()
        .width(width)
        .padding(CARD_PAD)
        .surface(scheme.rail)
        .outline(scheme.line)
}

fn heading(text: &'static str) -> impl View + use<> {
    Text::new(text).size(15.0)
}

/// A small muted title over a group.
fn label(scheme: &Scheme, text: &'static str) -> impl View + use<> {
    muted(scheme, text, META_SIZE)
}

fn muted<T: Into<std::borrow::Cow<'static, str>>>(
    scheme: &Scheme,
    text: T,
    size: f32,
) -> impl View + use<T> {
    ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(size).wrap(WrapMode::Word),
    )
}

/// A one-character state mark in `color`.
fn mark(scheme: &Scheme, symbol: char, color: Color) -> impl View + use<> {
    ThemeProvider::new(
        Theme {
            text: color,
            ..scheme.theme()
        },
        Text::new(symbol.to_string()).size(15.0),
    )
}

/// A pill of `color` with `text` on it.
fn badge(scheme: &Scheme, text: &'static str, color: Color) -> impl View + use<> {
    ThemeProvider::new(
        Theme {
            attention: color,
            on_accent: scheme.page,
            ..scheme.theme()
        },
        Badge::new(text),
    )
}

/// A span style of plain text in `color`.
fn tint(color: Color) -> SpanStyle {
    SpanStyle {
        color: Some(color),
        ..SpanStyle::PLAIN
    }
}

fn check_color(scheme: &Scheme, state: CheckState) -> Color {
    match state {
        CheckState::Success => scheme.lanes[4],
        CheckState::Failed => scheme.lanes[3],
        CheckState::Running | CheckState::Pending => scheme.lanes[1],
        CheckState::Skipped | CheckState::Canceled => scheme.muted,
    }
}

// --- Markdown ----------------------------------------------------------------

/// A forge's Markdown as a rich document: paragraphs, headings, lists,
/// quotes, highlighted code blocks, rules, and tables as one line per row.
/// Inline HTML stays as text.
pub fn markdown(text: &str, scheme: &Scheme, colors: &ReaderColors) -> RichDoc {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut out = Markdown {
        doc: RichDoc::new(),
        spans: Vec::new(),
        bold: 0,
        italic: 0,
        strike: 0,
        link: 0,
        heading: None,
        quote: 0,
        lists: Vec::new(),
        item: None,
        code: None,
        muted: scheme.muted,
    };
    for event in Parser::new_ext(text, options) {
        out.event(event, colors);
    }
    out.flush();
    out.doc
}

struct Markdown {
    doc: RichDoc,
    spans: Vec<RichSpan>,
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
    heading: Option<u8>,
    quote: u32,
    /// The next number of each open list; `None` for a bulleted one.
    lists: Vec<Option<u64>>,
    /// The marker of the list item whose first block is not out yet.
    item: Option<ListMarker>,
    /// A code block's language and text so far.
    code: Option<(String, String)>,
    muted: Color,
}

impl Markdown {
    fn style(&self) -> SpanStyle {
        SpanStyle {
            bold: self.bold > 0 || self.heading.is_some(),
            italic: self.italic > 0,
            strike: self.strike > 0,
            link: self.link > 0,
            underline: self.link > 0,
            ..SpanStyle::PLAIN
        }
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        self.spans.push(RichSpan::new(text, style));
    }

    /// Ends the block the spans so far belong to.
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let block = if let Some(level) = self.heading {
            RichBlock::Heading { level, spans }
        } else if let Some(marker) = self.item.take() {
            RichBlock::ListItem {
                depth: self.lists.len().saturating_sub(1).min(8) as u8,
                marker,
                spans,
            }
        } else if self.quote > 0 {
            RichBlock::Quote(spans)
        } else {
            RichBlock::Paragraph(spans)
        };
        self.doc.blocks.push(block);
    }

    fn event(&mut self, event: Event<'_>, colors: &ReaderColors) {
        if let Some((_, code)) = &mut self.code {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.end_code(colors),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    self.flush();
                    self.heading = Some(level as u8);
                }
                Tag::BlockQuote(_) => {
                    self.flush();
                    self.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    self.flush();
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().unwrap_or("").to_owned()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((language, String::new()));
                }
                Tag::List(start) => {
                    self.flush();
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.flush();
                    let marker = match self.lists.last_mut() {
                        Some(Some(next)) => {
                            let marker = ListMarker::Number(*next as u32);
                            *next += 1;
                            marker
                        }
                        _ => ListMarker::Bullet,
                    };
                    self.item = Some(marker);
                }
                Tag::Emphasis => self.italic += 1,
                Tag::Strong => self.bold += 1,
                Tag::Strikethrough => self.strike += 1,
                Tag::Link { .. } => self.link += 1,
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Item | TagEnd::TableHead | TagEnd::TableRow => {
                    self.flush();
                }
                TagEnd::Heading(_) => {
                    self.flush();
                    self.heading = None;
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.quote = self.quote.saturating_sub(1);
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                }
                TagEnd::TableCell => {
                    self.spans.push(RichSpan::new("  │  ", tint(self.muted)));
                }
                TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
                TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
                TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
                TagEnd::Link => self.link = self.link.saturating_sub(1),
                _ => {}
            },
            Event::Text(text) => self.text(&text),
            Event::Code(text) => self.spans.push(RichSpan::code(text.into_string())),
            Event::Html(text) | Event::InlineHtml(text) => {
                let text = text.trim_end_matches('\n');
                if !text.is_empty() {
                    self.spans.push(RichSpan::new(text, tint(self.muted)));
                }
            }
            Event::SoftBreak | Event::HardBreak => self.text(" "),
            Event::Rule => {
                self.flush();
                self.doc.blocks.push(RichBlock::Rule);
            }
            Event::TaskListMarker(done) => self.text(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }

    fn end_code(&mut self, colors: &ReaderColors) {
        let Some((language, text)) = self.code.take() else {
            return;
        };
        let highlighter = highlight::by_name(&language);
        let mut state = State::default();
        let lines: Vec<Vec<RichSpan>> = text
            .lines()
            .map(|line| {
                let mut spans = Vec::new();
                highlighted(&mut spans, highlighter, &mut state, line, colors);
                spans
            })
            .collect();
        self.doc
            .blocks
            .push(RichBlock::CodeBlock { language, lines });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(doc: &RichDoc) -> Vec<String> {
        doc.blocks
            .iter()
            .map(|block| match block {
                RichBlock::Paragraph(spans)
                | RichBlock::Heading { spans, .. }
                | RichBlock::ListItem { spans, .. }
                | RichBlock::Quote(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
                RichBlock::CodeBlock { lines, .. } => lines
                    .iter()
                    .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n"),
                RichBlock::Rule => "---".to_owned(),
            })
            .collect()
    }

    #[test]
    fn markdown_keeps_blocks_and_styles() {
        let scheme = super::super::resolve_scheme("spruce", true);
        let colors = scheme.reader_colors();
        let doc = markdown(
            "# Fix\n\nSee **this** and `code`.\n\n- one\n- two\n\n1. a\n\n> quoted\n\n```rust\nfn a() {}\n```\n\n---",
            scheme,
            &colors,
        );
        assert_eq!(
            plain(&doc),
            [
                "Fix",
                "See this and code.",
                "one",
                "two",
                "a",
                "quoted",
                "fn a() {}",
                "---"
            ]
        );
        let RichBlock::Paragraph(spans) = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks[1]);
        };
        assert!(spans[1].style.bold);
        assert!(spans[3].style.code);
        assert!(matches!(
            doc.blocks[4],
            RichBlock::ListItem {
                marker: ListMarker::Number(1),
                ..
            }
        ));
        assert!(matches!(doc.blocks[5], RichBlock::Quote(_)));
        let RichBlock::CodeBlock { lines, .. } = &doc.blocks[6] else {
            panic!();
        };
        assert!(lines[0].len() > 1, "highlighted: {lines:?}");
    }

    #[test]
    fn range_diff_lines_take_their_state_color() {
        let colors = super::super::resolve_scheme("spruce", true).reader_colors();
        assert_eq!(range_color("1:  abc = 1:  abc one", &colors), colors.muted);
        assert_eq!(range_color("2:  abc ! 2:  def two", &colors), colors.hunk);
        assert_eq!(range_color("-:  --- > 3:  fed new", &colors), colors.added);
    }
}
