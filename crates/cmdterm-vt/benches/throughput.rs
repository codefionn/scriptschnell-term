//! Throughput of cmdterm-vt on the workload streams, plus the resize and reflow of a full
//! scrollback. The numbers recorded against wezterm-term are in the README. Run with `cargo bench -p cmdterm-vt`.
//!
//! Plain `std::time` harness: each case runs for at least `BUDGET` after a warm-up pass
//! and reports the best of several repetitions, which is steadier than the mean on a
//! machine that is doing other things.

#[path = "common/workloads.rs"]
mod workloads;

use cmdterm_vt::{Config, Size, Terminal};
use std::hint::black_box;
use std::time::Instant;

const ROWS: usize = 24;
const COLS: usize = 80;
const SCROLLBACK: usize = 10_000;
const SIZE: usize = 8 << 20;
const REPS: usize = 7;

fn ours(rows: usize, cols: usize) -> Terminal {
    let config = Config {
        scrollback: SCROLLBACK,
        ..Config::default()
    };
    Terminal::new(Size { rows, cols }, config)
}

fn feed_ours(t: &mut Terminal, data: &[u8]) {
    for chunk in data.chunks(4096) {
        t.advance(chunk);
    }
}

/// Best seconds per pass of `f`.
fn best(mut f: impl FnMut()) -> f64 {
    f(); // warm-up
    let mut best = f64::MAX;
    for _ in 0..REPS {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed().as_secs_f64());
    }
    best
}

fn mbps(bytes: usize, secs: f64) -> f64 {
    bytes as f64 / secs / (1 << 20) as f64
}

fn throughput() {
    println!("{:<8} {:>12}", "stream", "cmdterm-vt");
    // `cargo bench -- wide` runs only the streams whose name contains an argument.
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    for (name, data) in workloads::all(SIZE) {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        // Warm terminals start with a full scrollback and the stream's styles interned.
        let mut a = ours(ROWS, COLS);
        feed_ours(&mut a, &data);
        let ta = best(|| feed_ours(black_box(&mut a), black_box(&data)));
        println!("{name:<8} {:>8.0} MiB/s", mbps(data.len(), ta));
    }
}

fn reflow() {
    // 10000 lines of 100 columns in the scrollback, narrowed to 80 and widened again.
    let mut data = Vec::new();
    for i in 0..SCROLLBACK + ROWS {
        let line = format!("line {i:05} {}\r\n", "abcdefghij".repeat(9));
        data.extend_from_slice(line.as_bytes());
    }
    let mut a = ours(ROWS, 100);
    feed_ours(&mut a, &data);
    let ta = best(|| {
        a.resize(Size {
            rows: ROWS,
            cols: 80,
        });
        a.resize(Size {
            rows: ROWS,
            cols: 100,
        });
    });
    // Two resizes per pass.
    println!(
        "\nreflow of {SCROLLBACK} lines, per resize: cmdterm-vt {:.2} ms",
        ta * 500.0
    );
}

fn main() {
    // `cargo test` runs benches once with `--test`; skip the long run then.
    if std::env::args().any(|a| a == "--test") {
        return;
    }
    throughput();
    reflow();
}
