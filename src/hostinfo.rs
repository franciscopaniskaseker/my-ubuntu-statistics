//! Static host facts shown in the page header (read once at startup).

use crate::util::parse::key_number_lines;
use crate::util::sysfs::{Roots, read_opt};
use serde_json::{Value, json};

fn os_pretty_name() -> Option<String> {
    let s = std::fs::read_to_string("/etc/os-release").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
}

fn cpu_model(roots: &Roots) -> Option<String> {
    let s = std::fs::read_to_string(roots.proc("cpuinfo")).ok()?;
    s.lines()
        .find(|l| {
            l.starts_with("model name") || l.starts_with("Model") || l.starts_with("Hardware")
        })
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
}

pub fn collect(roots: &Roots) -> Value {
    let dmi = |f: &str| {
        read_opt(&roots.sys(&format!("class/dmi/id/{f}")))
            .filter(|v| !v.is_empty() && v != "To be filled by O.E.M." && v != "Default string")
    };
    let mem_total = std::fs::read_to_string(roots.proc("meminfo"))
        .ok()
        .and_then(|s| {
            key_number_lines(&s)
                .find(|(k, _)| *k == "MemTotal")
                .map(|(_, v)| v * 1024)
        });
    let threads = std::fs::read_to_string(roots.proc("stat"))
        .map(|s| {
            s.lines()
                .filter(|l| {
                    l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(|c| c.is_ascii_digit())
                })
                .count()
        })
        .unwrap_or(0);
    json!({
        "hostname": read_opt(&roots.proc("sys/kernel/hostname")),
        "os": os_pretty_name(),
        "kernel": read_opt(&roots.proc("sys/kernel/osrelease")),
        "arch": std::env::consts::ARCH,
        "vendor": dmi("sys_vendor"),
        "product": dmi("product_name"),
        "board": dmi("board_name"),
        "bios": dmi("bios_version"),
        "cpu": cpu_model(roots),
        "threads": threads,
        "mem_total": mem_total,
    })
}
