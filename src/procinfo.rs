//! Details of one process, read on demand from /proc/<pid> for
//! `/api/process`.
//!
//! Every file is optional and size-capped. The kernel shows some files only
//! to the process owner or root (`exe`, `cwd`, `io`, `fd`); when a read is
//! refused, the field is `null` and its name is listed in `restricted`.
//! `environ` is never read.

use crate::collectors::processes::{app_name, process_kind};
use crate::util::parse::{clock_ticks, key_number_lines, page_size};
use crate::util::sysfs::{Roots, list_dir, read_opt};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const CMDLINE_MAX: u64 = 64 * 1024;
const ARGS_MAX: usize = 512;
const ARG_CHARS_MAX: usize = 4096;
const SMALL_FILE_MAX: u64 = 16 * 1024;
const THREADS_READ_MAX: usize = 1024;
const THREADS_SHOWN: usize = 256;
const FDS_COUNT_MAX: usize = 1_000_000;

/// Fields of /proc/<pid>/stat used by the details view.
#[derive(Debug, Clone, PartialEq)]
pub struct StatFull {
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub pgrp: i64,
    pub session: i64,
    pub utime: u64,
    pub stime: u64,
    pub cutime: u64,
    pub cstime: u64,
    pub priority: i64,
    pub nice: i64,
    pub threads: u64,
    pub start: u64,
    pub vsize: u64,
    pub rss_pages: u64,
    pub processor: Option<u32>,
    pub rt_priority: u64,
    pub policy: u64,
}

/// Parse /proc/<pid>/stat (or task/<tid>/stat). Fields are located relative
/// to the last ')' because the command name may contain spaces and ')'.
pub fn parse_stat_full(s: &str) -> Option<StatFull> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close <= open {
        return None;
    }
    let rest: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    // rest[0] is field 3 (state) of proc(5).
    let u = |i: usize| rest.get(i).and_then(|x| x.parse::<u64>().ok());
    let i = |i: usize| rest.get(i).and_then(|x| x.parse::<i64>().ok());
    Some(StatFull {
        comm: s[open + 1..close].to_string(),
        state: rest.first()?.chars().next()?,
        ppid: u(1).unwrap_or(0) as u32,
        pgrp: i(2).unwrap_or(0),
        session: i(3).unwrap_or(0),
        utime: u(11)?,
        stime: u(12)?,
        cutime: u(13).unwrap_or(0),
        cstime: u(14).unwrap_or(0),
        priority: i(15).unwrap_or(0),
        nice: i(16).unwrap_or(0),
        threads: u(17).unwrap_or(1),
        start: u(19).unwrap_or(0),
        vsize: u(20).unwrap_or(0),
        rss_pages: u(21).unwrap_or(0),
        processor: u(36).map(|x| x as u32),
        rt_priority: u(37).unwrap_or(0),
        policy: u(38).unwrap_or(0),
    })
}

pub fn policy_name(p: u64) -> &'static str {
    match p {
        0 => "normal",
        1 => "real-time FIFO",
        2 => "real-time round robin",
        3 => "batch",
        5 => "idle",
        6 => "deadline",
        _ => "other",
    }
}

/// The systemd unit from a /proc/<pid>/cgroup file: the last `.scope` or
/// `.service` component of the unified (or name=systemd) hierarchy path.
pub fn cgroup_unit(cgroup: &str) -> (Option<String>, Option<String>) {
    let path = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .or_else(|| {
            cgroup
                .lines()
                .find_map(|l| l.split_once(":name=systemd:").map(|(_, p)| p))
        })
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let Some(path) = path else {
        return (None, None);
    };
    let unit = path
        .rsplit('/')
        .find(|c| c.ends_with(".scope") || c.ends_with(".service"))
        .map(unescape_systemd);
    (Some(path.to_string()), unit)
}

/// Decode systemd `\xNN` escapes ("google\x2dchrome" → "google-chrome").
fn unescape_systemd(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(h) = s.get(i + 2..i + 4)
            && let Ok(v) = u8::from_str_radix(h, 16)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Soft and hard "Max open files" from /proc/<pid>/limits (`None` = unlimited).
pub fn open_files_limit(limits: &str) -> Option<(Option<u64>, Option<u64>)> {
    let rest = limits
        .lines()
        .find_map(|l| l.strip_prefix("Max open files"))?;
    let mut it = rest.split_whitespace();
    let p = |x: Option<&str>| x.and_then(|v| v.parse::<u64>().ok());
    Some((p(it.next()), p(it.next())))
}

fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_text(path: &Path) -> io::Result<String> {
    read_capped(path, SMALL_FILE_MAX).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Split a command line into arguments. Programs that rewrite their command
/// line into one space-separated string (Chromium, Electron) are split
/// before each `--option` for display.
pub fn split_args(raw: &[u8]) -> (Vec<String>, bool) {
    let text = String::from_utf8_lossy(raw);
    let mut args: Vec<String> = text
        .split('\0')
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect();
    if args.len() == 1 && args[0].contains(" --") {
        let one = args.remove(0);
        let mut cur = String::new();
        for part in one.split(' ') {
            if part.starts_with("--") && !cur.is_empty() {
                args.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(part);
        }
        if !cur.is_empty() {
            args.push(cur);
        }
    }
    let mut truncated = raw.len() as u64 >= CMDLINE_MAX;
    if args.len() > ARGS_MAX {
        args.truncate(ARGS_MAX);
        truncated = true;
    }
    for a in &mut args {
        if a.chars().count() > ARG_CHARS_MAX {
            *a = a.chars().take(ARG_CHARS_MAX).collect::<String>() + "…";
            truncated = true;
        }
    }
    (args, truncated)
}

fn is_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

/// Read everything the details view shows about `pid`. `None` when the
/// process does not exist.
pub fn inspect(roots: &Roots, pid: u32, users: &HashMap<u32, String>) -> Option<Value> {
    let dir = roots.proc(&pid.to_string());
    let st = parse_stat_full(&read_text(&dir.join("stat")).ok()?)?;
    let tck = clock_ticks() as f64;
    let page = page_size();
    let mut restricted: Vec<&str> = Vec::new();
    let mut o = Map::new();
    let secs = |t: u64| (t as f64 / tck * 100.0).round() / 100.0;

    // Identity.
    let raw_cmd = read_capped(&dir.join("cmdline"), CMDLINE_MAX).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw_cmd);
    let argv: Vec<&str> = text.split('\0').filter(|x| !x.is_empty()).collect();
    let tokens: Vec<&str> = text.split(['\0', ' ']).filter(|x| !x.is_empty()).collect();
    let (args, args_truncated) = split_args(&raw_cmd);
    let status = read_text(&dir.join("status")).unwrap_or_default();
    let field = |k: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(':')))
            .map(str::trim)
    };
    let kernel_thread = field("Kthread") == Some("1") || (argv.is_empty() && st.state != 'Z');
    o.insert("pid".into(), json!(pid));
    o.insert("start_ticks".into(), json!(st.start));
    o.insert("comm".into(), json!(st.comm));
    o.insert("app".into(), json!(app_name(&argv, &st.comm, st.state)));
    o.insert("kind".into(), json!(process_kind(&tokens)));
    o.insert("state".into(), json!(st.state.to_string()));
    o.insert("ppid".into(), json!(st.ppid));
    o.insert("pgrp".into(), json!(st.pgrp));
    o.insert("session".into(), json!(st.session));
    o.insert("kernel_thread".into(), json!(kernel_thread));
    o.insert("args".into(), json!(args));
    o.insert("args_truncated".into(), json!(args_truncated));

    let name_of = |uid: u32| users.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
    let ids = |k: &str| -> Vec<u32> {
        field(k)
            .map(|v| {
                v.split_whitespace()
                    .filter_map(|x| x.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let uids = ids("Uid");
    let uid = uids
        .first()
        .copied()
        .or_else(|| std::fs::metadata(&dir).ok().map(|m| m.uid()));
    if let Some(uid) = uid {
        o.insert("uid".into(), json!(uid));
        o.insert("user".into(), json!(name_of(uid)));
        if let Some(&euid) = uids.get(1)
            && euid != uid
        {
            o.insert("euid".into(), json!(euid));
            o.insert("euser".into(), json!(name_of(euid)));
        }
    }
    if let Some(&gid) = ids("Gid").first() {
        o.insert("gid".into(), json!(gid));
    }

    // Time.
    let btime = read_opt(&roots.proc("stat")).and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("btime "))
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    let uptime = read_opt(&roots.proc("uptime")).and_then(|s| {
        s.split_whitespace()
            .next()
            .and_then(|x| x.parse::<f64>().ok())
    });
    let start_s = st.start as f64 / tck;
    if let Some(b) = btime {
        o.insert(
            "started_ms".into(),
            json!(((b as f64 + start_s) * 1000.0).round() as u64),
        );
    }
    if let Some(up) = uptime {
        o.insert("age_s".into(), json!((up - start_s).max(0.0).round()));
    }

    // CPU and scheduling.
    o.insert("user_s".into(), json!(secs(st.utime)));
    o.insert("system_s".into(), json!(secs(st.stime)));
    o.insert("cpu_s".into(), json!(secs(st.utime + st.stime)));
    o.insert("children_cpu_s".into(), json!(secs(st.cutime + st.cstime)));
    o.insert("nice".into(), json!(st.nice));
    o.insert("priority".into(), json!(st.priority));
    o.insert("policy".into(), json!(policy_name(st.policy)));
    o.insert("rt_priority".into(), json!(st.rt_priority));
    o.insert("processor".into(), json!(st.processor));
    o.insert("cpus_allowed".into(), json!(field("Cpus_allowed_list")));
    o.insert("threads".into(), json!(st.threads));
    let ctx = |k: &str| field(k).and_then(|v| v.parse::<u64>().ok());
    o.insert(
        "ctx_voluntary".into(),
        json!(ctx("voluntary_ctxt_switches")),
    );
    o.insert(
        "ctx_involuntary".into(),
        json!(ctx("nonvoluntary_ctxt_switches")),
    );
    o.insert(
        "seccomp".into(),
        json!(field("Seccomp").and_then(|v| v.parse::<u8>().ok())),
    );
    o.insert(
        "no_new_privs".into(),
        json!(field("NoNewPrivs").map(|v| v == "1")),
    );

    // Memory (status values are in kB).
    let kb: HashMap<&str, u64> = key_number_lines(&status)
        .filter(|(k, _)| k.starts_with("Vm") || k.starts_with("Rss"))
        .map(|(k, v)| (k, v * 1024))
        .collect();
    let m = |k: &str| kb.get(k).copied();
    o.insert(
        "mem".into(),
        json!({
            "rss": m("VmRSS").unwrap_or(st.rss_pages * page),
            "anon": m("RssAnon"),
            "file": m("RssFile"),
            "shmem": m("RssShmem"),
            "virt": m("VmSize").unwrap_or(st.vsize),
            "peak_virt": m("VmPeak"),
            "peak_rss": m("VmHWM"),
            "data": m("VmData"),
            "stack": m("VmStk"),
            "exe": m("VmExe"),
            "lib": m("VmLib"),
            "pte": m("VmPTE"),
            "swap": m("VmSwap"),
            "locked": m("VmLck"),
        }),
    );

    // Owner-only files.
    let link = |name: &str, restricted: &mut Vec<&str>, key: &'static str| match std::fs::read_link(
        dir.join(name),
    ) {
        Ok(p) => json!(p.to_string_lossy()),
        Err(e) => {
            if is_denied(&e) {
                restricted.push(key);
            }
            Value::Null
        }
    };
    if !kernel_thread {
        let exe = link("exe", &mut restricted, "exe");
        o.insert("exe".into(), exe);
        let cwd = link("cwd", &mut restricted, "cwd");
        o.insert("cwd".into(), cwd);
        match read_text(&dir.join("io")) {
            Ok(s) => {
                let io: Map<String, Value> = key_number_lines(&s)
                    .map(|(k, v)| (k.to_string(), json!(v)))
                    .collect();
                o.insert("io".into(), Value::Object(io));
            }
            Err(e) => {
                if is_denied(&e) {
                    restricted.push("io");
                }
                o.insert("io".into(), Value::Null);
            }
        }
        match std::fs::read_dir(dir.join("fd")) {
            Ok(rd) => {
                o.insert("fds".into(), json!(rd.take(FDS_COUNT_MAX).count()));
            }
            Err(e) => {
                if is_denied(&e) {
                    restricted.push("fds");
                }
                o.insert("fds".into(), Value::Null);
            }
        }
        if let Some((soft, hard)) = read_text(&dir.join("limits"))
            .ok()
            .as_deref()
            .and_then(open_files_limit)
        {
            o.insert("fd_limit".into(), json!(soft));
            o.insert("fd_limit_hard".into(), json!(hard));
        }
    }
    let num = |name: &str| {
        read_text(&dir.join(name))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
    };
    o.insert("oom_score".into(), json!(num("oom_score")));
    o.insert("oom_score_adj".into(), json!(num("oom_score_adj")));
    if let Ok(cg) = read_text(&dir.join("cgroup")) {
        let (path, unit) = cgroup_unit(&cg);
        o.insert("cgroup".into(), json!(path));
        o.insert("unit".into(), json!(unit));
    }

    // Threads: name, state and CPU time, busiest first.
    let tasks = list_dir(&dir.join("task"));
    let total = tasks.len();
    let mut threads: Vec<(u64, Value)> = tasks
        .iter()
        .take(THREADS_READ_MAX)
        .filter_map(|(name, tdir)| {
            let tid = name.parse::<u32>().ok()?;
            let t = parse_stat_full(&read_text(&tdir.join("stat")).ok()?)?;
            let ticks = t.utime + t.stime;
            Some((
                ticks,
                json!({
                    "tid": tid,
                    "name": t.comm,
                    "state": t.state.to_string(),
                    "cpu_s": secs(ticks),
                    "processor": t.processor,
                }),
            ))
        })
        .collect();
    threads.sort_by_key(|t| std::cmp::Reverse(t.0));
    threads.truncate(THREADS_SHOWN);
    o.insert("threads_total".into(), json!(total));
    o.insert(
        "thread_list".into(),
        Value::Array(threads.into_iter().map(|t| t.1).collect()),
    );

    restricted.dedup();
    o.insert("restricted".into(), json!(restricted));
    Some(Value::Object(o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tree() -> (PathBuf, Roots) {
        let base = std::env::temp_dir().join(format!(
            "hwstats-procinfo-{}-{}",
            std::process::id(),
            crate::util::time::now_ms()
        ));
        let p = base.join("proc");
        let w = |rel: &str, data: &[u8]| {
            let f = p.join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, data).unwrap();
        };
        w("stat", b"cpu  1 2 3 4\nbtime 1700000000\n");
        w("uptime", b"5000.00 1000.00\n");
        let stat = "4242 (chrome) S 4000 4000 4000 0 -1 4194560 10 0 0 0 250 50 3 1 20 0 3 0 100000 1048576 2500 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 7 0 0 0 0 0";
        w("4242/stat", stat.as_bytes());
        w(
            "4242/cmdline",
            b"/opt/google/chrome/chrome --type=renderer --lang=en-US --renderer-client-id=7",
        );
        w(
            "4242/status",
            b"Name:\tchrome\nState:\tS (sleeping)\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nVmPeak:\t  20000 kB\nVmSize:\t  18000 kB\nVmHWM:\t  12000 kB\nVmRSS:\t  10000 kB\nRssAnon:\t   6000 kB\nRssFile:\t   3000 kB\nRssShmem:\t   1000 kB\nVmSwap:\t     0 kB\nThreads:\t3\nCpus_allowed_list:\t0-31\nvoluntary_ctxt_switches:\t100\nnonvoluntary_ctxt_switches:\t5\nSeccomp:\t2\nNoNewPrivs:\t1\n",
        );
        w(
            "4242/cgroup",
            b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-google\\x2dchrome-1234.scope\n",
        );
        w(
            "4242/limits",
            b"Limit                     Soft Limit           Hard Limit           Units     \nMax open files            1024                 524288               files     \n",
        );
        w("4242/oom_score", b"300\n");
        w("4242/oom_score_adj", b"300\n");
        w(
            "4242/io",
            b"rchar: 10\nwchar: 20\nread_bytes: 4096\nwrite_bytes: 8192\n",
        );
        w("4242/fd/0", b"");
        w("4242/fd/1", b"");
        w("4242/task/4242/stat", stat.as_bytes());
        // Fields 23-37 (vsize .. cnswap) are zero; processor (39) is 3.
        let thread = format!(
            "4243 (Chrome_IOThread) S 4000 4000 4000 0 -1 0 0 0 0 0 900 100 0 0 20 0 3 0 100001 {} 17 3 0 0",
            ["0"; 15].join(" ")
        );
        w("4242/task/4243/stat", thread.as_bytes());
        let roots = Roots {
            proc: p.clone(),
            sys: base.join("sys"),
            state: base.clone(),
        };
        (base, roots)
    }

    #[test]
    fn inspect_reads_fixture() {
        let (base, roots) = tree();
        let users: HashMap<u32, String> = [(1000, "alice".to_string())].into();
        let v = inspect(&roots, 4242, &users).unwrap();
        assert_eq!(v["app"], "chrome");
        assert_eq!(v["kind"], "renderer (tabs, frames)");
        assert_eq!(v["user"], "alice");
        assert_eq!(v["ppid"], 4000);
        assert_eq!(v["args"][0], "/opt/google/chrome/chrome");
        assert_eq!(v["args"][1], "--type=renderer");
        assert_eq!(v["args"].as_array().unwrap().len(), 4);
        assert_eq!(v["mem"]["rss"], 10000 * 1024);
        assert_eq!(v["mem"]["anon"], 6000 * 1024);
        assert_eq!(v["unit"], "app-gnome-google-chrome-1234.scope");
        assert_eq!(v["fd_limit"], 1024);
        assert_eq!(v["fds"], 2);
        assert_eq!(v["io"]["read_bytes"], 4096);
        assert_eq!(v["oom_score"], 300);
        assert_eq!(v["seccomp"], 2);
        assert_eq!(v["threads_total"], 2);
        // Busiest thread first.
        assert_eq!(v["thread_list"][0]["name"], "Chrome_IOThread");
        assert_eq!(v["thread_list"][0]["processor"], 3);
        assert_eq!(v["processor"], 7);
        assert_eq!(v["policy"], "normal");
        let tck = clock_ticks() as f64;
        let start = 1_700_000_000.0 + 100_000.0 / tck;
        assert_eq!(v["started_ms"], (start * 1000.0).round() as u64);
        // exe/cwd links are missing (not denied), so nothing is restricted.
        assert_eq!(v["restricted"].as_array().unwrap().len(), 0);
        assert!(inspect(&roots, 999, &users).is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn helpers() {
        assert_eq!(
            cgroup_unit("12:name=systemd:/system.slice/sshd.service\n")
                .1
                .as_deref(),
            Some("sshd.service")
        );
        assert_eq!(cgroup_unit("0::/\n"), (Some("/".into()), None));
        assert_eq!(cgroup_unit(""), (None, None));
        assert_eq!(
            open_files_limit(
                "Max open files            unlimited            unlimited            files\n"
            ),
            Some((None, None))
        );
        let (a, t) = split_args(b"ls\0-la\0/tmp\0");
        assert_eq!(a, vec!["ls", "-la", "/tmp"]);
        assert!(!t);
        let (a, _) = split_args(b"/usr/lib/slack/slack --type=gpu-process --x=a b");
        assert_eq!(
            a,
            vec!["/usr/lib/slack/slack", "--type=gpu-process", "--x=a b"]
        );
        assert_eq!(policy_name(5), "idle");
    }
}
