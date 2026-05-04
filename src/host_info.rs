//! Lightweight host vitals: CPU%, MEM%, 1-min load average.
//!
//! Reads `/proc` directly on Linux. Returns `None` on other platforms (for now);
//! callers should treat absence as "metrics unavailable" and render a graceful
//! fallback.

#[derive(Debug, Clone, Copy)]
pub struct HostMetrics {
    /// Aggregate CPU usage in percent (0.0 - 100.0). Computed across all cores.
    pub cpu_pct: f64,
    /// Used memory in percent (0.0 - 100.0). Used = MemTotal - MemAvailable.
    pub mem_pct: f64,
    /// 1-minute load average.
    pub load1: f64,
}

/// Stateful sampler that remembers the previous `/proc/stat` snapshot so it
/// can compute CPU usage as a delta between ticks.
#[derive(Debug, Default)]
pub struct HostSampler {
    prev: Option<CpuTimes>,
}

#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    /// All non-idle jiffies (user + nice + system + irq + softirq + steal).
    busy: u64,
    /// idle + iowait.
    idle: u64,
}

impl HostSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample current host metrics. Returns `None` if /proc is unavailable
    /// (non-Linux, or first sample where no CPU delta exists yet).
    pub fn sample(&mut self) -> Option<HostMetrics> {
        let cpu_pct = self.sample_cpu()?;
        let mem_pct = sample_mem()?;
        let load1 = sample_load()?;
        Some(HostMetrics {
            cpu_pct,
            mem_pct,
            load1,
        })
    }

    fn sample_cpu(&mut self) -> Option<f64> {
        let now = read_cpu_times()?;
        let pct = match self.prev {
            Some(prev) => {
                let busy_d = now.busy.saturating_sub(prev.busy) as f64;
                let idle_d = now.idle.saturating_sub(prev.idle) as f64;
                let total = busy_d + idle_d;
                if total > 0.0 {
                    (busy_d / total) * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        self.prev = Some(now);
        Some(pct)
    }
}

#[cfg(target_os = "linux")]
fn read_cpu_times() -> Option<CpuTimes> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
    // Layout: user nice system idle iowait irq softirq steal guest guest_nice
    if nums.len() < 4 {
        return None;
    }
    let user = nums[0];
    let nice = nums[1];
    let system = nums[2];
    let idle = nums[3];
    let iowait = *nums.get(4).unwrap_or(&0);
    let irq = *nums.get(5).unwrap_or(&0);
    let softirq = *nums.get(6).unwrap_or(&0);
    let steal = *nums.get(7).unwrap_or(&0);
    Some(CpuTimes {
        busy: user + nice + system + irq + softirq + steal,
        idle: idle + iowait,
    })
}

#[cfg(target_os = "linux")]
fn sample_mem() -> Option<f64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut avail = 0u64;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest)?;
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb(rest)?;
        }
        if total > 0 && avail > 0 {
            break;
        }
    }
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(avail) as f64;
    Some((used / total as f64) * 100.0)
}

#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> Option<u64> {
    s.split_whitespace().next().and_then(|n| n.parse().ok())
}

#[cfg(target_os = "linux")]
fn sample_load() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    s.split_whitespace().next().and_then(|n| n.parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn read_cpu_times() -> Option<CpuTimes> {
    None
}
#[cfg(not(target_os = "linux"))]
fn sample_mem() -> Option<f64> {
    None
}
#[cfg(not(target_os = "linux"))]
fn sample_load() -> Option<f64> {
    None
}

/// Aggregate per-session metrics into a single agent-wide summary.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentAggregate {
    pub mem_mb: u64,
    /// Average context window fill across active sessions (0.0 - 100.0).
    pub avg_ctx_pct: f64,
    pub active_count: usize,
}

impl AgentAggregate {
    pub fn from_sessions(sessions: &[crate::model::AgentSession]) -> Self {
        let mut mem_mb = 0u64;
        let mut ctx_sum = 0.0;
        let mut ctx_n = 0usize;
        let mut active = 0usize;
        for s in sessions {
            mem_mb = mem_mb.saturating_add(s.mem_mb);
            if s.context_percent > 0.0 {
                ctx_sum += s.context_percent;
                ctx_n += 1;
            }
            if s.status.is_active() {
                active += 1;
            }
        }
        let avg_ctx_pct = if ctx_n > 0 {
            ctx_sum / ctx_n as f64
        } else {
            0.0
        };
        Self {
            mem_mb,
            avg_ctx_pct,
            active_count: active,
        }
    }
}
