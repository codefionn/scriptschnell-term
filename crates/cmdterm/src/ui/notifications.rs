//! The notification overlay, opened with the bell in the rail's footer. It
//! sits beside the rail's bottom edge and lists recent notices and update
//! check results, newest first, with Update and Dismiss buttons. When two or
//! more updates wait, Update all runs them one after another. It is its own
//! region; opening it marks every notification read, which clears the bell's
//! badge.

use std::time::SystemTime;

use schnellui::scene::{Color, Point, Size};
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, Dialog, DialogPosition, Justify, Row, Scroll, Text,
    ThemeProvider, View, WrapMode,
};

use super::settings::label;
use super::{Closed, Fill};
use crate::i18n::Strings;
use crate::notifications::{Notification, Topic};
use crate::workspace::Workspace;

const WIDTH: f32 = 400.0;
const MAX_HEIGHT: f32 = 460.0;
const PAD: f32 = 12.0;
const INNER: f32 = WIDTH - PAD * 2.0;
/// Width of a card: the list's width less room for its scroll bar.
const CARD: f32 = INNER - 14.0;
/// Characters of a card's title on one line, about: 13.5 px text in the
/// title's column.
const TITLE_CHARS: usize = 38;
/// Characters of a card's body on one line, about: 12 px text across the card.
const BODY_CHARS: usize = 58;
const TITLE_LINE: f32 = 19.0;
const BODY_LINE: f32 = 16.5;
/// Space between the overlay and the status bar.
const BOTTOM: f32 = 6.0;
const HEADER_HEIGHT: f32 = 30.0;
/// Height of the row of actions under the title.
const ACTIONS_HEIGHT: f32 = 28.0;
/// Height of the line that says there is nothing new.
const EMPTY_HEIGHT: f32 = 24.0;
/// How far the age sits below the title's top, which puts both on one
/// baseline.
const AGE_DROP: f32 = 4.5;

/// Room for a card's age right of its title.
const AGE_WIDTH: f32 = 72.0;

impl Workspace {
    /// Opens the notification overlay, or closes it when it is open.
    pub fn toggle_notifications(&self) {
        if self.notifications_is_open() {
            self.close_notifications();
            return;
        }
        let mut inner = self.borrow_mut();
        inner.notifications_open = true;
        inner.notifications.mark_all_read();
        inner.dirty.notifications = true;
        // The rail's bell shows the popover is open.
        inner.dirty.sidebar = true;
    }

    pub fn close_notifications(&self) {
        let mut inner = self.borrow_mut();
        if inner.notifications_open {
            inner.notifications_open = false;
            inner.notifications.mark_all_read();
            inner.dirty.notifications = true;
            inner.dirty.sidebar = true;
        }
    }

    pub fn notifications_is_open(&self) -> bool {
        self.borrow().notifications_open
    }

    /// Looks for outdated agent integrations and agent releases in every
    /// environment a project lives in, on a worker thread.
    pub fn check_updates(&self) {
        let envs = self.agent_environments();
        let center = self.borrow().notifications.clone();
        center.check(envs, self.redraw_handle());
    }

    /// Runs the update notification `id` offers.
    pub fn run_notification(&self, id: u64) {
        let center = self.borrow().notifications.clone();
        center.run(id, self.redraw_handle());
    }

    /// Runs the updates of the notifications `ids`, one after another.
    pub fn run_notifications(&self, ids: Vec<u64>) {
        let center = self.borrow().notifications.clone();
        center.run_ids(ids, self.redraw_handle());
    }

    /// Runs every update the notifications offer, one after another.
    pub fn run_all_notifications(&self) {
        let center = self.borrow().notifications.clone();
        center.run_all(self.redraw_handle());
    }

    pub fn dismiss_notification(&self, id: u64) {
        self.borrow().notifications.dismiss(id);
    }

    pub fn clear_notifications(&self) {
        self.borrow().notifications.clear();
    }
}

/// The overlay: a dialog while open, an empty slot while closed.
pub fn notifications(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let inner = ws.borrow();
    if !inner.notifications_open {
        return Box::new(Closed);
    }
    let (scheme, tr, focus) = (inner.scheme, inner.tr, inner.regions.notifications_focus);
    let items = inner.notifications.items();
    let checking = inner.notifications.checking();
    drop(inner);
    let pending = items
        .iter()
        .filter(|item| !item.busy && item.topic.actionable())
        .count();

    let (check, update_all, clear, close) = (ws.clone(), ws.clone(), ws.clone(), ws.clone());
    // Outdated integrations of several agents share one card: the same
    // sentence five times hides the notices around it.
    let outdated: Vec<&Notification> = items
        .iter()
        .filter(|item| !item.busy && matches!(item.topic, Topic::IntegrationOutdated { .. }))
        .collect();
    let group = outdated.len() >= 2;
    let mut actions = Row::new()
        .gap(6.0)
        .align(Align::Center)
        .height(ACTIONS_HEIGHT);
    // One card's update has its own button on that card.
    let pending_cards = pending.saturating_sub(if group { outdated.len() - 1 } else { 0 });
    // Focus starts on Close: Enter or Space right after opening the bell
    // must never run an update. The ref goes to one button only; the
    // others get a spare one.
    let spare = schnellui::scene::ComponentRef::new;
    let (update_focus, mut card_focus, check_focus, close_focus) = (
        spare(),
        None::<schnellui::scene::ComponentRef>,
        spare(),
        focus,
    );
    // The title with Close on one row, the actions on their own row under
    // it, so neither crowds the other.
    let title = Row::new()
        .width(INNER)
        .height(HEADER_HEIGHT)
        .justify(Justify::SpaceBetween)
        .align(Align::Center)
        .child(Text::new(tr.notifications).size(15.0).bold())
        .child(
            // Framed, like the settings' close button.
            Button::new(tr.close_notifications)
                .display("×")
                .appearance(ButtonAppearance::Outline)
                .width(28.0)
                .height(28.0)
                .text_size(18.0)
                .tooltip(tr.close_escape)
                .on_click(move || close.close_notifications())
                .with_ref(close_focus),
        );
    let primary = pending_cards < 2;
    if pending_cards >= 2 {
        actions = actions.child(
            Button::new(tr.update_all)
                .appearance(ButtonAppearance::Solid)
                .text_size(12.5)
                .width(super::text_button_width(tr.update_all, 12.5))
                .on_click(move || update_all.run_all_notifications())
                .with_ref(update_focus),
        );
    }
    let actions = actions
        .child(
            Button::new(if checking {
                tr.checking_for_updates
            } else {
                tr.check_for_updates
            })
            .appearance(ButtonAppearance::Outline)
            .text_size(12.5)
            // As wide whether it checks or not, so the row keeps still.
            .width(
                super::text_button_width(tr.check_for_updates, 12.5)
                    .max(super::text_button_width(tr.checking_for_updates, 12.5)),
            )
            .disabled(checking)
            .on_click(move || check.check_updates())
            .with_ref(check_focus),
        )
        .child(
            Button::new(tr.clear_notifications)
                .appearance(ButtonAppearance::Outline)
                .text_size(12.5)
                .width(super::text_button_width(tr.clear_notifications, 12.5))
                .disabled(items.is_empty())
                .on_click(move || clear.clear_notifications()),
        );
    let header = Column::new()
        .width(INNER)
        .gap(4.0)
        .child(title)
        .child(actions);

    let now = SystemTime::now();
    let mut list = Column::new().width(CARD).gap(10.0);
    if items.is_empty() {
        list = list.child(label(scheme, tr.no_notifications));
    }
    let mut first = true;
    let mut grouped = false;
    // The list's height, about, so a short list gets a popover that shows
    // every card whole, buttons included.
    let mut list_need = if items.is_empty() { EMPTY_HEIGHT } else { 0.0 };
    for item in &items {
        let in_group = group && outdated.iter().any(|other| other.id == item.id);
        if in_group && grouped {
            continue;
        }
        // A rule between cards keeps their buttons with their own text.
        if !first {
            list = list.child(Fill::new(CARD, 1.0, scheme.line));
            list_need += 1.0 + 20.0;
        }
        first = false;
        if in_group {
            grouped = true;
            list_need += outdated_height(tr, &outdated);
            let card_ref = card_focus.take().unwrap_or_else(spare);
            list = list.child(outdated_card(
                ws, tr, scheme, &outdated, now, primary, card_ref,
            ));
        } else {
            let (title, body) = text(tr, &item.topic);
            list_need += card_height(&title, &[&body]);
            let card_ref = if item.topic.actionable() && !item.busy {
                card_focus.take().unwrap_or_else(spare)
            } else {
                spare()
            };
            list = list.child(card(ws, tr, scheme, item, now, primary, card_ref));
        }
    }

    // As tall as the cards need, up to the most the window allows; past
    // that the list scrolls.
    let chrome = PAD * 2.0 + HEADER_HEIGHT + 4.0 + ACTIONS_HEIGHT + 8.0;
    let height = (chrome + list_need)
        .min(MAX_HEIGHT)
        .min(viewport.height - super::STATUS_HEIGHT - BOTTOM * 2.0)
        .max(chrome + EMPTY_HEIGHT);
    let list_height = height - PAD * 2.0 - HEADER_HEIGHT - 4.0 - ACTIONS_HEIGHT - 8.0;
    let dismiss = ws.clone();
    Box::new(
        Dialog::new(tr.notifications)
            .modal()
            .undecorated()
            // The point counts from inside the viewport inset.
            .position(DialogPosition::At(Point {
                x: ws.rail_width(),
                // Above the status bar, so the notice it opens from stays
                // in view. With the rail hidden the bell is in the tab
                // strip, so the popover hangs under it.
                y: if ws.borrow().rail_hidden {
                    super::TAB_HEIGHT - BOTTOM
                } else {
                    viewport.height - height - super::STATUS_HEIGHT - BOTTOM * 2.0
                },
            }))
            .width(WIDTH)
            .max_width(WIDTH)
            .viewport_inset(BOTTOM)
            // A popover: clicking beside it closes it without dimming the
            // window.
            .backdrop(Color::TRANSPARENT)
            // On a light scheme the rail is as pale as a terminal; the
            // raised surface sets the popover apart from what it covers.
            .surface(if scheme.dark {
                scheme.rail
            } else {
                scheme.raised
            })
            .padding(PAD)
            .gap(8.0)
            .on_dismiss(move || dismiss.close_notifications())
            .child(header)
            .child(
                Scroll::new()
                    .label(tr.notifications)
                    .restoration_key(tr.notifications)
                    // The bar says more cards wait below the last one shown.
                    .scrollbar(true)
                    .size(INNER, list_height)
                    .child(list),
            ),
    )
}

/// Lines `text` wraps to at `per_line` characters a line, about.
fn lines(text: &str, per_line: usize) -> f32 {
    text.chars().count().div_ceil(per_line).max(1) as f32
}

/// About the height of a card with `title` and the paragraphs of its body,
/// with its row of buttons.
fn card_height(title: &str, body: &[&str]) -> f32 {
    let body: f32 = body
        .iter()
        .filter(|text| !text.is_empty())
        .map(|text| lines(text, BODY_CHARS) * BODY_LINE + 3.0)
        .sum();
    lines(title, TITLE_CHARS) * TITLE_LINE + 3.0 + body + 28.0 + 4.0
}

/// About the height of the card of several outdated integrations.
fn outdated_height(tr: &Strings, items: &[&Notification]) -> f32 {
    let names: usize = items
        .iter()
        .map(|item| match &item.topic {
            Topic::IntegrationOutdated { agent, env } => {
                agent.name().chars().count() + env.label().chars().count() + 5
            }
            _ => 0,
        })
        .sum();
    let names = "x".repeat(names);
    card_height(
        &(tr.integrations_outdated)(items.len()),
        &[&names, tr.integration_outdated_body],
    )
}

/// One notification: its title and age, what it says, and its buttons.
/// A card's Update: filled while no "Update all" stands above it.
fn update_appearance(primary: bool) -> ButtonAppearance {
    if primary {
        ButtonAppearance::Solid
    } else {
        ButtonAppearance::Outline
    }
}

/// A card's title, and its age right of it on the title's first baseline.
fn title_row(scheme: &super::Scheme, title: &str, age: String) -> impl View + use<> {
    Row::new()
        .width(CARD)
        .justify(Justify::SpaceBetween)
        .align(Align::Start)
        .child(
            Column::new()
                .width(CARD - AGE_WIDTH)
                .child(Text::new(title.to_owned()).size(13.5).wrap(WrapMode::Word)),
        )
        // The age never wraps, so it cannot take the title's width. It sits
        // lower: the title's wrapped lines are taller than the age's single one.
        .child(
            schnellui::widgets::Pad::insets(schnellui::layout::EdgeInsets {
                top: AGE_DROP,
                ..schnellui::layout::EdgeInsets::default()
            })
            .child(ThemeProvider::new(
                scheme.muted_theme(),
                Text::new(age).size(12.0).wrap(WrapMode::NoWrap),
            )),
        )
}

fn card(
    ws: &Workspace,
    tr: &'static Strings,
    scheme: &super::Scheme,
    item: &Notification,
    now: SystemTime,
    primary: bool,
    update_ref: schnellui::scene::ComponentRef,
) -> impl View + use<> {
    let (title, body) = text(tr, &item.topic);
    let mut column =
        Column::new()
            .width(CARD)
            .gap(3.0)
            .child(title_row(scheme, &title, ago(tr, item.at, now)));
    if !body.is_empty() {
        column = column.child(ThemeProvider::new(
            scheme.muted_theme(),
            Text::new(body).size(12.0).wrap(WrapMode::Word),
        ));
    }
    let id = item.id;
    let mut buttons = Row::new().gap(4.0).justify(Justify::End).width(CARD);
    if item.topic.actionable() {
        let run = ws.clone();
        buttons = buttons.child(
            Button::new((tr.update_notification)(&title))
                .display(if item.busy { tr.updating } else { tr.update })
                .width(
                    super::text_button_width(tr.update, 12.5)
                        .max(super::text_button_width(tr.updating, 12.5)),
                )
                // Filled when it is the popover's main action; framed under
                // "Update all", so one button stays the filled one.
                .appearance(update_appearance(primary))
                .text_size(12.5)
                .height(28.0)
                .disabled(item.busy)
                .on_click(move || run.run_notification(id))
                .with_ref(update_ref),
        );
    }
    if !item.busy {
        let dismiss = ws.clone();
        buttons = buttons.child(
            Button::new((tr.dismiss_notification)(&title))
                .display(tr.dismiss)
                .width(super::text_button_width(tr.dismiss, 12.5))
                .appearance(ButtonAppearance::Outline)
                .text_size(12.5)
                .height(28.0)
                .on_click(move || dismiss.dismiss_notification(id)),
        );
    }
    column.child(buttons)
}

/// One card for the outdated integrations of several agents: which ones,
/// and buttons that update or dismiss them all.
fn outdated_card(
    ws: &Workspace,
    tr: &'static Strings,
    scheme: &super::Scheme,
    items: &[&Notification],
    now: SystemTime,
    primary: bool,
    update_ref: schnellui::scene::ComponentRef,
) -> impl View + use<> {
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| match &item.topic {
            Topic::IntegrationOutdated { agent, env } => {
                Some(format!("{} ({})", agent.name(), env.label()))
            }
            _ => None,
        })
        .collect();
    let title = (tr.integrations_outdated)(items.len());
    let newest = items.iter().map(|item| item.at).max().unwrap_or(now);
    let ids: Vec<u64> = items.iter().map(|item| item.id).collect();
    let (run, dismiss) = (ws.clone(), ws.clone());
    let run_ids = ids.clone();
    Column::new()
        .width(CARD)
        .gap(3.0)
        .child(title_row(scheme, &title, ago(tr, newest, now)))
        .child(ThemeProvider::new(
            scheme.muted_theme(),
            Column::new()
                .width(CARD)
                .gap(2.0)
                .child(Text::new(names.join(", ")).size(12.0).wrap(WrapMode::Word))
                .child(
                    Text::new(tr.integration_outdated_body)
                        .size(12.0)
                        .wrap(WrapMode::Word),
                ),
        ))
        .child(
            Row::new()
                .gap(4.0)
                .justify(Justify::End)
                .width(CARD)
                .child(
                    Button::new((tr.update_notification)(&title))
                        .display(tr.update)
                        .width(super::text_button_width(tr.update, 12.5))
                        .appearance(update_appearance(primary))
                        .text_size(12.5)
                        .height(28.0)
                        .on_click(move || run.run_notifications(run_ids.clone()))
                        .with_ref(update_ref),
                )
                .child(
                    Button::new((tr.dismiss_notification)(&title))
                        .display(tr.dismiss)
                        .width(super::text_button_width(tr.dismiss, 12.5))
                        .appearance(ButtonAppearance::Outline)
                        .text_size(12.5)
                        .height(28.0)
                        .on_click(move || {
                            for id in &ids {
                                dismiss.dismiss_notification(*id);
                            }
                        }),
                ),
        )
}

/// The title and body of a notification about `topic`.
pub fn text(tr: &Strings, topic: &Topic) -> (String, String) {
    match topic {
        Topic::Notice(text) => (tr.notice_title.to_owned(), text.clone()),
        Topic::IntegrationOutdated { agent, env } => (
            (tr.integration_outdated)(agent.name(), env.label()),
            tr.integration_outdated_body.to_owned(),
        ),
        Topic::AgentUpdate {
            agent,
            env,
            manager,
            current,
            latest,
            command,
        } => (
            (tr.agent_update)(agent.name(), latest),
            (tr.agent_update_body)(current, env.label(), manager.name(), &command.join(" ")),
        ),
        Topic::Updated { agent, env } => {
            ((tr.agent_updated)(agent.name(), env.label()), String::new())
        }
        Topic::Failed { agent, env, error } => (
            (tr.agent_update_failed)(agent.name(), env.label()),
            error.clone(),
        ),
        Topic::AppUpdate { version } => ((tr.app_update)(version), tr.app_update_body.to_owned()),
        Topic::AppUpdated { version } => {
            ((tr.app_updated)(version), tr.app_updated_body.to_owned())
        }
        Topic::AppUpdateFailed { error } => (tr.app_update_failed.to_owned(), error.clone()),
    }
}

/// How long ago `at` was, roughly.
fn ago(tr: &Strings, at: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(at).map_or(0, |age| age.as_secs());
    match seconds {
        0..60 => tr.just_now.to_owned(),
        60..3600 => (tr.minutes_ago)(seconds / 60),
        3600..86400 => (tr.hours_ago)(seconds / 3600),
        _ => (tr.days_ago)(seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Agent;
    use crate::i18n::{DE, EN};
    use crate::notifications::Manager;
    use cmdterm_env::Environment;
    use std::time::Duration;

    #[test]
    fn ages_round_down() {
        let now = SystemTime::now();
        let at = |seconds| now - Duration::from_secs(seconds);
        assert_eq!(ago(&EN, at(5), now), "just now");
        assert_eq!(ago(&EN, at(150), now), "2 min ago");
        assert_eq!(ago(&EN, at(7300), now), "2 h ago");
        assert_eq!(ago(&EN, at(90_000), now), "yesterday");
        assert_eq!(ago(&DE, at(200_000), now), "vor 2 Tagen");
    }

    #[test]
    fn updates_name_the_command_they_run() {
        let topic = Topic::AgentUpdate {
            agent: Agent::ClaudeCode,
            env: Environment::Local,
            manager: Manager::Mise,
            current: "2.1.304".into(),
            latest: "2.1.305".into(),
            command: vec!["mise".into(), "upgrade".into(), "claude-code".into()],
        };
        let (title, body) = text(&EN, &topic);
        assert_eq!(title, "Claude Code 2.1.305 is available");
        assert_eq!(
            body,
            "2.1.304 is installed on local with mise. Update runs mise upgrade claude-code."
        );
    }
}
