//! Pressure stall information (/proc/pressure/{cpu,memory,io,irq}).

use super::{Collector, Ctx};
use crate::model::{DescCache, Health, Out, SeriesDesc};
use crate::util::sysfs::Roots;

pub struct Pressure {
    roots: Roots,
    descs: DescCache,
}

impl Pressure {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            descs: DescCache::default(),
        }
    }
}

/// Returns (kind, avg10) pairs, e.g. ("some", 1.5).
pub fn parse_psi(s: &str) -> Vec<(&str, f64)> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let kind = it.next()?;
            let avg10 = it
                .find_map(|kv| kv.strip_prefix("avg10="))?
                .parse::<f64>()
                .ok()?;
            Some((kind, avg10))
        })
        .collect()
}

impl Collector for Pressure {
    fn collect(&mut self, _ctx: &Ctx, out: &mut Out) -> anyhow::Result<()> {
        let mut found = 0;
        let mut last_err: Option<std::io::Error> = None;
        for res in ["cpu", "memory", "io", "irq"] {
            let path = self.roots.proc(&format!("pressure/{res}"));
            let s = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    if res != "irq" {
                        last_err = Some(e);
                    }
                    continue;
                }
            };
            for (kind, v) in parse_psi(&s) {
                let key = format!("psi.{res}.{kind}");
                let d = self.descs.get(&key, || {
                    SeriesDesc::new(
                        "pressure",
                        key.clone(),
                        format!("{res}_{kind}"),
                        format!("{} {kind}", res.to_uppercase()),
                        "%",
                    )
                    .group(res, res.to_uppercase())
                });
                out.push(&d, v);
                found += 1;
            }
        }
        out.health = if found > 0 {
            Health::ok("/proc/pressure")
        } else {
            let reason = match last_err {
                Some(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => {
                    "PSI is disabled in this kernel".to_string()
                }
                Some(e) => format!("/proc/pressure not readable: {e}"),
                None => "no PSI data".to_string(),
            };
            Health::unavailable(reason)
                .hint("enable PSI with the kernel boot parameter psi=1 (needed on RHEL 8)")
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psi() {
        let s = "some avg10=2.47 avg60=3.22 avg300=3.47 total=250332071\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_psi(s), vec![("some", 2.47), ("full", 0.0)]);
        assert!(parse_psi("junk").is_empty());
    }

    #[test]
    fn missing_psi_is_unavailable() {
        let roots = Roots::fixture("rhel8");
        let mut p = Pressure::new(roots);
        let settings = crate::settings::Settings::default();
        let ctx = Ctx {
            now: std::time::Instant::now(),
            wall_ms: 0,
            interval: std::time::Duration::from_secs(1),
            settings: &settings,
        };
        let mut out = Out::new();
        p.collect(&ctx, &mut out).unwrap();
        assert_eq!(out.health.state, crate::model::State::Unavailable);
        assert!(out.health.hint.is_some());
    }
}
