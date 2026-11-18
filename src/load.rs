//! How busy this machine is: CPU, memory and disk use, read straight from
//! the operating system.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One sample of this machine. A part that could not be read is `None`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Load {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
    pub disk: Option<Disk>,
}

/// Percent busy across all cores, averaged over the sampling window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    pub percent: f64,
    pub cores: u32,
}

/// Percent of memory in use (not available), with sizes in bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub percent: f64,
    pub total: u64,
    pub swap_used: u64,
}

/// Percent of the filesystem at `path` that is not available to this user,
/// with sizes in bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub percent: f64,
    pub total: u64,
    pub free: u64,
    pub path: String,
}

/// Cumulative CPU time since boot, in clock ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticks {
    pub busy: u64,
    pub all: u64,
}

/// Percent busy between two readings. `None` when no time passed or a
/// counter went backwards (a wrap or a reset), never a made-up number.
pub fn cpu_percent(earlier: Ticks, later: Ticks) -> Option<f64> {
    let busy = later.busy.checked_sub(earlier.busy)?;
    let all = later.all.checked_sub(earlier.all)?;
    if all == 0 || busy > all {
        return None;
    }
    Some(100.0 * busy as f64 / all as f64)
}

/// How often a WhatsUp samples its machine.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);
/// How many samples CPU use is averaged over (10 s at `SAMPLE_EVERY`), so a
/// short spike does not flash red.
pub const CPU_WINDOW: usize = 5;

/// The last few CPU readings, giving the average across them.
pub struct CpuWindow {
    ticks: VecDeque<Ticks>,
    intervals: usize,
}

impl CpuWindow {
    pub fn new(intervals: usize) -> Self {
        Self {
            ticks: VecDeque::with_capacity(intervals + 1),
            intervals,
        }
    }

    /// Adds a reading and returns the percent busy from the oldest kept
    /// reading to this one.
    pub fn push(&mut self, ticks: Ticks) -> Option<f64> {
        self.ticks.push_back(ticks);
        while self.ticks.len() > self.intervals + 1 {
            self.ticks.pop_front();
        }
        cpu_percent(*self.ticks.front()?, ticks)
    }

    pub fn oldest(&self) -> Option<Ticks> {
        self.ticks.front().copied()
    }
}

/// Samples this machine on its own thread, every `SAMPLE_EVERY`, and keeps
/// the latest sample for whoever asks.
pub struct LoadMonitor {
    latest: Mutex<Load>,
}

impl LoadMonitor {
    /// Starts sampling now. Disk use is that of the filesystem holding
    /// `disk_path`.
    pub fn start(disk_path: PathBuf) -> Arc<LoadMonitor> {
        let monitor = Arc::new(LoadMonitor {
            latest: Mutex::default(),
        });
        let shared = monitor.clone();
        let spawned = std::thread::Builder::new()
            .name("whatsup-load".into())
            .spawn(move || {
                let cores = std::thread::available_parallelism()
                    .map(|cores| cores.get() as u32)
                    .unwrap_or(1);
                let mut window = CpuWindow::new(CPU_WINDOW);
                loop {
                    let cpu = read_ticks()
                        .ok()
                        .and_then(|ticks| window.push(ticks))
                        .map(|percent| Cpu { percent, cores });
                    let load = Load {
                        cpu,
                        memory: read_memory().ok(),
                        disk: read_disk(&disk_path).ok(),
                    };
                    *shared.latest.lock().expect("load lock") = load;
                    std::thread::sleep(SAMPLE_EVERY);
                }
            });
        if let Err(err) = spawned {
            eprintln!("whatsup: cannot sample this machine's load: {err}");
        }
        monitor
    }

    pub fn latest(&self) -> Load {
        self.latest.lock().expect("load lock").clone()
    }
}

/// The machine-wide `cpu` line of `/proc/stat`. Busy is everything but idle
/// and iowait; guest time is already inside user time.
pub fn parse_proc_stat(text: &str) -> Option<Ticks> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    if fields.len() < 5 {
        return None;
    }
    let all: u64 = fields.iter().sum();
    Some(Ticks {
        busy: all - fields[3] - fields[4],
        all,
    })
}

/// `/proc/meminfo`: in use is what is not available, which counts
/// reclaimable cache as free, as the kernel itself does.
pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    let swap_used = field("SwapTotal")?.saturating_sub(field("SwapFree")?);
    if total == 0 {
        return None;
    }
    Some(Memory {
        percent: 100.0 * total.saturating_sub(available) as f64 / total as f64,
        total,
        swap_used,
    })
}

/// macOS memory from `kern.memorystatus_level`, the kernel's free
/// percentage (what `memory_pressure` prints).
pub fn memory_from_pressure(total: u64, free_level: u32, swap_used: u64) -> Memory {
    Memory {
        percent: f64::from(100 - free_level.min(100)),
        total,
        swap_used,
    }
}

/// A filesystem's use from its `statvfs` counts. Blocks only root may use
/// count as used: the question is how much room is left for us.
pub fn disk_from_statvfs(blocks: u64, available: u64, fragment: u64, path: &str) -> Option<Disk> {
    let total = blocks.checked_mul(fragment)?;
    if total == 0 {
        return None;
    }
    let free = available.saturating_mul(fragment).min(total);
    Some(Disk {
        percent: 100.0 * (total - free) as f64 / total as f64,
        total,
        free,
        path: path.to_string(),
    })
}

/// The filesystem holding `path`.
pub fn read_disk(path: &Path) -> Result<Disk, String> {
    use std::os::unix::ffi::OsStrExt;
    let shown = path.display().to_string();
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{shown}: the path holds a NUL byte"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: c_path is NUL-terminated and stat is a valid out pointer.
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(format!("{shown}: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: statvfs succeeded, so it filled the struct.
    let stat = unsafe { stat.assume_init() };
    // The block counts are 32-bit on macOS and 64-bit on Linux.
    #[allow(clippy::unnecessary_cast)]
    let (blocks, available, fragment) = (
        stat.f_blocks as u64,
        stat.f_bavail as u64,
        stat.f_frsize as u64,
    );
    disk_from_statvfs(blocks, available, fragment, &shown)
        .ok_or_else(|| format!("{shown}: the filesystem reports no size"))
}

#[cfg(target_os = "linux")]
pub fn read_ticks() -> Result<Ticks, String> {
    let text = std::fs::read_to_string("/proc/stat").map_err(|err| format!("/proc/stat: {err}"))?;
    parse_proc_stat(&text).ok_or_else(|| "/proc/stat has no cpu line".to_string())
}

#[cfg(target_os = "linux")]
pub fn read_memory() -> Result<Memory, String> {
    let text =
        std::fs::read_to_string("/proc/meminfo").map_err(|err| format!("/proc/meminfo: {err}"))?;
    parse_meminfo(&text).ok_or_else(|| "/proc/meminfo lacks MemTotal or MemAvailable".to_string())
}

#[cfg(target_os = "macos")]
pub fn read_ticks() -> Result<Ticks, String> {
    let mut info = libc::host_cpu_load_info { cpu_ticks: [0; 4] };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: info is a host_cpu_load_info and count says how many
    // integer_t it holds, as host_statistics expects.
    let status = unsafe {
        libc::host_statistics(
            mac::host_port(),
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return Err(format!("host_statistics failed ({status})"));
    }
    let tick = |state: libc::c_int| u64::from(info.cpu_ticks[state as usize]);
    let busy =
        tick(libc::CPU_STATE_USER) + tick(libc::CPU_STATE_SYSTEM) + tick(libc::CPU_STATE_NICE);
    Ok(Ticks {
        busy,
        all: busy + tick(libc::CPU_STATE_IDLE),
    })
}

#[cfg(target_os = "macos")]
pub fn read_memory() -> Result<Memory, String> {
    let total: u64 = mac::sysctl(c"hw.memsize")?;
    let free_level: u32 = mac::sysctl(c"kern.memorystatus_level")?;
    let swap: libc::xsw_usage = mac::sysctl(c"vm.swapusage")?;
    Ok(memory_from_pressure(total, free_level, swap.xsu_used))
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::CStr;
    use std::sync::OnceLock;

    /// This task's host port. `mach_host_self` hands out a new send right
    /// on every call, so it is asked once.
    pub fn host_port() -> libc::mach_port_t {
        static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
        // libc points to the mach2 crate for this one call; not worth a
        // dependency.
        #[allow(deprecated)]
        // SAFETY: mach_host_self has no preconditions.
        *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    /// A fixed-size sysctl value by name.
    pub fn sysctl<T: Copy>(name: &CStr) -> Result<T, String> {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        let mut size = std::mem::size_of::<T>();
        // SAFETY: value has room for size bytes, and no new value is set.
        let status = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            return Err(format!(
                "sysctl {name:?}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if size != std::mem::size_of::<T>() {
            return Err(format!("sysctl {name:?}: unexpected size {size}"));
        }
        // SAFETY: sysctl filled all size_of::<T>() bytes.
        Ok(unsafe { value.assume_init() })
    }
}

/// How worrying a percent is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    High,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::High => "high",
        }
    }
}

pub const CPU_WARN: f64 = 75.0;
pub const CPU_HIGH: f64 = 90.0;
pub const MEMORY_WARN: f64 = 80.0;
pub const MEMORY_HIGH: f64 = 90.0;
pub const DISK_WARN: f64 = 85.0;
pub const DISK_HIGH: f64 = 95.0;

pub fn level(percent: f64, warn: f64, high: f64) -> Level {
    if percent >= high {
        Level::High
    } else if percent >= warn {
        Level::Warn
    } else {
        Level::Ok
    }
}

fn one_decimal(percent: f64) -> f64 {
    (percent * 10.0).round() / 10.0
}

/// A percent as shown, with the level of that same number, so a value never
/// reads as at a threshold its level says it has not reached.
fn shown(percent: f64, warn: f64, high: f64) -> (f64, &'static str) {
    let percent = one_decimal(percent);
    (percent, level(percent, warn, high).as_str())
}

/// A sample as the dashboard shows it: each part with its level, a missing
/// part as `null`.
pub fn view(load: &Load) -> Value {
    json!({
        "cpu": load.cpu.as_ref().map(|cpu| {
            let (percent, level) = shown(cpu.percent, CPU_WARN, CPU_HIGH);
            json!({ "percent": percent, "cores": cpu.cores, "level": level })
        }),
        "memory": load.memory.as_ref().map(|memory| {
            let (percent, level) = shown(memory.percent, MEMORY_WARN, MEMORY_HIGH);
            json!({
                "percent": percent,
                "total": memory.total,
                "swap_used": memory.swap_used,
                "level": level,
            })
        }),
        "disk": load.disk.as_ref().map(|disk| {
            let (percent, level) = shown(disk.percent, DISK_WARN, DISK_HIGH);
            json!({
                "percent": percent,
                "total": disk.total,
                "free": disk.free,
                "path": disk.path,
                "level": level,
            })
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn proc_stat_counts_iowait_as_idle() {
        let text = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\n";
        assert_eq!(
            parse_proc_stat(text),
            Some(Ticks {
                busy: 150,
                all: 1000
            })
        );
        assert_eq!(parse_proc_stat("intr 1 2 3\n"), None);
    }

    #[test]
    fn cpu_percent_over_a_window() {
        let earlier = Ticks {
            busy: 150,
            all: 1000,
        };
        let later = Ticks {
            busy: 450,
            all: 2000,
        };
        assert_eq!(cpu_percent(earlier, later), Some(30.0));
    }

    #[test]
    fn cpu_percent_is_none_without_progress_or_on_wrap() {
        let ticks = Ticks {
            busy: 150,
            all: 1000,
        };
        assert_eq!(cpu_percent(ticks, ticks), None);
        assert_eq!(
            cpu_percent(
                ticks,
                Ticks {
                    busy: 200,
                    all: 900
                }
            ),
            None
        );
        assert_eq!(
            cpu_percent(
                ticks,
                Ticks {
                    busy: 100,
                    all: 2000
                }
            ),
            None
        );
    }

    #[test]
    fn meminfo_uses_available_not_free() {
        let text = "MemTotal:       32110128 kB\nMemFree:         3543964 kB\nMemAvailable:   13645448 kB\nBuffers: 1 kB\nSwapTotal:       2097148 kB\nSwapFree:        1048574 kB\n";
        let memory = parse_meminfo(text).expect("parses");
        assert!((memory.percent - 57.5).abs() < 0.1, "{}", memory.percent);
        assert_eq!(memory.total, 32110128 * 1024);
        assert_eq!(memory.swap_used, 1048574 * 1024);
        let without_available = text.replace("MemAvailable", "Other");
        assert_eq!(parse_meminfo(&without_available), None);
    }

    #[test]
    fn pressure_level_is_free_percent() {
        assert_eq!(memory_from_pressure(36 << 30, 50, 0).percent, 50.0);
        assert_eq!(memory_from_pressure(36 << 30, 150, 0).percent, 0.0);
        assert_eq!(memory_from_pressure(36 << 30, 50, 7).swap_used, 7);
    }

    #[test]
    fn disk_uses_available_blocks() {
        let disk = disk_from_statvfs(1000, 120, 4096, "/home/jds").expect("disk");
        assert_eq!(disk.percent, 88.0);
        assert_eq!(disk.total, 4_096_000);
        assert_eq!(disk.free, 491_520);
        assert_eq!(disk.path, "/home/jds");
        assert_eq!(disk_from_statvfs(0, 0, 4096, "/"), None);
    }

    #[test]
    fn levels_at_their_edges() {
        assert_eq!(level(74.9, 75.0, 90.0), Level::Ok);
        assert_eq!(level(75.0, 75.0, 90.0), Level::Warn);
        assert_eq!(level(90.0, 75.0, 90.0), Level::High);
        assert_eq!(Level::Warn.as_str(), "warn");
    }

    #[test]
    fn view_attaches_levels_and_nulls() {
        let load = Load {
            cpu: None,
            memory: None,
            disk: Some(Disk {
                percent: 88.4321,
                total: 100,
                free: 12,
                path: "/home/jds".into(),
            }),
        };
        assert_eq!(
            view(&load),
            json!({
                "cpu": null,
                "memory": null,
                "disk": { "percent": 88.4, "total": 100, "free": 12, "path": "/home/jds", "level": "warn" },
            })
        );
        let busy = Load {
            cpu: Some(Cpu {
                percent: 95.0,
                cores: 12,
            }),
            memory: Some(Memory {
                percent: 81.0,
                total: 8,
                swap_used: 0,
            }),
            disk: None,
        };
        let busy = view(&busy);
        assert_eq!(busy["cpu"]["level"], "high");
        assert_eq!(busy["cpu"]["cores"], 12);
        assert_eq!(busy["memory"]["level"], "warn");
    }

    #[test]
    fn cpu_window_averages_over_the_window() {
        let mut window = CpuWindow::new(5);
        let tick = |all: u64| Ticks { busy: all / 2, all };
        assert_eq!(window.push(tick(0)), None);
        for all in [100, 200, 300, 400, 500] {
            assert_eq!(window.push(tick(all)), Some(50.0));
        }
        // The 7th push drops the first tick: the window starts at 100.
        let busy = Ticks {
            busy: 50 + 600,
            all: 700,
        };
        assert_eq!(window.push(busy), Some(100.0 * 600.0 / 600.0));
        assert_eq!(window.oldest(), Some(tick(100)));
    }

    #[test]
    fn level_follows_the_percent_shown() {
        let load = Load {
            cpu: Some(Cpu {
                percent: 74.96,
                cores: 12,
            }),
            ..Default::default()
        };
        let view = view(&load);
        assert_eq!(view["cpu"]["percent"], 75.0);
        assert_eq!(view["cpu"]["level"], "warn");
    }

    #[test]
    fn reads_this_machine() {
        let first = read_ticks().expect("ticks");
        std::thread::sleep(Duration::from_millis(200));
        let second = read_ticks().expect("ticks");
        if let Some(percent) = cpu_percent(first, second) {
            assert!((0.0..=100.0).contains(&percent), "{percent}");
        }
        let memory = read_memory().expect("memory");
        assert!(memory.total > 1 << 30, "{memory:?}");
        assert!((0.0..=100.0).contains(&memory.percent), "{memory:?}");
        let home = std::env::var_os("HOME").expect("HOME");
        let disk = read_disk(Path::new(&home)).expect("disk");
        assert!(disk.total > 0 && disk.free <= disk.total, "{disk:?}");
        assert!((0.0..=100.0).contains(&disk.percent), "{disk:?}");
    }
}
