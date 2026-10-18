//! Mermaid diagrams as box-drawing text for the Markdown reader.
//!
//! [`draw`] reads flowcharts (`graph`, `flowchart`) and sequence diagrams
//! and returns the rows of a text drawing. Flowcharts are laid out in
//! layers: every node sits one layer below the deepest node that points to
//! it, edges that skip layers run through placeholder nodes, and edges
//! that close a cycle are drawn against the flow with their arrow at the
//! upper end. Other diagram types are an error, and the reader shows the
//! source instead.
//!
//! Parsing runs once on the loader thread, so this allocates freely.

use std::collections::HashMap;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The rows of `source` drawn as text, or why it cannot be drawn.
pub fn draw(source: &str) -> Result<Vec<String>, String> {
    let statements = statements(source);
    let Some((header, rest)) = statements.split_first() else {
        return Err("the diagram is empty".into());
    };
    let mut words = header.split_whitespace();
    match words.next().unwrap_or_default() {
        "graph" | "flowchart" | "flowchart-elk" => {
            let direction = words.next().unwrap_or("TD");
            flowchart(direction, rest)
        }
        "sequenceDiagram" => sequence(rest),
        other => Err(format!("{other} diagrams are not supported")),
    }
}

/// Statements without comments and blank lines: one per line, and lines
/// split at `;` outside quotes and brackets.
fn statements(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = match line.find("%%") {
            Some(at) => &line[..at],
            None => line,
        };
        let mut depth = 0i32;
        let mut quoted = false;
        let mut start = 0;
        for (index, ch) in line.char_indices() {
            match ch {
                '"' => quoted = !quoted,
                '[' | '(' | '{' if !quoted => depth += 1,
                ']' | ')' | '}' if !quoted => depth -= 1,
                ';' if !quoted && depth <= 0 => {
                    out.push(line[start..index].trim().to_owned());
                    start = index + 1;
                }
                _ => {}
            }
        }
        out.push(line[start..].trim().to_owned());
    }
    out.retain(|statement| !statement.is_empty());
    out
}

/// Text cells of a grid, with line segments kept apart so crossing and
/// touching lines join into the right box-drawing character.
#[derive(Default)]
struct Canvas {
    /// `None` shows the line bits, `Some('\0')` is the second cell of a wide
    /// character.
    cells: Vec<Vec<Option<char>>>,
    /// Bits of [`UP`], [`DOWN`], [`LEFT`], [`RIGHT`] and the line's style
    /// in the high bits.
    lines: Vec<Vec<u8>>,
}

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;
const DOTTED: u8 = 16;
const THICK: u8 = 32;

impl Canvas {
    fn grow(&mut self, x: usize, y: usize) {
        if self.cells.len() <= y {
            self.cells.resize_with(y + 1, Vec::new);
            self.lines.resize_with(y + 1, Vec::new);
        }
        if self.cells[y].len() <= x {
            self.cells[y].resize(x + 1, None);
            self.lines[y].resize(x + 1, 0);
        }
    }

    fn put(&mut self, x: usize, y: usize, ch: char) {
        self.grow(x, y);
        self.cells[y][x] = Some(ch);
    }

    fn text(&mut self, x: usize, y: usize, text: &str) {
        let mut x = x;
        for ch in text.chars() {
            let width = ch.width().unwrap_or(0);
            if width == 0 {
                continue;
            }
            self.put(x, y, ch);
            if width == 2 {
                self.put(x + 1, y, '\0');
            }
            x += width;
        }
    }

    fn bits(&mut self, x: usize, y: usize, bits: u8) {
        self.grow(x, y);
        self.lines[y][x] |= bits;
    }

    fn hline(&mut self, y: usize, a: usize, b: usize, style: u8) {
        let (a, b) = (a.min(b), a.max(b));
        for x in a..=b {
            let mut bits = style;
            if x > a {
                bits |= LEFT;
            }
            if x < b {
                bits |= RIGHT;
            }
            self.bits(x, y, bits);
        }
    }

    fn vline(&mut self, x: usize, a: usize, b: usize, style: u8) {
        let (a, b) = (a.min(b), a.max(b));
        for y in a..=b {
            let mut bits = style;
            if y > a {
                bits |= UP;
            }
            if y < b {
                bits |= DOWN;
            }
            self.bits(x, y, bits);
        }
    }

    /// A box with its corners from `corners` (top left, top right, bottom
    /// left, bottom right) and `label` centered inside.
    fn frame(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        corners: [char; 4],
        label: &[String],
    ) {
        let (right, bottom) = (x + w - 1, y + h - 1);
        for cx in x + 1..right {
            self.put(cx, y, '─');
            self.put(cx, bottom, '─');
        }
        for cy in y + 1..bottom {
            self.put(x, cy, '│');
            self.put(right, cy, '│');
            for cx in x + 1..right {
                self.put(cx, cy, ' ');
            }
        }
        self.put(x, y, corners[0]);
        self.put(right, y, corners[1]);
        self.put(x, bottom, corners[2]);
        self.put(right, bottom, corners[3]);
        for (row, line) in label.iter().enumerate() {
            let pad = (w - 2).saturating_sub(line.width());
            self.text(x + 1 + pad / 2, y + 1 + row, line);
        }
    }

    fn rows(&self) -> Vec<String> {
        let mut rows = Vec::with_capacity(self.cells.len());
        for (cells, lines) in self.cells.iter().zip(&self.lines) {
            let mut row = String::new();
            for (cell, &bits) in cells.iter().zip(lines) {
                match cell {
                    Some('\0') => {}
                    Some(ch) => row.push(*ch),
                    None => row.push(line_char(bits)),
                }
            }
            rows.push(row.trim_end().to_owned());
        }
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }
}

fn line_char(bits: u8) -> char {
    let thick = bits & THICK != 0;
    let dotted = bits & DOTTED != 0;
    let dirs = bits & (UP | DOWN | LEFT | RIGHT);
    let vertical = dirs & (LEFT | RIGHT) == 0 && dirs != 0;
    let horizontal = dirs & (UP | DOWN) == 0 && dirs != 0;
    match () {
        _ if dirs == 0 => ' ',
        _ if vertical => match (thick, dotted) {
            (true, _) => '┃',
            (_, true) => '┆',
            _ => '│',
        },
        _ if horizontal => match (thick, dotted) {
            (true, _) => '━',
            (_, true) => '┄',
            _ => '─',
        },
        _ => {
            let set = if thick {
                ['┏', '┓', '┗', '┛', '┣', '┫', '┳', '┻', '╋']
            } else {
                ['┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼']
            };
            match dirs {
                d if d == DOWN | RIGHT => set[0],
                d if d == DOWN | LEFT => set[1],
                d if d == UP | RIGHT => set[2],
                d if d == UP | LEFT => set[3],
                d if d == UP | DOWN | RIGHT => set[4],
                d if d == UP | DOWN | LEFT => set[5],
                d if d == DOWN | LEFT | RIGHT => set[6],
                d if d == UP | LEFT | RIGHT => set[7],
                _ => set[8],
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Square,
    Round,
    Angled,
}

impl Shape {
    fn corners(self) -> [char; 4] {
        match self {
            Shape::Square => ['┌', '┐', '└', '┘'],
            Shape::Round => ['╭', '╮', '╰', '╯'],
            Shape::Angled => ['╱', '╲', '╲', '╱'],
        }
    }
}

struct Node {
    label: Vec<String>,
    shape: Shape,
}

struct Edge {
    from: usize,
    to: usize,
    label: Option<String>,
    style: u8,
    arrow_from: bool,
    arrow_to: bool,
}

/// Openers and closers of node shapes, longest first.
const SHAPES: [(&str, &str, Shape); 13] = [
    ("(((", ")))", Shape::Round),
    ("((", "))", Shape::Round),
    ("([", "])", Shape::Round),
    ("[[", "]]", Shape::Square),
    ("[(", ")]", Shape::Round),
    ("[/", "/]", Shape::Angled),
    ("[\\", "\\]", Shape::Angled),
    ("{{", "}}", Shape::Angled),
    ("[", "]", Shape::Square),
    ("(", ")", Shape::Round),
    ("{", "}", Shape::Angled),
    (">", "]", Shape::Angled),
    ("[/", "\\]", Shape::Angled),
];

struct Graph {
    nodes: Vec<Node>,
    ids: HashMap<String, usize>,
    edges: Vec<Edge>,
    /// Labels of edges from a node to itself, per node.
    loops: Vec<(usize, Option<String>)>,
}

/// A label with Mermaid's line breaks as lines, quotes and markup removed.
fn label_lines(text: &str) -> Vec<String> {
    let text = text.trim().trim_matches('"').trim_matches('`');
    let mut lines = Vec::new();
    let mut rest = text;
    loop {
        let lower = rest.to_ascii_lowercase();
        let found = ["<br/>", "<br />", "<br>", "\\n"]
            .iter()
            .filter_map(|tag| lower.find(tag).map(|at| (at, tag.len())))
            .min();
        match found {
            Some((at, len)) => {
                lines.push(rest[..at].trim().to_owned());
                rest = &rest[at + len..];
            }
            None => {
                lines.push(rest.trim().to_owned());
                break;
            }
        }
    }
    lines
}

impl Graph {
    fn node(&mut self, line: &str, at: &mut usize) -> Result<usize, String> {
        skip_space(line, at);
        let start = *at;
        for (index, ch) in line[start..].char_indices() {
            if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                *at = start + index + ch.len_utf8();
            } else {
                break;
            }
        }
        let id = &line[start..*at];
        if id.is_empty() {
            return Err(format!("cannot read a node at \"{}\"", &line[start..]));
        }
        let mut shape = None;
        for (open, close, kind) in SHAPES {
            if !line[*at..].starts_with(open) {
                continue;
            }
            let body = &line[*at + open.len()..];
            let end = if body.trim_start().starts_with('"') {
                let first = body.find('"').unwrap_or(0);
                body[first + 1..]
                    .find('"')
                    .and_then(|q| body[first + 1 + q..].find(close).map(|c| first + 1 + q + c))
            } else {
                body.find(close)
            };
            let Some(end) = end else {
                continue;
            };
            shape = Some((label_lines(&body[..end]), kind));
            *at += open.len() + end + close.len();
            break;
        }
        if line[*at..].starts_with(":::") {
            *at += 3;
            while line[*at..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                *at += 1;
            }
        }
        let index = match self.ids.get(id) {
            Some(&index) => index,
            None => {
                self.nodes.push(Node {
                    label: vec![id.to_owned()],
                    shape: Shape::Square,
                });
                self.ids.insert(id.to_owned(), self.nodes.len() - 1);
                self.nodes.len() - 1
            }
        };
        if let Some((label, kind)) = shape {
            self.nodes[index].label = label;
            self.nodes[index].shape = kind;
        }
        Ok(index)
    }

    /// Nodes joined with `&`.
    fn group(&mut self, line: &str, at: &mut usize) -> Result<Vec<usize>, String> {
        let mut group = vec![self.node(line, at)?];
        loop {
            skip_space(line, at);
            if !line[*at..].starts_with('&') {
                return Ok(group);
            }
            *at += 1;
            group.push(self.node(line, at)?);
        }
    }

    fn statement(&mut self, line: &str) -> Result<(), String> {
        let first = line.split_whitespace().next().unwrap_or_default();
        if matches!(
            first,
            "subgraph"
                | "end"
                | "style"
                | "classDef"
                | "class"
                | "click"
                | "linkStyle"
                | "direction"
                | "accTitle"
                | "accDescr"
                | "accTitle:"
                | "accDescr:"
        ) {
            return Ok(());
        }
        let mut at = 0;
        let mut left = self.group(line, &mut at)?;
        loop {
            skip_space(line, &mut at);
            if at >= line.len() {
                return Ok(());
            }
            let Some(link) = link(line, &mut at) else {
                return Err(format!("cannot read \"{}\"", &line[at..]));
            };
            let right = self.group(line, &mut at)?;
            for &from in &left {
                for &to in &right {
                    if from == to {
                        self.loops.push((from, link.label.clone()));
                        continue;
                    }
                    self.edges.push(Edge {
                        from,
                        to,
                        label: link.label.clone(),
                        style: link.style,
                        arrow_from: link.arrow_from,
                        arrow_to: link.arrow_to,
                    });
                }
            }
            left = right;
        }
    }
}

fn skip_space(line: &str, at: &mut usize) {
    while line[*at..].starts_with(char::is_whitespace) {
        *at += line[*at..].chars().next().map_or(1, char::len_utf8);
    }
}

struct Link {
    label: Option<String>,
    style: u8,
    arrow_from: bool,
    arrow_to: bool,
}

/// The run of link characters at `at` and the arrow head after it.
fn link_run(line: &str, at: &mut usize) -> (String, bool) {
    let start = *at;
    while line[*at..].starts_with(['-', '=', '.', '~']) {
        *at += 1;
    }
    let run = line[start..*at].to_owned();
    let rest = &line[*at..];
    // `x` and `o` heads count only before a space or a label, so `A --oB`
    // stays a link to the node `oB`.
    let head = rest.starts_with('>')
        || (rest.starts_with(['x', 'o'])
            && rest[1..].starts_with(|c: char| c.is_whitespace() || c == '|'));
    if head {
        *at += 1;
    }
    (run, head)
}

fn link(line: &str, at: &mut usize) -> Option<Link> {
    let arrow_from = line[*at..].starts_with('<')
        || (line[*at..].starts_with(['x', 'o']) && line[*at + 1..].starts_with(['-', '=', '.']));
    if arrow_from {
        *at += 1;
    }
    let (run, mut arrow_to) = link_run(line, at);
    if run.len() < 2 {
        return None;
    }
    let mut style = if run.contains('.') {
        DOTTED
    } else if run.contains('=') {
        THICK
    } else {
        0
    };
    let mut label = None;
    // `-- text -->`: the text runs up to the next link characters.
    if !arrow_to && run.len() == 2 && !line[*at..].trim_start().starts_with('|') {
        let rest = &line[*at..];
        let end = ["--", "==", ".-", "-."]
            .iter()
            .filter_map(|run| rest.find(run))
            .min();
        if let Some(end) = end
            && !rest[..end].trim().is_empty()
        {
            label = Some(rest[..end].trim().to_owned());
            *at += end;
            let (closing, head) = link_run(line, at);
            arrow_to = head;
            if closing.contains('.') {
                style = DOTTED;
            } else if closing.contains('=') {
                style = THICK;
            }
        }
    }
    skip_space(line, at);
    if line[*at..].starts_with('|')
        && let Some(end) = line[*at + 1..].find('|')
    {
        label = Some(line[*at + 1..*at + 1 + end].trim().to_owned());
        *at += end + 2;
    }
    let label = label
        .map(|l| label_lines(&l).join(" "))
        .filter(|l| !l.is_empty());
    Some(Link {
        label,
        style,
        arrow_from,
        arrow_to,
    })
}

/// One edge between adjacent layers. Long edges become chains of these
/// through placeholder nodes.
struct Segment {
    upper: usize,
    lower: usize,
    arrow_upper: bool,
    arrow_lower: bool,
    label: Option<String>,
    style: u8,
}

/// A node in the layout: a real node, or a placeholder an edge passes.
struct Slot {
    real: Option<usize>,
    rank: usize,
    /// Size across and along the flow, for a top-down layout; swapped for
    /// left to right.
    w: usize,
    h: usize,
    /// Position of the top left corner.
    x: usize,
    y: usize,
}

fn flowchart(direction: &str, statements: &[String]) -> Result<Vec<String>, String> {
    let (vertical, reversed) = match direction.to_ascii_uppercase().as_str() {
        "TD" | "TB" => (true, false),
        "BT" => (true, true),
        "LR" => (false, false),
        "RL" => (false, true),
        other => return Err(format!("unknown direction {other}")),
    };
    let mut graph = Graph {
        nodes: Vec::new(),
        ids: HashMap::new(),
        edges: Vec::new(),
        loops: Vec::new(),
    };
    for statement in statements {
        graph.statement(statement)?;
    }
    if graph.nodes.is_empty() {
        return Err("the flowchart has no nodes".into());
    }
    for (node, label) in std::mem::take(&mut graph.loops) {
        let mark = match label {
            Some(label) => format!(" ↺ {label}"),
            None => " ↺".to_owned(),
        };
        if let Some(line) = graph.nodes[node].label.last_mut() {
            line.push_str(&mark);
        }
    }

    // Orient every edge downstream, then turn edges that close a cycle.
    let n = graph.nodes.len();
    let mut oriented: Vec<(usize, usize, bool, bool, &Edge)> = graph
        .edges
        .iter()
        .map(|e| {
            if reversed {
                (e.to, e.from, e.arrow_to, e.arrow_from, e)
            } else {
                (e.from, e.to, e.arrow_from, e.arrow_to, e)
            }
        })
        .collect();
    let mut state = vec![0u8; n];
    let mut order = Vec::with_capacity(n);
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        // Iterative depth-first search: (node, next edge to look at).
        let mut stack = vec![(root, 0usize)];
        state[root] = 1;
        while let Some(&(node, next)) = stack.last() {
            let found = (next..oriented.len()).find(|&i| oriented[i].0 == node);
            match found {
                Some(i) => {
                    if let Some(top) = stack.last_mut() {
                        top.1 = i + 1;
                    }
                    let to = oriented[i].1;
                    match state[to] {
                        0 => {
                            state[to] = 1;
                            stack.push((to, 0));
                        }
                        1 => {
                            let (u, l, au, al, e) = oriented[i];
                            oriented[i] = (l, u, al, au, e);
                        }
                        _ => {}
                    }
                }
                None => {
                    state[node] = 2;
                    order.push(node);
                    stack.pop();
                }
            }
        }
    }
    let mut rank = vec![0usize; n];
    for &node in order.iter().rev() {
        for &(u, l, ..) in &oriented {
            if u == node {
                rank[l] = rank[l].max(rank[u] + 1);
            }
        }
    }

    let mut slots: Vec<Slot> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let width = node.label.iter().map(|l| l.width()).max().unwrap_or(0) + 4;
            let height = node.label.len() + 2;
            let (w, h) = if vertical {
                (width, height)
            } else {
                (height, width)
            };
            Slot {
                real: Some(index),
                rank: rank[index],
                w,
                h,
                x: 0,
                y: 0,
            }
        })
        .collect();
    let mut segments = Vec::new();
    for &(upper, lower, arrow_upper, arrow_lower, edge) in &oriented {
        let mut from = upper;
        for r in rank[upper] + 1..rank[lower] {
            slots.push(Slot {
                real: None,
                rank: r,
                w: 1,
                h: 0,
                x: 0,
                y: 0,
            });
            let slot = slots.len() - 1;
            segments.push(Segment {
                upper: from,
                lower: slot,
                arrow_upper: from == upper && arrow_upper,
                arrow_lower: false,
                label: None,
                style: edge.style,
            });
            from = slot;
        }
        segments.push(Segment {
            upper: from,
            lower,
            arrow_upper: from == upper && arrow_upper,
            arrow_lower,
            label: edge.label.clone(),
            style: edge.style,
        });
    }

    // Layers in order of appearance, then each sorted under its parents.
    let layers_count = slots.iter().map(|s| s.rank).max().unwrap_or(0) + 1;
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layers_count];
    for (index, slot) in slots.iter().enumerate() {
        layers[slot.rank].push(index);
    }
    let mut position = vec![0f32; slots.len()];
    for layer in &mut layers {
        for (i, &slot) in layer.iter().enumerate() {
            position[slot] = i as f32;
        }
    }
    for layer in layers.iter_mut().skip(1) {
        let mut keyed: Vec<(f32, usize)> = layer
            .iter()
            .map(|&slot| {
                let parents: Vec<f32> = segments
                    .iter()
                    .filter(|s| s.lower == slot)
                    .map(|s| position[s.upper])
                    .collect();
                let key = if parents.is_empty() {
                    position[slot]
                } else {
                    parents.iter().sum::<f32>() / parents.len() as f32
                };
                (key, slot)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        *layer = keyed.into_iter().map(|(_, slot)| slot).collect();
        for (i, &slot) in layer.iter().enumerate() {
            position[slot] = i as f32;
        }
    }

    let mut canvas = Canvas::default();
    if vertical {
        place_vertical(&mut slots, &layers, &segments, &mut canvas);
    } else {
        place_horizontal(&mut slots, &layers, &segments, &mut canvas);
    }
    for slot in &slots {
        if let Some(real) = slot.real {
            let node = &graph.nodes[real];
            let (w, h) = if vertical {
                (slot.w, slot.h)
            } else {
                (slot.h, slot.w)
            };
            canvas.frame(slot.x, slot.y, w, h, node.shape.corners(), &node.label);
        }
    }
    finish_ends(&slots, &segments, vertical, &mut canvas);
    Ok(canvas.rows())
}

/// The center of a slot across the flow.
fn across(slot: &Slot, vertical: bool) -> usize {
    if vertical {
        slot.x + slot.w / 2
    } else {
        slot.y + slot.w / 2
    }
}

/// Gap sizes between layer `r` and the next: (bus rows of each upper slot
/// that needs one, number of buses, label size).
fn gap(
    layer: &[usize],
    segments: &[Segment],
    slots: &[Slot],
    vertical: bool,
) -> (HashMap<usize, usize>, usize, usize) {
    let mut buses = HashMap::new();
    let mut label = 0;
    for segment in segments.iter().filter(|s| layer.contains(&s.upper)) {
        if across(&slots[segment.upper], vertical) != across(&slots[segment.lower], vertical) {
            let next = buses.len();
            buses.entry(segment.upper).or_insert(next);
        }
        if let Some(text) = &segment.label {
            label = label.max(if vertical { 1 } else { text.width() + 2 });
        }
    }
    let count = buses.len().max(1);
    (buses, count, label)
}

/// Lays out slots across each layer (`x`) and layers down (`y`).
fn spread(slots: &mut [Slot], layers: &[Vec<usize>], vertical: bool) {
    let sizes: Vec<usize> = layers
        .iter()
        .map(|layer| {
            let sum: usize = layer.iter().map(|&s| slots[s].w).sum();
            sum + 2 * layer.len().saturating_sub(1)
        })
        .collect();
    let total = sizes.iter().copied().max().unwrap_or(0);
    for (layer, size) in layers.iter().zip(&sizes) {
        let mut at = (total - size) / 2;
        for &slot in layer {
            if vertical {
                slots[slot].x = at;
            } else {
                slots[slot].y = at;
            }
            at += slots[slot].w + if vertical { 2 } else { 1 };
        }
    }
}

fn place_vertical(
    slots: &mut [Slot],
    layers: &[Vec<usize>],
    segments: &[Segment],
    canvas: &mut Canvas,
) {
    spread(slots, layers, true);
    let mut y = 0;
    for (r, layer) in layers.iter().enumerate() {
        let height = layer.iter().map(|&s| slots[s].h).max().unwrap_or(0);
        for &slot in layer {
            slots[slot].y = y;
        }
        let gap_top = y + height;
        if r + 1 == layers.len() {
            break;
        }
        let (buses, count, label) = gap(layer, segments, slots, true);
        let size = 1 + count + label + 1;
        let end = gap_top + size - 1;
        let mut labels: HashMap<usize, Vec<&str>> = HashMap::new();
        for segment in segments.iter().filter(|s| layer.contains(&s.upper)) {
            let (upper, lower) = (&slots[segment.upper], &slots[segment.lower]);
            let (px, cx) = (across(upper, true), across(lower, true));
            let start = upper.y + upper.h;
            let bus = gap_top + 1 + buses.get(&segment.upper).copied().unwrap_or(0);
            canvas.vline(px, start, bus, segment.style);
            canvas.hline(bus, px, cx, segment.style);
            canvas.vline(cx, bus, end, segment.style);
            if let Some(text) = &segment.label {
                labels.entry(cx).or_default().push(text);
            }
        }
        for (cx, texts) in labels {
            canvas.text(cx + 2, end - 1, &texts.join(", "));
        }
        y = gap_top + size;
    }
}

fn place_horizontal(
    slots: &mut [Slot],
    layers: &[Vec<usize>],
    segments: &[Segment],
    canvas: &mut Canvas,
) {
    spread(slots, layers, false);
    let mut x = 0;
    for (r, layer) in layers.iter().enumerate() {
        let width = layer.iter().map(|&s| slots[s].h).max().unwrap_or(0);
        for &slot in layer {
            slots[slot].x = x;
        }
        let gap_left = x + width;
        if r + 1 == layers.len() {
            break;
        }
        let (buses, count, label) = gap(layer, segments, slots, false);
        let size = 1 + count + label + 1;
        let end = gap_left + size - 1;
        let mut labels: HashMap<usize, Vec<&str>> = HashMap::new();
        for segment in segments.iter().filter(|s| layer.contains(&s.upper)) {
            let (upper, lower) = (&slots[segment.upper], &slots[segment.lower]);
            let (py, cy) = (across(upper, false), across(lower, false));
            let start = upper.x + upper.h;
            let bus = gap_left + 1 + buses.get(&segment.upper).copied().unwrap_or(0);
            canvas.hline(py, start, bus, segment.style);
            canvas.vline(bus, py, cy, segment.style);
            canvas.hline(cy, bus, end, segment.style);
            if let Some(text) = &segment.label {
                labels.entry(cy).or_default().push(text);
            }
        }
        for (cy, texts) in labels {
            canvas.text(gap_left + 1 + count + 1, cy, &texts.join(", "));
        }
        x = gap_left + size;
    }
}

/// Arrow heads, and joints where lines meet a box without one.
fn finish_ends(slots: &[Slot], segments: &[Segment], vertical: bool, canvas: &mut Canvas) {
    for segment in segments {
        let (upper, lower) = (&slots[segment.upper], &slots[segment.lower]);
        let (pu, pl) = (across(upper, vertical), across(lower, vertical));
        if vertical {
            let below = upper.y + upper.h;
            if segment.arrow_upper && upper.real.is_some() {
                canvas.put(pu, below, '▲');
            } else if upper.real.is_some() {
                canvas.put(pu, below - 1, '┬');
            }
            if lower.real.is_some() {
                if segment.arrow_lower {
                    canvas.put(pl, lower.y - 1, '▼');
                } else {
                    canvas.put(pl, lower.y, '┴');
                }
            }
        } else {
            let after = upper.x + upper.h;
            if segment.arrow_upper && upper.real.is_some() {
                canvas.put(after, pu, '◀');
            } else if upper.real.is_some() {
                canvas.put(after - 1, pu, '├');
            }
            if lower.real.is_some() {
                if segment.arrow_lower {
                    canvas.put(lower.x - 1, pl, '▶');
                } else {
                    canvas.put(lower.x, pl, '┤');
                }
            }
        }
    }
}

enum Step {
    Message {
        from: usize,
        to: usize,
        text: String,
        dotted: bool,
        head: char,
    },
    Note {
        first: usize,
        last: usize,
        side: i8,
        text: String,
    },
    /// `loop`, `alt`, `else`, `end` and the like: a line of text.
    Block(String),
}

/// The index of participant `id`, added on first use.
fn index(names: &mut Vec<(String, String)>, id: &str) -> usize {
    let id = id.trim().trim_start_matches(['+', '-']).trim();
    match names.iter().position(|(known, _)| known == id) {
        Some(index) => index,
        None => {
            names.push((id.to_owned(), id.to_owned()));
            names.len() - 1
        }
    }
}

/// Message arrows, longest first so `-->>` wins over `->`.
const ARROWS: [(&str, bool, char); 8] = [
    ("-->>", true, '▶'),
    ("--x", true, 'x'),
    ("--)", true, '▷'),
    ("-->", true, '▶'),
    ("->>", false, '▶'),
    ("-x", false, 'x'),
    ("-)", false, '▷'),
    ("->", false, '▶'),
];

fn sequence(statements: &[String]) -> Result<Vec<String>, String> {
    let mut names: Vec<(String, String)> = Vec::new();
    let mut steps = Vec::new();
    for line in statements {
        let (first, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match first {
            "participant" | "actor" => {
                let (id, alias) = match rest.split_once(" as ") {
                    Some((id, alias)) => (id.trim(), alias.trim()),
                    None => (rest.trim(), rest.trim()),
                };
                let at = index(&mut names, id);
                names[at].1 = label_lines(alias).join(" ");
                continue;
            }
            "autonumber" | "activate" | "deactivate" | "title" | "box" | "create" | "destroy" => {
                continue;
            }
            "loop" | "alt" | "else" | "opt" | "par" | "and" | "critical" | "break" | "rect"
            | "option" | "end" => {
                steps.push(Step::Block(line.clone()));
                continue;
            }
            _ => {}
        }
        if first.eq_ignore_ascii_case("note") {
            let Some((place, text)) = rest.split_once(':') else {
                return Err(format!("cannot read \"{line}\""));
            };
            let place = place.trim();
            let (side, who) = if let Some(who) = place.strip_prefix("left of") {
                (-1, who)
            } else if let Some(who) = place.strip_prefix("right of") {
                (1, who)
            } else if let Some(who) = place.strip_prefix("over") {
                (0, who)
            } else {
                return Err(format!("cannot read \"{line}\""));
            };
            let mut who = who.split(',').map(|w| index(&mut names, w));
            let first = who.next().unwrap_or(0);
            let last = who.next().unwrap_or(first);
            steps.push(Step::Note {
                first: first.min(last),
                last: first.max(last),
                side,
                text: label_lines(text).join(" "),
            });
            continue;
        }
        let found = (0..line.len())
            .filter(|&at| line.is_char_boundary(at))
            .find_map(|at| {
                ARROWS
                    .iter()
                    .find(|(arrow, ..)| line[at..].starts_with(arrow))
                    .map(|arrow| (at, arrow))
            });
        let Some((at, &(arrow, dotted, head))) = found else {
            return Err(format!("cannot read \"{line}\""));
        };
        let (target, text) = line[at + arrow.len()..]
            .split_once(':')
            .unwrap_or((&line[at + arrow.len()..], ""));
        if line[..at].trim().is_empty() || target.trim().is_empty() {
            return Err(format!("cannot read \"{line}\""));
        }
        let from = index(&mut names, &line[..at]);
        let to = index(&mut names, target);
        steps.push(Step::Message {
            from,
            to,
            text: label_lines(text).join(" "),
            dotted,
            head,
        });
    }
    if names.is_empty() {
        return Err("the sequence diagram has no participants".into());
    }

    // Lifeline centers: far enough apart for the boxes and every label.
    let widths: Vec<usize> = names.iter().map(|(_, label)| label.width() + 4).collect();
    let mut gaps: Vec<usize> = widths
        .windows(2)
        .map(|pair| pair[0].div_ceil(2) + pair[1] / 2 + 2)
        .collect();
    let mut needs: Vec<(usize, usize, usize)> = Vec::new();
    for step in &steps {
        match step {
            Step::Message { from, to, text, .. } if from != to => {
                needs.push(((*from).min(*to), (*from).max(*to), text.width() + 4));
            }
            Step::Message { from, text, .. } if *from + 1 < names.len() => {
                needs.push((*from, from + 1, text.width() + 6));
            }
            Step::Note {
                first,
                side: 1,
                text,
                ..
            } if first + 1 < names.len() => {
                needs.push((*first, first + 1, text.width() + 7));
            }
            Step::Note {
                first,
                side: -1,
                text,
                ..
            } if *first > 0 => {
                needs.push((first - 1, *first, text.width() + 7));
            }
            _ => {}
        }
    }
    needs.sort_by_key(|&(a, b, _)| b - a);
    for (a, b, need) in needs {
        let have: usize = gaps[a..b].iter().sum();
        if have < need {
            gaps[b - 1] += need - have;
        }
    }
    let margin = steps
        .iter()
        .filter_map(|step| match step {
            Step::Note {
                first: 0,
                side: -1,
                text,
                ..
            } => Some(text.width() + 7),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .max(widths[0] / 2);
    let mut centers = vec![margin];
    for gap in &gaps {
        centers.push(centers.last().copied().unwrap_or(0) + gap);
    }

    let mut canvas = Canvas::default();
    let boxes = |canvas: &mut Canvas, y: usize, joint: char, row: usize| {
        for ((center, width), (_, label)) in centers.iter().zip(&widths).zip(&names) {
            let x = center - width / 2;
            canvas.frame(
                x,
                y,
                *width,
                3,
                Shape::Square.corners(),
                std::slice::from_ref(label),
            );
            canvas.put(*center, y + row, joint);
        }
    };
    let mut y = 3;
    let mut marks: Vec<(usize, usize, char)> = Vec::new();
    let mut texts: Vec<(usize, usize, String)> = Vec::new();
    let mut frames: Vec<(usize, usize, usize, String)> = Vec::new();
    for step in &steps {
        match step {
            Step::Message {
                from,
                to,
                text,
                dotted,
                head,
            } if from != to => {
                let style = if *dotted { DOTTED } else { 0 };
                let (a, b) = (centers[*from], centers[*to]);
                if !text.is_empty() {
                    let (lo, hi) = (a.min(b), a.max(b));
                    let x = lo + (hi - lo).saturating_sub(text.width()) / 2;
                    texts.push((x, y, text.clone()));
                    y += 1;
                }
                if a < b {
                    canvas.hline(y, a, b - 1, style);
                    marks.push((b - 1, y, *head));
                } else {
                    let head = if *head == '▶' {
                        '◀'
                    } else if *head == '▷' {
                        '◁'
                    } else {
                        *head
                    };
                    canvas.hline(y, b + 1, a, style);
                    marks.push((b + 1, y, head));
                }
                y += 1;
            }
            Step::Message {
                from, text, dotted, ..
            } => {
                let style = if *dotted { DOTTED } else { 0 };
                let c = centers[*from];
                canvas.hline(y, c, c + 2, style);
                canvas.vline(c + 2, y, y + 1, style);
                canvas.hline(y + 1, c + 1, c + 2, style);
                marks.push((c + 1, y + 1, '◀'));
                texts.push((c + 4, y, text.clone()));
                y += 2;
            }
            Step::Note {
                first,
                last,
                side,
                text,
            } => {
                let width = text.width() + 4;
                let x = match side {
                    1 => centers[*first] + 2,
                    -1 => centers[*first].saturating_sub(width + 1),
                    _ => {
                        let mid = (centers[*first] + centers[*last]) / 2;
                        let span = centers[*last] - centers[*first] + 4;
                        let width = width.max(span);
                        mid.saturating_sub(width / 2)
                    }
                };
                let width = if *side == 0 {
                    width.max(centers[*last] - centers[*first] + 4)
                } else {
                    width
                };
                frames.push((x, y, width, text.clone()));
                y += 3;
            }
            Step::Block(text) => {
                texts.push((0, y, text.clone()));
                y += 1;
            }
        }
    }
    for &center in &centers {
        canvas.vline(center, 3, y, 0);
    }
    boxes(&mut canvas, 0, '┬', 2);
    boxes(&mut canvas, y + 1, '┴', 0);
    for (x, y, ch) in marks {
        canvas.put(x, y, ch);
    }
    for (x, y, text) in texts {
        canvas.text(x, y, &text);
    }
    for (x, y, width, text) in frames {
        canvas.frame(
            x,
            y,
            width,
            3,
            Shape::Square.corners(),
            std::slice::from_ref(&text),
        );
    }
    Ok(canvas.rows())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn a_top_down_flowchart_stacks_its_layers() {
        let rows = draw("graph TD\n  A[Start] --> B{Ok?}\n  B -->|yes| C(Done)\n  B -- no --> D\n")
            .unwrap();
        let text = show(&rows);
        assert!(text.contains("│ Start │"), "{text}");
        assert!(text.contains("╱─────╲"), "{text}");
        assert!(text.contains("yes"), "{text}");
        assert!(text.contains("no"), "{text}");
        assert!(text.contains('▼'), "{text}");
        let start = rows.iter().position(|r| r.contains("Start")).unwrap();
        let ok = rows.iter().position(|r| r.contains("Ok?")).unwrap();
        let done = rows.iter().position(|r| r.contains("Done")).unwrap();
        assert!(start < ok && ok < done, "{text}");
        assert_eq!(
            rows.iter().position(|r| r.contains("│ D │")),
            Some(done),
            "{text}"
        );
    }

    #[test]
    fn a_left_to_right_flowchart_reads_across() {
        let rows = draw("flowchart LR\n  a[One] --> b[Two] --> c[Three]\n").unwrap();
        let text = show(&rows);
        let row = rows.iter().find(|r| r.contains("One")).unwrap();
        let (one, two, three) = (
            row.find("One").unwrap(),
            row.find("Two").unwrap(),
            row.find("Three").unwrap(),
        );
        assert!(one < two && two < three, "{text}");
        assert!(row.contains("▶"), "{text}");
    }

    #[test]
    fn cycles_and_long_edges_still_draw() {
        let rows = draw("graph TD; A-->B; B-->C; C-.->A; A==>C").unwrap();
        let text = show(&rows);
        assert!(text.contains('▲'), "{text}");
        assert!(text.contains('┆') || text.contains('┄'), "{text}");
        assert!(text.contains('┃'), "{text}");
    }

    #[test]
    fn sequence_diagrams_draw_lifelines_and_messages() {
        let source = "sequenceDiagram\n  participant A as Alice\n  actor B as Bob\n  A->>B: Hello Bob\n  B-->>A: Hi\n  Note right of B: thinks\n  A->>A: self\n";
        let rows = draw(source).unwrap();
        let text = show(&rows);
        assert!(text.contains("│ Alice │"), "{text}");
        assert!(text.contains("Hello Bob"), "{text}");
        let hello = rows.iter().position(|r| r.contains("Hello Bob")).unwrap();
        assert!(rows[hello + 1].contains("──▶"), "{text}");
        let hi = rows.iter().position(|r| r.contains("Hi")).unwrap();
        assert!(rows[hi + 1].contains("◀┄"), "{text}");
        assert!(text.contains("thinks"), "{text}");
        assert_eq!(
            rows.iter().filter(|r| r.contains("Alice")).count(),
            2,
            "{text}"
        );
    }

    #[test]
    fn other_diagrams_are_refused() {
        assert!(draw("pie title Pets\n  \"Dogs\" : 3\n").is_err());
        assert!(draw("").is_err());
        assert!(draw("graph TD\n  A --> [").is_err());
    }
}
