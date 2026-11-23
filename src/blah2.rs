//! blah2's web API (github.com/30hours/blah2, api/server.js), which serves the latest
//! CPI's results. blah2 itself does the cross-ambiguity function and CFAR detection
//! (src/process/ambiguity, src/process/detection); this app only reads what they find:
//!
//!   GET /api/detection  {"timestamp": unix ms, "delay": [km], "doppler": [Hz], "snr": [dB]}
//!   GET /api/config     the config.yml blah2 was started with, as JSON
//!   GET /api/timing     {"timestamp", "nCpi", "uptime_s", "uptime_days", <stage>: ms, "cpi": ms}
//!
//! The shapes come from blah2's source (checked at commit c821bee, February 2026):
//! Detection::to_json and delay_bin_to_km in src/data/Detection.cpp, Timing::to_json in
//! src/data/meta/Timing.cpp, and src/blah2.cpp, which stamps each CPI with the time its
//! processing started and times each stage separately. "delay" is the bistatic range
//! (|Tx-P| + |P-Rx| - |Tx-Rx|), already turned from delay bins into km; every number has
//! at most two decimals. Before the first CPI the endpoints return an empty body.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::geo::{C, Pair, ecef};

/// More than this in one CPI means the CFAR threshold is far too low; keep the strongest.
pub const MAX_PER_CPI: usize = 500;

/// server.js appends each message from blah2 to a variable that starts out undefined, so
/// the first one after it starts is served as "undefined{...}".
fn json_part(body: &str) -> &str {
    let b = body.trim();
    b.strip_prefix("undefined").unwrap_or(b)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Det {
    pub range_km: f64,
    pub doppler_hz: f64,
    pub snr_db: f64,
}

/// One CPI's detections.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub ts: i64,
    pub dets: Vec<Det>,
}

#[derive(Deserialize)]
struct RawDetection {
    timestamp: i64,
    delay: Vec<f64>,
    doppler: Vec<f64>,
    snr: Vec<f64>,
}

pub fn parse_detection(body: &str) -> Result<Option<Frame>> {
    if body.trim().is_empty() {
        return Ok(None);
    }
    let r: RawDetection = serde_json::from_str(json_part(body)).context("blah2 detection JSON")?;
    if r.delay.len() != r.doppler.len() || r.delay.len() != r.snr.len() {
        bail!(
            "blah2 detection arrays differ in length: {} delay, {} doppler, {} snr",
            r.delay.len(),
            r.doppler.len(),
            r.snr.len()
        );
    }
    let mut dets: Vec<Det> = (0..r.delay.len())
        .map(|i| Det {
            range_km: r.delay[i],
            doppler_hz: r.doppler[i],
            snr_db: r.snr[i],
        })
        .filter(|d| d.range_km.is_finite() && d.doppler_hz.is_finite() && d.snr_db.is_finite())
        .collect();
    if dets.len() > MAX_PER_CPI {
        dets.sort_by(|a, b| b.snr_db.total_cmp(&a.snr_db));
        dets.truncate(MAX_PER_CPI);
    }
    Ok(Some(Frame {
        ts: r.timestamp,
        dets,
    }))
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Site {
    pub latitude: f64,
    pub longitude: f64,
    /// metres
    pub altitude: f64,
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize)]
struct RawConfig {
    capture: RawCapture,
    process: RawProcess,
    location: RawLocation,
}

#[derive(Deserialize)]
struct RawCapture {
    fs: f64,
    fc: f64,
    device: RawDevice,
}

#[derive(Deserialize)]
struct RawDevice {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct RawProcess {
    data: RawData,
    ambiguity: RawAmbiguity,
    detection: RawDetector,
}

#[derive(Deserialize)]
struct RawData {
    cpi: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAmbiguity {
    delay_min: f64,
    delay_max: f64,
    doppler_min: f64,
    doppler_max: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetector {
    #[serde(default = "yes")]
    enable: bool,
    min_delay: f64,
    min_doppler: f64,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct RawLocation {
    rx: Site,
    tx: Site,
}

/// Where in bistatic range and Doppler blah2 can report a detection at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub min_km: f64,
    pub max_km: f64,
    pub dop_min: f64,
    pub dop_max: f64,
    /// the CFAR detector ignores anything slower than this (|Doppler| below it), where the
    /// direct signal and ground clutter sit
    pub notch_hz: f64,
}

impl Window {
    pub fn contains(&self, range_km: f64, doppler_hz: f64) -> bool {
        (self.min_km..=self.max_km).contains(&range_km)
            && (self.dop_min..=self.dop_max).contains(&doppler_hz)
            && doppler_hz.abs() >= self.notch_hz
    }
}

/// What this app needs from blah2's config.
#[derive(Clone, Debug, PartialEq)]
pub struct Setup {
    pub device: String,
    /// sample rate, Hz
    pub fs: f64,
    /// illuminator centre frequency, Hz
    pub fc: f64,
    pub cpi_s: f64,
    pub detection: bool,
    pub window: Window,
    pub rx: Site,
    pub tx: Site,
}

impl Setup {
    /// One delay bin in km of bistatic range.
    pub fn bin_km(&self) -> f64 {
        C / self.fs / 1000.0
    }

    pub fn pair(&self) -> Pair {
        Pair::new(
            ecef(self.tx.latitude, self.tx.longitude, self.tx.altitude),
            ecef(self.rx.latitude, self.rx.longitude, self.rx.altitude),
            self.fc,
        )
    }

    /// Changes whenever something that moves an aircraft's bistatic range or Doppler, or
    /// what blah2 can see, changes; statistics from different setups don't mix.
    pub fn fingerprint(&self) -> String {
        let w = &self.window;
        format!(
            "rx {:.5},{:.5},{:.0} tx {:.5},{:.5},{:.0} fc {:.0} fs {:.0} cpi {} window {:.2}-{:.2} km {}..{} Hz notch {}",
            self.rx.latitude,
            self.rx.longitude,
            self.rx.altitude,
            self.tx.latitude,
            self.tx.longitude,
            self.tx.altitude,
            self.fc,
            self.fs,
            self.cpi_s,
            w.min_km,
            w.max_km,
            w.dop_min,
            w.dop_max,
            w.notch_hz
        )
    }
}

pub fn parse_config(body: &str) -> Result<Setup> {
    let r: RawConfig = serde_json::from_str(body).context("blah2 config JSON")?;
    if r.capture.fs <= 0.0 || r.capture.fc <= 0.0 || r.process.data.cpi <= 0.0 {
        bail!("blah2 config has a zero or negative fs, fc or cpi");
    }
    for s in [&r.location.rx, &r.location.tx] {
        if !(-90.0..=90.0).contains(&s.latitude) || !(-180.0..=180.0).contains(&s.longitude) {
            bail!(
                "blah2 config location {:?} is not a valid latitude/longitude",
                s.name
            );
        }
    }
    let bin_km = C / r.capture.fs / 1000.0;
    let a = &r.process.ambiguity;
    let d = &r.process.detection;
    Ok(Setup {
        device: r.capture.device.kind.chars().take(32).collect(),
        fs: r.capture.fs,
        fc: r.capture.fc,
        cpi_s: r.process.data.cpi,
        detection: d.enable,
        window: Window {
            min_km: a.delay_min.max(d.min_delay).max(0.0) * bin_km,
            max_km: a.delay_max * bin_km,
            dop_min: a.doppler_min,
            dop_max: a.doppler_max,
            notch_hz: d.min_doppler,
        },
        rx: trim_name(r.location.rx),
        tx: trim_name(r.location.tx),
    })
}

fn trim_name(mut s: Site) -> Site {
    s.name = s.name.chars().take(64).collect();
    s
}

/// How long blah2 takes to process a CPI, from /api/timing.
#[derive(Clone, Debug, PartialEq)]
pub struct Timing {
    pub n_cpi: i64,
    pub uptime_s: f64,
    /// wall time for the whole CPI's processing, ms
    pub cpi_ms: f64,
}

pub fn parse_timing(body: &str) -> Result<Timing> {
    let v: Value = serde_json::from_str(json_part(body)).context("blah2 timing JSON")?;
    let num = |k: &str| {
        v.get(k)
            .and_then(Value::as_f64)
            .with_context(|| format!("blah2 timing has no {k:?}"))
    };
    Ok(Timing {
        n_cpi: num("nCpi")? as i64,
        uptime_s: num("uptime_s")?,
        cpi_ms: num("cpi")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_detection_frame() {
        let f = parse_detection(include_str!("../fixtures/blah2-detection.json"))
            .unwrap()
            .unwrap();
        assert_eq!(f.ts, 1_795_000_000_250);
        assert_eq!(f.dets.len(), 3);
        assert_eq!(
            f.dets[1],
            Det {
                range_km: 21.74,
                doppler_hz: -63.47,
                snr_db: 17.05
            }
        );
    }

    #[test]
    fn empty_and_broken_detection_bodies() {
        assert_eq!(parse_detection("").unwrap(), None);
        let none = parse_detection(r#"{"timestamp":5,"delay":[],"doppler":[],"snr":[]}"#)
            .unwrap()
            .unwrap();
        assert!(none.dets.is_empty());
        let err =
            parse_detection(r#"{"timestamp":5,"delay":[1.0],"doppler":[],"snr":[]}"#).unwrap_err();
        assert!(err.to_string().contains("differ in length"), "{err}");
        assert!(parse_detection("{\"timestamp\":").is_err());
    }

    #[test]
    fn too_many_detections_keeps_the_strongest() {
        let n = MAX_PER_CPI + 10;
        let snr: Vec<String> = (0..n).map(|i| format!("{i}.0")).collect();
        let ones = vec!["1.0"; n].join(",");
        let body = format!(
            r#"{{"timestamp":1,"delay":[{ones}],"doppler":[{ones}],"snr":[{}]}}"#,
            snr.join(",")
        );
        let f = parse_detection(&body).unwrap().unwrap();
        assert_eq!(f.dets.len(), MAX_PER_CPI);
        assert!(f.dets.iter().all(|d| d.snr_db >= 10.0));
    }

    #[test]
    fn reads_the_example_config() {
        // blah2's own config/config.yml, as /api/config serves it
        let s = parse_config(include_str!("../fixtures/blah2-config.json")).unwrap();
        assert_eq!(s.device, "RspDuo");
        assert_eq!(s.fc, 204_640_000.0);
        assert_eq!(s.cpi_s, 0.75);
        assert_eq!(s.rx.name, "Adelaide");
        assert_eq!(s.tx.altitude, 750.0);
        // 2 MHz sampling: one bin is c/fs = 149.9 m; delayMax 400 bins, minDelay 5 bins
        assert!((s.bin_km() - 0.149_896).abs() < 1e-6);
        assert!((s.window.max_km - 59.958).abs() < 1e-3);
        assert!((s.window.min_km - 0.749_48).abs() < 1e-4);
        assert_eq!((s.window.dop_min, s.window.dop_max), (-200.0, 200.0));
        assert!(s.window.contains(20.0, 40.0));
        assert!(!s.window.contains(20.0, 10.0), "inside the clutter notch");
        assert!(!s.window.contains(70.0, 40.0), "past delayMax");
        assert!(s.detection);
        let mut moved = s.clone();
        moved.tx.latitude += 0.001;
        assert_ne!(s.fingerprint(), moved.fingerprint());
        assert_eq!(s.fingerprint(), s.clone().fingerprint());
    }

    #[test]
    fn config_without_a_location_is_refused() {
        let mut v: Value =
            serde_json::from_str(include_str!("../fixtures/blah2-config.json")).unwrap();
        v.as_object_mut().unwrap().remove("location");
        assert!(parse_config(&v.to_string()).is_err());
    }

    #[test]
    fn reads_timing() {
        let body = include_str!("../fixtures/blah2-timing.json");
        let t = parse_timing(body).unwrap();
        assert_eq!(t.n_cpi, 48_211);
        assert_eq!(t.cpi_ms, 412.37);
        assert!(parse_timing("{}").is_err());
        // blah2 times each stage on its own, so the stages add up to about the whole CPI
        let v: Value = serde_json::from_str(body).unwrap();
        let stages: f64 = v
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, _)| {
                !["timestamp", "nCpi", "uptime_s", "uptime_days", "cpi"].contains(&k.as_str())
            })
            .map(|(_, x)| x.as_f64().unwrap())
            .sum();
        assert!((stages - t.cpi_ms).abs() < 1.0, "{stages} vs {}", t.cpi_ms);
    }

    #[test]
    fn the_first_message_after_blah2s_api_starts() {
        let det = include_str!("../fixtures/blah2-detection.json");
        assert_eq!(
            parse_detection(&format!("undefined{det}")).unwrap(),
            parse_detection(det).unwrap()
        );
        let timing = include_str!("../fixtures/blah2-timing.json");
        assert_eq!(
            parse_timing(&format!("undefined{timing}")).unwrap(),
            parse_timing(timing).unwrap()
        );
    }
}
