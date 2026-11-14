//! Processes (/proc/[pid]/stat, cmdline): applications grouped by name with
//! their total CPU and memory, plus counts of running, blocked (D state)
//! and zombie tasks. Live only (not stored in history).
//!
//! The application name comes from the executable in `argv[0]`, so the
//! many processes of a browser or an Electron app form one group. For
//! Chromium-based programs (`--type=`) and Firefox (`-contentproc`) each
//! process also gets a type (main, renderer, extension, GPU, utility, ...).
//! The full per-process list is kept as `detail` for `/api/processes`.

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::counter::TickClock;
use crate::util::parse::{clock_ticks, key_number_lines, page_size};
use crate::util::sysfs::{Roots, list_dir, read_opt};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

const TOP_N: usize = 15;
const CMD_MAX: usize = 400;

#[derive(Debug, Clone, PartialEq)]
pub struct PidStat {
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub ticks: u64,
    pub threads: u64,
    pub start: u64,
    pub rss_pages: u64,
}

/// Parse /proc/[pid]/stat. The command name may contain spaces and
/// parentheses, so fields are located relative to the last ')'.
pub fn parse_pid_stat(s: &str) -> Option<PidStat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = s[open + 1..close].to_string();
    let rest: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    let n = |i: usize| rest.get(i).and_then(|x| x.parse::<u64>().ok());
    Some(PidStat {
        comm,
        state: rest.first()?.chars().next()?,
        ppid: n(1).unwrap_or(0) as u32,
        ticks: n(11)?.saturating_add(n(12)?),
        threads: n(17).unwrap_or(1),
        start: n(19).unwrap_or(0),
        rss_pages: n(21).unwrap_or(0),
    })
}

/// Application name for grouping: the executable name from argv[0]
/// (kernel threads have no command line and form one group).
pub fn app_name(argv: &[&str], comm: &str, state: char) -> String {
    let Some(arg0) = argv.first().filter(|a| !a.is_empty()) else {
        return if state == 'Z' {
            comm.to_string()
        } else {
            "kernel threads".to_string()
        };
    };
    // Chromium/Electron rewrite their command line into one space-separated
    // string: keep the executable before the first option.
    let arg0 = match arg0.split_once(" -") {
        Some((head, _)) if argv.len() == 1 => head,
        _ => arg0,
    };
    // "nginx: worker process", "sshd: user@pts/0" (set by setproctitle)
    let first = arg0.split(": ").next().unwrap_or(arg0);
    let base = first.rsplit('/').next().unwrap_or(first);
    let base = base.split(' ').next().unwrap_or(base).trim();
    if base.is_empty() {
        comm.to_string()
    } else {
        base.to_string()
    }
}

fn utility_name(sub: &str) -> String {
    match sub {
        "network.mojom.NetworkService" => "network service".into(),
        "audio.mojom.AudioService" => "audio service".into(),
        "storage.mojom.StorageService" => "storage service".into(),
        "video_capture.mojom.VideoCaptureService" => "video capture".into(),
        "data_decoder.mojom.DataDecoderService" => "data decoder".into(),
        "proxy_resolver.mojom.ProxyResolverFactory" => "proxy resolver".into(),
        "on_device_model.mojom.OnDeviceModelService" => "on-device AI model".into(),
        other => other.split('.').next().unwrap_or(other).replace('_', " "),
    }
}

/// Process type for multi-process programs (Chromium/Electron, Firefox).
pub fn process_kind(argv: &[&str]) -> Option<String> {
    if let Some(t) = argv.iter().find_map(|a| a.strip_prefix("--type=")) {
        let kind = match t {
            "renderer" => {
                if argv.contains(&"--extension-process") {
                    "extension".to_string()
                } else {
                    "renderer (tabs, frames)".to_string()
                }
            }
            "gpu-process" => "GPU process".into(),
            "utility" => match argv
                .iter()
                .find_map(|a| a.strip_prefix("--utility-sub-type="))
            {
                Some(sub) => format!("utility: {}", utility_name(sub)),
                None => "utility".into(),
            },
            "zygote" => "zygote (process launcher)".into(),
            "crashpad-handler" => "crash reporter".into(),
            "broker" => "broker".into(),
            other => other.to_string(),
        };
        return Some(kind);
    }
    if argv.contains(&"-contentproc") {
        let kind = match argv.last().copied().unwrap_or("") {
            "tab" => "content (tabs)",
            "rdd" => "media decoder",
            "gpu" => "GPU process",
            "socket" => "network",
            "utility" => "utility",
            "forkserver" => "fork server",
            "vr" => "VR",
            _ => "content",
        };
        return Some(kind.to_string());
    }
    None
}

#[derive(Debug, Clone)]
struct Meta {
    start: u64,
    app: String,
    cmd: String,
    user: String,
    kind: Option<String>,
}

struct Proc {
    pid: u32,
    st: PidStat,
    cpu: f64,
}

#[derive(Default)]
struct App {
    count: usize,
    cpu: f64,
    rss: u64,
    threads: u64,
    users: BTreeSet<String>,
    multi: bool,
    pids: Vec<u32>,
}

pub struct Processes {
    roots: Roots,
    descs: DescCache,
    clock: TickClock,
    prev: HashMap<u32, (u64, u64)>,
    meta: HashMap<u32, Meta>,
    users: HashMap<u32, String>,
    users_at: Option<Instant>,
    tck: f64,
    page: u64,
}

impl Processes {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            clock: TickClock::default(),
            prev: HashMap::new(),
            meta: HashMap::new(),
            users: HashMap::new(),
            users_at: None,
            tck: clock_ticks() as f64,
            page: page_size(),
        }
    }

    fn load_users(&mut self, now: Instant) {
        if self
            .users_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(300))
        {
            return;
        }
        self.users_at = Some(now);
        self.users = crate::util::users::passwd_names();
    }

    /// Identity of a process, read once per process lifetime.
    fn meta_for(&mut self, pid: u32, st: &PidStat) -> Meta {
        if let Some(m) = self.meta.get(&pid)
            && m.start == st.start
        {
            return m.clone();
        }
        let raw = std::fs::read(self.roots.proc(&format!("{pid}/cmdline"))).unwrap_or_default();
        let text = String::from_utf8_lossy(&raw);
        let argv: Vec<&str> = text.split('\0').filter(|x| !x.is_empty()).collect();
        let app = app_name(&argv, &st.comm, st.state);
        // Options may be NUL- or space-separated (rewritten command lines).
        let tokens: Vec<&str> = text.split(['\0', ' ']).filter(|x| !x.is_empty()).collect();
        let mut cmd = argv.join(" ");
        if cmd.is_empty() {
            cmd = format!("[{}]", st.comm);
        } else if cmd.chars().count() > CMD_MAX {
            cmd = cmd.chars().take(CMD_MAX).collect::<String>() + "…";
        }
        let user = match std::fs::metadata(self.roots.proc(&pid.to_string())) {
            Ok(m) => self
                .users
                .get(&m.uid())
                .cloned()
                .unwrap_or_else(|| m.uid().to_string()),
            Err(_) => String::new(),
        };
        let m = Meta {
            start: st.start,
            app,
            cmd,
            user,
            kind: process_kind(&tokens),
        };
        self.meta.insert(pid, m.clone());
        m
    }
}

impl Collector for Processes {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        self.load_users(ctx.now);
        let dt = self.clock.tick(ctx.now, ctx.interval);
        let mem_total = std::fs::read_to_string(self.roots.proc("meminfo"))
            .ok()
            .and_then(|s| {
                key_number_lines(&s)
                    .find(|(k, _)| *k == "MemTotal")
                    .map(|(_, v)| v * 1024)
            })
            .unwrap_or(0);
        let uptime = read_opt(&self.roots.proc("uptime"))
            .and_then(|s| {
                s.split_whitespace()
                    .next()
                    .and_then(|x| x.parse::<f64>().ok())
            })
            .unwrap_or(0.0);

        let mut procs = Vec::with_capacity(512);
        let mut new_prev = HashMap::with_capacity(512);
        let (mut running, mut blocked, mut zombie, mut threads) = (0u64, 0u64, 0u64, 0u64);
        for (name, dir) in list_dir(&self.roots.proc) {
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            // Processes may exit between listing and reading: just skip them.
            let Ok(s) = std::fs::read_to_string(dir.join("stat")) else {
                continue;
            };
            let Some(st) = parse_pid_stat(&s) else {
                continue;
            };
            match st.state {
                'R' => running += 1,
                'D' => blocked += 1,
                'Z' => zombie += 1,
                _ => {}
            }
            threads += st.threads;
            let cpu = match (dt, self.prev.get(&pid)) {
                (Some(dt), Some((start, ticks))) if *start == st.start => {
                    st.ticks.saturating_sub(*ticks) as f64 / self.tck / dt * 100.0
                }
                _ => 0.0,
            };
            new_prev.insert(pid, (st.start, st.ticks));
            procs.push(Proc { pid, st, cpu });
        }
        self.prev = new_prev;
        let alive: std::collections::HashSet<u32> = procs.iter().map(|p| p.pid).collect();
        self.meta.retain(|pid, _| alive.contains(pid));

        let total = procs.len();
        let mut push = |key: &str, metric: &str, label: &str, v: f64| {
            let d = self.descs.get(key, || {
                SeriesDesc::new("processes", key, metric, label, "count")
            });
            out.push(&d, v);
        };
        push("proc.count", "count", "Processes", total as f64);
        push("proc.threads", "threads", "Threads", threads as f64);
        push("proc.running", "running", "Running", running as f64);
        push(
            "proc.blocked",
            "blocked",
            "Uninterruptible (D)",
            blocked as f64,
        );
        push("proc.zombie", "zombie", "Zombies", zombie as f64);

        let page = self.page;
        let pct_mem = |rss: u64| {
            if mem_total > 0 {
                (rss as f64 / mem_total as f64 * 1000.0).round() / 10.0
            } else {
                0.0
            }
        };
        let mut apps: HashMap<String, App> = HashMap::new();
        let mut detail = Vec::with_capacity(procs.len());
        for p in &procs {
            let m = self.meta_for(p.pid, &p.st);
            let rss = p.st.rss_pages * page;
            let a = apps.entry(m.app.clone()).or_default();
            a.count += 1;
            a.cpu += p.cpu;
            a.rss += rss;
            a.threads += p.st.threads;
            a.multi |= m.kind.is_some();
            a.pids.push(p.pid);
            if !m.user.is_empty() {
                a.users.insert(m.user.clone());
            }
            let age = (uptime - p.st.start as f64 / self.tck).max(0.0);
            detail.push(json!({
                "pid": p.pid,
                "ppid": p.st.ppid,
                "app": m.app,
                "kind": m.kind,
                "user": m.user,
                "cpu": (p.cpu * 10.0).round() / 10.0,
                "rss": rss,
                "mem": pct_mem(rss),
                "threads": p.st.threads,
                "state": p.st.state.to_string(),
                "age_s": age.round(),
                "cmd": m.cmd,
            }));
        }
        let app_json = |name: &String, a: &App| {
            json!({
                "name": name,
                "count": a.count,
                "cpu": (a.cpu * 10.0).round() / 10.0,
                "rss": a.rss,
                "mem": pct_mem(a.rss),
                "threads": a.threads,
                "users": a.users.iter().collect::<Vec<_>>(),
                "multi": a.multi,
                "pids": a.pids,
            })
        };
        let mut list: Vec<(&String, &App)> = apps.iter().collect();
        list.sort_by(|x, y| y.1.cpu.total_cmp(&x.1.cpu).then(y.1.rss.cmp(&x.1.rss)));
        let apps_cpu: Vec<Value> = list
            .iter()
            .take(TOP_N)
            .map(|(n, a)| app_json(n, a))
            .collect();
        list.sort_by_key(|x| std::cmp::Reverse(x.1.rss));
        let apps_mem: Vec<Value> = list
            .iter()
            .take(TOP_N)
            .map(|(n, a)| app_json(n, a))
            .collect();
        out.extra = Some(json!({
            "ts": ctx.wall_ms,
            "count": total,
            "threads": threads,
            "apps": apps.len(),
            "apps_cpu": apps_cpu,
            "apps_mem": apps_mem,
        }));
        out.detail = Some(Value::Array(detail));
        out.health = if total > 0 {
            Health::ok("/proc/[pid]/stat")
        } else {
            Health::unavailable("no processes visible in /proc")
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_with_parens_and_spaces() {
        let s = "1234 (Web Content (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 31 0 98765 1000000 2500 18446744073709551615";
        let p = parse_pid_stat(s).unwrap();
        assert_eq!(p.comm, "Web Content (x)");
        assert_eq!(p.state, 'S');
        assert_eq!(p.ppid, 1);
        assert_eq!(p.ticks, 300);
        assert_eq!(p.threads, 31);
        assert_eq!(p.start, 98765);
        assert_eq!(p.rss_pages, 2500);
        assert!(parse_pid_stat("garbage").is_none());
        assert!(parse_pid_stat("1 (x) S").is_none());
    }

    #[test]
    fn app_names() {
        assert_eq!(
            app_name(
                &["/opt/google/chrome/chrome", "--type=renderer"],
                "chrome",
                'S'
            ),
            "chrome"
        );
        assert_eq!(app_name(&["nginx: worker process"], "nginx", 'S'), "nginx");
        assert_eq!(
            app_name(
                &[
                    "/usr/lib/slack/slack --type=renderer --user-data-dir=/x /usr/lib/slack/resources/app.asar"
                ],
                "slack",
                'S'
            ),
            "slack"
        );
        assert_eq!(app_name(&["sshd: user@pts/0"], "sshd", 'S'), "sshd");
        assert_eq!(app_name(&[], "kworker/0:1", 'I'), "kernel threads");
        assert_eq!(app_name(&[], "defunct", 'Z'), "defunct");
        assert_eq!(
            app_name(
                &["/usr/lib/firefox/firefox", "-contentproc"],
                "Isolated Web Co",
                'S'
            ),
            "firefox"
        );
    }

    #[test]
    fn browser_kinds() {
        assert_eq!(
            process_kind(&["chrome", "--type=renderer"]).as_deref(),
            Some("renderer (tabs, frames)")
        );
        assert_eq!(
            process_kind(&["chrome", "--type=renderer", "--extension-process"]).as_deref(),
            Some("extension")
        );
        assert_eq!(
            process_kind(&[
                "chrome",
                "--type=utility",
                "--utility-sub-type=network.mojom.NetworkService"
            ])
            .as_deref(),
            Some("utility: network service")
        );
        assert_eq!(
            process_kind(&["chrome", "--type=gpu-process"]).as_deref(),
            Some("GPU process")
        );
        assert_eq!(
            process_kind(&["firefox", "-contentproc", "123", "true", "tab"]).as_deref(),
            Some("content (tabs)")
        );
        assert_eq!(process_kind(&["/opt/google/chrome/chrome"]), None);
    }
}
