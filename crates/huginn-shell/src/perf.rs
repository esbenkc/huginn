//! Timing of the pipeline phases, for the benchmark and for `state`.
//!
//! A [`span`] records how long a phase took, under a fixed name. The numbers
//! are kept per process: count, total, maximum and the most recent durations
//! (for the median and the 95th percentile). Recording costs two clock reads
//! and a short lock, so the spans stay on in every build.
//!
//! Names in use: `style`, `layout` (engine threads), `update` (one whole
//! page update after input or a script: restyle, new images, layout),
//! `raster` (one strip: display list and rasterizer), `compose` (one frame
//! put together), `present` (the frame handed to the window system) and
//! `suggest` (the address bar's suggestions for one keystroke; the first
//! one reads the history file).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Recent durations kept per span for the percentiles.
const RECENT: usize = 512;

#[derive(Default)]
struct Stat {
    count: u64,
    total: Duration,
    max: Duration,
    recent: Vec<Duration>,
    /// Where the next duration goes once `recent` is full.
    next: usize,
}

static STATS: Mutex<BTreeMap<&'static str, Stat>> = Mutex::new(BTreeMap::new());

/// Times the phase `name` until it is dropped.
pub struct Span {
    name: &'static str,
    start: Instant,
}

/// Start timing the phase `name`.
pub fn span(name: &'static str) -> Span {
    Span {
        name,
        start: Instant::now(),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        record(self.name, self.start.elapsed());
    }
}

/// Add one duration to the phase `name`.
pub fn record(name: &'static str, d: Duration) {
    let mut stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let s = stats.entry(name).or_default();
    s.count += 1;
    s.total += d;
    s.max = s.max.max(d);
    if s.recent.len() < RECENT {
        s.recent.push(d);
    } else {
        s.recent[s.next] = d;
        s.next = (s.next + 1) % RECENT;
    }
}

/// Forget everything recorded so far.
pub fn reset() {
    STATS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The numbers per phase: `count`, and `total_ms`, `p50_ms`, `p95_ms` and
/// `max_ms`. The percentiles are over the last 512 durations.
pub fn report() -> serde_json::Value {
    let stats = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let ms = |d: Duration| (d.as_secs_f64() * 1e5).round() / 100.0;
    let mut out = serde_json::Map::new();
    for (name, s) in stats.iter() {
        let mut recent = s.recent.clone();
        recent.sort_unstable();
        let at = |q: f64| recent[((recent.len() - 1) as f64 * q).round() as usize];
        out.insert(
            (*name).to_owned(),
            serde_json::json!({
                "count": s.count,
                "total_ms": ms(s.total),
                "p50_ms": ms(at(0.5)),
                "p95_ms": ms(at(0.95)),
                "max_ms": ms(s.max),
            }),
        );
    }
    serde_json::Value::Object(out)
}

/// Time this thread has spent on a CPU, if the system tells us (Linux:
/// `/proc/thread-self/schedstat`). Wall time minus this is time the thread
/// waited: for the script phase, on the network.
pub fn thread_cpu() -> Option<Duration> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let ns: u64 = s.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_nanos(ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_counted_and_summarized() {
        for ms in [1, 2, 3, 4, 100] {
            record("perf-test", Duration::from_millis(ms));
        }
        let r = report();
        let s = &r["perf-test"];
        assert_eq!(s["count"], 5);
        assert_eq!(s["total_ms"], 110.0);
        assert_eq!(s["p50_ms"], 3.0);
        assert_eq!(s["max_ms"], 100.0);
    }
}
