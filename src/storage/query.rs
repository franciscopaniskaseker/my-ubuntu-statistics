//! History queries. The coarsest tier that covers the range is queried
//! first, then each finer tier fills the time after the last bucket of the
//! previous one (rollups lag behind), and finally the in-memory ring covers
//! data not yet written. Gaps (suspend, service down) become `null`.

use crate::hub::round_value;
use crate::settings::History;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Default, Serialize, Clone, PartialEq)]
pub struct Hist {
    pub t: Vec<i64>,
    pub avg: Vec<Option<f64>>,
    pub min: Vec<Option<f64>>,
    pub max: Vec<Option<f64>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tier {
    Hour,
    Minute,
    Raw,
}

impl Tier {
    fn step(self, resolution: i64) -> i64 {
        match self {
            Tier::Hour => 3600,
            Tier::Minute => 60,
            Tier::Raw => resolution.max(1),
        }
    }
}

/// Pick the starting tier and bucket width for a range.
pub fn plan(h: &History, from: i64, to: i64, points: usize, now: i64) -> (Tier, i64) {
    let span = (to - from).max(1);
    let raw_start = now - h.raw_retention_hours as i64 * 3600;
    let min_start = now - h.minute_retention_days as i64 * 86_400;
    let tier = if span <= 6 * 3600 && from >= raw_start - 60 {
        Tier::Raw
    } else if span <= 7 * 86_400 && from >= min_start - 3600 {
        Tier::Minute
    } else {
        Tier::Hour
    };
    let step = tier.step(h.resolution_s as i64);
    let want = (span as f64 / points.max(1) as f64).ceil() as i64;
    let bucket = (want.max(step) + step - 1) / step * step;
    (tier, bucket)
}

pub fn open_read(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

type Row = (i64, f64, f64, f64);
/// Supplies in-memory `(ms, value)` points for a key newer than a ms timestamp.
pub type RingFn<'a> = &'a dyn Fn(&str, i64) -> Vec<(i64, f64)>;

fn query_tier(
    conn: &Connection,
    tier: Tier,
    id: i64,
    from: i64,
    to: i64,
    b: i64,
) -> Result<Vec<Row>> {
    let sql = match tier {
        Tier::Raw => {
            "SELECT (ts/?4)*?4 AS bt, avg(value), min(value), max(value) FROM samples
             WHERE series_id=?1 AND ts>=?2 AND ts<=?3 GROUP BY bt ORDER BY bt"
        }
        Tier::Minute => {
            "SELECT (ts/?4)*?4 AS bt, avg(avg), min(min), max(max) FROM samples_1m
             WHERE series_id=?1 AND ts>=?2 AND ts<=?3 GROUP BY bt ORDER BY bt"
        }
        Tier::Hour => {
            "SELECT (ts/?4)*?4 AS bt, avg(avg), min(min), max(max) FROM samples_1h
             WHERE series_id=?1 AND ts>=?2 AND ts<=?3 GROUP BY bt ORDER BY bt"
        }
    };
    let mut st = conn.prepare_cached(sql)?;
    let rows = st
        .query_map(params![id, from, to, b], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<Row>, _>>()?;
    Ok(rows)
}

/// Bucket raw `(ms, value)` points with width `b` seconds.
pub fn bucket_points(points: &[(i64, f64)], b: i64) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    let mut cur: Option<(i64, f64, f64, f64, u32)> = None;
    for (ms, v) in points {
        let bt = (ms / 1000 / b) * b;
        match &mut cur {
            Some((t, sum, mn, mx, n)) if *t == bt => {
                *sum += v;
                *mn = mn.min(*v);
                *mx = mx.max(*v);
                *n += 1;
            }
            _ => {
                if let Some((t, sum, mn, mx, n)) = cur.take() {
                    out.push((t, sum / n as f64, mn, mx));
                }
                cur = Some((bt, *v, *v, *v, 1));
            }
        }
    }
    if let Some((t, sum, mn, mx, n)) = cur {
        out.push((t, sum / n as f64, mn, mx));
    }
    out
}

/// Build the response arrays, inserting `null` where the gap between two
/// buckets exceeds `gap` seconds.
pub fn to_hist(rows: &[Row], gap: i64) -> Hist {
    let mut h = Hist::default();
    let mut prev: Option<i64> = None;
    for (t, a, mn, mx) in rows {
        if let Some(p) = prev
            && t - p > gap
        {
            h.t.push(p + 1);
            h.avg.push(None);
            h.min.push(None);
            h.max.push(None);
        }
        h.t.push(*t);
        h.avg.push(Some(round_value(*a)));
        h.min.push(Some(round_value(*mn)));
        h.max.push(Some(round_value(*mx)));
        prev = Some(*t);
    }
    h
}

pub struct Request<'a> {
    pub keys: &'a [String],
    pub from: i64,
    pub to: i64,
    pub points: usize,
    pub now: i64,
    pub history: &'a History,
}

/// Query the database tiers. `ring` supplies in-memory points (ms, value)
/// newer than a given ms timestamp; `gap_for` returns the natural sampling
/// interval (seconds) of a key.
pub fn history(
    db: Option<&Path>,
    req: &Request,
    ring: RingFn,
    gap_for: &dyn Fn(&str) -> i64,
) -> Result<(i64, HashMap<String, Hist>)> {
    let (tier, b) = plan(req.history, req.from, req.to, req.points, req.now);
    let conn = match db {
        Some(p) if p.exists() => Some(open_read(p)?),
        _ => None,
    };
    let mut ids: HashMap<String, i64> = HashMap::new();
    if let Some(c) = &conn {
        let mut st = c.prepare_cached("SELECT id FROM series WHERE key=?1")?;
        for k in req.keys {
            if let Ok(id) = st.query_row([k], |r| r.get::<_, i64>(0)) {
                ids.insert(k.clone(), id);
            }
        }
    }
    let tiers: &[Tier] = match tier {
        Tier::Hour => &[Tier::Hour, Tier::Minute, Tier::Raw],
        Tier::Minute => &[Tier::Minute, Tier::Raw],
        Tier::Raw => &[Tier::Raw],
    };
    let mut out = HashMap::new();
    for k in req.keys {
        let mut rows: Vec<Row> = Vec::new();
        let mut cursor = req.from;
        if let (Some(c), Some(id)) = (&conn, ids.get(k)) {
            for t in tiers {
                let part = query_tier(c, *t, *id, cursor, req.to, b)?;
                if let Some(last) = part.last() {
                    cursor = last.0 + b;
                }
                rows.extend(part);
            }
        }
        let fresh: Vec<(i64, f64)> = ring(k, cursor * 1000 - 1)
            .into_iter()
            .filter(|(ms, _)| *ms / 1000 <= req.to)
            .collect();
        rows.extend(
            bucket_points(&fresh, b)
                .into_iter()
                .filter(|r| r.0 >= cursor),
        );
        let gap = (b * 5 / 2)
            .max(gap_for(k) * 3)
            .max(req.history.resolution_s as i64 * 3);
        out.insert(k.clone(), to_hist(&rows, gap));
    }
    Ok((b, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_picks_tiers() {
        let h = History::default();
        let now = 1_000_000_000;
        assert_eq!(plan(&h, now - 3600, now, 600, now), (Tier::Raw, 10));
        assert_eq!(plan(&h, now - 86_400, now, 600, now).0, Tier::Minute);
        assert_eq!(plan(&h, now - 86_400, now, 600, now).1, 180);
        assert_eq!(plan(&h, now - 30 * 86_400, now, 600, now).0, Tier::Hour);
        // an old 1-hour window falls back to minute data
        assert_eq!(
            plan(&h, now - 5 * 86_400, now - 5 * 86_400 + 3600, 600, now).0,
            Tier::Minute
        );
    }

    #[test]
    fn gaps_become_null() {
        let rows = vec![
            (0, 1.0, 1.0, 1.0),
            (10, 2.0, 2.0, 2.0),
            (100, 3.0, 3.0, 3.0),
        ];
        let h = to_hist(&rows, 25);
        assert_eq!(h.t, vec![0, 10, 11, 100]);
        assert_eq!(h.avg, vec![Some(1.0), Some(2.0), None, Some(3.0)]);
    }

    #[test]
    fn bucketing() {
        let pts = vec![(1000, 1.0), (5000, 3.0), (11000, 10.0)];
        let r = bucket_points(&pts, 10);
        assert_eq!(r, vec![(0, 2.0, 1.0, 3.0), (10, 10.0, 10.0, 10.0)]);
    }
}
