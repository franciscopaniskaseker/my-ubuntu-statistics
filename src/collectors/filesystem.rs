//! Filesystem usage for local filesystems (mountinfo + statvfs).
//!
//! Only an allow-list of local block filesystems is queried. Network, FUSE
//! and automount filesystems are skipped so a dead server or daemon can
//! never block the collector.

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::parse::{slug, unescape_octal};
use crate::util::sysfs::Roots;
use anyhow::Context;
use std::collections::HashMap;
use std::ffi::CString;

pub const LOCAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "vfat", "msdos", "exfat", "ntfs", "ntfs3", "f2fs",
    "zfs", "jfs", "reiserfs", "bcachefs", "hfsplus", "nilfs2",
];

const SKIP_UNDER: &[&str] = &[
    "/snap/",
    "/var/lib/docker/",
    "/var/lib/containers/",
    "/run/",
    "/proc/",
    "/sys/",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub dev: String,
    pub mount_point: String,
    pub fstype: String,
    pub source: String,
}

pub fn parse_mountinfo(s: &str) -> Vec<Mount> {
    s.lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let lf: Vec<&str> = left.split_whitespace().collect();
            let rf: Vec<&str> = right.split_whitespace().collect();
            if lf.len() < 5 || rf.len() < 2 {
                return None;
            }
            Some(Mount {
                dev: lf[2].to_string(),
                mount_point: unescape_octal(lf[4]),
                fstype: rf[0].to_string(),
                source: unescape_octal(rf[1]),
            })
        })
        .collect()
}

/// Local filesystems, one entry per device (shortest mount point wins).
pub fn select_mounts(all: Vec<Mount>) -> Vec<Mount> {
    let mut by_dev: HashMap<String, Mount> = HashMap::new();
    for m in all {
        if !LOCAL_FS.contains(&m.fstype.as_str()) {
            continue;
        }
        let mp = format!("{}/", m.mount_point.trim_end_matches('/'));
        if SKIP_UNDER.iter().any(|p| mp.starts_with(p)) {
            continue;
        }
        match by_dev.get(&m.dev) {
            Some(e) if e.mount_point.len() <= m.mount_point.len() => {}
            _ => {
                by_dev.insert(m.dev.clone(), m);
            }
        }
    }
    let mut v: Vec<Mount> = by_dev.into_values().collect();
    v.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    v
}

pub struct FsUsage {
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub inodes_total: u64,
    pub inodes_free: u64,
}

pub fn statvfs(path: &str) -> std::io::Result<FsUsage> {
    let c =
        CString::new(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: zeroed statvfs is a valid out-parameter; c is NUL terminated.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let frsize = if st.f_frsize > 0 {
        st.f_frsize
    } else {
        st.f_bsize
    } as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bfree as u64 * frsize;
    let avail = st.f_bavail as u64 * frsize;
    Ok(FsUsage {
        total,
        used: total.saturating_sub(free),
        avail,
        inodes_total: st.f_files as u64,
        inodes_free: st.f_ffree as u64,
    })
}

pub struct Filesystem {
    roots: Roots,
    descs: DescCache,
}

impl Filesystem {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
        }
    }
}

impl Collector for Filesystem {
    fn collect(&mut self, _ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let p = self.roots.proc("self/mountinfo");
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let mounts = select_mounts(parse_mountinfo(&s));
        let mut ok = 0;
        let mut failed = Vec::new();
        for m in &mounts {
            // Fixture trees describe mounts that do not exist locally.
            if !self.roots.is_live() {
                continue;
            }
            let u = match statvfs(&m.mount_point) {
                Ok(u) => u,
                Err(e) => {
                    failed.push(format!("{}: {e}", m.mount_point));
                    continue;
                }
            };
            if u.total == 0 {
                continue;
            }
            ok += 1;
            let id = if m.mount_point == "/" {
                "root".to_string()
            } else {
                slug(&m.mount_point)
            };
            let glabel = format!("{} ({}, {})", m.mount_point, m.source, m.fstype);
            let mut push = |metric: &str, label: &str, unit: &'static str, v: f64| {
                let key = format!("fs.{id}.{metric}");
                let d = self.descs.get(&key, || {
                    SeriesDesc::new("filesystem", key.clone(), metric, label, unit)
                        .group(id.clone(), glabel.clone())
                });
                out.push(&d, v);
            };
            push("total", "Size", "B", u.total as f64);
            push("used", "Used", "B", u.used as f64);
            push("avail", "Available", "B", u.avail as f64);
            let denom = u.used + u.avail;
            if denom > 0 {
                push(
                    "used_pct",
                    "Used",
                    "%",
                    u.used as f64 / denom as f64 * 100.0,
                );
            }
            if u.inodes_total > 0 {
                push(
                    "inodes_pct",
                    "Inodes used",
                    "%",
                    (u.inodes_total - u.inodes_free.min(u.inodes_total)) as f64
                        / u.inodes_total as f64
                        * 100.0,
                );
            }
        }
        out.health = if ok > 0 && failed.is_empty() {
            Health::ok("/proc/self/mountinfo, statvfs")
        } else if ok > 0 {
            Health::degraded("/proc/self/mountinfo, statvfs", failed.join("; "))
        } else if !self.roots.is_live() {
            Health::ok("fixture")
        } else {
            Health::unavailable(if failed.is_empty() {
                "no local filesystems found".into()
            } else {
                failed.join("; ")
            })
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_selection() {
        let s = "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
                 23 22 259:2 /home /home rw shared:2 - ext4 /dev/nvme0n1p2 rw\n\
                 24 22 259:1 / /boot/efi rw - vfat /dev/nvme0n1p1 rw\n\
                 25 22 0:50 / /mnt/nas rw - nfs4 server:/x rw\n\
                 26 22 7:3 / /snap/core/1 ro - squashfs /dev/loop3 ro\n\
                 27 22 0:60 / /home/u/.gvfs rw - fuse.gvfsd-fuse gvfsd rw\n\
                 28 22 8:17 / /media/my\\040disk rw - exfat /dev/sdb1 rw\n";
        let m = select_mounts(parse_mountinfo(s));
        let mps: Vec<&str> = m.iter().map(|x| x.mount_point.as_str()).collect();
        assert_eq!(mps, vec!["/", "/boot/efi", "/media/my disk"]);
    }

    #[test]
    fn statvfs_root_works() {
        let u = statvfs("/").unwrap();
        assert!(u.total > 0);
    }
}
