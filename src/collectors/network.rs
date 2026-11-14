//! Network interfaces (/proc/net/dev), Wi-Fi signal (/proc/net/wireless)
//! and socket counts (/proc/net/sockstat).

use super::{Collector, Ctx};
use crate::model::{Desc, DescCache, Health, Out, SeriesDesc};
use crate::util::counter::Rates;
use crate::util::parse::glob_match;
use crate::util::sysfs::{Roots, read_opt, read_opt_i64};
use anyhow::Context;
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IfCounters {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errs: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errs: u64,
    pub tx_drop: u64,
}

pub fn parse_net_dev(s: &str) -> Vec<(String, IfCounters)> {
    s.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let f: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            if f.len() < 12 {
                return None;
            }
            Some((
                name.trim().to_string(),
                IfCounters {
                    rx_bytes: f[0],
                    rx_packets: f[1],
                    rx_errs: f[2],
                    rx_drop: f[3],
                    tx_bytes: f[8],
                    tx_packets: f[9],
                    tx_errs: f[10],
                    tx_drop: f[11],
                },
            ))
        })
        .collect()
}

/// (interface, link quality, signal dBm) from /proc/net/wireless.
pub fn parse_wireless(s: &str) -> Vec<(String, f64, f64)> {
    s.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() < 3 {
                return None;
            }
            let num = |x: &str| x.trim_end_matches('.').parse::<f64>().ok();
            Some((name.trim().to_string(), num(f[1])?, num(f[2])?))
        })
        .collect()
}

/// Sum of `inuse` for a protocol line (e.g. "TCP:") in sockstat files.
fn sockstat_field(s: &str, proto: &str, field: &str) -> Option<u64> {
    let line = s.lines().find(|l| l.starts_with(proto))?;
    let mut it = line.split_whitespace().skip(1);
    while let Some(k) = it.next() {
        let v = it.next()?;
        if k == field {
            return v.parse().ok();
        }
    }
    None
}

pub struct Network {
    roots: Roots,
    descs: DescCache,
    rates: Rates,
}

impl Network {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
            rates: Rates::default(),
        }
    }

    fn desc(
        &mut self,
        iface: &str,
        glabel: &str,
        metric: &str,
        label: &str,
        unit: &'static str,
    ) -> Desc {
        let key = format!("net.{iface}.{metric}");
        self.descs.get(&key, || {
            SeriesDesc::new("network", key.clone(), metric, label, unit).group(iface, glabel)
        })
    }

    fn global(&mut self, key: &str, metric: &str, label: &str, unit: &'static str) -> Desc {
        self.descs
            .get(key, || SeriesDesc::new("network", key, metric, label, unit))
    }
}

impl Collector for Network {
    fn collect(&mut self, ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let p = self.roots.proc("net/dev");
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let excludes = &ctx.settings.network.exclude;
        let (now, iv) = (ctx.now, ctx.interval);
        let (mut tot_rx, mut tot_tx, mut have_total) = (0.0, 0.0, false);
        let mut count = 0;
        for (name, c) in parse_net_dev(&s) {
            if excludes.iter().any(|g| glob_match(g, &name)) {
                continue;
            }
            count += 1;
            let base = self.roots.sys(&format!("class/net/{name}"));
            let physical = base.join("device").exists();
            let kind = if base.join("wireless").exists() || base.join("phy80211").exists() {
                "Wi-Fi"
            } else if physical {
                "Ethernet"
            } else {
                "virtual"
            };
            let glabel = format!("{name} ({kind})");
            let rx = self.rates.rate(&format!("{name}.rxb"), c.rx_bytes, now, iv);
            let tx = self.rates.rate(&format!("{name}.txb"), c.tx_bytes, now, iv);
            let rxp = self
                .rates
                .rate(&format!("{name}.rxp"), c.rx_packets, now, iv);
            let txp = self
                .rates
                .rate(&format!("{name}.txp"), c.tx_packets, now, iv);
            let err = self
                .rates
                .rate(&format!("{name}.err"), c.rx_errs + c.tx_errs, now, iv);
            let drop = self
                .rates
                .rate(&format!("{name}.drop"), c.rx_drop + c.tx_drop, now, iv);
            for (metric, label, unit, v) in [
                ("rx_bps", "Download", "B/s", rx),
                ("tx_bps", "Upload", "B/s", tx),
                ("rx_pps", "Packets in", "/s", rxp),
                ("tx_pps", "Packets out", "/s", txp),
                ("errors", "Errors", "/s", err),
                ("drops", "Drops", "/s", drop),
            ] {
                if let Some(v) = v {
                    let d = self.desc(&name, &glabel, metric, label, unit);
                    out.push(&d, v);
                }
            }
            if physical && let (Some(r), Some(t)) = (rx, tx) {
                tot_rx += r;
                tot_tx += t;
                have_total = true;
            }
            let up = read_opt(&base.join("operstate")).unwrap_or_default();
            let d = self.desc(&name, &glabel, "up", "Link up", "bool");
            out.push(
                &d,
                if up == "up" || (up == "unknown" && !physical) {
                    1.0
                } else {
                    0.0
                },
            );
            if let Some(speed) = read_opt_i64(&base.join("speed")).filter(|v| *v > 0) {
                out.info(format!("{name}.speed_mbps"), speed.to_string());
            }
        }
        self.rates.retain_recent(now, Duration::from_secs(600));
        if have_total {
            let d = self.global("net.total.rx_bps", "total_rx_bps", "Total download", "B/s");
            out.push(&d, tot_rx);
            let d = self.global("net.total.tx_bps", "total_tx_bps", "Total upload", "B/s");
            out.push(&d, tot_tx);
        }

        if let Ok(w) = std::fs::read_to_string(self.roots.proc("net/wireless")) {
            for (name, quality, dbm) in parse_wireless(&w) {
                let glabel = format!("{name} (Wi-Fi)");
                let d = self.desc(&name, &glabel, "wifi_quality", "Link quality", "%");
                out.push(&d, (quality / 70.0 * 100.0).clamp(0.0, 100.0));
                if dbm < 0.0 {
                    let d = self.desc(&name, &glabel, "wifi_dbm", "Signal", "dBm");
                    out.push(&d, dbm);
                }
            }
        }

        let s4 = std::fs::read_to_string(self.roots.proc("net/sockstat")).unwrap_or_default();
        let s6 = std::fs::read_to_string(self.roots.proc("net/sockstat6")).unwrap_or_default();
        let tcp = sockstat_field(&s4, "TCP:", "inuse").unwrap_or(0)
            + sockstat_field(&s6, "TCP6:", "inuse").unwrap_or(0);
        let udp = sockstat_field(&s4, "UDP:", "inuse").unwrap_or(0)
            + sockstat_field(&s6, "UDP6:", "inuse").unwrap_or(0);
        if !s4.is_empty() {
            let d = self.global("net.tcp.inuse", "tcp_inuse", "TCP sockets", "count");
            out.push(&d, tcp as f64);
            let d = self.global("net.udp.inuse", "udp_inuse", "UDP sockets", "count");
            out.push(&d, udp as f64);
            if let Some(tw) = sockstat_field(&s4, "TCP:", "tw") {
                let d = self.global("net.tcp.tw", "tcp_tw", "TCP TIME_WAIT", "count");
                out.push(&d, tw as f64);
            }
        }

        out.health = if count > 0 {
            Health::ok("/proc/net/dev")
        } else {
            Health::unavailable("no network interfaces (after exclusions)")
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_dev() {
        let s = "Inter-|   Receive |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 100 2 0 0 0 0 0 0 100 2 0 0 0 0 0 0\n  wlo1: 5000 50 1 2 0 0 0 0 7000 60 3 4 0 0 0 0\n";
        let v = parse_net_dev(s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].0, "wlo1");
        assert_eq!(v[1].1.tx_bytes, 7000);
        assert_eq!(v[1].1.tx_drop, 4);
    }

    #[test]
    fn wireless() {
        let s = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n  wlo1: 0000   52.  -58.  -256        0      0      0      0   1666        0\n";
        assert_eq!(parse_wireless(s), vec![("wlo1".to_string(), 52.0, -58.0)]);
    }

    #[test]
    fn sockstat() {
        let s =
            "sockets: used 500\nTCP: inuse 25 orphan 0 tw 3 alloc 30 mem 2\nUDP: inuse 5 mem 1\n";
        assert_eq!(sockstat_field(s, "TCP:", "inuse"), Some(25));
        assert_eq!(sockstat_field(s, "TCP:", "tw"), Some(3));
        assert_eq!(sockstat_field(s, "UDP:", "inuse"), Some(5));
        assert_eq!(sockstat_field(s, "RAW:", "inuse"), None);
    }
}
