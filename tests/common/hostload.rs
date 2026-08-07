//! What else the host was doing when a gate A verdict was taken.
//!
//! CLAUDE.md's 2026-08-04 ruling stands and nothing here touches it: load is
//! not an excuse, no threshold branches on any number below, and a
//! load-coincident red is investigated as a real defect of the pipeline. What
//! this adds is that the investigation starts from a recorded fact, and that an
//! A/B can say whether its two arms were taken under comparable conditions —
//! which `tests/audio-baseline.toml`'s recorded sample can only claim in prose.
//!
//! Three readings, because they fail in different directions:
//!
//! - **The load average triple**, 1/5/15 minutes. No figure of it resolves a
//!   single ~15 s boot; the 1-minute one lags by design. But the competition is
//!   other worktrees' builds, which last minutes, and the triple's *shape* says
//!   whether the host was ramping up or winding down where one figure cannot.
//!   The 1-minute figure leads because it is the one the baseline's ceiling
//!   derivation recorded per run, so a fresh reading compares directly to it.
//! - **QEMU processes machine-wide.** Exact and instantaneous where the load
//!   average is neither, and it counts the guests the *host* has rather than
//!   the ones this run started — `qemu::live_instances()` is asserted zero
//!   before gate A, so the harness's own knowledge is a constant here. This
//!   run's own guest is still up when the sample is taken, so 1 is quiet.
//! - **`toyos-build` processes machine-wide**, drivers and harnesses alike: the
//!   count that names the competition as ToyOS work rather than as whatever
//!   else a laptop is doing. 1 is this run alone.
//!
//! A reading that cannot be taken is reported as unknown. A gate A verdict must
//! not turn on whether `ps` answered.

use std::fmt;
use std::process::Command;

#[derive(Clone, Copy)]
pub struct HostLoad {
    /// 1, 5 and 15-minute load averages.
    pub load: Option<[f64; 3]>,
    pub qemu: Option<usize>,
    pub toyos_build: Option<usize>,
}

impl HostLoad {
    pub fn sample() -> Self {
        let names = process_names();
        HostLoad {
            load: load_average(),
            qemu: names.as_deref().map(|n| count(n, |name| name.starts_with("qemu-system"))),
            toyos_build: names.as_deref().map(|n| count(n, is_toyos_build)),
        }
    }
}

impl fmt::Display for HostLoad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.load {
            Some([one, five, fifteen]) => {
                write!(f, "host: load {one:.1}/{five:.1}/{fifteen:.1}")?
            }
            None => write!(f, "host: load ?")?,
        }
        write!(f, " qemu {} toyos-build {}", show(self.qemu), show(self.toyos_build))
    }
}

/// The conditions a whole sample was taken under, for the re-record that sample
/// becomes. The thorough tier prints its own numbers in a form meant to be
/// pasted into `tests/audio-baseline.toml`; this is the sentence that has to go
/// beside them, and its absence is why the recorded sample's own conditions are
/// a claim rather than a measurement.
pub fn summarise(runs: &[HostLoad]) -> String {
    let load: Vec<f64> = runs.iter().filter_map(|r| r.load.map(|l| l[0])).collect();
    let load = match span_f64(&load) {
        Some((lo, med, hi)) => format!("1-min load {lo:.1}-{hi:.1} (median {med:.1})"),
        None => "1-min load unreadable".to_string(),
    };
    format!(
        "host conditions over {} runs: {load}, qemu {}, toyos-build {}",
        runs.len(),
        span_usize(runs.iter().filter_map(|r| r.qemu)),
        span_usize(runs.iter().filter_map(|r| r.toyos_build)),
    )
}

fn span_f64(values: &[f64]) -> Option<(f64, f64, f64)> {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some((*sorted.first()?, sorted[sorted.len() / 2], *sorted.last()?))
}

fn span_usize(values: impl Iterator<Item = usize>) -> String {
    let mut sorted: Vec<usize> = values.collect();
    sorted.sort_unstable();
    match (sorted.first(), sorted.last()) {
        (Some(lo), Some(hi)) => format!("{lo}-{hi}"),
        _ => "?".to_string(),
    }
}

fn show(count: Option<usize>) -> String {
    count.map_or_else(|| "?".to_string(), |n| n.to_string())
}

fn load_average() -> Option<[f64; 3]> {
    let mut out = [0.0f64; 3];
    // SAFETY: getloadavg writes at most `nelem` doubles through the pointer.
    let filled = unsafe { libc::getloadavg(out.as_mut_ptr(), out.len() as i32) };
    (filled == out.len() as i32).then_some(out)
}

/// Every process on the host, by executable basename.
fn process_names() -> Option<Vec<String>> {
    let out = Command::new("ps").args(["-Ao", "comm="]).output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| line.trim().rsplit('/').next().unwrap_or(line).to_string())
            .collect()
    })
}

fn count(names: &[String], matches: impl Fn(&str) -> bool) -> usize {
    names.iter().filter(|name| matches(name)).count()
}

/// Cargo spells the two halves of this build system differently — the driver is
/// the package's bin target `toyos-build`, the harness is the test target's
/// `toyos_build-<hash>` — so a single prefix misses whichever one is asking.
fn is_toyos_build(name: &str) -> bool {
    name.starts_with("toyos-build") || name.starts_with("toyos_build")
}
