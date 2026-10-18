//! Memory pressure on this machine, which shortens how long idle agents in
//! hidden terminals keep running before they freeze.
//!
//! The freeze thread samples `/proc/meminfo` and `/proc/pressure/memory`
//! every few seconds and stores a [`Pressure`] level in an atomic the frame
//! loop reads. A level above [`Pressure::Normal`] needs little available
//! memory and a trend toward none, or the kernel already stalling tasks on
//! memory. Without these files, as off Linux, the level stays normal.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// How often the freeze thread samples memory.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);

/// How many samples the trend looks back over: one minute.
const WINDOW: usize = 12;

/// How long agents may sit idle in hidden terminals under
/// [`Pressure::High`], at most.
const HIGH_FREEZE_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Pressure {
    Normal = 0,
    /// Memory runs low and keeps falling: idle agents freeze after a minute.
    High = 1,
    /// Memory is nearly gone or about to be: idle agents freeze at the next
    /// check.
    Critical = 2,
}

impl Pressure {
    pub fn load(level: &AtomicU8) -> Self {
        match level.load(Ordering::Acquire) {
            1 => Self::High,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }

    /// How long an agent may sit idle in a hidden terminal at this level,
    /// given the configured time.
    pub fn freeze_after(self, configured: Duration) -> Duration {
        match self {
            Self::Normal => configured,
            Self::High => configured.min(HIGH_FREEZE_AFTER),
            Self::Critical => Duration::ZERO,
        }
    }
}

/// One reading of `/proc/meminfo`, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub total: u64,
    pub available: u64,
}

/// Stall percentages from `/proc/pressure/memory`, averaged over ten
/// seconds: `some` counts time at least one task waited for memory, `full`
/// time every task did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stall {
    pub some: f32,
    pub full: f32,
}

/// The last minute of readings and the level they give.
#[derive(Default)]
pub struct Monitor {
    samples: VecDeque<(Instant, u64)>,
}

impl Monitor {
    /// Reads this machine's memory and returns the level the last minute
    /// gives.
    pub fn sample(&mut self, now: Instant) -> Pressure {
        let reading = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| parse_meminfo(&text));
        let stall = std::fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|text| parse_psi(&text));
        self.push(now, reading, stall)
    }

    /// Adds a reading and classifies the window. A missing reading clears
    /// the trend, since it can no longer be trusted.
    pub fn push(
        &mut self,
        now: Instant,
        reading: Option<Reading>,
        stall: Option<Stall>,
    ) -> Pressure {
        let Some(reading) = reading.filter(|r| r.total > 0) else {
            self.samples.clear();
            return stall.map_or(Pressure::Normal, |stall| classify(None, None, stall));
        };
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back((now, reading.available));
        classify(
            Some(reading),
            self.time_to_full(),
            stall.unwrap_or_default(),
        )
    }

    /// When available memory runs out if it keeps falling as fast as it did
    /// over the window. `None` while it does not fall or the window is too
    /// short to tell.
    fn time_to_full(&self) -> Option<Duration> {
        let (&(first_at, first), &(last_at, last)) = (self.samples.front()?, self.samples.back()?);
        let elapsed = last_at.saturating_duration_since(first_at).as_secs_f64();
        if self.samples.len() < 3 || elapsed <= 0.0 || last >= first {
            return None;
        }
        let per_second = (first - last) as f64 / elapsed;
        Some(Duration::from_secs_f64(last as f64 / per_second))
    }
}

/// The level for a reading, the time until memory runs out at the current
/// trend, and the kernel's stall averages.
pub fn classify(reading: Option<Reading>, to_full: Option<Duration>, stall: Stall) -> Pressure {
    let mut level = Pressure::Normal;
    if stall.some >= 10.0 {
        level = Pressure::High;
    }
    if stall.full >= 10.0 {
        level = Pressure::Critical;
    }
    let Some(reading) = reading else {
        return level;
    };
    let free = reading.available as f64 / reading.total as f64;
    let soon = |within: u64| to_full.is_some_and(|left| left <= Duration::from_secs(within));
    let memory = if free < 0.05 || (free < 0.20 && soon(60)) {
        Pressure::Critical
    } else if free < 0.10 || (free < 0.20 && soon(15 * 60)) {
        Pressure::High
    } else {
        Pressure::Normal
    };
    level.max(memory)
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> Option<Reading> {
    let (mut total, mut available) = (None, None);
    for line in text.lines() {
        let mut words = line.split_ascii_whitespace();
        let slot = match words.next() {
            Some("MemTotal:") => &mut total,
            Some("MemAvailable:") => &mut available,
            _ => continue,
        };
        *slot = words
            .next()
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb * 1024);
    }
    Some(Reading {
        total: total?,
        available: available?,
    })
}

/// The `avg10` values of `/proc/pressure/memory`.
pub fn parse_psi(text: &str) -> Option<Stall> {
    let mut stall = Stall::default();
    let mut found = false;
    for line in text.lines() {
        let mut words = line.split_ascii_whitespace();
        let slot = match words.next() {
            Some("some") => &mut stall.some,
            Some("full") => &mut stall.full,
            _ => continue,
        };
        let avg10 = words.find_map(|word| word.strip_prefix("avg10="));
        if let Some(value) = avg10.and_then(|v| v.parse().ok()) {
            *slot = value;
            found = true;
        }
    }
    found.then_some(stall)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn reading(available: u64) -> Option<Reading> {
        Some(Reading {
            total: 16 * GIB,
            available,
        })
    }

    #[test]
    fn parses_meminfo_and_psi() {
        let meminfo =
            "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:   12000000 kB\n";
        assert_eq!(
            parse_meminfo(meminfo),
            Some(Reading {
                total: 16_000_000 * 1024,
                available: 12_000_000 * 1024,
            })
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
        let psi = "some avg10=12.50 avg60=3.00 avg300=1.00 total=100\n\
                   full avg10=0.40 avg60=0.10 avg300=0.00 total=10\n";
        assert_eq!(
            parse_psi(psi),
            Some(Stall {
                some: 12.5,
                full: 0.4
            })
        );
        assert_eq!(parse_psi(""), None);
    }

    #[test]
    fn plenty_of_memory_is_normal_even_when_it_falls() {
        let mut monitor = Monitor::default();
        let start = Instant::now();
        for i in 0..WINDOW as u64 {
            let at = start + SAMPLE_EVERY * i as u32;
            let level = monitor.push(at, reading(12 * GIB - i * GIB / 4), None);
            assert_eq!(level, Pressure::Normal);
        }
    }

    #[test]
    fn low_memory_that_holds_steady_is_not_high() {
        let mut monitor = Monitor::default();
        let start = Instant::now();
        for i in 0..WINDOW as u32 {
            let level = monitor.push(start + SAMPLE_EVERY * i, reading(3 * GIB), None);
            assert_eq!(level, Pressure::Normal);
        }
    }

    #[test]
    fn low_memory_falling_toward_full_is_high_then_critical() {
        let mut monitor = Monitor::default();
        let start = Instant::now();
        // 3 GiB of 16 left, losing 10 MiB a second: full in about five
        // minutes.
        let mut level = Pressure::Normal;
        for i in 0..WINDOW as u64 {
            let at = start + SAMPLE_EVERY * i as u32;
            level = monitor.push(at, reading(3 * GIB - i * 50 * (1 << 20)), None);
        }
        assert_eq!(level, Pressure::High);
        // Losing 40 MiB a second: full within the minute.
        let mut monitor = Monitor::default();
        for i in 0..WINDOW as u64 {
            let at = start + SAMPLE_EVERY * i as u32;
            level = monitor.push(at, reading(3 * GIB - i * 200 * (1 << 20)), None);
        }
        assert_eq!(level, Pressure::Critical);
    }

    #[test]
    fn stalls_raise_the_level() {
        let stall = |some, full| Stall { some, full };
        assert_eq!(classify(None, None, stall(0.0, 0.0)), Pressure::Normal);
        assert_eq!(classify(None, None, stall(15.0, 0.0)), Pressure::High);
        assert_eq!(classify(None, None, stall(30.0, 12.0)), Pressure::Critical);
        assert_eq!(
            classify(reading(GIB / 2), None, stall(0.0, 0.0)),
            Pressure::Critical
        );
    }

    #[test]
    fn levels_shorten_the_freeze_time() {
        let fifteen = Duration::from_secs(15 * 60);
        assert_eq!(Pressure::Normal.freeze_after(fifteen), fifteen);
        assert_eq!(Pressure::High.freeze_after(fifteen), HIGH_FREEZE_AFTER);
        assert_eq!(
            Pressure::High.freeze_after(Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        assert_eq!(Pressure::Critical.freeze_after(fifteen), Duration::ZERO);
    }
}
