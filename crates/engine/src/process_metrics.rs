//! Process metrics for the TUI Status screen.
//!
//! **Deadlock / perf contract**
//! - Collect **only on the TUI thread**, never under catalog/registry/disk locks.
//! - Caller samples **only while Status is open**, at **≤ 1 Hz**.
//! - Self-process only; never full process-table scans.
//! - No peer-io / disk / hash work; no channels; no engine mutexes.
//!
//! **CPU % (wall-clock, top-compatible)**
//! ```text
//! %CPU = (Δcpu_seconds) / Δwall_secs * 100   // one core; can exceed 100
//! ```
//! - **Linux:** jiffies from **`/proc/self/task/<tid>/stat`** (not `/proc/<tid>/stat`,
//!   which reports wrong per-thread times).
//! - **FreeBSD:** `ki_runtime` (µs) from `sysctl(KERN_PROC_PID|KERN_PROC_INC_THREAD)`.
//! - **Darwin:** Mach `task_threads` + `THREAD_BASIC_INFO` (µs); names via
//!   `pthread_getname_np`.
//!
//! **Portability**
//! - Process RSS / FDs / I/O / uptime: [`sysinfo`] on supported OSes.
//! - Thread names + per-group CPU: Linux/Android, FreeBSD, Darwin (see above).
//! - Other OSes: process-level CPU only via sysinfo.
//! - Filesystem free/total: POSIX `statvfs` for default download root and
//!   roots of open (`want_start`) torrents.
//!
//! **Process I/O rates** (`io_read_bps` / `io_write_bps`)
//! - **Linux:** **bytes/s** from `/proc/self/io` via sysinfo.
//! - **FreeBSD:** filesystem I/O **ops/s** from `ki_rusage.ru_inblock` /
//!   `ru_oublock` (`io_as_ops` is true). Status labels these as "I/O ops".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sysinfo::{get_current_pid, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Linux USER_HZ (jiffies/sec) used in `/proc/*/stat` time fields.
#[cfg(any(target_os = "linux", target_os = "android"))]
const CLK_TCK: f64 = 100.0;

/// Threads sharing the same OS name (`comm`), with aggregated CPU.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadGroup {
    pub name: String,
    pub count: u32,
    /// Sum of per-TID wall-clock CPU % for this name. `None` until 2nd sample.
    pub cpu_pct: Option<f32>,
}

/// One process sample for the Status screen.
#[derive(Debug, Clone, Default)]
pub struct ProcessSample {
    pub pid: u32,
    pub uptime_secs: Option<u64>,
    pub rss_bytes: Option<u64>,
    /// Process-wide CPU % of one core (sum of threads on Linux).
    pub cpu_pct: Option<f32>,
    pub threads: Option<u32>,
    pub fd_count: Option<u32>,
    pub fd_soft_limit: Option<u64>,
    /// Process I/O rate: **bytes/s** on Linux; **FS ops/s** on FreeBSD when
    /// [`Self::io_as_ops`] is true.
    pub io_read_bps: Option<u64>,
    pub io_write_bps: Option<u64>,
    /// When true, `io_*_bps` are filesystem I/O operations/s (FreeBSD), not B/s.
    pub io_as_ops: bool,
    pub thread_groups: Vec<ThreadGroup>,
    pub available: bool,
}

/// Long-lived sample state (TUI thread only).
pub struct ProcessSampleState {
    sys: System,
    pid: Pid,
    prev_at: Option<Instant>,
    /// Previous per-tid CPU counters: Linux jiffies; FreeBSD/Darwin user+system µs.
    prev_cpu: HashMap<u64, u64>,
    prev_disk: Option<(u64, u64)>,
}

impl Default for ProcessSampleState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampleState {
    pub fn new() -> Self {
        let _ = sysinfo::set_open_files_limit(0);
        let pid = get_current_pid().unwrap_or_else(|_| Pid::from_u32(std::process::id()));
        Self {
            sys: System::new(),
            pid,
            prev_at: None,
            prev_cpu: HashMap::new(),
            prev_disk: None,
        }
    }

    /// Collect a sample. Safe to call with no engine locks held.
    pub fn collect(&mut self) -> ProcessSample {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return ProcessSample {
                pid: self.pid.as_u32(),
                available: false,
                ..ProcessSample::default()
            };
        }

        // Process-level fields via sysinfo (self only — never All).
        let kind = ProcessRefreshKind::nothing()
            .without_tasks()
            .with_memory()
            .with_cpu()
            .with_disk_usage();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::Some(&[self.pid]), true, kind);

        let now = Instant::now();
        let mut sample = ProcessSample {
            pid: self.pid.as_u32(),
            available: false,
            ..ProcessSample::default()
        };

        let Some(proc_) = self.sys.process(self.pid) else {
            return sample;
        };
        sample.available = true;
        sample.rss_bytes = Some(proc_.memory());
        sample.uptime_secs = Some(proc_.run_time());
        sample.fd_count = proc_.open_files().map(|n| n as u32);
        sample.fd_soft_limit = proc_
            .open_files_limit()
            .or_else(System::open_files_limit)
            .map(|n| n as u64);

        let wall_secs = self
            .prev_at
            .map(|t0| now.duration_since(t0).as_secs_f64())
            .filter(|&dt| dt > 0.05);

        // Disk rates from absolute totals + wall clock.
        // FreeBSD: sysinfo puts ru_inblock/oublock (ops) in total_*_bytes — leave
        // as ops/s; TUI labels via `io_as_ops` (do not invent a block size).
        let disk = proc_.disk_usage();
        let read_tot = disk.total_read_bytes;
        let write_tot = disk.total_written_bytes;
        #[cfg(target_os = "freebsd")]
        {
            sample.io_as_ops = true;
        }
        if let (Some(dt), Some((pr, pw))) = (wall_secs, self.prev_disk) {
            sample.io_read_bps = Some(((read_tot.saturating_sub(pr)) as f64 / dt) as u64);
            sample.io_write_bps = Some(((write_tot.saturating_sub(pw)) as f64 / dt) as u64);
        }
        self.prev_disk = Some((read_tot, write_tot));

        // --- CPU + thread groups ---
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let (groups, new_cpu, proc_cpu) =
                linux_thread_cpu_and_groups(&self.prev_cpu, wall_secs);
            apply_thread_sample(&mut sample, groups, new_cpu, proc_cpu, &mut self.prev_cpu);
        }

        #[cfg(target_os = "freebsd")]
        {
            let (groups, new_cpu, proc_cpu) =
                freebsd_thread_cpu_and_groups(&self.prev_cpu, wall_secs);
            apply_thread_sample(&mut sample, groups, new_cpu, proc_cpu, &mut self.prev_cpu);
        }

        #[cfg(target_os = "macos")]
        {
            let (groups, new_cpu, proc_cpu) =
                macos_thread_cpu_and_groups(&self.prev_cpu, wall_secs);
            apply_thread_sample(&mut sample, groups, new_cpu, proc_cpu, &mut self.prev_cpu);
        }

        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos"
        )))]
        {
            // Process-level only (no LWP walk on this OS).
            let acc_ms = proc_.accumulated_cpu_time();
            let key = u64::from(self.pid.as_u32());
            if let (Some(dt), Some(&prev_ms)) = (wall_secs, self.prev_cpu.get(&key)) {
                let dms = acc_ms.saturating_sub(prev_ms) as f64;
                sample.cpu_pct = Some(((dms / 1000.0) / dt * 100.0) as f32);
            }
            self.prev_cpu.clear();
            self.prev_cpu.insert(key, acc_ms);
            sample.thread_groups.clear();
            sample.threads = None;
        }

        self.prev_at = Some(now);
        sample
    }
}

fn apply_thread_sample(
    sample: &mut ProcessSample,
    groups: Vec<ThreadGroup>,
    new_cpu: HashMap<u64, u64>,
    proc_cpu: Option<f32>,
    prev_cpu: &mut HashMap<u64, u64>,
) {
    let n: u32 = groups.iter().map(|g| g.count).sum();
    sample.threads = (n > 0).then_some(n);
    sample.thread_groups = groups;
    if let Some(c) = proc_cpu {
        sample.cpu_pct = Some(c);
    }
    *prev_cpu = new_cpu;
}

/// Aggregate named threads into groups with wall-clock CPU %.
///
/// `counter_to_secs` converts the OS-specific counter unit to seconds of CPU time.
fn thread_groups_from_counters(
    threads: impl IntoIterator<Item = (u64, String, u64)>,
    prev_cpu: &HashMap<u64, u64>,
    wall_secs: Option<f64>,
    counter_to_secs: impl Fn(u64) -> f64,
) -> (Vec<ThreadGroup>, HashMap<u64, u64>, Option<f32>) {
    let mut by_name: HashMap<String, (u32, f32)> = HashMap::new();
    let mut new_cpu: HashMap<u64, u64> = HashMap::new();
    let mut proc_cpu = 0.0f32;
    let mut any = false;

    for (tid, name, counter) in threads {
        if name.is_empty() {
            continue;
        }
        new_cpu.insert(tid, counter);
        let tid_cpu = match (wall_secs, prev_cpu.get(&tid)) {
            (Some(dt), Some(&prev)) if dt > 0.0 => {
                let dsec = counter_to_secs(counter.saturating_sub(prev));
                any = true;
                ((dsec / dt) * 100.0) as f32
            }
            _ => 0.0,
        };
        proc_cpu += tid_cpu;
        let label = pretty_thread_name(&name);
        let e = by_name.entry(label).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += tid_cpu;
    }

    (sort_groups(by_name, any), new_cpu, any.then_some(proc_cpu))
}

/// Linux: walk `/proc/self/task` for names + jiffies (correct path).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_thread_cpu_and_groups(
    prev_cpu: &HashMap<u64, u64>,
    wall_secs: Option<f64>,
) -> (Vec<ThreadGroup>, HashMap<u64, u64>, Option<f32>) {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return (Vec::new(), HashMap::new(), None);
    };

    let mut threads = Vec::new();
    for ent in dir.flatten() {
        let tid: u64 = match ent.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        let name = std::fs::read_to_string(ent.path().join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let Some(jiffies) = read_task_jiffies(tid) else {
            continue;
        };
        threads.push((tid, name, jiffies));
    }

    thread_groups_from_counters(threads, prev_cpu, wall_secs, |j| j as f64 / CLK_TCK)
}

/// FreeBSD: `sysctl(KERN_PROC_PID | KERN_PROC_INC_THREAD)` → one `kinfo_proc` per LWP.
///
/// Names: `ki_tdname` + `ki_moretdname` (from `pthread_set_name_np` / Rust thread names).
/// CPU counter: `ki_runtime` (cumulative run time in **microseconds**).
#[cfg(target_os = "freebsd")]
fn freebsd_thread_cpu_and_groups(
    prev_cpu: &HashMap<u64, u64>,
    wall_secs: Option<f64>,
) -> (Vec<ThreadGroup>, HashMap<u64, u64>, Option<f32>) {
    let threads = freebsd_list_lwps();
    thread_groups_from_counters(threads, prev_cpu, wall_secs, |us| us as f64 / 1_000_000.0)
}

/// `(tid, name, ki_runtime_us)` for every LWP of this process.
#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)] // sysctl + kinfo_proc buffer — no engine locks
fn freebsd_list_lwps() -> Vec<(u64, String, u64)> {
    use std::mem;
    use std::ptr;

    let pid = std::process::id() as libc::c_int;
    let mut mib: [libc::c_int; 4] = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID | libc::KERN_PROC_INC_THREAD,
        pid,
    ];

    // Size + data with a couple retries (thread count can change between calls).
    for _ in 0..4 {
        let mut len: usize = 0;
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                ptr::null_mut(),
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return Vec::new();
        }
        // Slack for races where threads spawn between size and data queries.
        let mut buf = vec![0u8; len.saturating_add(4 * mem::size_of::<libc::kinfo_proc>())];
        let mut got = buf.len();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                buf.as_mut_ptr().cast(),
                &mut got,
                ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            // ENOMEM → retry with larger estimate
            continue;
        }
        let sz = mem::size_of::<libc::kinfo_proc>();
        if sz == 0 || got < sz {
            return Vec::new();
        }
        let n = got / sz;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let kp = unsafe { &*(buf.as_ptr().add(i * sz) as *const libc::kinfo_proc) };
            let tid = kp.ki_tid as u64;
            let name = freebsd_thread_name(kp);
            let runtime_us = kp.ki_runtime;
            out.push((tid, name, runtime_us));
        }
        return out;
    }
    Vec::new()
}

#[cfg(target_os = "freebsd")]
fn freebsd_thread_name(kp: &libc::kinfo_proc) -> String {
    let mut s = c_char_buf_to_string(&kp.ki_tdname);
    // Longer names continue in ki_moretdname (TDNAMLEN is only 16).
    let more = c_char_buf_to_string(&kp.ki_moretdname);
    s.push_str(&more);
    if s.is_empty() {
        // Fallback: process name when the LWP was never named.
        s = c_char_buf_to_string(&kp.ki_comm);
    }
    s
}

#[cfg(target_os = "freebsd")]
fn c_char_buf_to_string(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Names: `pthread_getname_np`. CPU: user+system µs from `THREAD_BASIC_INFO`.
#[cfg(target_os = "macos")]
fn macos_thread_cpu_and_groups(
    prev_cpu: &HashMap<u64, u64>,
    wall_secs: Option<f64>,
) -> (Vec<ThreadGroup>, HashMap<u64, u64>, Option<f32>) {
    let threads = macos_list_threads();
    thread_groups_from_counters(threads, prev_cpu, wall_secs, |us| us as f64 / 1_000_000.0)
}

/// `(tid, name, cpu_us)` for every thread of this task.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // Mach task_threads / thread_info — self only, no engine locks
#[allow(deprecated)] // mach_task_self in libc; same symbol as mach_task_self_
fn macos_list_threads() -> Vec<(u64, String, u64)> {
    use std::mem;
    use std::ptr;

    // Not re-exported by libc crate; required to release task_threads ports.
    #[link(name = "System", kind = "dylib")]
    extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    unsafe {
        let task = libc::mach_task_self();
        let mut thread_list: *mut libc::thread_act_t = ptr::null_mut();
        let mut thread_count: libc::mach_msg_type_number_t = 0;
        let kr = libc::task_threads(task, &mut thread_list, &mut thread_count);
        if kr != libc::KERN_SUCCESS || thread_list.is_null() || thread_count == 0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(thread_count as usize);
        for i in 0..thread_count {
            let port = *thread_list.add(i as usize);

            let mut basic: libc::thread_basic_info = mem::zeroed();
            let mut basic_count = libc::THREAD_BASIC_INFO_COUNT;
            let kr = libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                &mut basic as *mut _ as libc::thread_info_t,
                &mut basic_count,
            );
            if kr != libc::KERN_SUCCESS {
                let _ = mach_port_deallocate(task, port);
                continue;
            }
            let cpu_us = time_value_to_us(basic.user_time) + time_value_to_us(basic.system_time);

            let mut idinfo: libc::thread_identifier_info = mem::zeroed();
            let mut id_count = libc::THREAD_IDENTIFIER_INFO_COUNT;
            let kr = libc::thread_info(
                port,
                libc::THREAD_IDENTIFIER_INFO as libc::thread_flavor_t,
                &mut idinfo as *mut _ as libc::thread_info_t,
                &mut id_count,
            );
            let tid = if kr == libc::KERN_SUCCESS {
                idinfo.thread_id
            } else {
                u64::from(port)
            };

            // pthread_t is an opaque integer on Darwin (not a raw pointer).
            let pt = libc::pthread_from_mach_thread_np(port);
            let mut name_buf = [0i8; 64];
            let name = if pt != 0 {
                let _ = libc::pthread_getname_np(pt, name_buf.as_mut_ptr(), name_buf.len());
                let s = std::ffi::CStr::from_ptr(name_buf.as_ptr())
                    .to_string_lossy()
                    .into_owned();
                if s.is_empty() {
                    format!("thread-{tid}")
                } else {
                    s
                }
            } else {
                format!("thread-{tid}")
            };

            out.push((tid, name, cpu_us));
            let _ = mach_port_deallocate(task, port);
        }

        let size = (thread_count as usize).saturating_mul(mem::size_of::<libc::thread_act_t>());
        let _ = libc::vm_deallocate(
            task,
            thread_list as libc::vm_address_t,
            size as libc::vm_size_t,
        );
        out
    }
}

#[cfg(target_os = "macos")]
fn time_value_to_us(tv: libc::time_value_t) -> u64 {
    (tv.seconds as u64)
        .saturating_mul(1_000_000)
        .saturating_add(tv.microseconds as u64)
}

/// Human label for a Linux `comm` / thread name (15-char truncated).
///
/// Groups related workers (e.g. all `seedchamp-hash-*` → "piece hash") so the
/// Status THREADS table is readable.
pub fn pretty_thread_name(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return "unknown".into();
    }

    // Exact / prefix matches first (full names and TASK_COMM_LEN=16 truncations).
    // Order: longer / more specific prefixes before shorter ones.
    let lower = s.to_ascii_lowercase();

    // seedchamp workers -------------------------------------------------
    if matches_seed(&lower, "seedchamp-io") || lower.starts_with("seedchamp-io") {
        return "peer i/o".into();
    }
    // Dedicated Compio accept runtime (single listener + service tasks).
    if matches_seed(&lower, "seedchamp-acc") || lower == "seedchamp-acc" {
        return "accept".into();
    }
    // Dedicated Compio tracker runtime (HTTP announce via cyper).
    if matches_seed(&lower, "seedchamp-trk") || lower == "seedchamp-trk" {
        return "tracker".into();
    }
    // Compio asyncify / spawn_blocking pool (catalog SQLite, etc.).
    if matches_seed(&lower, "seedchamp-block") || lower.starts_with("seedchamp-bloc") {
        return "blocking pool".into();
    }
    if matches_seed(&lower, "seedchamp-hash") {
        // seedchamp-hash-0 … and truncated "seedchamp-hash-"
        return "piece hash".into();
    }
    // Disk backends: full `comm` keeps the suffix; Linux truncates both
    // seedchamp-disk-uring and seedchamp-disk-aio to "seedchamp-disk-" (15 chars).
    if lower == "seedchamp-disk-uring" {
        return "disk (io_uring)".into();
    }
    if lower == "seedchamp-disk-aio" {
        return "disk (aio)".into();
    }
    if lower == "seedchamp-disk" {
        return "disk (thread)".into();
    }
    if lower == "seedchamp-disk-" || lower.starts_with("seedchamp-disk") {
        return "disk".into();
    }
    if matches_seed(&lower, "seedchamp-control") || lower.starts_with("seedchamp-contr") {
        return "control plane".into();
    }
    if matches_seed(&lower, "seedchamp-mutate") || lower.starts_with("seedchamp-mutat") {
        return "catalog mutate".into();
    }
    // Catalog RO worker (`seedchamp-cread`, 15-char `comm` friendly).
    if matches_seed(&lower, "seedchamp-cread") || lower.starts_with("seedchamp-crea") {
        return "catalog reader".into();
    }
    if matches_seed(&lower, "seedchamp-watch") {
        return "watch dir".into();
    }
    if matches_seed(&lower, "seedchamp-serve") {
        return "serve loop".into();
    }
    if matches_seed(&lower, "seedchamp-recheck") || lower.starts_with("seedchamp-reche") {
        return "recheck".into();
    }
    if matches_seed(&lower, "seedchamp-tui-add") || lower.starts_with("seedchamp-tui-a") {
        return "tui add".into();
    }
    if matches_seed(&lower, "seedchamp-rt-drop") || lower.starts_with("seedchamp-rt-dr") {
        return "runtime teardown".into();
    }

    // Bare process name: main LWP **and** Compio spawn_blocking / asyncify
    // workers (unnamed `std::thread::spawn` → FreeBSD falls back to ki_comm
    // "seedchamp"). Count is often >1 for that reason.
    if lower == "seedchamp" || (lower.starts_with("seedchamp") && !lower.contains('-')) {
        return "main".into();
    }
    // Test / harness binary names (same: unnamed pool + primary thread)
    if lower.starts_with("seedchamp_engin") || lower == "seedchamp-engine" {
        return "main".into();
    }

    // Generic: strip common prefix, keep rest readable
    if let Some(rest) = s.strip_prefix("seedchamp-") {
        if !rest.is_empty() {
            return rest.replace('-', " ");
        }
    }

    s.to_string()
}

/// True if `comm` is `prefix` or a Linux-truncated form of it (`prefix` cut to 15 chars).
fn matches_seed(comm: &str, full: &str) -> bool {
    if comm == full || comm.starts_with(full) {
        return true;
    }
    // TASK_COMM_LEN is 16 bytes including NUL → 15 visible chars.
    let trunc: String = full.chars().take(15).collect();
    comm == trunc || comm.starts_with(&trunc)
}

/// utime+stime from `/proc/self/task/<tid>/stat` (not `/proc/<tid>/stat`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_task_jiffies(tid: u64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
    // comm may contain spaces/parens — split on last ')'
    let after = stat.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    // fields after ')': state(3)… utime is field 14 overall → index 11
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

fn sort_groups(map: HashMap<String, (u32, f32)>, cpu_ready: bool) -> Vec<ThreadGroup> {
    let mut groups: Vec<ThreadGroup> = map
        .into_iter()
        .map(|(name, (count, cpu))| ThreadGroup {
            name,
            count,
            cpu_pct: cpu_ready.then_some(cpu),
        })
        .collect();
    groups.sort_by(|a, b| {
        let cpu_ord = match (a.cpu_pct, b.cpu_pct) {
            (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        };
        cpu_ord
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

/// Process RSS in bytes; `None` if unavailable.
pub fn current_rss_bytes() -> Option<u64> {
    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return None;
    }
    let mut sys = System::new();
    let pid = get_current_pid().ok()?;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks().with_memory(),
    );
    sys.process(pid).map(|p| p.memory())
}

/// One filesystem (device) with free/total space for the Status screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemUsage {
    /// Filesystem **mount point** (not a torrent data_root subdirectory).
    pub path: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    /// This row includes the process default download root.
    pub is_default: bool,
    /// Open (`want_start`) torrents whose `data_root` lives on this FS.
    pub open_torrents: u32,
}

impl FilesystemUsage {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    /// Used fraction 0.0–1.0 (0 if total unknown).
    pub fn used_frac(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.used_bytes() as f64 / self.total_bytes as f64
        }
    }
}

/// Free/total space for the default download FS and every FS hosting open torrents.
///
/// - Always includes `default_root` (even with zero open torrents).
/// - `open_roots`: `data_root` of each open (`want_start`) torrent (duplicates OK).
/// - Groups by device id; cheap `statvfs` per unique device (TUI thread, ≤1 Hz).
/// - No catalog I/O; paths come from already-loaded list rows / config.
pub fn collect_filesystem_usage(
    default_root: &Path,
    open_roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<FilesystemUsage> {
    #[cfg(unix)]
    {
        collect_filesystem_usage_unix(default_root, open_roots)
    }
    #[cfg(not(unix))]
    {
        let _ = (default_root, open_roots);
        Vec::new()
    }
}

#[cfg(unix)]
fn collect_filesystem_usage_unix(
    default_root: &Path,
    open_roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<FilesystemUsage> {
    use std::os::unix::fs::MetadataExt;

    struct Acc {
        free: u64,
        total: u64,
        is_default: bool,
        open_torrents: u32,
        /// Mount point for this device (not a torrent subdirectory).
        mount: PathBuf,
    }

    let mut by_dev: HashMap<u64, Acc> = HashMap::new();

    let mut touch = |path: &Path, is_default: bool, count_torrent: bool| {
        let probe = resolve_existing_ancestor(path);
        let Ok(meta) = std::fs::metadata(&probe) else {
            return;
        };
        let dev = meta.dev();
        let Ok(st) = nix::sys::statvfs::statvfs(probe.as_path()) else {
            return;
        };
        let fr = st.fragment_size() as u64;
        if fr == 0 {
            return;
        }
        let total = (st.blocks() as u64).saturating_mul(fr);
        // Available to unprivileged users (matches `df` "Avail").
        let free = (st.blocks_available() as u64).saturating_mul(fr);
        let mount = filesystem_mount_point(&probe, dev);

        let e = by_dev.entry(dev).or_insert_with(|| Acc {
            free,
            total,
            is_default: false,
            open_torrents: 0,
            mount,
        });
        // Refresh sizes (same FS; values may change between paths rarely).
        e.free = free;
        e.total = total;
        if is_default {
            e.is_default = true;
        }
        if count_torrent {
            e.open_torrents = e.open_torrents.saturating_add(1);
        }
    };

    // Default download root first (always).
    touch(default_root, true, false);

    for root in open_roots {
        if root.as_os_str().is_empty() {
            continue;
        }
        // Count as open torrent; may be same device as default.
        touch(&root, false, true);
    }

    let mut out: Vec<FilesystemUsage> = by_dev
        .into_values()
        .map(|acc| FilesystemUsage {
            path: shorten_path_display(&acc.mount),
            free_bytes: acc.free,
            total_bytes: acc.total,
            is_default: acc.is_default,
            open_torrents: acc.open_torrents,
        })
        .collect();

    // Default FS first, then more open torrents, then path.
    out.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| b.open_torrents.cmp(&a.open_torrents))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Walk up to an existing ancestor so `statvfs` works before dirs are created.
#[cfg(unix)]
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            if let Ok(c) = p.canonicalize() {
                return c;
            }
            return p;
        }
        if !p.pop() {
            // Fall back to cwd / root
            return PathBuf::from("/");
        }
    }
}

/// Top-most path on `dev` walking toward `/` — the filesystem mount point.
///
/// Avoids labeling Status rows with torrent directories like
/// `/zfs/storage/movies/hd/FLY (2024)` when the mount is `/zfs/storage/movies/hd`.
#[cfg(unix)]
fn filesystem_mount_point(path: &Path, dev: u64) -> PathBuf {
    use std::os::unix::fs::MetadataExt;

    let mut cur = path.to_path_buf();
    loop {
        let Some(parent) = cur.parent() else {
            break;
        };
        // Root: parent of "/" is "" or same.
        if parent.as_os_str().is_empty() || parent == cur {
            break;
        }
        let Ok(pm) = std::fs::metadata(parent) else {
            break;
        };
        if pm.dev() != dev {
            // `cur` is the first path still on this device → mount point.
            break;
        }
        cur = parent.to_path_buf();
    }
    cur
}

/// Replace `$HOME` prefix with `~` for Status path display.
fn shorten_path_display(path: &Path) -> String {
    let s = path.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if s == home {
                return "~".into();
            }
            let prefix = format!("{home}/");
            if let Some(rest) = s.strip_prefix(&prefix) {
                return format!("~/{rest}");
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn filesystem_usage_includes_default_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let fs = collect_filesystem_usage(&root, std::iter::empty::<PathBuf>());
        assert!(
            !fs.is_empty(),
            "expected at least default FS stats for {root:?}"
        );
        assert!(fs.iter().any(|f| f.is_default));
        assert!(fs[0].total_bytes > 0);
        // Label is a mount point, not the nested temp path (unless temp is its own FS).
        assert!(
            !fs[0].path.contains("filesystem_usage"),
            "label should be mount point, got {}",
            fs[0].path
        );
    }

    #[test]
    fn filesystem_usage_counts_open_torrents_same_fs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let a = root.join("a");
        let b = root.join("b");
        let _ = std::fs::create_dir_all(&a);
        let _ = std::fs::create_dir_all(&b);
        let fs = collect_filesystem_usage(&root, [a, b]);
        let def = fs.iter().find(|f| f.is_default).expect("default fs");
        assert_eq!(def.open_torrents, 2);
        // Nested torrent dirs must not appear as the row label.
        assert!(
            !def.path.ends_with("/a") && !def.path.ends_with("/b"),
            "expected mount point label, got {}",
            def.path
        );
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_mount_point_walks_to_device_root() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("deep").join("torrent-name");
        std::fs::create_dir_all(&nested).unwrap();
        let probe = nested.canonicalize().unwrap();
        let dev = std::fs::metadata(&probe).unwrap().dev();
        let mnt = filesystem_mount_point(&probe, dev);
        // Mount is a prefix of the nested path (or equal if nested is the mount).
        assert!(
            probe.starts_with(&mnt),
            "mount {mnt:?} should be ancestor of {probe:?}"
        );
        // And parent of mount (if any) is a different device or absent.
        if let Some(parent) = mnt.parent() {
            if !parent.as_os_str().is_empty() {
                if let Ok(pm) = std::fs::metadata(parent) {
                    assert_ne!(pm.dev(), dev, "parent of mount should leave the device");
                }
            }
        }
    }

    #[test]
    fn pretty_names_map_seedchamp_workers() {
        assert_eq!(pretty_thread_name("seedchamp-io"), "peer i/o");
        assert_eq!(pretty_thread_name("seedchamp-acc"), "accept");
        assert_eq!(pretty_thread_name("seedchamp-trk"), "tracker");
        assert_eq!(pretty_thread_name("seedchamp-block"), "blocking pool");
        assert_eq!(pretty_thread_name("seedchamp-bloc"), "blocking pool");
        assert_eq!(pretty_thread_name("seedchamp-hash-0"), "piece hash");
        assert_eq!(pretty_thread_name("seedchamp-hash-12"), "piece hash");
        // 15-char truncation of seedchamp-hash-0
        assert_eq!(pretty_thread_name("seedchamp-hash-"), "piece hash");
        assert_eq!(
            pretty_thread_name("seedchamp-disk-uring"),
            "disk (io_uring)"
        );
        assert_eq!(pretty_thread_name("seedchamp-disk-aio"), "disk (aio)");
        assert_eq!(pretty_thread_name("seedchamp-disk"), "disk (thread)");
        // Both uring and aio truncate to the same 15-char comm:
        assert_eq!(pretty_thread_name("seedchamp-disk-"), "disk");
        assert_eq!(pretty_thread_name("seedchamp-control"), "control plane");
        assert_eq!(pretty_thread_name("seedchamp-contr"), "control plane");
        assert_eq!(pretty_thread_name("seedchamp-mutate"), "catalog mutate");
        assert_eq!(pretty_thread_name("seedchamp-mutat"), "catalog mutate");
        assert_eq!(pretty_thread_name("seedchamp-cread"), "catalog reader");
        assert_eq!(pretty_thread_name("seedchamp-crea"), "catalog reader");
        assert_eq!(pretty_thread_name("seedchamp-watch"), "watch dir");
        assert_eq!(pretty_thread_name("seedchamp-recheck-9"), "recheck");
        assert_eq!(pretty_thread_name("seedchamp-reche"), "recheck");
        assert_eq!(pretty_thread_name("seedchamp"), "main");
        assert_eq!(pretty_thread_name("seedchamp_engin"), "main");
        // unknown kept (no Tokio runtime in process)
        assert_eq!(pretty_thread_name("custom-worker"), "custom-worker");
    }

    #[test]
    fn current_rss_does_not_panic() {
        let _ = current_rss_bytes();
    }

    #[test]
    fn collect_does_not_panic() {
        let mut st = ProcessSampleState::new();
        let s1 = st.collect();
        assert_eq!(s1.pid, std::process::id());
        thread::sleep(Duration::from_millis(50));
        let s2 = st.collect();
        assert_eq!(s2.pid, s1.pid);

        if sysinfo::IS_SUPPORTED_SYSTEM {
            assert!(s1.available && s2.available);
            assert!(s1.rss_bytes.is_some() && s2.rss_bytes.is_some());
            assert!(s1.cpu_pct.is_none());
            assert!(s2.cpu_pct.is_some());
        } else {
            assert!(!s1.available);
        }
    }

    #[test]
    fn linux_task_stat_differs_from_proc_tid_stat() {
        // Documents the kernel quirk that broke sysinfo-based thread CPU.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let (tx, rx) = mpsc::channel::<()>();
            let h = thread::Builder::new()
                .name("stat-path-test".into())
                .spawn(move || {
                    let t0 = Instant::now();
                    let mut x = 0u64;
                    while t0.elapsed() < Duration::from_millis(200) {
                        x = x.wrapping_add(1);
                    }
                    std::hint::black_box(x);
                    let _ = rx.recv();
                })
                .unwrap();
            thread::sleep(Duration::from_millis(50));

            // Find our busy tid via /proc/self/task
            let mut busy_tid = None;
            for ent in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
                let tid = ent.file_name().to_string_lossy().to_string();
                let comm = std::fs::read_to_string(ent.path().join("comm")).unwrap_or_default();
                if comm.trim() == "stat-path-test" {
                    busy_tid = tid.parse().ok();
                    break;
                }
            }
            let tid = busy_tid.expect("busy tid");
            let task_j = read_task_jiffies(tid).unwrap_or(0);
            // /proc/TID/stat (sysinfo path) — often disagrees
            let proc_stat = std::fs::read_to_string(format!("/proc/{tid}/stat")).unwrap();
            let after = proc_stat.rsplit_once(')').unwrap().1;
            let mut f = after.split_whitespace();
            let ut: u64 = f.nth(11).unwrap().parse().unwrap();
            let st: u64 = f.next().unwrap().parse().unwrap();
            let proc_j = ut + st;
            // Busy thread should have non-zero task jiffies.
            assert!(task_j > 0, "task jiffies={task_j}");
            // They may differ; at least task path is the one we trust for CPU.
            let _ = proc_j;
            drop(tx);
            let _ = h.join();
        }
    }

    #[test]
    fn linux_thread_groups_when_named_threads_exist() {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return;
        }
        let keepers: Vec<_> = (0..4)
            .map(|i| {
                let (tx, rx) = mpsc::channel::<()>();
                let name = if i < 2 {
                    "sc-test-io".into()
                } else {
                    "sc-test-hash".into()
                };
                thread::Builder::new()
                    .name(name)
                    .spawn(move || {
                        let _ = rx.recv();
                    })
                    .unwrap();
                tx
            })
            .collect();
        thread::sleep(Duration::from_millis(20));
        let mut st = ProcessSampleState::new();
        let s1 = st.collect();
        thread::sleep(Duration::from_millis(50));
        let sample = st.collect();
        drop(keepers);

        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos"
        ))]
        {
            let names: Vec<&str> = sample
                .thread_groups
                .iter()
                .map(|g| g.name.as_str())
                .collect();
            assert!(
                names.iter().any(|n| n.contains("sc-test-io"))
                    || names.iter().any(|n| n.contains("sc-test-hash")),
                "groups={:?}",
                sample.thread_groups
            );
            assert!(sample.threads.unwrap_or(0) >= 4);
            assert!(
                sample.thread_groups.iter().any(|g| g.cpu_pct.is_some())
                    || s1.thread_groups.is_empty()
            );
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos"
        )))]
        {
            let _ = s1;
            assert!(sample.thread_groups.is_empty());
        }
    }

    #[test]
    fn idle_cpu_near_zero() {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return;
        }
        let keepers: Vec<_> = (0..8)
            .map(|i| {
                let (tx, rx) = mpsc::channel::<()>();
                thread::Builder::new()
                    .name(format!("idle-t-{i}"))
                    .spawn(move || {
                        let _ = rx.recv();
                    })
                    .unwrap();
                tx
            })
            .collect();
        thread::sleep(Duration::from_millis(30));
        let mut st = ProcessSampleState::new();
        let _ = st.collect();
        thread::sleep(Duration::from_millis(1000));
        let s = st.collect();
        drop(keepers);

        let proc_cpu = s.cpu_pct.unwrap_or(0.0);
        assert!(proc_cpu < 5.0, "process cpu inflated: {proc_cpu}% {s:?}");
        for g in &s.thread_groups {
            if let Some(c) = g.cpu_pct {
                // Parked workers should be ~0; main test thread may use a little.
                if g.name.starts_with("idle-t-") {
                    assert!(c < 2.0, "idle group {} cpu={c}%", g.name);
                }
            }
        }
    }

    #[test]
    fn busy_thread_shows_cpu() {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return;
        }
        let (tx, rx) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name("sc-busy".into())
            .spawn(move || {
                let t0 = Instant::now();
                let mut x = 0u64;
                while t0.elapsed() < Duration::from_millis(800) {
                    x = x.wrapping_mul(7).wrapping_add(1);
                }
                std::hint::black_box(x);
                let _ = rx.recv();
            })
            .unwrap();

        let mut st = ProcessSampleState::new();
        let _ = st.collect();
        thread::sleep(Duration::from_millis(400));
        let s = st.collect();
        drop(tx);
        let _ = handle.join();

        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "macos"
        ))]
        {
            let busy = s
                .thread_groups
                .iter()
                .find(|g| g.name.contains("sc-busy"))
                .and_then(|g| g.cpu_pct);
            assert!(
                busy.unwrap_or(0.0) > 20.0,
                "expected busy high cpu, got {busy:?} groups={:?}",
                s.thread_groups
            );
            // Idle-named groups shouldn't steal that budget
            let proc = s.cpu_pct.unwrap_or(0.0);
            assert!(
                proc > 20.0,
                "process cpu should include busy thread: {proc}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_list_threads_sees_named_thread() {
        let (tx, rx) = mpsc::channel::<()>();
        let h = thread::Builder::new()
            .name("sc-macos-lwp".into())
            .spawn(move || {
                let _ = rx.recv();
            })
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let list = macos_list_threads();
        drop(tx);
        let _ = h.join();
        assert!(
            list.iter().any(|(_, n, _)| n.contains("sc-macos-lwp")),
            "expected named thread: {list:?}"
        );
        assert!(list.len() >= 2, "main + worker: {list:?}");
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn freebsd_list_lwps_sees_named_thread() {
        let (tx, rx) = mpsc::channel::<()>();
        let h = thread::Builder::new()
            .name("sc-fbsd-lwp".into())
            .spawn(move || {
                let _ = rx.recv();
            })
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let lwps = freebsd_list_lwps();
        drop(tx);
        let _ = h.join();
        assert!(
            lwps.iter().any(|(_, n, _)| n.contains("sc-fbsd-lwp")),
            "expected named LWP in kinfo_proc list: {lwps:?}"
        );
        assert!(lwps.len() >= 2, "main + worker: {lwps:?}");
    }
}
