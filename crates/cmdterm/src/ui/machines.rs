//! The settings pages for the active project, remote machines (with its VPN
//! networks subpage), virtual machines and sandboxing (with its Files and
//! Hosts subpages).
//! Each page scrolls as a whole; `crate::machines` holds what they change.

use std::borrow::Cow;
use std::net::SocketAddr;

use crate::state::{EngineAccess, HOST_GROUPS, Runtime, SidecarEngine};
use schnellui::layout::EdgeInsets;
use schnellui::scene::ComponentRef;
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, Dropdown, DropdownOption, Justify, Pad, PasswordInput,
    Row, Scroll, Switch, Text, TextInput, ThemeProvider, View, WrapMode,
};

use super::settings::{
    GAP, Picker, SCROLLBAR_ROOM, SELECT_WIDTH, body_height, content_width, label, section_title,
};
use super::theme::Scheme;
use super::{Fill, Named, truncate};
use crate::machines::{ImageJob, MachineText, Probe, VmField};
use crate::vpn::{Peer, State, Vpn};
use crate::workspace::{FolderTarget, Workspace};
use cmdterm_remote::image;
use cmdterm_remote::keys::{AuthorizedKey, KeyOptions, PublicKey};

/// Room for the scroll bar right of the page.
fn page_width() -> f32 {
    content_width() - SCROLLBAR_ROOM
}
const BUTTON_WIDTH: f32 = 76.0;

fn page(name: &'static str, column: Column) -> impl View + use<> {
    Scroll::new()
        .label(name)
        .restoration_key(name)
        .scrollbar(true)
        .size(content_width(), body_height())
        // Room under the last item, so it does not touch the edge.
        .child(
            Pad::insets(EdgeInsets {
                bottom: 16.0,
                ..EdgeInsets::default()
            })
            .child(column),
        )
}

/// Text size of the fields and dropdowns in settings pages: the size of
/// the labels and buttons beside them.
pub(super) const INPUT_TEXT_SIZE: f32 = 13.0;
/// Height of a one-line (hint) text input at `INPUT_TEXT_SIZE`, for a
/// button beside one.
pub(super) const INPUT_HEIGHT: f32 = 30.0;

/// An outlined action button, the settings' medium emphasis.
fn button(name: impl Into<Cow<'static, str>>, display: &'static str) -> Button {
    Button::new(name)
        .display(display)
        .appearance(ButtonAppearance::Outline)
        .text_size(13.0)
        .width(super::text_button_width(display, 13.0).max(BUTTON_WIDTH))
        // As tall as the one-line fields it sits beside.
        .height(INPUT_HEIGHT)
}

/// A text-only button for Remove and Forget, which repeat down a list and
/// would crowd it with outlines.
fn quiet_button(name: impl Into<Cow<'static, str>>, display: &'static str) -> Button {
    button(name, display).appearance(ButtonAppearance::Ghost)
}

/// The × that removes one entry of a long list, named and explained by
/// its tooltip.
pub(super) fn remove_icon(name: String) -> Button {
    Button::new(name.clone())
        .display("×")
        .appearance(ButtonAppearance::Ghost)
        .width(REMOVE_WIDTH)
        .height(REMOVE_WIDTH)
        .text_size(16.0)
        .tooltip(name)
}

/// Side of [`remove_icon`].
const REMOVE_WIDTH: f32 = 28.0;

/// A row with a title and subtitle on the left and `controls` on the right.
fn item(title: String, subtitle: String, controls: Vec<Box<dyn View>>, width: f32) -> Row {
    // Buttons side by side keep a gap, so they read as separate buttons.
    let gaps = CONTROL_GAP * controls.len().saturating_sub(1) as f32;
    let mut buttons = Row::new().gap(CONTROL_GAP).align(Align::Center);
    for control in controls {
        buttons = buttons.child(control);
    }
    Row::new()
        .width(page_width())
        .align(Align::Center)
        .child(
            super::settings::list_row(truncate(&title, 48))
                .subtitle(truncate(&subtitle, 64))
                // Not clickable, so no inset: the text lines up with the
                // page's headings.
                .padding(0.0)
                .width(page_width() - width - gaps),
        )
        .child(buttons)
}

/// Space between the buttons at the end of an [`item`].
const CONTROL_GAP: f32 = 8.0;

/// One line of `text` in the terminal font on the dialog's surface.
fn mono_line(scheme: &Scheme, text: &str, family: Option<&str>, name: String) -> impl View + use<> {
    use schnellui::widgets::{TerminalCell, TerminalGrid, TerminalGridModel, TerminalGridPoint};
    let columns = text.chars().count().max(1);
    let mut model = TerminalGridModel::with_colors(columns, 1, scheme.ink, scheme.rail);
    for (column, ch) in text.chars().enumerate() {
        if let Some(cell) = model.cell_mut(TerminalGridPoint { row: 0, column }) {
            *cell = TerminalCell::new(ch.to_string(), scheme.ink, scheme.rail);
        }
    }
    let grid = TerminalGrid::new(model).font_size(13.0).label(name);
    match family {
        Some(family) => grid.font_family(family.to_owned()),
        None => grid,
    }
}

/// A key fingerprint in blocks of four after its algorithm, so two can be
/// compared by eye. Only for display; Copy copies the key itself.
fn grouped(fingerprint: &str) -> String {
    let (algorithm, digest) = fingerprint.split_once(':').unwrap_or(("", fingerprint));
    let mut out = String::with_capacity(fingerprint.len() + fingerprint.len() / 4);
    if !algorithm.is_empty() {
        out.push_str(algorithm);
        out.push_str(": ");
    }
    for (i, c) in digest.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Muted text that breaks into lines at the page's width.
fn note(scheme: &Scheme, text: &'static str) -> impl View + use<> {
    Column::new().width(page_width()).child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(13.0).wrap(WrapMode::Word),
    ))
}

/// A dropdown that takes focus while it is open.
fn focused(scheme: &Scheme, dropdown: Dropdown, open: bool, focus: ComponentRef) -> Box<dyn View> {
    let theme = scheme.control_theme();
    if open {
        Box::new(ThemeProvider::new(theme, dropdown.with_ref(focus)))
    } else {
        Box::new(ThemeProvider::new(theme, dropdown))
    }
}

/// A labeled switch row.
/// A setting with a switch: the title, the note wrapped under it in full,
/// and the switch at the row's end. Notes are what a user decides by, so
/// they never end in an ellipsis.
fn switch(
    scheme: &Scheme,
    title: &'static str,
    subtitle: String,
    on: bool,
    toggle: impl Fn(bool) + 'static,
) -> Row {
    switch_at(scheme, 0.0, title, subtitle, on, toggle)
}

/// A switch row for a setting that only applies while the one above it is
/// on: indented under it, with its switch in the same column.
fn sub_switch(
    scheme: &Scheme,
    title: &'static str,
    subtitle: String,
    on: bool,
    toggle: impl Fn(bool) + 'static,
) -> Row {
    switch_at(scheme, SUB_INDENT, title, subtitle, on, toggle)
}

/// Indent of a setting under the one it depends on.
const SUB_INDENT: f32 = 16.0;

fn switch_at(
    scheme: &Scheme,
    indent: f32,
    title: &'static str,
    subtitle: String,
    on: bool,
    toggle: impl Fn(bool) + 'static,
) -> Row {
    switch_row(scheme, indent, true, title, subtitle, on, toggle)
}

fn switch_row(
    scheme: &Scheme,
    indent: f32,
    live: bool,
    title: &'static str,
    subtitle: String,
    on: bool,
    toggle: impl Fn(bool) + 'static,
) -> Row {
    // A setting with no effect yet keeps its switch's colors, so on reads
    // as on everywhere; only its title is muted.
    // A setting the settings search found shows its title in the accent.
    let title_theme = match super::settings::found_color(title) {
        Some(color) => schnellui::widgets::Theme {
            text: color,
            ..scheme.theme()
        },
        None if live => scheme.theme(),
        None => scheme.muted_theme(),
    };
    let switch_theme = scheme.switch_theme();
    let text_width = switch_row_width() - SWITCH_ROOM - ROW_PAD - indent;
    Row::new()
        .width(switch_row_width())
        .align(Align::Center)
        .justify(Justify::SpaceBetween)
        .child(
            Pad::insets(EdgeInsets {
                left: ROW_PAD + indent,
                right: ROW_END_PAD,
                top: 4.0,
                bottom: 4.0,
            })
            .child(
                Column::new()
                    .width(text_width)
                    .gap(2.0)
                    .child(ThemeProvider::new(
                        title_theme,
                        if super::settings::found_color(title).is_some() {
                            Text::new(title).size(13.0).bold()
                        } else {
                            Text::new(title).size(13.0)
                        },
                    ))
                    .child(ThemeProvider::new(
                        scheme.muted_theme(),
                        Text::new(subtitle).size(12.5).wrap(WrapMode::Word),
                    )),
            ),
        )
        .child(Named(
            title.into(),
            ThemeProvider::new(switch_theme, Switch::new(on).on_toggle(toggle)),
        ))
}

/// Room at a row's end for its switch.
const SWITCH_ROOM: f32 = 56.0;
/// Width of a switch row: the page's, so switches end on the same edge as
/// the buttons of other rows.
fn switch_row_width() -> f32 {
    page_width()
}
/// Inset of a row's text. None: a row's text starts on the same edge as
/// the page's headings, notes and fields.
const ROW_PAD: f32 = 0.0;
/// Room between a switch row's text and its switch.
const ROW_END_PAD: f32 = 12.0;

/// Width of a field that holds a number, wide enough for its hint.
const NUMBER_FIELD_WIDTH: f32 = 180.0;

fn input(
    ws: &Workspace,
    field: MachineText,
    name: &'static str,
    value: String,
    width: f32,
) -> TextInput {
    let typed = ws.clone();
    // The section heading names the field, so its name shows only while
    // it is empty and the field keeps the height of the buttons beside it.
    TextInput::new(value)
        .text_size(INPUT_TEXT_SIZE)
        .hint(name)
        .height(INPUT_HEIGHT)
        .width(width)
        .on_input(move |value| typed.set_machine_text(field, value))
}

/// `field` with its hint in the dimmer placeholder color, so an empty field
/// does not read as filled in.
fn hinted(scheme: &Scheme, field: impl View + 'static) -> ThemeProvider {
    ThemeProvider::new(scheme.field_theme(), field)
}

pub fn remote_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (
        files,
        form_address,
        key_line,
        typing_key,
        typing_address,
        focus,
        probe,
        serving,
        listen,
        family,
        tr,
    ) = {
        let inner = ws.borrow();
        let ui = &inner.settings_ui.machines;
        (
            ui.files.clone(),
            ui.form.address.clone(),
            ui.form.key_line.clone(),
            ui.form.typing_key,
            ui.form.typing_address,
            inner.regions.settings_focus,
            ui.probe.get(),
            inner.serving.as_ref().map(|s| s.bound),
            inner.settings.remote.listen.clone(),
            inner.settings.font_family.clone(),
            inner.tr,
        )
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);

    // This machine, with the steps that pair two machines first.
    column = column.child(section_title(tr.this_machine));
    column = column.child(note(scheme, tr.pairing_steps));
    let fingerprint = files
        .own_key
        .as_ref()
        .map(|k| grouped(&k.fingerprint()))
        .unwrap_or_default();
    let copy = ws.clone();
    // The fingerprint is set in the terminal font, where 0 and O or 1, l
    // and I differ, because people compare it by eye to pair machines.
    column = column.child(
        Row::new()
            .width(page_width())
            .align(Align::Center)
            .child(
                Pad::insets(EdgeInsets {
                    left: ROW_PAD,
                    ..EdgeInsets::default()
                })
                .child(
                    Column::new()
                        .width(page_width() - BUTTON_WIDTH * 1.4 - ROW_PAD)
                        .gap(4.0)
                        .child(Text::new(tr.your_key).size(13.0))
                        .child(mono_line(
                            scheme,
                            &fingerprint,
                            family.as_deref(),
                            (tr.your_key_named)(&fingerprint),
                        )),
                ),
            )
            .child(
                button(tr.copy_key_short, tr.copy_key_short)
                    .width(BUTTON_WIDTH * 1.4)
                    .on_click(move || copy.copy_own_key()),
            ),
    );
    let serve = ws.clone();
    // 0.0.0.0 is no address anyone dials; it means every interface.
    let shown = |address: &str| match address.rsplit_once(':') {
        Some(("0.0.0.0" | "[::]" | "::", port)) => (tr.any_address)(port),
        _ => address.to_owned(),
    };
    let status = match serving {
        Some(bound) => (tr.serving_on)(&shown(&bound.to_string())),
        None => (tr.serve_on)(&shown(&listen)),
    };
    column = column.child(switch(
        scheme,
        tr.serve_machine,
        status,
        serving.is_some(),
        move |on| serve.set_serving(on),
    ));
    // What connecting machines reach does nothing while nobody may
    // connect, so the section shows only once serving is on.
    if serving.is_some() {
        column = column.child(
            Pad::insets(EdgeInsets {
                top: GAP / 2.0,
                ..EdgeInsets::default()
            })
            .child(section_title(tr.when_machines_connect)),
        );
        let offer = ws.clone();
        let vms_note = if files.config.firecracker.is_some() {
            tr.offer_host_note
        } else {
            tr.offer_host_no_vms
        };
        column = column.child(switch(
            scheme,
            tr.offer_host,
            vms_note.to_owned(),
            files.config.offer_host,
            move |on| offer.set_offer_host(on),
        ));
        let share = ws.clone();
        column = column.child(switch(
            scheme,
            tr.share_projects,
            tr.share_projects_note.to_owned(),
            files.config.offer_host && files.config.share_projects,
            move |on| share.set_share_projects(on),
        ));
        let web_links = ws.clone();
        column = column.child(switch(
            scheme,
            tr.web_links_vm_only,
            tr.web_links_vm_only_note.to_owned(),
            files.config.web_links_vm_only,
            move |on| web_links.set_web_links_vm_only(on),
        ));
    }
    if let Some(error) = &files.config_error {
        column = column.child(label(scheme, (tr.error)(&truncate(error, 70))));
    }

    // Keys that may connect to this machine.
    column = column.child(section_title(tr.allowed_keys));
    // Keys can be allowed ahead of time, but they do nothing until the
    // switch above is on, so the section says so.
    if serving.is_none() {
        column = column.child(label(scheme, tr.allowed_keys_not_serving));
    }
    if files.allowed.is_empty() {
        column = column.child(label(scheme, tr.no_allowed_keys));
    }
    for entry in &files.allowed {
        let title = if entry.comment.is_empty() {
            entry.key.short_id()
        } else {
            entry.comment.clone()
        };
        let options = entry.options;
        let host = files.config.host_for(&options);
        let projects = files.config.projects_for(&options);
        let mut subtitle = entry.key.fingerprint();
        if options != KeyOptions::default() {
            subtitle = format!("{} · {subtitle}", tr.key_own_settings);
        }
        if !host {
            subtitle = format!("{} · {subtitle}", tr.vms_only);
        }
        let remove = ws.clone();
        let key = entry.key.clone();
        column = column.child(item(
            title.clone(),
            subtitle,
            vec![Box::new(
                quiet_button((tr.remove_named)(&title), tr.remove)
                    .width(BUTTON_WIDTH)
                    .on_click(move || remove.remove_allowed(&key)),
            )],
            BUTTON_WIDTH,
        ));
        column = column.child(key_options(ws, scheme, entry, &title, host, projects));
    }
    let (allow, only) = (ws.clone(), ws.clone());
    // Two buttons say what the key gets, so no switch holds a choice for a
    // key not yet pasted.
    let allow_width = super::text_button_width(tr.allow_vms_only, 13.0).max(BUTTON_WIDTH * 1.4);
    let allow_all_width = super::text_button_width(tr.allow, 13.0).max(BUTTON_WIDTH * 2.0);
    let no_key = key_line.trim().is_empty();
    column = column.child(
        Row::new()
            .width(page_width())
            .align(Align::Center)
            .gap(GAP / 2.0)
            .child({
                let field = input(
                    ws,
                    MachineText::KeyLine,
                    tr.key_to_allow,
                    key_line,
                    page_width() - allow_all_width - allow_width - GAP,
                );
                // Typing rebuilds the page when the Allow buttons turn on
                // or off, and the field keeps focus through it.
                let view: Box<dyn View> = if typing_key {
                    Box::new(hinted(scheme, field.with_ref(focus)))
                } else {
                    Box::new(hinted(scheme, field))
                };
                view
            })
            // Without a key the buttons are disabled; the note under the
            // row says to paste a key first.
            .child(
                button(tr.allow, tr.allow)
                    .disabled(no_key)
                    .width(allow_all_width)
                    .height(INPUT_HEIGHT)
                    .on_click(move || {
                        allow.set_allow_vm_only(false);
                        allow.allow_key();
                    }),
            )
            .child(
                button(tr.allow_vms_only, tr.allow_vms_only)
                    .appearance(ButtonAppearance::Outline)
                    .disabled(no_key)
                    .width(allow_width)
                    .height(INPUT_HEIGHT)
                    .on_click(move || {
                        only.set_allow_vm_only(true);
                        only.allow_key();
                        only.set_allow_vm_only(false);
                    }),
            ),
    );
    // With no key yet, the line above the field already says to paste one.
    if no_key && !files.allowed.is_empty() {
        column = column.child(note(scheme, tr.paste_key_first));
    }
    column = column.child(note(scheme, tr.vms_only_note));

    // Machines this one connects to.
    column = column.child(section_title(tr.known_machines));
    if files.known_hosts.is_empty() {
        column = column.child(label(scheme, tr.no_known_machines));
    }
    for (address, key) in &files.known_hosts {
        let open = ws.clone();
        let forget = ws.clone();
        let fuse = ws.clone();
        let location = format!("cmdterm://{address}/~/");
        let target = address.clone();
        let fused = address.clone();
        let mut subtitle = key.fingerprint();
        if ws.fused_unreachable(address) {
            subtitle = format!("{} · {subtitle}", tr.fused_unreachable);
        }
        column = column.child(item(
            address.clone(),
            subtitle,
            vec![
                Box::new(Named(
                    (tr.fuse_named)(address).into(),
                    ThemeProvider::new(
                        scheme.switch_theme(),
                        Switch::new(ws.is_fused(address))
                            .on_toggle(move |on| fuse.set_fused(&fused, on)),
                    ),
                )),
                Box::new(
                    button((tr.open_on)(address), tr.open)
                        .width(BUTTON_WIDTH)
                        .on_click(move || open.open_project_at(&location)),
                ),
                Box::new(
                    quiet_button((tr.forget_named)(address), tr.forget)
                        .width(BUTTON_WIDTH)
                        .on_click(move || forget.forget_machine(&target)),
                ),
            ],
            BUTTON_WIDTH * 2.0 + 44.0,
        ));
    }
    let connect = ws.clone();
    let no_address = form_address.trim().is_empty();
    let fetching = matches!(probe, Probe::Fetching(_));
    column = column.child(
        Row::new()
            .width(page_width())
            .align(Align::Center)
            .gap(GAP / 2.0)
            .child({
                let field = input(
                    ws,
                    MachineText::Address,
                    tr.machine_address,
                    form_address,
                    page_width() - BUTTON_WIDTH - GAP / 2.0,
                );
                // Enter adds the machine, like the Add button beside it.
                let enter = connect.clone();
                let field = if fetching {
                    field
                } else {
                    field.on_submit(move || enter.probe_machine())
                };
                let view: Box<dyn View> = if typing_address {
                    Box::new(hinted(scheme, field.with_ref(focus)))
                } else {
                    Box::new(hinted(scheme, field))
                };
                view
            })
            .child(
                button(tr.add_machine, tr.add)
                    .width(BUTTON_WIDTH)
                    .height(INPUT_HEIGHT)
                    .disabled(no_address || fetching)
                    .on_click(move || connect.probe_machine()),
            ),
    );
    column = column.child(probe_view(ws, scheme, probe));
    column = column.child(label(scheme, tr.remote_note));
    page(tr.section_remote, column)
}

/// The switches under an allowed key: whether it gets the host's shell and
/// this machine's projects. They show what the key gets now; flipping one
/// gives the key a setting of its own, which "Defaults" takes back.
fn key_options(
    ws: &Workspace,
    scheme: &Scheme,
    entry: &AuthorizedKey,
    title: &str,
    host: bool,
    projects: bool,
) -> Row {
    let tr = ws.tr();
    let set_host = ws.clone();
    let set_projects = ws.clone();
    let host_key = entry.key.clone();
    let projects_key = entry.key.clone();
    let mut row = Row::new()
        .width(page_width())
        .align(Align::Center)
        .gap(GAP)
        .child(Named(
            (tr.key_host_named)(title).into(),
            ThemeProvider::new(
                scheme.switch_theme(),
                Switch::new(host).on_toggle(move |on| set_host.set_key_host(&host_key, on)),
            ),
        ))
        .child(label(scheme, tr.key_host))
        .child(Named(
            (tr.key_projects_named)(title).into(),
            ThemeProvider::new(
                scheme.switch_theme(),
                Switch::new(projects)
                    .on_toggle(move |on| set_projects.set_key_projects(&projects_key, on)),
            ),
        ))
        .child(label(scheme, tr.key_projects));
    if entry.options != KeyOptions::default() {
        let reset = ws.clone();
        let key = entry.key.clone();
        row = row.child(
            button((tr.key_defaults_named)(title), tr.key_defaults)
                .width(BUTTON_WIDTH)
                .on_click(move || reset.reset_key_options(&key)),
        );
    }
    row
}

/// What fetching a server's key found, with the buttons that act on it.
fn probe_view(ws: &Workspace, scheme: &Scheme, probe: Probe) -> Box<dyn View> {
    let tr = ws.tr();
    let dismiss = ws.clone();
    let cancel = move |text: &'static str| {
        button(text, text)
            .width(BUTTON_WIDTH)
            .on_click(move || dismiss.dismiss_probe())
    };
    match probe {
        Probe::Idle => Box::new(Column::new()),
        Probe::Fetching(address) => Box::new(label(scheme, (tr.fetching_key)(&address))),
        Probe::Unknown { address, key } => {
            let trust = ws.clone();
            Box::new(
                Column::new()
                    .width(page_width())
                    .gap(GAP / 2.0)
                    .child(label(scheme, (tr.compare_key)(&address)))
                    .child(label(scheme, key.fingerprint()))
                    .child(
                        Row::new()
                            .gap(GAP)
                            .child(
                                button((tr.trust_named)(&address), tr.trust)
                                    .width(BUTTON_WIDTH)
                                    .on_click(move || trust.trust_probed()),
                            )
                            .child(cancel(tr.cancel)),
                    ),
            )
        }
        Probe::Trusted(address) => Box::new(
            Row::new()
                .width(page_width())
                .align(Align::Center)
                .gap(GAP)
                .child(label(scheme, (tr.already_trusted)(&address)))
                .child(cancel(tr.ok)),
        ),
        Probe::Changed {
            address,
            fingerprint,
        } => Box::new(
            Column::new()
                .width(page_width())
                .gap(GAP / 2.0)
                .child(label(scheme, (tr.key_changed)(&address)))
                .child(label(scheme, fingerprint))
                .child(cancel(tr.ok)),
        ),
        Probe::Failed { address, error } => {
            // The address is in the field above, so the error leaves it out.
            // The client's error names it, in one of two forms.
            let long = format!("could not connect to {address}: ");
            let short = format!("{address}: ");
            let error = error
                .strip_prefix(&long)
                .or_else(|| error.strip_prefix(&short))
                .unwrap_or(&error);
            // "Connection refused (os error 111)": the code says nothing
            // the words do not.
            let error = match error.rfind(" (os error ") {
                Some(at) if error.ends_with(')') => &error[..at],
                _ => error,
            };
            Box::new(
                Column::new()
                    .width(page_width())
                    .gap(GAP / 2.0)
                    // In the danger color, so it reads as a failure and
                    // not as one more note.
                    .child(ThemeProvider::new(
                        schnellui::widgets::Theme {
                            text: scheme.lanes[3],
                            ..scheme.theme()
                        },
                        Text::new((tr.connect_failed)(&truncate(error, 80)))
                            .size(13.0)
                            .wrap(WrapMode::Word),
                    ))
                    .child(cancel(tr.dismiss)),
            )
        }
    }
}

/// The VPN networks page: per VPN this machine's address with a button to
/// serve it there, and the peers with buttons to open a project on them.
pub fn vpn_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (scan, known_hosts, probe, serving, tr) = {
        let inner = ws.borrow();
        let ui = &inner.settings_ui.machines;
        (
            ui.vpn.get(),
            ui.files.known_hosts.clone(),
            ui.probe.get(),
            inner.serving.as_ref().map(|s| s.bound),
            inner.tr,
        )
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);
    let rescan = ws.clone();
    let status = if scan.scanning { tr.vpn_scanning } else { "" };
    // A heading like the other pages', with the scan beside it.
    column = column.child(
        Row::new()
            .width(page_width())
            .justify(Justify::SpaceBetween)
            .align(Align::End)
            .child(section_title(tr.section_vpn))
            .child(
                // While a scan runs the button says so, rather than
                // only greying out.
                button(
                    tr.vpn_scan,
                    if scan.scanning {
                        tr.vpn_scan_running
                    } else {
                        tr.vpn_scan_again
                    },
                )
                .width(BUTTON_WIDTH * 1.5)
                .disabled(scan.scanning)
                .on_click(move || rescan.refresh_vpn()),
            ),
    );
    if !status.is_empty() {
        column = column.child(label(scheme, status));
    }
    let mut missing = Vec::new();
    for network in &scan.networks {
        let name = network.vpn.name();
        match &network.state {
            State::Missing => {
                missing.push(name);
                continue;
            }
            State::Down(why) => {
                column = column.child(label(scheme, (tr.vpn_down)(name, &truncate(why, 60))));
                continue;
            }
            State::Up => {}
        }
        column = column.child(label(scheme, name));
        if let Some(this) = &network.this {
            column = column.child(this_machine_row(ws, this, serving));
        }
        if network.peers.is_empty() {
            column = column.child(label(scheme, tr.vpn_no_peers));
        }
        for peer in &network.peers {
            column = column.child(peer_row(ws, network.vpn, peer, &known_hosts));
        }
        if network.vpn == Vpn::Nebula {
            column = column.child(note(scheme, tr.vpn_nebula_note));
        }
    }
    if !missing.is_empty() {
        column = column.child(label(scheme, (tr.vpn_missing)(&missing.join(", "))));
    }
    column = column.child(probe_view(ws, scheme, probe));
    column = column.child(note(scheme, tr.vpn_note));
    page(tr.section_vpn, column)
}

/// This machine on a VPN, with a button that serves it on that address.
fn this_machine_row(ws: &Workspace, this: &Peer, serving: Option<SocketAddr>) -> Row {
    let tr = ws.tr();
    let addresses: Vec<String> = this.ips.iter().map(ToString::to_string).collect();
    let ip = this.ips.iter().find(|ip| ip.is_ipv4()).or(this.ips.first());
    let served_here = serving.is_some_and(|bound| this.ips.contains(&bound.ip()));
    let subtitle = match serving {
        Some(bound) if served_here => (tr.serving_on)(&bound.to_string()),
        _ => addresses.join(", "),
    };
    let mut controls: Vec<Box<dyn View>> = Vec::new();
    if let Some(&ip) = ip {
        let serve = ws.clone();
        controls.push(Box::new(
            button((tr.vpn_serve_named)(&ip.to_string()), tr.vpn_serve)
                .width(BUTTON_WIDTH)
                .disabled(served_here)
                .on_click(move || serve.serve_on_vpn(ip)),
        ));
    }
    item(
        (tr.vpn_this_machine)(&this.name),
        subtitle,
        controls,
        BUTTON_WIDTH,
    )
}

/// A peer with its SSH button and a Remote button, which trusts the
/// peer's key first when it is not trusted yet.
fn peer_row(ws: &Workspace, vpn: Vpn, peer: &Peer, known_hosts: &[(String, PublicKey)]) -> Row {
    let tr = ws.tr();
    let address = peer.address().unwrap_or_default();
    let mut subtitle = address.clone();
    match peer.online {
        Some(true) => subtitle = format!("{subtitle} · {}", tr.online),
        Some(false) => subtitle = format!("{subtitle} · {}", tr.offline),
        None => {}
    }
    if let Some(os) = &peer.os {
        subtitle = format!("{subtitle} · {os}");
    }
    let mut controls: Vec<Box<dyn View>> = Vec::new();
    if let Some(name) = peer.location_name() {
        let open = ws.clone();
        let location = format!("{}://{name}/~/", vpn.scheme());
        controls.push(Box::new(
            button((tr.ssh_named)(&peer.name), tr.ssh)
                .width(BUTTON_WIDTH)
                .on_click(move || open.open_project_at(&location)),
        ));
        let remote_host = if address.contains(':') {
            format!("[{address}]")
        } else {
            address.clone()
        };
        let target = cmdterm_env::RemoteTarget::new(&remote_host).address;
        let remote: Box<dyn View> = if known_hosts.iter().any(|(known, _)| *known == target) {
            let open = ws.clone();
            let location = format!("cmdterm://{target}/~/");
            Box::new(
                button((tr.remote_named)(&peer.name), tr.remote)
                    .width(BUTTON_WIDTH)
                    .on_click(move || open.open_project_at(&location)),
            )
        } else {
            let trust = ws.clone();
            Box::new(
                button((tr.trust_named)(&target), tr.trust)
                    .width(BUTTON_WIDTH)
                    .on_click(move || trust.trust_vpn_peer(&target)),
            )
        };
        controls.push(remote);
    }
    item(peer.name.clone(), subtitle, controls, BUTTON_WIDTH * 2.0)
}

pub fn vm_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (files, form, vm_name, jobs, advanced, armed, tr) = {
        let inner = ws.borrow();
        let ui = &inner.settings_ui.machines;
        (
            ui.files.clone(),
            VmField::ALL.map(|f| ui.form.vm_text(f).to_owned()),
            ui.form.vm_name.clone(),
            ui.jobs.get(),
            ui.advanced,
            ui.armed,
            inner.tr,
        )
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);
    if !files.kvm {
        column = column.child(label(scheme, tr.no_kvm));
    }

    // Ready-made images.
    column = column.child(section_title(tr.ready_made_vms));
    for preset in &image::PRESETS {
        let built = files.built.contains(&preset.name);
        let job = jobs.get(preset.name);
        let running = matches!(job, Some(ImageJob::Running(_)));
        let armed = armed == Some(preset.name) && !running;
        let status = match job {
            Some(ImageJob::Running(step)) => step.clone(),
            Some(ImageJob::Failed(error)) => (tr.failed_with)(error),
            _ if built => (tr.image_ready)(preset.summary),
            _ => preset.summary.to_owned(),
        };
        let build = ws.clone();
        let open = ws.clone();
        let location = format!("vm://{}/~/", preset.name);
        let name = preset.name;
        // The name stays while the label turns to Start, so focus stays on
        // the button between the two presses.
        let (action, action_name) = if built {
            (tr.rebuild, (tr.rebuild_image)(preset.title))
        } else {
            (tr.set_up, (tr.set_up_image)(preset.title))
        };
        let action = if armed { tr.start_set_up } else { action };
        // An image that is not built yet has nothing to open, so its row
        // only offers to set it up, in the column where Open goes.
        let start = button(action_name, action)
            .width(BUTTON_WIDTH)
            .disabled(running || !files.missing_tools.is_empty())
            .on_click(move || build.arm_or_build_image(name));
        // Armed, Start is the committing action and Cancel the way back.
        let start = if armed {
            start.appearance(ButtonAppearance::Solid)
        } else {
            start
        };
        let mut controls: Vec<Box<dyn View>> = vec![Box::new(start)];
        if armed {
            let disarm = ws.clone();
            controls.push(Box::new(
                button((tr.cancel_set_up)(preset.title), tr.cancel)
                    .width(BUTTON_WIDTH)
                    .on_click(move || disarm.disarm_image()),
            ));
        } else if built {
            controls.push(Box::new(
                button((tr.open_on)(preset.name), tr.open)
                    .width(BUTTON_WIDTH)
                    .disabled(running || !files.kvm)
                    .on_click(move || open.open_project_at(&location)),
            ));
        }
        let width = BUTTON_WIDTH * controls.len() as f32;
        column = column.child(item(preset.title.to_owned(), status, controls, width));
        // What the second press does, in full under the row, which would
        // cut a sentence this long.
        if armed {
            column = column.child(label(scheme, tr.set_up_explained));
        }
    }
    if !files.missing_tools.is_empty() {
        column = column.child(label(
            scheme,
            (tr.install_first)(&files.missing_tools.join(", ")),
        ));
    }
    column = column.child(label(
        scheme,
        if files.user_network {
            tr.vms_have_network
        } else {
            tr.vms_without_network
        },
    ));

    // Local VMs.
    if let Some(fc) = &files.config.firecracker {
        column = column.child(section_title(tr.local_vms));
        if files.vms.is_empty() {
            column = column.child(label(scheme, tr.no_local_vms));
        }
        for vm in &files.vms {
            let open = ws.clone();
            let delete = ws.clone();
            let location = format!("vm://{}/~/", vm.name);
            let name = vm.name.clone();
            let mut status = if vm.running { tr.running } else { tr.stopped }.to_owned();
            if let Some(image) = &vm.image {
                status = format!("{status} · {image}");
            }
            column = column.child(item(
                vm.name.clone(),
                status,
                vec![
                    Box::new(
                        button((tr.open_on)(&vm.name), tr.open)
                            .width(BUTTON_WIDTH)
                            .on_click(move || open.open_project_at(&location)),
                    ),
                    Box::new(
                        button((tr.delete_disk_of)(&vm.name), tr.reset)
                            .width(BUTTON_WIDTH)
                            .disabled(vm.running)
                            .tooltip(tr.reset_note)
                            .on_click(move || delete.delete_local_vm(&name)),
                    ),
                ],
                BUTTON_WIDTH * 2.0,
            ));
        }
        let open = ws.clone();
        let enter = ws.clone();
        column = column.child(
            Row::new()
                .width(page_width())
                .align(Align::Center)
                .gap(GAP)
                .child(hinted(
                    scheme,
                    input(
                        ws,
                        MachineText::VmName,
                        tr.vm_name,
                        vm_name,
                        page_width() - BUTTON_WIDTH - GAP,
                    )
                    .on_submit(move || enter.open_new_vm()),
                ))
                .child(
                    button(tr.open_vm, tr.open)
                        .width(BUTTON_WIDTH)
                        .on_click(move || open.open_new_vm()),
                ),
        );
        column = column.child(label(scheme, tr.vm_name_note));
        let stop = ws.clone();
        let any_running = files.vms.iter().any(|vm| vm.running);
        column = column.child(
            button(tr.stop_vms, tr.stop_vms)
                .disabled(!any_running)
                .on_click(move || stop.stop_local_vms()),
        );
        column = column.child(label(
            scheme,
            (tr.vm_disks_in)(&truncate(&fc.state_dir.display().to_string(), 60)),
        ));
    }

    // The firecracker section of server.json, for images made by hand.
    let toggle = ws.clone();
    // A disclosure, as the host groups are: the arrow says whether the
    // fields show, and the label is the name in both states, so the
    // settings rebuild gives focus back to it and a voice command that
    // says what it reads reaches it.
    let shown = format!(
        "{} {}",
        if advanced { "▾" } else { "▸" },
        tr.advanced_settings
    );
    column = column.child(
        button(tr.advanced_settings, tr.advanced_settings)
            .display(shown)
            .on_click(move || toggle.toggle_advanced_vm_settings()),
    );
    if advanced {
        // Each field keeps its name beside it: a hint alone leaves a filled
        // field a bare number.
        let name_width = 210.0;
        for (field, value) in VmField::ALL.into_iter().zip(form) {
            column = column.child(
                Row::new()
                    .width(page_width())
                    .gap(GAP / 2.0)
                    .align(Align::Center)
                    .child(
                        Row::new()
                            .width(name_width)
                            .child(Text::new(tr.vm_field(field)).size(13.0)),
                    )
                    // The name beside the field is its accessible name too;
                    // its hint says what goes in it.
                    .child(hinted(
                        scheme,
                        super::Named(
                            tr.vm_field(field).into(),
                            input(
                                ws,
                                MachineText::Vm(field),
                                tr.vm_field_hints[field as usize],
                                value,
                                // A number needs no path's width.
                                if matches!(
                                    field,
                                    crate::machines::VmField::Vcpus
                                        | crate::machines::VmField::MemMib
                                        | crate::machines::VmField::IdleSecs
                                ) {
                                    NUMBER_FIELD_WIDTH
                                } else {
                                    page_width() - name_width - GAP / 2.0
                                },
                            ),
                        ),
                    )),
            );
        }
        // The notes say what the fields do, so they come before the button
        // that saves them.
        for line in tr.vm_notes {
            column = column.child(label(scheme, *line));
        }
        let save = ws.clone();
        column = column.child(
            button(tr.save_vm_settings, tr.save_vm_settings)
                .on_click(move || save.save_vm_settings()),
        );
    }
    page(tr.section_vms, column)
}

/// "Changes apply to terminals started afterwards", and while the active
/// worktree runs in a sandbox, the button that starts its terminals again
/// so they pick the changes up.
fn apply_note(ws: &Workspace, scheme: &Scheme) -> Box<dyn View> {
    // The restart button shows once a sandbox setting changed, so it is
    // never offered when restarting would change nothing.
    let (sandboxed, tr) = {
        let inner = ws.borrow();
        let changed = inner
            .settings_ui
            .sandbox_applied
            .as_ref()
            .is_some_and(|applied| *applied != inner.settings.sandbox);
        (
            changed && inner.active_key().is_some() && inner.active_runtime() == Runtime::Sandbox,
            inner.tr,
        )
    };
    // Room above, so the note reads as about the whole page and not as
    // part of the last group.
    let spaced = |view: Box<dyn View>| -> Box<dyn View> {
        Box::new(
            Pad::insets(EdgeInsets {
                top: GAP / 2.0,
                ..EdgeInsets::default()
            })
            .child(view),
        )
    };
    if !sandboxed {
        return spaced(Box::new(label(scheme, tr.sandbox_note)));
    }
    let restart = ws.clone();
    spaced(Box::new(
        Row::new()
            .width(page_width())
            .justify(Justify::SpaceBetween)
            .align(Align::Center)
            .child(label(scheme, tr.sandbox_note))
            .child(
                button(tr.restart_terminals, tr.restart_terminals)
                    .tooltip(tr.restart_terminals_hint)
                    .height(28.0)
                    .on_click(move || restart.restart_active_terminals()),
            ),
    ))
}

/// The heading of a scope, such as "Every worktree": a small capitalized
/// muted label, like the nav's group names, so it reads as a divider above
/// the section titles under it instead of one more title among them.
fn scope_title(scheme: &Scheme, text: &str) -> impl View + use<> {
    // A muted capital label over a rule across the page, as the nav names
    // its groups: it splits the page into the part that applies to one
    // worktree and the part for every one, above the bold section titles
    // inside each, so the scope reads as the outer level.
    let width = page_width();
    Pad::insets(EdgeInsets {
        top: GAP * 1.5,
        bottom: 2.0,
        ..EdgeInsets::default()
    })
    .child(
        Column::new()
            .width(width)
            .gap(4.0)
            .child(ThemeProvider::new(
                scheme.muted_theme(),
                Text::new(text.to_uppercase()).size(12.0).bold(),
            ))
            .child(Fill::new(width, 1.0, scheme.line)),
    )
}

/// The Sandboxing page: where the active worktree's terminals run, the
/// devcontainer engine and the sandbox's network. Its paths and hosts have
/// their own pages.
pub fn sandbox_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (engine, sandbox, active, branch, runtime, preparing, picker, focus, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.container_engine.clone(),
            inner.settings.sandbox.clone(),
            inner.active_key().is_some(),
            inner.active_branch().map(str::to_owned),
            inner.active_runtime(),
            inner.active_runtime_preparing(),
            inner.settings_ui.picker,
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);

    // Where the active worktree's terminals run. Two scope headings, a
    // step above the group titles, split the page into what applies to this
    // worktree and what applies to all of them.
    column = column.child(scope_title(
        scheme,
        &match &branch {
            Some(branch) => (tr.terminals_of_branch)(branch),
            None => tr.this_worktree.to_owned(),
        },
    ));
    if active {
        let open = picker == Some(Picker::Runtime);
        let toggle = ws.clone();
        let mut dropdown = Dropdown::new(tr.this_worktree)
            .min_width(SELECT_WIDTH)
            .text_size(INPUT_TEXT_SIZE)
            .open(open)
            .on_toggle(move || toggle.toggle_picker(Picker::Runtime));
        for (choice, title) in [
            (Runtime::Host, tr.run_on_host),
            (Runtime::Devcontainer, tr.run_in_devcontainer),
            (Runtime::Sandbox, tr.run_in_sandbox),
        ] {
            let pick = ws.clone();
            dropdown = dropdown.option(
                DropdownOption::new(title)
                    .selected(runtime == choice)
                    .on_select(move || pick.pick_runtime(choice)),
            );
        }
        column = column.child(focused(scheme, dropdown, open, focus));
        if cfg!(windows) && runtime == Runtime::Sandbox {
            column = column.child(label(scheme, tr.setup_sandbox_note_windows));
        }
        if runtime == Runtime::Devcontainer && preparing {
            column = column.child(label(scheme, tr.runtime_starting));
        }
        // Only a devcontainer has anything to rebuild.
        if runtime == Runtime::Devcontainer {
            let rebuild = ws.clone();
            column = column.child(
                button(tr.rebuild_devcontainer, tr.rebuild_devcontainer)
                    .on_click(move || rebuild.rebuild_devcontainer_from_settings()),
            );
        }
    } else {
        column = column.child(label(scheme, tr.no_active_worktree));
    }

    // The rest applies to every worktree; its scope heading says so.
    column = column.child(scope_title(scheme, tr.all_worktrees));
    // Devcontainer engine. An engine settings.json names other than docker
    // or podman shows as its own option.
    column = column.child(section_title(tr.container_engine));
    let open = picker == Some(Picker::ContainerEngine);
    let toggle = ws.clone();
    let mut dropdown = Dropdown::new(tr.container_engine)
        .min_width(SELECT_WIDTH)
        .text_size(INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::ContainerEngine));
    let custom = engine
        .clone()
        .filter(|e| !matches!(e.as_str(), "docker" | "podman"));
    for (choice, title) in [
        (None, Cow::Borrowed(tr.automatic)),
        (Some("docker".to_owned()), Cow::Borrowed("docker")),
        (Some("podman".to_owned()), Cow::Borrowed("podman")),
    ]
    .into_iter()
    .chain(custom.map(|c| (Some(c.clone()), Cow::Owned(c))))
    {
        let pick = ws.clone();
        let selected = engine == choice;
        dropdown = dropdown.option(
            DropdownOption::new(title)
                .selected(selected)
                .on_select(move || pick.pick_container_engine(choice.as_deref())),
        );
    }
    column = column.child(focused(scheme, dropdown, open, focus));

    // Sandbox.
    column = column.child(section_title(tr.section_sandbox));
    let network = ws.clone();
    column = column.child(switch(
        scheme,
        tr.sandbox_network,
        tr.sandbox_network_note.to_owned(),
        sandbox.network,
        move |on| network.set_sandbox_network(on),
    ));
    if sandbox.network {
        let restrict = ws.clone();
        column = column.child(sub_switch(
            scheme,
            tr.sandbox_restrict,
            tr.sandbox_restrict_note.to_owned(),
            sandbox.restrict_network,
            move |on| restrict.set_sandbox_restrict_network(on),
        ));
    }

    if sandbox.network && !sandbox.restrict_network {
        let host = ws.clone();
        column = column.child(sub_switch(
            scheme,
            tr.sandbox_host_network,
            tr.sandbox_host_network_note.to_owned(),
            sandbox.host_network,
            move |on| host.set_sandbox_host_network(on),
        ));
    }

    if sandbox.network && sandbox.restrict_network {
        let intercept = ws.clone();
        column = column.child(sub_switch(
            scheme,
            tr.sandbox_intercept_tls,
            tr.sandbox_intercept_tls_note.to_owned(),
            sandbox.intercept_tls,
            move |on| intercept.set_sandbox_intercept_tls(on),
        ));
    }

    // How sandboxes reach container engines.
    column = column.child(section_title(tr.sandbox_engines));
    let open = picker == Some(Picker::SandboxEngines);
    let toggle = ws.clone();
    let mut dropdown = Dropdown::new(tr.sandbox_engines)
        .min_width(SELECT_WIDTH)
        .text_size(INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::SandboxEngines));
    for (choice, title) in [
        (EngineAccess::Passthrough, tr.engines_passthrough),
        (EngineAccess::Sidecar, tr.engines_sidecar),
        (EngineAccess::Off, tr.engines_off),
    ] {
        let pick = ws.clone();
        dropdown = dropdown.option(
            DropdownOption::new(title)
                .selected(sandbox.engines == choice)
                .on_select(move || pick.set_sandbox_engines(choice)),
        );
    }
    column = column.child(focused(scheme, dropdown, open, focus));
    match sandbox.engines {
        // Passthrough lets containers out of the sandbox, so its note is a
        // warning, not a muted aside.
        EngineAccess::Passthrough => {
            // A boxed callout with a warning-colored edge, so it does not
            // read as one more muted note.
            column = column.child(
                schnellui::widgets::Panel::new()
                    .width(page_width())
                    .insets(EdgeInsets {
                        left: 12.0,
                        right: 10.0,
                        top: 8.0,
                        bottom: 8.0,
                    })
                    .surface(scheme.warning_surface())
                    .outline(scheme.lanes[1])
                    .rail(scheme.lanes[1])
                    .child(
                        Row::new()
                            .gap(8.0)
                            .align(Align::Start)
                            .child(
                                schnellui_icons_md::MdIcon::outlined(
                                    "warning",
                                    schnellui_icons_md::outlined::ICON_WARNING,
                                )
                                .size(18.0)
                                .color(scheme.lanes[1]),
                            )
                            .child(ThemeProvider::new(
                                schnellui::widgets::Theme {
                                    text: scheme.ink,
                                    ..scheme.theme()
                                },
                                engine_warning(ws, tr, sandbox.engines_acknowledged),
                            )),
                    ),
            );
        }
        EngineAccess::Sidecar => {
            column = column.child(label(scheme, tr.sandbox_sidecar_note));
            column = column.child(section_title(tr.sidecar_engine));
            let open = picker == Some(Picker::SidecarEngine);
            let toggle = ws.clone();
            let mut dropdown = Dropdown::new(tr.sidecar_engine)
                .min_width(SELECT_WIDTH)
                .text_size(INPUT_TEXT_SIZE)
                .open(open)
                .on_toggle(move || toggle.toggle_picker(Picker::SidecarEngine));
            for (choice, title) in [
                (SidecarEngine::Podman, tr.sidecar_podman),
                (SidecarEngine::Docker, tr.sidecar_docker),
            ] {
                let pick = ws.clone();
                dropdown = dropdown.option(
                    DropdownOption::new(title)
                        .selected(sandbox.sidecar.engine == choice)
                        .on_select(move || pick.set_sidecar_engine(choice)),
                );
            }
            column = column.child(focused(scheme, dropdown, open, focus));
        }
        EngineAccess::Off => {}
    }
    column = column.child(apply_note(ws, scheme));
    if !cmdterm_env::SandboxKind::native().limits_reads() {
        column = column.child(label(scheme, tr.sandbox_windows_note));
    }
    page(tr.section_sandboxing, column)
}

/// Whether `host` can be added: a host pattern with a domain, an address or
/// `localhost`, not yet in `hosts`. A bare word is more likely a search.
fn addable(host: &str, hosts: &[String]) -> bool {
    cmdterm_env::valid_host_pattern(host)
        && (host.contains('.') || host.contains(':') || host == "localhost")
        && !hosts.iter().any(|known| known == host)
}

/// The Hosts page under Sandboxing: the hosts a sandbox limited to allowed
/// hosts may reach.
pub fn sandbox_hosts_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (sandbox, host, removed, filtering, flipped, focus, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.sandbox.clone(),
            inner.settings_ui.machines.form.sandbox_host.clone(),
            inner.settings_ui.machines.form.removed_host.clone(),
            inner.settings_ui.machines.form.filtering_hosts,
            inner.settings_ui.machines.form.flipped_host_groups.clone(),
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);
    if !(sandbox.network && sandbox.restrict_network) {
        column = column.child(label(scheme, tr.sandbox_hosts_inactive));
    }
    // What is typed to add a host also narrows the list, so a long list
    // shows whether a host is already in it.
    let needle = host.trim().trim_start_matches("*.").to_ascii_lowercase();
    let shown: Vec<&String> = sandbox
        .allowed_hosts
        .iter()
        .filter(|entry| needle.is_empty() || entry.to_ascii_lowercase().contains(&needle))
        .collect();
    // Grouped as the host setup offers them, then the user's own, each
    // sorted by domain so example.com and *.example.com sit together.
    let mut groups: Vec<(&'static str, Vec<&String>)> = HOST_GROUPS
        .iter()
        .map(|group| {
            let hosts = shown
                .iter()
                .copied()
                .filter(|host| group.hosts.contains(&host.as_str()))
                .collect();
            ((tr.host_group)(group.id), hosts)
        })
        .collect();
    let own: Vec<&String> = shown
        .iter()
        .copied()
        .filter(|host| !HOST_GROUPS.iter().any(|g| g.hosts.contains(&host.as_str())))
        .collect();
    groups.push((tr.hosts_added_by_you, own));
    for (_, hosts) in &mut groups {
        hosts.sort_by_cached_key(|entry| {
            let bare = entry.trim_start_matches("*.");
            (bare.to_ascii_lowercase(), entry.len())
        });
    }
    // The heading holds the shortcut to common hosts; adding one comes
    // next, before a list that can run long.
    let choose = ws.clone();
    column = column.child(
        Row::new()
            .width(page_width())
            .justify(Justify::SpaceBetween)
            .align(Align::End)
            .child(section_title(
                match (shown.len(), sandbox.allowed_hosts.len()) {
                    (_, 0) => tr.sandbox_allowed_hosts.to_owned(),
                    (shown, all) if shown < all => {
                        format!(
                            "{} ({})",
                            tr.sandbox_allowed_hosts,
                            (tr.count_of)(shown, all)
                        )
                    }
                    (_, all) => format!("{} ({all})", tr.sandbox_allowed_hosts),
                },
            ))
            .child(
                button(tr.choose_hosts, tr.choose_hosts_open)
                    .appearance(ButtonAppearance::Outline)
                    .height(28.0)
                    .text_size(12.5)
                    .on_click(move || choose.open_host_setup()),
            ),
    );
    let add = ws.clone();
    let add_width = 120.0;
    let can_add = addable(host.trim(), &sandbox.allowed_hosts);
    // The field spans the page until there is a host to add.
    let field_width = page_width() - if can_add { add_width + GAP / 2.0 } else { 0.0 };
    column = column.child(
        Row::new()
            .width(page_width())
            .gap(GAP / 2.0)
            .align(Align::Center)
            .child({
                let field = input(
                    ws,
                    MachineText::SandboxHost,
                    tr.sandbox_host,
                    host.clone(),
                    field_width,
                );
                // Enter adds the host while Add host shows.
                let enter = add.clone();
                let field = if can_add {
                    field.on_submit(move || enter.add_sandbox_host())
                } else {
                    field
                };
                let view: Box<dyn View> = if filtering {
                    Box::new(hinted(scheme, field.with_ref(focus)))
                } else {
                    Box::new(hinted(scheme, field))
                };
                view
            })
            .child({
                // Typing rebuilds the page, so the button follows the field:
                // it shows only for a host pattern not yet in the list.
                let view: Box<dyn View> = if can_add {
                    Box::new(
                        button(tr.add_host, tr.add_host)
                            .appearance(ButtonAppearance::Solid)
                            .width(add_width)
                            .height(INPUT_HEIGHT)
                            .on_click(move || add.add_sandbox_host()),
                    )
                } else {
                    // Nothing, and no gap before it: the field ends where
                    // the rows below it end.
                    Box::new(schnellui::widgets::Stack::new().size(0.0, INPUT_HEIGHT))
                };
                view
            })
            .gap(if can_add { GAP / 2.0 } else { 0.0 }),
    );
    // A bare word filters only; say so, since the field also adds.
    if !needle.is_empty()
        && !can_add
        && !sandbox
            .allowed_hosts
            .iter()
            .any(|known| known == host.trim())
    {
        column = column.child(label(scheme, tr.sandbox_host_bare));
    }
    if shown.is_empty() && !needle.is_empty() {
        column = column.child(label(scheme, (tr.sandbox_host_none)(host.trim())));
    }
    let rule = super::theme::mix(scheme.rail, scheme.line, 128);
    let own_group = HOST_GROUPS.len();
    for (index, (title, hosts)) in groups.into_iter().enumerate() {
        if hosts.is_empty() {
            continue;
        }
        // The common groups start closed, so the page opens short; the
        // user's own hosts start open. A search opens every group.
        let open = !needle.is_empty() || ((index == own_group) != flipped.contains(&index));
        let toggle = ws.clone();
        // While searching, the count says how many of the group match.
        let total = sandbox
            .allowed_hosts
            .iter()
            .filter(|host| match HOST_GROUPS.get(index) {
                Some(group) => group.hosts.contains(&host.as_str()),
                None => !HOST_GROUPS.iter().any(|g| g.hosts.contains(&host.as_str())),
            })
            .count();
        let heading = if hosts.len() < total {
            format!("{title} ({})", (tr.count_of)(hosts.len(), total))
        } else {
            format!("{title} ({})", hosts.len())
        };
        column = column.child(
            Pad::insets(EdgeInsets {
                top: GAP / 4.0,
                ..EdgeInsets::default()
            })
            .child(Named(
                (if open { tr.hide_named } else { tr.show_named })(&heading).into(),
                // The chevron leads, next to the name it opens.
                super::settings::list_row(format!("{}  {heading}", if open { "▾" } else { "▸" }))
                    .width(page_width())
                    .height(30.0)
                    .on_click(move || toggle.toggle_host_group(index)),
            )),
        );
        if !open {
            continue;
        }
        for host in hosts {
            let remove = ws.clone();
            let target = host.clone();
            // The remove buttons stay at the right edge of the field above,
            // filtered or not, and a faint rule under each row ties the
            // name to its button.
            column = column.child(
                Pad::insets(EdgeInsets {
                    left: ROW_PAD + HOST_INDENT,
                    ..EdgeInsets::default()
                })
                .child(
                    Column::new()
                        .width(host_row_width())
                        .child(
                            Row::new()
                                .width(host_row_width())
                                .height(28.0)
                                .align(Align::Center)
                                .justify(Justify::SpaceBetween)
                                .child(Text::new(host.clone()).size(14.0))
                                .child(
                                    remove_icon((tr.remove_named)(host))
                                        .on_click(move || remove.remove_sandbox_host(&target)),
                                ),
                        )
                        .child(Fill::new(host_row_width(), 1.0, rule)),
                ),
            );
        }
    }
    // A removed host can come back, since one click removes it. The undo
    // row sits under the lists, so no row above moves under the pointer.
    if let Some(removed) = removed.filter(|h| !sandbox.allowed_hosts.contains(h)) {
        let undo = ws.clone();
        column = column.child(item(
            (tr.host_removed)(&removed),
            String::new(),
            vec![Box::new(
                button((tr.undo_remove_named)(&removed), tr.undo)
                    .appearance(ButtonAppearance::Outline)
                    .width(BUTTON_WIDTH)
                    .on_click(move || undo.undo_remove_sandbox_host()),
            )],
            BUTTON_WIDTH,
        ));
    }
    let column = column.child(apply_note(ws, scheme));
    page(tr.section_sandbox_hosts, column)
}

/// The buttons in the passthrough warning that seal the sandbox again: a
/// sidecar engine where there is one, or no engine access at all.
/// The text and choices inside the passthrough warning. Once the user kept
/// passthrough, only the warning stays: the dropdown above holds the other
/// choices, and a box of buttons would ask again what was answered.
fn engine_warning(ws: &Workspace, tr: &'static crate::i18n::Strings, acknowledged: bool) -> Column {
    let column = Column::new().width(page_width() - 48.0).gap(8.0).child(
        Text::new(tr.sandbox_engines_note)
            .size(12.5)
            .wrap(WrapMode::Word),
    );
    if acknowledged {
        return column;
    }
    column
        // The way out sits in the warning itself: what each choice costs,
        // then the choices.
        .child(
            Text::new(tr.engines_fixes_note)
                .size(12.5)
                .wrap(WrapMode::Word),
        )
        .child(engine_fixes(ws, tr))
}

fn engine_fixes(ws: &Workspace, tr: &'static crate::i18n::Strings) -> Row {
    let mut row = Row::new().gap(8.0);
    if cfg!(target_os = "linux") {
        let sidecar = ws.clone();
        row = row.child(
            // Filled, as the recommended way out: it keeps containers
            // working and the sandbox whole.
            button(tr.engines_use_sidecar, tr.engines_use_sidecar)
                .appearance(ButtonAppearance::Solid)
                .height(28.0)
                .on_click(move || sidecar.set_sandbox_engines(EngineAccess::Sidecar)),
        );
    }
    let off = ws.clone();
    row = row.child(
        button(tr.engines_turn_off, tr.engines_turn_off)
            .height(28.0)
            .on_click(move || off.set_sandbox_engines(EngineAccess::Off)),
    );
    // Keeping passthrough on purpose ends the status bar's warning and
    // shrinks this box to its warning.
    let keep = ws.clone();
    row.child(
        button(tr.engines_keep, tr.engines_keep)
            .appearance(ButtonAppearance::Outline)
            .height(28.0)
            .on_click(move || keep.acknowledge_sandbox_engines()),
    )
}

/// Width of a host's row on the Hosts page: it ends where the field above
/// it ends.
fn host_row_width() -> f32 {
    page_width() - ROW_PAD - HOST_INDENT
}
/// How far a group's hosts sit in from its heading.
const HOST_INDENT: f32 = 18.0;

/// The Files page under Sandboxing: paths the sandbox may write besides the
/// worktree, and paths it cannot see.
pub fn sandbox_files_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (sandbox, path, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.sandbox.clone(),
            inner.settings_ui.machines.form.sandbox_path.clone(),
            inner.tr,
        )
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);
    for (writable, title, list) in [
        (true, tr.sandbox_writable, &sandbox.writable),
        (false, tr.sandbox_hidden, &sandbox.hidden),
    ] {
        column = column.child(section_title(title));
        for entry in list {
            let remove = ws.clone();
            let target = entry.clone();
            column = column.child(item(
                entry.clone(),
                String::new(),
                vec![Box::new(remove_icon((tr.remove_named)(entry)).on_click(
                    move || remove.remove_sandbox_path(writable, &target),
                ))],
                REMOVE_WIDTH,
            ));
        }
    }
    let add_writable = ws.clone();
    let add_hidden = ws.clone();
    column = column
        .child(hinted(
            scheme,
            input(
                ws,
                MachineText::SandboxPath,
                tr.sandbox_path,
                path,
                page_width(),
            ),
        ))
        .child(
            Row::new()
                .gap(GAP)
                .child(
                    button(tr.add_writable, tr.add_writable)
                        .on_click(move || add_writable.add_sandbox_path(true)),
                )
                .child(
                    button(tr.add_hidden, tr.add_hidden)
                        .on_click(move || add_hidden.add_sandbox_path(false)),
                ),
        )
        .child(apply_note(ws, scheme));
    if !cmdterm_env::SandboxKind::native().limits_reads() {
        column = column.child(label(scheme, tr.sandbox_windows_note));
    }
    page(tr.section_sandbox_files, column)
}

/// The Project page: settings of the active project, with the folders its
/// sandboxes may use besides the worktree.
/// The active project's Allowed page, or with `blocked` its Blocked page:
/// the guarded requests its sandboxes send or are refused without a
/// question, each of which can be removed or moved to the other page, and a
/// form that adds one. The Allowed page also has the switch for launches.
pub fn project_answers_section(
    ws: &Workspace,
    scheme: &Scheme,
    blocked: bool,
) -> impl View + use<> {
    let inner = ws.borrow();
    let tr = inner.tr;
    let title = if blocked {
        tr.section_project_blocked
    } else {
        tr.section_project_allowed
    };
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);
    let Some(project) = inner.state.active().cloned() else {
        drop(inner);
        column = column.child(note(scheme, tr.project_none));
        return page(title, column);
    };
    let guards = crate::runtime::all_guards(inner.ext.sandbox_rules());
    let form = &inner.settings_ui.machines.form;
    let picked = (form.project_guard.clone())
        .or_else(|| guards.first().map(|g| g.id.clone()))
        .unwrap_or_default();
    let host = form.project_answer_host.clone();
    let open = inner.settings_ui.picker == Some(Picker::ProjectGuard);
    let focus = inner.regions.settings_focus;
    drop(inner);

    let id = project.id;
    column = column
        .child(section_title(project.name.clone()))
        .child(note(
            scheme,
            if blocked {
                tr.project_blocked_note
            } else {
                tr.project_allowed_note
            },
        ));
    if !project.env.is_local() {
        column = column.child(note(scheme, tr.project_answers_remote_note));
    }
    if !blocked {
        let toggle = ws.clone();
        column = column.child(switch(
            scheme,
            tr.project_launches,
            tr.project_launches_note.to_owned(),
            project.allows_launches,
            move |on| toggle.set_project_launches(id, on),
        ));
    }
    column = column.child(section_title(tr.ext_guards_heading));
    let list = if blocked {
        &project.always_blocked
    } else {
        &project.always_allowed
    };
    if list.is_empty() {
        column = column.child(label(scheme, tr.no_project_folders));
    }
    let move_display = if blocked {
        tr.project_answer_to_allowed
    } else {
        tr.project_answer_to_blocked
    };
    let move_width = super::text_button_width(move_display, 13.0).max(BUTTON_WIDTH);
    for (index, entry) in list.iter().enumerate() {
        let guard = (guards.iter())
            .find(|g| g.id == entry.guard)
            .map_or(entry.guard.as_str(), |g| g.title.as_str());
        let named = (tr.always_entry)(guard, &entry.host);
        let (shift, remove) = (ws.clone(), ws.clone());
        column = column.child(item(
            guard.to_owned(),
            entry.host.clone(),
            vec![
                Box::new(
                    quiet_button(format!("{move_display}: {named}"), move_display)
                        .on_click(move || shift.move_project_answer(id, blocked, index)),
                ),
                Box::new(
                    remove_icon((tr.remove_named)(&named))
                        .on_click(move || remove.remove_project_answer(id, blocked, index)),
                ),
            ],
            move_width + CONTROL_GAP + REMOVE_WIDTH,
        ));
    }

    column = column.child(section_title(tr.project_answer_add_heading));
    let toggle = ws.clone();
    let mut dropdown = Dropdown::new(tr.project_answer_guard)
        .min_width(SELECT_WIDTH)
        .text_size(INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::ProjectGuard));
    for guard in &guards {
        let pick = ws.clone();
        let guard_id = guard.id.clone();
        dropdown = dropdown.option(
            DropdownOption::new(guard.title.clone())
                .selected(guard.id == picked)
                .on_select(move || pick.set_project_answer_guard(&guard_id)),
        );
    }
    column = column.child(focused(scheme, dropdown, open, focus));
    let add = ws.clone();
    let add_width = super::text_button_width(tr.project_answer_add, 13.0).max(BUTTON_WIDTH);
    column = column.child(
        Row::new()
            .gap(GAP)
            .align(Align::Center)
            .child(hinted(
                scheme,
                input(
                    ws,
                    MachineText::ProjectAnswerHost,
                    tr.project_answer_host,
                    host,
                    page_width() - add_width - GAP,
                ),
            ))
            .child(
                button(tr.project_answer_add, tr.project_answer_add)
                    .on_click(move || add.add_project_answer(blocked)),
            ),
    );
    page(title, column)
}

pub fn project_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let inner = ws.borrow();
    let tr = inner.tr;
    let mut column = Column::new().width(page_width()).gap(GAP / 2.0);
    let Some(project) = inner.state.active().cloned() else {
        drop(inner);
        column = column
            .child(note(scheme, tr.project_none))
            .child(super::settings::reset_settings(ws, scheme));
        return page(tr.section_project, column);
    };
    let jj = project.uses_jj(inner.settings.jj);
    let runtime = inner
        .active_key()
        .and_then(|(_, path)| project.worktree(&path).map(|w| w.runtime))
        .unwrap_or_default();
    let path = inner.settings_ui.machines.form.project_path.clone();
    let env_name = inner.settings_ui.machines.form.project_env_name.clone();
    let env_value = inner.settings_ui.machines.form.project_env_value.clone();
    let env_error = inner.settings_ui.machines.form.project_env_error.clone();
    let keyring_available = inner.project_env.available();
    let has_locked_env = inner.project_env.has_locked_vars(&project);
    let env_rows: Vec<(String, crate::project_env::VarStorage)> = project
        .env_vars
        .iter()
        .map(|var| {
            (
                var.name.clone(),
                inner.project_env.var_storage(&project, var),
            )
        })
        .collect();
    let base = match &inner.settings_ui.machines.form.project_base {
        Some((id, typed)) if *id == project.id => typed.clone(),
        _ => project.base.clone().unwrap_or_default(),
    };
    drop(inner);

    let place = format!("{}  {}", project.env.label(), project.root.display());
    // The project's name heads the page like the other pages' titles.
    column = column
        .child(section_title(project.name.clone()))
        .child(label(scheme, place))
        // Room between where the project lives and its first setting.
        .child(Fill::new(1.0, 6.0, schnellui::scene::Color::TRANSPARENT));
    // The switches sit together under the project's name, before the
    // titled sections, so none reads as part of the section above it.
    let id = project.id;
    if !project.is_global() {
        let toggle = ws.clone();
        let id = project.id;
        column = column.child(switch(
            scheme,
            tr.project_jj,
            tr.project_jj_note.to_owned(),
            jj,
            move |on| toggle.set_project_jj(id, on),
        ));
        let (typed, set) = (ws.clone(), ws.clone());
        column = column
            .child(section_title(tr.project_base))
            .child(note(scheme, tr.project_base_note))
            .child(
                Row::new()
                    .gap(GAP)
                    .align(Align::Center)
                    .child(super::Named(
                        // The section title above names the field; the hint
                        // says what empty means.
                        tr.project_base.into(),
                        TextInput::new(base)
                            .text_size(INPUT_TEXT_SIZE)
                            .hint(tr.project_base_hint)
                            .height(INPUT_HEIGHT)
                            .width(page_width() - BUTTON_WIDTH - GAP)
                            .on_input(move |value| typed.set_project_base_draft(id, value)),
                    ))
                    .child(
                        button(tr.project_base_set, tr.project_base_set)
                            .width(BUTTON_WIDTH)
                            .on_click(move || set.apply_project_base_draft(id)),
                    ),
            );
    }
    let env_note = if project.temporary {
        tr.project_env_note_temporary
    } else if keyring_available {
        tr.project_env_note_keyring
    } else {
        tr.project_env_note_plain
    };
    column = column
        .child(section_title(tr.project_env))
        .child(note(scheme, env_note));
    if !project.env.is_local() {
        column = column.child(note(scheme, tr.project_env_remote_note));
    }
    if has_locked_env && keyring_available {
        let reload = ws.clone();
        column = column.child(
            button(tr.project_env_reload, tr.project_env_reload)
                .on_click(move || reload.reload_project_env_vars(id)),
        );
    }
    if env_rows.is_empty() {
        column = column.child(label(scheme, tr.no_project_folders));
    } else {
        for (name, storage) in &env_rows {
            let subtitle = match storage {
                crate::project_env::VarStorage::Keyring => tr.project_env_storage_keyring,
                crate::project_env::VarStorage::Loading => tr.project_env_storage_loading,
                crate::project_env::VarStorage::Locked => tr.project_env_storage_locked,
                crate::project_env::VarStorage::Plain if project.temporary => {
                    tr.project_env_storage_session
                }
                crate::project_env::VarStorage::Plain => tr.project_env_storage_plain,
            };
            let remove = ws.clone();
            let target = name.clone();
            column = column.child(item(
                name.clone(),
                subtitle.to_owned(),
                vec![Box::new(remove_icon((tr.remove_named)(name)).on_click(
                    move || remove.remove_project_env_var(id, &target),
                ))],
                REMOVE_WIDTH,
            ));
        }
    }
    let name_width = ((page_width() - BUTTON_WIDTH - GAP * 2.0) * 0.42).floor();
    let value_width = (page_width() - BUTTON_WIDTH - GAP * 2.0 - name_width).max(80.0);
    let (typed_value, add_env) = (ws.clone(), ws.clone());
    column = column.child(
        Row::new()
            .gap(GAP)
            .align(Align::Center)
            .child(hinted(
                scheme,
                input(
                    ws,
                    MachineText::ProjectEnvName,
                    tr.project_env_name,
                    env_name,
                    name_width,
                ),
            ))
            .child(hinted(
                scheme,
                PasswordInput::new(env_value)
                    .text_size(INPUT_TEXT_SIZE)
                    .hint(tr.project_env_value)
                    .height(INPUT_HEIGHT)
                    .width(value_width)
                    .on_input(move |value| {
                        typed_value.set_machine_text(MachineText::ProjectEnvValue, value);
                    }),
            ))
            .child(
                button(tr.project_env_add, tr.project_env_add)
                    .width(BUTTON_WIDTH)
                    .on_click(move || add_env.add_project_env_var()),
            ),
    );
    if let Some(error) = env_error {
        column = column.child(label(scheme, error));
    }

    column = column
        .child(section_title(tr.project_folders))
        .child(note(scheme, tr.project_folders_note));
    if runtime != Runtime::Sandbox {
        let sandbox = ws.clone();
        column = column.child(note(scheme, tr.project_not_sandboxed)).child(
            button(tr.run_in_sandbox, tr.run_in_sandbox)
                .on_click(move || sandbox.pick_runtime(Runtime::Sandbox)),
        );
    }
    if !project.env.is_local() {
        column = column.child(note(scheme, tr.project_remote_note));
    }
    for (writable, title, list) in [
        (true, tr.project_writable, &project.sandbox.writable),
        (false, tr.project_readable, &project.sandbox.readable),
    ] {
        // The list's name in ink, so it reads as a heading over its
        // entries; an empty list says so on the same line.
        let mut heading = Row::new()
            .gap(8.0)
            .align(Align::Center)
            .child(Text::new(title).size(13.0).bold());
        if list.is_empty() {
            heading = heading.child(label(scheme, tr.no_project_folders));
        }
        column = column.child(
            Pad::insets(EdgeInsets {
                top: 4.0,
                ..EdgeInsets::default()
            })
            .child(heading),
        );
        for entry in list {
            let remove = ws.clone();
            let target = entry.clone();
            column = column.child(item(
                entry.clone(),
                String::new(),
                vec![Box::new(remove_icon((tr.remove_named)(entry)).on_click(
                    move || remove.remove_project_sandbox_path(writable, &target),
                ))],
                REMOVE_WIDTH,
            ));
        }
    }
    let add_writable = ws.clone();
    let add_readable = ws.clone();
    // The chooser shows this machine's folders, so remote projects type theirs.
    let path_input = if project.env.is_local() {
        let choose = ws.clone();
        let title = tr.choose_a_folder;
        Box::new(
            Row::new()
                .gap(GAP)
                .align(Align::Center)
                .child(hinted(
                    scheme,
                    input(
                        ws,
                        MachineText::ProjectPath,
                        tr.sandbox_path,
                        path,
                        page_width() - BUTTON_WIDTH - GAP,
                    ),
                ))
                .child(
                    button(tr.choose_folder, tr.open_folder)
                        .width(BUTTON_WIDTH)
                        .tooltip(tr.choose_a_folder)
                        .on_click(move || {
                            choose.choose_folder(title, FolderTarget::ProjectSandbox)
                        }),
                ),
        ) as Box<dyn View>
    } else {
        Box::new(hinted(
            scheme,
            input(
                ws,
                MachineText::ProjectPath,
                tr.sandbox_path,
                path,
                page_width(),
            ),
        ))
    };
    column = column.child(path_input).child(
        Row::new()
            .gap(GAP)
            .child(
                button(tr.add_writable, tr.add_writable)
                    .on_click(move || add_writable.add_project_sandbox_path(true)),
            )
            .child(
                button(tr.add_readable, tr.add_readable)
                    .on_click(move || add_readable.add_project_sandbox_path(false)),
            ),
    );
    column = column.child(super::settings::reset_settings(ws, scheme));
    page(tr.section_project, column)
}
