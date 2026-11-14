//! Optional lookup of PCI vendor/device names from the system `pci.ids`
//! database. Used only for friendly labels; missing database is fine.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

const CANDIDATES: &[&str] = &[
    "/usr/share/misc/pci.ids",
    "/usr/share/hwdata/pci.ids",
    "/usr/share/pci.ids",
];

type Cache = Mutex<HashMap<(u16, u16), Option<String>>>;

static CACHE: LazyLock<Cache> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns "Vendor Device" for the given ids, if known.
pub fn lookup(vendor: u16, device: u16) -> Option<String> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = cache.get(&(vendor, device)) {
        return v.clone();
    }
    let v = CANDIDATES.iter().find_map(|p| {
        let s = std::fs::read_to_string(p).ok()?;
        find(&s, vendor, device)
    });
    cache.insert((vendor, device), v.clone());
    v
}

/// Forget cached lookups (a hardware rescan reads the database again).
pub fn clear_cache() {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

fn find(db: &str, vendor: u16, device: u16) -> Option<String> {
    let vhex = format!("{vendor:04x}");
    let dhex = format!("{device:04x}");
    let mut vendor_name: Option<&str> = None;
    for line in db.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if vendor_name.is_some() {
                break;
            }
            if line.len() > 6 && line[..4].eq_ignore_ascii_case(&vhex) {
                vendor_name = Some(line[4..].trim());
            }
            continue;
        }
        if let Some(vn) = vendor_name {
            if line.starts_with("\t\t") {
                continue;
            }
            let l = &line[1..];
            if l.len() > 6 && l[..4].eq_ignore_ascii_case(&dhex) {
                return Some(format!("{} {}", short_vendor(vn), l[4..].trim()));
            }
        }
    }
    vendor_name.map(|v| short_vendor(v).to_string())
}

fn short_vendor(v: &str) -> &str {
    if v.starts_with("NVIDIA") {
        "NVIDIA"
    } else if v.starts_with("Advanced Micro Devices") {
        "AMD"
    } else if v.starts_with("Intel") {
        "Intel"
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_device() {
        let db = "# comment\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t1234  Example iGPU\n\t\t1043 8877  Sub\n10de  NVIDIA Corporation\n\t2d18  Some GPU\n";
        assert_eq!(
            find(db, 0x1002, 0x1234).as_deref(),
            Some("AMD Example iGPU")
        );
        assert_eq!(find(db, 0x10de, 0x2d18).as_deref(), Some("NVIDIA Some GPU"));
        assert_eq!(find(db, 0x10de, 0x9999).as_deref(), Some("NVIDIA"));
        assert_eq!(find(db, 0x1234, 0x1).as_deref(), None);
    }
}
