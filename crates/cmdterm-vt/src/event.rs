//! Events the terminal queues for its owner.

use crate::osc::{CurrentDir, Progress};
use crate::parser::OscEnd;

/// Something that happened that is not a screen change. Read with
/// [`Terminal::drain_events`](crate::Terminal::drain_events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Bell,
    /// OSC 0 or 2, or a title restored from the title stack. The variant owns its
    /// string, so a title change allocates.
    Title(String),
    /// OSC 1, or an icon name restored from the title stack.
    IconName(String),
    /// OSC 7: the program's working directory changed. Also
    /// [`Terminal::current_dir`](crate::Terminal::current_dir).
    CurrentDir(CurrentDir),
    /// OSC 52 set: the program put `data` (decoded) on the clipboard. `selection` is the
    /// target letters as sent (`c`, `p`, `s`, `0` to `7`); an empty target means `s0`
    /// as in xterm and arrives as `"s0"`.
    Clipboard {
        selection: String,
        data: Vec<u8>,
    },
    /// OSC 52 query. Only queued when [`Config::allow_clipboard_query`](crate::Config) is
    /// on. Answer it with [`Terminal::answer_clipboard`](crate::Terminal::answer_clipboard).
    ClipboardQuery {
        selection: String,
        bell: bool,
    },
    /// OSC 9 or OSC 777;notify: a desktop notification.
    Toast {
        title: Option<String>,
        body: String,
    },
    /// OSC 9;4: a progress report.
    Progress(Progress),
    /// OSC 1337 SetUserVar: `value` is the decoded base64.
    UserVar {
        name: String,
        value: String,
    },
    /// OSC 4, 10, 11 or 12 set a color, or OSC 104 or 110 to 112 reset one. Read the new
    /// colors from [`Terminal::palette`](crate::Terminal::palette).
    PaletteChanged,
    /// An OSC the terminal does not handle: the text before the first `;`, everything
    /// after it byte for byte, and how the string ended (so a reply can end the same way).
    Osc {
        code: String,
        payload: Vec<u8>,
        end: OscEnd,
    },
}
