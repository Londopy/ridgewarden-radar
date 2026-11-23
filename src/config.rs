//! `radar.toml`. The transmitter, receiver, frequency and CPI are not here: they come
//! from blah2's own config, so the two can't disagree.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    #[serde(default)]
    pub blah2: Blah2Cfg,
    #[serde(default)]
    pub adsb: AdsbCfg,
    #[serde(default, rename = "match")]
    pub gates: MatchCfg,
    #[serde(default)]
    pub stats: StatsCfg,
    #[serde(default)]
    pub mesh: MeshCfg,
    #[serde(default)]
    pub tracker: TrackerCfg,
    #[serde(default)]
    pub fusion: FusionCfg,
}

/// Where the ridgewarden core runs on this box.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to send detection reports
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Blah2Cfg {
    /// blah2's API (its network.ports.api)
    #[serde(default = "d_blah2_url")]
    pub url: String,
    /// how often to read /api/detection; faster than the CPI so none are missed
    #[serde(default = "d_poll_ms")]
    pub poll_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AdsbCfg {
    /// aircraft.json from dump1090-fa, readsb or tar1090
    #[serde(default = "d_adsb_url")]
    pub url: String,
    #[serde(default = "d_one")]
    pub poll_s: u64,
    /// ADS-B positions older than this are not used to judge a CPI
    #[serde(default = "d_max_age")]
    pub max_age_s: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct MatchCfg {
    /// a detection this close in bistatic range to where ADS-B puts an aircraft...
    #[serde(default = "d_range_gate")]
    pub range_gate_km: f64,
    /// ...and this close in Doppler is that aircraft
    #[serde(default = "d_doppler_gate")]
    pub doppler_gate_hz: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct StatsCfg {
    #[serde(default = "d_bin_km")]
    pub bin_km: i64,
    /// "reliable range" is how far out Pd stays at or above this
    #[serde(default = "d_threshold")]
    pub pd_threshold: f64,
    /// a range bin or aircraft needs this many CPIs in coverage before it is judged
    #[serde(default = "d_min_opps")]
    pub min_opportunities: i64,
    /// an aircraft counts as detected after this many matched CPIs
    #[serde(default = "d_detected_hits")]
    pub detected_hits: i64,
    /// show ICAO addresses and callsigns from the ADS-B feed on the pages
    #[serde(default = "d_true")]
    pub show_aircraft_ids: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct MeshCfg {
    /// send detection reports through the core
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_batch_s")]
    pub batch_s: u64,
    /// what the reports may average, signed events included; past it, or past
    /// max_per_batch, the weakest detections are left out
    #[serde(default = "d_one")]
    pub max_kbps: u64,
    #[serde(default = "d_max_per_batch")]
    pub max_per_batch: usize,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-radar")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8802".parse().unwrap()
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_blah2_url() -> String {
    "http://127.0.0.1:3000".into()
}
fn d_poll_ms() -> u64 {
    250
}
fn d_adsb_url() -> String {
    "http://127.0.0.1:8080/data/aircraft.json".into()
}
fn d_one() -> u64 {
    1
}
fn d_max_age() -> u64 {
    10
}
fn d_range_gate() -> f64 {
    2.0
}
fn d_doppler_gate() -> f64 {
    8.0
}
fn d_bin_km() -> i64 {
    5
}
fn d_threshold() -> f64 {
    0.8
}
fn d_min_opps() -> i64 {
    30
}
fn d_detected_hits() -> i64 {
    3
}
fn d_true() -> bool {
    true
}
fn d_batch_s() -> u64 {
    60
}
fn d_max_per_batch() -> usize {
    1000
}

/// The per-site tracker, which runs on every site's reports.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TrackerCfg {
    /// a track is confirmed after confirm_m detections in its first confirm_n CPIs
    #[serde(default = "d_confirm_m")]
    pub confirm_m: u32,
    #[serde(default = "d_confirm_n")]
    pub confirm_n: u32,
    /// missed CPIs in a row that end a track
    #[serde(default = "d_drop_after")]
    pub drop_after: u32,
    /// how far blah2's detections scatter about the truth
    #[serde(default = "d_range_sigma")]
    pub range_sigma_km: f64,
    #[serde(default = "d_doppler_sigma")]
    pub doppler_sigma_hz: f64,
}

fn d_confirm_m() -> u32 {
    3
}
fn d_confirm_n() -> u32 {
    5
}
fn d_drop_after() -> u32 {
    6
}
fn d_range_sigma() -> f64 {
    0.3
}
fn d_doppler_sigma() -> f64 {
    1.0
}

impl Default for TrackerCfg {
    fn default() -> Self {
        Self {
            confirm_m: d_confirm_m(),
            confirm_n: d_confirm_n(),
            drop_after: d_drop_after(),
            range_sigma_km: d_range_sigma(),
            doppler_sigma_hz: d_doppler_sigma(),
        }
    }
}

/// Locating aircraft from two or more sites' reports, without ADS-B.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FusionCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// work out a fix this often
    #[serde(default = "d_every")]
    pub every_s: u64,
    /// and this long after the moment it is for, so every site's reports have arrived
    #[serde(default = "d_settle")]
    pub settle_s: u64,
    /// seconds of each track one fix uses
    #[serde(default = "d_window")]
    pub window_s: u64,
    /// typical aircraft height and its spread, for the fit
    #[serde(default = "d_alt")]
    pub alt_m: f64,
    #[serde(default = "d_alt_sigma")]
    pub alt_sigma_m: f64,
    /// search this far from this site's receiver
    #[serde(default = "d_area")]
    pub area_km: f64,
    /// fixes less sure than this (1-sigma ellipse) are dropped
    #[serde(default = "d_max_error")]
    pub max_error_km: f64,
    /// a fix this close to an ADS-B aircraft is that aircraft
    #[serde(default = "d_truth_km")]
    pub truth_km: f64,
    /// draw track ellipses and fixes no ADS-B aircraft explains on the map; they are
    /// counted either way
    #[serde(default)]
    pub show_unmatched: bool,
}

fn d_every() -> u64 {
    10
}
fn d_settle() -> u64 {
    300
}
fn d_window() -> u64 {
    60
}
fn d_alt() -> f64 {
    6000.0
}
fn d_alt_sigma() -> f64 {
    3000.0
}
fn d_area() -> f64 {
    100.0
}
fn d_max_error() -> f64 {
    5.0
}
fn d_truth_km() -> f64 {
    3.0
}

impl Default for FusionCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            every_s: d_every(),
            settle_s: d_settle(),
            window_s: d_window(),
            alt_m: d_alt(),
            alt_sigma_m: d_alt_sigma(),
            area_km: d_area(),
            max_error_km: d_max_error(),
            truth_km: d_truth_km(),
            show_unmatched: false,
        }
    }
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
        }
    }
}

impl Default for Blah2Cfg {
    fn default() -> Self {
        Self {
            url: d_blah2_url(),
            poll_ms: d_poll_ms(),
        }
    }
}

impl Default for AdsbCfg {
    fn default() -> Self {
        Self {
            url: d_adsb_url(),
            poll_s: d_one(),
            max_age_s: d_max_age(),
        }
    }
}

impl Default for MatchCfg {
    fn default() -> Self {
        Self {
            range_gate_km: d_range_gate(),
            doppler_gate_hz: d_doppler_gate(),
        }
    }
}

impl Default for StatsCfg {
    fn default() -> Self {
        Self {
            bin_km: d_bin_km(),
            pd_threshold: d_threshold(),
            min_opportunities: d_min_opps(),
            detected_hits: d_detected_hits(),
            show_aircraft_ids: true,
        }
    }
}

impl Default for MeshCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            batch_s: d_batch_s(),
            max_kbps: d_one(),
            max_per_batch: d_max_per_batch(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, url) in [("blah2.url", &self.blah2.url), ("adsb.url", &self.adsb.url)] {
            if !url.starts_with("http://") {
                bail!("{name} must be an http:// URL, not {url:?}");
            }
        }
        if self.blah2.poll_ms < 100 {
            bail!("blah2.poll_ms must be at least 100");
        }
        if !(1..=60).contains(&self.adsb.max_age_s)
            || !(1..=self.adsb.max_age_s).contains(&self.adsb.poll_s)
        {
            bail!("adsb.max_age_s must be 1 to 60, and adsb.poll_s 1 to adsb.max_age_s");
        }
        let g = &self.gates;
        if !(g.range_gate_km > 0.0 && g.range_gate_km <= 20.0)
            || !(g.doppler_gate_hz > 0.0 && g.doppler_gate_hz <= 100.0)
        {
            bail!("match.range_gate_km must be 0 to 20 and match.doppler_gate_hz 0 to 100");
        }
        let s = &self.stats;
        if !(1..=100).contains(&s.bin_km) {
            bail!("stats.bin_km must be 1 to 100");
        }
        if !(s.pd_threshold > 0.0 && s.pd_threshold <= 1.0) {
            bail!("stats.pd_threshold must be above 0 and at most 1");
        }
        if s.min_opportunities < 1 || s.detected_hits < 1 {
            bail!("stats.min_opportunities and stats.detected_hits must be at least 1");
        }
        let m = &self.mesh;
        if !(10..=3600).contains(&m.batch_s)
            || !(1..=8).contains(&m.max_kbps)
            || !(1..=5000).contains(&m.max_per_batch)
        {
            bail!(
                "mesh.batch_s must be 10 to 3600, mesh.max_kbps 1 to 8 and mesh.max_per_batch \
                 1 to 5000"
            );
        }
        let t = &self.tracker;
        if !(1..=t.confirm_n).contains(&t.confirm_m) || !(1..=16).contains(&t.confirm_n) {
            bail!("tracker.confirm_m must be 1 to tracker.confirm_n, and confirm_n 1 to 16");
        }
        if !(1..=100).contains(&t.drop_after)
            || t.range_sigma_km <= 0.0
            || t.doppler_sigma_hz <= 0.0
        {
            bail!("tracker.drop_after must be 1 to 100, and the sigmas above 0");
        }
        let f = &self.fusion;
        if !(5..=600).contains(&f.every_s) || !(10..=300).contains(&f.window_s) {
            bail!("fusion.every_s must be 5 to 600 and fusion.window_s 10 to 300");
        }
        if f.settle_s <= self.mesh.batch_s || f.settle_s > 3600 {
            bail!(
                "fusion.settle_s must be longer than mesh.batch_s ({}) and at most 3600, or reports \
                 arrive after their moment has been worked out",
                self.mesh.batch_s
            );
        }
        if f.alt_sigma_m <= 0.0 || !(10.0..=500.0).contains(&f.area_km) {
            bail!("fusion.alt_sigma_m must be above 0 and fusion.area_km 10 to 500");
        }
        if f.max_error_km <= 0.0 || f.truth_km <= 0.0 {
            bail!("fusion.max_error_km and fusion.truth_km must be above 0");
        }
        Ok(())
    }

    pub fn hub(&self) -> crate::hub::Cfg {
        let (t, f) = (&self.tracker, &self.fusion);
        crate::hub::Cfg {
            track: crate::track::Params {
                range_sigma_km: t.range_sigma_km,
                doppler_sigma_hz: t.doppler_sigma_hz,
                confirm_m: t.confirm_m,
                confirm_n: t.confirm_n,
                drop_after: t.drop_after,
                ..Default::default()
            },
            fuse: crate::fuse::Cfg {
                alt_m: f.alt_m,
                alt_sigma_m: f.alt_sigma_m,
                range_sigma_km: t.range_sigma_km,
                doppler_sigma_hz: t.doppler_sigma_hz,
                area_km: f.area_km,
                max_chi2: 2.0,
                max_error_km: f.max_error_km,
            },
            history_ms: (f.window_s as i64 + 60).max(120) * 1000,
            window_ms: f.window_s as i64 * 1000,
            budget: std::time::Duration::from_millis(f.every_s * 500),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("radar.db")
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-radar configuration (experimental)
#
# The transmitter and receiver positions, illuminator frequency and CPI come from blah2's
# own config.yml (its location, capture and process sections), read from its API.
data_dir = "/var/lib/ridgewarden-radar"
listen = "0.0.0.0:8802"

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"

[blah2]
# blah2's API: network.ports.api in its config.yml
url = "http://127.0.0.1:3000"
poll_ms = 250

[adsb]
# aircraft.json from the ADS-B receiver. PiAware / dump1090-fa:
#   http://<piaware>/skyaware/data/aircraft.json
# readsb with tar1090:
#   http://<box>/tar1090/data/aircraft.json
url = "http://127.0.0.1:8080/data/aircraft.json"
poll_s = 1
max_age_s = 10

[match]
# a detection within both gates of where ADS-B puts an aircraft is that aircraft
range_gate_km = 2.0
doppler_gate_hz = 8.0

[stats]
bin_km = 5
pd_threshold = 0.8
min_opportunities = 30
detected_hits = 3
# false hides ICAO addresses and callsigns; the pages then number the aircraft instead
show_aircraft_ids = true

[mesh]
# detection reports (range, Doppler, SNR, time) as telemetry through the core. They
# average at most max_kbps, signed events included (1 is the spec's "kilobits"); past
# that, or past max_per_batch in one report, the weakest detections are left out
enabled = true
batch_s = 60
max_kbps = 1
max_per_batch = 1000

[tracker]
# runs on every site's reports: confirmed after 3 detections in 5 CPIs, ended after 6 misses
confirm_m = 3
confirm_n = 5
drop_after = 6
range_sigma_km = 0.3
doppler_sigma_hz = 1.0

[fusion]
# locate aircraft seen by two or more sites, without ADS-B
enabled = true
every_s = 10
# work out each moment this long after it, once every site's reports are in; keep it
# above mesh.batch_s plus the core's sync interval
settle_s = 300
window_s = 60
alt_m = 6000
alt_sigma_m = 3000
area_km = 100
max_error_km = 5
truth_km = 3
# track ellipses and fixes no ADS-B aircraft explains may follow aircraft public ADS-B
# doesn't show; they are counted, but only drawn on the map if this is true
show_unmatched = false
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen.port(), 8802);
    }

    #[test]
    fn bad_values_are_refused() {
        let c: Config = toml::from_str("[match]\nrange_gate_km = 0").unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str("[adsb]\nurl = \"https://x/aircraft.json\"").unwrap();
        assert!(c.validate().is_err());
        assert!(toml::from_str::<Config>("[site]\nlat = 1").is_err());
        let c: Config = toml::from_str("[fusion]\nsettle_s = 30").unwrap();
        assert!(c.validate().is_err(), "settle_s under batch_s");
        let c: Config = toml::from_str("[tracker]\nconfirm_m = 6").unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str("[mesh]\nmax_kbps = 9").unwrap();
        assert!(c.validate().is_err());
    }
}
