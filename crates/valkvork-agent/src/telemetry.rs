//! Host and container telemetry read straight from the kernel (FR-AGE-03).
//! The Docker stats API is deliberately not used.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// `usage_usec` from a cgroup v2 `cpu.stat`.
pub fn cgroup_cpu_usage_usec(dir: &Path) -> Option<u64> {
    let s = std::fs::read_to_string(dir.join("cpu.stat")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// `memory.current` from a cgroup v2 directory.
pub fn cgroup_memory_current(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Converts successive cumulative CPU counters into average millicores.
#[derive(Debug, Default)]
pub struct CpuRate {
    last: Option<(Instant, u64)>,
}

impl CpuRate {
    /// Feed the cumulative `usage_usec`; returns millicores averaged since the
    /// previous sample (0 on the first call).
    pub fn update(&mut self, now: Instant, usage_usec: u64) -> u32 {
        let out = match self.last {
            Some((t, prev)) if usage_usec >= prev => {
                let elapsed_us = now.duration_since(t).as_micros() as f64;
                if elapsed_us > 0.0 {
                    ((usage_usec - prev) as f64 / elapsed_us * 1000.0).round() as u32
                } else {
                    0
                }
            }
            _ => 0,
        };
        self.last = Some((now, usage_usec));
        out
    }
}

/// Container telemetry, given the candidate cgroup dirs from the runtime.
pub fn read_container(paths: &[PathBuf]) -> Option<(u64, u64)> {
    let dir = paths.iter().find(|p| p.join("cpu.stat").exists())?;
    Some((
        cgroup_cpu_usage_usec(dir)?,
        cgroup_memory_current(dir).unwrap_or(0),
    ))
}

/// Node CPU busy % from `/proc/stat` deltas.
#[derive(Debug, Default)]
pub struct NodeCpu {
    last: Option<(u64, u64)>,
}

impl NodeCpu {
    pub fn sample(&mut self) -> f64 {
        let Some((busy, total)) = read_proc_stat() else {
            return 0.0;
        };
        let pct = match self.last {
            Some((b0, t0)) if total > t0 => {
                (busy.saturating_sub(b0)) as f64 / (total - t0) as f64 * 100.0
            }
            _ => 0.0,
        };
        self.last = Some((busy, total));
        pct
    }
}

fn read_proc_stat() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    let idle = v[3] + v.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = v.iter().take(8).sum();
    Some((total - idle, total))
}

/// Used memory (MemTotal − MemAvailable) from `/proc/meminfo`.
pub fn node_used_memory() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .map(|kib| kib * 1024)
            .unwrap_or(0)
    };
    field("MemTotal:").saturating_sub(field("MemAvailable:"))
}

/// Total bytes of regular files under `dir` (alloc dir accounting, FR-AGE-08).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cpu_rate_millicores() {
        let mut r = CpuRate::default();
        let t0 = Instant::now();
        assert_eq!(r.update(t0, 1_000_000), 0);
        // 1 s of CPU over 2 s wall = 500 m
        assert_eq!(r.update(t0 + Duration::from_secs(2), 2_000_000), 500);
    }

    #[test]
    fn reads_cgroup_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("cpu.stat"),
            "usage_usec 4200\nuser_usec 4000\n",
        )
        .unwrap();
        std::fs::write(d.path().join("memory.current"), "1048576\n").unwrap();
        assert_eq!(
            read_container(&[PathBuf::from("/nope"), d.path().to_path_buf()]),
            Some((4200, 1 << 20))
        );
    }

    #[test]
    fn sizes_directories() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(d.path().join("sub/b"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size(d.path()), 150);
    }
}
