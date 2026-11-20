//! Putting every site's reports together: run each site's tracker over its recent CPIs,
//! locate aircraft seen by two or more sites, follow those fixes from one moment to the
//! next, and score them against ADS-B where it is available.

use std::time::{Duration, Instant};

use crate::adsb::Track as Aircraft;
use crate::blah2::{Frame, Setup};
use crate::fuse::{self, Fix, Obs, Plane, TrackObs};
use crate::geo::{C, add, ecef, horizontal_km, scale};
use crate::reports::SiteGeo;
use crate::store::{FixRow, Truth};
use crate::track::{Params, Tracker};

#[derive(Clone, Copy, Debug)]
pub struct Cfg {
    pub track: Params,
    pub fuse: fuse::Cfg,
    /// CPIs before the fix time each tracker sees, so tracks are confirmed by then
    pub history_ms: i64,
    /// the stretch of each track the position fit uses
    pub window_ms: i64,
    /// stop pairing tracks after this long, so a busy moment can't hold up the next
    pub budget: Duration,
}

/// One site's confirmed tracks at a moment, for the map.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteTracks {
    pub call: String,
    pub setup: Setup,
    /// track id, bistatic range km, Doppler Hz
    pub tracks: Vec<(u32, f64, f64)>,
}

/// One moment worked out from every site's reports: what each site was tracking, the
/// fixes, and the ADS-B aircraft then (None if the feed wasn't current).
#[derive(Clone, Debug)]
pub struct View {
    pub ts: i64,
    pub plane: Plane,
    pub sites: Vec<SiteTracks>,
    pub fixes: Vec<FixRow>,
    pub truth: Option<Vec<Aircraft>>,
}

/// The fixes `link` should look back on next time: this moment's, plus earlier ones
/// that no fix continued and that are still recent enough to be picked up.
pub fn carry(prev: Vec<(u64, Fix)>, now: Vec<(u64, Fix)>, t: i64) -> Vec<(u64, Fix)> {
    let mut out = now;
    for (id, f) in prev {
        if t - f.ts <= LINK_MS && !out.iter().any(|(i, _)| *i == id) {
            out.push((id, f));
        }
    }
    out
}

/// Score each fix against the ADS-B aircraft, if there were any to score against.
pub fn score(fix: &Fix, truth: Option<&[Aircraft]>, max_km: f64) -> Truth {
    let Some(aircraft) = truth else {
        return Truth::Unknown;
    };
    match nearest(fix, aircraft, max_km) {
        Some((i, d)) => Truth::Aircraft {
            hex: aircraft[i].hex.clone(),
            flight: aircraft[i].flight.clone(),
            err_km: d,
        },
        None => Truth::Nothing,
    }
}

/// A track needs this many detections in the fit window to be used.
const MIN_HITS: usize = 6;
/// and its last one this close to the fix time, so the fit is not extrapolating
const MAX_LAG_MS: i64 = 5_000;

/// Track every site over the CPIs in `frames` (one list per site, oldest first) up to
/// `t_ref`, and locate whatever two or more sites see together.
pub fn epoch(
    sites: &[SiteGeo],
    frames: &[Vec<Frame>],
    plane: Plane,
    cfg: &Cfg,
    t_ref: i64,
) -> (Vec<SiteTracks>, Vec<Fix>) {
    let deadline = Instant::now() + cfg.budget;
    let mut views = vec![];
    let mut tracks = vec![];
    for (i, (site, frames)) in sites.iter().zip(frames).enumerate() {
        let mut tr = Tracker::new(cfg.track, site.setup.fc);
        for f in frames.iter().filter(|f| f.ts <= t_ref) {
            tr.step(f);
        }
        let lambda = C / site.setup.fc;
        let recent =
            |t: &crate::track::Track| t.hits.last().is_some_and(|h| t_ref - h.ts <= MAX_LAG_MS);
        views.push(SiteTracks {
            call: site.call.clone(),
            setup: site.setup.clone(),
            tracks: tr
                .live()
                .iter()
                .filter(|t| t.confirmed && recent(t))
                .map(|t| (t.id, t.range_km(), t.doppler_hz(lambda)))
                .collect(),
        });
        let mut mine = vec![];
        for t in tr.confirmed().filter(|t| recent(t)) {
            let obs: Vec<Obs> = t
                .hits
                .iter()
                .filter(|h| t_ref - h.ts <= cfg.window_ms)
                .map(|h| Obs {
                    site: i,
                    ts: h.ts,
                    range_km: h.range_km,
                    doppler_hz: h.doppler_hz,
                })
                .collect();
            if obs.len() >= MIN_HITS {
                mine.push(TrackObs { site: i, obs });
            }
        }
        mine.sort_by_key(|t| std::cmp::Reverse(t.obs.len()));
        mine.truncate(fuse::MAX_TRACKS_PER_SITE);
        tracks.extend(mine);
    }
    let pairs: Vec<_> = sites.iter().map(|s| s.setup.pair()).collect();
    let fixes = fuse::associate(plane, &pairs, &tracks, t_ref, &cfg.fuse, deadline)
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    (views, fixes)
}

/// The ADS-B aircraft nearest a fix, if within `max_km` across the ground: its index and
/// the distance.
pub fn nearest(fix: &Fix, aircraft: &[Aircraft], max_km: f64) -> Option<(usize, f64)> {
    let p = ecef(fix.lat, fix.lon, fix.alt_m);
    aircraft
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let at = add(a.pos, scale(a.vel, (fix.ts - a.ts) as f64 / 1000.0));
            (i, horizontal_km(fix.lat, fix.lon, p, at))
        })
        .filter(|(_, d)| *d <= max_km)
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// How long a fused track waits for its next fix before it is given up.
pub const LINK_MS: i64 = 60_000;

/// Carry fused-track ids from earlier fixes to this moment's: a fix continues an earlier
/// one if it is where that one was heading. New tracks take ids from `next_id`.
pub fn link(prev: &[(u64, Fix)], fixes: &[Fix], next_id: &mut u64) -> Vec<u64> {
    let mut cands = vec![];
    for (i, f) in fixes.iter().enumerate() {
        let here = ecef(f.lat, f.lon, f.alt_m);
        for (k, (_, p)) in prev.iter().enumerate() {
            let dt = (f.ts - p.ts) as f64 / 1000.0;
            if !(0.0..=LINK_MS as f64 / 1000.0).contains(&dt) {
                continue;
            }
            let plane = Plane {
                lat0: p.lat,
                lon0: p.lon,
            };
            let (lat, lon) = plane.to_ll(p.ve * dt / 1000.0, p.vn * dt / 1000.0);
            let d = horizontal_km(f.lat, f.lon, here, ecef(lat, lon, p.alt_m));
            if d <= (3.0 * (p.major_km + f.major_km)).max(3.0) {
                cands.push((d, i, k));
            }
        }
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut ids = vec![None; fixes.len()];
    let mut taken = vec![false; prev.len()];
    for (_, i, k) in cands {
        if ids[i].is_none() && !taken[k] {
            ids[i] = Some(prev[k].0);
            taken[k] = true;
        }
    }
    ids.into_iter()
        .map(|id| {
            id.unwrap_or_else(|| {
                *next_id += 1;
                *next_id
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(ts: i64, lat: f64, lon: f64, ve: f64, vn: f64) -> Fix {
        Fix {
            ts,
            lat,
            lon,
            alt_m: 6000.0,
            alt_sigma_m: 3000.0,
            ve,
            vn,
            major_km: 0.3,
            minor_km: 0.2,
            angle_deg: 0.0,
            chi2: 1.0,
            sites: vec![0, 1],
        }
    }

    #[test]
    fn fixes_continue_the_track_they_follow() {
        let plane = Plane {
            lat0: 39.5,
            lon0: -104.8,
        };
        let (lat_a, lon_a) = plane.to_ll(0.0, 0.0);
        let (lat_b, lon_b) = plane.to_ll(20.0, 0.0);
        let prev = vec![
            (7, fix(0, lat_a, lon_a, 200.0, 0.0)),
            (9, fix(0, lat_b, lon_b, -200.0, 0.0)),
        ];
        // 10 s on, each has moved 2 km towards the other; a third is new
        let (la, oa) = plane.to_ll(2.0, 0.0);
        let (lb, ob) = plane.to_ll(18.0, 0.0);
        let (lc, oc) = plane.to_ll(0.0, 40.0);
        let now = vec![
            fix(10_000, lb, ob, -200.0, 0.0),
            fix(10_000, la, oa, 200.0, 0.0),
            fix(10_000, lc, oc, 0.0, 0.0),
        ];
        let mut next = 20;
        assert_eq!(link(&prev, &now, &mut next), [9, 7, 21]);
        assert_eq!(next, 21);
        // too long a gap starts afresh
        let late = vec![fix(90_000, la, oa, 200.0, 0.0)];
        assert_eq!(link(&prev, &late, &mut next), [22]);

        // a track missing for one moment is still there to pick up at the next
        let kept = carry(prev.clone(), vec![(7, now[1].clone())], 10_000);
        assert_eq!(kept.len(), 2, "9 waits for another fix");
        let (lb2, ob2) = plane.to_ll(16.0, 0.0);
        let mut next = 30;
        assert_eq!(
            link(&kept, &[fix(20_000, lb2, ob2, -200.0, 0.0)], &mut next),
            [9]
        );
        assert!(
            carry(prev, vec![], 70_000).is_empty(),
            "given up after a minute"
        );
    }

    #[test]
    fn nearest_moves_the_aircraft_to_the_fix_time() {
        let plane = Plane {
            lat0: 39.5,
            lon0: -104.8,
        };
        let (lat, lon) = plane.to_ll(10.0, 0.0);
        let f = fix(20_000, lat, lon, 0.0, 0.0);
        // reported 20 s earlier 4 km to the west, flying east at 200 m/s, and 3 km lower
        let (la, lo) = plane.to_ll(6.0, 0.0);
        let a = Aircraft {
            hex: "a00001".into(),
            flight: String::new(),
            ts: 0,
            lat: la,
            lon: lo,
            pos: ecef(la, lo, 3000.0),
            vel: crate::geo::enu_to_ecef(la, lo, [200.0, 0.0, 0.0]),
        };
        let (i, d) = nearest(&f, std::slice::from_ref(&a), 5.0).unwrap();
        assert_eq!(i, 0);
        assert!(d < 0.1, "{d}");
        assert!(nearest(&fix(0, lat, lon, 0.0, 0.0), &[a], 3.0).is_none());
    }
}
