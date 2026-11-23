//! UI languages: English and German. Every string a view shows comes from a
//! [`Strings`] table, picked once from the `language` setting and kept in
//! `Inner::tr`. A missing translation is a compile error, since both tables
//! fill every field. Text with values in it is a plain `fn` that formats.
//!
//! Some names stay English in both languages because code finds widgets by
//! them: the terminal and web content names of each pane, and the palette's
//! inputs (see `build_app`). Errors that come from git, extensions or
//! `settings.json` parsing also stay as their source wrote them.

use crate::forge::PipelineStatus;
use crate::term::Attention;

use crate::agents::{Note, Status};
use crate::keymap::{Action, Preset};
use crate::state::SplitAxis;
use crate::ui::SettingsSection;

/// A language the UI speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    De,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::De];

    /// The id stored in `settings.json`.
    pub fn id(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
        }
    }

    /// The language's name in itself, so a reader finds their own.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::De => "Deutsch",
        }
    }

    /// The language of `strings`, one of the tables [`Self::strings`] gives.
    pub fn of(strings: &Strings) -> Lang {
        (Self::ALL.into_iter())
            .find(|lang| std::ptr::eq(lang.strings(), strings))
            .unwrap_or_default()
    }

    pub fn strings(self) -> &'static Strings {
        match self {
            Lang::En => &EN,
            Lang::De => &DE,
        }
    }

    /// The language for a settings id. [`SYSTEM_LANGUAGE`] and unknown ids
    /// follow the locale.
    pub fn resolve(id: &str) -> Lang {
        match id {
            "en" => Lang::En,
            "de" => Lang::De,
            _ => Lang::system(),
        }
    }

    /// The language of the locale: `LANGUAGE`, then `LC_ALL`, `LC_MESSAGES`
    /// and `LANG`, the first one set decides. Tests always get English so
    /// they do not depend on the machine's locale.
    pub fn system() -> Lang {
        if cfg!(test) {
            return Lang::En;
        }
        let locale = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|var| std::env::var(var).ok())
            .find(|value| !value.is_empty())
            .unwrap_or_default();
        Lang::from_locale(&locale)
    }

    /// The language of a locale value such as `de_DE.UTF-8` or the
    /// `de:en` list of `LANGUAGE`. Anything but German is English.
    pub fn from_locale(locale: &str) -> Lang {
        let first = locale.split(':').next().unwrap_or_default();
        let code = first.split(['_', '.', '@', '-']).next().unwrap_or_default();
        if code.eq_ignore_ascii_case("de") {
            Lang::De
        } else {
            Lang::En
        }
    }
}

/// Every string the UI shows, in one language.
pub struct Strings {
    // Settings overlay.
    pub settings: &'static str,
    // The notification center.
    pub notifications: &'static str,
    pub notifications_unread: fn(usize) -> String,
    pub no_notifications: &'static str,
    pub check_for_updates: &'static str,
    pub checking_for_updates: &'static str,
    pub update_all: &'static str,
    pub clear_notifications: &'static str,
    pub close_notifications: &'static str,
    pub updating: &'static str,
    pub dismiss: &'static str,
    pub dismiss_notification: fn(&str) -> String,
    pub update_notification: fn(&str) -> String,
    pub notice_title: &'static str,
    pub integration_outdated: fn(&str, &str) -> String,
    pub integration_outdated_body: &'static str,
    /// Title of the one card for several outdated integrations.
    pub integrations_outdated: fn(usize) -> String,
    pub agent_update: fn(&str, &str) -> String,
    pub agent_update_body: fn(&str, &str, &str, &str) -> String,
    pub agent_updated: fn(&str, &str) -> String,
    pub agent_update_failed: fn(&str, &str) -> String,
    pub app_update: fn(&str) -> String,
    pub app_update_body: &'static str,
    /// A newer release this installation does not install itself. Takes
    /// the release page.
    pub app_release_body: fn(&str) -> String,
    pub app_updated: fn(&str) -> String,
    pub app_updated_body: &'static str,
    pub app_update_failed: &'static str,
    pub just_now: &'static str,
    pub minutes_ago: fn(u64) -> String,
    pub hours_ago: fn(u64) -> String,
    pub days_ago: fn(u64) -> String,
    pub close_settings: &'static str,
    pub close_escape: &'static str,
    pub section_font: &'static str,
    pub section_color_scheme: &'static str,
    pub section_keyboard: &'static str,
    pub section_extensions: &'static str,
    pub section_agents: &'static str,
    pub section_language: &'static str,
    pub section_shells: &'static str,
    pub default_shell: &'static str,
    pub default_shell_system: &'static str,
    pub shells_note: &'static str,
    pub shells_listed: &'static str,
    pub git_bash_found: fn(&str) -> String,
    pub git_bash_missing: &'static str,
    pub new_shell_named: fn(&str) -> String,
    pub bundled_font: &'static str,
    /// Over the sample terminal lines on the font page.
    pub font_preview: &'static str,
    pub font_picker: &'static str,
    pub keymap_picker: &'static str,
    pub smaller_font: &'static str,
    pub larger_font: &'static str,
    pub font_size: &'static str,
    pub size: &'static str,
    pub system: &'static str,
    /// The pill on the color scheme in use.
    pub scheme_in_use: &'static str,
    pub follows_desktop: fn(&str) -> String,
    pub system_language: fn(&str) -> String,
    pub dark: &'static str,
    pub light: &'static str,
    pub from_extension: fn(&str) -> String,
    /// Under a key binding the user set: where it is kept.
    pub from_user: &'static str,
    pub bindings: &'static str,
    pub binding_list: &'static str,
    pub bindings_hint: &'static str,
    /// A recorded chord another action had: (chord, that action).
    pub binding_taken: fn(&str, &str) -> String,
    pub reset_binding: &'static str,
    /// The keys of an action no key is bound to.
    pub binding_unset: &'static str,
    pub reset_binding_for: fn(&str) -> String,
    /// The key recorder's prompt on the Keyboard page.
    pub press_keys: &'static str,
    /// A recorded binding's title with the keys it has until new ones come.
    pub recording_was: fn(&str, &str) -> String,
    pub preset_description: fn(&Preset) -> &'static str,
    pub off: &'static str,
    pub on: &'static str,
    pub failed: &'static str,
    pub failed_with: fn(&str) -> String,
    pub no_extensions: &'static str,
    pub enable_extension: fn(&str) -> String,
    pub extension_list: &'static str,
    pub all_extensions: &'static str,
    pub extension_filter: &'static str,
    pub filter_all: &'static str,
    pub filter_active: &'static str,
    pub filter_disabled: &'static str,
    pub no_matching_extensions: &'static str,
    pub find_extension: &'static str,
    /// The hint of the find field above the settings' nav.
    pub find_setting: &'static str,
    /// Shown in the nav when no page matches.
    /// Where the page would be when the find field matches none.
    pub settings_no_match: fn(&str) -> String,
    /// Words the find field matches a settings page by, besides its title.
    pub settings_keywords: fn(SettingsSection) -> &'static str,
    pub loaded_from: fn(&str) -> String,
    pub reload_note: &'static str,
    /// An extension waits for permissions, listed.
    pub needs_permissions: fn(&str) -> String,
    /// Over a loaded extension's permission switches.
    pub permissions_switch_note: &'static str,
    pub grant: &'static str,
    pub revoke: &'static str,
    /// Accessible name of the Grant button, with the extension's name.
    pub grant_for: fn(&str) -> String,
    /// Accessible name of the Revoke button, with the extension's name.
    pub revoke_for: fn(&str) -> String,
    /// Accessible name of one permission's switch: the extension's name and
    /// the permission's.
    pub permission_for: fn(&str, &str) -> String,
    /// What one permission lets an extension do, by its manifest name.
    pub permission_note: fn(&str) -> &'static str,
    /// Under a permission that reaches this machine's secrets, files,
    /// programs or variables: its manifest name and a warning.
    pub permission_sensitive: fn(&str) -> String,
    pub builtin_page: &'static str,
    pub builtin_subtitle: &'static str,
    pub builtin_description: &'static str,
    /// Accessible label of the scrolling page of one extension.
    pub extension_page: &'static str,
    pub no_description: &'static str,
    pub extension_enabled: &'static str,
    pub permissions_heading: &'static str,
    pub no_permissions: &'static str,
    pub settings_heading: &'static str,
    pub ext_guards_heading: &'static str,
    pub list_empty: &'static str,
    pub list_new_entry_for: fn(&str) -> String,
    pub list_add_to: fn(&str) -> String,
    pub list_remove: &'static str,
    pub list_remove_from: fn(&str, &str) -> String,
    pub list_duplicate: fn(&str) -> String,
    pub guards_heading: &'static str,
    pub guards_note: &'static str,
    pub guard_ask: &'static str,
    pub guard_block: &'static str,
    pub guard_allow: &'static str,
    /// Accessible name of a guard's mode button: guard title, then mode.
    pub guard_mode_for: fn(&str, &str) -> String,
    /// Subtitle of a guard row: extension name, then hosts.
    pub guard_from: fn(&str, &str) -> String,
    pub every_host: &'static str,
    pub approval_dialog: &'static str,
    /// The question, with the guard's title.
    pub approval_question: fn(&str) -> String,
    pub approval_note: &'static str,
    /// Which project a question comes from, with its name.
    pub approval_project: fn(&str) -> String,
    pub approval_always: &'static str,
    pub approval_never: &'static str,
    pub always_heading: &'static str,
    pub always_note: &'static str,
    /// A guard's title and host in the list of standing answers.
    pub always_entry: fn(&str, &str) -> String,
    pub always_remove: &'static str,
    pub change_requests_heading: &'static str,
    pub change_requests_note: &'static str,
    pub change_request_create: &'static str,
    pub change_request_comment: &'static str,
    pub change_request_merge: &'static str,
    /// Subtitle of a change request row: the guard id suffix it covers.
    pub change_request_subtitle: fn(&str) -> String,
    /// What a held request sends, from its body.
    pub approval_sends: fn(&str) -> String,
    /// What a sandbox wants with a keyring secret: the action code of
    /// `cmdterm_env::KeyringAction` and the secret's label.
    pub keyring_request: fn(&str, &str) -> String,
    pub keyring_note: &'static str,
    /// Where the keyring guard applies, in its settings row.
    pub keyring_scope: &'static str,
    /// Which agent changes which settings file in a sandbox.
    pub agent_settings_request: fn(&str, &str) -> String,
    /// A key an agent removed from its settings.
    pub agent_settings_removed: fn(&str) -> String,
    pub agent_settings_more: fn(usize) -> String,
    pub agent_settings_note: &'static str,
    /// Where the agent settings guard applies, in its settings row.
    pub agent_settings_scope: &'static str,
    /// The link a sandbox wants to open in the browser.
    pub open_url_request: fn(&str) -> String,
    pub open_url_note: &'static str,
    /// Where the open-url guard applies, in its settings row.
    pub open_url_scope: &'static str,
    pub approval_allow: &'static str,
    pub approval_block: &'static str,
    pub control_dialog: &'static str,
    pub control_install: fn(&str) -> String,
    pub control_change: fn(&str) -> String,
    /// Project and worktree, machine, environment kind.
    pub control_source: fn(&str, &str, &str) -> String,
    pub control_file_added: fn(&str, usize) -> String,
    pub control_file_changed: fn(&str, usize) -> String,
    pub control_file_removed: fn(&str) -> String,
    pub control_more_files: fn(usize) -> String,
    pub control_permissions: fn(&str) -> String,
    pub control_no_permissions: &'static str,
    pub control_shipped: &'static str,
    pub control_note: &'static str,
    pub control_deny: &'static str,
    pub control_applied: fn(&str) -> String,
    pub control_failed: fn(&str) -> String,
    pub control_launch_dialog: &'static str,
    /// Branch, project.
    pub control_new_worktree: fn(&str, &str) -> String,
    pub control_open_worktree: fn(&str) -> String,
    pub control_new_detached: fn(&str) -> String,
    pub control_worktree_from: fn(&str) -> String,
    pub control_worktree_from_main: &'static str,
    pub control_start_agent: fn(&str) -> String,
    pub control_type_prompt: fn(&str) -> String,
    pub control_launch_note: &'static str,
    /// Launch dialog button that allows this project's launches until the
    /// app quits.
    pub control_allow_session: &'static str,
    /// Entry of the "Always allowed" list for a project's launches.
    pub always_launches: &'static str,
    pub control_settings_dialog: &'static str,
    pub control_settings_question: &'static str,
    pub control_reason: fn(&str) -> String,
    /// Key path, value before, value after.
    pub control_setting_set: fn(&str, &str, &str) -> String,
    pub control_setting_add: fn(&str, &str) -> String,
    pub control_setting_remove: fn(&str, &str) -> String,
    pub control_settings_note: &'static str,
    pub control_settings_applied: &'static str,
    pub reload_extensions: &'static str,
    pub agent_on: fn(&str, &str) -> String,
    pub working: &'static str,
    pub install: &'static str,
    pub update: &'static str,
    pub remove: &'static str,
    pub retry: &'static str,
    /// Accessible name of an agent's button: action, then agent.
    pub agent_button: fn(&str, &str) -> String,
    pub agent_list: &'static str,
    /// The agents page's explanation, one line each.
    pub agents_note: [&'static str; 2],
    /// Under the agents note: the skill every integration brings along.
    pub agents_skill: [&'static str; 2],
    pub hooks_run: &'static str,
    pub check_agents: &'static str,
    /// The button that updates every outdated agent integration.
    pub agents_update_all: fn(usize) -> String,
    /// The heading of the agent list.
    pub agents_installed: &'static str,
    /// The banner over the agent list when several integrations are outdated.
    pub agents_outdated: fn(usize) -> String,
    pub agent_control: &'static str,
    pub agent_control_on: &'static str,
    pub agent_control_local: &'static str,
    pub agent_control_host: &'static str,
    pub agent_control_off: &'static str,
    pub agent_control_note: &'static str,
    pub checking: &'static str,
    pub not_installed: &'static str,
    pub on_outdated: &'static str,
    pub note_trust_hooks: &'static str,
    pub note_enable_hooks: &'static str,
    pub note_next_session: &'static str,
    pub language: &'static str,

    // Actions, as the settings list, palette and tooltips name them.
    pub command_palette: &'static str,
    /// The buttons at the right end of the tab strip, while the app draws
    /// the window's title bar.
    pub minimize_window: &'static str,
    pub maximize_window: &'static str,
    pub restore_window: &'static str,
    pub close_window: &'static str,
    /// The keys of the command palette, under its list.
    pub palette_keys_hint: &'static str,
    /// After the palette's keys: the key that moves focus between the
    /// window's areas.
    pub palette_areas_hint: fn(&str) -> String,
    /// The palette's key hint while no command matches.
    pub palette_empty_keys_hint: &'static str,
    /// The palette's footer while it asks for a line of text.
    pub palette_input_keys_hint: &'static str,
    /// The palette's confirm button when it asks for a new worktree's branch.
    pub palette_create: &'static str,
    /// The palette's confirm button when it asks where to clone.
    pub palette_clone: &'static str,
    /// The footer's count of matches past the list, `type to find 9 more`.
    /// The footer's place in a long list: the first and last row shown,
    /// the count of rows, and how many wait below.
    pub palette_more: fn(usize, usize, usize, usize) -> String,
    /// A destructive palette row after the first Enter, waiting for the second.
    pub palette_confirm: fn(&str) -> String,
    pub new_terminal: &'static str,
    pub close_panel: &'static str,
    /// The close button of a split pane's title strip, `Close README.md`.
    /// Names a split pane's close button by the pane's number and title.
    pub close_panel_named: fn(usize, &str) -> String,
    pub move_pane: fn(usize) -> String,
    pub pane_menu: fn(usize) -> String,
    pub drag_tab_hint: &'static str,
    pub drag_shown_tab_hint: &'static str,
    pub drag_pane_hint: &'static str,
    pub zone_left: &'static str,
    pub zone_right: &'static str,
    pub zone_top: &'static str,
    pub zone_bottom: &'static str,
    pub zone_swap: &'static str,
    /// Names a pane's drop on another tab.
    pub drop_into_tab: &'static str,
    /// Names a pane's drop on the `+` after the tabs.
    pub drop_new_tab: &'static str,
    pub close_pane: &'static str,
    pub resize_panes: &'static str,
    pub close_tab: &'static str,
    pub close_other_tabs: &'static str,
    pub move_pane_to_tab: &'static str,
    pub split_side_by_side: &'static str,
    pub split_stacked: &'static str,
    pub focus_next_pane: &'static str,
    pub focus_previous_pane: &'static str,
    pub grow_pane: &'static str,
    pub shrink_pane: &'static str,
    pub next_tab: &'static str,
    pub previous_tab: &'static str,
    /// Palette entry and Keyboard page name of the rail toggle.
    pub toggle_sidebar: &'static str,
    /// The rail header's button that hides the rail.
    pub hide_sidebar: &'static str,
    /// The tab strip's button that shows a hidden rail.
    pub show_sidebar: &'static str,
    pub tab_n: fn(u8) -> String,
    pub focus_pane_n: fn(u8) -> String,
    pub send_text: fn(&str) -> String,

    // Rail.
    pub projects: &'static str,
    pub change_requests: &'static str,
    pub sidebar: &'static str,
    pub adding: fn(&str) -> String,
    pub set_token: fn(&str) -> String,
    pub open_host: fn(&str) -> String,
    pub new_worktree_from: fn(&str) -> String,
    /// The line under the new worktree dialog's title naming its source.
    pub worktree_source: fn(&str) -> String,
    pub loading: &'static str,
    pub open_project: &'static str,
    pub worktree_count: fn(&str, usize) -> String,
    pub ci_ok: &'static str,
    pub ci_failed: &'static str,
    pub ci_running: &'static str,
    pub ci_pending: &'static str,
    pub ci_canceled: &'static str,
    pub locked: &'static str,
    /// After the count of changed files in a worktree row.
    pub meta_changed: &'static str,
    /// After the count of untracked files in a worktree row.
    pub meta_new: &'static str,
    pub missing: &'static str,
    pub busy: &'static str,
    pub done: &'static str,
    pub input: &'static str,
    pub approve: &'static str,
    pub busy_description: &'static str,
    pub done_description: &'static str,
    pub input_description: &'static str,
    pub approve_description: &'static str,

    // Tabs, panel and status bar.
    pub new_shell: &'static str,
    /// A palette row that starts a coding agent, by its name.
    pub start_agent: fn(&str) -> String,
    /// The `+` after the last tab and the panel list it opens.
    pub new_panel: &'static str,
    pub new_panel_tip: &'static str,
    /// The file panel's palette entry and action name.
    pub browse_files: &'static str,
    /// The file panel's tab title and header without a tree.
    pub files: &'static str,
    pub files_count: fn(usize) -> String,
    pub files_lines: fn(usize) -> String,
    pub files_truncated: &'static str,
    pub files_loading: &'static str,
    pub files_empty: &'static str,
    pub files_pick: &'static str,
    pub files_binary: fn(usize) -> String,
    pub files_unreadable: fn(&str) -> String,
    pub files_unrendered: fn(&str) -> String,
    /// Over the source of a Mermaid diagram the reader cannot draw.
    pub files_mermaid_unsupported: fn(&str) -> String,
    pub files_pixels: fn(u32, u32) -> String,
    pub files_page: fn(usize, usize) -> String,
    pub files_show_source: &'static str,
    pub files_show_rendered: &'static str,
    pub explore_history: &'static str,
    /// The heading over the history list: its commits.
    pub explore_list_commits: &'static str,
    /// The heading over the review list: its files.
    pub explore_list_files: &'static str,
    pub explore_review: &'static str,
    pub explore_open_history: &'static str,
    pub status_open_history: &'static str,
    pub status_open_files: &'static str,
    pub status_open_machines: &'static str,
    pub explore_open_review: &'static str,
    pub explore_review_against: &'static str,
    pub explore_pick: &'static str,
    pub explore_no_commits: &'static str,
    pub explore_sideways: &'static str,
    pub explore_no_changes: fn(&str) -> String,
    pub explore_no_base: &'static str,
    /// The review's right side while the list says there are no changes.
    pub explore_no_changes_hint: fn(&str) -> String,
    pub explore_failed: fn(&str) -> String,
    /// The review when its base shares no commits with HEAD.
    pub explore_unrelated: fn(&str) -> String,
    /// The review when git fails for another reason.
    pub explore_review_failed: fn(&str) -> String,
    pub explore_against: fn(&str) -> String,
    pub explore_from: fn(&str) -> String,
    pub explore_commits: fn(usize) -> String,
    pub explore_uncommitted: &'static str,
    pub explore_truncated: &'static str,
    /// The debug browser panel's tab title while the page has none.
    pub debug_browser: &'static str,
    /// Its palette entry and action name.
    pub debug_browser_open: &'static str,
    pub debug_browser_url_ask: &'static str,
    /// Palette rows and notices for forwarding a sandbox's port to the host.
    pub expose_port: fn(u16) -> String,
    pub stop_exposing_port: fn(u16) -> String,
    pub port_exposed: fn(u16, u16) -> String,
    pub port_expose_failed: fn(u16, &str) -> String,
    pub debug_browser_failed: &'static str,
    pub debug_browser_retry: &'static str,
    pub debug_browser_alt: &'static str,
    /// The monitor panel's tab title.
    pub monitor: &'static str,
    /// Beside the monitor panel's name in its bar.
    pub monitor_note: &'static str,
    /// Its palette entry and action name.
    pub monitor_open: &'static str,
    pub monitor_waiting: &'static str,
    pub monitor_no_proc: &'static str,
    pub monitor_failed: fn(&str) -> String,
    /// Gauge labels.
    pub monitor_cpu: &'static str,
    pub monitor_memory: &'static str,
    pub monitor_swap: &'static str,
    pub monitor_network: &'static str,
    pub monitor_disk: &'static str,
    /// Labels of the monitor's graphs, and the word before the network
    /// graph's top value.
    pub monitor_cpu_history: &'static str,
    pub monitor_net_history: &'static str,
    /// The network graph's legend: received and sent bytes.
    pub monitor_net_down: &'static str,
    pub monitor_net_up: &'static str,
    pub monitor_peak: &'static str,
    /// The right end of the graphs' time axis.
    pub monitor_now: &'static str,
    /// Under the graphs until they have history to draw.
    pub monitor_collecting: &'static str,
    /// At the left of the time axis while the history fills.
    pub copy_command: &'static str,
    pub copy_command_hint: &'static str,
    pub copy_install_for: fn(&str) -> String,
    pub command_copied: &'static str,
    /// How to install an agent that is not installed.
    pub agent_install_with: fn(&str) -> String,
    /// The action that sets up the hooks of an agent that is installed.
    pub agent_turn_on: &'static str,
    /// A tab's title with the number of panes it shows.
    pub tab_with_panes: fn(&str, usize) -> String,
    /// Name of the button that steps to tabs the full strip hides.
    pub tabs_hidden: fn(usize) -> String,
    /// The overflow button's short label: `17 more`.
    pub tabs_more: fn(usize) -> String,
    /// Headings of the command palette's groups.
    pub palette_group_project: &'static str,
    /// Beside the magnifier in the tab strip that opens the palette.
    pub commands: &'static str,
    pub palette_group_agents: &'static str,
    pub palette_group_panels: &'static str,
    pub palette_group_shells: &'static str,
    pub palette_group_layout: &'static str,
    pub palette_group_app: &'static str,
    pub palette_group_worktrees: &'static str,
    pub palette_group_runtime: &'static str,
    pub palette_group_extensions: &'static str,
    /// Column headings of the tasks panel.
    pub tasks_col_state: &'static str,
    pub tasks_col_kind: &'static str,
    pub tasks_col_panel: &'static str,
    /// Heading of the column with a panel's tab and pane.
    pub tasks_col_where: &'static str,
    pub tasks_col_task: &'static str,
    pub tasks_col_schedule: &'static str,
    pub tasks_col_thread: &'static str,
    pub tasks_col_cpu: &'static str,
    pub tasks_col_time: &'static str,
    /// Between a disk's sizes and its mount point.
    /// Header words: `up 3d 04:12 · load 0.52 0.61 0.70 · 1234 tasks, 2 running`.
    pub monitor_up: &'static str,
    pub monitor_days: &'static str,
    pub monitor_load: &'static str,
    pub monitor_tasks: &'static str,
    pub monitor_running: &'static str,
    /// The tasks panel's tab title and header.
    pub tasks: &'static str,
    /// Beside the tasks panel's name in its bar.
    pub tasks_note: &'static str,
    pub tasks_refresh: &'static str,
    /// Its palette entry and action name.
    pub tasks_open: &'static str,
    /// The header's counts after the title.
    pub tasks_summary: fn(&crate::taskview::Summary) -> String,
    /// Section headings.
    pub tasks_panels: &'static str,
    pub tasks_extension: &'static str,
    pub tasks_threads: &'static str,
    /// A section with nothing in it.
    pub tasks_none: &'static str,
    pub tasks_no_threads: &'static str,
    /// The line the idle threads fold into.
    pub tasks_idle_threads: fn(usize) -> String,
    /// After the shown worktree's name.
    pub tasks_shown_worktree: &'static str,
    /// Panel states.
    pub tasks_shown: &'static str,
    pub tasks_hidden: &'static str,
    pub tasks_frozen: &'static str,
    pub tasks_thawing: &'static str,
    pub tasks_exited: &'static str,
    pub tasks_killed: &'static str,
    /// Thread and extension task states.
    pub tasks_running: &'static str,
    pub tasks_sleeping: &'static str,
    /// A thread that is asleep now but used the CPU since the last look.
    pub tasks_busy: &'static str,
    pub tasks_due: &'static str,
    pub tasks_scheduled: &'static str,
    pub tasks_kind: fn(crate::taskview::Kind) -> &'static str,
    /// What an agent reported last; empty for nothing.
    pub tasks_agent_state: fn(crate::term::Attention) -> &'static str,
    /// A panel's tab and, in a split tab, its pane, both from 1.
    pub tasks_place: fn(usize, Option<usize>) -> String,
    pub tasks_exit_code: fn(i32) -> String,
    /// These take a duration such as `12m`.
    pub tasks_age: fn(&str) -> String,
    pub tasks_freezes_in: fn(&str) -> String,
    pub tasks_every: fn(&str) -> String,
    pub tasks_last: fn(&str) -> String,
    pub tasks_next: fn(&str) -> String,
    pub tasks_never: &'static str,
    pub pane_n: fn(usize) -> String,
    pub press: fn(&str) -> String,
    pub open_a_project: &'static str,
    pub open_the_palette: &'static str,
    pub then_open_project: fn(&str) -> String,
    pub no_panels: &'static str,
    pub no_panels_message: fn(&str) -> String,
    pub use_new_shell: &'static str,
    pub no_project: &'static str,
    pub select_project: &'static str,
    pub status_bar: &'static str,

    // Palette.
    pub prompt_project: &'static str,
    pub prompt_clone_folder: &'static str,
    pub prompt_review_base: &'static str,
    pub clone_from: fn(&str) -> String,
    pub clone_here: &'static str,
    pub clone_here_devcontainer: &'static str,
    pub clone_in_vm: fn(&str) -> String,
    pub clone_on: fn(&str) -> String,
    pub clone_devcontainer_on: fn(&str) -> String,
    pub clone_vm_on: fn(&str) -> String,
    pub clone_vm_name: &'static str,
    pub prompt_branch: &'static str,
    pub prompt_worktree: &'static str,
    pub prompt_url: &'static str,
    pub prompt_command: &'static str,
    pub prompt_panel: &'static str,
    pub open_project_ask: &'static str,
    pub new_worktree_ask: &'static str,
    pub open_url_ask: &'static str,
    pub switch_to: fn(&str) -> String,
    pub switch_to_worktree: fn(&str, &str) -> String,
    pub remove_worktree: fn(&str) -> String,
    /// Context menu and palette entries that talk to a repository's
    /// remotes, and the notices of their runs.
    pub vcs_fetch: &'static str,
    pub vcs_fetch_project: fn(&str) -> String,
    pub vcs_pull: fn(&str) -> String,
    pub vcs_push: fn(&str) -> String,
    /// Branch, then remote.
    pub vcs_push_to: fn(&str, &str) -> String,
    pub vcs_fetching: fn(&str) -> String,
    pub vcs_pulling: fn(&str) -> String,
    pub vcs_pushing: fn(&str) -> String,
    pub vcs_fetched: fn(&str) -> String,
    pub vcs_pulled: fn(&str) -> String,
    /// Branch, then remote.
    pub vcs_pushed: fn(&str, &str) -> String,
    /// Project or branch, then the reason.
    pub vcs_fetch_failed: fn(&str, &str) -> String,
    pub vcs_pull_failed: fn(&str, &str) -> String,
    pub vcs_push_failed: fn(&str, &str) -> String,
    pub vcs_busy: fn(&str) -> String,
    pub forget_project: fn(&str) -> String,
    pub open_temporary_ask: &'static str,
    pub global_session_ask: &'static str,
    pub prompt_global_session: &'static str,
    /// Name of a new global session in the rail.
    pub global_session: &'static str,
    /// Beside the machine of a session that is not saved.
    pub temporary: &'static str,
    pub keep_session: fn(&str) -> String,
    pub close_session: fn(&str) -> String,
    /// The rail's tabs and the Agents tab.
    pub agents_tab: &'static str,
    pub agents_tab_count: fn(usize) -> String,
    pub agents_finished: &'static str,
    pub agents_working: fn(usize) -> String,
    pub agents_merged: fn(usize) -> String,
    pub agents_earlier: fn(usize) -> String,
    pub agents_none: &'static str,
    pub agents_none_finished: &'static str,
    /// Under [`Strings::agents_none`]: what fills the list.
    pub agents_none_hint: &'static str,
    /// The button under an empty Agents tab.
    pub agents_start: &'static str,
    pub change_open: &'static str,
    pub change_merged: &'static str,
    pub change_closed: &'static str,
    /// Forge accounts on an extension's page.
    pub forge_accounts: &'static str,
    pub forge_signed_in_as: fn(&str) -> String,
    pub forge_signed_in: &'static str,
    pub forge_signed_out: &'static str,
    pub forge_signing_in: &'static str,
    pub forge_from_env: fn(&str) -> String,
    pub forge_failed: fn(&str) -> String,
    pub forge_create_token: &'static str,
    pub forge_create_token_hint: &'static str,
    pub forge_create_token_for: fn(&str) -> String,
    pub forge_token_for: fn(&str) -> String,
    pub forge_sign_in: &'static str,
    pub forge_sign_in_for: fn(&str) -> String,
    pub forge_sign_out: &'static str,
    pub forge_sign_out_for: fn(&str) -> String,
    pub forge_token_missing: &'static str,
    pub extension_request_failed: fn(&str) -> String,
    pub forge_new_account_for: fn(&str) -> String,
    pub forge_add_account: &'static str,
    pub forge_add_account_open: &'static str,
    pub forge_other_host: &'static str,
    pub forge_open_add_for: fn(&str) -> String,
    pub forge_sign_in_open: &'static str,
    pub forge_open_sign_in_for: fn(&str) -> String,
    pub forge_add_account_for: fn(&str) -> String,
    pub forge_remove_account_for: fn(&str) -> String,
    pub forge_not_an_account: fn(&str) -> String,
    /// Toasts in the settings header about how an action ended.
    pub toast_done: &'static str,
    pub toast_failed: &'static str,
    pub toast_signed_in: fn(&str, &str) -> String,
    pub toast_sign_in_failed: fn(&str, &str) -> String,
    pub toast_signed_out: fn(&str) -> String,
    pub toast_account_added: fn(&str) -> String,
    pub toast_account_removed: fn(&str) -> String,
    pub toast_trusted: fn(&str) -> String,
    pub toast_forgotten: fn(&str) -> String,
    /// The sidebar shows a machine's projects from now on.
    pub toast_fused: fn(&str) -> String,
    pub toast_unfused: fn(&str) -> String,
    pub toast_key_allowed: &'static str,
    pub toast_key_removed: &'static str,
    pub toast_stopped_serving: &'static str,
    pub toast_saved: &'static str,
    pub toast_image_ready: fn(&str) -> String,
    pub toast_image_failed: fn(&str, &str) -> String,
    pub toast_disk_deleted: fn(&str) -> String,
    pub toast_agent_installed: fn(&str, &str) -> String,
    pub toast_agent_removed: fn(&str, &str) -> String,
    pub toast_agent_failed: fn(&str, &str, &str) -> String,
    pub toast_host_added: fn(&str) -> String,
    pub toast_path_added: fn(&str) -> String,
    pub toast_project_base: fn(Option<&str>) -> String,
    pub toast_project_env_keyring: fn(&str) -> String,
    pub toast_project_env_plain: fn(&str) -> String,
    pub toast_project_env_session: fn(&str) -> String,
    pub toast_project_env_fallback: fn(&str, &str) -> String,
    pub toast_project_env_removed: fn(&str) -> String,
    pub project_env_loaded_notice: fn(&str) -> String,
    /// The Accounts page of the settings.
    pub section_accounts: &'static str,
    pub accounts_forges: &'static str,
    pub accounts_forges_note: &'static str,
    /// After the title of the forge the active project's remote is on.
    pub forge_used_by: fn(&str, &str) -> String,
    pub accounts_no_forges: &'static str,
    pub accounts_example: fn(&str) -> String,
    pub accounts_logins: &'static str,
    pub accounts_logins_note: &'static str,
    pub logins_file: fn(&str) -> String,
    pub logins_variable: fn(&str) -> String,
    pub logins_other: fn(&str) -> String,
    /// Opens the page of the extension that reads a registry's logins.
    pub logins_extension_for: fn(&str) -> String,
    /// The button of a registry's row: opens its extension, named by id.
    pub logins_extension: &'static str,
    pub logins_for: fn(&str) -> String,
    pub logins_none: &'static str,
    /// The Registry logins page with no extension that passes logins.
    pub logins_no_sources: &'static str,
    pub logins_off: &'static str,
    pub open_accounts: &'static str,
    pub ext_accounts_moved: &'static str,
    pub ext_hosts_heading: &'static str,
    pub ext_hosts_note: &'static str,
    pub ext_hosts_none: &'static str,
    pub not_a_host: fn(&str) -> String,
    pub not_a_url: fn(&str) -> String,
    /// The discussion of a change request, `discussion.rs`. `since` is a
    /// short commit or empty.
    pub discussion_open: fn(&str) -> String,
    pub discussion_loading: &'static str,
    pub discussion_waiting: &'static str,
    pub discussion_no_change: &'static str,
    pub discussion_unsupported: &'static str,
    pub discussion_failed: fn(&str) -> String,
    /// A worktree's `.git` leads elsewhere than the metadata the app reads.
    pub git_dir_replaced: fn(&str) -> String,
    pub discussion_by: fn(&str) -> String,
    pub discussion_on_forge: &'static str,
    pub discussion_pushes: &'static str,
    pub discussion_pushed: &'static str,
    pub discussion_force_pushed: &'static str,
    pub discussion_rewritten: &'static str,
    pub discussion_conversation: &'static str,
    pub discussion_line: fn(&str, u64) -> String,
    pub discussion_unchanged: fn(&str) -> String,
    pub discussion_moved: fn(u64, &str) -> String,
    pub discussion_changed: fn(u64, &str) -> String,
    pub discussion_found: fn(u64) -> String,
    pub discussion_file_gone: fn(&str) -> String,
    pub discussion_line_gone: fn(u64, &str) -> String,
    pub discussion_removed: fn(u64) -> String,
    pub discussion_resolved: &'static str,
    pub discussion_outdated: &'static str,
    pub explore_change: &'static str,
    pub explore_open_change: &'static str,
    pub change_overview: &'static str,
    pub change_checks: &'static str,
    pub change_checks_label: fn(usize, usize) -> String,
    pub change_checks_summary: fn(usize, usize, usize) -> String,
    pub change_threads: fn(usize, usize) -> String,
    pub change_reviews: &'static str,
    pub change_approved: &'static str,
    pub change_requested_changes: &'static str,
    pub change_commented: &'static str,
    pub change_review_pending: &'static str,
    pub change_draft: &'static str,
    pub change_conflicts: &'static str,
    pub change_pipeline: &'static str,
    /// Accessible name of the change request page's list.
    pub change_sections: &'static str,
    pub change_reload: &'static str,
    /// The button under the page of a forge no extension shows
    /// discussions for.
    pub change_open_extensions: &'static str,
    pub change_open_on_forge: &'static str,
    /// A button that opens a check's link.
    pub change_open_link: &'static str,
    pub change_open_check: fn(&str) -> String,
    pub change_description: &'static str,
    pub change_no_description: &'static str,
    pub change_line: fn(u64) -> String,
    /// Label of the rail's search field, and its palette row and action.
    pub search_worktrees: &'static str,
    pub focus_tabs: &'static str,
    pub focus_back: &'static str,
    pub search_no_match: &'static str,
    /// Palette rows that change what the rail names worktrees after.
    pub title_worktrees_by_branch: &'static str,
    pub title_worktrees_by_agent: &'static str,
    pub use_system_title_bar: &'static str,
    pub use_app_title_bar: &'static str,
    /// Palette rows that turn Jujutsu on or off for every project that
    /// does not choose for itself.
    pub jj_on: &'static str,
    /// Palette: switches the CPU mode of this machine's terminals to the
    /// mode named.
    pub cpu_mode_local: fn(&str) -> String,
    /// Palette: the same for terminals on other machines.
    pub cpu_mode_remote: fn(&str) -> String,
    /// The CPU modes' short names.
    pub cpu_off: &'static str,
    pub cpu_balanced: &'static str,
    pub cpu_quiet: &'static str,
    /// What a CPU mode does: cores kept free, percent of the cores at
    /// most, nice level of agents in hidden terminals.
    pub cpu_detail: fn(u32, u32, u8) -> String,
    /// Status bar: the CPU mode where the active project runs.
    pub cpu_status: fn(&str) -> String,
    /// The CPU part's accessible name: its visible text, then the next mode
    /// a click switches to.
    pub cpu_status_hint: fn(&str, &str) -> String,
    /// The CPU part's tooltip: the mode's name and the next one's, short
    /// enough for one line over the status bar.
    pub cpu_status_tip: fn(&str, &str) -> String,
    /// Agents settings page: the CPU heading, what it does, and the labels
    /// of its two dropdowns.
    pub cpu_heading: &'static str,
    pub cpu_note: &'static str,
    pub cpu_local: &'static str,
    pub cpu_remote: &'static str,
    pub jj_off: &'static str,
    /// Palette rows that turn Jujutsu on or off for the active project.
    pub project_jj_on: &'static str,
    pub project_jj_off: &'static str,
    /// Palette row that runs an extension task now, with the task's title.
    pub run_task: fn(&str) -> String,
    pub no_folder_chooser: &'static str,
    pub open_folder: &'static str,
    pub choose_folder: &'static str,
    pub choose_a_folder: &'static str,
    pub branch_optional: &'static str,
    pub creates_next_to: fn(&str) -> String,
    pub no_matching_command: &'static str,
    pub no_matching_panel: &'static str,

    // Notices.
    pub state_not_loaded: fn(&str) -> String,
    pub settings_not_loaded: fn(&str) -> String,
    pub state_not_saved: fn(&str) -> String,
    pub settings_not_saved: fn(&str) -> String,
    pub not_a_path: fn(&str) -> String,
    pub repository_not_loaded: &'static str,
    pub bad_worktree_name: fn(&str) -> String,
    pub bad_branch_name: &'static str,
    pub bad_folder_name: &'static str,
    pub max_panes: fn(usize) -> String,
    pub blocklist_failed: fn(&str, &str) -> String,
    pub warning: fn(&str) -> String,
    pub error: fn(&str) -> String,
    pub no_worktree_at: fn(&str) -> String,
    /// An extension asked to open a view nobody registered.
    pub view_unknown: fn(&str) -> String,
    /// A view panel whose key its extension no longer renders.
    pub view_gone: fn(&str) -> String,
    pub terminal_failed: fn(&str) -> String,
    /// Rule groups of the macOS sandbox profile that `sandbox-exec` turned
    /// down, comma-separated.
    pub sandbox_rules_dropped: fn(&str) -> String,
    /// Terminals in Windows sandboxes start the second shell instead of
    /// the first, which runs on Git's MSYS runtime.
    pub sandbox_shell_replaced: fn(&str, &str) -> String,
    /// Claude Code started in a Windows sandbox, where Git Bash, which it
    /// runs its shell commands through, cannot start.
    pub sandbox_claude_without_bash: &'static str,
    /// A terminal's program exited with an error right after it started:
    /// its exit code (`None` for a signal) and its last line of output.
    pub terminal_failed_start: fn(Option<i32>, &str) -> String,
    pub paste_image_failed: fn(&str) -> String,
    // Worktree runtimes: devcontainer, sandbox, host.
    pub run_on_host: &'static str,
    pub run_in_devcontainer: &'static str,
    pub run_in_sandbox: &'static str,
    pub rebuild_devcontainer: &'static str,
    pub runtime_devcontainer: &'static str,
    pub runtime_sandbox: &'static str,
    /// The status item of a sandbox whose container engines pass through.
    pub runtime_sandbox_open: &'static str,
    /// The tooltip of the status bar's sandbox item while container engines
    /// pass through.
    pub runtime_sandbox_open_hint: &'static str,
    /// The status bar's runtime item, which opens the sandbox settings.
    pub open_sandboxing: &'static str,
    /// A sandbox whose container engines are passed through.
    pub runtime_starting: &'static str,
    pub no_devcontainer: &'static str,
    pub env_not_ready: &'static str,
    pub devcontainer_ready: fn(&str) -> String,
    pub devcontainer_failed: fn(&str) -> String,
    pub copy_failed: fn(&str) -> String,
    pub run_failed: fn(&str) -> String,
    pub trust_failed: fn(&str) -> String,
    // Settings: remote machines, virtual machines, containers.
    pub section_remote: &'static str,
    pub section_vms: &'static str,
    pub section_sandboxing: &'static str,
    pub section_sandbox_files: &'static str,
    pub section_sandbox_hosts: &'static str,
    pub this_machine: &'static str,
    pub your_key: &'static str,
    /// The accessible name of this machine's key fingerprint.
    pub your_key_named: fn(&str) -> String,
    pub copy_key_short: &'static str,
    pub key_copied: &'static str,
    pub serve_machine: &'static str,
    /// Heading of the switches that say what connecting machines get.
    pub when_machines_connect: &'static str,
    /// The steps that pair two machines, at the top of Remote machines.
    pub pairing_steps: &'static str,
    pub serve_on: fn(&str) -> String,
    pub serving_on: fn(&str) -> String,
    /// A listen address on every interface, by its port.
    pub any_address: fn(&str) -> String,
    pub serve_failed: fn(&str) -> String,
    pub offer_host: &'static str,
    pub offer_host_note: &'static str,
    pub offer_host_no_vms: &'static str,
    /// The switch that tells clients this machine's projects.
    pub share_projects: &'static str,
    pub share_projects_note: &'static str,
    /// The switch that keeps web links (cmdterm-web) to VMs.
    pub web_links_vm_only: &'static str,
    pub web_links_vm_only_note: &'static str,
    pub allowed_keys: &'static str,
    pub no_allowed_keys: &'static str,
    pub settings_results: &'static str,
    pub allowed_keys_not_serving: &'static str,
    pub key_to_allow: &'static str,
    pub vms_only: &'static str,
    /// What the VMs only switch does to the next allowed key.
    pub vms_only_note: &'static str,
    pub allow: &'static str,
    pub allow_vms_only: &'static str,
    pub paste_key_first: &'static str,
    pub not_a_key: &'static str,
    pub remove_named: fn(&str) -> String,
    /// Opens a closed group of hosts.
    pub show_named: fn(&str) -> String,
    /// Closes an open group of hosts.
    pub hide_named: fn(&str) -> String,
    /// The switch under an allowed key that gives it the host's shell.
    pub key_host: &'static str,
    pub key_host_named: fn(&str) -> String,
    /// The switch under an allowed key that shares the projects with it.
    pub key_projects: &'static str,
    pub key_projects_named: fn(&str) -> String,
    /// The button that makes an allowed key follow the defaults again.
    pub key_defaults: &'static str,
    pub key_defaults_named: fn(&str) -> String,
    /// Subtitle marker of a key with settings of its own.
    pub key_own_settings: &'static str,
    pub known_machines: &'static str,
    pub no_known_machines: &'static str,
    pub machine_address: &'static str,
    pub add_machine: &'static str,
    pub add: &'static str,
    pub open: &'static str,
    pub open_on: fn(&str) -> String,
    pub forget: &'static str,
    pub forget_named: fn(&str) -> String,
    /// The switch on a trusted machine's row that shows its projects in the
    /// sidebar.
    pub fuse_named: fn(&str) -> String,
    /// On a fused machine's row while it does not answer.
    pub fused_unreachable: &'static str,
    pub fetching_key: fn(&str) -> String,
    pub compare_key: fn(&str) -> String,
    pub trust: &'static str,
    pub trust_named: fn(&str) -> String,
    pub cancel: &'static str,
    pub cancel_set_up: fn(&str) -> String,
    pub ok: &'static str,
    pub already_trusted: fn(&str) -> String,
    pub key_changed: fn(&str) -> String,
    pub connect_failed: fn(&str) -> String,
    pub remote_note: &'static str,
    pub section_vpn: &'static str,
    pub vpn_scan: &'static str,
    pub vpn_scan_again: &'static str,
    pub vpn_scanning: &'static str,
    /// The scan button's text while a scan runs.
    pub vpn_scan_running: &'static str,
    pub vpn_down: fn(&str, &str) -> String,
    pub vpn_missing: fn(&str) -> String,
    pub vpn_this_machine: fn(&str) -> String,
    pub vpn_serve: &'static str,
    pub vpn_serve_named: fn(&str) -> String,
    pub vpn_no_peers: &'static str,
    pub online: &'static str,
    pub offline: &'static str,
    pub ssh: &'static str,
    pub ssh_named: fn(&str) -> String,
    pub remote: &'static str,
    pub remote_named: fn(&str) -> String,
    pub vpn_nebula_note: &'static str,
    pub vpn_note: &'static str,
    pub config_not_saved: fn(&str) -> String,
    pub no_kvm: &'static str,
    pub local_vms: &'static str,
    pub no_local_vms: &'static str,
    pub running: &'static str,
    pub stopped: &'static str,
    pub reset: &'static str,
    pub reset_note: &'static str,
    pub delete_disk_of: fn(&str) -> String,
    pub vm_name: &'static str,
    pub open_vm: &'static str,
    pub bad_vm_name: &'static str,
    pub stop_vms: &'static str,
    pub vms_stopping: &'static str,
    pub vm_disks_in: fn(&str) -> String,
    /// Labels of the Firecracker fields, in `VmField::ALL` order.
    pub vm_fields: [&'static str; 7],
    /// What goes in each empty Firecracker field, in `VmField::ALL` order.
    pub vm_field_hints: [&'static str; 7],
    pub save_vm_settings: &'static str,
    pub vm_settings_saved: &'static str,
    pub not_a_number: fn(&str) -> String,
    pub vm_notes: &'static [&'static str],
    pub ready_made_vms: &'static str,
    pub image_ready: fn(&str) -> String,
    pub set_up: &'static str,
    pub start_set_up: &'static str,
    pub set_up_explained: &'static str,
    pub set_up_image: fn(&str) -> String,
    pub rebuild: &'static str,
    pub rebuild_image: fn(&str) -> String,
    pub preparing: &'static str,
    pub install_first: fn(&str) -> String,
    pub vms_have_network: &'static str,
    pub vms_without_network: &'static str,
    pub vm_name_note: &'static str,
    /// The name of the Show/Hide advanced settings button, the same in
    /// both states so focus stays on it when it flips.
    pub advanced_settings: &'static str,
    pub container_engine: &'static str,
    pub automatic: &'static str,
    pub section_sandbox: &'static str,
    pub sandbox_network: &'static str,
    pub sandbox_network_note: &'static str,
    pub sandbox_restrict: &'static str,
    pub sandbox_restrict_note: &'static str,
    pub sandbox_host_network: &'static str,
    pub sandbox_host_network_note: &'static str,
    /// Notice when local sandboxes with the host network cannot be kept
    /// from abstract Unix sockets, because the kernel lacks Landlock ABI 6.
    pub sandbox_host_network_unscoped: &'static str,
    pub sandbox_allowed_hosts: &'static str,
    pub sandbox_hosts_inactive: &'static str,
    pub sandbox_intercept_tls: &'static str,
    pub sandbox_intercept_tls_note: &'static str,
    pub sandbox_engines: &'static str,
    pub sandbox_engines_note: &'static str,
    pub engines_passthrough: &'static str,
    pub engines_sidecar: &'static str,
    pub engines_off: &'static str,
    /// Buttons in the passthrough warning.
    pub engines_use_sidecar: &'static str,
    pub engines_turn_off: &'static str,
    pub engines_keep: &'static str,
    pub engines_fixes_note: &'static str,
    pub sandbox_sidecar_note: &'static str,
    pub sidecar_engine: &'static str,
    pub sidecar_podman: &'static str,
    pub sidecar_docker: &'static str,
    pub sidecar_ready: fn(&str) -> String,
    pub sidecar_failed: fn(&str) -> String,
    pub sandbox_host: &'static str,
    pub sandbox_host_none: fn(&str) -> String,
    pub count_of: fn(usize, usize) -> String,
    pub sandbox_host_bare: &'static str,
    pub add_host: &'static str,
    /// The button on the containers page that opens the host setup again.
    pub choose_hosts: &'static str,
    /// The button text of `choose_hosts` on the Hosts page.
    pub choose_hosts_open: &'static str,
    pub host_setup_dialog: &'static str,
    pub host_setup_intro: &'static str,
    pub host_setup_note: &'static str,
    pub host_setup_done: &'static str,
    /// The first-start setup's stages, see `ui/setup.rs`.
    pub setup_dialog: &'static str,
    pub setup_step: fn(usize, usize) -> String,
    pub setup_back: &'static str,
    pub setup_next: &'static str,
    pub setup_appearance: &'static str,
    pub setup_appearance_intro: &'static str,
    pub setup_runtime: &'static str,
    pub setup_runtime_intro: &'static str,
    pub setup_shell: &'static str,
    pub setup_shell_intro: &'static str,
    /// The keymap action and palette entry that start the default agent
    /// in a new tab.
    pub new_agent: &'static str,
    pub default_agent: &'static str,
    /// The default agent's option for no choice: the first agent with an
    /// integration on the project's machine.
    pub default_agent_first: &'static str,
    /// The accessible name of the setup's option for an agent.
    pub default_agent_named: fn(&str) -> String,
    pub default_agent_note: &'static str,
    pub setup_runtime_note: &'static str,
    pub setup_sandbox_note: &'static str,
    /// The sandbox note of the setup and the runtime settings on Windows,
    /// where the sandbox is not recommended.
    pub setup_sandbox_note_windows: &'static str,
    pub setup_devcontainer_note: &'static str,
    pub setup_host_note: &'static str,
    pub setup_recommended: &'static str,
    pub setup_agents: &'static str,
    pub setup_agents_intro: &'static str,
    pub settings_group_app: &'static str,
    pub settings_group_integrations: &'static str,
    pub settings_group_machines: &'static str,
    pub settings_group_sandbox: &'static str,
    pub settings_group_about: &'static str,
    pub section_changelog: &'static str,
    pub section_licenses: &'static str,
    /// The line over the changelog, with the running version.
    pub changelog_running: fn(&str) -> String,
    /// The changelog's heading for changes not released yet.
    pub changelog_unreleased: &'static str,
    pub licenses_app_hint: &'static str,
    pub license_mit: &'static str,
    pub license_apache: &'static str,
    pub licenses_third_party: &'static str,
    /// How many third-party crates the app is built with.
    pub licenses_third_party_hint: fn(usize) -> String,
    /// The build has no list of its crates.
    pub licenses_none: &'static str,
    /// A crate that names a license file instead of a license.
    pub license_in_file: &'static str,
    /// A crate that ships no license file, with its license.
    pub license_no_file: fn(&str) -> String,
    pub reset_settings: &'static str,
    pub reset_settings_confirm: &'static str,
    pub reset_settings_hint: &'static str,
    /// The name of a `crate::state::HOST_GROUPS` entry, by id.
    pub host_group: fn(&str) -> &'static str,
    /// The group of allowed hosts no host group holds.
    pub hosts_added_by_you: &'static str,
    pub host_removed: fn(&str) -> String,
    pub undo: &'static str,
    pub undo_remove_named: fn(&str) -> String,
    pub bad_host: &'static str,
    pub sandbox_writable: &'static str,
    pub sandbox_hidden: &'static str,
    pub sandbox_path: &'static str,
    pub add_writable: &'static str,
    pub add_hidden: &'static str,
    pub sandbox_note: &'static str,
    pub restart_terminals: &'static str,
    pub restart_terminals_hint: &'static str,
    pub sandbox_windows_note: &'static str,
    // Settings: the active project.
    pub section_project: &'static str,
    pub project_settings: &'static str,
    /// A palette row that opens one settings page.
    pub settings_page: fn(&str) -> String,
    pub project_none: &'static str,
    pub project_jj: &'static str,
    pub project_jj_note: &'static str,
    pub project_launches: &'static str,
    pub section_project_allowed: &'static str,
    pub section_project_blocked: &'static str,
    pub project_allowed_note: &'static str,
    pub project_blocked_note: &'static str,
    pub project_answers_remote_note: &'static str,
    pub project_answer_to_blocked: &'static str,
    pub project_answer_to_allowed: &'static str,
    pub project_answer_add_heading: &'static str,
    pub project_answer_guard: &'static str,
    pub project_answer_host: &'static str,
    pub project_answer_add: &'static str,
    pub project_launches_note: &'static str,
    pub project_base: &'static str,
    /// The reference branch field's hint.
    pub project_base_hint: &'static str,
    pub project_base_note: &'static str,
    pub project_base_set: &'static str,
    pub project_env: &'static str,
    pub project_env_note_keyring: &'static str,
    pub project_env_note_plain: &'static str,
    pub project_env_note_temporary: &'static str,
    pub project_env_remote_note: &'static str,
    pub project_env_name: &'static str,
    pub project_env_value: &'static str,
    pub project_env_add: &'static str,
    pub project_env_reload: &'static str,
    pub project_env_invalid_name: &'static str,
    pub project_env_invalid_value: &'static str,
    pub project_env_storage_keyring: &'static str,
    pub project_env_storage_loading: &'static str,
    pub project_env_storage_locked: &'static str,
    pub project_env_storage_plain: &'static str,
    pub project_env_storage_session: &'static str,
    pub project_folders: &'static str,
    pub project_folders_note: &'static str,
    pub project_writable: &'static str,
    pub project_readable: &'static str,
    pub no_project_folders: &'static str,
    pub add_readable: &'static str,
    pub project_remote_note: &'static str,
    pub project_not_sandboxed: &'static str,
    /// The Sandboxing page's note under where new worktrees run.
    pub default_runtime_note: &'static str,
    /// The Project page's note under where its worktrees run.
    pub project_runtime_note: &'static str,
    /// The Project page's choice that follows the Sandboxing page.
    pub runtime_follow_default: &'static str,
}

impl Strings {
    /// The badge text for an attention state; empty for none.
    pub fn attention_label(&self, attention: Attention) -> &'static str {
        match attention {
            Attention::None => "",
            Attention::Busy => self.busy,
            Attention::Done => self.done,
            Attention::Input => self.input,
            Attention::Permission => self.approve,
        }
    }

    /// What an attention state means, for accessible names.
    pub fn attention_description(&self, attention: Attention) -> &'static str {
        match attention {
            Attention::None => "",
            Attention::Busy => self.busy_description,
            Attention::Done => self.done_description,
            Attention::Input => self.input_description,
            Attention::Permission => self.approve_description,
        }
    }

    /// Short CI status for a worktree row; empty when unknown.
    pub fn pipeline_label(&self, status: Option<PipelineStatus>) -> &'static str {
        match status {
            Some(PipelineStatus::Success) => self.ci_ok,
            Some(PipelineStatus::Failed) => self.ci_failed,
            Some(PipelineStatus::Running) => self.ci_running,
            Some(PipelineStatus::Pending) => self.ci_pending,
            Some(PipelineStatus::Canceled) => self.ci_canceled,
            Some(PipelineStatus::Unknown) | None => "",
        }
    }

    /// What the settings list and the palette call an action.
    pub fn action(&self, action: &Action) -> String {
        match action {
            Action::Palette => self.command_palette.into(),
            Action::Settings => self.settings.into(),
            Action::SearchWorktrees => self.search_worktrees.into(),
            Action::FocusTabs => self.focus_tabs.into(),
            Action::FocusBack => self.focus_back.into(),
            Action::ToggleSidebar => self.toggle_sidebar.into(),
            Action::NewTerminal => self.new_terminal.into(),
            Action::NewAgent => self.new_agent.into(),
            Action::Files => self.browse_files.into(),
            Action::Explore => self.explore_open_history.into(),
            Action::Review => self.explore_open_review.into(),
            Action::Monitor => self.monitor_open.into(),
            Action::Tasks => self.tasks_open.into(),
            Action::DebugBrowser => self.debug_browser_open.into(),
            Action::ClosePanel => self.close_panel.into(),
            Action::ClosePane => self.close_pane.into(),
            Action::CloseTab => self.close_tab.into(),
            Action::MovePaneToTab => self.move_pane_to_tab.into(),
            Action::Split(SplitAxis::SideBySide) => self.split_side_by_side.into(),
            Action::Split(SplitAxis::Stacked) => self.split_stacked.into(),
            Action::NextPane => self.focus_next_pane.into(),
            Action::PreviousPane => self.focus_previous_pane.into(),
            Action::GrowPane => self.grow_pane.into(),
            Action::ShrinkPane => self.shrink_pane.into(),
            Action::NextTab => self.next_tab.into(),
            Action::PreviousTab => self.previous_tab.into(),
            Action::Tab(n) => (self.tab_n)(*n),
            Action::Pane(n) => (self.focus_pane_n)(*n),
            Action::SendText(text) => (self.send_text)(&text.escape_default().to_string()),
            Action::Command(name) => name.clone(),
        }
    }

    /// The label of a Firecracker field on the Virtual machines page.
    pub fn vm_field(&self, field: crate::machines::VmField) -> &'static str {
        self.vm_fields[field as usize]
    }

    /// An agent integration's state, as its settings row shows it.
    pub fn agent_status(&self, status: &Status) -> String {
        match status {
            Status::Checking => self.checking.to_owned(),
            Status::NotFound => self.not_installed.to_owned(),
            Status::Off => self.off.to_owned(),
            Status::On => self.on.to_owned(),
            Status::Outdated => self.on_outdated.to_owned(),
            Status::Failed(error) => (self.failed_with)(error),
        }
    }

    /// What the user still has to do after installing an agent integration.
    pub fn agent_note(&self, note: Note) -> &'static str {
        match note {
            Note::TrustHooks => self.note_trust_hooks,
            Note::EnableHooks => self.note_enable_hooks,
            Note::NextSession => self.note_next_session,
        }
    }
}

/// What each settings page holds, for the find field above the nav: the
/// names of its settings, then after `|` more words that find the page.
fn cpu_detail_en(free: u32, percent: u32, nice: u8) -> String {
    let mut parts = Vec::with_capacity(3);
    match free {
        0 => {}
        1 => parts.push("keeps 1 core free".to_owned()),
        n => parts.push(format!("keeps {n} cores free")),
    }
    if percent < 100 {
        parts.push(format!("uses at most {percent}% CPU"));
    }
    if nice > 0 {
        parts.push(format!("background agents at nice {nice}"));
    }
    if parts.is_empty() {
        return "no limit".to_owned();
    }
    parts.join(", ")
}

fn cpu_detail_de(free: u32, percent: u32, nice: u8) -> String {
    let mut parts = Vec::with_capacity(3);
    match free {
        0 => {}
        1 => parts.push("lässt 1 Kern frei".to_owned()),
        n => parts.push(format!("lässt {n} Kerne frei")),
    }
    if percent < 100 {
        parts.push(format!("nutzt höchstens {percent} % CPU"));
    }
    if nice > 0 {
        parts.push(format!("Agenten im Hintergrund mit nice {nice}"));
    }
    if parts.is_empty() {
        return "kein Limit".to_owned();
    }
    parts.join(", ")
}

fn keywords_en(section: SettingsSection) -> &'static str {
    match section {
        SettingsSection::Project => {
            "Where terminals run, Environment variables, Sandbox folders, Jujutsu, Reset all settings | project jj env secrets keyring host devcontainer sandbox runtime rebuild"
        }
        SettingsSection::ProjectAllowed => {
            "Guarded requests, Allow agents to open worktrees without asking, Add an entry | always allowed approvals answers git push keyring launches"
        }
        SettingsSection::ProjectBlocked => {
            "Guarded requests, Add an entry | always blocked approvals answers deny refuse git push keyring"
        }
        SettingsSection::Font => "Terminal font, Terminal font size | text ligatures family",
        SettingsSection::ColorScheme => "Color scheme, Follow the system | colour theme dark light",
        SettingsSection::Keyboard => "Keymap, Key bindings | keys shortcuts record",
        SettingsSection::Shells => "Default shell | powershell cmd wsl",
        SettingsSection::Language => "Language | english german deutsch",
        SettingsSection::Extensions => {
            "Extension permissions, Guarded requests, Reload extensions | lua plugins guards ask block allow"
        }
        SettingsSection::Accounts => {
            "Forge accounts, Sign in | github gitlab forgejo gitea bitbucket azure devops token"
        }
        SettingsSection::Registries => {
            "Registry logins, Cargo, Forge CLIs, Git over HTTPS, npm, NuGet, PyPI | credentials tokens"
        }
        SettingsSection::Agents => {
            "Agent integrations, Update hooks, Control socket, CPU | claude codex gemini antigravity agy install processor cores nice limit"
        }
        SettingsSection::Remote => {
            "Key fingerprint, Let other machines connect, Keys allowed to connect, Machines you connect to | ssh server port pair"
        }
        SettingsSection::Vpn => "Tailscale, NetBird, Nebula | vpn peers network",
        SettingsSection::Vms => "Virtual machines, VM images | firecracker",
        SettingsSection::Sandboxing => {
            "Where terminals run, Devcontainer engine, Network, Allowed hosts only, Inspect HTTPS, Container engines, Sidecar engine, Restart terminals | proxy certificate docker podman passthrough host sandbox default runtime"
        }
        SettingsSection::SandboxFiles => "Additional writable paths, Hidden paths | files folders",
        SettingsSection::SandboxHosts => "Allowed hosts, Common hosts | domains allowlist proxy",
        SettingsSection::Changelog => "Changelog | release notes version changes news what's new",
        SettingsSection::Licenses => {
            "MIT license, Apache License 2.0, Third-party crates | licence open source copyright notices dependencies"
        }
    }
}

fn keywords_de(section: SettingsSection) -> &'static str {
    match section {
        SettingsSection::Project => {
            "Wo Terminals laufen, Umgebungsvariablen, Sandbox-Ordner, Jujutsu, Alle Einstellungen zurücksetzen | projekt jj env geheimnisse schlüsselbund host devcontainer sandbox laufzeit neu bauen"
        }
        SettingsSection::ProjectAllowed => {
            "Geschützte Anfragen, Agenten dürfen Worktrees ohne Nachfrage öffnen, Eintrag hinzufügen | immer erlaubt freigaben antworten git push schlüsselbund"
        }
        SettingsSection::ProjectBlocked => {
            "Geschützte Anfragen, Eintrag hinzufügen | immer blockiert freigaben antworten ablehnen git push schlüsselbund"
        }
        SettingsSection::Font => {
            "Terminalschrift, Schriftgröße im Terminal | text ligaturen font schriftart"
        }
        SettingsSection::ColorScheme => "Farbschema, Dem System folgen | farbe thema dunkel hell",
        SettingsSection::Keyboard => "Keymap, Tastenbelegung | tasten tastenkürzel aufnehmen",
        SettingsSection::Shells => "Standard-Shell | powershell cmd wsl",
        SettingsSection::Language => "Sprache | englisch deutsch english",
        SettingsSection::Extensions => {
            "Berechtigungen, Geschützte Anfragen, Erweiterungen neu laden | lua plugins guards fragen blockieren erlauben"
        }
        SettingsSection::Accounts => {
            "Forge-Konten, Anmelden | github gitlab forgejo gitea bitbucket azure devops token"
        }
        SettingsSection::Registries => {
            "Registry-Logins, Cargo, Forge CLIs, Git über HTTPS, npm, NuGet, PyPI | zugangsdaten tokens"
        }
        SettingsSection::Agents => {
            "Agenten-Integrationen, Hooks aktualisieren, Steuer-Socket, CPU | claude codex gemini antigravity agy installieren Prozessor Kerne nice Limit"
        }
        SettingsSection::Remote => {
            "Schlüssel-Fingerabdruck, Andere Maschinen verbinden lassen, Erlaubte Schlüssel, Maschinen, mit denen du dich verbindest | ssh server port koppeln"
        }
        SettingsSection::Vpn => "Tailscale, NetBird, Nebula | vpn peers netzwerk",
        SettingsSection::Vms => "Virtuelle Maschinen, VM-Images | firecracker",
        SettingsSection::Sandboxing => {
            "Wo Terminals laufen, Devcontainer-Engine, Netzwerk, Nur erlaubte Hosts, HTTPS prüfen, Container-Engines, Sidecar-Engine, Terminals neu starten | proxy zertifikat docker podman durchreichen host sandbox standard laufzeit"
        }
        SettingsSection::SandboxFiles => {
            "Zusätzliche beschreibbare Pfade, Verborgene Pfade | dateien ordner"
        }
        SettingsSection::SandboxHosts => "Erlaubte Hosts, Häufige Hosts | domains allowlist proxy",
        SettingsSection::Changelog => {
            "Änderungen | changelog versionshinweise version neuigkeiten was ist neu"
        }
        SettingsSection::Licenses => {
            "MIT-Lizenz, Apache License 2.0, Crates anderer Autoren | lizenz open source urheberrecht abhängigkeiten"
        }
    }
}

pub static EN: Strings = Strings {
    settings: "Settings",
    notifications: "Notifications",
    notifications_unread: |count| match count {
        1 => "Notifications, 1 unread".to_owned(),
        _ => format!("Notifications, {count} unread"),
    },
    no_notifications: "Nothing new.",
    check_for_updates: "Check for updates",
    checking_for_updates: "Checking…",
    update_all: "Update all",
    clear_notifications: "Clear all",
    close_notifications: "Close notifications",
    updating: "Updating…",
    dismiss: "Dismiss",
    dismiss_notification: |title| format!("Dismiss {title}"),
    update_notification: |title| format!("Update: {title}"),
    notice_title: "Notice",
    integration_outdated: |agent, env| format!("{agent} integration on {env} is outdated"),
    integrations_outdated: |count| format!("{count} agent integrations are outdated"),
    integration_outdated_body: "These hooks were installed by an older version of scriptschnell-term or edited since. Updating installs the current hooks and skills.",
    agent_update: |agent, latest| format!("{agent} {latest} is available"),
    agent_update_body: |current, env, manager, command| {
        format!("{current} is installed on {env} with {manager}. Update runs {command}.")
    },
    agent_updated: |agent, env| format!("{agent} on {env} is up to date"),
    agent_update_failed: |agent, env| format!("Updating {agent} on {env} failed"),
    app_update: |version| format!("scriptschnell-term {version} is available"),
    app_update_body: "Downloads the release, verifies it against the signed manifest, and replaces the installed files.",
    app_release_body: |url| {
        format!(
            "This copy was installed via a package manager or cargo. Update it there, or download the release from {url}."
        )
    },
    app_updated: |version| format!("scriptschnell-term {version} is installed"),
    app_updated_body: "Restart scriptschnell-term to use it.",
    app_update_failed: "Updating scriptschnell-term failed",
    just_now: "just now",
    minutes_ago: |n| format!("{n} min ago"),
    hours_ago: |n| format!("{n} h ago"),
    days_ago: |n| match n {
        1 => "yesterday".to_owned(),
        _ => format!("{n} days ago"),
    },
    close_settings: "Close settings",
    close_escape: "Close (Escape)",
    section_font: "Font",
    section_color_scheme: "Color scheme",
    section_keyboard: "Keyboard",
    section_extensions: "Extensions",
    section_agents: "Agents",
    section_language: "Language",
    section_shells: "Shells",
    default_shell: "Default shell",
    default_shell_system: "System default (%COMSPEC%)",
    shells_note: "New terminals use the default shell. Open terminals are not affected.",
    shells_listed: "Shells shown in the New panel menu after New shell",
    git_bash_found: |path| {
        format!(
            "Git Bash found at {path}. Every terminal sets CLAUDE_CODE_GIT_BASH_PATH to this path for Claude Code."
        )
    },
    git_bash_missing: "Git Bash is not installed. Claude Code requires it on Windows; install Git for Windows and restart the app.",
    new_shell_named: |shell| format!("New shell: {shell}"),
    bundled_font: "Bundled (Liberation Mono)",
    font_preview: "Preview",
    font_picker: "Terminal font",
    keymap_picker: "Keymap",
    smaller_font: "Smaller font",
    larger_font: "Larger font",
    font_size: "Font size",
    size: "Terminal font size",
    system: "System",
    scheme_in_use: "In use",
    follows_desktop: |name| format!("Follows system theme (currently {name})"),
    system_language: |name| format!("System ({name})"),
    dark: "dark",
    light: "light",
    from_extension: |name| format!("extension {name}"),
    from_user: "Customized in settings.json",
    bindings: "Bindings",
    binding_list: "Binding list",
    bindings_hint: "Click a binding and press a new shortcut. Key sequences and unbound actions can be set under \"keybindings\" in settings.json.",
    binding_taken: |keys, action| format!("{keys} was reassigned from {action} (now unbound)"),
    reset_binding: "Reset",
    binding_unset: "Unbound",
    reset_binding_for: |action| format!("Reset the keys of {action}"),
    press_keys: "Press a new shortcut (Esc to cancel)",
    recording_was: |title, keys| format!("{title} (currently {keys})"),
    preset_description: |preset| preset.description,
    off: "Off",
    on: "On",
    failed: "Failed",
    failed_with: |error| format!("Failed: {error}"),
    no_extensions: "No extensions installed.",
    enable_extension: |name| format!("Enable {name}"),
    extension_list: "Extension list",
    all_extensions: "All extensions",
    extension_filter: "Filter by state",
    filter_all: "All",
    filter_active: "On",
    filter_disabled: "Off",
    no_matching_extensions: "No extensions match the filter.",
    find_extension: "Find an extension",
    find_setting: "Find a setting",
    settings_no_match: |query| format!("No setting matches \"{query}\". Esc clears the search."),
    settings_keywords: keywords_en,
    loaded_from: |dir| format!("Loaded from {dir}"),
    reload_note: "Reload after editing extension files. Enabling or disabling an extension reloads all extensions.",
    needs_permissions: |list| format!("Needs permission: {list}"),
    permissions_switch_note: "Toggle individual permissions below.",
    grant: "Grant all",
    revoke: "Revoke all",
    grant_for: |name| format!("Grant {name} its permissions"),
    revoke_for: |name| format!("Revoke the permissions of {name}"),
    permission_for: |name, permission| format!("the {permission} permission for {name}"),
    permission_sensitive: |name| format!("{name} · sensitive"),
    permission_note: |name| match name {
        "terminal" => "Opens terminals and sends input to them",
        "browser" => "Opens browser panels",
        "worktree" => "Switches the active worktree",
        "copy" => "Copies files into devcontainers, VMs and remote machines",
        "run" => "Runs commands in a worktree's environment",
        "trust" => "Adds certificates to an environment's trust store",
        "sandbox" => "Changes sandbox network and file access",
        "secrets" => "Reads and stores secrets in this machine's keyring",
        "guards" => "Intercepts sandbox requests, such as pushes, until you approve them",
        "network" => "Sends HTTP requests and downloads blocklists",
        "agents" => "Registers agent tools and custom coding agents",
        "environments" => "Adds custom environments whose commands run through a local program",
        "storage" => "Stores extension data globally across the app",
        "project_storage" => "Stores extension data per project",
        "filesystem" => "Reads and writes files outside its own folder",
        "process" => "Starts programs on this machine",
        "env" => "Reads this machine's environment variables",
        _ => "",
    },
    builtin_page: "Built-in rules",
    builtin_subtitle: "always on",
    builtin_description: "Built-in rules controlling external actions from sandboxes. They apply even without extensions.",
    extension_page: "Extension page",
    no_description: "No description provided.",
    extension_enabled: "Enabled",
    permissions_heading: "Permissions",
    no_permissions: "No permissions required.",
    settings_heading: "Settings",
    ext_guards_heading: "Guarded requests",
    list_empty: "No entries",
    list_new_entry_for: |title| format!("New entry for {title}"),
    list_add_to: |title| format!("Add to {title}"),
    list_remove: "Remove",
    list_remove_from: |entry, title| format!("Remove {entry} from {title}"),
    list_duplicate: |entry| format!("{entry} is already in the list"),
    guards_heading: "External sandbox actions",
    guards_note: "Ask prompts you before allowing each request. Unanswered requests are blocked after two minutes.",
    guard_ask: "Ask",
    guard_block: "Block",
    guard_allow: "Allow",
    guard_mode_for: |title, mode| format!("{title}: {mode}"),
    guard_from: |extension, hosts| format!("{extension}, {hosts}"),
    every_host: "every host",
    approval_dialog: "Sandbox request",
    approval_question: |title| format!("A sandbox wants to {title}"),
    approval_note: "Your choice applies to this host for one minute.",
    approval_project: |name| format!("From project {name}"),
    approval_always: "Always allow in this project",
    approval_never: "Always block in this project",
    always_heading: "Always allowed",
    always_note: "Actions permanently allowed for a project in approval dialogs, including sandbox requests and agents creating worktrees or starting other agents.",
    always_entry: |title, host| format!("{title}: {host}"),
    always_remove: "Remove",
    change_requests_heading: "Pull and merge request permissions",
    change_requests_note: "These defaults apply to all forge extensions, such as GitHub and GitLab. Settings on an individual extension's page take precedence.",
    change_request_create: "Open a pull or merge request",
    change_request_comment: "Comment on or review a pull or merge request",
    change_request_merge: "Merge or complete a pull or merge request",
    change_request_subtitle: |_| "Applies to all forge extensions".to_owned(),
    approval_sends: |detail| format!("Sends: {detail}"),
    keyring_request: |action, label| match action {
        "read" => format!("read \"{label}\""),
        "change" => format!("change \"{label}\""),
        "delete" => format!("delete \"{label}\""),
        _ => format!("create a secret in \"{label}\""),
    },
    keyring_note: "Your choice applies to this secret for one minute.",
    keyring_scope: "this session's keyring",
    agent_settings_request: |agent, file| format!("{agent} changes {file}:"),
    agent_settings_removed: |key| format!("{key}: removed"),
    agent_settings_more: |n| format!("and {n} more keys"),
    agent_settings_note: "Until allowed, the change stays inside the sandbox. Your choice applies to this file for one minute.",
    agent_settings_scope: "settings agents change in sandboxes",
    open_url_request: |link| format!("Open {link}"),
    open_url_note: "The link opens in your browser on this machine, where you are logged in. Allow it when you started what opens it, such as a login. A port on localhost that the link names reaches the sandbox's server on that port for 10 minutes.",
    open_url_scope: "links programs in sandboxes open",
    approval_allow: "Allow",
    approval_block: "Block",
    control_dialog: "Extension change",
    control_install: |name| format!("An agent wants to install the extension {name}"),
    control_change: |name| format!("An agent wants to change the extension {name}"),
    control_source: |place, machine, kind| {
        format!("From a terminal in {place} on {machine} ({kind})")
    },
    control_file_added: |path, lines| format!("{path}: new, {lines} lines"),
    control_file_changed: |path, lines| format!("{path}: replaced, now {lines} lines"),
    control_file_removed: |path| format!("{path}: removed"),
    control_more_files: |n| format!("and {n} more files"),
    control_permissions: |list| {
        format!("Requested permissions: {list}. You can grant them on the Extensions page.")
    },
    control_no_permissions: "No permissions requested.",
    control_shipped: "This is a built-in extension. Modified files will no longer be updated automatically.",
    control_note: "Extensions run in the app, outside every sandbox. Allow only code you have read.",
    control_deny: "Deny",
    control_applied: |name| format!("An agent changed the extension {name}"),
    control_failed: |error| format!("Control socket: {error}"),
    control_settings_dialog: "Settings change",
    control_settings_question: "An agent suggests a settings change",
    control_reason: |reason| format!("Why: {reason}"),
    control_setting_set: |path, before, after| format!("{path}: {before} → {after}"),
    control_setting_add: |path, items| format!("{path}: add {items}"),
    control_setting_remove: |path, items| format!("{path}: remove {items}"),
    control_settings_note: "Allowing writes settings.json immediately. You can revert this in Settings (Ctrl+,).",
    control_settings_applied: "An agent changed the settings",
    control_launch_dialog: "Start agents",
    control_new_worktree: |branch, project| {
        format!("An agent wants to create the worktree {branch} in {project}")
    },
    control_open_worktree: |path| format!("An agent wants to switch to {path}"),
    control_new_detached: |project| {
        format!("An agent wants to create a detached worktree in {project}")
    },
    control_worktree_from: |revision| format!("starting at {revision}"),
    control_worktree_from_main: "starting at the main worktree's revision",
    control_start_agent: |command| format!("and start {command} there"),
    control_type_prompt: |prompt| format!("then type its first prompt: {prompt}"),
    control_launch_note: "Agents start in new terminals for that worktree, using its configured environment (host, sandbox, or devcontainer).",
    control_allow_session: "Allow in this session",
    always_launches: "Create worktrees and start agents",
    reload_extensions: "Reload extensions",
    agent_on: |agent, env| format!("{agent} on {env}"),
    working: "working…",
    install: "Install",
    update: "Update",
    remove: "Remove",
    retry: "Retry",
    agent_button: |action, agent| format!("{action} {agent} integration"),
    agent_list: "Agent list",
    agents_note: [
        "Installed agents report when they are working, finished, or waiting",
        "for input or approval; the sidebar highlights their worktree.",
    ],
    agents_skill: [
        "They also receive two skills covering the settings, extension,",
        "and sandbox documentation.",
    ],
    hooks_run: "Hooks report agent activity via a small script that scriptschnell-term installs and keeps up to date.",
    check_agents: "Check agents again",
    agents_update_all: |count| format!("Update all ({count})"),
    agents_installed: "Agent integrations",
    agents_outdated: |count| {
        format!("{count} agents have outdated hooks. Updating installs the current hook script.")
    },
    agent_control: "Control socket",
    agent_control_on: "Enabled everywhere, including sandboxes",
    agent_control_local: "Enabled for this machine and its sandboxes",
    agent_control_host: "Enabled outside sandboxes only",
    agent_control_off: "Off",
    agent_control_note: "Lets agents request changes to extensions, settings, or worktrees. Each request requires your approval.",
    checking: "Checking…",
    not_installed: "Not installed",
    on_outdated: "On, hooks outdated",
    note_trust_hooks: "Codex asks to trust new hooks before it runs them.",
    note_enable_hooks: "Turn on hooks in ~/.codex/config.toml: [features] hooks = true",
    note_next_session: "Takes effect in new sessions.",
    language: "Language",

    command_palette: "Command palette",
    minimize_window: "Minimize",
    maximize_window: "Maximize",
    restore_window: "Restore",
    close_window: "Close window",
    palette_keys_hint: "↑↓ navigate · Enter run · Esc close",
    palette_areas_hint: |keys| format!("{keys} switch areas"),
    palette_empty_keys_hint: "Esc close",
    palette_input_keys_hint: "Enter confirm · Esc back",
    palette_create: "Create",
    palette_clone: "Clone",
    palette_more: |from, to, total, below| {
        if below > 0 {
            format!("{from}–{to} of {total} · {below} more ↓")
        } else {
            format!("{from}–{to} of {total}")
        }
    },
    palette_confirm: |label| format!("{label}? Press Enter again to confirm"),
    new_terminal: "New shell",
    close_panel: "Close panel",
    close_panel_named: |pane, title| format!("Close pane {pane}: {title}"),
    move_pane: |pane| format!("Drag to move pane {pane}"),
    pane_menu: |pane| format!("Pane {pane} menu"),
    drag_tab_hint: "Drop on an edge to split, or between tabs to reorder (Esc to cancel)",
    drag_shown_tab_hint: "Drop between tabs to reorder (Esc to cancel)",
    drag_pane_hint: "Drop on an edge to split, center to swap, or tab/+ to move (Esc to cancel)",
    zone_left: "Split left",
    zone_right: "Split right",
    zone_top: "Split above",
    zone_bottom: "Split below",
    zone_swap: "Swap",
    drop_into_tab: "Move into this tab",
    drop_new_tab: "Move to a new tab",
    close_pane: "Close pane",
    resize_panes: "Resize panes",
    close_tab: "Close tab",
    close_other_tabs: "Close other tabs",
    move_pane_to_tab: "Move pane to new tab",
    split_side_by_side: "Split side by side",
    split_stacked: "Split stacked",
    focus_next_pane: "Focus next pane",
    focus_previous_pane: "Focus previous pane",
    grow_pane: "Grow pane",
    shrink_pane: "Shrink pane",
    next_tab: "Next tab",
    previous_tab: "Previous tab",
    toggle_sidebar: "Toggle sidebar",
    hide_sidebar: "Hide sidebar",
    show_sidebar: "Show sidebar",
    tab_n: |n| format!("Tab {n}"),
    focus_pane_n: |n| format!("Focus pane {n}"),
    send_text: |text| format!("Send {text}"),

    projects: "Projects",
    change_requests: "Change requests",
    sidebar: "Sidebar",
    adding: |input| format!("adding {input}…"),
    set_token: |var| format!("Set {var} to list change requests"),
    open_host: |host| format!("Open {host}"),
    new_worktree_from: |name| format!("New worktree from {name}"),
    worktree_source: |name| format!("From {name}"),
    loading: "loading…",
    open_project: "Open project",
    worktree_count: |description, count| format!("{description} ({count} worktrees)"),
    ci_ok: "ci ok",
    ci_failed: "ci failed",
    ci_running: "ci running",
    ci_pending: "ci pending",
    ci_canceled: "ci canceled",
    locked: "locked",
    meta_changed: "changed",
    meta_new: "new",
    missing: "missing",
    busy: "busy",
    done: "done",
    input: "input",
    approve: "approve",
    busy_description: "An agent is working",
    done_description: "An agent finished and is waiting for the next prompt",
    input_description: "Waiting for input",
    approve_description: "An agent is waiting for approval",

    new_shell: "New shell",
    start_agent: |agent| format!("Start {agent}"),
    new_panel: "New panel",
    new_panel_tip: "New panel. Drop a pane here to open it in a new tab",
    browse_files: "Browse files",
    files: "Files",
    files_count: |n| {
        if n == 1 {
            "1 file".to_owned()
        } else {
            format!("{n} files")
        }
    },
    files_lines: |n| {
        if n == 1 {
            "1 line".to_owned()
        } else {
            format!("{n} lines")
        }
    },
    files_truncated: "first 16 MiB",
    files_loading: "Loading…",
    files_empty: "No files",
    files_pick: "Select a file on the left and press Enter to open it.",
    files_binary: |size| format!("Binary file, {size} bytes"),
    files_unreadable: |error| format!("Could not read the file: {error}"),
    files_unrendered: |error| format!("Could not render the file: {error}"),
    files_mermaid_unsupported: |reason| format!("Unsupported Mermaid diagram: {reason}"),
    files_pixels: |width, height| format!("{width} × {height} px"),
    files_page: |page, pages| format!("page {page} of {pages}"),
    files_show_source: "press v for source",
    files_show_rendered: "press v for rendered view",
    explore_history: "History",
    explore_list_commits: "Commits",
    explore_list_files: "Files",
    explore_review: "Review",
    explore_open_history: "Explore history",
    status_open_history: "Show branch history",
    status_open_files: "Browse worktree files",
    status_open_machines: "Open machine settings",
    explore_open_review: "Review changes",
    explore_review_against: "Review changes against a branch",
    explore_pick: "Select an entry on the left. Press v to switch between history, review, and change request.",
    explore_no_commits: "No commits yet",
    explore_sideways: "← → to scroll long lines",
    explore_no_changes: |base| format!("No changes against {base}."),
    explore_no_base: "No base branch found. Press b to choose one.",
    explore_no_changes_hint: |base| {
        format!(
            "Commits and uncommitted changes not on {base} appear here. Press b to compare against another branch."
        )
    },
    explore_failed: |error| format!("Could not read history: {error}"),
    explore_unrelated: |base| {
        format!(
            "This branch shares no commit history with {base}, so there is nothing to compare. Press b to compare against another branch."
        )
    },
    explore_review_failed: |error| format!("Could not compare changes: {error}"),
    explore_against: |base| format!("against {base}"),
    explore_from: |commit| format!("from {commit}"),
    explore_commits: |n| {
        if n == 1 {
            "1 commit".to_owned()
        } else {
            format!("{n} commits")
        }
    },
    explore_uncommitted: "with uncommitted changes",
    explore_truncated: "first 32 MiB",
    debug_browser: "Debug browser",
    debug_browser_open: "Debug browser",
    debug_browser_url_ask: "Open URL in debug browser…",
    expose_port: |port| format!("Expose port {port}"),
    stop_exposing_port: |port| format!("Stop exposing port {port}"),
    port_exposed: |port, host| format!("Port {port} is open on this machine at 127.0.0.1:{host}"),
    port_expose_failed: |port, error| format!("Could not expose port {port}: {error}"),
    debug_browser_failed: "Debug browser failed to start",
    debug_browser_retry: "Try again",
    debug_browser_alt: "Page in the debug browser",
    monitor: "Monitor",
    monitor_note: "This machine, updated every second",
    monitor_open: "Monitor machine",
    monitor_waiting: "Measuring…",
    monitor_no_proc: "System monitor requires Linux /proc, which is not available on this machine.",
    monitor_failed: |error| format!("Could not read system metrics: {error}"),
    monitor_cpu: "CPU",
    monitor_memory: "Memory",
    monitor_swap: "Swap",
    monitor_network: "Network",
    monitor_disk: "Disk",
    monitor_cpu_history: "CPU history",
    monitor_net_history: "Network history",
    monitor_net_down: "↓ in",
    monitor_net_up: "↑ out",
    monitor_peak: "peak",
    monitor_now: "now",
    monitor_collecting: "Collecting history (one sample per second)…",
    copy_command: "Copy install command",
    copy_command_hint: "Copies the install command. Paste it into a terminal.",
    copy_install_for: |agent| format!("Copy install command for {agent}"),
    command_copied: "Command copied",
    agent_install_with: |command| format!("Not installed · {command}"),
    agent_turn_on: "Turn on",
    tab_with_panes: |title, panes| format!("{title} · {panes} panes"),
    tabs_more: |count| format!("{count} more"),
    tabs_hidden: |count| {
        if count == 1 {
            "1 more tab".to_owned()
        } else {
            format!("{count} more tabs")
        }
    },
    palette_group_project: "Project",
    commands: "Commands",
    palette_group_agents: "Agents",
    palette_group_panels: "Panels",
    palette_group_shells: "Shells",
    palette_group_layout: "Tabs and panes",
    palette_group_app: "App",
    palette_group_worktrees: "Worktrees",
    palette_group_runtime: "Runtime",
    palette_group_extensions: "Extensions",
    tasks_col_state: "State",
    tasks_col_kind: "Kind",
    tasks_col_panel: "Panel",
    tasks_col_where: "Where",
    tasks_col_task: "Task",
    tasks_col_schedule: "Schedule",
    tasks_col_thread: "Thread",
    tasks_col_cpu: "CPU",
    tasks_col_time: "CPU time",
    monitor_up: "up",
    monitor_days: "d",
    monitor_load: "load",
    monitor_tasks: "tasks",
    monitor_running: "running",
    tasks: "Tasks",
    tasks_note: "Open panels, extension tasks, and app threads",
    tasks_refresh: "Reload",
    tasks_open: "Show tasks",
    tasks_summary: |s| {
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut out = format!(
            "{} in {} · {}",
            plural(s.panels, "panel", "panels"),
            plural(s.worktrees, "worktree", "worktrees"),
            plural(s.terminals, "terminal", "terminals"),
        );
        for (n, word) in [
            (s.frozen, "frozen"),
            (s.exited, "exited"),
            (s.waiting, "waiting for you"),
        ] {
            if n > 0 {
                out.push_str(&format!(", {n} {word}"));
            }
        }
        if let Some(threads) = s.threads {
            out.push_str(&format!(" · {}", plural(threads, "thread", "threads")));
        }
        out
    },
    tasks_panels: "Panels",
    tasks_extension: "Extension tasks",
    tasks_threads: "App threads",
    tasks_none: "None.",
    tasks_no_threads: "Thread listing requires /proc/self/task, which is not available on this system.",
    tasks_idle_threads: |count| match count {
        1 => "1 idle thread".to_owned(),
        _ => format!("{count} idle threads"),
    },
    tasks_shown_worktree: "active",
    tasks_shown: "visible",
    tasks_hidden: "in background",
    tasks_frozen: "frozen",
    tasks_thawing: "resuming",
    tasks_exited: "exited",
    tasks_killed: "killed",
    tasks_running: "running",
    tasks_sleeping: "idle",
    tasks_busy: "busy",
    tasks_due: "due",
    tasks_scheduled: "scheduled",
    tasks_kind: |kind| {
        use crate::taskview::Kind;
        match kind {
            Kind::Terminal => "terminal",
            Kind::Browser => "browser",
            Kind::Files => "files",
            Kind::Explore => "explore",
            Kind::Monitor => "monitor",
            Kind::DebugBrowser => "debug browser",
            Kind::Tasks => "tasks",
        }
    },
    tasks_agent_state: |state| {
        use crate::term::Attention;
        match state {
            Attention::None => "",
            Attention::Busy => "working",
            Attention::Done => "waiting for a prompt",
            Attention::Input => "waiting for input",
            Attention::Permission => "waiting for approval",
        }
    },
    tasks_place: |tab, pane| match pane {
        Some(pane) => format!("tab {tab}, pane {pane}"),
        None => format!("tab {tab}"),
    },
    tasks_exit_code: |code| format!("exit code {code}"),
    tasks_age: |age| format!("open for {age}"),
    tasks_freezes_in: |left| format!("freezes in {left}"),
    tasks_every: |every| format!("every {every}"),
    tasks_last: |since| format!("last run {since} ago"),
    tasks_next: |left| format!("next in {left}"),
    tasks_never: "never run",
    pane_n: |n| format!("Pane {n}"),
    press: |keys| format!("Press {keys}"),
    open_a_project: "Open a project",
    open_the_palette: "Open the command palette",
    then_open_project: |first| format!("{first} and choose Open project."),
    no_panels: "No panels",
    no_panels_message: |how| format!("This worktree has no panels. {how} to open a shell."),
    use_new_shell: "Click the + button next to the tabs",
    no_project: "No project selected",
    select_project: "Select a project in the sidebar.",
    status_bar: "Status bar",

    prompt_project: "Path, host:/path, ssh://, tailscale:// or a repository URL",
    prompt_clone_folder: "Folder to clone into",
    prompt_review_base: "Base branch or revision (leave empty for project default)",
    clone_from: |url| format!("Clone {url} to"),
    clone_here: "This machine",
    clone_here_devcontainer: "This machine (devcontainer)",
    clone_in_vm: |name| format!("VM {name} on this machine"),
    clone_on: |address| format!("Remote machine {address}"),
    clone_devcontainer_on: |address| format!("Remote machine {address} (devcontainer)"),
    clone_vm_on: |address| format!("VM on remote machine {address}"),
    clone_vm_name: "VM name",
    prompt_branch: "Branch name",
    prompt_worktree: "Worktree name",
    prompt_url: "URL",
    prompt_command: "Type a command or a worktree name",
    prompt_panel: "Find a panel",
    open_project_ask: "Open project…",
    new_worktree_ask: "New worktree…",
    open_url_ask: "Open URL…",
    switch_to: |name| format!("Switch to {name}"),
    switch_to_worktree: |project, branch| format!("Switch to {project} / {branch}"),
    remove_worktree: |branch| format!("Remove worktree {branch}"),
    vcs_fetch: "Fetch",
    vcs_fetch_project: |project| format!("Fetch {project}"),
    vcs_pull: |branch| format!("Pull {branch}"),
    vcs_push: |branch| format!("Push {branch}"),
    vcs_push_to: |branch, remote| format!("Push {branch} to {remote}"),
    vcs_fetching: |project| format!("Fetching {project}…"),
    vcs_pulling: |branch| format!("Pulling {branch}…"),
    vcs_pushing: |branch| format!("Pushing {branch}…"),
    vcs_fetched: |project| format!("Fetched {project}"),
    vcs_pulled: |branch| format!("Pulled {branch}"),
    vcs_pushed: |branch, remote| format!("Pushed {branch} to {remote}"),
    vcs_fetch_failed: |project, reason| format!("Could not fetch {project}: {reason}"),
    vcs_pull_failed: |branch, reason| format!("Could not pull {branch}: {reason}"),
    vcs_push_failed: |branch, reason| format!("Could not push {branch}: {reason}"),
    vcs_busy: |name| format!("{name} is still fetching, pulling or pushing"),
    forget_project: |name| format!("Forget project {name}"),
    open_temporary_ask: "Open project temporarily…",
    global_session_ask: "New global session…",
    prompt_global_session: "Folder, host:, or ssh://host/path (leave empty for home directory)",
    global_session: "Global session",
    temporary: "temporary",
    keep_session: |name| format!("Keep session {name}"),
    close_session: |name| format!("Close session {name}"),
    agents_tab: "Agents",
    agents_tab_count: |count| format!("Agents ({count})"),
    agents_finished: "Finished, not merged",
    agents_working: |count| format!("Working ({count})"),
    agents_merged: |count| format!("Merged ({count})"),
    agents_earlier: |count| format!("Earlier sessions ({count})"),
    agents_none: "No agent sessions yet",
    agents_none_finished: "No finished agents",
    agents_none_hint: "Agents started in a worktree appear here along with their current status.",
    agents_start: "Start an agent…",
    change_open: "open",
    change_merged: "merged",
    change_closed: "closed",
    forge_accounts: "Accounts",
    forge_signed_in_as: |user| format!("Signed in as {user}"),
    forge_signed_in: "Signed in",
    forge_signed_out: "Not signed in",
    forge_signing_in: "Verifying token…",
    forge_from_env: |var| format!("Token from {var}"),
    forge_failed: |error| format!("Sign-in failed: {error}"),
    forge_create_token: "Create token",
    forge_create_token_hint: "Opens the forge's token page in your browser",
    forge_create_token_for: |host| format!("Create a token on {host}"),
    forge_token_for: |host| format!("Token for {host}"),
    forge_sign_in: "Sign in",
    forge_sign_in_for: |host| format!("Sign in to {host}"),
    forge_sign_out: "Sign out",
    forge_sign_out_for: |host| format!("Sign out of {host}"),
    forge_token_missing: "Paste a token first",
    extension_request_failed: |error| format!("An extension task failed: {error}"),
    forge_new_account_for: |forge| format!("New {forge} account"),
    forge_add_account: "Add account",
    forge_add_account_open: "+ Other host…",
    forge_other_host: "Add an account on another host…",
    forge_open_add_for: |forge| format!("Add another {forge} account"),
    forge_sign_in_open: "Sign in…",
    forge_open_sign_in_for: |host| format!("Open sign-in for {host}"),
    forge_add_account_for: |forge| format!("Add {forge} account"),
    forge_remove_account_for: |account| format!("Remove account {account}"),
    forge_not_an_account: |entry| {
        format!("{entry} must be a host or host/path, such as example.com/team")
    },
    toast_done: "Done",
    toast_failed: "Failed",
    toast_signed_in: |account, user| {
        if user.is_empty() {
            format!("Signed in to {account}")
        } else {
            format!("Signed in to {account} as {user}")
        }
    },
    toast_sign_in_failed: |account, error| format!("Sign-in to {account} failed: {error}"),
    toast_signed_out: |account| format!("Signed out of {account}"),
    toast_account_added: |account| format!("Added {account}"),
    toast_account_removed: |account| format!("Removed {account}"),
    toast_trusted: |address| format!("Trusted {address}"),
    toast_forgotten: |address| format!("Forgot {address}"),
    toast_fused: |address| format!("Showing the projects of {address}"),
    toast_unfused: |address| format!("No longer showing the projects of {address}"),
    toast_key_allowed: "Key allowed",
    toast_key_removed: "Key removed",
    toast_stopped_serving: "Stopped serving this machine",
    toast_saved: "Saved",
    toast_image_ready: |name| format!("The {name} image is ready"),
    toast_image_failed: |name, error| format!("The {name} image failed: {error}"),
    toast_disk_deleted: |name| format!("Deleted disk for {name}"),
    toast_agent_installed: |agent, env| format!("Installed the {agent} integration on {env}"),
    toast_agent_removed: |agent, env| format!("Removed the {agent} integration from {env}"),
    toast_agent_failed: |agent, env, error| format!("{agent} on {env}: {error}"),
    toast_host_added: |host| format!("Added {host}"),
    toast_path_added: |path| format!("Added {path}"),
    toast_project_base: |base| match base {
        Some(base) => format!("Reviews compare against {base}"),
        None => "Reviews will auto-detect the base branch".to_owned(),
    },
    toast_project_env_keyring: |name| format!("Saved {name} in the system keyring"),
    toast_project_env_plain: |name| format!("Saved {name} in state.json"),
    toast_project_env_session: |name| format!("Set {name} for this session"),
    toast_project_env_fallback: |name, error| {
        format!("Saved {name} in state.json (keyring unavailable: {error})")
    },
    toast_project_env_removed: |name| format!("Removed {name}"),
    project_env_loaded_notice: |project| {
        format!(
            "Loaded environment variables for {project} from the keyring. Restart terminals to apply."
        )
    },
    section_accounts: "Accounts",
    accounts_forges: "Forges",
    forge_used_by: |forge, project| format!("{forge} · used by {project}"),
    accounts_forges_note: "The Agents tab checks these forges to see if finished agent branches have change requests and whether they are merged. Each account stores its token in the system keyring. The extension can send requests to each account's host and subdomains.",
    accounts_no_forges: "No forge extension is enabled. Enable GitHub, GitLab, Forgejo, Gitea, Bitbucket, or Azure DevOps on the Extensions page.",
    accounts_example: |example| format!("e.g. {example}"),
    accounts_logins: "Registry logins",
    accounts_logins_note: "These extensions pass placeholder tokens into sandboxes, and the proxy substitutes real credentials for matching hosts. Update credentials in their source file or environment variable; new sandboxes pick them up on start.",
    logins_file: |path| format!("Reads file {path}"),
    logins_variable: |name| format!("Reads variable {name}"),
    logins_other: |source| format!("Reads {source}"),
    logins_extension_for: |name| format!("Open the {name} extension"),
    logins_extension: "Settings ›",
    logins_for: |hosts| format!("Logins for {hosts}"),
    logins_none: "No logins found",
    logins_no_sources: "No enabled extension passes registry logins into sandboxes.",
    logins_off: "Disabled. Enable it on the Extensions page.",
    open_accounts: "Open Accounts",
    ext_accounts_moved: "Manage accounts and sign-ins on the Accounts page.",
    ext_hosts_heading: "Hosts",
    ext_hosts_note: "Network requests and blocklist downloads are limited to these hosts, configured account hosts, and hosts listed in settings.",
    ext_hosts_none: "No additional hosts.",
    not_a_host: |entry| format!("{entry} is not a host name or *.domain"),
    not_a_url: |entry| format!("{entry} is not an https:// URL"),
    discussion_open: |title| format!("Discussion of {title}"),
    discussion_loading: "Loading discussion…",
    discussion_waiting: "Waiting for the repository and its change request…",
    discussion_no_change: "This branch has no change request.",
    discussion_unsupported: "No enabled extension supports change requests for this repository's forge. Enable the matching forge extension under Settings › Extensions (GitHub, GitLab, Forgejo, Gitea, Bitbucket, Azure DevOps), then sign in under Accounts.",
    discussion_failed: |error| format!("Could not reach the forge: {error}"),
    git_dir_replaced: |path| {
        format!(
            "{path}/.git no longer points to the original repository (a sandbox may have replaced it). \
             scriptschnell-term ignores it, but git in host terminals will still read it."
        )
    },
    discussion_by: |author| format!("by {author}"),
    discussion_on_forge: "on the forge",
    discussion_pushes: "Pushes",
    discussion_pushed: "pushed",
    discussion_force_pushed: "force-pushed",
    discussion_rewritten: "rewritten",
    discussion_conversation: "Conversation",
    discussion_line: |path, line| format!("`{path}` line {line}"),
    discussion_unchanged: |since| {
        if since.is_empty() {
            "Unchanged.".to_owned()
        } else {
            format!("Unchanged since `{since}`.")
        }
    },
    discussion_moved: |old, since| {
        if since.is_empty() {
            format!("Moved from line {old}.")
        } else {
            format!("Moved from line {old} since `{since}`.")
        }
    },
    discussion_changed: |old, since| {
        if since.is_empty() {
            format!("Line {old} was modified; showing the updated line.")
        } else {
            format!("Line {old} was modified since `{since}`; showing the updated line.")
        }
    },
    discussion_found: |old| {
        format!(
            "Matched by content (formerly line {old}); its original commit is not in this repository."
        )
    },
    discussion_file_gone: |since| {
        if since.is_empty() {
            "File was deleted.".to_owned()
        } else {
            format!("File deleted since `{since}`.")
        }
    },
    discussion_line_gone: |old, since| {
        if since.is_empty() {
            format!("Line {old} was removed.")
        } else {
            format!("Line {old} removed since `{since}`.")
        }
    },
    discussion_removed: |old| format!("On removed base line {old}."),
    discussion_resolved: "resolved",
    discussion_outdated: "outdated on the forge",
    explore_change: "Change request",
    explore_open_change: "Show the change request",
    change_overview: "Overview",
    change_checks: "Checks",
    change_checks_label: |passed, total| format!("Checks {passed}/{total}"),
    change_checks_summary: |passed, failed, waiting| {
        let mut parts = vec![format!("{passed} passed")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if waiting > 0 {
            parts.push(format!("{waiting} running or waiting"));
        }
        parts.join(", ")
    },
    change_threads: |open, resolved| format!("Threads: {open} open, {resolved} resolved"),
    change_reviews: "Reviews",
    change_approved: "approved",
    change_requested_changes: "requested changes",
    change_commented: "commented",
    change_review_pending: "review requested",
    change_draft: "draft",
    change_conflicts: "Has merge conflicts with the target branch.",
    change_pipeline: "Pipeline",
    change_sections: "Change request sections",
    change_reload: "Reload",
    change_open_extensions: "Open extension settings",
    change_open_on_forge: "Open on forge",
    change_open_link: "Open",
    change_open_check: |name| format!("Open {name}"),
    change_description: "Description",
    change_no_description: "No description.",
    change_line: |line| format!("line {line}"),
    search_worktrees: "Search worktrees",
    focus_tabs: "Move focus: tab strip, status bar, sidebar, panel",
    focus_back: "Move focus back: sidebar, status bar, tab strip, panel",
    search_no_match: "No matching worktree",
    title_worktrees_by_branch: "Name worktrees by branch",
    title_worktrees_by_agent: "Name worktrees by last agent session",
    use_system_title_bar: "Use the system's title bar",
    use_app_title_bar: "Draw the title bar in the tab strip",
    jj_on: "Use jj for jj repositories",
    cpu_mode_local: |mode| format!("CPU for terminals on this machine: {mode}"),
    cpu_mode_remote: |mode| format!("CPU for terminals on other machines: {mode}"),
    cpu_off: "off",
    cpu_balanced: "balanced",
    cpu_quiet: "quiet",
    cpu_detail: cpu_detail_en,
    cpu_status: |mode| format!("CPU {mode}"),
    cpu_status_hint: |status, next| format!("{status}, click to switch to {next}"),
    cpu_status_tip: |mode, next| format!("CPU mode: {mode}. Click for {next}."),
    cpu_heading: "CPU",
    cpu_note: "Limits total CPU usage across terminals on a machine so it stays responsive during builds. Agents in background terminals also run at lower priority. Changes apply immediately to running terminals. Customize balanced and quiet limits via cpu_mode in settings.json.",
    cpu_local: "This machine, its sandboxes, devcontainers and VMs",
    cpu_remote: "Other machines: SSH hosts and remote machines",
    jj_off: "Stop using jj for jj repositories",
    project_jj_on: "Use jj in this project",
    project_jj_off: "Stop using jj in this project",
    run_task: |title| format!("Run task: {title}"),
    no_folder_chooser: "no native folder chooser on this system",
    open_folder: "Browse…",
    choose_folder: "Choose folder",
    choose_a_folder: "Choose a folder",
    branch_optional: "Branch (optional, empty for detached)",
    creates_next_to: |repo| {
        format!("Creates {repo}-<name> next to {repo}. Tab moves to the branch field.")
    },
    no_matching_command: "No matching command or worktree",
    no_matching_panel: "No matching panel",

    state_not_loaded: |error| format!("state not loaded: {error}"),
    settings_not_loaded: |error| format!("settings not loaded: {error}"),
    state_not_saved: |error| format!("state not saved: {error}"),
    settings_not_saved: |error| format!("settings not saved: {error}"),
    not_a_path: |input| format!("not a path: {input}"),
    repository_not_loaded: "repository not loaded yet",
    bad_worktree_name: |name| format!("bad worktree name {name:?}"),
    bad_branch_name: "Branch names cannot contain spaces or ~ ^ : ? * [ \\ ..",
    bad_folder_name: "Folder names cannot contain / or \\, and cannot be . or ..",
    max_panes: |max| format!("a tab shows at most {max} panes"),
    blocklist_failed: |id, error| format!("blocklist {id}: download failed: {error}"),
    warning: |message| format!("warning: {message}"),
    error: |message| format!("error: {message}"),
    no_worktree_at: |path| format!("no worktree at {path}"),
    view_unknown: |view| format!("no extension provides the view {view}"),
    view_gone: |view| {
        format!(
            "{view} is no longer available. Its extension may be disabled, or the item was deleted."
        )
    },
    terminal_failed: |error| format!("terminal failed: {error}"),
    sandbox_rules_dropped: |groups| {
        format!("sandbox-exec rejected some sandbox rules; sandboxes are running without: {groups}")
    },
    sandbox_shell_replaced: |shell, with| {
        format!(
            "{shell} cannot run in the Windows sandbox because it blocks Git's MSYS shared memory; using {with} instead"
        )
    },
    sandbox_claude_without_bash: "Claude Code cannot start programs in the Windows sandbox: its hooks, shell commands and tools fail with \"operation not permitted\". Run it on the host or in a devcontainer instead.",
    terminal_failed_start: |code, output| {
        let status = code.map_or_else(|| "signal".to_owned(), |code| format!("code {code}"));
        if output.is_empty() {
            format!("terminal exited immediately ({status})")
        } else {
            format!("terminal exited immediately ({status}): {output}")
        }
    },
    paste_image_failed: |error| format!("Could not paste the image: {error}"),
    run_on_host: "Run terminals on the host",
    run_in_devcontainer: "Run terminals in the devcontainer",
    run_in_sandbox: "Run terminals in a sandbox",
    rebuild_devcontainer: "Rebuild the devcontainer",
    runtime_devcontainer: "devcontainer",
    runtime_sandbox: "sandbox",
    runtime_sandbox_open: "sandbox · weakened",
    runtime_sandbox_open_hint: "Containers started here run outside the sandbox. Click to open Sandboxing settings.",
    open_sandboxing: "Open sandbox settings",
    runtime_starting: "devcontainer starting",
    no_devcontainer: "this worktree has no devcontainer.json",
    env_not_ready: "the worktree's environment is not ready yet",
    devcontainer_ready: |name| format!("devcontainer {name} is ready"),
    devcontainer_failed: |error| format!("devcontainer failed: {error}"),
    copy_failed: |error| format!("copy failed: {error}"),
    run_failed: |error| format!("command failed: {error}"),
    trust_failed: |error| format!("adding the certificate failed: {error}"),
    section_remote: "Remote machines",
    section_vms: "Virtual machines",
    section_sandboxing: "Sandboxing",
    section_sandbox_files: "Files",
    section_sandbox_hosts: "Hosts",
    this_machine: "This machine",
    your_key: "Your key fingerprint",
    your_key_named: |fingerprint| format!("Your key {fingerprint}"),
    copy_key_short: "Copy key",
    key_copied: "public key copied",
    serve_machine: "Let other machines connect",
    when_machines_connect: "Access for connecting machines",
    pairing_steps: "To pair two machines, copy your key and allow it on the other machine, then add that machine under \"Machines you connect to\". For access the other way, do the same there.",
    serve_on: |address| format!("When enabled, other machines can connect at {address}"),
    serving_on: |address| format!("Serving on {address}"),
    any_address: |port| format!("port {port} on all network interfaces"),
    serve_failed: |error| format!("could not serve this machine: {error}"),
    offer_host: "Offer this machine's shell",
    offer_host_note: "Default for new keys. When off, clients can only access VMs.",
    offer_host_no_vms: "Keep enabled until VMs are set up.",
    share_projects: "Share this machine's projects",
    share_projects_note: "Default for new keys. Connected clients display shared projects in their sidebar.",
    web_links_vm_only: "Restrict linked web servers to VMs only",
    web_links_vm_only_note: "When enabled, linked web servers can access VMs only, not this machine's shell.",
    allowed_keys: "Keys allowed to connect",
    settings_results: "PAGES THAT MATCH",
    allowed_keys_not_serving: "Enable \"Let other machines connect\" to accept incoming connections.",
    no_allowed_keys: "No keys yet. On the other machine, click Copy key and paste it below.",
    key_to_allow: "Public key to allow",
    vms_only: "VMs only",
    vms_only_note: "\"Allow VMs only\" grants access to virtual machines without access to this machine's shell.",
    allow: "Allow shell and VMs",
    allow_vms_only: "Allow VMs only",
    paste_key_first: "Paste the other machine's public key above to enable the Allow buttons.",
    not_a_key: "not a valid cmdterm public key",
    remove_named: |name| format!("Remove {name}"),
    show_named: |name| format!("Show {name}"),
    hide_named: |name| format!("Hide {name}"),
    key_host: "Shell",
    key_host_named: |name| format!("Shell for {name}"),
    key_projects: "Projects",
    key_projects_named: |name| format!("Projects for {name}"),
    key_defaults: "Defaults",
    key_defaults_named: |name| format!("Use defaults for {name}"),
    key_own_settings: "Own settings",
    known_machines: "Machines you connect to",
    no_known_machines: "No trusted machines yet",
    machine_address: "Machine address, host or host:port",
    add_machine: "Add machine",
    add: "Add",
    open: "Open",
    open_on: |name| format!("Open a project on {name}"),
    forget: "Forget",
    forget_named: |name| format!("Forget {name}"),
    fuse_named: |name| format!("Show projects of {name}"),
    fused_unreachable: "Unreachable, retrying…",
    fetching_key: |address| format!("Fetching key from {address}…"),
    compare_key: |address| {
        format!("{address} has this key. Compare it with cmdterm-remote keygen on that machine.")
    },
    trust: "Trust",
    trust_named: |address| format!("Trust {address}"),
    cancel: "Cancel",
    cancel_set_up: |image| format!("Cancel setting up {image}"),
    ok: "OK",
    already_trusted: |address| format!("{address} is already trusted"),
    key_changed: |address| {
        format!(
            "The key for {address} has changed. Someone may be intercepting the connection. Only forget the old key if the server's key was intentionally regenerated."
        )
    },
    connect_failed: |error| format!("Could not connect: {error}"),
    remote_note: "The other machine must allow your key, too.",
    section_vpn: "VPN networks",
    vpn_scan: "Scan for VPN peers again",
    vpn_scan_again: "Scan again",
    vpn_scanning: "Looking for Tailscale, NetBird and Nebula…",
    vpn_scan_running: "Scanning…",
    vpn_down: |vpn, why| {
        // The reason only adds words when it says more than "not connected".
        if why.is_empty() || why == "not connected" {
            format!("{vpn} is installed but not connected")
        } else {
            format!("{vpn} is installed but not connected ({why})")
        }
    },
    vpn_missing: |names| format!("Not found on this machine: {names}"),
    vpn_this_machine: |name| format!("This machine: {name}"),
    vpn_serve: "Serve",
    vpn_serve_named: |address| format!("Serve this machine on {address}"),
    vpn_no_peers: "No other machines on this network",
    online: "online",
    offline: "offline",
    ssh: "SSH",
    ssh_named: |peer| format!("Open a project on {peer} over SSH"),
    remote: "Remote",
    remote_named: |peer| format!("Open a project on {peer} as a remote machine"),
    vpn_nebula_note: "Nebula has no peer list. Showing its lighthouses, static_host_map, and matching /etc/hosts entries.",
    vpn_note: "SSH opens a project over the peer's VPN address. Remote requires scriptschnell-term to be serving on that peer with its key trusted here. In the command palette, use tailscale://box/~/src, netbird://box/~/src, or nebula://box/~/src to target a peer directly.",
    config_not_saved: |error| format!("not saved: {error}"),
    no_kvm: "/dev/kvm is not available, so VMs cannot run here",
    local_vms: "Your VMs",
    no_local_vms: "No VMs yet. Enter a name below to open one.",
    running: "running",
    stopped: "stopped",
    reset: "Reset",
    reset_note: "Deletes the VM's disk so it starts fresh from the base image next time.",
    delete_disk_of: |name| format!("Reset {name}"),
    vm_name: "New VM name",
    open_vm: "Open a project in this VM",
    bad_vm_name: "VM names use letters, digits, dots, dashes and underscores",
    stop_vms: "Stop all VMs",
    vms_stopping: "stopping VMs",
    vm_disks_in: |dir| format!("Disks are in {dir}"),
    vm_fields: [
        "Firecracker",
        "Kernel",
        "Root filesystem (default image)",
        "Disk folder",
        "CPUs",
        "Memory (MiB)",
        "Idle stop (s)",
    ],
    vm_field_hints: [
        "Name or path of the firecracker binary",
        "Path to a vmlinux kernel",
        "Path to an ext4 image, empty for the default",
        "Folder for the VMs' disks",
        "Virtual CPUs per VM",
        "MiB per VM",
        "Idle seconds before it stops",
    ],
    save_vm_settings: "Save VM settings",
    vm_settings_saved: "VM settings saved",
    not_a_number: |value| format!("not a number: {value}"),
    vm_notes: &[
        "For custom images. Leave a field empty to use the default.",
        "Running VMs keep their current settings until they stop.",
    ],
    ready_made_vms: "Ready-made VMs",
    image_ready: |summary| format!("Ready. {summary}"),
    set_up: "Set up",
    start_set_up: "Start",
    set_up_explained: "Downloads Firecracker and a kernel, then builds the image using your container engine. Click Start to begin, or Cancel.",
    set_up_image: |title| format!("Set up {title}"),
    rebuild: "Rebuild",
    rebuild_image: |title| format!("Rebuild {title}"),
    preparing: "Starting",
    install_first: |tools| format!("Install {tools} to set up images."),
    vms_have_network: "VMs reach the internet through pasta, without root.",
    vms_without_network: "VMs have no network access. Install passt (pasta) to enable networking.",
    vm_name_note: "A VM named ubuntu or ubuntu-something starts from the Ubuntu image.",
    advanced_settings: "Advanced settings",
    container_engine: "Devcontainer engine",
    automatic: "Automatic: docker if installed, else podman",
    section_sandbox: "Sandbox",
    sandbox_network: "Network",
    sandbox_network_note: "Allow sandboxed terminals to access the network",
    sandbox_restrict: "Allowed hosts only",
    sandbox_restrict_note: "Limit sandboxed terminals to allowed hosts (Sandboxing › Hosts) via HTTP_PROXY and HTTPS_PROXY",
    sandbox_host_network: "Host network",
    sandbox_host_network_note: "When off, sandboxes use an isolated loopback: servers cannot bind host ports, and only HTTP and HTTPS leave through the proxy. When on, sandboxes share this machine's network and ports.",
    sandbox_host_network_unscoped: "This kernel lacks Landlock ABI 6 (Linux 6.12), so sandboxes using the host network can reach abstract Unix sockets (including X11, allowing programs to send input to your windows). Disable Host network or update your kernel.",
    sandbox_allowed_hosts: "Allowed hosts",
    sandbox_hosts_inactive: "This list only applies when Network and Allowed hosts only are both enabled under Sandboxing.",
    sandbox_intercept_tls: "Inspect HTTPS",
    sandbox_intercept_tls_note: "The proxy decrypts HTTPS with its own CA, which sandboxes trust. HTTPS to a host with an invalid certificate fails inside the sandbox.",
    sandbox_engines: "Container engines",
    sandbox_engines_note: "This weakens the sandbox. Sandboxed terminals directly access docker, podman, containerd, buildkit, and WSL interop. Any containers they start run outside the sandbox with the engine's full file and network access.",
    engines_passthrough: "Direct passthrough (weakens the sandbox)",
    engines_sidecar: "Sidecar per project",
    engines_off: "Off",
    engines_use_sidecar: "Use a sidecar engine",
    engines_turn_off: "Turn engine access off",
    engines_keep: "Keep direct passthrough",
    engines_fixes_note: "A sidecar keeps containers working inside an isolated engine that only sees the project. Turning access off prevents sandboxes from starting containers.",
    sandbox_sidecar_note: "Each project gets an isolated container engine shared by its worktree sandboxes. It can access only the worktrees, its own storage, and the host's registry certificates. Its network is not restricted to allowed hosts. Linux only.",
    sidecar_engine: "Sidecar engine",
    sidecar_podman: "podman in podman",
    sidecar_docker: "docker in docker",
    sidecar_ready: |name| format!("sidecar {name} is ready"),
    sidecar_failed: |error| format!("sidecar failed: {error}"),
    sandbox_host: "Find or add a host, *.example.com for subdomains",
    sandbox_host_none: |host| format!("No allowed host matches \"{host}\"."),
    count_of: |shown, all| format!("{shown} of {all} match"),
    sandbox_host_bare: "Filtering list. Type a full hostname (such as example.com) to add it.",
    add_host: "Add host",
    choose_hosts: "Choose common hosts",
    choose_hosts_open: "+ Choose common hosts…",
    host_setup_dialog: "Allowed hosts for sandboxes",
    host_setup_intro: "Sandboxed terminals reach only the hosts you allow, and the proxy inspects their HTTPS. Pick what your work needs.",
    host_setup_note: "You can change this later under Settings › Sandboxing › Hosts, or add individual hosts there.",
    host_setup_done: "Done",
    setup_dialog: "Set up scriptschnell-term",
    setup_step: |at, of| format!("Step {at} of {of}"),
    setup_back: "Back",
    setup_next: "Next",
    setup_appearance: "Language, keyboard and colors",
    setup_appearance_intro: "Changes take effect immediately. You can adjust these anytime in Settings (Ctrl+,).",
    setup_runtime: "Where terminals run",
    setup_runtime_intro: "Choose the default environment for terminals in newly opened worktrees.",
    setup_shell: "Default shell",
    setup_shell_intro: "Choose the shell new terminals start. The Shells page of the settings changes it later.",
    new_agent: "New agent tab",
    default_agent: "Default agent",
    default_agent_first: "First set up",
    default_agent_named: |agent| format!("Default agent: {agent}"),
    default_agent_note: "New agent tab starts this agent. Without a choice it starts the first agent with an integration on the project's machine.",
    setup_runtime_note: "Change it later under Settings › Sandboxing. A project can choose for itself on its settings page.",
    setup_sandbox_note: "Restricts writes to the worktree and filters network access through a proxy.",
    setup_sandbox_note_windows: "Not recommended on Windows: Claude Code and other Node.js programs cannot start programs there, and Git Bash cannot start at all. Use a devcontainer to isolate a worktree.",
    setup_devcontainer_note: "Uses the worktree's devcontainer.json if present, otherwise falls back to the sandbox.",
    setup_host_note: "Runs directly on this machine with your full user permissions.",
    setup_recommended: "Recommended",
    setup_agents: "Coding agents",
    setup_agents_intro: "Selected agents receive hooks to report their status to scriptschnell-term, plus a skill explaining how to work with the app. Unchecking removes both.",
    settings_group_app: "GENERAL",
    settings_group_integrations: "INTEGRATIONS",
    settings_group_machines: "MACHINES",
    settings_group_sandbox: "SANDBOX",
    settings_group_about: "ABOUT",
    section_changelog: "Changelog",
    section_licenses: "Licenses",
    changelog_running: |version| format!("You are running version {version}."),
    changelog_unreleased: "Unreleased",
    licenses_app_hint: "scriptschnell-term is licensed under the MIT license or the Apache License 2.0, at your option.",
    license_mit: "MIT license",
    license_apache: "Apache License 2.0",
    licenses_third_party: "Third-party crates",
    licenses_third_party_hint: |count| {
        format!(
            "This app is built with {count} crates by other authors. Click one to read its license."
        )
    },
    licenses_none: "This build has no list of the crates it is built with.",
    license_in_file: "license in its files",
    license_no_file: |license| {
        format!("This crate ships no license file. Its license is {license}.")
    },
    reset_settings: "Reset all settings",
    reset_settings_confirm: "Click again to reset",
    reset_settings_hint: "Restores all settings to their defaults and reopens the initial setup",
    hosts_added_by_you: "Added by you or an extension",
    host_removed: |host| format!("Removed {host}"),
    undo: "Undo",
    undo_remove_named: |host| format!("Allow {host} again"),
    host_group: |id| match id {
        "forges" => "GitHub, GitLab, Codeberg, Bitbucket",
        "search" => "Search engines",
        "search_apis" => "Search APIs such as Exa and Tavily",
        "ai" => "AI model APIs",
        "docs" => "Documentation and Q&A",
        "rust" => "Rust: crates.io and rustup",
        "javascript" => "JavaScript: npm, Yarn, Node.js",
        "python" => "Python: PyPI",
        "go" => "Go modules",
        "local" => "This machine: localhost",
        "jvm" => "Java and Kotlin: Maven, Gradle",
        "dotnet" => ".NET: NuGet",
        "ruby_php" => "Ruby and PHP: RubyGems, Packagist",
        "containers" => "Container registries",
        "linux" => "Linux packages: Debian, Ubuntu, Alpine",
        _ => "Other hosts",
    },
    bad_host: "Enter a hostname or IP address, optionally starting with *.",
    sandbox_writable: "Additional writable paths",
    sandbox_hidden: "Hidden from the sandbox",
    sandbox_path: "Path, ~/ allowed",
    add_writable: "Add as writable",
    add_hidden: "Add as hidden",
    sandbox_note: "Changes apply to newly started terminals.",
    restart_terminals: "Restart this worktree's terminals",
    restart_terminals_hint: "Stops running programs and restarts terminals with the current settings",
    sandbox_windows_note: "On Windows, the sandbox only restricts writes. Hidden paths remain readable and network access remains enabled.",
    section_project: "Project",
    project_settings: "Project settings",
    settings_page: |page| format!("Settings › {page}"),
    project_none: "Open a project to change its settings.",
    project_jj: "Use Jujutsu",
    project_jj_note: "When the project contains a jj repository",
    project_launches: "Allow agents to open worktrees without asking",
    section_project_allowed: "Allowed",
    section_project_blocked: "Blocked",
    project_allowed_note: "Requests from this project's sandboxes that pass without a question. \"Always allow in this project\" in the approval dialog adds them here.",
    project_blocked_note: "Requests from this project's sandboxes that are refused without a question. \"Always block in this project\" in the approval dialog adds them here.",
    project_answers_remote_note: "Only sandboxes of projects on this machine use these entries.",
    project_answer_to_blocked: "Block instead",
    project_answer_to_allowed: "Allow instead",
    project_answer_add_heading: "Add an entry",
    project_answer_guard: "Guarded request",
    project_answer_host: "Host, or the secret's label for the keyring",
    project_answer_add: "Add",
    project_launches_note: "Lets agents create worktrees and start new agent sessions automatically",
    project_base: "Reference branch",
    project_base_hint: "Leave empty to auto-detect (origin/HEAD, main, master, or trunk)",
    project_base_note: "Default branch or revision for code reviews. Leave empty to auto-detect (origin/HEAD, main, master, or trunk; default for Mercurial; trunk() for jj).",
    project_base_set: "Set",
    project_env: "Environment variables",
    project_env_note_keyring: "Set for newly started terminals in this project, including sandboxed terminals. Values are stored encrypted in the system keyring and never written to state.json.",
    project_env_note_plain: "Set for newly started terminals in this project, including sandboxed terminals. No system keyring is available here, so values are stored in state.json (readable only by your user).",
    project_env_note_temporary: "Set for newly started terminals in this temporary session. Values stay in memory until the app closes.",
    project_env_remote_note: "This project is on another machine. Terminals there receive variables on the remote command line, which other users on this machine can inspect.",
    project_env_name: "Variable name",
    project_env_value: "Value",
    project_env_add: "Save",
    project_env_reload: "Reload from keyring",
    project_env_invalid_name: "Variable names must start with a letter or underscore and contain only letters, digits, and underscores.",
    project_env_invalid_value: "Variable values cannot contain null characters.",
    project_env_storage_keyring: "Encrypted in system keyring",
    project_env_storage_loading: "Loading from system keyring…",
    project_env_storage_locked: "Unavailable in keyring; reload to unlock",
    project_env_storage_plain: "Saved in state.json",
    project_env_storage_session: "In memory for this session",
    project_folders: "Sandbox folders",
    project_folders_note: "Additional folders on this project's machine accessible to sandboxed terminals. A read-only folder inside a hidden path will still be visible. Folders containing a hidden path or app files are skipped. Changes apply to newly started terminals.",
    project_writable: "Writable",
    project_readable: "Read-only",
    no_project_folders: "None yet",
    add_readable: "Add as read-only",
    project_remote_note: "This project is on another machine. Only absolute paths apply there.",
    project_not_sandboxed: "This worktree is not using a sandbox, so these folder rules do not apply.",
    default_runtime_note: "Where the terminals of a worktree run when the app opens it for the first time. A project can choose for itself on its settings page, and the palette changes it for one worktree.",
    project_runtime_note: "Replaces the default from Sandboxing for this project's worktrees and moves the ones it knows. A devcontainer applies only to worktrees with a devcontainer.json; the others use the default.",
    runtime_follow_default: "Use the default from Sandboxing",
};

pub static DE: Strings = Strings {
    settings: "Einstellungen",
    notifications: "Benachrichtigungen",
    notifications_unread: |count| match count {
        1 => "Benachrichtigungen, 1 ungelesen".to_owned(),
        _ => format!("Benachrichtigungen, {count} ungelesen"),
    },
    no_notifications: "Nichts Neues.",
    check_for_updates: "Nach Updates suchen",
    checking_for_updates: "Suche läuft…",
    update_all: "Alle aktualisieren",
    clear_notifications: "Alle entfernen",
    close_notifications: "Benachrichtigungen schließen",
    updating: "Wird aktualisiert…",
    dismiss: "Verwerfen",
    dismiss_notification: |title| format!("{title} verwerfen"),
    update_notification: |title| format!("Aktualisieren: {title}"),
    notice_title: "Hinweis",
    integration_outdated: |agent, env| format!("{agent}-Integration auf {env} ist veraltet"),
    integrations_outdated: |count| format!("{count} Agent-Integrationen sind veraltet"),
    integration_outdated_body: "Diese Hooks wurden von einer älteren Version von scriptschnell-term installiert oder seitdem geändert. Aktualisieren installiert die aktuellen Hooks und Skills.",
    agent_update: |agent, latest| format!("{agent} {latest} ist verfügbar"),
    agent_update_body: |current, env, manager, command| {
        format!(
            "Auf {env} ist {current} über {manager} installiert. Aktualisieren führt {command} aus."
        )
    },
    agent_updated: |agent, env| format!("{agent} auf {env} ist aktuell"),
    agent_update_failed: |agent, env| {
        format!("Aktualisierung von {agent} auf {env} fehlgeschlagen")
    },
    app_update: |version| format!("scriptschnell-term {version} ist verfügbar"),
    app_update_body: "Lädt die neue Version herunter, prüft sie anhand des signierten Release-Manifests und ersetzt die installierten Dateien.",
    app_release_body: |url| {
        format!(
            "Diese Installation stammt aus einem Paketmanager oder cargo. Aktualisiere sie dort oder lade das Release von {url} herunter."
        )
    },
    app_updated: |version| format!("scriptschnell-term {version} ist installiert"),
    app_updated_body: "Starte scriptschnell-term neu, um das Update zu verwenden.",
    app_update_failed: "Aktualisierung von scriptschnell-term fehlgeschlagen",
    just_now: "gerade eben",
    minutes_ago: |n| format!("vor {n} Min."),
    hours_ago: |n| format!("vor {n} Std."),
    days_ago: |n| match n {
        1 => "gestern".to_owned(),
        _ => format!("vor {n} Tagen"),
    },
    close_settings: "Einstellungen schließen",
    close_escape: "Schließen (Escape)",
    section_font: "Schrift",
    section_color_scheme: "Farbschema",
    section_keyboard: "Tastatur",
    section_extensions: "Erweiterungen",
    section_agents: "Agenten",
    section_language: "Sprache",
    section_shells: "Shells",
    default_shell: "Standard-Shell",
    default_shell_system: "Systemstandard (%COMSPEC%)",
    shells_note: "Neue Terminals verwenden die Standard-Shell. Bereits offene Terminals bleiben unverändert.",
    shells_listed: "Shells im Menü „Neues Panel“ nach „Neue Shell“",
    git_bash_found: |path| {
        format!(
            "Git Bash unter {path} gefunden. Jedes Terminal setzt CLAUDE_CODE_GIT_BASH_PATH für Claude Code auf diesen Pfad."
        )
    },
    git_bash_missing: "Git Bash ist nicht installiert. Claude Code benötigt es unter Windows; installiere Git for Windows und starte die App neu.",
    new_shell_named: |shell| format!("Neue Shell: {shell}"),
    bundled_font: "Mitgeliefert (Liberation Mono)",
    font_preview: "Vorschau",
    font_picker: "Terminalschrift",
    keymap_picker: "Tastenbelegung",
    smaller_font: "Kleinere Schrift",
    larger_font: "Größere Schrift",
    font_size: "Schriftgröße",
    size: "Schriftgröße im Terminal",
    system: "System",
    scheme_in_use: "Aktiv",
    follows_desktop: |name| format!("Folgt dem Systemdesign (derzeit {name})"),
    system_language: |name| format!("System ({name})"),
    dark: "dunkel",
    light: "hell",
    from_extension: |name| format!("Erweiterung {name}"),
    from_user: "Angepasst in settings.json",
    bindings: "Belegungen",
    binding_list: "Liste der Belegungen",
    bindings_hint: "Eine Belegung anklicken und eine neue Tastenkombination drücken. Tastenfolgen und aufgehobene Belegungen lassen sich unter \"keybindings\" in settings.json festlegen.",
    binding_taken: |keys, action| {
        format!("{keys} wurde von {action} übernommen (jetzt nicht belegt)")
    },
    reset_binding: "Zurücksetzen",
    binding_unset: "Nicht belegt",
    reset_binding_for: |action| format!("Tasten von {action} zurücksetzen"),
    press_keys: "Neue Tastenkombination drücken (Esc zum Abbrechen)",
    recording_was: |title, keys| format!("{title} (derzeit {keys})"),
    preset_description: |preset| match preset.id {
        "cmdterm" => "Nutzt Ctrl+Shift, damit alle Ctrl+Buchstabe-Kürzel die Shell erreichen",
        "tmux" => "Ctrl+B, dann eine Taste. Zweimal Ctrl+B schickt Ctrl+B an die Shell",
        "vscode" => {
            "Tastenkürzel des integrierten VS-Code-Terminals: Ctrl+Shift+` öffnet ein Terminal, Ctrl+Shift+5 teilt"
        }
        "cursor" => "Die Tasten von VS Code, dazu Ctrl+Shift+J für die Einstellungen",
        "windows-terminal" => {
            "Alt+Shift+Plus und Alt+Shift+Minus teilen, Alt+Pfeiltasten wechseln den Bereich"
        }
        "konsole" => "Ctrl+( und Ctrl+) teilen, Shift+Pfeiltasten wechseln den Tab",
        _ => preset.description,
    },
    off: "Aus",
    on: "An",
    failed: "Fehlgeschlagen",
    failed_with: |error| format!("Fehlgeschlagen: {error}"),
    no_extensions: "Keine Erweiterungen installiert.",
    enable_extension: |name| format!("{name} aktivieren"),
    extension_list: "Liste der Erweiterungen",
    all_extensions: "Alle Erweiterungen",
    extension_filter: "Nach Zustand filtern",
    filter_all: "Alle",
    filter_active: "An",
    filter_disabled: "Aus",
    no_matching_extensions: "Keine Erweiterung passt zum Filter.",
    find_extension: "Erweiterung suchen",
    find_setting: "Einstellung suchen",
    settings_no_match: |query| {
        format!("Keine Einstellung passt zu \"{query}\". Esc leert die Suche.")
    },
    settings_keywords: keywords_de,
    loaded_from: |dir| format!("Geladen aus {dir}"),
    reload_note: "Nach Änderungen an Erweiterungsdateien neu laden. Das Aktivieren oder Deaktivieren einer Erweiterung lädt alle neu.",
    needs_permissions: |list| format!("Braucht Berechtigung: {list}"),
    permissions_switch_note: "Einzelne Berechtigungen lassen sich unten ein- oder ausschalten.",
    grant: "Alle erteilen",
    revoke: "Alle entziehen",
    grant_for: |name| format!("{name} die Berechtigungen erteilen"),
    revoke_for: |name| format!("{name} die Berechtigungen entziehen"),
    permission_for: |name, permission| format!("die {permission}-Berechtigung für {name}"),
    permission_sensitive: |name| format!("{name} · sensibel"),
    permission_note: |name| match name {
        "terminal" => "Öffnet Terminals und sendet Eingaben an sie",
        "browser" => "Öffnet Browser-Bereiche",
        "worktree" => "Wechselt den aktiven Worktree",
        "copy" => "Kopiert Dateien in Devcontainer, VMs und entfernte Maschinen",
        "run" => "Führt Befehle in der Umgebung eines Worktrees aus",
        "trust" => "Fügt Zertifikate zum Vertrauensspeicher einer Umgebung hinzu",
        "sandbox" => "Ändert den Netzwerk- und Dateizugriff von Sandboxes",
        "secrets" => "Liest und speichert Geheimnisse im Schlüsselbund dieser Maschine",
        "guards" => "Fängt Sandbox-Anfragen wie Pushes ab, bis du sie freigibst",
        "network" => "Sendet HTTP-Anfragen und lädt Sperrlisten herunter",
        "agents" => "Registriert Agenten-Werkzeuge und eigene Coding-Agenten",
        "environments" => {
            "Fügt eigene Umgebungen hinzu, deren Befehle über ein lokales Programm laufen"
        }
        "storage" => "Speichert Erweiterungsdaten global in der App",
        "project_storage" => "Speichert Erweiterungsdaten pro Projekt",
        "filesystem" => "Liest und schreibt Dateien außerhalb des eigenen Ordners",
        "process" => "Startet Programme auf dieser Maschine",
        "env" => "Liest die Umgebungsvariablen dieser Maschine",
        _ => "",
    },
    builtin_page: "Eingebaute Regeln",
    builtin_subtitle: "immer aktiv",
    builtin_description: "Eingebaute Regeln für externe Aktionen aus Sandboxes. Sie gelten auch ohne Erweiterungen.",
    extension_page: "Erweiterungsseite",
    no_description: "Keine Beschreibung vorhanden.",
    extension_enabled: "Aktiviert",
    permissions_heading: "Berechtigungen",
    no_permissions: "Keine Berechtigungen erforderlich.",
    settings_heading: "Einstellungen",
    ext_guards_heading: "Geschützte Anfragen",
    list_empty: "Keine Einträge",
    list_new_entry_for: |title| format!("Neuer Eintrag für {title}"),
    list_add_to: |title| format!("Zu {title} hinzufügen"),
    list_remove: "Entfernen",
    list_remove_from: |entry, title| format!("{entry} aus {title} entfernen"),
    list_duplicate: |entry| format!("{entry} ist bereits in der Liste"),
    guards_heading: "Externe Sandbox-Aktionen",
    guards_note: "Fragen bittet vor jeder Anfrage um Freigabe. Unbeantwortete Anfragen werden nach zwei Minuten blockiert.",
    guard_ask: "Fragen",
    guard_block: "Blockieren",
    guard_allow: "Erlauben",
    guard_mode_for: |title, mode| format!("{title}: {mode}"),
    guard_from: |extension, hosts| format!("{extension}, {hosts}"),
    every_host: "alle Hosts",
    approval_dialog: "Anfrage aus der Sandbox",
    approval_question: |title| format!("Eine Sandbox möchte: {title}"),
    approval_note: "Deine Wahl gilt eine Minute lang für diesen Host.",
    approval_project: |name| format!("Aus dem Projekt {name}"),
    approval_always: "In diesem Projekt immer erlauben",
    approval_never: "In diesem Projekt immer blockieren",
    always_heading: "Immer erlaubt",
    always_note: "Aktionen, die in Dialogen dauerhaft für ein Projekt erlaubt wurden, darunter Sandbox-Anfragen sowie das Anlegen von Worktrees und Starten von Agenten.",
    always_entry: |title, host| format!("{title}: {host}"),
    always_remove: "Entfernen",
    change_requests_heading: "Berechtigungen für Pull- und Merge-Requests",
    change_requests_note: "Diese Standardwerte gelten für alle Forge-Erweiterungen, etwa GitHub und GitLab. Einstellungen auf der Seite einer Erweiterung haben Vorrang.",
    change_request_create: "Einen Pull- oder Merge-Request öffnen",
    change_request_comment: "Einen Pull- oder Merge-Request kommentieren oder reviewen",
    change_request_merge: "Einen Pull- oder Merge-Request mergen oder abschließen",
    change_request_subtitle: |_| "Gilt für alle Forge-Erweiterungen".to_owned(),
    approval_sends: |detail| format!("Sendet: {detail}"),
    keyring_request: |action, label| match action {
        "read" => format!("\"{label}\" lesen"),
        "change" => format!("\"{label}\" ändern"),
        "delete" => format!("\"{label}\" löschen"),
        _ => format!("ein Geheimnis in \"{label}\" anlegen"),
    },
    keyring_note: "Deine Wahl gilt eine Minute lang für dieses Geheimnis.",
    keyring_scope: "der Schlüsselbund dieser Sitzung",
    agent_settings_request: |agent, file| format!("{agent} ändert {file}:"),
    agent_settings_removed: |key| format!("{key}: entfernt"),
    agent_settings_more: |n| format!("und {n} weitere Schlüssel"),
    agent_settings_note: "Bis zur Freigabe bleibt die Änderung innerhalb der Sandbox. Deine Wahl gilt eine Minute lang für diese Datei.",
    agent_settings_scope: "Einstellungen, die Agenten in Sandboxes ändern",
    open_url_request: |link| format!("Öffnen: {link}"),
    open_url_note: "Der Link öffnet sich in deinem Browser auf diesem Rechner, wo du angemeldet bist. Erlaube ihn, wenn du selbst gestartet hast, was ihn öffnet, etwa eine Anmeldung. Ein Port auf localhost, den der Link nennt, erreicht 10 Minuten lang den Server der Sandbox auf diesem Port.",
    open_url_scope: "Links, die Programme in Sandboxes öffnen",
    approval_allow: "Erlauben",
    approval_block: "Blockieren",
    control_dialog: "Erweiterung ändern",
    control_install: |name| format!("Ein Agent möchte die Erweiterung {name} installieren"),
    control_change: |name| format!("Ein Agent möchte die Erweiterung {name} ändern"),
    control_source: |place, machine, kind| {
        format!("Aus einem Terminal in {place} auf {machine} ({kind})")
    },
    control_file_added: |path, lines| format!("{path}: neu, {lines} Zeilen"),
    control_file_changed: |path, lines| format!("{path}: ersetzt, jetzt {lines} Zeilen"),
    control_file_removed: |path| format!("{path}: entfernt"),
    control_more_files: |n| format!("und {n} weitere Dateien"),
    control_permissions: |list| {
        format!(
            "Angeforderte Berechtigungen: {list}. Du kannst sie auf der Seite Erweiterungen erteilen."
        )
    },
    control_no_permissions: "Keine Berechtigungen angefordert.",
    control_shipped: "Dies ist eine mitgelieferte Erweiterung. Geänderte Dateien werden nicht mehr automatisch aktualisiert.",
    control_note: "Erweiterungen laufen in der App, außerhalb jeder Sandbox. Erlaube nur Code, den du gelesen hast.",
    control_deny: "Ablehnen",
    control_applied: |name| format!("Ein Agent hat die Erweiterung {name} geändert"),
    control_failed: |error| format!("Steuer-Socket: {error}"),
    control_settings_dialog: "Einstellungen ändern",
    control_settings_question: "Ein Agent schlägt eine Änderung der Einstellungen vor",
    control_reason: |reason| format!("Grund: {reason}"),
    control_setting_set: |path, before, after| format!("{path}: {before} → {after}"),
    control_setting_add: |path, items| format!("{path}: {items} hinzufügen"),
    control_setting_remove: |path, items| format!("{path}: {items} entfernen"),
    control_settings_note: "Mit „Erlauben“ wird settings.json sofort geschrieben. Du kannst dies in den Einstellungen (Strg+,) rückgängig machen.",
    control_settings_applied: "Ein Agent hat die Einstellungen geändert",
    control_launch_dialog: "Agenten starten",
    control_new_worktree: |branch, project| {
        format!("Ein Agent möchte den Worktree {branch} in {project} anlegen")
    },
    control_open_worktree: |path| format!("Ein Agent möchte zu {path} wechseln"),
    control_new_detached: |project| {
        format!("Ein Agent möchte einen losgelösten Worktree in {project} anlegen")
    },
    control_worktree_from: |revision| format!("ab {revision}"),
    control_worktree_from_main: "ab der Revision des Haupt-Worktrees",
    control_start_agent: |command| format!("und dort {command} starten"),
    control_type_prompt: |prompt| format!("dann seinen ersten Prompt eingeben: {prompt}"),
    control_launch_note: "Die Agenten starten in neuen Terminals dieses Worktrees in dessen konfigurierter Umgebung (Host, Sandbox oder Devcontainer).",
    control_allow_session: "In dieser Sitzung erlauben",
    always_launches: "Worktrees anlegen und Agenten starten",
    reload_extensions: "Erweiterungen neu laden",
    agent_on: |agent, env| format!("{agent} auf {env}"),
    working: "arbeitet…",
    install: "Installieren",
    update: "Aktualisieren",
    remove: "Entfernen",
    retry: "Erneut",
    agent_button: |action, agent| format!("{agent}-Integration: {action}"),
    agent_list: "Liste der Agenten",
    agents_note: [
        "Installierte Agenten melden, wenn sie arbeiten, fertig sind oder auf",
        "Eingabe oder Freigabe warten; die Seitenleiste markiert den Worktree.",
    ],
    agents_skill: [
        "Sie erhalten außerdem zwei Skills mit der Dokumentation zu Einstellungen,",
        "Erweiterungen und Sandboxes.",
    ],
    hooks_run: "Hooks melden die Aktivität eines Agenten über ein kleines Skript, das die App installiert und aktuell hält.",
    check_agents: "Agenten erneut prüfen",
    agents_update_all: |count| format!("Alle aktualisieren ({count})"),
    agents_installed: "Agenten-Integrationen",
    agents_outdated: |count| {
        format!(
            "{count} Agenten haben veraltete Hooks. Aktualisieren installiert das aktuelle Hook-Skript."
        )
    },
    agent_control: "Steuer-Socket",
    agent_control_on: "Überall aktiv, auch in Sandboxes",
    agent_control_local: "Für diesen Rechner und seine Sandboxes aktiv",
    agent_control_host: "Nur außerhalb von Sandboxes aktiv",
    agent_control_off: "Aus",
    agent_control_note: "Ermöglicht Agenten, Änderungen an Erweiterungen und Einstellungen oder neue Worktrees anzufragen; jede Anfrage erfordert deine Freigabe.",
    checking: "Wird geprüft…",
    not_installed: "Nicht installiert",
    on_outdated: "An, Hooks veraltet",
    note_trust_hooks: "Codex fragt nach, bevor es neue Hooks ausführt.",
    note_enable_hooks: "Hooks in ~/.codex/config.toml einschalten: [features] hooks = true",
    note_next_session: "Gilt für neue Sitzungen.",
    language: "Sprache",

    command_palette: "Befehlspalette",
    minimize_window: "Minimieren",
    maximize_window: "Maximieren",
    restore_window: "Wiederherstellen",
    close_window: "Fenster schließen",
    palette_keys_hint: "↑↓ wählen · Enter ausführen · Esc schließen",
    palette_areas_hint: |keys| format!("{keys} Bereich wechseln"),
    palette_empty_keys_hint: "Esc schließen",
    palette_input_keys_hint: "Enter bestätigen · Esc zurück",
    palette_create: "Erstellen",
    palette_clone: "Klonen",
    palette_more: |from, to, total, below| {
        if below > 0 {
            format!("{from}–{to} von {total} · {below} weitere ↓")
        } else {
            format!("{from}–{to} von {total}")
        }
    },
    palette_confirm: |label| format!("{label}? Zum Bestätigen noch einmal Enter drücken"),
    new_terminal: "Neue Shell",
    close_panel: "Panel schließen",
    close_panel_named: |pane, title| format!("Bereich {pane} schließen: {title}"),
    move_pane: |pane| format!("Ziehen verschiebt Bereich {pane}"),
    pane_menu: |pane| format!("Menü von Bereich {pane}"),
    drag_tab_hint: "Am Rand ablegen zum Teilen, zwischen Tabs zum Umsortieren (Esc bricht ab)",
    drag_shown_tab_hint: "Zwischen Tabs ablegen zum Umsortieren (Esc bricht ab)",
    drag_pane_hint: "Am Rand teilen, in der Mitte tauschen, auf Tab oder + verschieben (Esc bricht ab)",
    zone_left: "Links teilen",
    zone_right: "Rechts teilen",
    zone_top: "Oben teilen",
    zone_bottom: "Unten teilen",
    zone_swap: "Tauschen",
    drop_into_tab: "In diesen Tab verschieben",
    drop_new_tab: "In einen neuen Tab verschieben",
    close_pane: "Bereich schließen",
    resize_panes: "Bereiche anpassen",
    close_tab: "Tab schließen",
    close_other_tabs: "Andere Tabs schließen",
    move_pane_to_tab: "Bereich in neuen Tab verschieben",
    split_side_by_side: "Nebeneinander teilen",
    split_stacked: "Übereinander teilen",
    focus_next_pane: "Nächster Bereich",
    focus_previous_pane: "Vorheriger Bereich",
    grow_pane: "Bereich vergrößern",
    shrink_pane: "Bereich verkleinern",
    next_tab: "Nächster Tab",
    previous_tab: "Vorheriger Tab",
    toggle_sidebar: "Seitenleiste ein- oder ausblenden",
    hide_sidebar: "Seitenleiste ausblenden",
    show_sidebar: "Seitenleiste einblenden",
    tab_n: |n| format!("Tab {n}"),
    focus_pane_n: |n| format!("Bereich {n}"),
    send_text: |text| format!("{text} senden"),

    projects: "Projekte",
    change_requests: "Merge-Requests",
    sidebar: "Seitenleiste",
    adding: |input| format!("{input} wird hinzugefügt…"),
    set_token: |var| format!("{var} setzen, um Merge-Requests zu sehen"),
    open_host: |host| format!("{host} öffnen"),
    new_worktree_from: |name| format!("Neuer Worktree aus {name}"),
    worktree_source: |name| format!("Aus {name}"),
    loading: "lädt…",
    open_project: "Projekt öffnen",
    worktree_count: |description, count| format!("{description} ({count} Worktrees)"),
    ci_ok: "CI ok",
    ci_failed: "CI rot",
    ci_running: "CI läuft",
    ci_pending: "CI wartet",
    ci_canceled: "CI gestoppt",
    locked: "gesperrt",
    meta_changed: "geändert",
    meta_new: "neu",
    missing: "fehlt",
    busy: "aktiv",
    done: "fertig",
    input: "Eingabe",
    approve: "Freigabe",
    busy_description: "Ein Agent arbeitet",
    done_description: "Ein Agent ist fertig und wartet auf den nächsten Prompt",
    input_description: "Wartet auf Eingabe",
    approve_description: "Ein Agent wartet auf Freigabe",

    new_shell: "Neue Shell",
    start_agent: |agent| format!("{agent} starten"),
    new_panel: "Neues Panel",
    new_panel_tip: "Neues Panel. Einen Bereich hier ablegen, um ihn in einem neuen Tab zu öffnen",
    browse_files: "Dateien durchsuchen",
    files: "Dateien",
    files_count: |n| {
        if n == 1 {
            "1 Datei".to_owned()
        } else {
            format!("{n} Dateien")
        }
    },
    files_lines: |n| {
        if n == 1 {
            "1 Zeile".to_owned()
        } else {
            format!("{n} Zeilen")
        }
    },
    files_truncated: "erste 16 MiB",
    files_loading: "Wird geladen…",
    files_empty: "Keine Dateien",
    files_pick: "Links eine Datei auswählen und mit Enter öffnen.",
    files_binary: |size| format!("Binärdatei, {size} Bytes"),
    files_unreadable: |error| format!("Datei nicht lesbar: {error}"),
    files_unrendered: |error| format!("Datei nicht darstellbar: {error}"),
    files_mermaid_unsupported: |reason| format!("Nicht unterstütztes Mermaid-Diagramm: {reason}"),
    files_pixels: |width, height| format!("{width} × {height} px"),
    files_page: |page, pages| format!("Seite {page} von {pages}"),
    files_show_source: "v zeigt den Quelltext",
    files_show_rendered: "v zeigt die Darstellung",
    explore_history: "Verlauf",
    explore_list_commits: "Commits",
    explore_list_files: "Dateien",
    explore_review: "Review",
    explore_open_history: "Verlauf ansehen",
    status_open_history: "Branch-Verlauf anzeigen",
    status_open_files: "Worktree-Dateien durchsuchen",
    status_open_machines: "Maschineneinstellungen öffnen",
    explore_open_review: "Änderungen prüfen",
    explore_review_against: "Änderungen gegen einen Branch prüfen",
    explore_pick: "Links einen Eintrag auswählen. Mit v zwischen Verlauf, Review und Merge-Request wechseln.",
    explore_no_commits: "Noch keine Commits",
    explore_sideways: "← → zum Scrollen langer Zeilen",
    explore_no_changes: |base| format!("Keine Änderungen gegenüber {base}."),
    explore_no_base: "Kein Basis-Branch gefunden. Mit b einen auswählen.",
    explore_no_changes_hint: |base| {
        format!(
            "Commits und nicht committete Änderungen, die nicht auf {base} sind, erscheinen hier. Mit b gegen einen anderen Branch vergleichen."
        )
    },
    explore_failed: |error| format!("Verlauf nicht lesbar: {error}"),
    explore_unrelated: |base| {
        format!(
            "Dieser Branch hat keine gemeinsame Historie mit {base}, es gibt nichts zu vergleichen. Mit b gegen einen anderen Branch vergleichen."
        )
    },
    explore_review_failed: |error| {
        format!("Die Änderungen konnten nicht verglichen werden: {error}")
    },
    explore_against: |base| format!("gegen {base}"),
    explore_from: |commit| format!("ab {commit}"),
    explore_commits: |n| {
        if n == 1 {
            "1 Commit".to_owned()
        } else {
            format!("{n} Commits")
        }
    },
    explore_uncommitted: "mit nicht committeten Änderungen",
    explore_truncated: "erste 32 MiB",
    debug_browser: "Debug-Browser",
    debug_browser_open: "Debug-Browser",
    debug_browser_url_ask: "URL im Debug-Browser öffnen…",
    expose_port: |port| format!("Port {port} freigeben"),
    stop_exposing_port: |port| format!("Port {port} nicht mehr freigeben"),
    port_exposed: |port, host| {
        format!("Port {port} ist auf diesem Rechner unter 127.0.0.1:{host} offen")
    },
    port_expose_failed: |port, error| format!("Port {port} ließ sich nicht freigeben: {error}"),
    debug_browser_failed: "Debug-Browser konnte nicht gestartet werden",
    debug_browser_retry: "Erneut versuchen",
    debug_browser_alt: "Seite im Debug-Browser",
    monitor: "Monitor",
    monitor_note: "Dieser Rechner, jede Sekunde aktualisiert",
    monitor_open: "Rechner überwachen",
    monitor_waiting: "Messe…",
    monitor_no_proc: "Der Monitor benötigt /proc unter Linux, das auf diesem Rechner nicht verfügbar ist.",
    monitor_failed: |error| format!("Systemauslastung konnte nicht gelesen werden: {error}"),
    monitor_cpu: "CPU",
    monitor_memory: "Speicher",
    monitor_swap: "Swap",
    monitor_network: "Netzwerk",
    monitor_disk: "Datenträger",
    monitor_cpu_history: "CPU-Verlauf",
    monitor_net_history: "Netzwerkverlauf",
    monitor_net_down: "↓ ein",
    monitor_net_up: "↑ aus",
    monitor_peak: "Spitze",
    monitor_now: "jetzt",
    monitor_collecting: "Verlauf wird erfasst (ein Messwert pro Sekunde)…",
    copy_command: "Installationsbefehl kopieren",
    copy_command_hint: "Kopiert den Installationsbefehl. In ein Terminal einfügen.",
    copy_install_for: |agent| format!("Installationsbefehl für {agent} kopieren"),
    command_copied: "Befehl kopiert",
    agent_install_with: |command| format!("Nicht installiert · {command}"),
    agent_turn_on: "Einschalten",
    tab_with_panes: |title, panes| format!("{title} · {panes} Bereiche"),
    tabs_more: |count| format!("{count} weitere"),
    tabs_hidden: |count| {
        if count == 1 {
            "1 weiterer Tab".to_owned()
        } else {
            format!("{count} weitere Tabs")
        }
    },
    palette_group_project: "Projekt",
    commands: "Befehle",
    palette_group_agents: "Agenten",
    palette_group_panels: "Panels",
    palette_group_shells: "Shells",
    palette_group_layout: "Tabs und Bereiche",
    palette_group_app: "App",
    palette_group_worktrees: "Worktrees",
    palette_group_runtime: "Ausführung",
    palette_group_extensions: "Erweiterungen",
    tasks_col_state: "Zustand",
    tasks_col_kind: "Art",
    tasks_col_panel: "Panel",
    tasks_col_where: "Ort",
    tasks_col_task: "Aufgabe",
    tasks_col_schedule: "Zeitplan",
    tasks_col_thread: "Thread",
    tasks_col_cpu: "CPU",
    tasks_col_time: "CPU-Zeit",
    monitor_up: "läuft seit",
    monitor_days: "T",
    monitor_load: "Last",
    monitor_tasks: "Prozesse",
    monitor_running: "laufend",
    tasks: "Aufgaben",
    tasks_note: "Offene Panels, Erweiterungsaufgaben und App-Threads",
    tasks_refresh: "Neu laden",
    tasks_open: "Aufgaben anzeigen",
    tasks_summary: |s| {
        let mut out = format!(
            "{} {} in {} {} · {} {}",
            s.panels,
            if s.panels == 1 { "Panel" } else { "Panels" },
            s.worktrees,
            if s.worktrees == 1 {
                "Worktree"
            } else {
                "Worktrees"
            },
            s.terminals,
            if s.terminals == 1 {
                "Terminal"
            } else {
                "Terminals"
            },
        );
        for (n, word) in [
            (s.frozen, "eingefroren"),
            (s.exited, "beendet"),
            (
                s.waiting,
                if s.waiting == 1 {
                    "wartet auf dich"
                } else {
                    "warten auf dich"
                },
            ),
        ] {
            if n > 0 {
                out.push_str(&format!(", {n} {word}"));
            }
        }
        if let Some(threads) = s.threads {
            let label = if threads == 1 { "Thread" } else { "Threads" };
            out.push_str(&format!(" · {threads} {label}"));
        }
        out
    },
    tasks_panels: "Panels",
    tasks_extension: "Aufgaben der Erweiterungen",
    tasks_threads: "App-Threads",
    tasks_none: "Keine.",
    tasks_no_threads: "Die Thread-Liste benötigt /proc/self/task, das auf diesem System nicht verfügbar ist.",
    tasks_idle_threads: |count| match count {
        1 => "1 ruhender Thread".to_owned(),
        _ => format!("{count} ruhende Threads"),
    },
    tasks_shown_worktree: "aktiv",
    tasks_shown: "sichtbar",
    tasks_hidden: "im Hintergrund",
    tasks_frozen: "eingefroren",
    tasks_thawing: "setzt fort",
    tasks_exited: "beendet",
    tasks_killed: "abgebrochen",
    tasks_running: "läuft",
    tasks_sleeping: "ruht",
    tasks_busy: "aktiv",
    tasks_due: "fällig",
    tasks_scheduled: "geplant",
    tasks_kind: |kind| {
        use crate::taskview::Kind;
        match kind {
            Kind::Terminal => "Terminal",
            Kind::Browser => "Browser",
            Kind::Files => "Dateien",
            Kind::Explore => "Verlauf",
            Kind::Monitor => "Monitor",
            Kind::DebugBrowser => "Debug-Browser",
            Kind::Tasks => "Aufgaben",
        }
    },
    tasks_agent_state: |state| {
        use crate::term::Attention;
        match state {
            Attention::None => "",
            Attention::Busy => "arbeitet",
            Attention::Done => "wartet auf einen Prompt",
            Attention::Input => "wartet auf Eingabe",
            Attention::Permission => "wartet auf Freigabe",
        }
    },
    tasks_place: |tab, pane| match pane {
        Some(pane) => format!("Tab {tab}, Bereich {pane}"),
        None => format!("Tab {tab}"),
    },
    tasks_exit_code: |code| format!("Exit-Code {code}"),
    tasks_age: |age| format!("seit {age} offen"),
    tasks_freezes_in: |left| format!("friert in {left} ein"),
    tasks_every: |every| format!("alle {every}"),
    tasks_last: |since| format!("zuletzt vor {since}"),
    tasks_next: |left| format!("nächster Lauf in {left}"),
    tasks_never: "lief noch nie",
    pane_n: |n| format!("Bereich {n}"),
    press: |keys| format!("{keys} drücken"),
    open_a_project: "Projekt öffnen",
    open_the_palette: "Befehlspalette öffnen",
    then_open_project: |first| format!("{first} und Projekt öffnen… wählen."),
    no_panels: "Keine Panels",
    no_panels_message: |how| format!("Dieser Worktree hat keine Panels. Für eine Shell: {how}."),
    use_new_shell: "Auf + neben den Tabs klicken",
    no_project: "Kein Projekt ausgewählt",
    select_project: "Ein Projekt in der Seitenleiste auswählen.",
    status_bar: "Statusleiste",

    prompt_project: "Pfad, host:/pfad, ssh://, tailscale:// oder eine Repository-URL",
    prompt_clone_folder: "Ordner, in den geklont wird",
    prompt_review_base: "Basis-Branch oder Revision (leer für Projektstandard)",
    clone_from: |url| format!("{url} klonen nach"),
    clone_here: "Dieser Rechner",
    clone_here_devcontainer: "Dieser Rechner (Devcontainer)",
    clone_in_vm: |name| format!("VM {name} auf diesem Rechner"),
    clone_on: |address| format!("Entfernter Rechner {address}"),
    clone_devcontainer_on: |address| format!("Entfernter Rechner {address} (Devcontainer)"),
    clone_vm_on: |address| format!("VM auf dem entfernten Rechner {address}"),
    clone_vm_name: "Name der VM",
    prompt_branch: "Branch-Name",
    prompt_worktree: "Worktree-Name",
    prompt_url: "URL",
    prompt_command: "Befehl oder Worktree-Namen eingeben",
    prompt_panel: "Bereich suchen",
    open_project_ask: "Projekt öffnen…",
    new_worktree_ask: "Neuer Worktree…",
    open_url_ask: "URL öffnen…",
    switch_to: |name| format!("Wechseln zu {name}"),
    switch_to_worktree: |project, branch| format!("Wechseln zu {project} / {branch}"),
    remove_worktree: |branch| format!("Worktree {branch} entfernen"),
    vcs_fetch: "Abrufen",
    vcs_fetch_project: |project| format!("{project} abrufen"),
    vcs_pull: |branch| format!("{branch} pullen"),
    vcs_push: |branch| format!("{branch} pushen"),
    vcs_push_to: |branch, remote| format!("{branch} nach {remote} pushen"),
    vcs_fetching: |project| format!("{project} wird abgerufen…"),
    vcs_pulling: |branch| format!("{branch} wird gepullt…"),
    vcs_pushing: |branch| format!("{branch} wird gepusht…"),
    vcs_fetched: |project| format!("{project} abgerufen"),
    vcs_pulled: |branch| format!("{branch} gepullt"),
    vcs_pushed: |branch, remote| format!("{branch} nach {remote} gepusht"),
    vcs_fetch_failed: |project, reason| {
        format!("{project} konnte nicht abgerufen werden: {reason}")
    },
    vcs_pull_failed: |branch, reason| format!("{branch} konnte nicht gepullt werden: {reason}"),
    vcs_push_failed: |branch, reason| format!("{branch} konnte nicht gepusht werden: {reason}"),
    vcs_busy: |name| format!("{name} wird noch abgerufen, gepullt oder gepusht"),
    forget_project: |name| format!("Projekt {name} vergessen"),
    open_temporary_ask: "Projekt vorübergehend öffnen…",
    global_session_ask: "Neue globale Sitzung…",
    prompt_global_session: "Ordner, host: oder ssh://host/pfad (leer für Home-Verzeichnis)",
    global_session: "Globale Sitzung",
    temporary: "vorübergehend",
    keep_session: |name| format!("Sitzung {name} behalten"),
    close_session: |name| format!("Sitzung {name} schließen"),
    agents_tab: "Agenten",
    agents_tab_count: |count| format!("Agenten ({count})"),
    agents_finished: "Fertig, nicht gemergt",
    agents_working: |count| format!("Aktiv ({count})"),
    agents_merged: |count| format!("Gemergt ({count})"),
    agents_earlier: |count| format!("Frühere Sitzungen ({count})"),
    agents_none: "Noch keine Agent-Sitzungen",
    agents_none_finished: "Kein Agent ist fertig",
    agents_none_hint: "Agenten, die du in einem Worktree startest, erscheinen hier mit ihrem aktuellen Status.",
    agents_start: "Agent starten…",
    change_open: "offen",
    change_merged: "gemergt",
    change_closed: "geschlossen",
    forge_accounts: "Konten",
    forge_signed_in_as: |user| format!("Angemeldet als {user}"),
    forge_signed_in: "Angemeldet",
    forge_signed_out: "Nicht angemeldet",
    forge_signing_in: "Token wird geprüft…",
    forge_from_env: |var| format!("Token aus {var}"),
    forge_failed: |error| format!("Anmeldung fehlgeschlagen: {error}"),
    forge_create_token: "Token erstellen",
    forge_create_token_hint: "Öffnet die Token-Seite der Forge im Browser",
    forge_create_token_for: |host| format!("Token auf {host} erstellen"),
    forge_token_for: |host| format!("Token für {host}"),
    forge_sign_in: "Anmelden",
    forge_sign_in_for: |host| format!("Bei {host} anmelden"),
    forge_sign_out: "Abmelden",
    forge_sign_out_for: |host| format!("Von {host} abmelden"),
    forge_token_missing: "Erst ein Token einfügen",
    extension_request_failed: |error| {
        format!("Eine Erweiterungsaufgabe ist fehlgeschlagen: {error}")
    },
    forge_new_account_for: |forge| format!("Neues {forge}-Konto"),
    forge_add_account: "Konto hinzufügen",
    forge_add_account_open: "+ Anderer Host…",
    forge_other_host: "Konto auf einem anderen Host hinzufügen…",
    forge_open_add_for: |forge| format!("Weiteres {forge}-Konto hinzufügen"),
    forge_sign_in_open: "Anmelden…",
    forge_open_sign_in_for: |host| format!("Anmeldung für {host} öffnen"),
    forge_add_account_for: |forge| format!("{forge}-Konto hinzufügen"),
    forge_remove_account_for: |account| format!("Konto {account} entfernen"),
    forge_not_an_account: |entry| {
        format!("{entry} muss ein Host oder Host/Pfad sein, z. B. example.com/team")
    },
    toast_done: "Erledigt",
    toast_failed: "Fehlgeschlagen",
    toast_signed_in: |account, user| {
        if user.is_empty() {
            format!("Bei {account} angemeldet")
        } else {
            format!("Bei {account} als {user} angemeldet")
        }
    },
    toast_sign_in_failed: |account, error| {
        format!("Anmeldung bei {account} fehlgeschlagen: {error}")
    },
    toast_signed_out: |account| format!("Von {account} abgemeldet"),
    toast_account_added: |account| format!("{account} hinzugefügt"),
    toast_account_removed: |account| format!("{account} entfernt"),
    toast_trusted: |address| format!("{address} vertraut"),
    toast_forgotten: |address| format!("{address} vergessen"),
    toast_fused: |address| format!("Projekte von {address} werden angezeigt"),
    toast_unfused: |address| format!("Projekte von {address} werden nicht mehr angezeigt"),
    toast_key_allowed: "Schlüssel erlaubt",
    toast_key_removed: "Schlüssel entfernt",
    toast_stopped_serving: "Dieser Rechner ist nicht mehr freigegeben",
    toast_saved: "Gespeichert",
    toast_image_ready: |name| format!("Das Image {name} ist bereit"),
    toast_image_failed: |name, error| format!("Das Image {name} ist fehlgeschlagen: {error}"),
    toast_disk_deleted: |name| format!("Festplatte von {name} gelöscht"),
    toast_agent_installed: |agent, env| format!("{agent}-Integration auf {env} installiert"),
    toast_agent_removed: |agent, env| format!("{agent}-Integration von {env} entfernt"),
    toast_agent_failed: |agent, env, error| format!("{agent} auf {env}: {error}"),
    toast_host_added: |host| format!("{host} hinzugefügt"),
    toast_path_added: |path| format!("{path} hinzugefügt"),
    toast_project_base: |base| match base {
        Some(base) => format!("Reviews vergleichen mit {base}"),
        None => "Reviews erkennen den Basis-Branch wieder automatisch".to_owned(),
    },
    toast_project_env_keyring: |name| format!("{name} im System-Schlüsselbund gespeichert"),
    toast_project_env_plain: |name| format!("{name} in state.json gespeichert"),
    toast_project_env_session: |name| format!("{name} für diese Sitzung gesetzt"),
    toast_project_env_fallback: |name, error| {
        format!("{name} in state.json gespeichert (Schlüsselbund nicht verfügbar: {error})")
    },
    toast_project_env_removed: |name| format!("{name} entfernt"),
    project_env_loaded_notice: |project| {
        format!(
            "Umgebungsvariablen für {project} aus dem Schlüsselbund geladen. Starte Terminals neu, um sie zu übernehmen."
        )
    },
    section_accounts: "Konten",
    accounts_forges: "Forges",
    forge_used_by: |forge, project| format!("{forge} · von {project} genutzt"),
    accounts_forges_note: "Der Agenten-Tab fragt diese Forges ab, ob die Branches fertiger Agenten Change Requests haben und ob sie gemergt sind. Jedes Konto speichert sein Token im Schlüsselbund des Systems. Die Erweiterung darf Anfragen an den Host eines Kontos und seine Subdomains senden.",
    accounts_no_forges: "Keine Forge-Erweiterung ist aktiviert. Aktiviere GitHub, GitLab, Forgejo, Gitea, Bitbucket oder Azure DevOps auf der Seite Erweiterungen.",
    accounts_example: |example| format!("z. B. {example}"),
    accounts_logins: "Registry-Anmeldungen",
    accounts_logins_note: "Diese Erweiterungen geben Platzhalter-Tokens an Sandboxes weiter, und der Proxy setzt die echten Zugangsdaten für passende Hosts ein. Ändere Zugangsdaten in ihrer Datei oder Umgebungsvariable; neue Sandboxes übernehmen sie beim Start.",
    logins_file: |path| format!("Liest Datei {path}"),
    logins_variable: |name| format!("Liest Variable {name}"),
    logins_other: |source| format!("Liest {source}"),
    logins_extension_for: |name| format!("Erweiterung {name} öffnen"),
    logins_extension: "Einstellungen ›",
    logins_for: |hosts| format!("Anmeldungen für {hosts}"),
    logins_none: "Keine Anmeldungen gefunden",
    logins_no_sources: "Keine aktivierte Erweiterung reicht Registry-Anmeldungen an Sandboxes weiter.",
    logins_off: "Deaktiviert. Auf der Seite Erweiterungen aktivieren.",
    open_accounts: "Konten öffnen",
    ext_accounts_moved: "Konten und Anmeldungen lassen sich auf der Seite Konten verwalten.",
    ext_hosts_heading: "Hosts",
    ext_hosts_note: "Netzwerkanfragen und Blocklist-Downloads sind auf diese Hosts, konfigurierte Konto-Hosts und in den Einstellungen genannte Hosts beschränkt.",
    ext_hosts_none: "Keine zusätzlichen Hosts.",
    not_a_host: |entry| format!("{entry} ist kein Hostname und kein *.domain"),
    not_a_url: |entry| format!("{entry} ist keine https://-URL"),
    discussion_open: |title| format!("Diskussion zu {title}"),
    discussion_loading: "Diskussion wird geladen…",
    discussion_waiting: "Warte auf das Repository und seinen Change Request…",
    discussion_no_change: "Dieser Branch hat keinen Change Request.",
    discussion_unsupported: "Keine aktivierte Erweiterung unterstützt Change Requests für die Forge dieses Repositorys. Aktiviere die passende Forge-Erweiterung unter Einstellungen › Erweiterungen (GitHub, GitLab, Forgejo, Gitea, Bitbucket, Azure DevOps) und melde dich dann unter Konten an.",
    discussion_failed: |error| format!("Forge nicht erreichbar: {error}"),
    git_dir_replaced: |path| {
        format!(
            "{path}/.git verweist nicht mehr auf das ursprüngliche Repository (möglicherweise hat \
             eine Sandbox es ersetzt). scriptschnell-term ignoriert es, aber git in Host-Terminals \
             liest es weiterhin."
        )
    },
    discussion_by: |author| format!("von {author}"),
    discussion_on_forge: "auf der Forge",
    discussion_pushes: "Pushes",
    discussion_pushed: "gepusht",
    discussion_force_pushed: "force-gepusht",
    discussion_rewritten: "umgeschrieben",
    discussion_conversation: "Unterhaltung",
    discussion_line: |path, line| format!("`{path}` Zeile {line}"),
    discussion_unchanged: |since| {
        if since.is_empty() {
            "Unverändert.".to_owned()
        } else {
            format!("Unverändert seit `{since}`.")
        }
    },
    discussion_moved: |old, since| {
        if since.is_empty() {
            format!("Von Zeile {old} verschoben.")
        } else {
            format!("Seit `{since}` von Zeile {old} verschoben.")
        }
    },
    discussion_changed: |old, since| {
        if since.is_empty() {
            format!("Zeile {old} wurde geändert; die aktualisierte Zeile wird angezeigt.")
        } else {
            format!(
                "Zeile {old} wurde seit `{since}` geändert; die aktualisierte Zeile wird angezeigt."
            )
        }
    },
    discussion_found: |old| {
        format!(
            "Anhand des Inhalts gefunden (zuvor Zeile {old}); der ursprüngliche Commit ist nicht in diesem Repository."
        )
    },
    discussion_file_gone: |since| {
        if since.is_empty() {
            "Datei wurde gelöscht.".to_owned()
        } else {
            format!("Datei seit `{since}` gelöscht.")
        }
    },
    discussion_line_gone: |old, since| {
        if since.is_empty() {
            format!("Zeile {old} wurde entfernt.")
        } else {
            format!("Zeile {old} seit `{since}` entfernt.")
        }
    },
    discussion_removed: |old| format!("Auf der entfernten Zeile {old} der Basis."),
    discussion_resolved: "erledigt",
    discussion_outdated: "auf der Forge veraltet",
    explore_change: "Merge-Request",
    explore_open_change: "Merge-Request anzeigen",
    change_overview: "Übersicht",
    change_checks: "Prüfungen",
    change_checks_label: |passed, total| format!("Prüfungen {passed}/{total}"),
    change_checks_summary: |passed, failed, waiting| {
        let mut parts = vec![format!("{passed} bestanden")];
        if failed > 0 {
            parts.push(format!("{failed} fehlgeschlagen"));
        }
        if waiting > 0 {
            parts.push(format!("{waiting} laufen oder warten"));
        }
        parts.join(", ")
    },
    change_threads: |open, resolved| format!("Threads: {open} offen, {resolved} erledigt"),
    change_reviews: "Reviews",
    change_approved: "genehmigt",
    change_requested_changes: "Änderungen angefordert",
    change_commented: "kommentiert",
    change_review_pending: "Review angefragt",
    change_draft: "Entwurf",
    change_conflicts: "Hat Merge-Konflikte mit dem Zielbranch.",
    change_pipeline: "Pipeline",
    change_sections: "Abschnitte des Merge-Requests",
    change_reload: "Neu laden",
    change_open_extensions: "Erweiterungseinstellungen öffnen",
    change_open_on_forge: "Auf der Forge öffnen",
    change_open_link: "Öffnen",
    change_open_check: |name| format!("{name} öffnen"),
    change_description: "Beschreibung",
    change_no_description: "Keine Beschreibung.",
    change_line: |line| format!("Zeile {line}"),
    search_worktrees: "Worktrees durchsuchen",
    focus_tabs: "Fokus wechseln: Tableiste, Statusleiste, Seitenleiste, Bereich",
    focus_back: "Fokus zurück: Seitenleiste, Statusleiste, Tableiste, Bereich",
    search_no_match: "Kein passender Worktree",
    title_worktrees_by_branch: "Worktrees nach Branch benennen",
    title_worktrees_by_agent: "Worktrees nach letzter Agent-Sitzung benennen",
    use_system_title_bar: "Titelleiste des Systems verwenden",
    use_app_title_bar: "Titelleiste in der Tab-Leiste zeichnen",
    jj_on: "jj für jj-Repositorys verwenden",
    cpu_mode_local: |mode| format!("CPU für Terminals auf diesem Rechner: {mode}"),
    cpu_mode_remote: |mode| format!("CPU für Terminals auf anderen Rechnern: {mode}"),
    cpu_off: "aus",
    cpu_balanced: "ausgewogen",
    cpu_quiet: "leise",
    cpu_detail: cpu_detail_de,
    cpu_status: |mode| format!("CPU {mode}"),
    cpu_status_hint: |status, next| format!("{status}, Klicken wechselt zu {next}"),
    cpu_status_tip: |mode, next| format!("CPU-Modus: {mode}. Klicken für {next}."),
    cpu_heading: "CPU",
    cpu_note: "Begrenzt die gesamte CPU-Nutzung aller Terminals auf einem Rechner, damit er während Builds benutzbar bleibt. Agenten in Hintergrund-Terminals laufen zudem mit niedrigerer Priorität. Änderungen wirken sofort auf laufende Terminals. Mit cpu_mode in settings.json lassen sich ausgewogen und leise anpassen.",
    cpu_local: "Dieser Rechner, seine Sandboxes, Devcontainer und VMs",
    cpu_remote: "Andere Rechner: SSH-Hosts und entfernte Rechner",
    jj_off: "jj für jj-Repositorys nicht mehr verwenden",
    project_jj_on: "jj in diesem Projekt verwenden",
    project_jj_off: "jj in diesem Projekt nicht mehr verwenden",
    run_task: |title| format!("Aufgabe ausführen: {title}"),
    no_folder_chooser: "kein nativer Ordnerdialog auf diesem System",
    open_folder: "Durchsuchen…",
    choose_folder: "Ordner wählen",
    choose_a_folder: "Einen Ordner wählen",
    branch_optional: "Branch (optional, leer für losgelösten HEAD)",
    creates_next_to: |repo| {
        format!("Legt {repo}-<name> neben {repo} an. Tab wechselt zum Branch-Feld.")
    },
    no_matching_command: "Kein passender Befehl oder Worktree",
    no_matching_panel: "Kein passendes Panel",

    state_not_loaded: |error| format!("Zustand nicht geladen: {error}"),
    settings_not_loaded: |error| format!("Einstellungen nicht geladen: {error}"),
    state_not_saved: |error| format!("Zustand nicht gespeichert: {error}"),
    settings_not_saved: |error| format!("Einstellungen nicht gespeichert: {error}"),
    not_a_path: |input| format!("kein Pfad: {input}"),
    repository_not_loaded: "Repository noch nicht geladen",
    bad_worktree_name: |name| format!("ungültiger Worktree-Name {name:?}"),
    bad_branch_name: "Branch-Namen dürfen keine Leerzeichen oder ~ ^ : ? * [ \\ .. enthalten",
    bad_folder_name: "Ordnernamen dürfen kein / oder \\ enthalten und nicht . oder .. lauten",
    max_panes: |max| format!("ein Tab zeigt höchstens {max} Bereiche"),
    blocklist_failed: |id, error| format!("Sperrliste {id}: Download fehlgeschlagen: {error}"),
    warning: |message| format!("Warnung: {message}"),
    error: |message| format!("Fehler: {message}"),
    no_worktree_at: |path| format!("kein Worktree unter {path}"),
    view_unknown: |view| format!("keine Erweiterung bietet die Ansicht {view}"),
    view_gone: |view| {
        format!(
            "{view} ist nicht mehr verfügbar. Die Erweiterung ist möglicherweise deaktiviert, oder der Inhalt wurde gelöscht."
        )
    },
    terminal_failed: |error| format!("Terminal fehlgeschlagen: {error}"),
    sandbox_rules_dropped: |groups| {
        format!("sandbox-exec hat Sandbox-Regeln abgelehnt; Sandboxes laufen ohne: {groups}")
    },
    sandbox_shell_replaced: |shell, with| {
        format!(
            "{shell} kann nicht in der Windows-Sandbox laufen, da sie den gemeinsamen Speicher \
             von Gits MSYS-Programmen blockiert; stattdessen wird {with} gestartet"
        )
    },
    sandbox_claude_without_bash: "Claude Code kann in der Windows-Sandbox keine Programme starten: Seine Hooks, Shell-Befehle und Werkzeuge schlagen mit \"operation not permitted\" fehl. Starte es stattdessen auf dem Host oder in einem Devcontainer.",
    terminal_failed_start: |code, output| {
        let status = code.map_or_else(|| "Signal".to_owned(), |code| format!("Code {code}"));
        if output.is_empty() {
            format!("Terminal wurde sofort beendet ({status})")
        } else {
            format!("Terminal wurde sofort beendet ({status}): {output}")
        }
    },
    paste_image_failed: |error| format!("Bild konnte nicht eingefügt werden: {error}"),
    run_on_host: "Terminals auf dem Host ausführen",
    run_in_devcontainer: "Terminals im Devcontainer ausführen",
    run_in_sandbox: "Terminals in einer Sandbox ausführen",
    rebuild_devcontainer: "Devcontainer neu bauen",
    runtime_devcontainer: "Devcontainer",
    runtime_sandbox: "Sandbox",
    runtime_sandbox_open: "Sandbox · geschwächt",
    runtime_sandbox_open_hint: "Container von hier laufen außerhalb der Sandbox. Klicken öffnet die Sandboxing-Einstellungen.",
    open_sandboxing: "Sandbox-Einstellungen öffnen",
    runtime_starting: "Devcontainer startet",
    no_devcontainer: "Dieser Worktree hat keine devcontainer.json",
    env_not_ready: "Die Umgebung des Worktrees ist noch nicht bereit",
    devcontainer_ready: |name| format!("Devcontainer {name} ist bereit"),
    devcontainer_failed: |error| format!("Devcontainer fehlgeschlagen: {error}"),
    copy_failed: |error| format!("Kopieren fehlgeschlagen: {error}"),
    run_failed: |error| format!("Befehl fehlgeschlagen: {error}"),
    trust_failed: |error| format!("Zertifikat hinzufügen fehlgeschlagen: {error}"),
    section_remote: "Entfernte Rechner",
    section_vms: "Virtuelle Maschinen",
    section_sandboxing: "Sandboxing",
    section_sandbox_files: "Dateien",
    section_sandbox_hosts: "Hosts",
    this_machine: "Dieser Rechner",
    your_key: "Fingerabdruck deines Schlüssels",
    your_key_named: |fingerprint| format!("Dein Schlüssel {fingerprint}"),
    copy_key_short: "Schlüssel kopieren",
    key_copied: "Öffentlicher Schlüssel kopiert",
    serve_machine: "Andere Rechner verbinden lassen",
    when_machines_connect: "Zugriff verbundener Rechner",
    pairing_steps: "Um zwei Rechner zu koppeln, kopiere deinen Schlüssel und lasse ihn auf dem anderen Rechner zu, dann füge den Rechner unter \"Rechner, mit denen du dich verbindest\" hinzu. Für den Zugriff in die andere Richtung dort dasselbe tun.",
    serve_on: |address| format!("Wenn aktiv, erreichen andere Rechner diesen unter {address}"),
    serving_on: |address| format!("Bereit auf {address}"),
    any_address: |port| format!("Port {port} auf allen Netzwerkschnittstellen"),
    serve_failed: |error| format!("Rechner konnte nicht freigegeben werden: {error}"),
    offer_host: "Shell dieses Rechners anbieten",
    offer_host_note: "Standard für neue Schlüssel. Wenn aus, sind nur VMs erreichbar.",
    offer_host_no_vms: "Aktiviert lassen, bis VMs eingerichtet sind.",
    share_projects: "Projekte dieses Rechners teilen",
    share_projects_note: "Standard für neue Schlüssel. Verbundene Clients zeigen geteilte Projekte in ihrer Seitenleiste.",
    web_links_vm_only: "Verknüpfte Webserver auf VMs beschränken",
    web_links_vm_only_note: "Wenn aktiv, erreichen verknüpfte Webserver nur VMs, nicht die Shell dieses Rechners.",
    allowed_keys: "Zugelassene Schlüssel",
    settings_results: "PASSENDE SEITEN",
    allowed_keys_not_serving: "Aktiviere „Andere Rechner verbinden lassen“, um eingehende Verbindungen anzunehmen.",
    no_allowed_keys: "Noch keine Schlüssel. Klicke auf dem anderen Rechner auf Schlüssel kopieren und füge ihn unten ein.",
    key_to_allow: "Öffentlicher Schlüssel zum Zulassen",
    vms_only: "Nur VMs",
    vms_only_note: "„Nur VMs zulassen“ gewährt Zugriff auf virtuelle Maschinen, nicht aber auf die Shell dieses Rechners.",
    allow: "Shell und VMs zulassen",
    allow_vms_only: "Nur VMs zulassen",
    paste_key_first: "Füge oben den öffentlichen Schlüssel des anderen Rechners ein, um die Zulassen-Schaltflächen zu aktivieren.",
    not_a_key: "kein gültiger öffentlicher cmdterm-Schlüssel",
    remove_named: |name| format!("{name} entfernen"),
    show_named: |name| format!("{name} zeigen"),
    hide_named: |name| format!("{name} ausblenden"),
    key_host: "Shell",
    key_host_named: |name| format!("Shell für {name}"),
    key_projects: "Projekte",
    key_projects_named: |name| format!("Projekte für {name}"),
    key_defaults: "Standard",
    key_defaults_named: |name| format!("Standard für {name} nutzen"),
    key_own_settings: "Eigene Einstellungen",
    known_machines: "Rechner, mit denen du dich verbindest",
    no_known_machines: "Noch keine vertrauten Rechner",
    machine_address: "Adresse des Rechners, host oder host:port",
    add_machine: "Rechner hinzufügen",
    add: "Hinzufügen",
    open: "Öffnen",
    open_on: |name| format!("Projekt auf {name} öffnen"),
    forget: "Vergessen",
    forget_named: |name| format!("{name} vergessen"),
    fuse_named: |name| format!("Projekte von {name} zeigen"),
    fused_unreachable: "Nicht erreichbar, neuer Versuch…",
    fetching_key: |address| format!("Schlüssel von {address} wird abgerufen…"),
    compare_key: |address| {
        format!(
            "{address} hat diesen Schlüssel. Vergleiche ihn mit cmdterm-remote keygen auf dem Rechner."
        )
    },
    trust: "Vertrauen",
    trust_named: |address| format!("{address} vertrauen"),
    cancel: "Abbrechen",
    cancel_set_up: |image| format!("Einrichtung von {image} abbrechen"),
    ok: "OK",
    already_trusted: |address| format!("{address} wird bereits vertraut"),
    key_changed: |address| {
        format!(
            "Der Schlüssel von {address} hat sich geändert. Jemand könnte die Verbindung abfangen. Vergiss den alten Schlüssel nur, wenn auf dem Server absichtlich ein neuer Schlüssel erzeugt wurde."
        )
    },
    connect_failed: |error| format!("Verbindung fehlgeschlagen: {error}"),
    remote_note: "Der andere Rechner muss deinen Schlüssel auch zulassen.",
    section_vpn: "VPN-Netze",
    vpn_scan: "Erneut nach VPN-Rechnern suchen",
    vpn_scan_again: "Neu suchen",
    vpn_scanning: "Suche nach Tailscale, NetBird und Nebula…",
    vpn_scan_running: "Suche läuft…",
    vpn_down: |vpn, why| {
        if why.is_empty() || why == "not connected" {
            format!("{vpn} ist installiert, aber nicht verbunden")
        } else {
            format!("{vpn} ist installiert, aber nicht verbunden ({why})")
        }
    },
    vpn_missing: |names| format!("Auf diesem Rechner nicht gefunden: {names}"),
    vpn_this_machine: |name| format!("Dieser Rechner: {name}"),
    vpn_serve: "Freigeben",
    vpn_serve_named: |address| format!("Diesen Rechner auf {address} freigeben"),
    vpn_no_peers: "Keine anderen Rechner in diesem Netz",
    online: "online",
    offline: "offline",
    ssh: "SSH",
    ssh_named: |peer| format!("Projekt auf {peer} über SSH öffnen"),
    remote: "Remote",
    remote_named: |peer| format!("Projekt auf {peer} als entfernten Rechner öffnen"),
    vpn_nebula_note: "Nebula führt keine Peer-Liste. Angezeigt werden Lighthouses, static_host_map und passende Einträge aus /etc/hosts.",
    vpn_note: "SSH öffnet ein Projekt über die VPN-Adresse des Rechners. Remote erfordert ein dort freigegebenes scriptschnell-term mit hier vertrautem Schlüssel. In der Befehlspalette sprechen tailscale://box/~/src, netbird://box/~/src und nebula://box/~/src einen Rechner direkt an.",
    config_not_saved: |error| format!("nicht gespeichert: {error}"),
    no_kvm: "/dev/kvm ist nicht verfügbar, VMs laufen hier nicht",
    local_vms: "Deine VMs",
    no_local_vms: "Noch keine VMs. Gib unten einen Namen ein, um eine zu öffnen.",
    running: "läuft",
    stopped: "gestoppt",
    reset: "Zurücksetzen",
    reset_note: "Löscht die Festplatte der VM, sodass sie beim nächsten Mal frisch vom Basis-Image startet.",
    delete_disk_of: |name| format!("{name} zurücksetzen"),
    vm_name: "Name der neuen VM",
    open_vm: "Projekt in dieser VM öffnen",
    bad_vm_name: "VM-Namen bestehen aus Buchstaben, Ziffern, Punkten, Binde- und Unterstrichen",
    stop_vms: "Alle VMs stoppen",
    vms_stopping: "VMs werden gestoppt",
    vm_disks_in: |dir| format!("Platten liegen in {dir}"),
    vm_fields: [
        "Firecracker",
        "Kernel",
        "Root-Dateisystem (Standard-Image)",
        "Plattenordner",
        "CPUs",
        "Speicher (MiB)",
        "Leerlauf-Stopp (s)",
    ],
    vm_field_hints: [
        "Name oder Pfad der firecracker-Datei",
        "Pfad zu einem vmlinux-Kernel",
        "Pfad zu einem ext4-Image, leer für das Standard-Image",
        "Ordner für die Platten der VMs",
        "Virtuelle CPUs je VM",
        "MiB je VM",
        "Sekunden bis zum Stopp",
    ],
    save_vm_settings: "VM-Einstellungen speichern",
    vm_settings_saved: "VM-Einstellungen gespeichert",
    not_a_number: |value| format!("keine Zahl: {value}"),
    vm_notes: &[
        "Für eigene Images. Ein leeres Feld verwendet den Standard.",
        "Laufende VMs behalten ihre bisherigen Einstellungen, bis sie stoppen.",
    ],
    ready_made_vms: "Fertige VMs",
    image_ready: |summary| format!("Bereit. {summary}"),
    set_up: "Einrichten",
    start_set_up: "Starten",
    set_up_explained: "Lädt Firecracker und einen Kernel herunter und baut das Image mit deiner Container-Engine. Klicke auf Starten oder Abbrechen.",
    set_up_image: |title| format!("{title} einrichten"),
    rebuild: "Neu bauen",
    rebuild_image: |title| format!("{title} neu bauen"),
    preparing: "Startet",
    install_first: |tools| format!("Installiere {tools}, um Images einzurichten."),
    vms_have_network: "VMs erreichen das Internet über pasta, ohne root.",
    vms_without_network: "VMs haben keinen Netzwerkzugriff. Installiere passt (pasta), um Netzwerk zu aktivieren.",
    vm_name_note: "Eine VM namens ubuntu oder ubuntu-irgendwas startet vom Ubuntu-Image.",
    advanced_settings: "Erweiterte Einstellungen",
    container_engine: "Devcontainer-Engine",
    automatic: "Automatisch: docker, wenn installiert, sonst podman",
    section_sandbox: "Sandbox",
    sandbox_network: "Netzwerk",
    sandbox_network_note: "Erlaubt Terminals in der Sandbox den Netzwerkzugriff",
    sandbox_restrict: "Nur erlaubte Hosts",
    sandbox_restrict_note: "Beschränkt Terminals in der Sandbox auf erlaubte Hosts (Sandboxing › Hosts) über HTTP_PROXY und HTTPS_PROXY",
    sandbox_host_network: "Netzwerk des Hosts",
    sandbox_host_network_note: "Aus: Sandboxes nutzen ein isoliertes Loopback. Server darin belegen keine Host-Ports, und nur HTTP und HTTPS laufen über den Proxy. An: Sandboxes teilen Netzwerk und Ports dieses Rechners.",
    sandbox_host_network_unscoped: "Diesem Kernel fehlt Landlock-ABI 6 (Linux 6.12). Sandboxes mit Host-Netzwerk können daher abstrakte Unix-Sockets dieses Rechners erreichen (einschließlich X11, wodurch Programme Eingaben in deine Fenster senden können). Schalte „Netzwerk des Hosts“ aus oder aktualisiere den Kernel.",
    sandbox_allowed_hosts: "Erlaubte Hosts",
    sandbox_hosts_inactive: "Diese Liste gilt nur, wenn unter Sandboxing sowohl Netzwerk als auch Nur erlaubte Hosts aktiviert sind.",
    sandbox_intercept_tls: "HTTPS prüfen",
    sandbox_intercept_tls_note: "Der Proxy entschlüsselt HTTPS mit einer eigenen CA, der Sandboxes vertrauen. HTTPS zu einem Host mit ungültigem Zertifikat schlägt in der Sandbox fehl.",
    sandbox_engines: "Container-Engines",
    sandbox_engines_note: "Dies schwächt die Sandbox. Terminals in der Sandbox greifen direkt auf docker, podman, containerd, buildkit und WSL-Interop zu. Gestartete Container laufen außerhalb der Sandbox mit vollem Datei- und Netzwerkzugriff der Engine.",
    engines_passthrough: "Direkt durchreichen (schwächt die Sandbox)",
    engines_sidecar: "Sidecar pro Projekt",
    engines_off: "Aus",
    engines_use_sidecar: "Sidecar-Engine nutzen",
    engines_turn_off: "Engine-Zugriff abschalten",
    engines_keep: "Direktes Durchreichen behalten",
    engines_fixes_note: "Ein Sidecar hält Container mit einer isolierten Engine am Laufen, die nur das Projekt sieht. Bei „Aus“ können Sandboxes keine Container starten.",
    sandbox_sidecar_note: "Jedes Projekt erhält eine isolierte Container-Engine, die sich die Sandboxes seiner Worktrees teilen. Sie sieht nur die Worktrees, ihren eigenen Speicher und die Registry-Zertifikate des Hosts. Ihr Netzwerk ist nicht auf erlaubte Hosts beschränkt. Nur unter Linux.",
    sidecar_engine: "Sidecar-Engine",
    sidecar_podman: "podman in podman",
    sidecar_docker: "docker in docker",
    sidecar_ready: |name| format!("Sidecar {name} ist bereit"),
    sidecar_failed: |error| format!("Sidecar fehlgeschlagen: {error}"),
    sandbox_host: "Host suchen oder hinzufügen, *.example.com für Subdomains",
    sandbox_host_none: |host| format!("Kein erlaubter Host passt zu \"{host}\"."),
    count_of: |shown, all| format!("{shown} von {all} passen"),
    sandbox_host_bare: "Liste wird gefiltert. Gib einen vollständigen Hostnamen ein (z. B. example.com), um ihn hinzuzufügen.",
    add_host: "Host hinzufügen",
    choose_hosts: "Häufige Hosts wählen",
    choose_hosts_open: "+ Häufige Hosts wählen…",
    host_setup_dialog: "Erlaubte Hosts für Sandboxes",
    host_setup_intro: "Terminals in der Sandbox erreichen nur die Hosts, die du erlaubst, und der Proxy prüft ihr HTTPS. Wähle, was deine Arbeit braucht.",
    host_setup_note: "Du kannst dies später unter Einstellungen › Sandboxing › Hosts ändern und dort auch einzelne Hosts hinzufügen.",
    host_setup_done: "Fertig",
    setup_dialog: "scriptschnell-term einrichten",
    setup_step: |at, of| format!("Schritt {at} von {of}"),
    setup_back: "Zurück",
    setup_next: "Weiter",
    setup_appearance: "Sprache, Tastatur und Farben",
    setup_appearance_intro: "Änderungen wirken sofort. Du kannst dies jederzeit unter Einstellungen (Strg+,) anpassen.",
    setup_runtime: "Wo Terminals laufen",
    setup_runtime_intro: "Wähle die Standardumgebung für Terminals in neu geöffneten Worktrees.",
    setup_shell: "Standard-Shell",
    setup_shell_intro: "Wähle die Shell, die neue Terminals starten. Die Seite Shells der Einstellungen ändert sie später.",
    new_agent: "Neuer Agenten-Tab",
    default_agent: "Standard-Agent",
    default_agent_first: "Erster eingerichteter",
    default_agent_named: |agent| format!("Standard-Agent: {agent}"),
    default_agent_note: "Neuer Agenten-Tab startet diesen Agenten. Ohne Auswahl startet er den ersten Agenten mit Integration auf dem Rechner des Projekts.",
    setup_runtime_note: "Später änderst du das unter Einstellungen › Sandboxing. Ein Projekt kann das auf seiner Einstellungsseite selbst festlegen.",
    setup_sandbox_note: "Beschränkt Schreibzugriffe auf den Worktree und filtert Netzwerkzugriffe über einen Proxy.",
    setup_sandbox_note_windows: "Unter Windows nicht empfohlen: Claude Code und andere Node.js-Programme können dort keine Programme starten, und Git Bash startet gar nicht. Nutze einen Devcontainer, um einen Worktree abzuschotten.",
    setup_devcontainer_note: "Nutzt die devcontainer.json des Worktrees, falls vorhanden, sonst die Sandbox.",
    setup_host_note: "Läuft direkt auf diesem Rechner mit deinen vollen Benutzerrechten.",
    setup_recommended: "Empfohlen",
    setup_agents: "Coding-Agenten",
    setup_agents_intro: "Ausgewählte Agenten erhalten Hooks, um ihren Status an scriptschnell-term zu melden, sowie einen Skill zur Bedienung der App. Abwählen entfernt beides.",
    settings_group_app: "ALLGEMEIN",
    settings_group_integrations: "INTEGRATIONEN",
    settings_group_machines: "MASCHINEN",
    settings_group_sandbox: "SANDBOX",
    settings_group_about: "ÜBER",
    section_changelog: "Änderungen",
    section_licenses: "Lizenzen",
    changelog_running: |version| format!("Du verwendest Version {version}."),
    changelog_unreleased: "Noch nicht veröffentlicht",
    licenses_app_hint: "scriptschnell-term steht nach deiner Wahl unter der MIT-Lizenz oder der Apache License 2.0.",
    license_mit: "MIT-Lizenz",
    license_apache: "Apache License 2.0",
    licenses_third_party: "Crates anderer Autoren",
    licenses_third_party_hint: |count| {
        format!(
            "Diese App enthält {count} Crates anderer Autoren. Klicke auf eines, um seine Lizenz zu lesen."
        )
    },
    licenses_none: "Dieser Build hat keine Liste der Crates, die er enthält.",
    license_in_file: "Lizenz in seinen Dateien",
    license_no_file: |license| {
        format!("Dieses Crate enthält keine Lizenzdatei. Seine Lizenz ist {license}.")
    },
    reset_settings: "Alles zurücksetzen",
    reset_settings_confirm: "Zum Bestätigen klicken",
    reset_settings_hint: "Setzt alle Einstellungen auf die Standardwerte zurück und öffnet die Ersteinrichtung",
    hosts_added_by_you: "Von dir oder einer Erweiterung hinzugefügt",
    host_removed: |host| format!("{host} entfernt"),
    undo: "Rückgängig",
    undo_remove_named: |host| format!("{host} wieder erlauben"),
    host_group: |id| match id {
        "forges" => "GitHub, GitLab, Codeberg, Bitbucket",
        "search" => "Suchmaschinen",
        "search_apis" => "Such-APIs wie Exa und Tavily",
        "ai" => "APIs von KI-Modellen",
        "docs" => "Dokumentation und Q&A",
        "rust" => "Rust: crates.io und rustup",
        "javascript" => "JavaScript: npm, Yarn, Node.js",
        "python" => "Python: PyPI",
        "go" => "Go-Module",
        "local" => "Dieser Rechner: localhost",
        "jvm" => "Java und Kotlin: Maven, Gradle",
        "dotnet" => ".NET: NuGet",
        "ruby_php" => "Ruby und PHP: RubyGems, Packagist",
        "containers" => "Container-Registries",
        "linux" => "Linux-Pakete: Debian, Ubuntu, Alpine",
        _ => "Andere Hosts",
    },
    bad_host: "Gib einen Hostnamen oder eine IP-Adresse ein, optional mit *. davor",
    sandbox_writable: "Zusätzliche beschreibbare Pfade",
    sandbox_hidden: "Vor der Sandbox verborgen",
    sandbox_path: "Pfad, ~/ erlaubt",
    add_writable: "Als beschreibbar hinzufügen",
    add_hidden: "Als verborgen hinzufügen",
    sandbox_note: "Änderungen gelten für neu gestartete Terminals.",
    restart_terminals: "Terminals dieses Worktrees neu starten",
    restart_terminals_hint: "Beendet laufende Programme und startet die Terminals mit den aktuellen Einstellungen neu",
    sandbox_windows_note: "Unter Windows beschränkt die Sandbox nur Schreibzugriffe. Verborgene Pfade bleiben lesbar und der Netzwerkzugriff bleibt aktiv.",
    section_project: "Projekt",
    project_settings: "Projekteinstellungen",
    settings_page: |page| format!("Einstellungen › {page}"),
    project_none: "Öffne ein Projekt, um seine Einstellungen zu ändern.",
    project_jj: "Jujutsu verwenden",
    project_jj_note: "Wenn das Projekt ein jj-Repository enthält",
    project_launches: "Agenten dürfen Worktrees ohne Nachfrage öffnen",
    section_project_allowed: "Erlaubt",
    section_project_blocked: "Blockiert",
    project_allowed_note: "Anfragen aus den Sandboxen dieses Projekts, die ohne Nachfrage durchgehen. \"In diesem Projekt immer erlauben\" im Freigabedialog fügt sie hier hinzu.",
    project_blocked_note: "Anfragen aus den Sandboxen dieses Projekts, die ohne Nachfrage abgelehnt werden. \"In diesem Projekt immer blockieren\" im Freigabedialog fügt sie hier hinzu.",
    project_answers_remote_note: "Nur Sandboxen von Projekten auf dieser Maschine verwenden diese Einträge.",
    project_answer_to_blocked: "Stattdessen blockieren",
    project_answer_to_allowed: "Stattdessen erlauben",
    project_answer_add_heading: "Eintrag hinzufügen",
    project_answer_guard: "Geschützte Anfrage",
    project_answer_host: "Host, beim Schlüsselbund der Name des Geheimnisses",
    project_answer_add: "Hinzufügen",
    project_launches_note: "Erlaubt Agenten, Worktrees anzulegen und dort automatisch Agenten zu starten",
    project_base: "Referenz-Branch",
    project_base_hint: "Leer für automatische Erkennung (origin/HEAD, main, master oder trunk)",
    project_base_note: "Standard-Branch oder Revision für Code-Reviews. Leer für automatische Erkennung (origin/HEAD, main, master oder trunk; default für Mercurial; trunk() für jj).",
    project_base_set: "Setzen",
    project_env: "Umgebungsvariablen",
    project_env_note_keyring: "Gilt für neu gestartete Terminals in diesem Projekt, auch in Sandboxes. Werte werden verschlüsselt im System-Schlüsselbund gespeichert und nicht in state.json geschrieben.",
    project_env_note_plain: "Gilt für neu gestartete Terminals in diesem Projekt, auch in Sandboxes. Kein System-Schlüsselbund verfügbar, daher werden Werte in state.json gespeichert (nur für deinen Benutzer lesbar).",
    project_env_note_temporary: "Gilt für neu gestartete Terminals in dieser temporären Sitzung. Werte bleiben nur bis zum Beenden im Speicher.",
    project_env_remote_note: "Dieses Projekt liegt auf einem anderen Rechner. Terminals dort erhalten Variablen über die Kommandozeile, die andere Benutzer auf diesem Rechner einsehen können.",
    project_env_name: "Variablenname",
    project_env_value: "Wert",
    project_env_add: "Speichern",
    project_env_reload: "Aus Schlüsselbund laden",
    project_env_invalid_name: "Variablennamen müssen mit einem Buchstaben oder Unterstrich beginnen und dürfen nur Buchstaben, Ziffern und Unterstriche enthalten.",
    project_env_invalid_value: "Variablenwerte dürfen keine Nullzeichen enthalten.",
    project_env_storage_keyring: "Verschlüsselt im System-Schlüsselbund",
    project_env_storage_loading: "Wird aus dem Schlüsselbund geladen…",
    project_env_storage_locked: "Im Schlüsselbund nicht verfügbar; zum Entsperren neu laden",
    project_env_storage_plain: "In state.json gespeichert",
    project_env_storage_session: "Für diese Sitzung im Speicher",
    project_folders: "Sandbox-Ordner",
    project_folders_note: "Zusätzliche Ordner auf dem Rechner des Projekts für Terminals in der Sandbox. Ein schreibgeschützter Ordner innerhalb eines verborgenen Pfads bleibt dennoch sichtbar. Ordner, die einen verborgenen Pfad oder App-Dateien enthalten, werden übersprungen. Änderungen gelten für neu gestartete Terminals.",
    project_writable: "Beschreibbar",
    project_readable: "Nur lesbar",
    no_project_folders: "Noch keine",
    add_readable: "Als nur lesbar hinzufügen",
    project_remote_note: "Dieses Projekt liegt auf einem anderen Rechner. Dort gelten nur absolute Pfade.",
    project_not_sandboxed: "Dieser Worktree läuft nicht in einer Sandbox, daher gelten diese Ordnerregeln nicht.",
    default_runtime_note: "Wo die Terminals eines Worktrees laufen, wenn die App ihn zum ersten Mal öffnet. Ein Projekt kann das auf seiner Einstellungsseite selbst festlegen, und die Palette ändert es für einen einzelnen Worktree.",
    project_runtime_note: "Ersetzt für die Worktrees dieses Projekts den Standard aus Sandboxing und verschiebt die bekannten. Ein Devcontainer gilt nur für Worktrees mit einer devcontainer.json; die anderen nutzen den Standard.",
    runtime_follow_default: "Standard aus Sandboxing verwenden",
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SYSTEM_LANGUAGE;

    #[test]
    fn locales_pick_german_only_for_german() {
        assert_eq!(Lang::from_locale("de_DE.UTF-8"), Lang::De);
        assert_eq!(Lang::from_locale("de_AT"), Lang::De);
        assert_eq!(Lang::from_locale("de:en"), Lang::De);
        assert_eq!(Lang::from_locale("DE"), Lang::De);
        assert_eq!(Lang::from_locale("en_US.UTF-8"), Lang::En);
        assert_eq!(Lang::from_locale("fr_FR"), Lang::En);
        assert_eq!(Lang::from_locale("C"), Lang::En);
        assert_eq!(Lang::from_locale(""), Lang::En);
        assert_eq!(Lang::from_locale("dev"), Lang::En);
    }

    #[test]
    fn settings_ids_resolve() {
        assert_eq!(Lang::resolve("de"), Lang::De);
        assert_eq!(Lang::resolve("en"), Lang::En);
        // Tests see an English locale.
        assert_eq!(Lang::resolve(SYSTEM_LANGUAGE), Lang::En);
    }

    #[test]
    fn german_preset_descriptions_cover_every_preset() {
        for preset in &crate::keymap::PRESETS {
            assert_ne!(
                (DE.preset_description)(preset),
                preset.description,
                "{} has no German description",
                preset.id
            );
        }
    }

    #[test]
    fn tables_format_values() {
        assert_eq!(
            (EN.switch_to_worktree)("app", "main"),
            "Switch to app / main"
        );
        assert_eq!(
            (DE.switch_to_worktree)("app", "main"),
            "Wechseln zu app / main"
        );
        assert_eq!(DE.action(&Action::Pane(3)), "Bereich 3");
        assert_eq!(EN.action(&Action::SendText("\u{2}".into())), "Send \\u{2}");
    }
}
