//! Detection reports over the mesh. Detections are batched into `radar.detections`
//! telemetry events: bistatic range, Doppler, SNR and time only, never samples or audio
//! (Part 97.113(c)). A minute of a busy CPI stream is a few kilobytes. Each site also logs
//! its geometry as a `radar.site` event whenever blah2's setup changes, so any site can
//! locate aircraft from everyone's reports.
//!
//! The `Reports` projection notes every station's batches as they come back through the
//! feed, and keeps the last two hours of their CPIs and detections for the tracker.

use anyhow::Result;
use ridgewarden_client::core::Projection;
use ridgewarden_client::event::Event;
use ridgewarden_client::util::now_ms;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::blah2::{Det, Frame, MAX_PER_CPI, Setup, Site, Window};

pub const KIND: &str = "radar.detections";
pub const SITE_KIND: &str = "radar.site";
/// more than this in one report from someone else is not a report we believe
const MAX_IN_REPORT: usize = 5000;
const MAX_CPIS: usize = 40_000;
const MAX_SPAN_MS: i64 = 2 * 3_600_000;
/// how much of everyone's detections is kept for tracking
pub const KEEP_MESH_MS: i64 = 2 * 3_600_000;

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// Detections waiting to go out in the next report.
#[derive(Default)]
pub struct Batch {
    t0: Option<i64>,
    cpis: Vec<i64>,
    dets: Vec<(i64, Det)>,
}

impl Batch {
    pub fn push(&mut self, f: &Frame) {
        self.t0.get_or_insert(f.ts);
        self.cpis.push(f.ts);
        self.dets.extend(f.dets.iter().map(|d| (f.ts, *d)));
    }

    /// The report payload, emptying the batch; None if no CPI came in. Past `max`
    /// detections the weakest are dropped, and the payload says how many.
    pub fn take(&mut self, fc_hz: f64, max: usize) -> Option<Value> {
        let t0 = self.t0?;
        let mut dets = std::mem::take(&mut self.dets);
        let dropped = dets.len().saturating_sub(max);
        if dropped > 0 {
            dets.sort_by(|a, b| b.1.snr_db.total_cmp(&a.1.snr_db));
            dets.truncate(max);
            dets.sort_by_key(|d| d.0);
        }
        let d: Vec<Value> = dets
            .iter()
            .map(|(ts, d)| {
                json!([
                    ts - t0,
                    round(d.range_km, 2),
                    round(d.doppler_hz, 1),
                    round(d.snr_db, 1)
                ])
            })
            .collect();
        let c: Vec<i64> = self.cpis.iter().map(|t| t - t0).collect();
        let v = json!({
            "v": 1,
            "fc_hz": fc_hz,
            "t0": t0,
            "span_ms": c.last().copied().unwrap_or(0),
            "cpis": c.len(),
            "c": c,
            "fields": ["dt_ms", "range_km", "doppler_hz", "snr_db"],
            "d": d,
            "dropped": dropped,
        });
        *self = Self::default();
        Some(v)
    }
}

/// The `radar.site` payload for a setup.
pub fn site_payload(s: &Setup) -> Value {
    let site = |x: &Site| json!({"latitude": x.latitude, "longitude": x.longitude, "altitude": x.altitude, "name": x.name});
    let w = &s.window;
    json!({
        "v": 1,
        "device": s.device,
        "fs_hz": s.fs,
        "fc_hz": s.fc,
        "cpi_s": s.cpi_s,
        "detection": s.detection,
        "window": {"min_km": w.min_km, "max_km": w.max_km, "dop_min": w.dop_min,
                   "dop_max": w.dop_max, "notch_hz": w.notch_hz},
        "rx": site(&s.rx),
        "tx": site(&s.tx),
    })
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k)?.as_f64().filter(|x| x.is_finite())
}

fn parse_site(v: &Value) -> Option<Site> {
    Some(Site {
        latitude: num(v, "latitude").filter(|x| (-90.0..=90.0).contains(x))?,
        longitude: num(v, "longitude").filter(|x| (-180.0..=180.0).contains(x))?,
        altitude: num(v, "altitude").filter(|x| (-500.0..=9000.0).contains(x))?,
        name: v.get("name")?.as_str()?.chars().take(64).collect(),
    })
}

fn parse_site_payload(p: &Value) -> Option<Setup> {
    let w = p.get("window")?;
    let positive = |x: &f64| *x > 0.0;
    Some(Setup {
        device: p.get("device")?.as_str()?.chars().take(32).collect(),
        fs: num(p, "fs_hz").filter(positive)?,
        fc: num(p, "fc_hz").filter(positive)?,
        cpi_s: num(p, "cpi_s").filter(positive)?,
        detection: p.get("detection")?.as_bool()?,
        window: Window {
            min_km: num(w, "min_km")?,
            max_km: num(w, "max_km")?,
            dop_min: num(w, "dop_min")?,
            dop_max: num(w, "dop_max")?,
            notch_hz: num(w, "notch_hz")?,
        },
        rx: parse_site(p.get("rx")?)?,
        tx: parse_site(p.get("tx")?)?,
    })
}

type Parsed = (Vec<i64>, Vec<(i64, Det)>);

/// A report's CPI times and detections, if it is well formed.
fn parse_report(p: &Value) -> Option<Parsed> {
    // a report from the future is a broken clock, and would never be pruned
    let t0 = p
        .get("t0")?
        .as_i64()
        .filter(|t| (0..=now_ms() + 60_000).contains(t))?;
    let offset = |x: &Value| x.as_i64().filter(|d| (0..=MAX_SPAN_MS).contains(d));
    let c = p.get("c")?.as_array().filter(|c| c.len() <= MAX_CPIS)?;
    let cpis: Vec<i64> = c
        .iter()
        .map(|x| offset(x).map(|d| t0 + d))
        .collect::<Option<_>>()?;
    let d = p
        .get("d")?
        .as_array()
        .filter(|d| d.len() <= MAX_IN_REPORT)?;
    let dets = d
        .iter()
        .map(|row| {
            let r = row.as_array().filter(|r| r.len() == 4)?;
            let f = |i: usize, lim: f64| r[i].as_f64().filter(|x| x.is_finite() && x.abs() <= lim);
            Some((
                t0 + offset(&r[0])?,
                Det {
                    range_km: f(1, 1000.0)?,
                    doppler_hz: f(2, 10_000.0)?,
                    snr_db: f(3, 1000.0)?,
                },
            ))
        })
        .collect::<Option<_>>()?;
    Some((cpis, dets))
}

pub struct Reports;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reports (
    event_id   TEXT PRIMARY KEY,
    ts         INTEGER NOT NULL,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    cpis       INTEGER NOT NULL,
    dets       INTEGER NOT NULL,
    bytes      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS reports_ts ON reports(ts);
CREATE TABLE IF NOT EXISTS radar_sites (
    event_id   TEXT PRIMARY KEY,
    ts         INTEGER NOT NULL,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS mesh_cpi (
    author_key TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (author_key, ts)
);
CREATE TABLE IF NOT EXISTS mesh_det (
    author_key TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    range_km   REAL NOT NULL,
    doppler_hz REAL NOT NULL,
    snr_db     REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS mesh_det_key_ts ON mesh_det(author_key, ts);";

impl Projection for Reports {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM reports; DELETE FROM radar_sites; DELETE FROM mesh_cpi; DELETE FROM mesh_det;",
        )?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        let call: String = e.author.call.chars().take(16).collect();
        if e.kind == SITE_KIND {
            // only each station's latest geometry matters; keep that, as we read it
            let newer: bool = tx.query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM radar_sites WHERE author_key = ?1 AND ts > ?2)",
                params![e.author.key, ts],
                |r| r.get(0),
            )?;
            if let Some(setup) = parse_site_payload(&e.payload).filter(|_| newer) {
                tx.execute(
                    "DELETE FROM radar_sites WHERE author_key = ?1",
                    [&e.author.key],
                )?;
                tx.execute(
                    "INSERT INTO radar_sites (event_id, ts, author_key, call, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        e.id,
                        ts,
                        e.author.key,
                        call,
                        site_payload(&setup).to_string()
                    ],
                )?;
            }
            return Ok(());
        }
        if e.kind != KIND {
            return Ok(());
        }
        let Some((cpis, dets)) = parse_report(&e.payload) else {
            return Ok(());
        };
        let added = tx.execute(
            "INSERT OR IGNORE INTO reports (event_id, ts, author_key, call, cpis, dets, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.id,
                ts,
                e.author.key,
                call,
                cpis.len() as i64,
                dets.len() as i64,
                e.payload.to_string().len() as i64
            ],
        )?;
        // replaying a long feed history: only recent reports are worth tracking
        if added == 0 || cpis.first().is_none_or(|t| *t < now_ms() - KEEP_MESH_MS) {
            return Ok(());
        }
        for t in &cpis {
            tx.execute(
                "INSERT OR IGNORE INTO mesh_cpi (author_key, ts) VALUES (?1, ?2)",
                params![e.author.key, t],
            )?;
        }
        for (t, d) in &dets {
            tx.execute(
                "INSERT INTO mesh_det (author_key, ts, range_km, doppler_hz, snr_db)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![e.author.key, t, d.range_km, d.doppler_hz, d.snr_db],
            )?;
        }
        Ok(())
    }
}

/// A radar site heard over the mesh: who runs it, since when, and its geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteGeo {
    pub key: String,
    pub call: String,
    /// when this setup was logged; its detections before then belong to another setup
    pub since: i64,
    pub setup: Setup,
}

/// Every station's latest site, by callsign.
pub fn sites(conn: &Connection) -> Result<Vec<SiteGeo>> {
    let mut st = conn.prepare(
        "SELECT author_key, call, ts, payload FROM radar_sites ORDER BY ts DESC, rowid DESC",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out: Vec<SiteGeo> = vec![];
    for row in rows {
        let (key, call, since, payload) = row?;
        if out.iter().any(|s| s.key == key) {
            continue;
        }
        let setup = serde_json::from_str::<Value>(&payload)
            .ok()
            .and_then(|v| parse_site_payload(&v));
        if let Some(setup) = setup {
            out.push(SiteGeo {
                key,
                call,
                since,
                setup,
            });
        }
    }
    out.sort_by(|a, b| a.call.cmp(&b.call).then(a.key.cmp(&b.key)));
    Ok(out)
}

/// The fingerprint of the site `key` last logged, to know when to log a new one.
pub fn logged_fingerprint(conn: &Connection, key: &str) -> Result<Option<String>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM radar_sites WHERE author_key = ?1 ORDER BY ts DESC, rowid DESC LIMIT 1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(payload
        .and_then(|p| serde_json::from_str::<Value>(&p).ok())
        .and_then(|v| parse_site_payload(&v))
        .map(|s| s.fingerprint()))
}

/// One station's CPIs from `from` to `to`, with their detections, oldest first.
pub fn frames(conn: &Connection, key: &str, from: i64, to: i64) -> Result<Vec<Frame>> {
    let mut st = conn.prepare(
        "SELECT ts FROM mesh_cpi WHERE author_key = ?1 AND ts >= ?2 AND ts <= ?3 ORDER BY ts",
    )?;
    let mut frames: Vec<Frame> = st
        .query_map(params![key, from, to], |r| {
            Ok(Frame {
                ts: r.get(0)?,
                dets: vec![],
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut st = conn.prepare(
        "SELECT ts, range_km, doppler_hz, snr_db FROM mesh_det
         WHERE author_key = ?1 AND ts >= ?2 AND ts <= ?3 ORDER BY ts",
    )?;
    let mut rows = st.query(params![key, from, to])?;
    while let Some(r) = rows.next()? {
        let ts: i64 = r.get(0)?;
        if let Ok(i) = frames.binary_search_by_key(&ts, |f| f.ts) {
            frames[i].dets.push(Det {
                range_km: r.get(1)?,
                doppler_hz: r.get(2)?,
                snr_db: r.get(3)?,
            });
        }
    }
    // the same cap blah2's own feed gets here, so one station can't flood the tracker
    for f in &mut frames {
        if f.dets.len() > MAX_PER_CPI {
            f.dets.sort_by(|a, b| b.snr_db.total_cmp(&a.snr_db));
            f.dets.truncate(MAX_PER_CPI);
        }
    }
    Ok(frames)
}

/// One station's reports since some time.
#[derive(Clone, Debug, PartialEq)]
pub struct Station {
    pub call: String,
    pub reports: i64,
    pub cpis: i64,
    pub dets: i64,
    pub bytes: i64,
    pub last: i64,
}

pub fn stations(conn: &Connection, from: i64) -> Result<Vec<Station>> {
    let mut st = conn.prepare(
        "SELECT max(call), count(*), sum(cpis), sum(dets), sum(bytes), max(ts)
         FROM reports WHERE ts >= ?1 GROUP BY author_key ORDER BY max(call)",
    )?;
    let rows = st.query_map([from], |r| {
        Ok(Station {
            call: r.get(0)?,
            reports: r.get(1)?,
            cpis: r.get(2)?,
            dets: r.get(3)?,
            bytes: r.get(4)?,
            last: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM reports WHERE ts < ?1",
        [now - 35 * 24 * crate::store::HOUR],
    )?;
    conn.execute("DELETE FROM mesh_cpi WHERE ts < ?1", [now - KEEP_MESH_MS])?;
    conn.execute("DELETE FROM mesh_det WHERE ts < ?1", [now - KEEP_MESH_MS])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::mesh::testing::Fake;

    fn frame(ts: i64, n: usize) -> Frame {
        Frame {
            ts,
            dets: (0..n)
                .map(|i| Det {
                    range_km: 10.0 + i as f64 * 0.123_456,
                    doppler_hz: -40.0 - i as f64 * 0.77,
                    snr_db: 12.0 + (i % 20) as f64,
                })
                .collect(),
        }
    }

    #[test]
    fn a_minute_of_detections_is_kilobytes() {
        let mut b = Batch::default();
        assert!(b.take(96.9e6, 1000).is_none(), "nothing to send");
        // 80 CPIs of 0.75 s with 6 detections each: a busy minute
        for i in 0..80 {
            b.push(&frame(1_000_000 + i * 750, 6));
        }
        let p = b.take(96.9e6, 1000).unwrap();
        assert_eq!(p["cpis"], 80);
        assert_eq!(p["span_ms"], 79 * 750);
        assert_eq!(p["c"][2], 1500);
        assert_eq!(p["d"].as_array().unwrap().len(), 480);
        assert_eq!(p["d"][7], json!([750, 10.12, -40.8, 13.0]));
        let bytes = p.to_string().len();
        assert!(bytes < 17_000, "{bytes} bytes");
        assert!(b.take(96.9e6, 1000).is_none(), "the batch was emptied");
    }

    #[test]
    fn over_the_cap_the_weakest_go() {
        let mut b = Batch::default();
        b.push(&frame(0, 30));
        let p = b.take(96.9e6, 10).unwrap();
        assert_eq!(p["dropped"], 20);
        let snr: Vec<f64> = p["d"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[3].as_f64().unwrap())
            .collect();
        assert!(snr.iter().all(|s| *s >= 22.0), "{snr:?}");
    }

    #[test]
    fn reports_are_counted_and_bad_ones_ignored() {
        let mut f = Fake::new(vec![Box::new(Reports)]);
        let now = now_ms();
        let mut b = Batch::default();
        b.push(&frame(now - 2_000, 4));
        b.push(&frame(now - 1_500, 0));
        let good = b.take(96.9e6, 1000).unwrap();
        let key = f.add(KIND, good.clone(), now).author.key;
        let bad = |d: Value, c: Value| json!({"t0": now, "cpis": 1, "c": c, "d": d});
        f.add(KIND, bad(json!([[1, 2, "x", 4]]), json!([0])), now);
        f.add(KIND, bad(json!("lots"), json!([0])), now);
        f.add(KIND, bad(json!([[1, 2, 3, 4]]), json!([-5])), now);
        f.add(
            KIND,
            bad(json!([[1, 2, 3, 4]]), json!(vec![0; MAX_CPIS + 1])),
            now,
        );
        f.add(KIND, bad(json!([[1, 2, 1e9, 4]]), json!([0])), now);
        let future = json!({"t0": i64::MAX - 10, "cpis": 1, "c": [5], "d": [[5, 2, 3, 4]]});
        f.add(KIND, future, now);
        f.add("other.thing", good, now);
        let s = stations(&f.conn, 0).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].reports, s[0].cpis, s[0].dets), (1, 2, 4));
        assert_eq!(s[0].call, "N0TEST");
        assert!(s[0].bytes > 50);
        let fr = frames(&f.conn, &key, now - 10_000, now).unwrap();
        assert_eq!(fr.len(), 2, "the empty CPI is kept too");
        assert_eq!((fr[0].dets.len(), fr[1].dets.len()), (4, 0));
        assert_eq!(fr[0].dets[1].range_km, 10.12);
    }

    #[test]
    fn old_reports_are_counted_but_not_kept_for_tracking() {
        let mut f = Fake::new(vec![Box::new(Reports)]);
        let then = now_ms() - KEEP_MESH_MS - 60_000;
        let mut b = Batch::default();
        b.push(&frame(then, 3));
        f.add(KIND, b.take(96.9e6, 1000).unwrap(), then);
        assert_eq!(stations(&f.conn, 0).unwrap()[0].dets, 3);
        let n: i64 = f
            .conn
            .query_row("SELECT count(*) FROM mesh_det", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn sites_round_trip_and_the_latest_wins() {
        let mut f = Fake::new(vec![Box::new(Reports)]);
        let setup =
            crate::blah2::parse_config(include_str!("../fixtures/blah2-config.json")).unwrap();
        let key = f.add(SITE_KIND, site_payload(&setup), 1_000).author.key;
        let mut moved = setup.clone();
        moved.rx.latitude += 0.01;
        f.add(SITE_KIND, site_payload(&moved), 2_000);
        let mut broken = site_payload(&setup);
        broken["rx"]["latitude"] = json!(95.0);
        f.add(SITE_KIND, broken, 3_000);
        // an older one arriving late doesn't replace it
        f.add(SITE_KIND, site_payload(&setup), 1_500);
        let s = sites(&f.conn).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].setup, moved);
        assert_eq!(s[0].since, 2_000);
        let rows: i64 = f
            .conn
            .query_row("SELECT count(*) FROM radar_sites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(
            logged_fingerprint(&f.conn, &key).unwrap(),
            Some(moved.fingerprint())
        );
        assert_eq!(logged_fingerprint(&f.conn, "nobody").unwrap(), None);
    }
}
