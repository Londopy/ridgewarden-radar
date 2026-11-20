//! What the app keeps of each CPI: hourly counts for the statistics (kept a long time,
//! small), per-aircraft hourly counts (kept just long enough for the longest window), and
//! the last few minutes of detections and expected aircraft positions for the
//! range-Doppler plot.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::blah2::Frame;
use crate::fuse::Fix;
use crate::truth::Outcome;

pub const HOUR: i64 = 3_600_000;
/// how much of the raw picture the plot can show
pub const RECENT_MS: i64 = 15 * 60_000;
const KEEP_HOURS: i64 = 400 * 24;
/// these carry ADS-B addresses and callsigns, so only as long as the 30-day window needs
const KEEP_AIRCRAFT_HOURS: i64 = 31 * 24;
/// multi-site fixes are many and positional, so only a week
pub const KEEP_FIXES_MS: i64 = 7 * 24 * HOUR;
/// expected positions are stored once per aircraft this often, enough to draw a track
pub const TRUTH_EVERY_MS: i64 = 2_000;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS hour_frames (
    hour      INTEGER PRIMARY KEY,
    cpis      INTEGER NOT NULL,
    dets      INTEGER NOT NULL,
    unmatched INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hour_range (
    hour  INTEGER NOT NULL,
    km    INTEGER NOT NULL,
    opps  INTEGER NOT NULL,
    hits  INTEGER NOT NULL,
    PRIMARY KEY (hour, km)
);
CREATE TABLE IF NOT EXISTS hour_aircraft (
    hour    INTEGER NOT NULL,
    hex     TEXT NOT NULL,
    flight  TEXT NOT NULL,
    opps    INTEGER NOT NULL,
    hits    INTEGER NOT NULL,
    min_km  REAL NOT NULL,
    max_km  REAL NOT NULL,
    PRIMARY KEY (hour, hex)
);
CREATE TABLE IF NOT EXISTS recent_det (
    ts         INTEGER NOT NULL,
    range_km   REAL NOT NULL,
    doppler_hz REAL NOT NULL,
    snr_db     REAL NOT NULL,
    hex        TEXT
);
CREATE INDEX IF NOT EXISTS recent_det_ts ON recent_det(ts);
CREATE TABLE IF NOT EXISTS recent_truth (
    ts         INTEGER NOT NULL,
    hex        TEXT NOT NULL,
    flight     TEXT NOT NULL,
    range_km   REAL NOT NULL,
    doppler_hz REAL NOT NULL,
    counted    INTEGER NOT NULL,
    hit        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS recent_truth_ts ON recent_truth(ts);
CREATE TABLE IF NOT EXISTS setups (
    since       INTEGER NOT NULL,
    fingerprint TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fixes (
    ts          INTEGER NOT NULL,
    track       INTEGER NOT NULL,
    lat         REAL NOT NULL,
    lon         REAL NOT NULL,
    alt_m       REAL NOT NULL,
    alt_sigma_m REAL NOT NULL,
    ve          REAL NOT NULL,
    vn          REAL NOT NULL,
    major_km    REAL NOT NULL,
    minor_km    REAL NOT NULL,
    angle_deg   REAL NOT NULL,
    chi2        REAL NOT NULL,
    sites       TEXT NOT NULL,
    nsites      INTEGER NOT NULL,
    scored      INTEGER NOT NULL,
    hex         TEXT,
    flight      TEXT,
    err_km      REAL
);
CREATE INDEX IF NOT EXISTS fixes_ts ON fixes(ts);
"#;

/// What ADS-B said about a fix.
#[derive(Clone, Debug, PartialEq)]
pub enum Truth {
    /// no current ADS-B list at the time
    Unknown,
    /// no ADS-B aircraft near it: one without ADS-B, or a ghost
    Nothing,
    /// this aircraft, this far across the ground from the fix
    Aircraft {
        hex: String,
        flight: String,
        err_km: f64,
    },
}

/// A fix as stored: which fused track it belongs to, the sites that saw it, and the truth.
#[derive(Clone, Debug, PartialEq)]
pub struct FixRow {
    pub track: u64,
    pub fix: Fix,
    pub sites: String,
    pub truth: Truth,
}

pub fn record_fixes(conn: &mut Connection, rows: &[FixRow]) -> Result<()> {
    let tx = conn.transaction()?;
    for r in rows {
        let f = &r.fix;
        let (scored, hex, flight, err) = match &r.truth {
            Truth::Unknown => (false, None, None, None),
            Truth::Nothing => (true, None, None, None),
            Truth::Aircraft {
                hex,
                flight,
                err_km,
            } => (true, Some(hex), Some(flight), Some(err_km)),
        };
        tx.execute(
            "INSERT INTO fixes (ts, track, lat, lon, alt_m, alt_sigma_m, ve, vn, major_km, minor_km,
                 angle_deg, chi2, sites, nsites, scored, hex, flight, err_km)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                f.ts,
                r.track as i64,
                f.lat,
                f.lon,
                f.alt_m,
                f.alt_sigma_m,
                f.ve,
                f.vn,
                f.major_km,
                f.minor_km,
                f.angle_deg,
                f.chi2,
                r.sites,
                f.sites.len() as i64,
                scored,
                hex,
                flight,
                err
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The highest fused-track id used so far.
pub fn last_track(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT coalesce(max(track), 0) FROM fixes", [], |r| {
        r.get(0)
    })?;
    Ok(n as u64)
}

/// When the statistics for blah2's current setup start (unix ms). A new setup starts at
/// the next whole hour, so no hourly row mixes two setups; the first one ever covers
/// everything.
pub fn setup_since(conn: &Connection, fingerprint: &str, now: i64) -> Result<i64> {
    let last: Option<(i64, String)> = conn
        .query_row(
            "SELECT since, fingerprint FROM setups ORDER BY rowid DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let since = match last {
        Some((since, fp)) if fp == fingerprint => return Ok(since),
        Some(_) => (now.div_euclid(HOUR) + 1) * HOUR,
        None => 0,
    };
    conn.execute(
        "INSERT INTO setups (since, fingerprint) VALUES (?1, ?2)",
        params![since, fingerprint],
    )?;
    Ok(since)
}

/// Add one matched CPI. `with_truth` also stores the expected positions for the plot.
pub fn record(conn: &mut Connection, f: &Frame, o: &Outcome, with_truth: bool) -> Result<()> {
    let hour = f.ts.div_euclid(HOUR);
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO hour_frames (hour, cpis, dets, unmatched) VALUES (?1, 1, ?2, ?3)
         ON CONFLICT(hour) DO UPDATE SET cpis = cpis + 1, dets = dets + excluded.dets,
             unmatched = unmatched + excluded.unmatched",
        params![hour, f.dets.len() as i64, o.unmatched() as i64],
    )?;
    for a in o.aircraft.iter().filter(|a| a.counted) {
        let hit = a.hit.is_some() as i64;
        tx.execute(
            "INSERT INTO hour_range (hour, km, opps, hits) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(hour, km) DO UPDATE SET opps = opps + 1, hits = hits + excluded.hits",
            params![hour, a.range_km.floor() as i64, hit],
        )?;
        tx.execute(
            "INSERT INTO hour_aircraft (hour, hex, flight, opps, hits, min_km, max_km)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5)
             ON CONFLICT(hour, hex) DO UPDATE SET opps = opps + 1, hits = hits + excluded.hits,
                 flight = CASE WHEN excluded.flight <> '' THEN excluded.flight ELSE flight END,
                 min_km = min(min_km, excluded.min_km), max_km = max(max_km, excluded.max_km)",
            params![hour, a.hex, a.flight, hit, a.range_km],
        )?;
    }
    for (d, m) in f.dets.iter().zip(&o.matched) {
        tx.execute(
            "INSERT INTO recent_det (ts, range_km, doppler_hz, snr_db, hex) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                f.ts,
                d.range_km,
                d.doppler_hz,
                d.snr_db,
                m.map(|i| &o.aircraft[i].hex)
            ],
        )?;
    }
    if with_truth {
        for a in &o.aircraft {
            tx.execute(
                "INSERT INTO recent_truth (ts, hex, flight, range_km, doppler_hz, counted, hit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    f.ts,
                    a.hex,
                    a.flight,
                    a.range_km,
                    a.doppler_hz,
                    a.counted,
                    a.hit.is_some()
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    let recent = now - RECENT_MS;
    let hour = now.div_euclid(HOUR);
    let old = hour - KEEP_HOURS;
    conn.execute("DELETE FROM recent_det WHERE ts < ?1", [recent])?;
    conn.execute("DELETE FROM recent_truth WHERE ts < ?1", [recent])?;
    conn.execute("DELETE FROM hour_frames WHERE hour < ?1", [old])?;
    conn.execute("DELETE FROM hour_range WHERE hour < ?1", [old])?;
    conn.execute(
        "DELETE FROM hour_aircraft WHERE hour < ?1",
        [hour - KEEP_AIRCRAFT_HOURS],
    )?;
    conn.execute("DELETE FROM fixes WHERE ts < ?1", [now - KEEP_FIXES_MS])?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecentDet {
    pub ts: i64,
    pub range_km: f64,
    pub doppler_hz: f64,
    pub matched: bool,
}

pub fn recent_dets(conn: &Connection, from: i64, limit: usize) -> Result<Vec<RecentDet>> {
    let mut st = conn.prepare(
        "SELECT ts, range_km, doppler_hz, hex IS NOT NULL FROM recent_det
         WHERE ts >= ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![from, limit as i64], |r| {
        Ok(RecentDet {
            ts: r.get(0)?,
            range_km: r.get(1)?,
            doppler_hz: r.get(2)?,
            matched: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// An aircraft's expected positions over the last few minutes, oldest first. An aircraft
/// that drops out of the ADS-B feed for a while comes back as a new path.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub hex: String,
    pub flight: String,
    pub points: Vec<(f64, f64)>,
}

pub fn recent_paths(conn: &Connection, from: i64) -> Result<Vec<Path>> {
    let mut st = conn.prepare(
        "SELECT hex, flight, range_km, doppler_hz, ts FROM recent_truth
         WHERE ts >= ?1 ORDER BY hex, ts",
    )?;
    let mut rows = st.query([from])?;
    let mut out: Vec<Path> = vec![];
    let mut last_ts = 0;
    while let Some(r) = rows.next()? {
        let hex: String = r.get(0)?;
        let ts: i64 = r.get(4)?;
        if out.last().is_none_or(|p| p.hex != hex) || ts - last_ts > 5 * TRUTH_EVERY_MS {
            out.push(Path {
                hex,
                flight: String::new(),
                points: vec![],
            });
        }
        last_ts = ts;
        let p = out.last_mut().expect("pushed above");
        let flight: String = r.get(1)?;
        if !flight.is_empty() {
            p.flight = flight;
        }
        p.points.push((r.get(2)?, r.get(3)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blah2::Det;
    use crate::truth::Expected;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn expected(hex: &str, r: f64, counted: bool, hit: Option<usize>) -> Expected {
        Expected {
            hex: hex.into(),
            flight: if hex == "a00001" {
                "TST1".into()
            } else {
                String::new()
            },
            range_km: r,
            doppler_hz: 50.0,
            counted,
            hit,
        }
    }

    fn det(r: f64) -> Det {
        Det {
            range_km: r,
            doppler_hz: 50.0,
            snr_db: 14.0,
        }
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn frames_add_up_by_hour_and_kilometre() {
        let mut c = db();
        let f = Frame {
            ts: 10 * HOUR + 5,
            dets: vec![det(12.3), det(30.0)],
        };
        let o = Outcome {
            aircraft: vec![
                expected("a00001", 12.4, true, Some(0)),
                expected("a00002", 40.2, true, None),
                expected("a00003", 80.0, false, None),
            ],
            matched: vec![Some(0), None],
        };
        record(&mut c, &f, &o, true).unwrap();
        record(
            &mut c,
            &Frame {
                ts: f.ts + 500,
                ..f.clone()
            },
            &o,
            false,
        )
        .unwrap();
        assert_eq!(
            count(
                &c,
                "SELECT cpis * 100 + dets * 10 + unmatched FROM hour_frames WHERE hour = 10"
            ),
            242
        );
        assert_eq!(count(&c, "SELECT hits FROM hour_range WHERE km = 12"), 2);
        assert_eq!(count(&c, "SELECT opps FROM hour_range WHERE km = 40"), 2);
        assert_eq!(
            count(&c, "SELECT count(*) FROM hour_range WHERE km = 80"),
            0,
            "not counted"
        );
        assert_eq!(count(&c, "SELECT count(*) FROM recent_det"), 4);
        assert_eq!(
            count(&c, "SELECT count(*) FROM recent_truth"),
            3,
            "truth only once"
        );
        let paths = recent_paths(&c, 0).unwrap();
        assert_eq!(paths[0].flight, "TST1");
        assert_eq!(paths.len(), 3);
        let dets = recent_dets(&c, 0, 10).unwrap();
        assert_eq!(dets.iter().filter(|d| d.matched).count(), 2);
    }

    #[test]
    fn a_gap_in_the_feed_starts_a_new_path() {
        let mut c = db();
        let o = Outcome {
            aircraft: vec![expected("a00001", 5.0, true, None)],
            matched: vec![],
        };
        for ts in [0, 2_000, 4_000, 60_000, 62_000] {
            let f = Frame { ts, dets: vec![] };
            record(&mut c, &f, &o, true).unwrap();
        }
        let paths = recent_paths(&c, 0).unwrap();
        let lens: Vec<usize> = paths.iter().map(|p| p.points.len()).collect();
        assert_eq!(lens, [3, 2]);
    }

    #[test]
    fn prune_drops_old_rows() {
        let mut c = db();
        let f = Frame {
            ts: 1_000,
            dets: vec![det(5.0)],
        };
        let o = Outcome {
            aircraft: vec![expected("a00001", 5.0, true, Some(0))],
            matched: vec![Some(0)],
        };
        record(&mut c, &f, &o, true).unwrap();
        prune(&c, 1_000 + RECENT_MS - 1).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM recent_det"), 1);
        prune(&c, 1_000 + RECENT_MS + 1).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM recent_det"), 0);
        assert_eq!(count(&c, "SELECT count(*) FROM recent_truth"), 0);
        assert_eq!(
            count(&c, "SELECT count(*) FROM hour_frames"),
            1,
            "hourly counts stay"
        );
        prune(&c, 32 * 24 * HOUR).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM hour_aircraft"), 0);
        assert_eq!(count(&c, "SELECT count(*) FROM hour_range"), 1);
        prune(&c, 401 * 24 * HOUR).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM hour_range"), 0);
    }

    #[test]
    fn a_new_setup_starts_at_the_next_hour() {
        let c = db();
        assert_eq!(setup_since(&c, "a", 5 * HOUR + 10).unwrap(), 0);
        assert_eq!(setup_since(&c, "a", 9 * HOUR).unwrap(), 0);
        assert_eq!(setup_since(&c, "b", 9 * HOUR + 10).unwrap(), 10 * HOUR);
        assert_eq!(setup_since(&c, "b", 12 * HOUR).unwrap(), 10 * HOUR);
        assert_eq!(setup_since(&c, "a", 12 * HOUR + 1).unwrap(), 13 * HOUR);
    }
}
