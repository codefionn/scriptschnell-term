//! The terminal: parser plus screen state and the core VT behavior.
//!
//! The emulator never writes anywhere. Replies to the program (device attributes, cursor
//! position reports) collect in a buffer read with [`Terminal::take_replies`]. Events
//! (bell, title, unknown OSC) collect in a queue read with [`Terminal::drain_events`].
//! Both are bounded; when the owner does not read them, the newest entries are dropped.
//!
//! Compatibility reference is xterm. Pending wrap works as in xterm: printing in the
//! last column leaves the cursor there and the next printable character wraps.
//!
//! # Grapheme clusters
//!
//! A character that continues the cluster in the cell left of the cursor joins that
//! cell (see `grapheme.rs`). The decision uses only the cell and the cursor, as in
//! xterm, so cursor moves between the parts of a cluster do not matter. A zero-width
//! character with no cell to join (cursor at column 0) is dropped. A cluster that
//! grows from one column to two takes the next cell, shifting the line in insert mode;
//! at the right margin, where there is no next cell, it stays one column. Wezterm does
//! the same for plain widening and differs where `COMPAT.md` says.
//!
//! # Resize
//!
//! xterm cuts lines on a column change; wezterm and most newer terminals reflow. This
//! terminal reflows the primary screen and its scrollback (`reflow.rs`) and cuts the
//! alternate screen, as the settled design says. `COMPAT.md` has the
//! notes.
//!
//! # Collection
//!
//! The style, link and cluster tables only grow while text is written. When a table has
//! grown well past what was live at the last collection, and a quarter of the scrollback
//! has left it (or many rows were written), the terminal marks the ids that cells, the
//! pen and the saved cursors still use, drops the rest and renumbers cell style ids.
//! Collection scans every cell once and is not done per character.
//!
//! # Allocation
//!
//! Printing, cursor movement, erasing, scrolling and SGR allocate nothing once the rows
//! have grown to the width in use and the styles seen are interned, also when the
//! scrollback is full. Sequences that rebuild structures (resize and reflow, RIS,
//! margin-limited scrolling, table collection) may allocate.

use crate::cell::{Cell, Width};
use crate::charset::Charset;
use crate::color::Color;
use crate::color::{Palette, Rgb, XTERM_ANSI};
use crate::event::Event;
use crate::grapheme::{
    ClusterTable, cluster_width, is_one_cluster, is_plain, may_prepend, surely_joins,
};
use crate::input::key::{KeyCode, KeyEventKind, KeyModes, KittyStack, encode_key};
use crate::input::mouse::{
    MouseButton, MouseEncoding, MouseEvent, MouseState, MouseTracking, encode_alt_scroll,
    encode_mouse,
};
use crate::input::{Mods, encode_focus, encode_paste};
use crate::parser::{MAX_PARAMS, OscEnd, Params, Parser, Perform, osc_split};
use crate::screen::{Region, Row, Screen};
use crate::style::{Style, StyleId, StyleTable, Underline, flags};
use std::collections::VecDeque;
use std::io::Write;
use unicode_width::UnicodeWidthChar;

mod osc_handlers;
use crate::osc::{CurrentDir, LinkTable, Marks, SemanticZone};
use osc_handlers::{Dcs, TitleEntry};

/// Private modes XTSAVE and XTRESTORE keep.
const SAVABLE: [u16; 25] = [
    1, 3, 5, 6, 7, 9, 12, 25, 40, 47, 66, 69, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016,
    1047, 1049, 2004, 2026,
];

const MAX_REPLIES: usize = 1 << 16;
const MAX_EVENTS: usize = 4096;

/// Terminal size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub rows: usize,
    pub cols: usize,
}

/// Settings fixed at creation.
#[derive(Debug, Clone)]
pub struct Config {
    /// Palette entries 0 to 15.
    pub ansi: [Rgb; 16],
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    /// Most lines kept in the scrollback of the primary screen.
    pub scrollback: usize,
    /// Size of one cell in pixels, `(width, height)`, for SGR-pixels mouse reports
    /// (mode 1016). `(0, 0)` means unknown; reports then use cell coordinates. Change it
    /// later with [`Terminal::set_cell_pixels`].
    pub cell_pixels: (u32, u32),
    /// Whether a program may ask for the clipboard with OSC 52 (`?` as data). Off by
    /// default: a program that can read the clipboard can read secrets. When on, the
    /// query arrives as [`Event::ClipboardQuery`] and the owner answers it.
    pub allow_clipboard_query: bool,
    /// What XTVERSION (`CSI > q`) answers, in xterm's `name(version)` form.
    pub xtversion: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ansi: XTERM_ANSI,
            fg: XTERM_ANSI[7],
            bg: XTERM_ANSI[0],
            cursor: XTERM_ANSI[7],
            scrollback: 10_000,
            cell_pixels: (0, 0),
            allow_clipboard_query: false,
            xtversion: concat!("cmdterm-vt(", env!("CARGO_PKG_VERSION"), ")").to_string(),
        }
    }
}

/// Where the cursor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    /// The last character filled the line; the next printable one wraps.
    pub pending_wrap: bool,
    /// The shape DECSCUSR chose.
    pub shape: CursorShape,
    /// The cursor blinks (DECSCUSR or mode 12).
    pub blink: bool,
}

/// The cursor shape, from DECSCUSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Mode flags that tests and renderers read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub autowrap: bool,
    pub origin: bool,
    pub insert: bool,
    /// DECLRMM, mode 69.
    pub left_right_margins: bool,
    pub alt_screen: bool,
    /// DECCKM, mode 1: cursor keys send SS3 forms.
    pub app_cursor: bool,
    /// DECKPAM (ESC =) or DECNKM, mode 66.
    pub app_keypad: bool,
    /// DECSCNM, mode 5: the renderer swaps foreground and background.
    pub reverse_video: bool,
    /// Mode 12 or DECSCUSR: the cursor blinks.
    pub cursor_blink: bool,
    /// The mouse tracking level (modes 9, 1000, 1002, 1003).
    pub mouse: MouseTracking,
    /// The mouse coordinate encoding (modes 1005, 1006, 1015, 1016).
    pub mouse_encoding: MouseEncoding,
    /// Mode 1004.
    pub focus_events: bool,
    /// Mode 1007.
    pub alternate_scroll: bool,
    /// Mode 2004.
    pub bracketed_paste: bool,
    /// Mode 2026: the program asked to hold rendering until it ends the update. The owner
    /// decides when to render.
    pub synchronized_output: bool,
    /// LNM, ANSI mode 20.
    pub newline: bool,
    /// Mode 40: DECCOLM may change the column mode.
    pub allow_132: bool,
    /// DECCOLM, mode 3, as xterm tracks it (only changes with mode 40 on). The terminal
    /// does not resize itself; the owner may.
    pub columns_132: bool,
    /// `modifyOtherKeys` level, 0 to 2.
    pub modify_other_keys: u8,
    /// Kitty keyboard protocol flags of the active screen.
    pub kitty_flags: u8,
}

#[derive(Clone)]
struct Saved {
    row: usize,
    col: usize,
    pending: bool,
    style: Style,
    origin: bool,
    g: [Charset; 4],
    gl: usize,
}

struct Core {
    config: Config,
    screens: [Screen; 2],
    active: usize,
    rows: usize,
    cols: usize,
    row: usize,
    col: usize,
    pending: bool,
    visible: bool,
    style: Style,
    style_id: StyleId,
    erase_id: StyleId,
    styles: StyleTable,
    palette: Palette,
    autowrap: bool,
    origin: bool,
    insert: bool,
    lrmm: bool,
    app_cursor: bool,
    app_keypad: bool,
    reverse: bool,
    blink: bool,
    shape: CursorShape,
    mouse: MouseState,
    focus_events: bool,
    alt_scroll: bool,
    bracketed: bool,
    sync_output: bool,
    lnm: bool,
    allow_132: bool,
    cols_132: bool,
    modify_other_keys: u8,
    kitty: [KittyStack; 2],
    saved_modes: [u8; SAVABLE.len()],
    saved_tracking: Option<MouseTracking>,
    saved_encoding: Option<MouseEncoding>,
    saved_alt: Option<bool>,
    top: usize,
    bottom: usize,
    left: usize,
    right: usize,
    tabs: Vec<bool>,
    g: [Charset; 4],
    gl: usize,
    single_shift: Option<usize>,
    saved: [Option<Saved>; 2],
    last_char: Option<char>,
    clusters: ClusterTable,
    /// Scratch marks for cluster collection, kept so a sweep does not allocate.
    cluster_marks: Vec<bool>,
    /// Dropped rows (all screens) at the last sweep.
    swept_dropped: u64,
    /// Screen sequence sum at the last sweep.
    swept_seq: u64,
    /// Ids in use after the last sweep.
    live_after_sweep: usize,
    sweeps: u64,
    /// Styles, links and link bytes in use after the last style and link collection.
    live_styles: usize,
    live_links: usize,
    live_link_bytes: usize,
    /// Dropped rows and screen sequence sum at the last style and link collection.
    tables_dropped: u64,
    tables_seq: u64,
    table_sweeps: u64,
    /// Scratch for style and link collection: marks and old-to-new id maps.
    style_marks: Vec<bool>,
    link_marks: Vec<bool>,
    style_map: Vec<u32>,
    link_map: Vec<u32>,
    /// The last print landed on the last column with wrap-around off, so the cursor
    /// sits on the cell it printed. Any other sequence clears it.
    edge_print: bool,
    /// Scratch text for joining a cluster with the next character.
    joined: String,
    replies: Vec<u8>,
    events: VecDeque<Event>,
    title: String,
    icon: String,
    title_stack: Vec<TitleEntry>,
    cwd: Option<CurrentDir>,
    links: LinkTable,
    marks: Marks,
    dcs: Dcs,
    dcs_data: Vec<u8>,
    dcs_overflow: bool,
}

/// Row touches since the last sweep before a full cluster table is swept again.
const SWEEP_RETRY_WRITES: u64 = 4096;

/// Row touches since the last style and link collection after which a table that has
/// grown is collected even though no row left the scrollback (a program that redraws the
/// screen with ever new colors or links).
const TABLE_SWEEP_WRITES: u64 = 1 << 16;

/// A terminal emulator instance.
pub struct Terminal {
    parser: Parser,
    core: Core,
}

impl Terminal {
    /// Number of cluster collections so far. For tests.
    #[doc(hidden)]
    pub fn cluster_sweeps(&self) -> u64 {
        self.core.sweeps
    }

    /// Entries in the style table, the link table, and the bytes of link text. For tests.
    #[doc(hidden)]
    pub fn table_sizes(&self) -> (usize, usize, usize) {
        let c = &self.core;
        (c.styles.len(), c.links.len(), c.links.bytes())
    }

    /// Number of style and link collections so far. For tests.
    #[doc(hidden)]
    pub fn table_sweeps(&self) -> u64 {
        self.core.table_sweeps
    }

    /// Number of semantic marks kept. For tests.
    #[doc(hidden)]
    pub fn mark_count(&self) -> usize {
        self.core.marks.len()
    }

    pub fn new(size: Size, config: Config) -> Self {
        Self {
            parser: Parser::new(),
            core: Core::new(size, config),
        }
    }

    /// Parses `bytes` and applies them. Input may be split anywhere.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.core, bytes);
    }

    /// Moves the pending replies to `out`.
    pub fn take_replies(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.core.replies);
    }

    /// Removes and returns the queued events, oldest first.
    pub fn drain_events(&mut self) -> std::collections::vec_deque::Drain<'_, Event> {
        self.core.events.drain(..)
    }

    pub fn size(&self) -> Size {
        Size {
            rows: self.core.rows,
            cols: self.core.cols,
        }
    }

    pub fn cursor(&self) -> Cursor {
        let c = &self.core;
        Cursor {
            row: c.row,
            col: c.col,
            visible: c.visible,
            pending_wrap: c.pending,
            shape: c.shape,
            blink: c.blink,
        }
    }

    pub fn modes(&self) -> Modes {
        let c = &self.core;
        Modes {
            autowrap: c.autowrap,
            origin: c.origin,
            insert: c.insert,
            left_right_margins: c.lrmm,
            alt_screen: c.active == 1,
            app_cursor: c.app_cursor,
            app_keypad: c.app_keypad,
            reverse_video: c.reverse,
            cursor_blink: c.blink,
            mouse: c.mouse.tracking,
            mouse_encoding: c.mouse.encoding,
            focus_events: c.focus_events,
            alternate_scroll: c.alt_scroll,
            bracketed_paste: c.bracketed,
            synchronized_output: c.sync_output,
            newline: c.lnm,
            allow_132: c.allow_132,
            columns_132: c.cols_132,
            modify_other_keys: c.modify_other_keys,
            kitty_flags: c.kitty[c.active].flags(),
        }
    }

    /// Whether the program asked for mouse reports. The owner then sends pointer events
    /// to [`Terminal::encode_mouse`] instead of using them for selection and scrolling.
    pub fn is_mouse_grabbed(&self) -> bool {
        self.core.mouse.tracking != MouseTracking::Off
    }

    /// Tells the terminal how big a cell is in pixels, for mode 1016.
    pub fn set_cell_pixels(&mut self, width: u32, height: u32) {
        self.core.config.cell_pixels = (width, height);
    }

    fn key_modes(&self) -> KeyModes {
        let c = &self.core;
        KeyModes {
            app_cursor: c.app_cursor,
            app_keypad: c.app_keypad,
            newline: c.lnm,
            modify_other_keys: c.modify_other_keys,
            kitty_flags: c.kitty[c.active].flags(),
        }
    }

    /// Encodes a key event into `out` under the current modes. `text` is the text the key
    /// produces, if known. Returns whether anything was written.
    pub fn encode_key(
        &self,
        key: KeyCode,
        mods: Mods,
        kind: KeyEventKind,
        text: Option<&str>,
        out: &mut Vec<u8>,
    ) -> bool {
        encode_key(&self.key_modes(), key, mods, kind, text, out)
    }

    /// Encodes a pointer event. Returns false, writing nothing, when the program has not
    /// asked for mouse reports or the event is not one that mode reports. The terminal
    /// remembers held buttons, so a drag may arrive as a move with [`MouseButton::None`].
    pub fn encode_mouse(&mut self, ev: &MouseEvent, out: &mut Vec<u8>) -> bool {
        let c = &mut self.core;
        let size = (c.rows, c.cols);
        let px = c.config.cell_pixels;
        encode_mouse(&mut c.mouse, ev, size, px, out)
    }

    /// Alternate scroll (mode 1007): turns a wheel notch into a cursor key when the
    /// alternate screen is active, the mode is on and the mouse is not grabbed.
    pub fn encode_alt_scroll(&self, button: MouseButton, out: &mut Vec<u8>) -> bool {
        let c = &self.core;
        if c.active != 1 || !c.alt_scroll || self.is_mouse_grabbed() {
            return false;
        }
        encode_alt_scroll(&self.key_modes(), button, out)
    }

    /// Encodes pasted text, in brackets when mode 2004 is on.
    pub fn encode_paste(&self, text: &str, out: &mut Vec<u8>) {
        encode_paste(self.core.bracketed, text, out);
    }

    /// Encodes a focus change when mode 1004 is on. Returns whether anything was written.
    pub fn encode_focus(&self, focused: bool, out: &mut Vec<u8>) -> bool {
        encode_focus(self.core.focus_events, focused, out)
    }

    /// The style new text is drawn with (the SGR state).
    pub fn pen(&self) -> &Style {
        &self.core.style
    }

    /// The character set slot (0 to 3) invoked into GL.
    pub fn gl(&self) -> usize {
        self.core.gl
    }

    /// The active screen.
    pub fn screen(&self) -> &Screen {
        &self.core.screens[self.core.active]
    }

    /// Viewport row `y`.
    pub fn row(&self, y: usize) -> Option<&Row> {
        self.screen().row(y)
    }

    /// The line with this stable index (scrollback or viewport).
    pub fn row_by_stable(&self, stable: u64) -> Option<&Row> {
        self.screen().row_by_stable(stable)
    }

    pub fn scrollback_len(&self) -> usize {
        self.screen().scrollback_len()
    }

    pub fn stable_index(&self, y: usize) -> u64 {
        self.screen().stable_index(y)
    }

    /// Scroll region as `(top, bottom, left, right)`, inclusive.
    pub fn margins(&self) -> (usize, usize, usize, usize) {
        let c = &self.core;
        (c.top, c.bottom, c.left, c.right)
    }

    /// The viewport text, one line per row, trailing blanks removed.
    pub fn screen_text(&self) -> Vec<String> {
        (0..self.core.rows)
            .map(|y| self.row(y).map(|r| self.row_text(r)).unwrap_or_default())
            .collect()
    }

    /// The text of a row of this terminal, with whole clusters, without the right halves
    /// of wide characters and without trailing blanks.
    pub fn row_text(&self, row: &Row) -> String {
        row.text(&self.core.clusters)
    }

    /// The text of the cell: its whole grapheme cluster. A blank is a space and the
    /// right half of a wide character is a space too. Does not allocate.
    pub fn cell_text<'a>(&'a self, cell: &'a Cell) -> &'a str {
        match cell.cluster() {
            0 => cell.first(),
            id => self.core.clusters.get(id),
        }
    }

    pub fn style(&self, id: StyleId) -> &Style {
        self.core.styles.get(id)
    }

    pub fn palette(&self) -> &Palette {
        &self.core.palette
    }

    pub fn palette_mut(&mut self) -> &mut Palette {
        &mut self.core.palette
    }

    /// The current title (OSC 0 or 2, or restored from the title stack).
    pub fn title(&self) -> &str {
        &self.core.title
    }

    /// The current icon name (OSC 0 or 1).
    pub fn icon_name(&self) -> &str {
        &self.core.icon
    }

    /// The directory the program last reported with OSC 7.
    pub fn current_dir(&self) -> Option<&CurrentDir> {
        self.core.cwd.as_ref()
    }

    /// The URI of the hyperlink (OSC 8) the cell carries, if any. Does not allocate.
    pub fn hyperlink(&self, cell: &Cell) -> Option<&str> {
        let id = self.core.styles.get(cell.style).hyperlink;
        self.core.links.uri(id)
    }

    /// The `id=` parameter of the cell's hyperlink, if it had one.
    pub fn hyperlink_id(&self, cell: &Cell) -> Option<&str> {
        let id = self.core.styles.get(cell.style).hyperlink;
        self.core.links.id(id)
    }

    /// The prompt, input and output zones of the primary screen (OSC 133), oldest first,
    /// in stable row indexes. Empty when the shell never sent a mark. Cold: allocates.
    pub fn semantic_zones(&self) -> Vec<SemanticZone> {
        self.core.semantic_zones()
    }

    /// Answers an [`Event::ClipboardQuery`] with the clipboard text. `bell` ends the reply
    /// the way the request ended.
    pub fn answer_clipboard(&mut self, selection: &str, data: &[u8], bell: bool) {
        self.core.clipboard_reply(selection, data, bell);
    }

    pub fn is_tab_stop(&self, col: usize) -> bool {
        self.core.tabs.get(col).copied().unwrap_or(false)
    }

    /// Changes the size. A column change reflows the primary screen and its scrollback
    /// (see `reflow.rs`): lines rewrap, the cursor, the saved cursor and the semantic
    /// marks stay on their characters, and every row gets a new sequence number. xterm
    /// itself cuts instead; wezterm reflows. The alternate screen is cut or padded. A
    /// change of the row count alone pushes rows into the scrollback or pads blank rows.
    pub fn resize(&mut self, size: Size) {
        self.core.resize(size);
    }
}

fn arg(args: &[u16], i: usize, default: usize) -> usize {
    match args.get(i) {
        Some(&v) if v != 0 => usize::from(v),
        _ => default,
    }
}

impl Core {
    fn new(size: Size, config: Config) -> Self {
        let rows = size.rows.max(1);
        let cols = size.cols.max(1);
        let palette = Palette::new(&config);
        let limit = config.scrollback;
        let mut c = Core {
            screens: [Screen::new(rows, cols, limit), Screen::new(rows, cols, 0)],
            config,
            active: 0,
            rows,
            cols,
            row: 0,
            col: 0,
            pending: false,
            visible: true,
            style: Style::default(),
            style_id: StyleId::DEFAULT,
            erase_id: StyleId::DEFAULT,
            styles: StyleTable::new(),
            palette,
            autowrap: true,
            origin: false,
            insert: false,
            lrmm: false,
            app_cursor: false,
            app_keypad: false,
            reverse: false,
            blink: false,
            shape: CursorShape::Block,
            mouse: MouseState::default(),
            focus_events: false,
            alt_scroll: false,
            bracketed: false,
            sync_output: false,
            lnm: false,
            allow_132: false,
            cols_132: false,
            modify_other_keys: 0,
            kitty: [KittyStack::new(); 2],
            saved_modes: [0; SAVABLE.len()],
            saved_tracking: None,
            saved_encoding: None,
            saved_alt: None,
            top: 0,
            bottom: rows - 1,
            left: 0,
            right: cols - 1,
            tabs: Vec::new(),
            g: [Charset::Ascii; 4],
            gl: 0,
            single_shift: None,
            saved: [None, None],
            last_char: None,
            clusters: ClusterTable::default(),
            cluster_marks: Vec::new(),
            swept_dropped: 0,
            swept_seq: 0,
            live_after_sweep: 0,
            sweeps: 0,
            live_styles: 1,
            live_links: 0,
            live_link_bytes: 0,
            tables_dropped: 0,
            tables_seq: 0,
            table_sweeps: 0,
            style_marks: Vec::new(),
            link_marks: Vec::new(),
            style_map: Vec::new(),
            link_map: Vec::new(),
            edge_print: false,
            joined: String::new(),
            replies: Vec::new(),
            events: VecDeque::new(),
            title: String::new(),
            icon: String::new(),
            title_stack: Vec::new(),
            cwd: None,
            links: LinkTable::default(),
            marks: Marks::default(),
            dcs: Dcs::None,
            dcs_data: Vec::new(),
            dcs_overflow: false,
        };
        c.reset_tabs();
        c
    }

    fn reset_tabs(&mut self) {
        self.tabs.clear();
        self.tabs.extend((0..self.cols).map(|x| x % 8 == 0));
    }

    fn scr(&mut self) -> &mut Screen {
        &mut self.screens[self.active]
    }

    fn region(&self) -> Region {
        Region {
            top: self.top,
            bottom: self.bottom,
            left: self.left,
            right: self.right,
        }
    }

    fn full_width(&self) -> bool {
        self.left == 0 && self.right + 1 >= self.cols
    }

    fn refresh_style(&mut self) {
        self.maybe_collect_tables();
        self.style_id = self.styles.intern(&self.style);
        // Like xterm's ClearCells: erased cells keep the pen's colors, not its attributes.
        let erase = Style {
            fg: self.style.fg,
            bg: self.style.bg,
            ..Style::default()
        };
        self.erase_id = self.styles.intern(&erase);
    }

    fn event(&mut self, e: Event) {
        if self.events.len() < MAX_EVENTS {
            self.events.push_back(e);
        }
    }

    fn reply(&mut self, bytes: &[u8]) {
        if self.replies.len() + bytes.len() <= MAX_REPLIES {
            self.replies.extend_from_slice(bytes);
        }
    }

    // Movement and scrolling ---------------------------------------------------------

    fn index(&mut self) {
        self.pending = false;
        if self.row == self.bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.pending = false;
        if self.row == self.top {
            let (region, style) = (self.region(), self.erase_id);
            self.scr().scroll_down(region, 1, style);
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let to_sb = self.top == 0 && self.full_width() && self.active == 0;
        let (region, style) = (self.region(), self.erase_id);
        self.scr().scroll_up(region, n, style, to_sb);
    }

    fn scroll_down(&mut self, n: usize) {
        let (region, style) = (self.region(), self.erase_id);
        self.scr().scroll_down(region, n, style);
    }

    fn carriage_return(&mut self) {
        self.pending = false;
        self.col = if self.col >= self.left { self.left } else { 0 };
    }

    fn backspace(&mut self) {
        self.pending = false;
        let min = if self.col >= self.left { self.left } else { 0 };
        if self.col > min {
            self.col -= 1;
        }
    }

    fn col_limit(&self) -> usize {
        if self.col <= self.right {
            self.right
        } else {
            self.cols - 1
        }
    }

    fn next_tab(&mut self) {
        self.pending = false;
        let limit = self.col_limit();
        let mut x = self.col + 1;
        while x < limit && !self.tabs[x] {
            x += 1;
        }
        self.col = x.min(limit).max(self.col);
    }

    fn prev_tab(&mut self) {
        self.pending = false;
        let min = if self.col >= self.left { self.left } else { 0 };
        let mut x = self.col;
        while x > min {
            x -= 1;
            if self.tabs[x] {
                break;
            }
        }
        self.col = x.max(min);
    }

    fn goto(&mut self, row: usize, col: usize) {
        let (rmin, rmax, cmin, cmax) = if self.origin {
            (self.top, self.bottom, self.left, self.right)
        } else {
            (0, self.rows - 1, 0, self.cols - 1)
        };
        self.row = (rmin + row).min(rmax);
        self.col = (cmin + col).min(cmax);
        self.pending = false;
    }

    fn home(&mut self) {
        self.goto(0, 0);
    }

    fn move_up(&mut self, n: usize) {
        let min = if self.row >= self.top { self.top } else { 0 };
        self.row = self.row.saturating_sub(n).max(min);
        self.pending = false;
    }

    fn move_down(&mut self, n: usize) {
        let max = if self.row <= self.bottom {
            self.bottom
        } else {
            self.rows - 1
        };
        self.row = (self.row + n).min(max).max(self.row);
        self.pending = false;
    }

    fn move_right(&mut self, n: usize) {
        self.col = (self.col + n).min(self.col_limit());
        self.pending = false;
    }

    fn move_left(&mut self, n: usize) {
        let min = if self.col >= self.left { self.left } else { 0 };
        self.col = self.col.saturating_sub(n).max(min);
        self.pending = false;
    }

    fn set_col_abs(&mut self, col: usize) {
        let (min, max) = if self.origin {
            (self.left, self.right)
        } else {
            (0, self.cols - 1)
        };
        self.col = (min + col).min(max);
        self.pending = false;
    }

    fn set_row_abs(&mut self, row: usize) {
        let (min, max) = if self.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.row = (min + row).min(max);
        self.pending = false;
    }

    // Printing -------------------------------------------------------------------------

    fn print_char(&mut self, c: char) {
        let set = match self.single_shift.take() {
            Some(i) => self.g[i],
            None => self.g[self.gl],
        };
        let c = set.map(c);
        self.put(c);
    }

    /// Prints `c`. A character that continues the cluster in the cell left of the cursor
    /// (see `grapheme.rs`) joins that cell. Otherwise it takes a cell of its own, and a
    /// zero-width character that joins nothing is dropped.
    fn put(&mut self, c: char) {
        self.put_char(c, true);
    }

    /// `put` with a switch for cluster joining. REP turns it off: a repeated character
    /// is a copy of the last base character and never merges with the cell before it,
    /// nor with itself (Hangul jamo, regional indicators).
    fn put_char(&mut self, c: char, join: bool) {
        let w = match c.width() {
            Some(w @ 0..=2) => w,
            _ => return,
        };
        if join && self.extend_cluster(c) {
            return;
        }
        if w == 0 {
            return;
        }
        // REP repeats the last character that started a cell.
        self.last_char = Some(c);
        if w == 2 && self.cols < 2 {
            return;
        }
        if self.pending && self.autowrap {
            let text_cols = self.col_limit() + 1;
            self.wrap(text_cols);
        }
        self.pending = false;
        let mut limit = self.col_limit();
        if self.col + w > limit + 1 {
            if self.autowrap {
                // The wide character moves to the next row; the cells from the cursor
                // on stay empty and are not text.
                let text_cols = self.col;
                self.wrap(text_cols);
                limit = self.col_limit();
            } else {
                self.col = (limit + 1).saturating_sub(w);
            }
        }
        let (row, col, style) = (self.row, self.col, self.style_id);
        if self.insert {
            let erase = self.erase_id;
            self.scr().insert_cells(row, col, w, limit, erase);
        }
        self.scr()
            .put_cell(row, col, Cell::new(c, style, Width::Single), w);
        self.col += w;
        if self.col > limit {
            self.col = limit;
            // With wrap-around off no wrap waits, so turning it on later does not
            // wrap the next character.
            self.pending = self.autowrap;
            self.edge_print = !self.autowrap;
        } else {
            self.edge_print = false;
        }
    }

    /// Joins `c` to the cluster in the cell left of the cursor (or at the cursor when a
    /// wrap waits), if it continues that cluster. Returns true when `c` is consumed:
    /// joined, or dropped because the cluster cannot grow.
    ///
    /// The cluster grows in place. When it becomes wide (VS16, an emoji modifier) it
    /// takes the next column too, shifting the rest of the line when insert mode is on.
    /// wezterm keeps such a cluster one column wide; `COMPAT.md` records
    /// that (`vs16-widens-narrow-base`). At the right margin there is no next column: the text joins
    /// and the cell stays narrow. A VS15 after a wide
    /// cluster makes it narrow, as the settled rules say: the cell becomes single, its
    /// second column is blanked and the cursor moves back onto it. xterm and wezterm keep
    /// such a cluster wide; `COMPAT.md` records that.
    fn extend_cluster(&mut self, c: char) -> bool {
        let tx = if self.pending || self.edge_print {
            self.col
        } else if self.col > 0 {
            self.col - 1
        } else {
            return false;
        };
        let (row, active) = (self.row, self.active);
        let Some(mut prev) = self.screens[active].row(row).map(|r| r.get(tx)) else {
            return false;
        };
        let mut tx = tx;
        if prev.width == Width::Continuation {
            if tx == 0 {
                return false;
            }
            tx -= 1;
            prev = self.screens[active]
                .row(row)
                .map_or(Cell::BLANK, |r| r.get(tx));
        }
        if prev.cluster() == 0 && is_plain(c) && is_plain(prev.ch()) {
            return false;
        }
        let text = match prev.cluster() {
            0 => prev.first(),
            id => self.clusters.get(id),
        };
        // An ASCII character only joins a cluster made of Prepend characters.
        if c.is_ascii()
            && (
                // Lead bytes below 0xD8 encode characters below U+0600.
                text.as_bytes().first().is_none_or(|&b| b < 0xD8) || !text.chars().all(may_prepend)
            )
        {
            return false;
        }
        self.joined.clear();
        self.joined.push_str(text);
        self.joined.push(c);
        if !surely_joins(text, c) && !is_one_cluster(&self.joined) {
            return false;
        }
        let old = if prev.width == Width::Wide { 2 } else { 1 };
        let limit = self.col_limit();
        let cw = cluster_width(&self.joined);
        let shrink = old == 2 && cw == 1;
        let mut w = if shrink { 1 } else { cw.max(old) };
        if w > old && tx + 1 > limit {
            w = old;
        }
        // Collect before interning: the sweep frees ids no cell holds yet, which
        // would include the one handed out below. `prev` still holds its old id.
        self.maybe_collect_clusters();
        let id = match self.clusters.intern(&self.joined) {
            Some(id) => id,
            None => {
                // Full (or the text is too long to keep). Sweep only if cells have
                // been written since the last sweep, so a table full of live ids
                // does not rescan on every character.
                if self.screen_seq().abs_diff(self.swept_seq) >= SWEEP_RETRY_WRITES {
                    self.collect_clusters();
                }
                match self.clusters.intern(&self.joined) {
                    Some(id) => id,
                    None => return true,
                }
            }
        };
        let cell = prev.with_cluster(id);
        if shrink {
            // VS15 makes a wide cluster narrow: free the second column and pull the
            // cursor back onto it.
            let erase = self.erase_id;
            self.scr().erase(row, tx + 1, tx + 2, erase);
            self.col = tx + 1;
            self.pending = false;
            self.edge_print = false;
        } else if w > old {
            if self.insert {
                let erase = self.erase_id;
                self.scr().insert_cells(row, tx + 1, 1, limit, erase);
            }
            self.col = tx + 2;
            if self.col > limit {
                self.col = limit;
                self.pending = self.autowrap;
            } else {
                self.pending = false;
            }
        }
        self.scr().put_cell(row, tx, cell, w);
        true
    }

    /// Row touches on all screens. Moves on every write, erase and scroll.
    fn screen_seq(&self) -> u64 {
        self.screens.iter().map(Screen::seq).sum()
    }

    fn dropped_rows(&self) -> u64 {
        self.screens.iter().map(Screen::dropped).sum()
    }

    /// Amortized collection: sweeps once a quarter of the scrollback has been dropped
    /// since the last sweep and the table has grown well past what was live then.
    fn maybe_collect_clusters(&mut self) {
        if self.clusters.len() < self.live_after_sweep * 2 + 256 {
            return;
        }
        let dropped = self.dropped_rows();
        let quarter = (self.screens[0].scrollback_limit() / 4).max(1) as u64;
        if dropped.abs_diff(self.swept_dropped) >= quarter {
            self.collect_clusters();
        }
    }

    /// Frees the cluster ids no cell refers to.
    fn collect_clusters(&mut self) {
        self.cluster_marks.clear();
        self.cluster_marks.resize(self.clusters.slots(), false);
        for s in &self.screens {
            s.mark_clusters(&mut self.cluster_marks);
        }
        self.clusters.sweep(&self.cluster_marks);
        self.swept_dropped = self.dropped_rows();
        self.swept_seq = self.screen_seq();
        self.live_after_sweep = self.clusters.len();
        self.sweeps += 1;
    }

    /// Wraps to the next line. `text_cols` is how many columns of the row hold text.
    /// Drops the marks of lines that left the primary screen's history.
    fn prune_marks(&mut self) {
        let first = self.screens[0].first_stable();
        self.marks.prune(first);
    }

    /// Amortized collection of styles and links. Runs when a table has more than doubled
    /// since the last collection and either a quarter of the scrollback has been dropped
    /// since then or many rows were written. A table at half its limit is collected
    /// regardless, so SGR alone cannot fill it.
    fn maybe_collect_tables(&mut self) {
        let grown = self.styles.len() >= self.live_styles * 2 + 256
            || self.links.len() >= self.live_links * 2 + 256
            || self.links.bytes() >= self.live_link_bytes * 2 + (1 << 20);
        if !grown {
            return;
        }
        let quarter = (self.screens[0].scrollback_limit() / 4).max(1) as u64;
        if self.dropped_rows().abs_diff(self.tables_dropped) >= quarter
            || self.screen_seq().abs_diff(self.tables_seq) >= TABLE_SWEEP_WRITES
            || self.styles.len() >= crate::style::MAX_STYLES / 2
        {
            self.collect_tables();
        }
    }

    /// Frees the styles no cell uses and the links no live style (or the pen, or a saved
    /// cursor) names, and renumbers what stays. Cell style ids are rewritten in place.
    pub(super) fn collect_tables(&mut self) {
        self.prune_marks();
        self.style_marks.clear();
        self.style_marks.resize(self.styles.len(), false);
        for s in &self.screens {
            s.mark_styles(&mut self.style_marks);
        }
        self.style_marks[0] = true;
        self.link_marks.clear();
        self.link_marks.resize(self.links.len() + 1, false);
        for (i, live) in self.style_marks.iter().enumerate() {
            if *live {
                let l = self.styles.get(StyleId(i as u32)).hyperlink as usize;
                if let Some(m) = self.link_marks.get_mut(l) {
                    *m = true;
                }
            }
        }
        let pen_links = std::iter::once(self.style.hyperlink)
            .chain(self.saved.iter().flatten().map(|s| s.style.hyperlink));
        for l in pen_links {
            if let Some(m) = self.link_marks.get_mut(l as usize) {
                *m = true;
            }
        }
        self.links.compact(&self.link_marks, &mut self.link_map);
        self.styles
            .compact(&self.style_marks, &self.link_map, &mut self.style_map);
        for s in &mut self.screens {
            s.remap_styles(&self.style_map);
        }
        let map = &self.link_map;
        let moved = |l: u32| map.get(l as usize).copied().unwrap_or(0);
        self.style.hyperlink = moved(self.style.hyperlink);
        for s in self.saved.iter_mut().flatten() {
            s.style.hyperlink = moved(s.style.hyperlink);
        }
        self.live_styles = self.styles.len();
        self.live_links = self.links.len();
        self.live_link_bytes = self.links.bytes();
        self.tables_dropped = self.dropped_rows();
        self.tables_seq = self.screen_seq();
        self.table_sweeps += 1;
        // The pen's ids moved. Interning again cannot recurse: the table just shrank.
        self.style_id = self.styles.intern(&self.style);
        let erase = Style {
            fg: self.style.fg,
            bg: self.style.bg,
            ..Style::default()
        };
        self.erase_id = self.styles.intern(&erase);
    }

    fn wrap(&mut self, text_cols: usize) {
        let row = self.row;
        self.scr().set_wrapped(row, text_cols);
        self.col = self.left;
        self.index();
    }

    // Erasing ----------------------------------------------------------------------------

    fn erase_display(&mut self, mode: u16) {
        let (row, col, style, rows, cols) =
            (self.row, self.col, self.erase_id, self.rows, self.cols);
        match mode {
            0 => {
                self.scr().erase(row, col, cols, style);
                for y in row + 1..rows {
                    self.scr().clear_row(y, style);
                }
            }
            1 => {
                for y in 0..row {
                    self.scr().clear_row(y, style);
                }
                self.scr().erase(row, 0, col + 1, style);
            }
            2 => {
                for y in 0..rows {
                    self.scr().clear_row(y, style);
                }
            }
            3 => {
                self.scr().clear_scrollback();
                self.prune_marks();
                self.maybe_collect_tables();
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (row, col, style, cols) = (self.row, self.col, self.erase_id, self.cols);
        match mode {
            0 => self.scr().erase(row, col, cols, style),
            1 => self.scr().erase(row, 0, col + 1, style),
            2 => self.scr().clear_row(row, style),
            _ => {}
        }
    }

    // Cursor save, resets, screens ------------------------------------------------------

    fn save_cursor(&mut self) {
        self.saved[self.active] = Some(Saved {
            row: self.row,
            col: self.col,
            pending: self.pending,
            style: self.style,
            origin: self.origin,
            g: self.g,
            gl: self.gl,
        });
    }

    fn restore_cursor(&mut self) {
        match self.saved[self.active].clone() {
            Some(s) => {
                self.row = s.row.min(self.rows - 1);
                self.col = s.col.min(self.cols - 1);
                self.pending = s.pending;
                // The hyperlink is not part of the saved cursor.
                self.style = Style {
                    hyperlink: self.style.hyperlink,
                    ..s.style
                };
                self.origin = s.origin;
                self.g = s.g;
                self.gl = s.gl;
            }
            None => {
                self.origin = false;
                self.home();
                self.style = Style {
                    hyperlink: self.style.hyperlink,
                    ..Style::default()
                };
                self.g = [Charset::Ascii; 4];
                self.gl = 0;
            }
        }
        self.refresh_style();
    }

    fn switch_screen(&mut self, alt: bool) {
        let to = usize::from(alt);
        if to == self.active {
            return;
        }
        let seq = self.screens[0].seq().max(self.screens[1].seq());
        self.active = to;
        self.screens[to].resync(seq);
        self.pending = false;
    }

    fn clear_active_screen(&mut self) {
        let style = self.erase_id;
        for y in 0..self.rows {
            self.scr().clear_row(y, style);
        }
    }

    fn reset_margins(&mut self) {
        self.top = 0;
        self.bottom = self.rows - 1;
        self.left = 0;
        self.right = self.cols - 1;
    }

    fn soft_reset(&mut self) {
        self.visible = true;
        self.origin = false;
        self.insert = false;
        self.app_cursor = false;
        self.app_keypad = false;
        self.autowrap = true;
        self.lrmm = false;
        self.reset_margins();
        self.style = Style::default();
        self.refresh_style();
        self.g = [Charset::Ascii; 4];
        self.gl = 0;
        self.single_shift = None;
        self.saved = [None, None];
        self.pending = false;
    }

    fn full_reset(&mut self) {
        let size = Size {
            rows: self.rows,
            cols: self.cols,
        };
        let replies = std::mem::take(&mut self.replies);
        let events = std::mem::take(&mut self.events);
        let seq = self.screens[0].seq().max(self.screens[1].seq());
        let next = [self.screens[0].next_stable(), self.screens[1].next_stable()];
        *self = Core::new(size, self.config.clone());
        for (s, n) in self.screens.iter_mut().zip(next) {
            s.continue_from(seq, n);
        }
        self.replies = replies;
        self.events = events;
    }

    fn resize(&mut self, size: Size) {
        let (rows, cols) = (size.rows.max(1), size.cols.max(1));
        if cols != self.cols {
            self.resize_reflow(rows, cols);
        } else {
            // On the alternate screen the primary cursor lives in the saved slot.
            let primary_row = match (self.active, &self.saved[0]) {
                (1, Some(s)) => s.row,
                _ => self.row,
            };
            let shifted = self.screens[0].resize(rows, cols, primary_row, true);
            self.screens[1].resize(rows, cols, self.row, false);
            if self.active == 0 {
                self.row = self.row.saturating_sub(shifted);
            } else if let Some(s) = self.saved[0].as_mut() {
                s.row = s.row.saturating_sub(shifted);
            }
        }
        self.finish_resize(rows, cols);
    }

    /// A column change: the primary screen and its scrollback reflow, with the cursor,
    /// the saved cursor and the semantic marks kept on their characters. The alternate
    /// screen is cut or padded.
    fn resize_reflow(&mut self, rows: usize, cols: usize) {
        use crate::reflow::Point;
        self.prune_marks();
        let old_base = self.screens[0].first_stable();
        let sb = self.screens[0].scrollback_len();
        let old_rows = self.screens[0].rows();
        let old_next = self.screens[0].next_stable();
        let at = |y: usize| sb + y.min(old_rows - 1);
        // Marks first, then the cursor and the saved cursor.
        let mut points: Vec<Point> = self
            .marks
            .iter()
            .map(|m| Point {
                row: (m.row.clamp(old_base, old_next - 1) - old_base) as usize,
                col: m.col,
                pending: false,
            })
            .collect();
        let marks = points.len();
        let cursor = Point {
            row: at(self.row),
            col: self.col,
            pending: self.pending,
        };
        let saved = self.saved[0].as_ref().map(|s| Point {
            row: at(s.row),
            col: s.col,
            pending: s.pending,
        });
        // The primary cursor lives in the saved slot while the alternate screen shows.
        let (anchor, cursor_at, saved_at) = match (self.active, saved) {
            (0, saved) => {
                points.push(cursor);
                let c = points.len() - 1;
                let s = saved.map(|p| {
                    points.push(p);
                    points.len() - 1
                });
                (Some(c), Some(c), s)
            }
            (_, Some(p)) => {
                points.push(p);
                let s = points.len() - 1;
                (Some(s), None, Some(s))
            }
            (_, None) => {
                points.push(cursor);
                (Some(points.len() - 1), None, None)
            }
        };
        let alt_cursor_row = self.row;
        let top = self.screens[0].reflow(rows, cols, &mut points, anchor);
        self.screens[1].resize(rows, cols, alt_cursor_row, false);
        for (m, p) in self.marks.iter_mut().zip(&points[..marks]) {
            m.row = old_base + p.row as u64;
            m.col = p.col;
        }
        let place = |p: &Point| (p.row.saturating_sub(top).min(rows - 1), p.col);
        if let Some(i) = cursor_at {
            let p = points[i];
            (self.row, self.col) = place(&p);
            self.pending = p.pending && self.autowrap;
        }
        if let (Some(i), Some(s)) = (saved_at, self.saved[0].as_mut()) {
            let p = points[i];
            (s.row, s.col) = place(&p);
            s.pending = p.pending && self.autowrap;
        }
        self.edge_print = false;
        self.prune_marks();
    }

    fn finish_resize(&mut self, rows: usize, cols: usize) {
        // The cursor of the primary screen keeps waiting to wrap: it still sits on the
        // character it printed. On the alternate screen the wait ends.
        let keep_pending = self.active == 0;
        self.rows = rows;
        self.cols = cols;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        if !keep_pending {
            self.pending = false;
        }
        self.reset_margins();
        let old = self.tabs.len();
        self.tabs.resize(cols, false);
        for x in old..cols {
            self.tabs[x] = x % 8 == 0;
        }
    }

    // Modes -------------------------------------------------------------------------------

    fn set_ansi_mode(&mut self, mode: u16, on: bool) {
        match mode {
            4 => self.insert = on,
            20 => self.lnm = on,
            _ => {}
        }
    }

    /// DECRQM value of an ANSI mode: 1 set, 2 reset, 0 not recognized.
    fn ansi_mode_report(&self, mode: u16) -> u8 {
        let b = |v: bool| if v { 1 } else { 2 };
        match mode {
            4 => b(self.insert),
            20 => b(self.lnm),
            _ => 0,
        }
    }

    /// DECRQM value of a DEC private mode: 1 set, 2 reset, 4 permanently reset, 0 not
    /// recognized.
    fn dec_mode_report(&self, mode: u16) -> u8 {
        let b = |v: bool| if v { 1 } else { 2 };
        let t = self.mouse.tracking;
        let e = self.mouse.encoding;
        match mode {
            1 => b(self.app_cursor),
            3 => b(self.cols_132),
            5 => b(self.reverse),
            6 => b(self.origin),
            7 => b(self.autowrap),
            9 => b(t == MouseTracking::X10),
            12 => b(self.blink),
            25 => b(self.visible),
            40 => b(self.allow_132),
            47 | 1047 | 1049 => b(self.active == 1),
            66 => b(self.app_keypad),
            69 => b(self.lrmm),
            1000 => b(t == MouseTracking::Normal),
            // Highlight tracking is recognized and never turned on.
            1001 => 4,
            1002 => b(t == MouseTracking::ButtonEvent),
            1003 => b(t == MouseTracking::AnyEvent),
            1004 => b(self.focus_events),
            1005 => b(e == MouseEncoding::Utf8),
            1006 => b(e == MouseEncoding::Sgr),
            1007 => b(self.alt_scroll),
            1015 => b(e == MouseEncoding::Urxvt),
            1016 => b(e == MouseEncoding::SgrPixels),
            1048 => 2,
            2004 => b(self.bracketed),
            2026 => b(self.sync_output),
            _ => 0,
        }
    }

    fn decrqm(&mut self, private: bool, mode: u16) {
        let v = if private {
            self.dec_mode_report(mode)
        } else {
            self.ansi_mode_report(mode)
        };
        let q = if private { "?" } else { "" };
        self.reply_fmt(format_args!("\x1b[{q}{mode};{v}$y"));
    }

    /// XTSAVE: remembers the state of the listed private modes. The mouse tracking level,
    /// the mouse encoding and the alternate screen each have one slot, as in xterm.
    fn save_modes(&mut self, modes: &[u16]) {
        for &m in modes {
            match m {
                9 | 1000 | 1002 | 1003 => self.saved_tracking = Some(self.mouse.tracking),
                1005 | 1006 | 1015 | 1016 => self.saved_encoding = Some(self.mouse.encoding),
                47 | 1047 | 1049 => self.saved_alt = Some(self.active == 1),
                _ => {
                    if let Some(i) = SAVABLE.iter().position(|&s| s == m) {
                        self.saved_modes[i] = self.dec_mode_report(m);
                    }
                }
            }
        }
    }

    /// XTRESTORE: puts the listed private modes back as XTSAVE saw them. Each slot group is
    /// restored once, however many of its modes the list names.
    fn restore_modes(&mut self, modes: &[u16]) {
        let (mut tracking, mut encoding, mut alt) = (false, false, None);
        for &m in modes {
            match m {
                9 | 1000 | 1002 | 1003 => tracking = true,
                1005 | 1006 | 1015 | 1016 => encoding = true,
                47 | 1047 | 1049 => {
                    alt.get_or_insert(m);
                }
                _ => {
                    if let Some(i) = SAVABLE.iter().position(|&s| s == m) {
                        match self.saved_modes[i] {
                            1 => self.set_dec_mode(m, true),
                            2 => self.set_dec_mode(m, false),
                            _ => {}
                        }
                    }
                }
            }
        }
        if tracking && let Some(t) = self.saved_tracking {
            self.mouse.tracking = t;
        }
        if encoding && let Some(e) = self.saved_encoding {
            self.mouse.encoding = e;
        }
        if let (Some(m), Some(on)) = (alt, self.saved_alt) {
            self.set_dec_mode(m, on);
        }
    }

    fn set_cursor_style(&mut self, style: u16) {
        let (shape, blink) = match style {
            0 | 1 => (CursorShape::Block, true),
            2 => (CursorShape::Block, false),
            3 => (CursorShape::Underline, true),
            4 => (CursorShape::Underline, false),
            5 => (CursorShape::Bar, true),
            6 => (CursorShape::Bar, false),
            _ => return,
        };
        self.shape = shape;
        self.blink = blink;
    }

    /// Writes a short reply without allocating.
    fn reply_fmt(&mut self, args: std::fmt::Arguments) {
        let mut buf = [0u8; 48];
        let mut w = &mut buf[..];
        if w.write_fmt(args).is_ok() {
            let left = w.len();
            let n = buf.len() - left;
            self.reply(&buf[..n]);
        }
    }

    fn kitty_set(&mut self, flags: u8, mode: usize) {
        let active = self.active;
        let stack = &mut self.kitty[active];
        let cur = stack.flags();
        let new = match mode {
            1 => flags,
            2 => cur | flags,
            3 => cur & !flags,
            _ => return,
        };
        stack.set(new);
    }

    fn set_dec_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.app_cursor = on,
            3 => {
                // Without mode 40 xterm ignores DECCOLM. With it, xterm clears the
                // screen, resets the margins and homes the cursor; the owner decides
                // whether to resize.
                if self.allow_132 {
                    self.cols_132 = on;
                    self.clear_active_screen();
                    self.reset_margins();
                    self.home();
                }
            }
            5 => self.reverse = on,
            6 => {
                self.origin = on;
                self.home();
            }
            7 => self.autowrap = on,
            9 => self.set_tracking(MouseTracking::X10, on),
            12 => self.blink = on,
            40 => self.allow_132 = on,
            66 => self.app_keypad = on,
            1000 => self.set_tracking(MouseTracking::Normal, on),
            1002 => self.set_tracking(MouseTracking::ButtonEvent, on),
            1003 => self.set_tracking(MouseTracking::AnyEvent, on),
            1004 => self.focus_events = on,
            1005 => self.set_encoding(MouseEncoding::Utf8, on),
            1006 => self.set_encoding(MouseEncoding::Sgr, on),
            1007 => self.alt_scroll = on,
            1015 => self.set_encoding(MouseEncoding::Urxvt, on),
            1016 => self.set_encoding(MouseEncoding::SgrPixels, on),
            1048 => {
                if on {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            2004 => self.bracketed = on,
            2026 => self.sync_output = on,
            25 => self.visible = on,
            47 => self.switch_screen(on),
            69 => {
                self.lrmm = on;
                if !on {
                    self.left = 0;
                    self.right = self.cols - 1;
                }
            }
            1047 => {
                if !on && self.active == 1 {
                    self.clear_active_screen();
                }
                self.switch_screen(on);
            }
            1049 => {
                if on {
                    if self.active == 0 {
                        self.save_cursor();
                        self.switch_screen(true);
                        self.clear_active_screen();
                    }
                } else if self.active == 1 {
                    self.switch_screen(false);
                    self.restore_cursor();
                }
            }
            _ => {}
        }
    }

    // As xterm does: setting a tracking mode replaces the current one, resetting any of
    // them turns tracking off.
    fn set_tracking(&mut self, level: MouseTracking, on: bool) {
        self.mouse.tracking = if on { level } else { MouseTracking::Off };
    }

    fn set_encoding(&mut self, encoding: MouseEncoding, on: bool) {
        if on {
            self.mouse.encoding = encoding;
        } else if self.mouse.encoding == encoding {
            self.mouse.encoding = MouseEncoding::X10;
        }
    }

    // SGR ---------------------------------------------------------------------------------

    fn sgr(&mut self, params: &Params) {
        let mut g: [&[u16]; MAX_PARAMS] = [&[]; MAX_PARAMS];
        let mut n = 0;
        for grp in params.iter() {
            g[n] = grp;
            n += 1;
        }
        // SGR 0 clears attributes, not the hyperlink.
        let link = self.style.hyperlink;
        if n == 0 {
            self.style = Style::default();
        }
        let mut i = 0;
        while i < n {
            let grp = g[i];
            i += 1;
            let code = grp[0];
            let st = &mut self.style;
            match code {
                0 => *st = Style::default(),
                1 => st.set(flags::BOLD, true),
                2 => st.set(flags::FAINT, true),
                3 => st.set(flags::ITALIC, true),
                4 => {
                    st.underline = match grp.get(1) {
                        None => Underline::Single,
                        Some(0) => Underline::None,
                        Some(2) => Underline::Double,
                        Some(3) => Underline::Curly,
                        Some(4) => Underline::Dotted,
                        Some(5) => Underline::Dashed,
                        Some(_) => Underline::Single,
                    }
                }
                5 | 6 => st.set(flags::BLINK, true),
                7 => st.set(flags::INVERSE, true),
                8 => st.set(flags::INVISIBLE, true),
                9 => st.set(flags::STRIKE, true),
                21 => st.underline = Underline::Double,
                22 => {
                    st.set(flags::BOLD, false);
                    st.set(flags::FAINT, false);
                }
                23 => st.set(flags::ITALIC, false),
                24 => st.underline = Underline::None,
                25 => st.set(flags::BLINK, false),
                27 => st.set(flags::INVERSE, false),
                28 => st.set(flags::INVISIBLE, false),
                29 => st.set(flags::STRIKE, false),
                30..=37 => st.fg = Color::Palette((code - 30) as u8),
                39 => st.fg = Color::Default,
                40..=47 => st.bg = Color::Palette((code - 40) as u8),
                49 => st.bg = Color::Default,
                59 => st.underline_color = Color::Default,
                90..=97 => st.fg = Color::Palette((code - 90 + 8) as u8),
                100..=107 => st.bg = Color::Palette((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let color = if grp.len() > 1 {
                        colon_color(grp)
                    } else {
                        let (c, used) = semicolon_color(&g[i..n]);
                        i += used;
                        c
                    };
                    if let Some(c) = color {
                        match code {
                            38 => st.fg = c,
                            48 => st.bg = c,
                            _ => st.underline_color = c,
                        }
                    }
                }
                _ => {}
            }
        }
        self.style.hyperlink = link;
        self.refresh_style();
    }

    // Replies -----------------------------------------------------------------------------

    fn cursor_report(&self) -> (usize, usize) {
        let (r0, c0) = if self.origin {
            (self.top, self.left)
        } else {
            (0, 0)
        };
        (
            self.row.saturating_sub(r0) + 1,
            self.col.saturating_sub(c0) + 1,
        )
    }

    fn dsr(&mut self, private: bool, code: u16) {
        match (private, code) {
            (false, 5) => self.reply(b"\x1b[0n"),
            (false, 6) => {
                let (r, c) = self.cursor_report();
                self.reply_fmt(format_args!("\x1b[{r};{c}R"));
            }
            (true, 6) => {
                let (r, c) = self.cursor_report();
                self.reply_fmt(format_args!("\x1b[?{r};{c};1R"));
            }
            _ => {}
        }
    }
}

fn colon_color(grp: &[u16]) -> Option<Color> {
    let b = |v: u16| v.min(255) as u8;
    match grp[1] {
        5 => grp.get(2).map(|&n| Color::Palette(b(n))),
        2 => match grp.len() {
            6.. => Some(Color::Rgb(b(grp[3]), b(grp[4]), b(grp[5]))),
            5 => Some(Color::Rgb(b(grp[2]), b(grp[3]), b(grp[4]))),
            _ => None,
        },
        _ => None,
    }
}

/// Parses `5;n` or `2;r;g;b` from the groups after a `38`. Returns the color and how many
/// groups it used.
fn semicolon_color(rest: &[&[u16]]) -> (Option<Color>, usize) {
    let b = |v: u16| v.min(255) as u8;
    match rest.first().map(|g| g[0]) {
        Some(5) => match rest.get(1) {
            Some(g) => (Some(Color::Palette(b(g[0]))), 2),
            None => (None, rest.len()),
        },
        Some(2) => {
            if rest.len() >= 4 {
                (
                    Some(Color::Rgb(b(rest[1][0]), b(rest[2][0]), b(rest[3][0]))),
                    4,
                )
            } else {
                (None, rest.len())
            }
        }
        _ => (None, 0),
    }
}

impl Perform for Core {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.edge_print = false;
        match byte {
            0x07 => self.event(Event::Bell),
            // ENQ: the answerback string is empty.
            0x05 => {}
            0x08 => self.backspace(),
            0x09 => self.next_tab(),
            0x0a..=0x0c => {
                self.index();
                if self.lnm {
                    self.carriage_return();
                }
            }
            0x84 => self.index(),
            0x0d => self.carriage_return(),
            0x0e => self.gl = 1,
            0x0f => self.gl = 0,
            0x85 => {
                self.carriage_return();
                self.index();
            }
            0x88 => self.tabs[self.col] = true,
            0x8d => self.reverse_index(),
            0x8e => self.single_shift = Some(2),
            0x8f => self.single_shift = Some(3),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.edge_print = false;
        if ignore {
            return;
        }
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.index(),
            ([], b'E') => {
                self.carriage_return();
                self.index();
            }
            ([], b'H') => self.tabs[self.col] = true,
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.app_keypad = true,
            ([], b'>') => self.app_keypad = false,
            ([], b'N') => self.single_shift = Some(2),
            ([], b'O') => self.single_shift = Some(3),
            ([], b'n') => self.gl = 2,
            ([], b'o') => self.gl = 3,
            ([b'#'], b'8') => {
                self.reset_margins();
                self.home();
                let (rows, cols, style) = (self.rows, self.cols, self.style_id);
                for y in 0..rows {
                    for x in 0..cols {
                        self.scr()
                            .put_cell(y, x, Cell::new('E', style, Width::Single), 1);
                    }
                }
            }
            ([i @ (b'(' | b')' | b'*' | b'+')], f) => {
                let slot = usize::from(*i - b'(');
                self.g[slot] = Charset::from_final(f);
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        private: Option<u8>,
        ignore: bool,
        action: u8,
    ) {
        self.edge_print = false;
        if ignore {
            return;
        }
        let mut a = [0u16; MAX_PARAMS];
        let mut n = 0;
        for grp in params.iter() {
            a[n] = grp[0];
            n += 1;
        }
        let a = &a[..n];
        let one = arg(a, 0, 1);
        match (private, intermediates, action) {
            (None, [], b'm') => self.sgr(params),
            (None, [], b'@') => {
                if self.col >= self.left && self.col <= self.right {
                    let (r, c, right, st) = (self.row, self.col, self.right, self.erase_id);
                    self.scr().insert_cells(r, c, one, right, st);
                    self.pending = false;
                }
            }
            (None, [], b'A') => self.move_up(one),
            (None, [], b'B' | b'e') => self.move_down(one),
            (None, [], b'C') => self.move_right(one),
            (None, [], b'a') => self.move_right(one),
            (None, [], b'D') => self.move_left(one),
            (None, [], b'E') => {
                self.move_down(one);
                self.carriage_return();
            }
            (None, [], b'F') => {
                self.move_up(one);
                self.carriage_return();
            }
            (None, [], b'G' | b'`') => self.set_col_abs(one - 1),
            (None, [], b'H' | b'f') => self.goto(one - 1, arg(a, 1, 1) - 1),
            (None, [], b'I') => (0..one.min(self.cols)).for_each(|_| self.next_tab()),
            (None, [], b'J') | (Some(b'?'), [], b'J') => {
                self.erase_display(a.first().copied().unwrap_or(0))
            }
            (None, [], b'K') | (Some(b'?'), [], b'K') => {
                self.erase_line(a.first().copied().unwrap_or(0))
            }
            (None, [], b'L') => {
                if self.row >= self.top
                    && self.row <= self.bottom
                    && self.col >= self.left
                    && self.col <= self.right
                {
                    let region = Region {
                        top: self.row,
                        ..self.region()
                    };
                    let st = self.erase_id;
                    self.scr().scroll_down(region, one, st);
                    self.col = self.left;
                    self.pending = false;
                }
            }
            (None, [], b'M') => {
                if self.row >= self.top
                    && self.row <= self.bottom
                    && self.col >= self.left
                    && self.col <= self.right
                {
                    let region = Region {
                        top: self.row,
                        ..self.region()
                    };
                    let st = self.erase_id;
                    self.scr().scroll_up(region, one, st, false);
                    self.col = self.left;
                    self.pending = false;
                }
            }
            (None, [], b'P') => {
                if self.col >= self.left && self.col <= self.right {
                    let (r, c, right, st) = (self.row, self.col, self.right, self.erase_id);
                    self.scr().delete_cells(r, c, one, right, st);
                    self.pending = false;
                }
            }
            (None, [], b'S') if n <= 1 => self.scroll_up(one),
            (None, [], b'T') if n <= 1 => self.scroll_down(one),
            (None, [], b'X') => {
                let (r, c, st, cols) = (self.row, self.col, self.erase_id, self.cols);
                self.scr().erase(r, c, (c + one).min(cols), st);
            }
            (None, [], b'Z') => (0..one.min(self.cols)).for_each(|_| self.prev_tab()),
            (None, [], b'b') => {
                if let Some(c) = self.last_char {
                    for _ in 0..one {
                        self.put_char(c, false);
                    }
                }
            }
            (None, [], b'c') if a.first().copied().unwrap_or(0) == 0 => self.reply(b"\x1b[?62;22c"),
            // VT220 class, version 10 (below the 95 that makes programs assume xterm).
            (Some(b'>'), [], b'c') if a.first().copied().unwrap_or(0) == 0 => {
                self.reply(b"\x1b[>1;10;0c")
            }
            // DA3: the unit id, xterm's all zeros.
            (Some(b'='), [], b'c') if a.first().copied().unwrap_or(0) == 0 => {
                self.reply(b"\x1bP!|00000000\x1b\\")
            }
            (Some(b'>'), [], b'q') if a.first().copied().unwrap_or(0) == 0 => self.xtversion(),
            (None, [], b't') => self.winops(a),
            (None, [], b'd') => self.set_row_abs(one - 1),
            (None, [], b'g') => match a.first().copied().unwrap_or(0) {
                0 => self.tabs[self.col] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            (None, [], b'h') => a.iter().for_each(|&m| self.set_ansi_mode(m, true)),
            (None, [], b'l') => a.iter().for_each(|&m| self.set_ansi_mode(m, false)),
            (Some(b'?'), [], b'h') => a.iter().for_each(|&m| self.set_dec_mode(m, true)),
            (Some(b'?'), [], b'l') => a.iter().for_each(|&m| self.set_dec_mode(m, false)),
            (None, [], b'n') => self.dsr(false, a.first().copied().unwrap_or(0)),
            (Some(b'?'), [], b'n') => self.dsr(true, a.first().copied().unwrap_or(0)),
            (None, [], b'r') => {
                let top = arg(a, 0, 1) - 1;
                let bottom = arg(a, 1, self.rows).min(self.rows) - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.home();
                }
            }
            (None, [], b's') => {
                if self.lrmm {
                    let left = arg(a, 0, 1) - 1;
                    let right = arg(a, 1, self.cols).min(self.cols) - 1;
                    if left < right {
                        self.left = left;
                        self.right = right;
                        self.home();
                    }
                } else {
                    self.save_cursor();
                }
            }
            (None, [], b'u') => self.restore_cursor(),
            (None, [b'!'], b'p') => self.soft_reset(),
            (None, [b'$'], b'p') => self.decrqm(false, a.first().copied().unwrap_or(0)),
            (Some(b'?'), [b'$'], b'p') => self.decrqm(true, a.first().copied().unwrap_or(0)),
            (Some(b'?'), [], b's') => self.save_modes(a),
            (Some(b'?'), [], b'r') => self.restore_modes(a),
            (None, [b' '], b'q') => self.set_cursor_style(a.first().copied().unwrap_or(0)),
            (Some(b'?'), [], b'u') => {
                let flags = self.kitty[self.active].flags();
                self.reply_fmt(format_args!("\x1b[?{flags}u"));
            }
            (Some(b'>'), [], b'u') => {
                let flags = (a.first().copied().unwrap_or(0) & 0x1f) as u8;
                let active = self.active;
                self.kitty[active].push(flags);
            }
            (Some(b'<'), [], b'u') => {
                let active = self.active;
                self.kitty[active].pop(one);
            }
            (Some(b'='), [], b'u') => {
                let flags = (a.first().copied().unwrap_or(0) & 0x1f) as u8;
                self.kitty_set(flags, arg(a, 1, 1));
            }
            (Some(b'>'), [], b'm') => match a.first().copied() {
                None => self.modify_other_keys = 0,
                Some(4) => match a.get(1).copied() {
                    None => self.modify_other_keys = 0,
                    Some(v @ 0..=2) => self.modify_other_keys = v as u8,
                    Some(_) => {}
                },
                Some(_) => {}
            },
            (Some(b'?'), [], b'm') if a.first().copied() == Some(4) => {
                let level = self.modify_other_keys;
                self.reply_fmt(format_args!("\x1b[>4;{level}m"));
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, payload: &[u8], end: OscEnd) {
        self.edge_print = false;
        self.osc(payload, end);
    }

    fn hook(
        &mut self,
        _params: &Params,
        intermediates: &[u8],
        private: Option<u8>,
        ignore: bool,
        action: u8,
    ) {
        self.dcs_hook(intermediates, private, ignore, action);
    }

    fn put(&mut self, byte: u8) {
        self.dcs_put(byte);
    }

    fn unhook(&mut self) {
        self.dcs_unhook();
    }
}
