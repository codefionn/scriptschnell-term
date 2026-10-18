//! The machine monitor panel: load per core, memory, swap, network traffic
//! and the worktree's disk, for the machine a project lives on, with graphs
//! of the CPU's load and the network's traffic over the last minutes.
//!
//! A sampler thread runs one short `sh` script through the project's
//! [`Environment`] every second and parses Linux's `/proc` from its output,
//! so an SSH or remote project shows its own machine. It only samples while
//! the panel is drawn: the grid's revision poll marks the panel seen, and
//! the thread of a hidden panel sleeps without starting anything.
//!
//! The panel draws into one terminal grid. Each row remembers a hash of the
//! rounded values it shows and is rewritten only when they change, so a
//! sample that moves one core's percentage rewrites one row, and an idle
//! frame rewrites none and allocates nothing.

use std::cell::{Cell, RefCell};
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::{Duration, Instant};

use crate::term::Wake;
use cmdterm_env::Environment;
use schnellui::raw_keyboard::{Key, NamedKey};
use schnellui::scene::Color;
use schnellui::widgets::{
    TerminalCell, TerminalCellAttrs, TerminalCellMetrics, TerminalCellWidth, TerminalGrid,
    TerminalGridPatch,
};
use schnellui::{
    FocusedInputEvent, FocusedInputResult, RawInputState, RawKeyEvent, RawPointerAction,
    RawPointerButton, RawPointerEvent, RawWheelDelta,
};

use crate::files::{digits, put, put_until};
use crate::i18n::Strings;
use crate::terminal_panel::GridStyle;
use crate::ui::ReaderColors;
use crate::workspace::MAX_PANES;

/// Accessible names of the monitor panel grids, by pane position, like
/// [`crate::terminal_panel::TERMINAL_NAMES`].
pub const MONITOR_NAMES: [&str; MAX_PANES] = [
    "Monitor",
    "Monitor (pane 2)",
    "Monitor (pane 3)",
    "Monitor (pane 4)",
    "Monitor (pane 5)",
    "Monitor (pane 6)",
    "Monitor (pane 7)",
    "Monitor (pane 8)",
];

/// Time between two samples.
const INTERVAL: Duration = Duration::from_secs(1);
/// Time between the two readings of a first sample, which needs a reading
/// to measure the load against.
const BASELINE: Duration = Duration::from_millis(400);
/// Time before the next try after a failed reading.
const RETRY: Duration = Duration::from_secs(5);
/// A reading older than this is no base for a load that means anything now.
const STALE: Duration = Duration::from_secs(3);
const WHEEL_LINES: f32 = 3.0;
/// Narrowest column of the per-core grid.
const CORE_WIDTH: usize = 22;
const MAX_CORE_COLUMNS: usize = 8;
/// Columns right of a gauge's bar for its numbers.
const GAUGE_TEXT: usize = 28;
/// Narrowest bar a gauge draws; narrower panels show only the numbers.
const MIN_BAR: usize = 4;
/// Partial bar cells, from one eighth to seven.
const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
/// Lower blocks in eighths of a cell, for the graphs.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Samples the graphs remember: four minutes at one a second, wider than
/// any graph a window leaves room for.
const HISTORY: usize = 240;
/// Rows of the CPU graph and of the network graph.
const CPU_GRAPH_ROWS: usize = 5;
const NET_GRAPH_ROWS: usize = 3;
/// The graphs grow into free rows up to these heights.
const CPU_GRAPH_MAX: usize = 16;
const NET_GRAPH_MAX: usize = 10;
/// The least the network graph scales to, so an idle link stays flat.
const NET_FLOOR: u64 = 1024;

/// The whole CPU's load and the network's bytes per second of the last
/// [`HISTORY`] samples, in a ring. Fixed size: a sample allocates nothing.
#[derive(Clone)]
struct History {
    cpu: [u16; HISTORY],
    /// Bytes a second in both directions.
    net: [u64; HISTORY],
    /// The received part of `net`.
    down: [u64; HISTORY],
    len: usize,
    next: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            cpu: [0; HISTORY],
            net: [0; HISTORY],
            down: [0; HISTORY],
            len: 0,
            next: 0,
        }
    }
}

impl History {
    fn push(&mut self, cpu: u16, down: u64, up: u64) {
        self.cpu[self.next] = cpu;
        self.net[self.next] = down.saturating_add(up);
        self.down[self.next] = down;
        self.next = (self.next + 1) % HISTORY;
        self.len = (self.len + 1).min(HISTORY);
    }

    /// The newest `count` samples or fewer, oldest first.
    fn last(&self, count: usize) -> impl Iterator<Item = (u16, u64, u64)> + '_ {
        let count = count.min(self.len);
        let start = (self.next + HISTORY - count) % HISTORY;
        (0..count).map(move |i| {
            let index = (start + i) % HISTORY;
            (self.cpu[index], self.net[index], self.down[index])
        })
    }
}

/// Which graph a graph row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Graph {
    Cpu,
    Net,
}

/// Prints the hostname, uptime and load average, then one `@` marked
/// section per source. `grep` keeps `/proc/stat`'s long interrupt lines off
/// the wire.
const SCRIPT: &str = "cat /proc/sys/kernel/hostname /proc/uptime /proc/loadavg; \
echo @stat; grep '^cpu' /proc/stat; \
echo @mem; grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree):' /proc/meminfo; \
echo @net; cat /proc/net/dev; \
echo @df; df -Pk .";

/// One parse of the script's output: counters as the machine reports them.
#[derive(Clone, Debug, Default, PartialEq)]
struct Reading {
    host: String,
    uptime: u64,
    load: [f32; 3],
    /// Running and total tasks.
    tasks: (u32, u32),
    /// Busy and total jiffies: the whole machine first, then each core.
    cpu: Vec<(u64, u64)>,
    /// Bytes.
    mem_total: u64,
    mem_available: u64,
    swap_total: u64,
    swap_free: u64,
    /// Bytes received and sent over every interface but loopback.
    net: Option<(u64, u64)>,
    disk: Option<Disk>,
}

/// The file system that holds the worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Disk {
    total: u64,
    used: u64,
    mount: String,
}

/// What the panel shows: two readings turned into rates and shares.
#[derive(Clone, Debug, PartialEq)]
struct Sample {
    host: String,
    uptime: u64,
    load: [f32; 3],
    tasks: (u32, u32),
    /// Load in per mille, the whole machine first, then each core.
    cpu: Vec<u16>,
    mem_total: u64,
    mem_used: u64,
    swap_total: u64,
    swap_used: u64,
    /// Bytes received and sent per second.
    net: Option<(u64, u64)>,
    disk: Option<Disk>,
}

impl Sample {
    fn cores(&self) -> &[u16] {
        self.cpu.get(1..).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Failure {
    /// The script ran, but the machine has no `/proc`, as on macOS.
    NoProc,
    /// The script did not run, such as when an SSH host is unreachable.
    Error(String),
}

/// What the sampler thread and the UI thread share.
#[derive(Default)]
struct Shared {
    /// The newest result; the UI takes it.
    latest: Mutex<Option<Result<Sample, Failure>>>,
    /// Bumped with every result, so the grid patches on the next frame.
    revision: AtomicU64,
    /// Set by every frame that shows the panel, cleared by the sampler.
    seen: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    fn send(&self, result: Result<Sample, Failure>, wake: &Wake) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(result);
        }
        self.revision.fetch_add(1, Ordering::Release);
        wake();
    }
}

fn sampler(env: Environment, root: PathBuf, shared: Arc<Shared>, wake: Wake) {
    let mut last: Option<(Reading, Instant)> = None;
    while !shared.stop.load(Ordering::Acquire) {
        if !shared.seen.swap(false, Ordering::AcqRel) {
            std::thread::park_timeout(INTERVAL);
            continue;
        }
        if last.as_ref().is_none_or(|(_, at)| at.elapsed() > STALE) {
            match read(&env, &root) {
                Ok(reading) => last = Some((reading, Instant::now())),
                Err(failure) => {
                    shared.send(Err(failure), &wake);
                    std::thread::park_timeout(RETRY);
                    continue;
                }
            }
            std::thread::park_timeout(BASELINE);
            if shared.stop.load(Ordering::Acquire) {
                break;
            }
        }
        match read(&env, &root) {
            Ok(reading) => {
                let at = Instant::now();
                if let Some((previous, then)) = &last {
                    shared.send(Ok(sample(previous, &reading, at - *then)), &wake);
                }
                last = Some((reading, at));
                std::thread::park_timeout(INTERVAL);
            }
            Err(failure) => {
                last = None;
                shared.send(Err(failure), &wake);
                std::thread::park_timeout(RETRY);
            }
        }
    }
}

fn read(env: &Environment, root: &Path) -> Result<Reading, Failure> {
    let output = env
        .command("sh")
        .arg("-c")
        .arg(SCRIPT)
        .cwd(root)
        .output()
        .map_err(|error| Failure::Error(error.to_string()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    match parse(&text) {
        Some(reading) => Ok(reading),
        None if text.contains("@stat") => Err(Failure::NoProc),
        None => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Err(Failure::Error(if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }))
        }
    }
}

/// Parses the output of [`SCRIPT`]. None without CPU counters.
fn parse(text: &str) -> Option<Reading> {
    let mut reading = Reading::default();
    let mut section = "";
    for line in text.lines() {
        if let Some(name) = line.strip_prefix('@') {
            section = name.trim();
            continue;
        }
        let mut fields = line.split_whitespace();
        match section {
            // The head: the hostname, `/proc/uptime` with two numbers and
            // `/proc/loadavg` with five, in any order a failing file leaves.
            "" => {
                let count = line.split_whitespace().count();
                if count == 5 && line.contains('/') {
                    for slot in &mut reading.load {
                        *slot = fields.next()?.parse().unwrap_or(0.0);
                    }
                    if let Some((running, total)) = fields.next().and_then(|f| f.split_once('/')) {
                        reading.tasks = (running.parse().unwrap_or(0), total.parse().unwrap_or(0));
                    }
                } else if count == 2
                    && let Some(Ok(uptime)) = fields.next().map(str::parse::<f64>)
                {
                    reading.uptime = uptime as u64;
                } else if count == 1 && reading.host.is_empty() {
                    reading.host = line.trim().to_owned();
                }
            }
            "stat" => {
                if fields.next().is_some_and(|name| name.starts_with("cpu")) {
                    let mut values = [0u64; 8];
                    for value in &mut values {
                        *value = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
                    }
                    // user nice system idle iowait irq softirq steal; guest
                    // time is already counted in user.
                    let total: u64 = values.iter().sum();
                    let idle = values[3] + values[4];
                    reading.cpu.push((total - idle, total));
                }
            }
            "mem" => {
                let (Some(name), Some(Ok(kib))) =
                    (fields.next(), fields.next().map(str::parse::<u64>))
                else {
                    continue;
                };
                let bytes = kib * 1024;
                match name {
                    "MemTotal:" => reading.mem_total = bytes,
                    "MemAvailable:" => reading.mem_available = bytes,
                    "SwapTotal:" => reading.swap_total = bytes,
                    "SwapFree:" => reading.swap_free = bytes,
                    _ => {}
                }
            }
            "net" => {
                let Some((name, counters)) = line.split_once(':') else {
                    continue;
                };
                if name.trim() == "lo" {
                    continue;
                }
                let counters: Vec<u64> = counters
                    .split_whitespace()
                    .map(|f| f.parse().unwrap_or(0))
                    .collect();
                if let (Some(rx), Some(tx)) = (counters.first(), counters.get(8)) {
                    let (all_rx, all_tx) = reading.net.get_or_insert((0, 0));
                    *all_rx += rx;
                    *all_tx += tx;
                }
            }
            "df" => {
                let columns: Vec<&str> = line.split_whitespace().collect();
                if columns.len() < 6 {
                    continue;
                }
                if let (Ok(total), Ok(used)) =
                    (columns[1].parse::<u64>(), columns[2].parse::<u64>())
                {
                    reading.disk = Some(Disk {
                        total: total * 1024,
                        used: used * 1024,
                        mount: columns[5..].join(" "),
                    });
                }
            }
            _ => {}
        }
    }
    (!reading.cpu.is_empty()).then_some(reading)
}

/// The sample `now` makes, `elapsed` after `previous`.
fn sample(previous: &Reading, now: &Reading, elapsed: Duration) -> Sample {
    let cpu = now
        .cpu
        .iter()
        .zip(&previous.cpu)
        .map(|(&(busy, total), &(was_busy, was_total))| {
            let total = total.saturating_sub(was_total);
            let busy = busy.saturating_sub(was_busy).min(total);
            (busy * 1000).checked_div(total).unwrap_or(0) as u16
        })
        .collect();
    let seconds = elapsed.as_secs_f64().max(0.001);
    let rate = |now: u64, was: u64| (now.saturating_sub(was) as f64 / seconds) as u64;
    Sample {
        host: now.host.clone(),
        uptime: now.uptime,
        load: now.load,
        tasks: now.tasks,
        cpu,
        mem_total: now.mem_total,
        mem_used: now.mem_total.saturating_sub(now.mem_available),
        swap_total: now.swap_total,
        swap_used: now.swap_total.saturating_sub(now.swap_free),
        net: now
            .net
            .zip(previous.net)
            .map(|((rx, tx), (was_rx, was_tx))| (rate(rx, was_rx), rate(tx, was_tx))),
        disk: now.disk.clone(),
    }
}

enum State {
    Waiting,
    Ready(Sample),
    /// A failure, already worded.
    Failed(String),
}

/// What one body line shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    Message,
    Blank,
    Cpu,
    /// Row `n` of the per-core grid.
    Cores(usize),
    Memory,
    Swap,
    Network,
    Disk,
    /// Row `n` of a graph, from the top.
    Graph(Graph, usize),
    /// The time axis under the graphs.
    Axis,
}

struct View {
    state: State,
    /// The environment's label, for the header until the hostname is known.
    label: String,
    cols: usize,
    rows: usize,
    /// First body line shown.
    top: usize,
    /// Bumped by every change that redraws all rows: a resize, a scroll.
    generation: u32,
    /// What the graphs draw.
    history: Box<History>,
}

impl View {
    fn body_rows(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    /// Columns of the per-core grid: as many as fit, or fewer that fill
    /// every row when that keeps at least half of them.
    fn core_columns(&self, cores: usize) -> usize {
        let fit = (self.cols.saturating_sub(1) / CORE_WIDTH)
            .clamp(1, MAX_CORE_COLUMNS)
            .min(cores.max(1));
        (fit.div_ceil(2)..=fit)
            .rev()
            .find(|columns| cores.is_multiple_of(*columns))
            .unwrap_or(fit)
    }

    fn core_rows(&self, cores: usize) -> usize {
        cores.div_ceil(self.core_columns(cores))
    }

    /// The lines after the cores. Allocation-free.
    fn tail(&self, sample: &Sample) -> impl Iterator<Item = Line> {
        let net = sample.net.is_some();
        let (cpu_rows, net_rows) = self.graph_rows(sample);
        let gap = |line: Option<Line>| line.into_iter().flat_map(|line| [Line::Blank, line]);
        [Line::Blank, Line::Memory]
            .into_iter()
            .chain(gap((sample.swap_total > 0).then_some(Line::Swap)))
            .chain(gap(sample.net.map(|_| Line::Network)))
            .chain(gap(sample.disk.as_ref().map(|_| Line::Disk)))
            .chain([Line::Blank])
            .chain((0..cpu_rows).map(|row| Line::Graph(Graph::Cpu, row)))
            .chain(net.then_some(Line::Blank))
            .chain((0..net_rows).map(|row| Line::Graph(Graph::Net, row)))
            .chain([Line::Axis])
    }

    /// Rows of the CPU and network graphs: their least heights, grown into
    /// the rows the panel has left.
    fn graph_rows(&self, sample: &Sample) -> (usize, usize) {
        let net = sample.net.is_some();
        let fixed = self.head(sample)
            + 2
            + 2 * usize::from(sample.swap_total > 0)
            + 2 * usize::from(net)
            + 2 * usize::from(sample.disk.is_some())
            + 1
            + usize::from(net)
            + 1;
        let spare = self.body_rows().saturating_sub(fixed);
        if !net {
            return (spare.clamp(CPU_GRAPH_ROWS, CPU_GRAPH_MAX), 0);
        }
        let cpu = (spare * 3 / 5).clamp(CPU_GRAPH_ROWS, CPU_GRAPH_MAX);
        let net = spare
            .saturating_sub(cpu)
            .clamp(NET_GRAPH_ROWS, NET_GRAPH_MAX);
        (cpu, net)
    }

    /// The lines up to the cores: a gap under the header, the whole CPU,
    /// then a gap and the cores when there are any.
    fn head(&self, sample: &Sample) -> usize {
        match self.core_rows(sample.cores().len()) {
            0 => 2,
            rows => 3 + rows,
        }
    }

    fn lines(&self) -> usize {
        match &self.state {
            State::Ready(sample) => self.head(sample) + self.tail(sample).count(),
            State::Waiting | State::Failed(_) => 1,
        }
    }

    fn line(&self, index: usize) -> Option<Line> {
        let State::Ready(sample) = &self.state else {
            return (index == 0).then_some(Line::Message);
        };
        let head = self.head(sample);
        match index {
            0 => Some(Line::Blank),
            1 => Some(Line::Cpu),
            2 if head > 2 => Some(Line::Blank),
            _ if index < head => Some(Line::Cores(index - 3)),
            _ => self.tail(sample).nth(index - head),
        }
    }

    fn scroll(&mut self, delta: isize) {
        let max = self.lines().saturating_sub(self.body_rows());
        let top = self.top.saturating_add_signed(delta).min(max);
        if top != self.top {
            self.top = top;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// What grid row `row` shows.
    fn row(&self, row: usize, tr: &'static Strings) -> Row<'_> {
        if row == 0 {
            let mut text = Buf::new();
            let _ = self.header(&mut text, tr);
            return Row::Header(text);
        }
        let Some(line) = self.line(self.top + row - 1) else {
            return Row::Blank;
        };
        let sample = match (&self.state, line) {
            (State::Waiting, _) => return Row::Message(tr.monitor_waiting),
            (State::Failed(message), _) => return Row::Message(message),
            (State::Ready(sample), _) => sample,
        };
        let mut text = Buf::new();
        let (label, fill) = match line {
            Line::Message | Line::Blank => return Row::Blank,
            Line::Axis => {
                return Row::Axis(tr.monitor_now);
            }
            Line::Cores(row) => return Row::Cores(row),
            Line::Graph(graph, row) => {
                // The top row names the graph and its scale.
                let label = match (graph, row) {
                    (Graph::Cpu, 0) => tr.monitor_cpu_history,
                    (Graph::Net, 0) => tr.monitor_net_history,
                    (Graph::Net, 2) => tr.monitor_net_down,
                    (Graph::Net, 3) => tr.monitor_net_up,
                    _ => "",
                };
                let (cpu_rows, net_rows) = self.graph_rows(sample);
                let rows = match graph {
                    Graph::Cpu => cpu_rows,
                    Graph::Net => net_rows,
                };
                // The scale: its top on the first row, half way for the
                // CPU, and zero on the last.
                if row + 1 == rows {
                    let _ = text.write_str("0");
                } else if graph == Graph::Cpu && row == rows / 2 && rows > 4 {
                    let _ = text.write_str("50%");
                }
                if row == 0 {
                    let _ = match graph {
                        Graph::Cpu => text.write_str("100%"),
                        Graph::Net => {
                            let _ = write!(text, "{} ", tr.monitor_peak);
                            let _ = bytes(&mut text, self.net_peak());
                            text.write_str("/s")
                        }
                    };
                }
                // Before the second sample the CPU graph says it is
                // collecting, in its middle row.
                let note = if graph == Graph::Cpu && row == rows / 2 && self.history.len < 2 {
                    tr.monitor_collecting
                } else {
                    ""
                };
                return Row::Graph {
                    graph,
                    row,
                    rows,
                    label,
                    text,
                    note,
                };
            }
            Line::Cpu => {
                let percent = percent(sample.cpu[0]);
                // Not padded: the text column starts where the memory and
                // disk texts start.
                let _ = write!(text, "{percent}%");
                (tr.monitor_cpu, Some(percent * 10))
            }
            Line::Memory => {
                let _ = used_of(&mut text, sample.mem_used, sample.mem_total);
                (
                    tr.monitor_memory,
                    Some(share(sample.mem_used, sample.mem_total)),
                )
            }
            Line::Swap => {
                let _ = used_of(&mut text, sample.swap_used, sample.swap_total);
                (
                    tr.monitor_swap,
                    Some(share(sample.swap_used, sample.swap_total)),
                )
            }
            Line::Network => {
                let (rx, tx) = sample.net.unwrap_or_default();
                let _ = text.write_str("↓ ");
                let _ = bytes(&mut text, rx);
                let _ = text.write_str("/s  ↑ ");
                let _ = bytes(&mut text, tx);
                let _ = text.write_str("/s");
                (tr.monitor_network, None)
            }
            Line::Disk => {
                let Some(disk) = &sample.disk else {
                    return Row::Blank;
                };
                let _ = used_of(&mut text, disk.used, disk.total);
                let _ = write!(text, " ({})", disk.mount);
                (tr.monitor_disk, Some(share(disk.used, disk.total)))
            }
        };
        Row::Gauge { label, fill, text }
    }

    /// `host · up 3d 04:12 · load 0.52 0.61 0.70 · 1234 tasks, 2 running`.
    fn header(&self, out: &mut Buf, tr: &Strings) -> fmt::Result {
        let State::Ready(sample) = &self.state else {
            return write!(out, "{} · {}", tr.monitor, self.label);
        };
        let host = if sample.host.is_empty() {
            &self.label
        } else {
            &sample.host
        };
        let (days, hours, minutes) = (
            sample.uptime / 86_400,
            sample.uptime / 3_600 % 24,
            sample.uptime / 60 % 60,
        );
        write!(out, "{host} · {} ", tr.monitor_up)?;
        if days > 0 {
            write!(out, "{days}{} ", tr.monitor_days)?;
        }
        let [one, five, fifteen] = sample.load;
        write!(
            out,
            "{hours:02}:{minutes:02} · {} {one:.2} {five:.2} {fifteen:.2} · {} {}, {} {}",
            tr.monitor_load, sample.tasks.1, tr.monitor_tasks, sample.tasks.0, tr.monitor_running,
        )
    }

    /// A hash of everything row `row` shows. Allocation-free.
    fn key(&self, row: &Row<'_>) -> u64 {
        let mut key = Hash::new();
        key.add(u64::from(self.generation));
        match row {
            Row::Blank => key.add(1),
            Row::Header(text) => {
                key.add(2);
                key.add_str(text.as_str());
            }
            Row::Message(text) => {
                key.add(3);
                key.add_str(text);
            }
            Row::Gauge { label, fill, text } => {
                key.add(4);
                key.add_str(label);
                key.add(fill.map_or(u64::MAX, u64::from));
                key.add_str(text.as_str());
            }
            Row::Axis(now) => {
                key.add(7);
                key.add_str(now);
                // Ticks appear as the history fills.
                key.add(self.history.len as u64);
                key.add(self.cols as u64);
            }
            Row::Graph {
                graph,
                row,
                rows,
                label,
                text,
                note,
            } => {
                key.add(6);
                key.add_str(note);
                key.add(*graph as u64);
                key.add(*row as u64);
                key.add(*rows as u64);
                key.add_str(label);
                key.add_str(text.as_str());
                key.add(self.history.next as u64);
                key.add(self.history.len as u64);
            }
            Row::Cores(row) => {
                key.add(5);
                key.add(*row as u64);
                if let State::Ready(sample) = &self.state {
                    for &load in self.cores_in_row(sample, *row) {
                        key.add(u64::from(percent(load)));
                    }
                }
            }
        }
        // A zero key marks a row never drawn.
        key.0 | 1
    }

    /// The top of the network graph's scale: its busiest second.
    fn net_peak(&self) -> u64 {
        self.history
            .last(HISTORY)
            .map(|(_, net, _)| net)
            .max()
            .unwrap_or(0)
            .max(NET_FLOOR)
    }

    /// The cores row `row` of the grid shows, numbered left to right.
    fn cores_in_row<'a>(&self, sample: &'a Sample, row: usize) -> impl Iterator<Item = &'a u16> {
        // Column-major: each block reads down in order, 0 1 2 3, then the
        // next block starts where it ended.
        let rows = self.core_rows(sample.cores().len());
        sample.cores().iter().skip(row).step_by(rows.max(1))
    }
}

/// What one grid row shows, worked out once for its key and its drawing.
enum Row<'a> {
    Blank,
    Header(Buf),
    Message(&'a str),
    Gauge {
        label: &'a str,
        /// Per mille; None draws no bar. Bars take the CPU's colors:
        /// green, brass above 70 %, red above 90 %.
        fill: Option<u16>,
        text: Buf,
    },
    Cores(usize),
    /// The time axis, with the word for its right end.
    Axis(&'a str),
    Graph {
        graph: Graph,
        row: usize,
        /// Rows of the whole graph.
        rows: usize,
        label: &'a str,
        /// The scale, on the top row.
        text: Buf,
        /// Text drawn across the middle of the graph, or empty.
        note: &'a str,
    },
}

pub struct MonitorPanel {
    shared: Arc<Shared>,
    sampler: Option<Thread>,
    view: RefCell<View>,
    /// The key of what each grid row shows; zero for rows not drawn yet.
    keys: RefCell<Vec<u64>>,
    /// Bumped for changes made on the UI thread, such as a scroll.
    view_revision: Cell<u64>,
    tr: Cell<&'static Strings>,
    /// The key of the rows that carry a label, as the page mounted them.
    /// The labels are widgets over the grid, in the UI font, so a change
    /// in where they sit mounts the page again.
    label_key: Cell<u64>,
}

impl Drop for MonitorPanel {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = &self.sampler {
            thread.unpark();
        }
    }
}

impl MonitorPanel {
    /// A panel for the machine of `env`. The disk row shows the file system
    /// that holds `root`.
    pub fn new(env: Environment, root: PathBuf, wake: Wake, tr: &'static Strings) -> Rc<Self> {
        let shared = Arc::new(Shared {
            seen: AtomicBool::new(true),
            ..Shared::default()
        });
        let label = env.label().to_owned();
        let sampler = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("cmdterm-monitor".to_owned())
                .spawn(move || sampler(env, root, shared, wake))
                .ok()
                .map(|handle| handle.thread().clone())
        };
        Rc::new(Self {
            shared,
            sampler,
            view: RefCell::new(View {
                state: State::Waiting,
                label,
                cols: 80,
                rows: 24,
                top: 0,
                generation: 0,
                history: Box::default(),
            }),
            keys: RefCell::new(Vec::new()),
            view_revision: Cell::new(0),
            tr: Cell::new(tr),
            label_key: Cell::new(0),
        })
    }

    /// Tab label.
    pub fn title(&self) -> String {
        self.tr.get().monitor.to_owned()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut view = self.view.borrow_mut();
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        if (view.cols, view.rows) != (cols, rows) {
            view.cols = cols;
            view.rows = rows;
            view.scroll(0);
            view.generation = view.generation.wrapping_add(1);
            drop(view);
            self.touch();
        }
    }

    fn touch(&self) {
        self.view_revision
            .set(self.view_revision.get().wrapping_add(1));
    }

    /// Changes with every sample and every scroll.
    pub fn revision(&self) -> u64 {
        self.shared
            .revision
            .load(Ordering::Acquire)
            .wrapping_add(self.view_revision.get())
    }

    /// Takes the newest result of the sampler. Allocation-free when there is
    /// none, and when it is a sample.
    fn receive(&self, view: &mut View) {
        let Some(result) = self
            .shared
            .latest
            .lock()
            .ok()
            .and_then(|mut latest| latest.take())
        else {
            return;
        };
        let tr = self.tr.get();
        view.state = match result {
            Ok(sample) => {
                let (rx, tx) = sample.net.unwrap_or_default();
                view.history.push(sample.cpu[0], rx, tx);
                State::Ready(sample)
            }
            Err(Failure::NoProc) => State::Failed(tr.monitor_no_proc.to_owned()),
            Err(Failure::Error(error)) => State::Failed((tr.monitor_failed)(&error)),
        };
        view.scroll(0);
    }

    /// The grid rows that carry a label and the label of each, for the
    /// widgets the page lays over the grid. Records where they sit, so
    /// [`Self::take_remount`] can tell when they move.
    pub fn labels(&self, tr: &'static Strings) -> Vec<(usize, &'static str)> {
        let mut view = self.view.borrow_mut();
        self.receive(&mut view);
        self.label_key.set(label_key(&view));
        (0..view.rows.max(1))
            .filter_map(|index| row_label(&view, index, tr).map(|label| (index, label)))
            .collect()
    }

    /// Whether the labels moved since the page mounted, after taking the
    /// newest sample. Allocation-free.
    pub fn take_remount(&self) -> bool {
        let mut view = self.view.borrow_mut();
        self.receive(&mut view);
        let key = label_key(&view);
        key != self.label_key.replace(key)
    }

    pub fn view(
        self: &Rc<Self>,
        style: &GridStyle,
        colors: ReaderColors,
        tr: &'static Strings,
        label: &str,
    ) -> TerminalGrid {
        self.tr.set(tr);
        let revision = self.clone();
        let patch = self.clone();
        let grid = TerminalGrid::patched(
            move || {
                // Every frame that shows the panel polls this, which is what
                // keeps the sampler running.
                revision.shared.seen.store(true, Ordering::Release);
                revision.revision()
            },
            move |grid| patch.draw(grid, &colors),
        )
        .metrics(style.metrics)
        .label(label);
        match &style.font_family {
            Some(family) => grid.font_family(family.clone()),
            None => grid,
        }
    }

    fn draw(&self, grid: &mut TerminalGridPatch<'_>, colors: &ReaderColors) {
        let mut view = self.view.borrow_mut();
        self.receive(&mut view);
        let view = &*view;
        let mut keys = self.keys.borrow_mut();
        let (cols, rows) = (view.cols.max(1), view.rows.max(1));
        let model = grid.model();
        if model.columns != cols || model.rows != rows {
            grid.resize(cols, rows);
            keys.clear();
        }
        let model = grid.model();
        if model.foreground != colors.ink || model.background != colors.page {
            grid.set_colors(colors.ink, colors.page);
            keys.clear();
        }
        keys.resize(rows, 0);
        let tr = self.tr.get();
        let labels = label_width(tr);
        for (index, shown) in keys.iter_mut().enumerate() {
            let row = view.row(index, tr);
            let key = view.key(&row);
            if *shown == key {
                continue;
            }
            *shown = key;
            draw_row(grid.row_mut(index), &row, view, labels, colors);
        }
        grid.set_cursor(None);
    }

    /// Handles one focused input event: scrolling, for machines with more
    /// cores than rows.
    pub fn input(
        &self,
        event: FocusedInputEvent,
        metrics: TerminalCellMetrics,
    ) -> FocusedInputResult {
        let handled = match event {
            FocusedInputEvent::Key(key) => self.key(&key),
            FocusedInputEvent::Pointer(pointer) => self.pointer(pointer, metrics),
            FocusedInputEvent::Focus(_) | FocusedInputEvent::Ime(_) => true,
            FocusedInputEvent::Clipboard(_) => false,
        };
        if handled {
            FocusedInputResult::Handled
        } else {
            FocusedInputResult::Ignored
        }
    }

    fn key(&self, key: &RawKeyEvent) -> bool {
        if key.state == RawInputState::Released {
            return true;
        }
        let mods = key.modifiers;
        if mods.control || mods.alt || mods.super_key {
            return false;
        }
        let mut view = self.view.borrow_mut();
        let page = view.body_rows().saturating_sub(1).max(1) as isize;
        let delta = match &key.logical_key {
            Key::Named(NamedKey::ArrowUp) => -1,
            Key::Named(NamedKey::ArrowDown) => 1,
            Key::Named(NamedKey::PageUp) => -page,
            Key::Named(NamedKey::PageDown | NamedKey::Space) => page,
            Key::Named(NamedKey::Home) => isize::MIN / 2,
            Key::Named(NamedKey::End) => isize::MAX / 2,
            Key::Character(text) => match text.as_str() {
                "k" => -1,
                "j" => 1,
                "g" => isize::MIN / 2,
                "G" => isize::MAX / 2,
                " " => page,
                _ => return false,
            },
            _ => return false,
        };
        view.scroll(delta);
        drop(view);
        self.touch();
        true
    }

    fn pointer(&self, pointer: RawPointerEvent, metrics: TerminalCellMetrics) -> bool {
        match pointer.action {
            RawPointerAction::Wheel(delta) => {
                let lines = match delta {
                    RawWheelDelta::Lines { y, .. } => y * WHEEL_LINES,
                    RawWheelDelta::Pixels { y, .. } => y / metrics.height,
                };
                self.view.borrow_mut().scroll(-(lines.round() as isize));
                self.touch();
                true
            }
            RawPointerAction::Button {
                button: RawPointerButton::Left,
                ..
            } => true,
            _ => false,
        }
    }
}

/// Columns of the gauge labels, with a gap after the longest.
/// The label grid row `index` shows: a gauge's, or a graph's on its top
/// row.
fn row_label(view: &View, index: usize, tr: &'static Strings) -> Option<&'static str> {
    if index == 0 || !matches!(view.state, State::Ready(_)) {
        return None;
    }
    Some(match view.line(view.top + index - 1)? {
        Line::Cpu => tr.monitor_cpu,
        Line::Memory => tr.monitor_memory,
        Line::Swap => tr.monitor_swap,
        Line::Network => tr.monitor_network,
        Line::Disk => tr.monitor_disk,
        Line::Graph(Graph::Cpu, 0) => tr.monitor_cpu_history,
        Line::Graph(Graph::Net, 0) => tr.monitor_net_history,
        _ => return None,
    })
}

/// A key of which rows carry which label.
fn label_key(view: &View) -> u64 {
    let mut key = Hash::new();
    key.add(view.rows as u64);
    key.add(view.top as u64);
    key.add(u64::from(matches!(view.state, State::Ready(_))));
    if let State::Ready(_) = view.state {
        for index in 1..view.rows.max(1) {
            let tag = match view.line(view.top + index - 1) {
                Some(Line::Cpu) => 1,
                Some(Line::Memory) => 2,
                Some(Line::Swap) => 3,
                Some(Line::Network) => 4,
                Some(Line::Disk) => 5,
                Some(Line::Graph(Graph::Cpu, 0)) => 6,
                Some(Line::Graph(Graph::Net, 0)) => 7,
                _ => continue,
            };
            key.add(index as u64);
            key.add(tag);
        }
    }
    key.0
}

fn label_width(tr: &Strings) -> usize {
    [
        tr.monitor_cpu,
        tr.monitor_memory,
        tr.monitor_swap,
        tr.monitor_network,
        tr.monitor_disk,
        tr.monitor_cpu_history,
        tr.monitor_net_history,
    ]
    .iter()
    .map(|label| label.chars().count())
    .max()
    .unwrap_or(0)
        + 2
}

const PLAIN: TerminalCellAttrs = TerminalCellAttrs {
    bold: false,
    italic: false,
    underline: false,
    strike: false,
    faint: false,
    inverse: false,
};

fn draw_row(
    cells: &mut [TerminalCell],
    row: &Row<'_>,
    view: &View,
    labels: usize,
    colors: &ReaderColors,
) {
    let background = match row {
        Row::Header(_) => colors.header,
        _ => colors.page,
    };
    for cell in cells.iter_mut() {
        cell.grapheme.clear();
        cell.foreground = colors.ink;
        cell.background = background;
        cell.attrs = PLAIN;
        cell.width = TerminalCellWidth::Single;
        cell.hyperlink = None;
    }
    let end = cells.len().saturating_sub(1);
    match row {
        Row::Blank => {}
        Row::Header(text) => put(cells, 1, text.as_str(), colors.ink, PLAIN),
        Row::Message(text) => put(cells, 1, text, colors.muted, PLAIN),
        // The label is a widget over the grid; see `MonitorPanel::labels`.
        Row::Gauge { fill, text, .. } => {
            let x = 1 + labels;
            let bar = end.saturating_sub(x + GAUGE_TEXT + 1);
            match fill {
                Some(fill) if bar >= MIN_BAR => {
                    draw_bar(
                        &mut cells[x..x + bar],
                        *fill,
                        // Disks share the CPU legend's thresholds, so the
                        // one legend on the page reads true for every bar.
                        level_color(*fill, colors),
                        colors,
                    );
                    put_until(cells, x + bar + 1, end, text.as_str(), colors.ink, PLAIN);
                }
                _ => {
                    put_until(cells, x, end, text.as_str(), colors.ink, PLAIN);
                }
            }
        }
        Row::Axis(now) => {
            // One sample a second, the newest at the right edge.
            let x = 1 + labels;
            let width = end.saturating_sub(x + GAUGE_TEXT + 1);
            if width >= MIN_BAR {
                if view.history.len < 2 {
                    return;
                }
                // Round numbers of seconds back, each centered on the
                // column of its sample; the newest sample is at the right
                // edge. At most five, so they never crowd.
                let per = sample_width(width);
                let span = width / per;
                let step = [10, 15, 30, 60, 120]
                    .into_iter()
                    .find(|&step| span / step <= 5)
                    .unwrap_or(300);
                let room = (x + width).saturating_sub(now.chars().count() + 1);
                let mut back = step;
                while back <= span {
                    let mut text = Buf::new();
                    let _ = write!(text, "-{back} s");
                    let column = (x + width).saturating_sub(back * per);
                    let at = column.saturating_sub(text.as_str().len() / 2).max(x);
                    if at + text.as_str().len() < room {
                        put_until(cells, at, end, text.as_str(), colors.muted, PLAIN);
                    }
                    back += step;
                }
                let from = (x + width).saturating_sub(now.chars().count());
                put_until(cells, from, end, now, colors.muted, PLAIN);
            }
        }
        Row::Graph {
            graph,
            row,
            rows,
            label,
            text,
            note,
        } => {
            // What the CPU colors mean, under the graph's label.
            const LEGEND: [(u16, &str); 3] = [(0, "<70%"), (800, "70-90%"), (1000, ">90%")];
            if matches!(graph, Graph::Cpu)
                && *rows > LEGEND.len() + 1
                && let Some(&(fill, text)) = (*row).checked_sub(2).and_then(|at| LEGEND.get(at))
            {
                let at = put_until(cells, 1, labels, "■", level_color(fill, colors), PLAIN);
                put_until(cells, at + 1, labels, text, colors.muted, PLAIN);
            }
            // What the network colors mean, under the graph's label.
            if matches!(graph, Graph::Net) && !label.is_empty() && *row > 0 {
                let color = if *row == 2 { NET_DOWN } else { NET_UP };
                let at = put_until(cells, 1, labels, "■", color.of(colors), PLAIN);
                put_until(cells, at + 1, labels, label, colors.muted, PLAIN);
            }
            let x = 1 + labels;
            let width = end.saturating_sub(x + GAUGE_TEXT + 1);
            if width >= MIN_BAR {
                draw_graph(&mut cells[x..x + width], *graph, *row, *rows, view, colors);
                // The note sits in the middle of the columns with no
                // sample yet, and only while it fits there.
                let per = sample_width(width);
                let empty = width - (width / per).min(view.history.len) * per;
                let length = note.chars().count();
                if !note.is_empty() && length + 2 <= empty {
                    let from = x + (empty - length) / 2;
                    put_until(cells, from, x + empty, note, colors.muted, PLAIN);
                }
                put_until(
                    cells,
                    x + width + 1,
                    end,
                    text.as_str(),
                    colors.muted,
                    PLAIN,
                );
            }
        }
        Row::Cores(row) => {
            let State::Ready(sample) = &view.state else {
                return;
            };
            let cores = sample.cores().len();
            let columns = view.core_columns(cores);
            let width = end / columns;
            let digits = digits(cores.saturating_sub(1));
            for (column, &load) in view.cores_in_row(sample, *row).enumerate() {
                let core = column * view.core_rows(cores) + row;
                let x = 1 + column * width;
                let stop = (x + width).min(end);
                let mut text = Buf::new();
                let _ = write!(text, "{core:>digits$}");
                let after = put_until(cells, x, stop, text.as_str(), colors.muted, PLAIN);
                let percent = percent(load);
                // " 100%" and a gap of six before the next column, so a
                // percent never reads as the next core's number.
                let bar = width.saturating_sub(digits + 12);
                if bar >= MIN_BAR {
                    draw_core_bar(
                        &mut cells[after + 1..after + 1 + bar],
                        percent * 10,
                        level_color(percent * 10, colors),
                        colors,
                    );
                }
                let mut text = Buf::new();
                let _ = write!(text, "{percent:>4}%");
                let from = if bar >= MIN_BAR {
                    after + 1 + bar
                } else {
                    after
                };
                put_until(cells, from, stop, text.as_str(), colors.ink, PLAIN);
            }
        }
    }
}

/// Row `row` of `graph` across `cells`: the newest samples at the right
/// edge, each a column of lower blocks as tall as its value.
fn draw_graph(
    cells: &mut [TerminalCell],
    graph: Graph,
    row: usize,
    rows: usize,
    view: &View,
    colors: &ReaderColors,
) {
    // Faint lines at a quarter, half and three quarters of each graph, so
    // a bar's height reads against a scale; bars draw over them.
    let quarter = (1..4).any(|k| {
        let at = (k * rows * 2).div_ceil(4).saturating_sub(1) / 2;
        rows - 1 - at == row
    });
    let per = sample_width(cells.len());
    let count = (cells.len() / per).min(view.history.len);
    let start = cells.len() - count * per;
    // The frame spans the whole window from the first sample; the note in
    // the middle row says the empty part is still filling.
    for cell in cells.iter_mut() {
        cell.background = colors.unfocused;
        if quarter {
            cell.grapheme.clear();
            cell.grapheme.push('┈');
            // The divider color vanishes on the graph's raised fill; the
            // grip's step above it stays visible and still under the bars.
            cell.foreground = colors.grip;
        }
    }
    let peak = view.net_peak();
    // Eighths below this row's bottom edge.
    let below = (rows - 1 - row) * 8;
    for (sample, (cpu, net, down)) in cells[start..].chunks_mut(per).zip(view.history.last(count)) {
        let fill = match graph {
            Graph::Cpu => cpu.min(1000),
            Graph::Net => share(net, peak),
        };
        let height = (usize::from(fill) * rows * 8 + 500) / 1000;
        let level = height.saturating_sub(below).min(8);
        if level == 0 {
            continue;
        }
        // Received bytes fill the bottom of a network bar and sent ones
        // sit on top, each in its own color; a cell goes to whichever
        // direction fills most of it.
        let color = match graph {
            Graph::Cpu => level_color(fill, colors),
            Graph::Net => {
                let received = (usize::from(share(down, peak)) * rows * 8 + 500) / 1000;
                if received >= below + 4 {
                    NET_DOWN.of(colors)
                } else {
                    NET_UP.of(colors)
                }
            }
        };
        for cell in sample {
            cell.grapheme.clear();
            cell.grapheme.push(BLOCKS[level - 1]);
            cell.foreground = color;
        }
    }
}

/// The color of one direction in the network graph.
#[derive(Clone, Copy)]
enum NetColor {
    Down,
    Up,
}

const NET_DOWN: NetColor = NetColor::Down;
const NET_UP: NetColor = NetColor::Up;

impl NetColor {
    fn of(self, colors: &ReaderColors) -> Color {
        match self {
            Self::Down => colors.folder,
            // Not the brass of the CPU legend's 70-90 % band, and a hue
            // apart from the periwinkle of received bytes.
            Self::Up => colors.accent,
        }
    }
}

/// Columns one sample takes in a graph `width` columns wide, so a full
/// history spans the graph.
fn sample_width(width: usize) -> usize {
    width.div_ceil(HISTORY).max(1)
}

/// A bar across `cells`, `fill` per mille full, in eighths of a cell. Full
/// cells are painted as background, which leaves no seams between glyphs.
fn draw_bar(cells: &mut [TerminalCell], fill: u16, color: Color, colors: &ReaderColors) {
    let eighths = (usize::from(fill.min(1000)) * cells.len() * 8 + 500) / 1000;
    for (index, cell) in cells.iter_mut().enumerate() {
        match eighths.saturating_sub(index * 8).min(8) {
            8 => cell.background = color,
            0 => cell.background = colors.unfocused,
            n => {
                cell.background = colors.unfocused;
                cell.grapheme.clear();
                cell.grapheme.push(EIGHTHS[n - 1]);
                cell.foreground = color;
            }
        }
    }
}

/// A core's bar: lower seven-eighths blocks, so the page shows between the
/// rows of cores and four busy cores never merge into one slab. A cell is
/// filled when at least half of it is.
fn draw_core_bar(cells: &mut [TerminalCell], fill: u16, color: Color, colors: &ReaderColors) {
    let filled = (usize::from(fill.min(1000)) * cells.len() + 500) / 1000;
    for (index, cell) in cells.iter_mut().enumerate() {
        cell.background = colors.page;
        cell.grapheme.clear();
        cell.grapheme.push('▇');
        cell.foreground = if index < filled {
            color
        } else {
            colors.unfocused
        };
    }
}

/// Green while there is room, brass above 70 % and red above 90 %.
fn level_color(fill: u16, colors: &ReaderColors) -> Color {
    match fill {
        0..700 => colors.added,
        700..900 => colors.syntax[5],
        _ => colors.removed,
    }
}

fn percent(per_mille: u16) -> u16 {
    (per_mille + 5) / 10
}

/// `part` of `whole` in per mille.
fn share(part: u64, whole: u64) -> u16 {
    (u128::from(part) * 1000)
        .checked_div(u128::from(whole))
        .map_or(0, |share| share.min(1000) as u16)
}

/// `5.2 GiB / 15.6 GiB`.
fn used_of(out: &mut Buf, used: u64, total: u64) -> fmt::Result {
    bytes(out, used)?;
    out.write_str(" / ")?;
    bytes(out, total)
}

/// `n` bytes in binary units, with one decimal below 100.
fn bytes(out: &mut Buf, n: u64) -> fmt::Result {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => write!(out, "{n} B"),
        _ if value < 100.0 => write!(out, "{value:.1} {}", UNITS[unit]),
        _ => write!(out, "{value:.0} {}", UNITS[unit]),
    }
}

/// A short string on the stack, for formatting numbers while drawing.
struct Buf {
    bytes: [u8; 160],
    len: usize,
}

impl Buf {
    fn new() -> Self {
        Self {
            bytes: [0; 160],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }
}

impl fmt::Write for Buf {
    /// Writes all of `text` or nothing, so the buffer stays valid UTF-8.
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        target.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// FNV-1a, for row keys.
pub(crate) struct Hash(pub(crate) u64);

impl Hash {
    pub(crate) fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    pub(crate) fn add_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub(crate) fn add(&mut self, n: u64) {
        self.add_bytes(&n.to_le_bytes());
    }

    pub(crate) fn add_str(&mut self, text: &str) {
        self.add_bytes(text.as_bytes());
        self.add(text.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\
box
93784.52 180000.10
0.52 0.61 0.70 2/1234 5678
@stat
cpu  100 0 100 700 100 0 0 0 0 0
cpu0 50 0 50 350 50 0 0 0 0 0
cpu1 50 0 50 350 50 0 0 0 0 0
@mem
MemTotal:       16000000 kB
MemAvailable:   12000000 kB
SwapTotal:       4000000 kB
SwapFree:        4000000 kB
@net
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  999999     10    0    0    0     0          0         0   999999      10    0    0    0     0       0          0
  eth0:    1000     10    0    0    0     0          0         0     2000      10    0    0    0     0       0          0
@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
/dev/nvme0n1p2   500000000 120000000 380000000      24% /home/me/my disk
";

    #[test]
    fn parses_the_script_output() {
        let reading = parse(OUTPUT).unwrap();
        assert_eq!(reading.host, "box");
        assert_eq!(reading.uptime, 93784);
        assert_eq!(reading.load, [0.52, 0.61, 0.70]);
        assert_eq!(reading.tasks, (2, 1234));
        assert_eq!(reading.cpu, vec![(200, 1000), (100, 500), (100, 500)]);
        assert_eq!(reading.mem_total, 16_000_000 * 1024);
        assert_eq!(reading.mem_available, 12_000_000 * 1024);
        assert_eq!(reading.net, Some((1000, 2000)));
        assert_eq!(
            reading.disk,
            Some(Disk {
                total: 500_000_000 * 1024,
                used: 120_000_000 * 1024,
                mount: "/home/me/my disk".to_owned(),
            })
        );
    }

    #[test]
    fn a_machine_without_proc_has_no_reading() {
        assert_eq!(parse("@stat\n@mem\n@net\n@df\n"), None);
    }

    #[test]
    fn two_readings_make_loads_and_rates() {
        let before = parse(OUTPUT).unwrap();
        let mut after = before.clone();
        after.cpu = vec![
            (200 + 500, 1000 + 1000),
            (100 + 450, 500 + 500),
            (150, 1000),
        ];
        after.net = Some((1000 + 4096, 2000));
        let sample = sample(&before, &after, Duration::from_secs(2));
        assert_eq!(sample.cpu, vec![500, 900, 100]);
        assert_eq!(sample.mem_used, 4_000_000 * 1024);
        assert_eq!(sample.swap_used, 0);
        assert_eq!(sample.net, Some((2048, 0)));
    }

    #[test]
    fn bytes_read_like_df_h() {
        let text = |n| {
            let mut out = Buf::new();
            bytes(&mut out, n).unwrap();
            out.as_str().to_owned()
        };
        assert_eq!(text(512), "512 B");
        assert_eq!(text(5 * 1024 * 1024 + 200 * 1024), "5.2 MiB");
        assert_eq!(text(500 * 1024 * 1024 * 1024), "500 GiB");
    }

    #[test]
    fn lines_follow_the_sample() {
        let reading = parse(OUTPUT).unwrap();
        let mut view = View {
            state: State::Ready(sample(&reading, &reading, INTERVAL)),
            label: "local".to_owned(),
            cols: 80,
            rows: 24,
            top: 0,
            generation: 0,
            history: Box::default(),
        };
        // Two cores fit one row at 80 columns.
        let lines: Vec<_> = (0..view.lines()).filter_map(|i| view.line(i)).collect();
        assert_eq!(
            lines,
            [
                Line::Blank,
                Line::Cpu,
                Line::Blank,
                Line::Cores(0),
                Line::Blank,
                Line::Memory,
                Line::Blank,
                Line::Swap,
                Line::Blank,
                Line::Network,
                Line::Blank,
                Line::Disk,
                Line::Blank,
                Line::Graph(Graph::Cpu, 0),
                Line::Graph(Graph::Cpu, 1),
                Line::Graph(Graph::Cpu, 2),
                Line::Graph(Graph::Cpu, 3),
                Line::Graph(Graph::Cpu, 4),
                Line::Blank,
                Line::Graph(Graph::Net, 0),
                Line::Graph(Graph::Net, 1),
                Line::Graph(Graph::Net, 2),
                Line::Axis,
            ]
        );
        view.cols = 20;
        assert_eq!(view.core_rows(2), 2);
        assert_eq!(view.line(4), Some(Line::Cores(1)));
    }
}
