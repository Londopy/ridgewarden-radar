//! The aircraft list from dump1090-fa, readsb or tar1090 (`aircraft.json`, documented in
//! dump1090's README-json.md): `now` in unix seconds, and for each aircraft `hex`,
//! `flight`, `lat`/`lon`, `alt_geom` or `alt_baro` (feet, or "ground"), `gs` (knots),
//! `track` (degrees true), `geom_rate` or `baro_rate` (feet/min) and `seen_pos` (seconds
//! before `now`). Any field can be missing; aircraft without a usable position and
//! velocity are left out.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::geo::{FT, V3, ecef, velocity};

/// A busy feed lists a few hundred; this keeps a broken one from costing much.
const MAX_AIRCRAFT: usize = 2000;

/// An aircraft's ADS-B position and velocity at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// ICAO address as the feed gives it; "~" marks a non-ICAO one
    pub hex: String,
    /// callsign, empty if the feed has none
    pub flight: String,
    /// when the position was reported, unix ms
    pub ts: i64,
    /// degrees, where the feed put it at `ts`
    pub lat: f64,
    pub lon: f64,
    pub pos: V3,
    pub vel: V3,
}

fn num(a: &Value, k: &str) -> Option<f64> {
    a.get(k).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn hex(a: &Value) -> Option<String> {
    let h = a.get("hex")?.as_str()?.to_ascii_lowercase();
    let digits = h.strip_prefix('~').unwrap_or(&h);
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

fn flight(a: &Value) -> String {
    let f = a.get("flight").and_then(Value::as_str).unwrap_or("").trim();
    if f.len() <= 8 && f.bytes().all(|b| b.is_ascii_alphanumeric()) {
        f.to_string()
    } else {
        String::new()
    }
}

fn track(a: &Value, now: f64, max_age_s: f64) -> Option<Track> {
    let seen = num(a, "seen_pos")?;
    if !(0.0..=max_age_s).contains(&seen) {
        return None;
    }
    let (lat, lon) = (num(a, "lat")?, num(a, "lon")?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    // alt_baro is the string "ground" for aircraft on the ground, which num() refuses
    let alt_ft = num(a, "alt_geom").or_else(|| num(a, "alt_baro"))?;
    let vrate = num(a, "geom_rate")
        .or_else(|| num(a, "baro_rate"))
        .unwrap_or(0.0);
    Some(Track {
        hex: hex(a)?,
        flight: flight(a),
        ts: ((now - seen) * 1000.0).round() as i64,
        lat,
        lon,
        pos: ecef(lat, lon, alt_ft * FT),
        vel: velocity(lat, lon, num(a, "gs")?, num(a, "track")?, vrate),
    })
}

/// One read of the feed: when the receiver wrote it (unix ms, by its clock), and the
/// airborne aircraft whose position was then at most `max_age_s` old.
pub struct List {
    pub now: i64,
    pub tracks: Vec<Track>,
}

pub fn parse(body: &str, max_age_s: f64) -> Result<List> {
    let v: Value = serde_json::from_str(body).context("aircraft.json")?;
    let now = num(&v, "now").context("aircraft.json has no \"now\"")?;
    let list = v
        .get("aircraft")
        .and_then(Value::as_array)
        .context("aircraft.json has no \"aircraft\" list")?;
    Ok(List {
        now: (now * 1000.0).round() as i64,
        tracks: list
            .iter()
            .take(MAX_AIRCRAFT)
            .filter_map(|a| track(a, now, max_age_s))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{norm, sub};

    #[test]
    fn reads_a_dump1090_list() {
        let l = parse(include_str!("../fixtures/aircraft.json"), 10.0).unwrap();
        assert_eq!(l.now, 1_795_000_000_400);
        let t = l.tracks;
        // the one on the ground, the one with no position and the one 45 s stale are out
        let hexes: Vec<&str> = t.iter().map(|a| a.hex.as_str()).collect();
        assert_eq!(hexes, ["a1b2c3", "~2a01f4"]);
        let a = &t[0];
        assert_eq!(a.flight, "TST101");
        assert_eq!(a.ts, 1_795_000_000_000);
        // alt_geom is preferred: 34575 ft
        let p = ecef(39.612345, -104.702211, 34_575.0 * FT);
        assert!(norm(sub(a.pos, p)) < 1e-6);
        assert!((norm(a.vel) - 451.3 * crate::geo::KT).abs() < 0.01);
        // no alt_geom or geom_rate: falls back to the barometric ones
        let b = &t[1];
        assert_eq!(b.flight, "TST7");
        assert!(norm(sub(b.pos, ecef(39.4, -104.9, 6500.0 * FT))) < 1e-6);
        let sink = 640.0 * FT / 60.0;
        let level = 140.0 * crate::geo::KT;
        assert!((norm(b.vel) - (sink * sink + level * level).sqrt()).abs() < 1e-6);
    }

    #[test]
    fn odd_values_are_left_out() {
        let body = r#"{"now": 100.0, "aircraft": [
            {"hex":"zzzzzz","lat":1,"lon":1,"alt_baro":1000,"gs":100,"track":0,"seen_pos":0},
            {"hex":"abcdef","flight":"<b>x</b>","lat":1,"lon":1,"alt_baro":1000,"gs":100,"track":0,"seen_pos":0},
            {"hex":"abcde0","lat":91,"lon":1,"alt_baro":1000,"gs":100,"track":0,"seen_pos":0}
        ]}"#;
        let t = parse(body, 10.0).unwrap().tracks;
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].flight, "", "markup in a callsign is dropped");
        assert!(parse("{}", 10.0).is_err());
        assert!(parse("[]", 10.0).is_err());
    }
}
