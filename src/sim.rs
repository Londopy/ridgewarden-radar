//! The `simulate` subcommand: a stand-in for blah2 and an ADS-B receiver, so the whole
//! app runs without radio hardware. Synthetic aircraft fly straight lines around the
//! given sites, each a transmitter and receiver pair. Each CPI each site detects an
//! aircraft with a probability that falls off with its bistatic range, with some range and
//! Doppler noise; there are random false alarms, and a few aircraft carry no ADS-B. Every
//! site gets its own blah2 API (/api/detection, /api/config, /api/timing, /api/timestamp),
//! and each also serves dump1090's /data/aircraft.json for the same aircraft.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::{Value, json};

use crate::blah2::{Setup, parse_config};
use crate::geo::{FT, KT, Pair, ecef, velocity};

/// xorshift64*: small and seedable, enough for simulated noise.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn between(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    /// Standard normal (Box-Muller).
    pub fn gauss(&mut self) -> f64 {
        let u = 1.0 - self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }

    /// Poisson-distributed count (Knuth's method; fine for small means).
    pub fn poisson(&mut self, mean: f64) -> usize {
        let limit = (-mean).exp();
        let mut p = self.unit();
        let mut n = 0;
        while p > limit {
            p *= self.unit();
            n += 1;
        }
        n
    }
}

/// "lat,lon,alt_m"
pub fn parse_point(s: &str) -> Result<[f64; 3]> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("{s:?} is not lat,lon,alt"))?;
    match v[..] {
        [lat, lon, alt] if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) => {
            Ok([lat, lon, alt])
        }
        _ => bail!("{s:?} is not lat,lon,alt with a valid latitude and longitude"),
    }
}

/// One simulated blah2: a receiver, the transmitter it listens to, and its frequency.
#[derive(Clone, Copy, Debug)]
pub struct SitePlan {
    pub rx: [f64; 3],
    pub tx: [f64; 3],
    pub fc_hz: f64,
}

pub struct Plan {
    pub sites: Vec<SitePlan>,
    pub cpi_s: f64,
    pub aircraft: usize,
    /// aircraft with no ADS-B: detected, but never in aircraft.json
    pub dark: usize,
    pub false_per_cpi: f64,
    /// bistatic range (km) where detection probability is about half its best
    pub r50_km: f64,
    /// aircraft fly within about this far of the receivers' middle
    pub radius_km: f64,
}

impl Plan {
    /// What site `i`'s blah2 would serve as /api/config, with its default processing.
    pub fn config_json(&self, i: usize) -> Value {
        let s = &self.sites[i];
        let n = i + 1;
        let site = |p: [f64; 3], name: String| json!({"latitude": p[0], "longitude": p[1], "altitude": p[2], "name": name});
        json!({
            "capture": {"fs": 2_000_000, "fc": s.fc_hz.round() as u64, "device": {"type": "Simulator"}},
            "process": {
                "data": {"cpi": self.cpi_s, "buffer": 2, "overlap": 0},
                "ambiguity": {"delayMin": -10, "delayMax": 400, "dopplerMin": -200, "dopplerMax": 200},
                "clutter": {"enable": true, "delayMin": -10, "delayMax": 400},
                "detection": {"enable": true, "pfa": 0.00001, "nGuard": 2, "nTrain": 6,
                              "minDelay": 5, "minDoppler": 15, "nCentroid": 6}
            },
            "network": {"ip": "0.0.0.0", "ports": {"api": 3000}},
            "location": {
                "rx": site(s.rx, format!("Simulated receiver {n}")),
                "tx": site(s.tx, format!("Simulated transmitter {n}"))
            }
        })
    }
}

struct Plane {
    hex: String,
    flight: String,
    lat: f64,
    lon: f64,
    alt_m: f64,
    gs_kt: f64,
    track: f64,
    vrate_fpm: f64,
    dark: bool,
    messages: u64,
}

const M_PER_DEG: f64 = 111_320.0;

struct SiteSim {
    setup: Setup,
    pair: Pair,
    detection: String,
}

pub struct Sim {
    plan: Plan,
    sites: Vec<SiteSim>,
    /// the middle of the receivers, where the aircraft fly
    centre: (f64, f64),
    planes: Vec<Plane>,
    rng: Rng,
    serial: u32,
    start: i64,
    /// time of the last CPI
    t: i64,
    n_cpi: i64,
    step_ms: f64,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

impl Sim {
    pub fn new(plan: Plan, seed: u64, now: i64) -> Result<Self> {
        if plan.sites.is_empty() {
            bail!("the simulator needs at least one site");
        }
        let sites = (0..plan.sites.len())
            .map(|i| {
                let setup = parse_config(&plan.config_json(i).to_string())?;
                Ok(SiteSim {
                    pair: setup.pair(),
                    setup,
                    detection: String::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let n = plan.sites.len() as f64;
        let centre = (
            plan.sites.iter().map(|s| s.rx[0]).sum::<f64>() / n,
            plan.sites.iter().map(|s| s.rx[1]).sum::<f64>() / n,
        );
        let mut sim = Self {
            plan,
            sites,
            centre,
            planes: vec![],
            rng: Rng::new(seed),
            serial: 0,
            start: now,
            t: now,
            n_cpi: 0,
            step_ms: 0.0,
        };
        for i in 0..sim.plan.aircraft + sim.plan.dark {
            let p = sim.spawn(true, i >= sim.plan.aircraft);
            sim.planes.push(p);
        }
        Ok(sim)
    }

    /// A new aircraft anywhere in the area, or else on its edge heading roughly inwards.
    fn spawn(&mut self, anywhere: bool, dark: bool) -> Plane {
        let radius = self.plan.radius_km * 1000.0;
        let r = &mut self.rng;
        let (dist, bearing) = if anywhere {
            (radius * r.unit().sqrt(), r.between(0.0, 360.0))
        } else {
            (radius * 1.25, r.between(0.0, 360.0))
        };
        let track = if anywhere {
            r.between(0.0, 360.0)
        } else {
            (bearing + 180.0 + r.between(-50.0, 50.0)).rem_euclid(360.0)
        };
        let (lat0, lon0) = self.centre;
        let (s, c) = bearing.to_radians().sin_cos();
        let lat = lat0 + dist * c / M_PER_DEG;
        let lon = lon0 + dist * s / (M_PER_DEG * lat0.to_radians().cos());
        let vrate_fpm = if r.unit() < 0.2 {
            r.between(-1500.0, 1500.0)
        } else {
            0.0
        };
        let p = Plane {
            hex: format!("~f{:05x}", self.serial),
            flight: format!("{:<8}", format!("TST{}", 100 + self.serial)),
            lat,
            lon,
            alt_m: r.between(1500.0, 11_500.0),
            gs_kt: r.between(200.0, 480.0),
            track,
            vrate_fpm,
            dark,
            messages: 0,
        };
        self.serial += 1;
        p
    }

    fn fly(&mut self, dt: f64) {
        let (lat0, lon0) = self.centre;
        for i in 0..self.planes.len() {
            let p = &mut self.planes[i];
            let v = p.gs_kt * KT * dt;
            let (s, c) = p.track.to_radians().sin_cos();
            p.lat += v * c / M_PER_DEG;
            p.lon += v * s / (M_PER_DEG * p.lat.to_radians().cos());
            p.alt_m += p.vrate_fpm * FT / 60.0 * dt;
            if !(300.0..=12_500.0).contains(&p.alt_m) {
                p.alt_m = p.alt_m.clamp(300.0, 12_500.0);
                p.vrate_fpm = 0.0;
            }
            p.messages += (dt * 6.0) as u64;
            let dn = (p.lat - lat0) * M_PER_DEG;
            let de = (p.lon - lon0) * M_PER_DEG * lat0.to_radians().cos();
            if dn.hypot(de) > self.plan.radius_km * 1500.0 {
                let dark = p.dark;
                self.planes[i] = self.spawn(false, dark);
            }
        }
    }

    fn pd(&self, range_km: f64) -> f64 {
        0.95 / (1.0 + (range_km / self.plan.r50_km).powi(4))
    }

    fn detect(&mut self, site: usize) -> String {
        let w = self.sites[site].setup.window;
        let pair = self.sites[site].pair;
        let mut dets = vec![];
        for i in 0..self.planes.len() {
            let p = &self.planes[i];
            let pos = ecef(p.lat, p.lon, p.alt_m);
            let vel = velocity(p.lat, p.lon, p.gs_kt, p.track, p.vrate_fpm);
            let (r, f) = (pair.range(pos) / 1000.0, pair.doppler(pos, vel));
            if w.contains(r, f) && self.rng.unit() < self.pd(r) {
                let snr = (28.0 - 14.0 * r / self.plan.r50_km).max(9.0) + self.rng.gauss();
                let noisy_r = r + 0.25 * self.rng.gauss();
                let noisy_f = f + 0.6 * self.rng.gauss();
                dets.push((noisy_r, noisy_f, snr));
            }
        }
        for _ in 0..self.rng.poisson(self.plan.false_per_cpi) {
            let f = self.rng.between(w.dop_min, w.dop_max);
            if f.abs() >= w.notch_hz {
                let r = self.rng.between(w.min_km, w.max_km);
                dets.push((r, f, self.rng.between(9.0, 13.0)));
            }
        }
        json!({
            "timestamp": self.t,
            "delay": dets.iter().map(|d| round2(d.0)).collect::<Vec<_>>(),
            "doppler": dets.iter().map(|d| round2(d.1)).collect::<Vec<_>>(),
            "snr": dets.iter().map(|d| round2(d.2)).collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// Move everything on to `now` and produce that CPI's detections at every site.
    pub fn step(&mut self, now: i64) {
        let started = Instant::now();
        self.fly((now - self.t) as f64 / 1000.0);
        self.t = now;
        self.n_cpi += 1;
        for i in 0..self.sites.len() {
            self.sites[i].detection = self.detect(i);
        }
        self.step_ms = started.elapsed().as_secs_f64() * 1000.0;
    }

    pub fn detection_json(&self, site: usize) -> &str {
        &self.sites[site].detection
    }

    pub fn config_json(&self, site: usize) -> String {
        self.plan.config_json(site).to_string()
    }

    /// dump1090's aircraft.json, positions as of the last CPI.
    pub fn aircraft_json(&self, now: i64) -> String {
        self.aircraft_list(now, false)
    }

    /// `with_dark` adds the aircraft that have no ADS-B, for checking fixes against
    /// everything that is really there.
    fn aircraft_list(&self, now: i64, with_dark: bool) -> String {
        let age = ((now - self.t) as f64 / 1000.0).max(0.0);
        let list: Vec<Value> = self
            .planes
            .iter()
            .filter(|p| with_dark || !p.dark)
            .map(|p| {
                let ft = (p.alt_m / FT).round() as i64;
                json!({
                    "hex": p.hex, "type": "adsb_other", "flight": p.flight,
                    "alt_baro": ft, "alt_geom": ft, "gs": (p.gs_kt * 10.0).round() / 10.0,
                    "track": (p.track * 10.0).round() / 10.0,
                    "baro_rate": p.vrate_fpm.round() as i64, "geom_rate": p.vrate_fpm.round() as i64,
                    "lat": (p.lat * 1e6).round() / 1e6, "lon": (p.lon * 1e6).round() / 1e6,
                    "nic": 8, "rc": 186, "seen_pos": age, "version": 2,
                    "mlat": [], "tisb": [], "messages": p.messages, "seen": age, "rssi": -18.0,
                })
            })
            .collect();
        let messages: u64 = self.planes.iter().map(|p| p.messages).sum();
        json!({"now": now as f64 / 1000.0, "messages": messages, "aircraft": list}).to_string()
    }

    pub fn timing_json(&self, now: i64) -> String {
        let up = (now - self.start) as f64 / 1000.0;
        json!({
            "timestamp": self.t, "nCpi": self.n_cpi, "uptime_s": up,
            "uptime_days": up / 86_400.0, "cpi": round2(self.step_ms),
        })
        .to_string()
    }
}

type Shared = Arc<Mutex<Sim>>;

fn json_body(s: String) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], s)
}

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Sim> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// Site `i`'s blah2 API, plus the shared aircraft.json.
fn router(sim: Shared, i: usize) -> Router {
    let now = ridgewarden_client::util::now_ms;
    Router::new()
        .route(
            "/api/detection",
            get(move |State(s): State<Shared>| async move {
                json_body(lock(&s).detection_json(i).to_string())
            }),
        )
        .route(
            "/api/config",
            get(move |State(s): State<Shared>| async move { json_body(lock(&s).config_json(i)) }),
        )
        .route(
            "/api/timing",
            get(move |State(s): State<Shared>| async move {
                json_body(lock(&s).timing_json(now()))
            }),
        )
        .route(
            "/api/timestamp",
            get(|State(s): State<Shared>| async move { lock(&s).t.to_string() }),
        )
        .route(
            "/data/aircraft.json",
            get(move |State(s): State<Shared>| async move {
                json_body(lock(&s).aircraft_json(now()))
            }),
        )
        .with_state(sim)
}

/// Run the simulator until interrupted, site `i` on `listen[i]`.
pub async fn serve(plan: Plan, seed: u64, listen: &[SocketAddr]) -> Result<()> {
    if listen.len() != plan.sites.len() {
        bail!(
            "{} listen addresses for {} sites",
            listen.len(),
            plan.sites.len()
        );
    }
    let cpi = Duration::from_secs_f64(plan.cpi_s);
    let sim: Shared = Arc::new(Mutex::new(Sim::new(
        plan,
        seed,
        ridgewarden_client::util::now_ms(),
    )?));
    let s = sim.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cpi);
        loop {
            tick.tick().await;
            lock(&s).step(ridgewarden_client::util::now_ms());
        }
    });
    let mut servers = tokio::task::JoinSet::new();
    for (i, addr) in listen.iter().enumerate() {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        tracing::info!(
            "simulated blah2 {} on http://{addr}/api/ and aircraft.json on http://{addr}/data/aircraft.json",
            i + 1
        );
        let app = router(sim.clone(), i);
        servers.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
        });
    }
    while let Some(r) = servers.join_next().await {
        r??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::{self, Plane as Grid};
    use crate::hub;
    use crate::reports::SiteGeo;
    use crate::track::Params;
    use crate::truth::{Gates, match_frame};
    use crate::{adsb, blah2};

    const TX: [f64; 3] = [39.62, -104.95, 2100.0];

    fn plan(sites: Vec<SitePlan>, dark: usize) -> Plan {
        Plan {
            sites,
            cpi_s: 0.5,
            aircraft: 12,
            dark,
            false_per_cpi: 0.5,
            r50_km: 35.0,
            radius_km: 60.0,
        }
    }

    fn one_site() -> Vec<SitePlan> {
        vec![SitePlan {
            rx: [39.50, -104.80, 1650.0],
            tx: TX,
            fc_hz: 96.9e6,
        }]
    }

    fn three_sites() -> Vec<SitePlan> {
        vec![
            one_site()[0],
            SitePlan {
                rx: [39.30, -104.55, 1800.0],
                tx: [39.45, -104.40, 1700.0],
                fc_hz: 99.5e6,
            },
            SitePlan {
                rx: [39.75, -104.70, 1600.0],
                tx: TX,
                fc_hz: 96.9e6,
            },
        ]
    }

    #[test]
    fn points_parse() {
        assert_eq!(
            parse_point("39.5, -104.8,1650").unwrap(),
            [39.5, -104.8, 1650.0]
        );
        assert!(parse_point("39.5,-104.8").is_err());
        assert!(parse_point("99,0,0").is_err());
    }

    #[test]
    fn rng_is_repeatable_and_poisson_has_the_right_mean() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(a.unit(), b.unit());
        let n: usize = (0..20_000).map(|_| a.poisson(0.5)).sum();
        let mean = n as f64 / 20_000.0;
        assert!((mean - 0.5).abs() < 0.03, "{mean}");
    }

    /// The simulator's output goes through the same parsers and matcher as blah2's and
    /// dump1090's would, and the statistics come out the way the model was set up.
    #[test]
    fn simulated_feeds_run_through_the_pipeline() {
        let t0 = 1_795_000_000_000;
        let mut sim = Sim::new(plan(one_site(), 2), 42, t0).unwrap();
        let setup = blah2::parse_config(&sim.config_json(0)).unwrap();
        assert_eq!(setup.device, "Simulator");
        let gates = Gates {
            range_km: 2.0,
            doppler_hz: 8.0,
            max_age_ms: 10_000,
        };
        let (mut near, mut near_hits, mut opps, mut hits, mut dets, mut unmatched) =
            (0, 0, 0, 0, 0, 0);
        for i in 1..=1200 {
            let now = t0 + i * 500;
            sim.step(now);
            let frame = blah2::parse_detection(sim.detection_json(0))
                .unwrap()
                .unwrap();
            assert_eq!(frame.ts, now);
            let tracks = adsb::parse(&sim.aircraft_json(now + 300), 10.0)
                .unwrap()
                .tracks;
            assert_eq!(tracks.len(), 12, "dark aircraft are not in aircraft.json");
            let o = match_frame(&setup.pair(), &setup.window, &gates, &tracks, &frame);
            for a in o.aircraft.iter().filter(|a| a.counted) {
                opps += 1;
                hits += a.hit.is_some() as i32;
                if a.range_km < 15.0 {
                    near += 1;
                    near_hits += a.hit.is_some() as i32;
                }
            }
            dets += frame.dets.len();
            unmatched += o.unmatched();
        }
        let pd = hits as f64 / opps as f64;
        let pd_near = near_hits as f64 / near as f64;
        assert!(opps > 2000, "{opps} opportunities");
        assert!(near > 100, "{near} near opportunities");
        assert!(pd_near > 0.85, "near Pd {pd_near}");
        assert!(pd < pd_near, "Pd falls off with range: {pd} vs {pd_near}");
        // false alarms (about 0.5 per CPI less the notch) plus the two dark aircraft
        let per_cpi = unmatched as f64 / 1200.0;
        assert!((0.3..2.5).contains(&per_cpi), "{per_cpi} unmatched per CPI");
        assert!(dets > unmatched);
        assert!(!sim.timing_json(t0).is_empty());
    }

    /// Three simulated sites, through the per-site trackers and the position fit, end
    /// up where the simulated aircraft are, without using ADS-B to find them.
    #[test]
    fn three_sites_locate_aircraft_without_ads_b() {
        let t0 = 1_795_000_000_000;
        let mut p = plan(three_sites(), 2);
        p.aircraft = 30;
        p.radius_km = 30.0;
        let mut sim = Sim::new(p, 9, t0).unwrap();
        let sites: Vec<SiteGeo> = (0..3)
            .map(|i| SiteGeo {
                key: format!("k{i}"),
                call: format!("N0SIM{i}"),
                since: 0,
                setup: blah2::parse_config(&sim.config_json(i)).unwrap(),
            })
            .collect();
        let mut frames = vec![vec![]; 3];
        let steps = 240;
        for k in 1..=steps {
            let now = t0 + k * 500;
            sim.step(now);
            for (i, f) in frames.iter_mut().enumerate() {
                f.push(
                    blah2::parse_detection(sim.detection_json(i))
                        .unwrap()
                        .unwrap(),
                );
            }
        }
        let t_ref = t0 + steps * 500;
        let cfg = hub::Cfg {
            track: Params::default(),
            fuse: fuse::Cfg {
                alt_m: 6000.0,
                alt_sigma_m: 3000.0,
                range_sigma_km: 0.3,
                doppler_sigma_hz: 1.0,
                area_km: 90.0,
                max_chi2: 2.0,
                max_error_km: 5.0,
            },
            history_ms: 120_000,
            window_ms: 60_000,
            budget: std::time::Duration::from_secs(600),
        };
        let grid = Grid {
            lat0: 39.5,
            lon0: -104.7,
        };
        let (views, fixes) = hub::epoch(&sites, &frames, grid, &cfg, t_ref);
        assert_eq!(views.len(), 3);
        assert!(views.iter().all(|v| !v.tracks.is_empty()));
        let truth = adsb::parse(&sim.aircraft_list(t_ref, true), 10.0)
            .unwrap()
            .tracks;
        assert!(fixes.len() >= 5, "{} fixes", fixes.len());
        // most fixes are real aircraft with the truth inside three sigma and a kilometre
        // (the fit assumes level flight); the rest are ghosts, two sites' tracks of
        // different aircraft that happen to fit together
        let inside = fixes
            .iter()
            .filter(|f| hub::nearest(f, &truth, 3.0 * f.major_km + 1.0).is_some())
            .count();
        assert!(
            inside * 10 >= fixes.len() * 6,
            "{inside} of {} fixes have an aircraft inside 3 sigma",
            fixes.len()
        );
        // three sites pin it down to well under a kilometre or two
        let three: Vec<_> = fixes.iter().filter(|f| f.sites.len() >= 3).collect();
        assert!(!three.is_empty());
        let good = three
            .iter()
            .filter(|f| hub::nearest(f, &truth, 1.5).is_some())
            .count();
        assert!(
            good * 2 > three.len(),
            "{good} of {} three-site fixes",
            three.len()
        );
    }
}
