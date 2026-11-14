//! Small, tolerant parsing helpers shared by collectors.

/// Turn arbitrary text into a key-safe slug: lowercase ASCII letters, digits,
/// `-`, `.` and `_`; everything else collapses into `_`.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_us = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            out.push(c);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let t = out.trim_matches('_');
    if t.is_empty() {
        "x".to_string()
    } else {
        t.to_string()
    }
}

/// Slug that also replaces `:` (PCI addresses) with `-`.
pub fn dev_slug(s: &str) -> String {
    slug(&s.replace(':', "-"))
}

/// Decode the octal escapes used in /proc/self/mountinfo (`\040` = space).
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (oct[0] - b'0') as u32 * 64
                    + (oct[1] - b'0') as u32 * 8
                    + (oct[2] - b'0') as u32;
                if v <= 255 {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal glob: supports `*` anywhere, everything else literal.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// Parse `Key: value [unit]` style files (meminfo) into `(key, number)`.
pub fn key_number_lines(s: &str) -> impl Iterator<Item = (&str, u64)> {
    s.lines().filter_map(|l| {
        let (k, v) = l.split_once(':')?;
        let n = v.split_whitespace().next()?.parse::<u64>().ok()?;
        Some((k.trim(), n))
    })
}

/// Parse `key value` style files (vmstat) into `(key, number)`.
pub fn space_number_lines(s: &str) -> impl Iterator<Item = (&str, u64)> {
    s.lines().filter_map(|l| {
        let mut it = l.split_whitespace();
        let k = it.next()?;
        let n = it.next()?.parse::<u64>().ok()?;
        Some((k, n))
    })
}

/// Page size of the running system.
pub fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 4096 }
}

/// Clock ticks per second used by /proc/[pid]/stat.
pub fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Samsung SSD 990 PRO 2TB"), "samsung_ssd_990_pro_2tb");
        assert_eq!(slug("/boot/efi"), "boot_efi");
        assert_eq!(slug("/"), "x");
        assert_eq!(dev_slug("0000:02:00.0"), "0000-02-00.0");
    }

    #[test]
    fn octal() {
        assert_eq!(unescape_octal("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_octal("/plain"), "/plain");
        assert_eq!(unescape_octal("/end\\04"), "/end\\04");
    }

    #[test]
    fn globs() {
        assert!(glob_match("veth*", "veth12ab"));
        assert!(glob_match("lo", "lo"));
        assert!(!glob_match("lo", "lo0"));
        assert!(glob_match("*docker*", "br-docker0"));
        assert!(glob_match("br-*", "br-1234"));
        assert!(!glob_match("br-*", "wlo1"));
        assert!(glob_match("a*c", "abc"));
        assert!(!glob_match("a*c", "abd"));
    }
}
