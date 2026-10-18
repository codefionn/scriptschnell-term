//! Terminal sessions: PTY processes (local or over SSH) emulated by cmdterm-vt
//! and mirrored into SchnellUI's retained terminal grid.

mod attention;
mod config;
pub mod identity;
pub mod keys;
mod session;
mod sync;

pub(crate) use attention::clean_title;
pub use attention::{AGENT_VAR, Attention, SESSION_VAR};
pub use cmdterm_vt::{KeyCode, Mods, MouseButton, MouseEvent, MouseEventKind, Terminal};
pub use config::{SCROLLBACK, TermColors};
pub use session::{SessionInput, SessionStatus, TermSize, TerminalSession, Wake};
pub use sync::GridSync;
