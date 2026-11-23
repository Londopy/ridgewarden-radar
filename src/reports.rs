//! Detection reports over the mesh. Detections are batched into `radar.detections`
//! telemetry events: bistatic range, Doppler, SNR and time only, never samples or audio
//! (Part 97.113(c)). Each site also logs its geometry as a `radar.site` event whenever
//! blah2's setup changes, so any site can locate aircraft from everyone's reports.
//!
//! The spec allows the reports kilobits: each site's average at most `mesh.max_kbps`
//! (1 by default), the core's signed envelope included. Past that the weakest detections
//! are left out. To fit more in, a report is columns of integers:
//!
//!   {"v": 2, "fc_hz", "t0": the first CPI's time (unix ms),
//!    "c": [ms since the CPI before, 0 for the first], "n": [detections in each CPI],
//!    "r": [bistatic range, 10 m], "f": [Doppler, 0.1 Hz], "s": [SNR, dB],
//!    "dropped": detections left out}
//!
//! with `r`, `f` and `s` in CPI order, the first `n[0]` of them in the first CPI.
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
/// What the core wraps around a payload (keys, signature, chain, clocks): about 530 bytes
/// typical, under 600. The budget test checks a worst case fits.
const ENVELOPE_BYTES: usize = 600;
/// The core refuses bigger payloads.
const CORE_MAX_PAYLOAD: usize = 128 * 1024;
/// more than this in one report from someone else is not a report we believe
const MAX_IN_REPORT: usize = 5000;
const MAX_CPIS: usize = 40_000;
const MAX_SPAN_MS: i64 = 2 * 3_600_000;
/// how much of everyone's detections is kept for tracking
pub const KEEP_MESH_MS: i64 = 2 * 3_600_000;

/// The largest payload a report every `batch_s` seconds may have, to average `kbps`.
pub fn budget(batch_s: u64, kbps: u64) -> usize {
    (bytes_allowed(batch_s, kbps) - ENVELOPE_BYTES).min(CORE_MAX_PAYLOAD)
}

/// What a report every `batch_s` seconds may take on the mesh, envelope and all.
pub fn bytes_allowed(batch_s: u64, kbps: u64) -> usize {
    (kbps * 1000 / 8 * batch_s) as usize
}

/// How many detections a report carries, and how many it left out.
pub fn kept_and_left_out(p: &Value) -> (usize, usize) {
    let kept = p["r"].as_array().map_or(0, Vec::len);
    let left = p["dropped"].as_u64().unwrap_or(0) as usize;
    (kept, left)
}

/// The largest k in lo..=hi for which `ok` holds, given that it holds for lo and, once
/// it fails, fails for every larger k.
fn largest(lo: usize, hi: usize, ok: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (lo, hi);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if ok(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Detections waiting to go out in the next report.
#[derive(Default)]
pub struct Batch {
    cpis: Vec<i64>,
    /// each with the index of its CPI
    dets: Vec<(usize, Det)>,
}

impl Batch {
    pub fn push(&mut self, f: &Frame) {
        // reports give each CPI as a step from the one before, and a step back would make
        // the whole report unreadable
        if self.cpis.last().is_some_and(|t| f.ts <= *t) {
            return;
        }
        let i = self.cpis.len();
        self.cpis.push(f.ts);
        self.dets.extend(f.dets.iter().map(|d| (i, *d)));
    }

    /// The report payload, emptying the batch; None if no CPI came in. It carries every
    /// CPI and the strongest detections, at most `max`, that fit in `max_bytes`. If not
    /// even the CPI times fit, only the newest CPIs go.
    pub fn take(&mut self, fc_hz: f64, max: usize, max_bytes: usize) -> Option<Value> {
        let b = std::mem::take(self);
        if b.cpis.is_empty() {
            return None;
        }
        let fits = |v: &Value| v.to_string().len() <= max_bytes;
        let mut strongest: Vec<usize> = (0..b.dets.len()).collect();
        strongest.sort_by(|&x, &y| b.dets[y].1.snr_db.total_cmp(&b.dets[x].1.snr_db));
        let with = |k: usize| {
            let mut keep = strongest[..k].to_vec();
            keep.sort_unstable();
            b.encode(fc_hz, 0, &keep)
        };
        if fits(&with(0)) {
            let k = largest(0, max.min(b.dets.len()), |k| fits(&with(k)));
            return Some(with(k));
        }
        let n = b.cpis.len();
        let m = largest(1, n, |m| fits(&b.encode(fc_hz, n - m, &[])));
        Some(b.encode(fc_hz, n - m, &[]))
    }

    /// The payload for the CPIs from `from` on and the detections `keep` (indices into
    /// `dets`, in order, all in those CPIs).
    fn encode(&self, fc_hz: f64, from: usize, keep: &[usize]) -> Value {
        let cpis = &self.cpis[from..];
        let c: Vec<i64> = cpis
            .iter()
            .scan(cpis[0], |prev, &t| Some(t - std::mem::replace(prev, t)))
            .collect();
        let mut n = vec![0u32; cpis.len()];
        let (mut r, mut f, mut s) = (vec![], vec![], vec![]);
        for &j in keep {
            let (i, d) = self.dets[j];
            n[i - from] += 1;
            r.push((d.range_km * 100.0).round() as i64);
            f.push((d.doppler_hz * 10.0).round() as i64);
            s.push(d.snr_db.round() as i64);
        }
        json!({
            "v": 2,
            "fc_hz": fc_hz,
            "t0": cpis[0],
            "c": c,
            "n": n,
            "r": r,
            "f": f,
            "s": s,
            "dropped": self.dets.len() - keep.len(),
        })
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

/// A report's CPI times and detections, if it is well formed. Reports from 0.1.0 (rows
/// instead of columns, `"v": 1`) are not read.
fn parse_report(p: &Value) -> Option<Parsed> {
    if p.get("v")?.as_i64()? != 2 {
        return None;
    }
    // a report from the future is a broken clock, and would never be pruned
    let t0 = p
        .get("t0")?
        .as_i64()
        .filter(|t| (0..=now_ms() + 60_000).contains(t))?;
    let ints = |k: &str, max_len: usize, lim: i64| -> Option<Vec<i64>> {
        p.get(k)?
            .as_array()
            .filter(|a| a.len() <= max_len)?
            .iter()
            .map(|x| x.as_i64().filter(|x| (-lim..=lim).contains(x)))
            .collect()
    };
    let c = ints("c", MAX_CPIS, MAX_SPAN_MS)?;
    let n = ints("n", MAX_CPIS, MAX_IN_REPORT as i64)?;
    let r = ints("r", MAX_IN_REPORT, 100_000)?;
    let f = ints("f", MAX_IN_REPORT, 100_000)?;
    let s = ints("s", MAX_IN_REPORT, 1000)?;
    let ok = n.len() == c.len()
        && f.len() == r.len()
        && s.len() == r.len()
        && n.iter().all(|k| *k >= 0)
        && n.iter().sum::<i64>() == r.len() as i64;
    if !ok {
        return None;
    }
    let mut cpis = Vec::with_capacity(c.len());
    let mut t = t0;
    for d in c {
        t += d;
        if d < 0 || t - t0 > MAX_SPAN_MS {
            return None;
        }
        cpis.push(t);
    }
    let mut dets = Vec::with_capacity(r.len());
    let mut j = 0;
    for (ts, k) in cpis.iter().zip(&n) {
        for _ in 0..*k {
            let det = Det {
                range_km: r[j] as f64 / 100.0,
                doppler_hz: f[j] as f64 / 10.0,
                snr_db: s[j] as f64,
            };
            dets.push((*ts, det));
            j += 1;
        }
    }
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
        // one this version can't read, such as 0.1.0's, still counts, with nothing in it
        let (cpis, dets) = parse_report(&e.payload).unwrap_or_default();
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
                // the whole signed event, as it crosses the mesh
                serde_json::to_string(e)?.len() as i64
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
    /// reports this version couldn't read, counted with no CPIs
    pub unreadable: i64,
}

pub fn stations(conn: &Connection, from: i64) -> Result<Vec<Station>> {
    let mut st = conn.prepare(
        "SELECT max(call), count(*), sum(cpis), sum(dets), sum(bytes), max(ts), sum(cpis = 0)
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
            unreadable: r.get(6)?,
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

    /// Size on the wire of a telemetry event carrying `payload`, with the biggest envelope
    /// the core can give it: a 20-character callsign and 2^53 for its counters.
    fn signed_size(payload: Value) -> usize {
        let e = Event {
            id: "0".repeat(64),
            author: ridgewarden_client::event::Author {
                call: "N0TEST/ABCDEFGH-1234".into(),
                key: "0".repeat(64),
            },
            seq: 1 << 53,
            prev: Some("0".repeat(64)),
            lamport: 1 << 53,
            wall_time: "2026-11-20T08:17:18.291Z".into(),
            mono_time: 1 << 53,
            kind: KIND.into(),
            payload,
            sig: "0".repeat(128),
            log: Some("telemetry".into()),
        };
        serde_json::to_string(&e).unwrap().len()
    }

    #[test]
    fn reports_round_trip_in_columns() {
        let mut b = Batch::default();
        assert!(b.take(96.9e6, 1000, 7000).is_none(), "nothing to send");
        let t0 = 1_795_000_000_250;
        b.push(&frame(t0, 3));
        b.push(&frame(t0 + 751, 0));
        // a CPI that isn't after the one before is left out
        b.push(&frame(t0 + 500, 2));
        b.push(&frame(t0 + 751, 2));
        b.push(&frame(t0 + 1500, 1));
        let p = b.take(96.9e6, 1000, 7000).unwrap();
        assert!(
            b.take(96.9e6, 1000, 7000).is_none(),
            "the batch was emptied"
        );
        assert_eq!(p["t0"], t0);
        assert_eq!(p["c"], json!([0, 751, 749]));
        assert_eq!(p["n"], json!([3, 0, 1]));
        // 10.12346 km and -40.77 Hz are 1012 tens of metres and -408 tenths of a hertz
        assert_eq!(p["r"], json!([1000, 1012, 1025, 1000]));
        assert_eq!(p["f"], json!([-400, -408, -415, -400]));
        assert_eq!(p["s"], json!([12, 13, 14, 12]));
        assert_eq!(p["dropped"], 0);
        let (cpis, dets) = parse_report(&p).unwrap();
        assert_eq!(cpis, [t0, t0 + 751, t0 + 1500]);
        let times: Vec<i64> = dets.iter().map(|d| d.0).collect();
        assert_eq!(times, [t0, t0, t0, t0 + 1500]);
        assert_eq!(
            dets[1].1,
            Det {
                range_km: 10.12,
                doppler_hz: -40.8,
                snr_db: 13.0
            }
        );
    }

    /// Detections as wide as blah2 can make them: far out, fast and strong.
    fn wide(ts: i64, n: usize) -> Frame {
        Frame {
            ts,
            dets: (0..n)
                .map(|i| Det {
                    range_km: 299.99 - i as f64 * 0.37,
                    doppler_hz: -999.99 + i as f64 * 1.3,
                    snr_db: 10.0 + ((ts / 7 + i as i64) % 89) as f64,
                })
                .collect(),
        }
    }

    #[test]
    fn reports_stay_inside_the_mesh_budget() {
        let t0 = 1_795_000_000_000;
        // (batch_s, kbps, CPI ms, detections per CPI): blah2's default CPI, short and long
        // ones, short and long batches, the most the config allows, and CPIs so short their
        // times alone don't fit
        for (batch_s, kbps, cpi_ms, per_cpi) in [
            (60, 1, 750, 40),
            (60, 1, 100, 10),
            (10, 1, 500, 40),
            (600, 1, 1000, 40),
            (60, 8, 500, 200),
            (10, 1, 20, 1),
        ] {
            let mut b = Batch::default();
            let n = batch_s as i64 * 1000 / cpi_ms;
            for k in 0..n {
                b.push(&wide(t0 + k * cpi_ms, per_cpi));
            }
            let p = b.take(96.9e6, 5000, budget(batch_s, kbps)).unwrap();
            let bytes = signed_size(p.clone());
            let allowed = bytes_allowed(batch_s, kbps);
            let case = format!("{batch_s} s of {cpi_ms} ms CPIs: {bytes} of {allowed} bytes");
            assert!(bytes <= allowed, "{case}");
            assert!(bytes > allowed * 9 / 10, "budget left unused, {case}");
            let (cpis, dets) = parse_report(&p).unwrap();
            assert_eq!(
                dets.len() + p["dropped"].as_u64().unwrap() as usize,
                n as usize * per_cpi,
                "{case}"
            );
            assert_eq!(*cpis.last().unwrap(), t0 + (n - 1) * cpi_ms, "{case}");
            if cpi_ms >= 100 {
                assert_eq!(cpis.len(), n as usize, "every CPI goes, {case}");
                // and the strongest detections with them
                let mut snr: Vec<f64> = (0..n)
                    .flat_map(|k| wide(t0 + k * cpi_ms, per_cpi).dets)
                    .map(|d| d.snr_db)
                    .collect();
                snr.sort_by(|a, b| b.total_cmp(a));
                let weakest_sent = dets.iter().map(|d| d.1.snr_db).fold(f64::MAX, f64::min);
                assert!(
                    !dets.is_empty() && weakest_sent >= snr[dets.len()],
                    "{case}"
                );
            } else {
                assert!(dets.is_empty() && cpis.len() < n as usize, "{case}");
            }
        }
    }

    #[test]
    fn over_the_budget_the_weakest_go() {
        let mut b = Batch::default();
        b.push(&frame(0, 30));
        let p = b.take(96.9e6, 10, 7000).unwrap();
        assert_eq!(p["dropped"], 20);
        assert_eq!(kept_and_left_out(&p), (10, 20));
        let snr: Vec<i64> = p["s"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_i64().unwrap())
            .collect();
        assert_eq!(snr.len(), 10);
        assert!(snr.iter().all(|s| *s >= 22), "{snr:?}");

        // the same by size: room for about half
        let mut b = Batch::default();
        b.push(&frame(0, 30));
        let all = b.take(96.9e6, 1000, 7000).unwrap().to_string().len();
        b.push(&frame(0, 30));
        let p = b.take(96.9e6, 1000, all - 100).unwrap();
        let kept = p["s"].as_array().unwrap();
        assert!((10..30).contains(&kept.len()), "{} kept", kept.len());
        let weakest_kept = kept.iter().map(|s| s.as_i64().unwrap()).min().unwrap();
        assert!(weakest_kept > 12, "the weakest went first: {p}");
        assert!(p.to_string().len() <= all - 100);
    }

    #[test]
    fn reports_are_counted_and_bad_ones_not_tracked() {
        let mut f = Fake::new(vec![Box::new(Reports)]);
        let now = now_ms();
        let mut b = Batch::default();
        b.push(&frame(now - 2_000, 4));
        b.push(&frame(now - 1_500, 0));
        let good = b.take(96.9e6, 1000, 7000).unwrap();
        let key = f.add(KIND, good.clone(), now).author.key;
        // the whole signed event is counted, not just its payload
        let s = stations(&f.conn, 0).unwrap();
        assert!(s[0].bytes as usize > good.to_string().len() + 300);
        let report = |c: Value, n: Value, r: Value| json!({"v": 2, "t0": now - 1000, "c": c, "n": n, "r": r, "f": [1], "s": [1]});
        assert!(parse_report(&report(json!([0]), json!([1]), json!([1]))).is_some());
        let bad = [
            report(json!([0]), json!([1]), json!(["x"])),
            report(json!([0]), json!([1]), json!("lots")),
            report(json!([0]), json!([1]), json!([1, 2])),
            report(json!([0]), json!([2]), json!([1])),
            report(json!([0, 0]), json!([-1, 2]), json!([1])),
            report(json!([0, -5]), json!([1, 0]), json!([1])),
            report(json!(vec![0; MAX_CPIS + 1]), json!([1]), json!([1])),
            report(json!([0, 7_000_000, 300_000]), json!([1, 0, 0]), json!([1])),
            report(json!([0]), json!([1]), json!([1e9])),
            report(json!([0]), json!([i64::MAX]), json!([1])),
            report(json!([0]), json!([1]), json!([i64::MIN])),
            json!({"v": 2, "t0": i64::MAX - 10, "c": [0], "n": [1], "r": [1], "f": [1], "s": [1]}),
            json!({"v": 2, "t0": now - 1000, "c": [0], "n": [1], "r": [1], "f": [], "s": [1]}),
            json!({"v": 2, "t0": now - 1000, "c": [0], "n": [1], "r": [1], "f": [1], "s": []}),
            // what 0.1.0 sent
            json!({"v": 1, "t0": now, "c": [0], "d": [[0, 2.5, 3.0, 4.0]]}),
        ];
        let n_bad = bad.len() as i64;
        for bad in bad {
            assert!(parse_report(&bad).is_none(), "{bad}");
            f.add(KIND, bad, now);
        }
        f.add("other.thing", good.clone(), now);
        let s = stations(&f.conn, 0).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].call, "N0TEST");
        // reports this version can't read still show, with nothing in them
        assert_eq!(
            (s[0].reports, s[0].cpis, s[0].dets, s[0].unreadable),
            (1 + n_bad, 2, 4, n_bad)
        );
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
        f.add(KIND, b.take(96.9e6, 1000, 7000).unwrap(), then);
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
