//! LaTeX math as Unicode text for the Markdown reader.
//!
//! [`inline`] turns an expression into one line: Greek letters and symbols
//! become their characters, and scripts become superscript and subscript
//! characters when every character has one. [`display`] lays an expression
//! out over several rows, with fractions stacked over a bar and limits above
//! and below big operators. Commands it does not know stay as written.
//!
//! Parsing runs once on the loader thread, so this allocates freely.

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

enum Node {
    Text(String),
    Row(Vec<Node>),
    Frac(Box<Node>, Box<Node>),
    /// The root's index, if any, and the radicand.
    Sqrt(Option<Box<Node>>, Box<Node>),
    Scripts {
        base: Box<Node>,
        sup: Option<Box<Node>>,
        sub: Option<Box<Node>>,
    },
    /// `\\`: the next row of display math.
    Break,
}

/// The expression `tex` on one line.
pub fn inline(tex: &str) -> String {
    let nodes = Parser::new(tex).row(None);
    let mut out = String::new();
    for node in &nodes {
        flat(node, &mut out);
    }
    out.trim().to_owned()
}

/// The expression `tex` laid out over rows, one block per `\\` line.
pub fn display(tex: &str) -> Vec<String> {
    let nodes = Parser::new(tex).row(None);
    let mut rows = Vec::new();
    let mut line = Vec::new();
    for node in nodes.into_iter().chain(std::iter::once(Node::Break)) {
        match node {
            Node::Break => {
                let block = layout(&Node::Row(std::mem::take(&mut line)));
                rows.extend(
                    block
                        .rows
                        .into_iter()
                        .map(|row| row.trim_end().to_owned())
                        .filter(|row| !row.is_empty()),
                );
            }
            Node::Text(text) if text == " " && line.is_empty() => {}
            node => line.push(node),
        }
    }
    rows
}

/// `source` with `\(..\)` turned into `$..$` and `\[..\]` into `$$..$$`,
/// outside code, so the Markdown parser reads them as math. `\[` counts
/// only at the start of a line, since `\[` elsewhere usually escapes a
/// bracket.
pub fn delimiters(source: &str) -> Cow<'_, str> {
    if !source.contains("\\(") && !source.contains("\\[") {
        return Cow::Borrowed(source);
    }
    let mut out = String::with_capacity(source.len() + 16);
    let mut fence: Option<(char, usize)> = None;
    let mut display = false;
    for line in source.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let trimmed = body.trim_start();
        let indent = &body[..body.len() - trimmed.len()];
        if let Some((ch, len)) = fence {
            let run = trimmed.chars().take_while(|&c| c == ch).count();
            if run >= len && trimmed[run * ch.len_utf8()..].trim().is_empty() {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if let Some(ch) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) {
            let run = trimmed.chars().take_while(|&c| c == ch).count();
            if run >= 3 {
                fence = Some((ch, run));
                out.push_str(line);
                continue;
            }
        }
        if display {
            match trimmed.strip_suffix("\\]") {
                Some(rest) => {
                    out.push_str(indent);
                    out.push_str(rest);
                    out.push_str("$$");
                    display = false;
                }
                None => out.push_str(body),
            }
            out.push_str(ending);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("\\[") {
            out.push_str(indent);
            out.push_str("$$");
            match rest.strip_suffix("\\]") {
                Some(inner) => {
                    out.push_str(inner);
                    out.push_str("$$");
                }
                None => {
                    out.push_str(rest);
                    display = true;
                }
            }
            out.push_str(ending);
            continue;
        }
        inline_delimiters(body, &mut out);
        out.push_str(ending);
    }
    Cow::Owned(out)
}

/// Copies `line` to `out` with `\(..\)` pairs outside code spans as `$..$`.
fn inline_delimiters(line: &str, out: &mut String) {
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'`' {
            let run = bytes[at..].iter().take_while(|&&b| b == b'`').count();
            let ticks = &line[at..at + run];
            let close = line[at + run..]
                .match_indices(ticks)
                .find(|(i, _)| {
                    let end = at + run + i + run;
                    bytes.get(end) != Some(&b'`') && bytes[at + run + i - 1] != b'`'
                })
                .map(|(i, _)| at + run + i + run);
            let end = close.unwrap_or(at + run);
            out.push_str(&line[at..end]);
            at = end;
            continue;
        }
        let escaped = at > 0 && bytes[at - 1] == b'\\';
        if bytes[at] == b'\\'
            && bytes.get(at + 1) == Some(&b'(')
            && !escaped
            && let Some(len) = line[at + 2..].find("\\)")
        {
            let inner = line[at + 2..at + 2 + len].trim();
            if !inner.is_empty() {
                out.push('$');
                out.push_str(inner);
                out.push('$');
                at += 2 + len + 2;
                continue;
            }
        }
        let ch = line[at..].chars().next().unwrap_or(' ');
        out.push(ch);
        at += ch.len_utf8();
    }
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn new(tex: &str) -> Self {
        Self {
            chars: tex.chars().collect(),
            at: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    /// Nodes up to `close` (consumed) or the end.
    fn row(&mut self, close: Option<char>) -> Vec<Node> {
        let mut nodes: Vec<Node> = Vec::new();
        while let Some(ch) = self.peek() {
            if Some(ch) == close {
                self.at += 1;
                break;
            }
            match ch {
                '}' => self.at += 1,
                '^' | '_' => {
                    self.at += 1;
                    while matches!(nodes.last(), Some(Node::Text(text)) if text == " ") {
                        nodes.pop();
                    }
                    let arg = self.atom();
                    attach(&mut nodes, ch == '^', arg);
                }
                '\'' => {
                    self.at += 1;
                    nodes.push(Node::Text("′".into()));
                }
                ch if ch.is_whitespace() => {
                    self.skip_space();
                    let after_open = matches!(nodes.last(),
                        Some(Node::Text(text)) if text.ends_with(['(', '[', '{', '⟨', '|']));
                    if !nodes.is_empty() && !after_open {
                        nodes.push(Node::Text(" ".into()));
                    }
                }
                _ => {
                    let item = self.item();
                    if matches!(&item, Node::Text(text) if text.starts_with([')', ']', '}', '⟩', ',']))
                    {
                        while matches!(nodes.last(), Some(Node::Text(text)) if text == " ") {
                            nodes.pop();
                        }
                    }
                    nodes.push(item);
                }
            }
        }
        while matches!(nodes.last(), Some(Node::Text(text)) if text == " ") {
            nodes.pop();
        }
        nodes
    }

    fn item(&mut self) -> Node {
        let Some(ch) = self.peek() else {
            return Node::Text(String::new());
        };
        match ch {
            '{' => {
                self.at += 1;
                Node::Row(self.row(Some('}')))
            }
            '\\' => self.command(),
            _ => {
                self.at += 1;
                Node::Text(
                    match ch {
                        '-' => '−',
                        '*' => '∗',
                        '~' | '&' => ' ',
                        ch => ch,
                    }
                    .to_string(),
                )
            }
        }
    }

    /// One argument: a group, a command or a char.
    fn atom(&mut self) -> Node {
        self.skip_space();
        self.item()
    }

    /// A braced argument as raw text, spaces kept.
    fn raw(&mut self) -> String {
        self.skip_space();
        if self.peek() != Some('{') {
            return match self.peek() {
                Some(ch) => {
                    self.at += 1;
                    ch.to_string()
                }
                None => String::new(),
            };
        }
        self.at += 1;
        let mut depth = 1;
        let mut text = String::new();
        while let Some(ch) = self.peek() {
            self.at += 1;
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            text.push(ch);
        }
        text
    }

    fn command(&mut self) -> Node {
        self.at += 1;
        let Some(first) = self.peek() else {
            return Node::Text("\\".into());
        };
        let name: String = if first.is_ascii_alphabetic() {
            let start = self.at;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.at += 1;
            }
            self.chars[start..self.at].iter().collect()
        } else {
            self.at += 1;
            first.to_string()
        };
        let text = |s: &str| Node::Text(s.to_owned());
        match name.as_str() {
            "\\" => Node::Break,
            "," | ":" | ";" | ">" | " " => text(" "),
            "!" => text(""),
            "quad" => text("  "),
            "qquad" => text("    "),
            "{" | "}" | "%" | "$" | "#" | "&" | "_" => Node::Text(name),
            "|" => text("‖"),
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let top = self.atom();
                let bottom = self.atom();
                Node::Frac(Box::new(top), Box::new(bottom))
            }
            "binom" | "dbinom" | "tbinom" => {
                let n = self.atom();
                let k = self.atom();
                Node::Row(vec![text("("), n, text(" choose "), k, text(")")])
            }
            "sqrt" => {
                self.skip_space();
                let index = (self.peek() == Some('[')).then(|| {
                    self.at += 1;
                    Box::new(Node::Row(self.row(Some(']'))))
                });
                Node::Sqrt(index, Box::new(self.atom()))
            }
            "text" | "textrm" | "textit" | "textbf" | "texttt" | "textsf" | "textnormal"
            | "mbox" | "hbox" | "operatorname" | "mathrm" | "mathit" | "mathsf" | "mathtt"
            | "mathnormal" => Node::Text(self.raw()),
            "mathbb" | "Bbb" | "mathcal" | "mathscr" | "mathfrak" | "mathbf" | "boldsymbol"
            | "bm" => {
                let font = match name.as_str() {
                    "mathbb" | "Bbb" => Font::DoubleStruck,
                    "mathcal" | "mathscr" => Font::Script,
                    "mathfrak" => Font::Fraktur,
                    _ => Font::Bold,
                };
                let mut plain = String::new();
                flat(&self.atom(), &mut plain);
                Node::Text(plain.chars().map(|ch| font.apply(ch)).collect())
            }
            "left" | "right" | "middle" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr"
            | "Bigl" | "Bigr" | "biggl" | "biggr" | "Biggl" | "Biggr" => {
                self.skip_space();
                if self.peek() == Some('.') {
                    self.at += 1;
                    return text("");
                }
                self.atom()
            }
            "begin" | "end" => {
                let open = name == "begin";
                match (self.raw().trim_end_matches('*'), open) {
                    ("pmatrix", true) => text("("),
                    ("pmatrix", false) => text(")"),
                    ("bmatrix", true) => text("["),
                    ("bmatrix", false) => text("]"),
                    ("vmatrix", _) => text("|"),
                    ("Vmatrix", _) => text("‖"),
                    ("cases", true) => text("{ "),
                    _ => text(""),
                }
            }
            "not" => {
                let mut negated = String::new();
                flat(&self.atom(), &mut negated);
                Node::Text(match negated.as_str() {
                    "=" => "≠".into(),
                    "∈" => "∉".into(),
                    "⊂" => "⊄".into(),
                    "⊆" => "⊈".into(),
                    "≡" => "≢".into(),
                    "∼" => "≁".into(),
                    "<" => "≮".into(),
                    ">" => "≯".into(),
                    "≤" => "≰".into(),
                    "≥" => "≱".into(),
                    other => format!("¬{other}"),
                })
            }
            "hat" | "widehat" | "tilde" | "widetilde" | "bar" | "overline" | "dot" | "ddot"
            | "vec" | "overrightarrow" | "underline" => {
                let body = self.atom();
                let mark = match name.as_str() {
                    "hat" | "widehat" => "\u{2c6}",
                    "tilde" | "widetilde" => "\u{2dc}",
                    "bar" | "overline" => "\u{af}",
                    "dot" => "\u{2d9}",
                    "ddot" => "\u{a8}",
                    "vec" | "overrightarrow" => "→",
                    _ => "",
                };
                Node::Row(vec![body, text(mark)])
            }
            "displaystyle" | "textstyle" | "scriptstyle" | "limits" | "nolimits" | "nonumber"
            | "notag" => text(""),
            "mod" | "bmod" => text(" mod "),
            "pmod" => {
                let m = self.atom();
                Node::Row(vec![text(" (mod "), m, text(")")])
            }
            name => match symbol(name) {
                Some(symbol) => text(symbol),
                None if FUNCTIONS.contains(&name) => text(name),
                None => Node::Text(format!("\\{name}")),
            },
        }
    }
}

/// Hangs a script on the last node.
fn attach(nodes: &mut Vec<Node>, sup: bool, arg: Node) {
    let arg = Some(Box::new(arg));
    let node = match nodes.pop() {
        Some(Node::Scripts {
            base,
            sup: None,
            sub,
        }) if sup => Node::Scripts {
            base,
            sup: arg,
            sub,
        },
        Some(Node::Scripts {
            base,
            sup: old,
            sub: None,
        }) if !sup => Node::Scripts {
            base,
            sup: old,
            sub: arg,
        },
        last => {
            let base = Box::new(last.unwrap_or(Node::Text(String::new())));
            if sup {
                Node::Scripts {
                    base,
                    sup: arg,
                    sub: None,
                }
            } else {
                Node::Scripts {
                    base,
                    sup: None,
                    sub: arg,
                }
            }
        }
    };
    nodes.push(node);
}

/// Whether `text` reads as one token, so it needs no parentheses.
fn simple(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '.' || ch == '′')
}

fn wrapped(text: &str, out: &mut String) {
    if simple(text) {
        out.push_str(text);
    } else {
        out.push('(');
        out.push_str(text);
        out.push(')');
    }
}

/// `text` as superscript (`sup`) or subscript characters, if every
/// character has one.
fn scripted(text: &str, sup: bool) -> Option<String> {
    let map = if sup { superscript } else { subscript };
    text.chars().filter(|ch| *ch != ' ').map(map).collect()
}

/// A script inline: Unicode script characters, else `^x` or `^(..)`.
fn script(node: &Node, sup: bool, out: &mut String) {
    let mut text = String::new();
    flat(node, &mut text);
    let text = text.trim();
    if text.chars().all(|ch| ch == '′' || ch == '″') {
        out.push_str(text);
        return;
    }
    match scripted(text, sup) {
        Some(mapped) => out.push_str(&mapped),
        None => {
            out.push(if sup { '^' } else { '_' });
            if text.chars().count() == 1 {
                out.push_str(text);
            } else {
                out.push('(');
                out.push_str(text);
                out.push(')');
            }
        }
    }
}

/// Appends `node` on one line.
fn flat(node: &Node, out: &mut String) {
    match node {
        Node::Text(text) => out.push_str(text),
        Node::Row(nodes) => nodes.iter().for_each(|node| flat(node, out)),
        Node::Break => out.push_str("  "),
        Node::Frac(top, bottom) => {
            let (mut a, mut b) = (String::new(), String::new());
            flat(top, &mut a);
            flat(bottom, &mut b);
            let (a, b) = (a.trim(), b.trim());
            if simple(a) && simple(b) {
                match (scripted(a, true), scripted(b, false)) {
                    (Some(a), Some(b)) if a.chars().all(|c| "⁰¹²³⁴⁵⁶⁷⁸⁹".contains(c)) =>
                    {
                        out.push_str(&a);
                        out.push('⁄');
                        out.push_str(&b);
                    }
                    _ => {
                        out.push_str(a);
                        out.push('⁄');
                        out.push_str(b);
                    }
                }
            } else {
                wrapped(a, out);
                out.push('/');
                wrapped(b, out);
            }
        }
        Node::Sqrt(index, body) => {
            if let Some(index) = index {
                let mut text = String::new();
                flat(index, &mut text);
                match scripted(text.trim(), true) {
                    Some(mapped) => out.push_str(&mapped),
                    None => {
                        out.push('(');
                        out.push_str(text.trim());
                        out.push(')');
                    }
                }
            }
            out.push('√');
            let mut text = String::new();
            flat(body, &mut text);
            wrapped(text.trim(), out);
        }
        Node::Scripts { base, sup, sub } => {
            flat(base, out);
            if let Some(sub) = sub {
                script(sub, false, out);
            }
            if let Some(sup) = sup {
                script(sup, true, out);
            }
        }
    }
}

/// Rows of text with the row the expression's baseline sits on.
struct Block {
    rows: Vec<String>,
    base: usize,
    width: usize,
}

impl Block {
    fn line(text: String) -> Self {
        let width = text.width();
        Self {
            rows: vec![text],
            base: 0,
            width,
        }
    }

    /// Row `index` padded to the block's width.
    fn padded(&self, index: usize, out: &mut String) {
        let row = self.rows.get(index).map_or("", String::as_str);
        out.push_str(row);
        out.extend(std::iter::repeat_n(
            ' ',
            self.width.saturating_sub(row.width()),
        ));
    }

    /// Row `index` centered in `width` cells.
    fn centered(&self, index: usize, width: usize, out: &mut String) {
        let pad = width.saturating_sub(self.width);
        out.extend(std::iter::repeat_n(' ', pad / 2));
        self.padded(index, out);
        out.extend(std::iter::repeat_n(' ', pad - pad / 2));
    }
}

/// Blocks side by side, their baselines on one row.
fn hcat(blocks: &[Block]) -> Block {
    let above = blocks.iter().map(|b| b.base).max().unwrap_or(0);
    let below = blocks
        .iter()
        .map(|b| b.rows.len().saturating_sub(b.base + 1))
        .max()
        .unwrap_or(0);
    let mut rows = Vec::with_capacity(above + below + 1);
    for row in 0..=above + below {
        let mut text = String::new();
        for block in blocks {
            match (row + block.base).checked_sub(above) {
                Some(index) => block.padded(index, &mut text),
                None => text.extend(std::iter::repeat_n(' ', block.width)),
            }
        }
        rows.push(text);
    }
    Block {
        rows,
        base: above,
        width: blocks.iter().map(|b| b.width).sum(),
    }
}

/// Blocks stacked and centered, the baseline at row `base`.
fn vstack(blocks: &[&Block], base: usize) -> Block {
    let width = blocks.iter().map(|b| b.width).max().unwrap_or(0);
    let mut rows = Vec::new();
    for block in blocks {
        for index in 0..block.rows.len() {
            let mut text = String::new();
            block.centered(index, width, &mut text);
            rows.push(text);
        }
    }
    Block { rows, base, width }
}

/// Operators whose scripts go above and below in display math.
const LIMITS: [&str; 12] = [
    "∑", "∏", "∐", "⋃", "⋂", "⨁", "lim", "max", "min", "sup", "inf", "argmax",
];

fn layout(node: &Node) -> Block {
    match node {
        Node::Text(text) => Block::line(text.clone()),
        Node::Break => Block::line(String::new()),
        Node::Row(nodes) => {
            let blocks: Vec<Block> = nodes.iter().map(layout).collect();
            if blocks.is_empty() {
                return Block::line(String::new());
            }
            hcat(&blocks)
        }
        Node::Frac(top, bottom) => {
            let (top, bottom) = (layout(top), layout(bottom));
            let width = top.width.max(bottom.width) + 2;
            let bar = Block::line("─".repeat(width));
            let mut block = vstack(&[&top, &bar, &bottom], top.rows.len());
            block.width = width;
            block
        }
        Node::Sqrt(index, body) => {
            let inner = layout(body);
            if inner.rows.len() == 1 && index.as_ref().is_none_or(|i| layout(i).rows.len() == 1) {
                let mut text = String::new();
                flat(node, &mut text);
                return Block::line(text);
            }
            let mut rows = vec![format!(" {}", "_".repeat(inner.width))];
            for (row, text) in inner.rows.iter().enumerate() {
                let mark = if row == inner.base { '√' } else { ' ' };
                rows.push(format!("{mark}{text}"));
            }
            Block {
                rows,
                base: inner.base + 1,
                width: inner.width + 1,
            }
        }
        Node::Scripts { base, sup, sub } => {
            let base_block = layout(base);
            let limits = matches!(&**base, Node::Text(text) if LIMITS.contains(&text.as_str()));
            let sup = sup.as_deref().map(layout);
            let sub = sub.as_deref().map(layout);
            if limits {
                let mut parts: Vec<&Block> = Vec::new();
                let mut baseline = base_block.base;
                if let Some(sup) = &sup {
                    parts.push(sup);
                    baseline += sup.rows.len();
                }
                parts.push(&base_block);
                if let Some(sub) = &sub {
                    parts.push(sub);
                }
                return vstack(&parts, baseline);
            }
            let one_row = |b: &Option<Block>| b.as_ref().is_none_or(|b| b.rows.len() == 1);
            if base_block.rows.len() == 1 && one_row(&sup) && one_row(&sub) {
                let mut text = String::new();
                flat(node, &mut text);
                let fits = [(&sup, true), (&sub, false)].iter().all(|(b, up)| {
                    b.as_ref()
                        .is_none_or(|b| scripted(b.rows[0].trim(), *up).is_some())
                });
                if fits || (sup.is_none() && sub.is_none()) {
                    return Block::line(text);
                }
            }
            // Scripts in their own column: the superscript ends on the row
            // above the baseline, the subscript starts on the row below.
            let empty = Block::line(String::new());
            let sup = sup.unwrap_or(Block {
                rows: Vec::new(),
                base: 0,
                width: 0,
            });
            let sub = sub.unwrap_or(Block {
                rows: Vec::new(),
                base: 0,
                width: 0,
            });
            let width = sup.width.max(sub.width);
            let mut rows = Vec::new();
            for block in [&sup, &empty, &sub] {
                for index in 0..block.rows.len() {
                    let mut text = String::new();
                    block.padded(index, &mut text);
                    rows.push(text);
                }
            }
            let column = Block {
                rows,
                base: sup.rows.len(),
                width,
            };
            hcat(&[base_block, column])
        }
    }
}

fn superscript(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '−' | '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        'A' => 'ᴬ',
        'B' => 'ᴮ',
        'D' => 'ᴰ',
        'E' => 'ᴱ',
        'G' => 'ᴳ',
        'H' => 'ᴴ',
        'I' => 'ᴵ',
        'J' => 'ᴶ',
        'K' => 'ᴷ',
        'L' => 'ᴸ',
        'M' => 'ᴹ',
        'N' => 'ᴺ',
        'O' => 'ᴼ',
        'P' => 'ᴾ',
        'R' => 'ᴿ',
        'T' => 'ᵀ',
        'U' => 'ᵁ',
        'V' => 'ⱽ',
        'W' => 'ᵂ',
        'α' => 'ᵅ',
        'β' => 'ᵝ',
        'γ' => 'ᵞ',
        'δ' => 'ᵟ',
        'φ' | 'ϕ' => 'ᵠ',
        'χ' => 'ᵡ',
        'θ' => 'ᶿ',
        '∗' => '*',
        '′' => '′',
        _ => return None,
    })
}

fn subscript(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '−' | '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        'β' => 'ᵦ',
        'γ' => 'ᵧ',
        'ρ' => 'ᵨ',
        'φ' | 'ϕ' => 'ᵩ',
        'χ' => 'ᵪ',
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum Font {
    DoubleStruck,
    Script,
    Fraktur,
    Bold,
}

impl Font {
    /// `ch` in this font's Mathematical Alphanumeric Symbols, or itself.
    fn apply(self, ch: char) -> char {
        let hole = match (self, ch) {
            (Font::DoubleStruck, 'C') => Some('ℂ'),
            (Font::DoubleStruck, 'H') => Some('ℍ'),
            (Font::DoubleStruck, 'N') => Some('ℕ'),
            (Font::DoubleStruck, 'P') => Some('ℙ'),
            (Font::DoubleStruck, 'Q') => Some('ℚ'),
            (Font::DoubleStruck, 'R') => Some('ℝ'),
            (Font::DoubleStruck, 'Z') => Some('ℤ'),
            (Font::Script, 'B') => Some('ℬ'),
            (Font::Script, 'E') => Some('ℰ'),
            (Font::Script, 'F') => Some('ℱ'),
            (Font::Script, 'H') => Some('ℋ'),
            (Font::Script, 'I') => Some('ℐ'),
            (Font::Script, 'L') => Some('ℒ'),
            (Font::Script, 'M') => Some('ℳ'),
            (Font::Script, 'R') => Some('ℛ'),
            (Font::Script, 'e') => Some('ℯ'),
            (Font::Script, 'g') => Some('ℊ'),
            (Font::Script, 'o') => Some('ℴ'),
            (Font::Fraktur, 'C') => Some('ℭ'),
            (Font::Fraktur, 'H') => Some('ℌ'),
            (Font::Fraktur, 'I') => Some('ℑ'),
            (Font::Fraktur, 'R') => Some('ℜ'),
            (Font::Fraktur, 'Z') => Some('ℨ'),
            _ => None,
        };
        if let Some(hole) = hole {
            return hole;
        }
        let (upper, lower, digit) = match self {
            Font::DoubleStruck => (0x1d538, 0x1d552, Some(0x1d7d8)),
            Font::Script => (0x1d49c, 0x1d4b6, None),
            Font::Fraktur => (0x1d504, 0x1d51e, None),
            Font::Bold => (0x1d400, 0x1d41a, Some(0x1d7ce)),
        };
        let code = match ch {
            'A'..='Z' => upper + (ch as u32 - 'A' as u32),
            'a'..='z' => lower + (ch as u32 - 'a' as u32),
            '0'..='9' => match digit {
                Some(digit) => digit + (ch as u32 - '0' as u32),
                None => return ch,
            },
            _ => return ch,
        };
        char::from_u32(code).unwrap_or(ch)
    }
}

/// Names written as words, such as `\sin`.
const FUNCTIONS: [&str; 33] = [
    "sin", "cos", "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh", "tanh",
    "coth", "log", "ln", "lg", "exp", "lim", "liminf", "limsup", "max", "min", "sup", "inf", "det",
    "dim", "ker", "deg", "gcd", "lcm", "arg", "hom", "Pr",
];

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "leqslant" => "⩽",
        "geqslant" => "⩾",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "equiv" => "≡",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        "to" | "rightarrow" => "→",
        "gets" | "leftarrow" => "←",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "leftrightarrow" => "↔",
        "implies" | "Longrightarrow" => "⟹",
        "impliedby" | "Longleftarrow" => "⟸",
        "iff" | "Longleftrightarrow" => "⟺",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "Uparrow" => "⇑",
        "Downarrow" => "⇓",
        "hookrightarrow" => "↪",
        "rightleftharpoons" => "⇌",
        "infty" => "∞",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "bigoplus" => "⨁",
        "partial" => "∂",
        "nabla" => "∇",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "cdot" => "⋅",
        "cdots" => "⋯",
        "ldots" | "dots" | "dotsc" | "dotsb" => "…",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "times" => "×",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "circ" => "∘",
        "bullet" => "•",
        "star" => "⋆",
        "ast" => "∗",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "odot" => "⊙",
        "dagger" => "†",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lvert" | "rvert" | "vert" | "mid" => "|",
        "lVert" | "rVert" | "Vert" => "‖",
        "parallel" => "∥",
        "perp" | "bot" => "⊥",
        "top" => "⊤",
        "angle" => "∠",
        "triangle" => "△",
        "prime" => "′",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "wp" => "℘",
        "vdash" => "⊢",
        "models" => "⊨",
        "therefore" => "∴",
        "because" => "∵",
        "colon" => ":",
        "backslash" => "\\",
        "degree" => "°",
        "checkmark" => "✓",
        "square" | "Box" => "□",
        "lbrace" => "{",
        "rbrace" => "}",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_scripts_and_fonts() {
        assert_eq!(inline(r"\alpha + \beta \le \infty"), "α + β ≤ ∞");
        assert_eq!(inline("x^2 + y_i^2 = z^{n+1}"), "x² + yᵢ² = zⁿ⁺¹");
        assert_eq!(inline(r"e^{i\pi} = -1"), "e^(iπ) = −1");
        assert_eq!(
            inline(r"\forall x \in \mathbb{R}, \exists n \in \mathbb N"),
            "∀ x ∈ ℝ, ∃ n ∈ ℕ"
        );
        assert_eq!(inline(r"\mathcal{L} \Rightarrow \nabla f"), "ℒ ⇒ ∇ f");
        assert_eq!(
            inline(r"\sin\theta \cdot \text{if } x \not= 0"),
            "sinθ ⋅ if  x ≠ 0"
        );
        assert_eq!(inline(r"f'(x) \to \foo"), "f′(x) → \\foo");
    }

    #[test]
    fn fractions_and_roots_inline() {
        assert_eq!(inline(r"\frac{a}{b}"), "a⁄b");
        assert_eq!(inline(r"\frac12"), "¹⁄₂");
        assert_eq!(inline(r"\frac{a+b}{2c}"), "(a+b)/2c");
        assert_eq!(
            inline(r"\sqrt{x} + \sqrt{x+1} + \sqrt[3]{y}"),
            "√x + √(x+1) + ³√y"
        );
        assert_eq!(inline(r"\left( \frac{x}{y} \right)"), "(x⁄y)");
    }

    #[test]
    fn display_stacks_fractions_and_limits() {
        assert_eq!(display(r"\frac{a+b}{c} = 1"), [" a+b", "───── = 1", "  c"]);
        assert_eq!(display(r"\sum_{i=1}^{n} i"), [" n", " ∑  i", "i=1"]);
        assert_eq!(display(r"a = 1 \\ b = 2"), ["a = 1", "b = 2"]);
    }

    #[test]
    fn delimiters_become_dollars_outside_code() {
        assert_eq!(
            delimiters(r"see \(x^2\) and `\(y\)`"),
            r"see $x^2$ and `\(y\)`"
        );
        assert_eq!(delimiters("\\[\nx\n\\]\n"), "$$\nx\n$$\n");
        assert_eq!(delimiters("\\[ x \\]"), "$$ x $$");
        assert_eq!(delimiters("```\n\\(x\\)\n```\n"), "```\n\\(x\\)\n```\n");
        assert_eq!(delimiters(r"a \[link\]"), r"a \[link\]");
        assert!(matches!(delimiters("no math"), Cow::Borrowed(_)));
    }
}
