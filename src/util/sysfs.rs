//! Tolerant readers for procfs/sysfs files.
//!
//! Every path goes through [`Roots`], so collectors can run against fixture
//! trees (`HWSTATS_PROC_ROOT`, `HWSTATS_SYS_ROOT`). Readers never panic; they
//! return `None`/`Err` and let the caller decide how to degrade.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Roots {
    pub proc: PathBuf,
    pub sys: PathBuf,
    /// Directory used for small runtime markers (e.g. the NVML crash guard).
    pub state: PathBuf,
}

impl Roots {
    pub fn from_env(state: PathBuf) -> Self {
        let proc = std::env::var_os("HWSTATS_PROC_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/proc"));
        let sys = std::env::var_os("HWSTATS_SYS_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys"));
        Self { proc, sys, state }
    }

    #[cfg(test)]
    pub fn fixture(name: &str) -> Self {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        Self {
            proc: base.join("proc"),
            sys: base.join("sys"),
            state: std::env::temp_dir(),
        }
    }

    pub fn proc(&self, rel: &str) -> PathBuf {
        self.proc.join(rel)
    }

    pub fn sys(&self, rel: &str) -> PathBuf {
        self.sys.join(rel)
    }

    /// True when running against the real system (not a fixture tree).
    pub fn is_live(&self) -> bool {
        self.proc == Path::new("/proc") && self.sys == Path::new("/sys")
    }
}

/// Read a whole file as a trimmed string.
pub fn read_str(path: &Path) -> io::Result<String> {
    let s = std::fs::read_to_string(path)?;
    Ok(s.trim().to_string())
}

/// Read a trimmed, non-empty string, or `None` on any error.
pub fn read_opt(path: &Path) -> Option<String> {
    match read_str(path) {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

pub fn read_u64(path: &Path) -> io::Result<u64> {
    let s = read_str(path)?;
    s.parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn read_i64(path: &Path) -> io::Result<i64> {
    let s = read_str(path)?;
    s.parse::<i64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn read_opt_u64(path: &Path) -> Option<u64> {
    read_u64(path).ok()
}

pub fn read_opt_i64(path: &Path) -> Option<i64> {
    read_i64(path).ok()
}

pub fn read_opt_f64(path: &Path) -> Option<f64> {
    read_opt(path)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Sorted directory listing as `(file name, full path)`. Empty on error.
pub fn list_dir(path: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = match std::fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                Some((name, e.path()))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    out
}

/// Resolve a symlink (e.g. `hwmonN/device`) to its canonical path.
pub fn resolve(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Name of the directory a symlink points to (e.g. a driver name).
pub fn link_name(path: &Path) -> Option<String> {
    let target = std::fs::read_link(path).ok()?;
    target.file_name()?.to_str().map(|s| s.to_string())
}

/// Natural ordering so that `cpu2` sorts before `cpu10`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let va = na.parse::<u128>().unwrap_or(0);
                let vb = nb.parse::<u128>().unwrap_or(0);
                match va.cmp(&vb) {
                    std::cmp::Ordering::Equal => continue,
                    o => return o,
                }
            }
            (Some(ca), Some(cb)) => {
                let o = ca.cmp(cb);
                ai.next();
                bi.next();
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// Is this a PCI device (or child of one) that is runtime-suspended?
/// Reading sensors of a suspended GPU would wake it up, so callers skip it.
pub fn runtime_suspended(device_dir: &Path) -> bool {
    matches!(
        read_opt(&device_dir.join("power/runtime_status")).as_deref(),
        Some("suspended") | Some("suspending")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_sort() {
        let mut v = vec!["cpu10", "cpu2", "cpu1", "cpu"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["cpu", "cpu1", "cpu2", "cpu10"]);
    }
}
