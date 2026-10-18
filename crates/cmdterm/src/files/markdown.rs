//! Markdown for the file panel's rendered mode.
//!
//! The loader thread parses a file once into paragraphs of styled text
//! ([`Markdown`]). [`Markdown::layout`] wraps them to the reader's width on
//! the UI thread, and only when that width changes. The result ([`Laid`]) is
//! one string with line starts and style runs, so drawing a row slices it the
//! way the source view slices a [`super::document::Document`].
//!
//! Math (`$..$`, `$$..$$`, `\(..\)`, `\[..\]` and ```` ```math ```` blocks)
//! becomes Unicode text through [`math`], and ```` ```mermaid ```` blocks
//! become box-drawing diagrams through [`mermaid`].

mod math;
mod mermaid;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use schnellui::widgets::TerminalCellAttrs;
use unicode_width::UnicodeWidthChar;

use super::document::push_line;
use super::highlight::{self, State, Token};
use crate::i18n::Strings;

/// Which reader color a run takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ink {
    #[default]
    Plain,
    Heading,
    Code,
    Link,
    /// List markers, quote bars, rules, table borders, inline HTML.
    Muted,
    /// Code block text, highlighted like the source view.
    Token(Token),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub ink: Ink,
    pub attrs: TerminalCellAttrs,
}

const MUTED: Style = Style {
    ink: Ink::Muted,
    attrs: TerminalCellAttrs {
        bold: false,
        italic: false,
        underline: false,
        strike: false,
        faint: false,
        inverse: false,
    },
};

/// The rule under an H1; thematic breaks and H2 use `─`.
const H1_RULE: char = '━';

/// Text with style runs. Each run ends at a byte offset into `text`.
#[derive(Default)]
struct Styled {
    text: String,
    runs: Vec<(u32, Style)>,
}

impl Styled {
    /// Appends `part` with tabs as spaces and control characters replaced,
    /// so every char covers the cells it says.
    fn push(&mut self, part: &str, style: Style) {
        for ch in part.chars() {
            match ch {
                '\t' => self.text.push(' '),
                '\n' => self.text.push('\n'),
                ch if ch.is_control() => self.text.push('\u{fffd}'),
                ch => self.text.push(ch),
            }
        }
        let end = self.text.len() as u32;
        match self.runs.last_mut() {
            Some((last, shown)) if *shown == style => *last = end,
            _ => self.runs.push((end, style)),
        }
    }

    fn cells(&self) -> usize {
        cells(&self.text)
    }

    /// Drops trailing spaces, such as the one a line break left before
    /// display math.
    fn trim_end(&mut self) {
        let len = self.text.trim_end_matches(' ').len();
        self.text.truncate(len);
        let keep = match self.runs.iter().position(|&(end, _)| end as usize >= len) {
            Some(_) if len == 0 => 0,
            Some(at) => at + 1,
            None => self.runs.len(),
        };
        self.runs.truncate(keep);
        if let Some(last) = self.runs.last_mut() {
            last.0 = len as u32;
        }
    }
}

fn cells(text: &str) -> usize {
    text.chars().map(|ch| ch.width().unwrap_or(0)).sum()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Wraps at spaces to the reader's width.
    Wrap,
    /// One line that never wraps: a table row.
    Fixed,
    /// One line of a code block: never wraps, shaded.
    Code,
    /// The table of this index in [`Markdown::tables`], fitted to the
    /// reader's width when laid out.
    Table(usize),
    /// A rule of this char across the reader.
    Rule(char),
}

struct Para {
    /// Prefix of the first line and of the lines it wraps onto: quote bars,
    /// list indentation and the item's marker.
    first: String,
    rest: String,
    body: Styled,
    kind: Kind,
}

/// A parsed Markdown file, ready to lay out at any width.
pub struct Markdown {
    paras: Vec<Para>,
    tables: Vec<Table>,
}

enum Container {
    Quote,
    /// A list item, indented by its marker's width.
    Item(usize),
}

struct List {
    /// The next number of an ordered list.
    next: Option<u64>,
}

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Styled>>,
    /// Rows from the table head, drawn bold above a border.
    head: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fence {
    Math,
    Mermaid,
}

/// The style of math text.
const MATH: Style = Style {
    ink: Ink::Code,
    attrs: TerminalCellAttrs {
        bold: false,
        italic: false,
        underline: false,
        strike: false,
        faint: false,
        inverse: false,
    },
};

struct Builder {
    paras: Vec<Para>,
    tables: Vec<Table>,
    current: Option<Para>,
    containers: Vec<Container>,
    lists: Vec<List>,
    /// An item's marker, waiting for the item's first line, with the index of
    /// the item in `containers`.
    marker: Option<(usize, String)>,
    styles: Vec<Style>,
    /// A blank line goes before the next block.
    gap: bool,
    /// The language and collected text of the open code or HTML block.
    code: Option<(Option<&'static highlight::Language>, String)>,
    /// The open code block is math or a Mermaid diagram.
    fence: Option<Fence>,
    table: Option<Table>,
    /// For the note over a Mermaid diagram the reader cannot draw.
    tr: &'static Strings,
}

impl Markdown {
    /// [`Self::parse_in`] with English notes, for tests.
    #[cfg(test)]
    pub fn parse(source: &str) -> Self {
        Self::parse_in(source, &crate::i18n::EN)
    }

    /// Parses `source`; notes the reader adds, such as about a diagram it
    /// cannot draw, are in the language of `tr`.
    pub fn parse_in(source: &str, tr: &'static Strings) -> Self {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
            | Options::ENABLE_MATH;
        let source = math::delimiters(source);
        let mut builder = Builder {
            paras: Vec::new(),
            tables: Vec::new(),
            current: None,
            containers: Vec::new(),
            lists: Vec::new(),
            marker: None,
            styles: vec![Style::default()],
            gap: false,
            code: None,
            fence: None,
            table: None,
            tr,
        };
        for event in Parser::new_ext(&source, options) {
            builder.event(event);
        }
        builder.flush();
        Self {
            paras: builder.paras,
            tables: builder.tables,
        }
    }

    /// Wraps the paragraphs to `columns` cells.
    pub fn layout(&self, columns: usize) -> Laid {
        let mut laid = Laid {
            columns,
            text: String::new(),
            starts: Vec::new(),
            runs: Vec::new(),
            line_runs: Vec::new(),
            shaded: Vec::new(),
            width: 0,
        };
        let mut breaks = Vec::new();
        for para in &self.paras {
            match para.kind {
                Kind::Rule(ch) => {
                    let prefix = cells(&para.first);
                    let len = columns.saturating_sub(prefix).max(3);
                    laid.begin(&para.first, false);
                    let mut rule = Styled::default();
                    // The heavy rule under an H1 takes the heading's color.
                    let style = if ch == H1_RULE {
                        Style {
                            ink: Ink::Heading,
                            ..MUTED
                        }
                    } else {
                        MUTED
                    };
                    rule.push(&ch.to_string().repeat(len), style);
                    laid.push(&rule, 0, rule.text.len());
                    laid.end();
                }
                Kind::Table(index) => {
                    lay_table(&self.tables[index], &para.first, columns, &mut laid);
                }
                Kind::Fixed | Kind::Code => {
                    laid.begin(&para.first, para.kind == Kind::Code);
                    laid.push(&para.body, 0, para.body.text.len());
                    laid.end();
                }
                Kind::Wrap => {
                    let avail = columns.saturating_sub(cells(&para.first)).max(8);
                    wrap(&para.body.text, avail, &mut breaks);
                    for (index, &(from, to)) in breaks.iter().enumerate() {
                        let prefix = if index == 0 { &para.first } else { &para.rest };
                        laid.begin(prefix, false);
                        laid.push(&para.body, from, to);
                        laid.end();
                    }
                }
            }
        }
        if laid.starts.is_empty() {
            laid.begin("", false);
            laid.end();
        }
        laid.starts.push(laid.text.len() as u32);
        laid.line_runs.push(laid.runs.len() as u32);
        laid
    }
}

/// Byte ranges of `text` that fit `avail` cells each, broken at spaces and
/// newlines. A word longer than a line breaks where the line ends.
fn wrap(text: &str, avail: usize, out: &mut Vec<(usize, usize)>) {
    out.clear();
    let mut start = 0;
    let mut column = 0;
    let mut space = None;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            out.push((start, index));
            start = index + 1;
            column = 0;
            space = None;
            continue;
        }
        let width = ch.width().unwrap_or(0);
        if column + width > avail && column > 0 {
            if ch == ' ' {
                out.push((start, index));
                start = index + 1;
                column = 0;
                space = None;
                continue;
            }
            match space {
                Some(at) => {
                    out.push((start, at));
                    start = at + 1;
                    column = cells(&text[start..index]);
                }
                None => {
                    out.push((start, index));
                    start = index;
                    column = 0;
                }
            }
            space = None;
        }
        if ch == ' ' {
            space = Some(index);
        }
        column += width;
    }
    out.push((start, text.len()));
}

impl Builder {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, change: impl FnOnce(&mut Style)) {
        let mut style = self.style();
        change(&mut style);
        self.styles.push(style);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    /// The prefix of continuation lines: quote bars and indentation.
    fn prefix(&self) -> String {
        let mut prefix = String::new();
        for container in &self.containers {
            match container {
                Container::Quote => prefix.push_str("│ "),
                Container::Item(width) => prefix.extend(std::iter::repeat_n(' ', *width)),
            }
        }
        prefix
    }

    /// The prefix of a block's first line, with a waiting item marker.
    fn first_prefix(&mut self) -> String {
        let Some((at, marker)) = self.marker.take() else {
            return self.prefix();
        };
        let mut prefix = String::new();
        for (index, container) in self.containers.iter().enumerate() {
            match container {
                Container::Quote => prefix.push_str("│ "),
                Container::Item(_) if index == at => prefix.push_str(&marker),
                Container::Item(width) => prefix.extend(std::iter::repeat_n(' ', *width)),
            }
        }
        prefix
    }

    fn flush(&mut self) {
        if let Some(para) = self.current.take() {
            self.paras.push(para);
        }
    }

    /// Starts a block of `kind`, after a blank line when one is due.
    fn begin(&mut self, kind: Kind) {
        self.flush();
        self.gap_line();
        let first = self.first_prefix();
        self.current = Some(Para {
            first,
            rest: self.prefix(),
            body: Styled::default(),
            kind,
        });
    }

    /// The blank line between two blocks, when one is due.
    fn gap_line(&mut self) {
        if self.gap && !self.paras.is_empty() {
            let prefix = self.prefix();
            let bars = prefix.trim_end().to_owned();
            self.paras.push(Para {
                first: bars.clone(),
                rest: bars,
                body: Styled::default(),
                kind: Kind::Fixed,
            });
        }
        self.gap = false;
    }

    /// Appends inline text to the open table cell or paragraph.
    fn text(&mut self, text: &str, style: Style) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.rows.last_mut().and_then(|row| row.last_mut()) {
                cell.push(text, style);
            }
            return;
        }
        if self.current.is_none() {
            self.begin(Kind::Wrap);
        }
        if let Some(para) = &mut self.current {
            para.body.push(text, style);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some((_, code)) = &mut self.code {
            match event {
                Event::Text(text) | Event::Html(text) => {
                    code.push_str(&text);
                    return;
                }
                Event::End(TagEnd::CodeBlock | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_)) => {
                    self.end_code();
                    return;
                }
                _ => {}
            }
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, self.style()),
            Event::Code(text) => {
                let mut style = self.style();
                style.ink = Ink::Code;
                self.text(&text, style);
            }
            Event::InlineMath(text) => {
                let mut style = self.style();
                style.ink = Ink::Code;
                self.text(&math::inline(&text), style);
            }
            Event::DisplayMath(text) => {
                if self.table.is_some() {
                    self.text(&math::inline(&text), MATH);
                } else {
                    self.display(&math::display(&text), MATH);
                }
            }
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text, MUTED),
            Event::FootnoteReference(label) => {
                self.text("[", MUTED);
                self.text(&label, MUTED);
                self.text("]", MUTED);
            }
            // Display math ends the paragraph's line it sat in, so a break
            // right after it has nothing to separate.
            Event::SoftBreak | Event::HardBreak
                if self.current.is_none() && self.table.is_none() => {}
            Event::SoftBreak => self.text(" ", self.style()),
            Event::HardBreak => self.text("\n", self.style()),
            Event::Rule => {
                self.begin(Kind::Rule('─'));
                self.flush();
                self.gap = true;
            }
            Event::TaskListMarker(done) => self.text(if done { "☑ " } else { "☐ " }, MUTED),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin(Kind::Wrap),
            Tag::Heading { level, .. } => {
                self.begin(Kind::Wrap);
                // H1 and H3 and below take the heading color, H2 the ink:
                // with their rules (heavy and colored under H1, thin and
                // muted under H2) the three levels read apart.
                let ink = if level == HeadingLevel::H2 {
                    Ink::Plain
                } else {
                    Ink::Heading
                };
                self.push_style(|style| {
                    style.ink = ink;
                    style.attrs.bold = true;
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.gap_line();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                let name = match &kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                        .next()
                        .unwrap_or_default(),
                    CodeBlockKind::Indented => "",
                };
                self.fence = match name {
                    "math" => Some(Fence::Math),
                    "mermaid" => Some(Fence::Mermaid),
                    _ => None,
                };
                let language = highlight::by_name(name);
                self.flush();
                self.code = Some((language, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.code = Some((Some(&highlight::HTML), String::new()));
            }
            Tag::MetadataBlock(_) => {
                self.flush();
                self.code = Some((Some(&highlight::YAML), String::new()));
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    // A top-level list sits apart from the paragraph above it.
                    self.gap |= !self.paras.is_empty();
                }
                self.lists.push(List { next: start });
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut().and_then(|list| list.next.as_mut()) {
                    Some(next) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    None => format!("{} ", ["•", "◦", "▪"][depth % 3]),
                };
                let width = cells(&marker);
                self.containers.push(Container::Item(width));
                self.marker = Some((self.containers.len() - 1, marker));
            }
            Tag::Table(aligns) => {
                self.begin(Kind::Fixed);
                self.current = None;
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    head: 0,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|table| table.rows.last_mut()) {
                    row.push(Styled::default());
                }
            }
            Tag::Emphasis => self.push_style(|style| style.attrs.italic = true),
            Tag::Strong => self.push_style(|style| style.attrs.bold = true),
            Tag::Strikethrough => self.push_style(|style| style.attrs.strike = true),
            Tag::Link { .. } => self.push_style(|style| {
                style.ink = Ink::Link;
                style.attrs.underline = true;
            }),
            Tag::Image { .. } => {
                self.text("▣ ", MUTED);
                self.push_style(|style| {
                    style.ink = Ink::Muted;
                    style.attrs.italic = true;
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.begin(Kind::Wrap);
                self.text("[", MUTED);
                self.text(&label, MUTED);
                self.text("]: ", MUTED);
            }
            _ => self.push_style(|_| {}),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.gap = true;
            }
            TagEnd::Heading(level) => {
                self.pop_style();
                self.flush();
                match level {
                    HeadingLevel::H1 => self.rule(H1_RULE),
                    HeadingLevel::H2 => self.rule('─'),
                    _ => {}
                }
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.gap = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
                self.marker = None;
            }
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head = table.rows.len();
                }
            }
            TagEnd::TableRow | TagEnd::TableCell => {}
            TagEnd::FootnoteDefinition => {
                self.flush();
                self.gap = true;
            }
            TagEnd::CodeBlock | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => self.end_code(),
            _ => self.pop_style(),
        }
    }

    /// A rule under the paragraph just flushed, as wide as the reader.
    fn rule(&mut self, ch: char) {
        let prefix = self.prefix();
        self.paras.push(Para {
            first: prefix.clone(),
            rest: prefix,
            body: Styled::default(),
            kind: Kind::Rule(ch),
        });
    }

    /// Rows of display math or a diagram, indented, each on its own line
    /// that never wraps. Text of the paragraph before them stays a
    /// paragraph of its own; an empty one gives its prefix, such as a list
    /// marker, to the first row.
    fn display(&mut self, rows: &[String], style: Style) {
        let (first, rest) = match self.current.take() {
            Some(para) if para.body.text.trim().is_empty() => (para.first, para.rest),
            Some(mut para) => {
                para.body.trim_end();
                self.paras.push(para);
                (self.prefix(), self.prefix())
            }
            None => {
                self.gap_line();
                (self.first_prefix(), self.prefix())
            }
        };
        for (index, row) in rows.iter().enumerate() {
            let mut body = Styled::default();
            body.push("  ", style);
            body.push(row, style);
            self.paras.push(Para {
                first: if index == 0 {
                    first.clone()
                } else {
                    rest.clone()
                },
                rest: rest.clone(),
                body,
                kind: Kind::Fixed,
            });
        }
    }

    /// A Mermaid drawing as shaded lines: lines and arrows muted, text plain.
    fn diagram(&mut self, rows: &[String]) {
        let plain = Style {
            ink: Ink::Token(Token::Plain),
            ..Style::default()
        };
        let mut part = String::new();
        for row in rows {
            self.begin(Kind::Code);
            let Some(mut para) = self.current.take() else {
                continue;
            };
            para.first.push(' ');
            let mut muted = false;
            part.clear();
            for ch in row.chars() {
                let line = matches!(
                    ch,
                    '\u{2500}'..='\u{257f}' | '▲' | '▼' | '◀' | '▶' | '◁' | '▷'
                );
                if line != muted && !part.is_empty() {
                    para.body.push(&part, if muted { MUTED } else { plain });
                    part.clear();
                }
                muted = line;
                part.push(ch);
            }
            para.body.push(&part, if muted { MUTED } else { plain });
            self.paras.push(para);
        }
    }

    fn end_code(&mut self) {
        let Some((mut language, code)) = self.code.take() else {
            return;
        };
        let code = code.strip_suffix('\n').unwrap_or(&code);
        match self.fence.take() {
            Some(Fence::Math) => {
                self.flush();
                self.display(&math::display(code), MATH);
                self.gap = true;
                return;
            }
            Some(Fence::Mermaid) => match mermaid::draw(code) {
                Ok(rows) => {
                    self.diagram(&rows);
                    self.gap = true;
                    return;
                }
                Err(reason) => {
                    self.begin(Kind::Wrap);
                    let style = Style {
                        attrs: TerminalCellAttrs {
                            italic: true,
                            ..TerminalCellAttrs::default()
                        },
                        ..MUTED
                    };
                    self.text(&(self.tr.files_mermaid_unsupported)(&reason), style);
                    self.flush();
                    language = None;
                }
            },
            None => {}
        }
        let mut state = State::Normal;
        let mut line = String::new();
        for raw in code.split('\n') {
            self.begin(Kind::Code);
            let Some(mut para) = self.current.take() else {
                continue;
            };
            para.first.push(' ');
            line.clear();
            push_line(&mut line, raw);
            match language {
                Some(language) if line.len() <= super::document::MAX_HIGHLIGHT => {
                    highlight::highlight(language, &mut state, &line, |from, to, token| {
                        para.body.push(
                            &line[from..to],
                            Style {
                                ink: Ink::Token(token),
                                attrs: TerminalCellAttrs {
                                    italic: token == Token::Comment,
                                    bold: token == Token::Heading,
                                    ..TerminalCellAttrs::default()
                                },
                            },
                        );
                    });
                }
                _ => para.body.push(
                    &line,
                    Style {
                        ink: Ink::Token(Token::Plain),
                        ..Style::default()
                    },
                ),
            }
            self.paras.push(para);
        }
        self.gap = true;
    }

    fn end_table(&mut self) {
        let Some(mut table) = self.table.take() else {
            return;
        };
        // The head's cells are bold, as its row is drawn.
        let head = table.head.min(table.rows.len());
        for row in &mut table.rows[..head] {
            for cell in row {
                for (_, style) in &mut cell.runs {
                    style.attrs.bold = true;
                }
            }
        }
        let prefix = self.prefix();
        self.paras.push(Para {
            first: prefix.clone(),
            rest: prefix,
            body: Styled::default(),
            kind: Kind::Table(self.tables.len()),
        });
        self.tables.push(table);
        self.gap = true;
    }
}

/// Lays `table` out in `columns` cells after `prefix`. Columns keep their
/// natural widths when they fit; else the widest give way first, down to a
/// fair share, and their cells wrap onto more lines of the row, so no
/// text is cut at the reader's edge.
fn lay_table(table: &Table, prefix: &str, columns: usize, laid: &mut Laid) {
    const SEPARATOR: usize = 3;
    const NARROWEST: usize = 4;
    let count = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if count == 0 {
        return;
    }
    let mut natural = vec![0; count];
    for row in &table.rows {
        for (width, cell) in natural.iter_mut().zip(row) {
            *width = (*width).max(cell.cells());
        }
    }
    let avail = columns
        .saturating_sub(cells(prefix))
        .saturating_sub(SEPARATOR * (count - 1));
    let widths: Vec<usize> = if natural.iter().sum::<usize>() <= avail {
        natural
    } else {
        // The largest cap on every column that still fits.
        let fits = |cap: usize| natural.iter().map(|&w| w.min(cap)).sum::<usize>() <= avail;
        let (mut low, mut high) = (NARROWEST, natural.iter().copied().max().unwrap_or(0));
        while low < high {
            let mid = (low + high).div_ceil(2);
            if fits(mid) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        natural.iter().map(|&w| w.min(low).max(1)).collect()
    };
    let mut separator = Styled::default();
    separator.push(" │ ", MUTED);
    let mut spaces = Styled::default();
    spaces.push(
        &" ".repeat(widths.iter().copied().max().unwrap_or(0)),
        Style::default(),
    );
    let mut wrapped: Vec<Vec<(usize, usize)>> = vec![Vec::new(); count];
    let mut breaks = Vec::new();
    for (index, row) in table.rows.iter().enumerate() {
        let mut height = 1;
        for (column, &width) in widths.iter().enumerate() {
            wrapped[column].clear();
            if let Some(cell) = row.get(column) {
                wrap(&cell.text, width, &mut breaks);
                wrapped[column].extend_from_slice(&breaks);
            }
            height = height.max(wrapped[column].len());
        }
        for line in 0..height {
            laid.begin(prefix, false);
            for (column, &width) in widths.iter().enumerate() {
                if column > 0 {
                    laid.push(&separator, 0, separator.text.len());
                }
                let part = row
                    .get(column)
                    .zip(wrapped[column].get(line))
                    .map(|(cell, &(from, to))| (cell, from, to));
                let used = part.map_or(0, |(cell, from, to)| cells(&cell.text[from..to]));
                let pad = width.saturating_sub(used);
                let (before, after) = match table.aligns.get(column) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                laid.push(&spaces, 0, before);
                if let Some((cell, from, to)) = part {
                    laid.push(cell, from, to);
                }
                laid.push(&spaces, 0, after);
            }
            laid.end();
        }
        if index + 1 == table.head {
            let mut border = Styled::default();
            for (column, &width) in widths.iter().enumerate() {
                if column > 0 {
                    border.push("─┼─", MUTED);
                }
                border.push(&"─".repeat(width), MUTED);
            }
            laid.begin(prefix, false);
            laid.push(&border, 0, border.text.len());
            laid.end();
        }
    }
}

/// Markdown wrapped to one width: the lines the reader draws.
pub struct Laid {
    /// The width it was laid out for.
    pub columns: usize,
    text: String,
    /// Byte offset where each line starts, then one past the end.
    starts: Vec<u32>,
    /// Style runs, each ending at a byte offset into `text`.
    runs: Vec<(u32, Style)>,
    /// Index of each line's first run, then the number of runs.
    line_runs: Vec<u32>,
    /// Lines of code blocks, drawn on a shaded background.
    shaded: Vec<bool>,
    /// Cells of the widest line.
    pub width: usize,
}

impl Laid {
    fn begin(&mut self, prefix: &str, shaded: bool) {
        self.starts.push(self.text.len() as u32);
        self.line_runs.push(self.runs.len() as u32);
        self.shaded.push(shaded);
        if !prefix.is_empty() {
            self.text.push_str(prefix);
            self.runs.push((self.text.len() as u32, MUTED));
        }
    }

    /// Appends bytes `from..to` of `styled` to the current line.
    fn push(&mut self, styled: &Styled, from: usize, to: usize) {
        let mut start = 0;
        for &(end, style) in &styled.runs {
            let end = end as usize;
            let (a, b) = (start.max(from), end.min(to));
            start = end;
            if a >= b {
                continue;
            }
            self.text.push_str(&styled.text[a..b]);
            let at = self.text.len() as u32;
            let first = *self.line_runs.last().unwrap_or(&0) as usize;
            let len = self.runs.len();
            match self.runs.last_mut() {
                Some((last, shown)) if len > first && *shown == style => *last = at,
                _ => self.runs.push((at, style)),
            }
        }
    }

    fn end(&mut self) {
        let start = *self.starts.last().unwrap_or(&0) as usize;
        self.width = self.width.max(cells(&self.text[start..]));
    }

    pub fn lines(&self) -> usize {
        self.starts.len() - 1
    }

    /// Line `line`, with its prefix.
    pub fn line(&self, line: usize) -> &str {
        &self.text[self.starts[line] as usize..self.starts[line + 1] as usize]
    }

    pub fn shaded(&self, line: usize) -> bool {
        self.shaded[line]
    }

    /// The styled parts of line `line`, in order.
    pub fn runs(&self, line: usize) -> impl Iterator<Item = (&str, Style)> {
        let runs = &self.runs[self.line_runs[line] as usize..self.line_runs[line + 1] as usize];
        let mut from = self.starts[line] as usize;
        runs.iter().map(move |&(end, style)| {
            let part = &self.text[from..end as usize];
            from = end as usize;
            (part, style)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(source: &str, columns: usize) -> Vec<String> {
        let laid = Markdown::parse(source).layout(columns);
        (0..laid.lines())
            .map(|line| laid.line(line).to_owned())
            .collect()
    }

    #[test]
    fn paragraphs_wrap_at_spaces() {
        assert_eq!(
            lines("one two three four five\nsix", 10),
            ["one two", "three four", "five six"]
        );
        // A word longer than the line breaks where the line ends.
        assert_eq!(lines("abcdefghijkl", 8), ["abcdefgh", "ijkl"]);
        assert_eq!(lines("a  \nb", 20), ["a", "b"]);
    }

    #[test]
    fn headings_lists_quotes_and_rules() {
        let text = "# Title\n\nSome *text*.\n\n- one\n- two\n  - nested\n\n> quoted\n> line\n\n---\n\n1. first\n2. second\n";
        assert_eq!(
            lines(text, 12),
            [
                "Title",
                "━━━━━━━━━━━━",
                "",
                "Some text.",
                "",
                "• one",
                "• two",
                "  ◦ nested",
                "",
                "│ quoted",
                "│ line",
                "",
                "────────────",
                "",
                "1. first",
                "2. second",
            ]
        );
        let laid = Markdown::parse("# Title\n\nSome *text*.").layout(12);
        let heading: Vec<_> = laid.runs(0).collect();
        assert_eq!(heading[0].0, "Title");
        assert_eq!(heading[0].1.ink, Ink::Heading);
        assert!(heading[0].1.attrs.bold);
        let runs: Vec<_> = laid.runs(3).collect();
        assert_eq!(runs[1].0, "text");
        assert!(runs[1].1.attrs.italic);
    }

    #[test]
    fn list_items_wrap_under_their_text() {
        assert_eq!(
            lines("- alpha beta gamma delta", 14),
            ["• alpha beta", "  gamma delta"]
        );
    }

    #[test]
    fn code_blocks_keep_lines_and_highlight() {
        let laid = Markdown::parse("```rust\nfn main() {\n\tlet x = 1;\n}\n```\n").layout(10);
        assert_eq!(laid.lines(), 3);
        assert_eq!(laid.line(0), " fn main() {");
        assert_eq!(laid.line(1), "     let x = 1;");
        assert!(laid.shaded(0));
        assert_eq!(laid.width, 15);
        let runs: Vec<_> = laid.runs(0).collect();
        assert_eq!(
            runs[1],
            (
                "fn",
                Style {
                    ink: Ink::Token(Token::Keyword),
                    ..Style::default()
                }
            )
        );
    }

    #[test]
    fn tables_align_their_columns() {
        let text = "| a | long name |\n|--:|:--|\n| 100 | x |\n";
        assert_eq!(
            lines(text, 40),
            ["  a │ long name", "────┼──────────", "100 │ x        "]
        );
    }

    #[test]
    fn a_table_wider_than_the_reader_wraps_its_widest_column() {
        let text = "| key | what it does |\n|--|--|\n| a | one two three four five |\n";
        assert_eq!(
            lines(text, 16),
            [
                "key │ what it   ",
                "    │ does      ",
                "────┼───────────",
                "a   │ one two   ",
                "    │ three four",
                "    │ five      ",
            ]
        );
    }

    #[test]
    fn task_lists_and_images() {
        assert_eq!(
            lines("- [x] done\n- [ ] open\n\n![logo](a.png)", 30),
            ["• ☑ done", "• ☐ open", "", "▣ logo"]
        );
    }

    #[test]
    fn math_renders_inline_and_on_its_own_lines() {
        assert_eq!(
            lines("Energy $E = mc^2$ and \\(\\alpha\\).", 40),
            ["Energy E = mc² and α."]
        );
        let laid = Markdown::parse("so $x_1$ holds").layout(40);
        let runs: Vec<_> = laid.runs(0).collect();
        assert_eq!(runs[1], ("x₁", MATH));
        // Prices are not math.
        assert_eq!(lines("It costs $5 and $10.", 40), ["It costs $5 and $10."]);
        assert_eq!(lines("Code `$x$` stays.", 40), ["Code $x$ stays."]);
        assert_eq!(
            lines("Before\n$$\n\\frac{a}{b}\n$$\nafter", 40),
            ["Before", "   a", "  ───", "   b", "after"]
        );
        assert_eq!(
            lines("```math\n\\sum_{i=1}^n i\n```\n\n- $$x^2$$\n", 40),
            ["   n", "   ∑  i", "  i=1", "", "•   x²"]
        );
    }

    #[test]
    fn mermaid_blocks_draw_or_show_their_source() {
        let laid = Markdown::parse("```mermaid\ngraph LR\n  A --> B\n```\n").layout(40);
        let text: Vec<_> = (0..laid.lines()).map(|line| laid.line(line)).collect();
        assert!(
            text.iter()
                .any(|line| line.contains("│ A ├") && line.contains("▶│ B │")),
            "{text:#?}"
        );
        assert!(laid.shaded(0));
        let runs: Vec<_> = laid.runs(1).collect();
        assert!(
            runs.iter()
                .any(|(part, style)| part.contains('A') && style.ink == Ink::Token(Token::Plain))
        );
        assert!(runs.iter().any(|(_, style)| *style == MUTED));
        assert_eq!(
            lines("```mermaid\npie\n  \"a\" : 1\n```\n", 80),
            [
                "Mermaid diagram the viewer cannot draw: pie diagrams are not supported",
                " pie",
                "   \"a\" : 1",
            ]
        );
    }
}
