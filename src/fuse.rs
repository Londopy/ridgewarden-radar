//! Locating aircraft from several sites without ADS-B. Each site's track gives a bistatic
//! range (an ellipse around its transmitter and receiver) and a range rate. Tracks from two
//! or more sites that belong to the same aircraft meet at one point: a least-squares fit of
//! position and velocity over a few seconds of their detections finds it, with an error
//! ellipse from the fit's covariance. The height is solved too, starting from a typical
//! cruising height with a wide allowance either side: two sites barely constrain it, so
//! their error ellipses come out long, while three or more pin it down.

use std::time::Instant;

use crate::geo::{Pair, dot, ecef, enu_to_ecef, norm, sub};

const KM_PER_DEG: f64 = 111.32;
/// m/s; faster than any airliner's ground speed, so a fit needing more is a ghost
const MAX_SPEED: f64 = 350.0;
/// how much worse than `max_chi2` a two-site fit may be and still be worth offering to a
/// third site
const SEED_SLACK: f64 = 25.0;
/// detections used from each track in a fit; more cost time and add little
const PER_TRACK: usize = 16;
/// two fits this far apart whose costs differ by less than `AMBIGUOUS_COST` (a
/// chi-square difference, so about two sigma) are both possible
const AMBIGUOUS_KM: f64 = 2.0;
const AMBIGUOUS_COST: f64 = 4.0;
/// tracks per site offered to `associate`, most detections first; the work grows with
/// the cube of the number of tracks
pub const MAX_TRACKS_PER_SITE: usize = 20;

/// A flat east/north grid in km around an origin, used only to parameterise positions;
/// all geometry is worked out in ECEF.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub lat0: f64,
    pub lon0: f64,
}

impl Plane {
    pub fn to_ll(&self, e: f64, n: f64) -> (f64, f64) {
        (
            self.lat0 + n / KM_PER_DEG,
            self.lon0 + e / (KM_PER_DEG * self.lat0.to_radians().cos()),
        )
    }

    pub fn to_en(&self, lat: f64, lon: f64) -> (f64, f64) {
        (
            (lon - self.lon0) * KM_PER_DEG * self.lat0.to_radians().cos(),
            (lat - self.lat0) * KM_PER_DEG,
        )
    }
}

/// One detection from one site, as it joined a track there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Obs {
    pub site: usize,
    pub ts: i64,
    pub range_km: f64,
    pub doppler_hz: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Cfg {
    /// typical aircraft height, and how far either side of it they fly (1 sigma)
    pub alt_m: f64,
    pub alt_sigma_m: f64,
    pub range_sigma_km: f64,
    pub doppler_sigma_hz: f64,
    /// search this far from the plane's origin, km
    pub area_km: f64,
    /// fits worse than this (chi-square per degree of freedom) are not believed
    pub max_chi2: f64,
    /// nor fits whose 1-sigma error ellipse is longer than this, km
    pub max_error_km: f64,
}

/// A position estimate at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    pub ts: i64,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f64,
    /// how well the height is known (1 sigma); near `Cfg::alt_sigma_m` means barely at all
    pub alt_sigma_m: f64,
    /// east and north velocity, m/s
    pub ve: f64,
    pub vn: f64,
    /// 1-sigma error ellipse: semi-axes in km, major axis in degrees anticlockwise from east
    pub major_km: f64,
    pub minor_km: f64,
    pub angle_deg: f64,
    /// chi-square per degree of freedom of the fit
    pub chi2: f64,
    /// the sites that saw it
    pub sites: Vec<usize>,
}

struct Problem<'a> {
    plane: Plane,
    pairs: &'a [Pair],
    obs: &'a [Obs],
    t_ref: i64,
    cfg: Cfg,
}

/// Parameters: east and north km at `t_ref`, east and north velocity m/s, and height km.
impl Problem<'_> {
    fn residuals(&self, p: &[f64]) -> Vec<f64> {
        let alt = p[4] * 1000.0;
        let mut r = Vec::with_capacity(self.obs.len() * 2 + 1);
        r.push((alt - self.cfg.alt_m) / self.cfg.alt_sigma_m);
        for o in self.obs {
            let dt = (o.ts - self.t_ref) as f64 / 1000.0;
            let (lat, lon) = self
                .plane
                .to_ll(p[0] + p[2] * dt / 1000.0, p[1] + p[3] * dt / 1000.0);
            let pos = ecef(lat, lon, alt);
            let vel = enu_to_ecef(lat, lon, [p[2], p[3], 0.0]);
            let pair = &self.pairs[o.site];
            r.push((o.range_km - pair.range(pos) / 1000.0) / self.cfg.range_sigma_km);
            r.push((o.doppler_hz - pair.doppler(pos, vel)) / self.cfg.doppler_sigma_hz);
        }
        r
    }

    fn cost(&self, p: &[f64]) -> f64 {
        self.residuals(p).iter().map(|x| x * x).sum()
    }

    fn jacobian(&self, p: &[f64]) -> Vec<Vec<f64>> {
        const STEP: [f64; 5] = [0.01, 0.01, 0.1, 0.1, 0.01];
        let cols: Vec<Vec<f64>> = (0..p.len())
            .map(|k| {
                let mut hi = p.to_vec();
                let mut lo = p.to_vec();
                hi[k] += STEP[k];
                lo[k] -= STEP[k];
                let (a, b) = (self.residuals(&hi), self.residuals(&lo));
                a.iter()
                    .zip(&b)
                    .map(|(x, y)| (x - y) / (2.0 * STEP[k]))
                    .collect()
            })
            .collect();
        (0..cols[0].len())
            .map(|i| cols.iter().map(|c| c[i]).collect())
            .collect()
    }
}

/// Solve `a x = b` by Gaussian elimination; None if `a` is singular.
fn solve_lin(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for c in 0..n {
        let piv = (c..n).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        if a[piv][c].abs() < 1e-12 {
            return None;
        }
        a.swap(c, piv);
        b.swap(c, piv);
        let pivot = a[c].clone();
        for r in c + 1..n {
            let f = a[r][c] / pivot[c];
            for (x, p) in a[r].iter_mut().zip(&pivot).skip(c) {
                *x -= f * p;
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = vec![0.0; n];
    for c in (0..n).rev() {
        let s: f64 = (c + 1..n).map(|k| a[c][k] * x[k]).sum();
        x[c] = (b[c] - s) / a[c][c];
    }
    Some(x)
}

fn normal(j: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = j[0].len();
    (0..n)
        .map(|a| {
            (0..n)
                .map(|b| j.iter().map(|row| row[a] * row[b]).sum())
                .collect()
        })
        .collect()
}

/// Levenberg-Marquardt from `p`; returns the parameters and their cost.
fn fit(prob: &Problem, mut p: Vec<f64>) -> (Vec<f64>, f64) {
    let mut cost = prob.cost(&p);
    let mut mu = 1e-3;
    for _ in 0..40 {
        let j = prob.jacobian(&p);
        let r = prob.residuals(&p);
        let jtj = normal(&j);
        let jtr: Vec<f64> = (0..p.len())
            .map(|k| -j.iter().zip(&r).map(|(row, ri)| row[k] * ri).sum::<f64>())
            .collect();
        let mut improved = false;
        while mu < 1e9 {
            let mut a = jtj.clone();
            for (k, row) in a.iter_mut().enumerate() {
                row[k] += mu * (jtj[k][k] + 1e-9);
            }
            let Some(step) = solve_lin(a, jtr.clone()) else {
                mu *= 10.0;
                continue;
            };
            let q: Vec<f64> = p.iter().zip(&step).map(|(x, d)| x + d).collect();
            let c = prob.cost(&q);
            if c < cost {
                let done = cost - c < 1e-6 * cost.max(1e-9);
                p = q;
                cost = c;
                mu = (mu / 3.0).max(1e-12);
                improved = !done;
                break;
            }
            mu *= 4.0;
        }
        if !improved {
            break;
        }
    }
    (p, cost)
}

/// Positions worth starting the fit from: the best local minima of the range misfit on a
/// grid, using each site's detection nearest `t_ref`.
fn starts(prob: &Problem) -> Vec<(f64, f64)> {
    let mut latest: Vec<&Obs> = vec![];
    for o in prob.obs {
        match latest.iter_mut().find(|l| l.site == o.site) {
            Some(l) if (o.ts - prob.t_ref).abs() < (l.ts - prob.t_ref).abs() => *l = o,
            Some(_) => {}
            None => latest.push(o),
        }
    }
    let step = 3.0;
    let n = (2.0 * prob.cfg.area_km / step) as usize + 1;
    let at = |i: usize| -prob.cfg.area_km + i as f64 * step;
    let alt = prob.cfg.alt_m;
    let mut grid = vec![0.0; n * n];
    for i in 0..n {
        for k in 0..n {
            let (lat, lon) = prob.plane.to_ll(at(i), at(k));
            let pos = ecef(lat, lon, alt);
            grid[i * n + k] = latest
                .iter()
                .map(|o| ((o.range_km - prob.pairs[o.site].range(pos) / 1000.0) / step).powi(2))
                .sum();
        }
    }
    let mut minima = vec![];
    for i in 1..n - 1 {
        for k in 1..n - 1 {
            let c = grid[i * n + k];
            let lowest = (i - 1..=i + 1)
                .flat_map(|a| (k - 1..=k + 1).map(move |b| (a, b)))
                .all(|(a, b)| grid[a * n + b] >= c);
            if lowest {
                minima.push((c, at(i), at(k)));
            }
        }
    }
    minima.sort_by(|a, b| a.0.total_cmp(&b.0));
    minima.into_iter().take(4).map(|m| (m.1, m.2)).collect()
}

/// Velocity that best explains each site's latest Doppler at position `(e, n)`.
fn start_velocity(prob: &Problem, e: f64, n: f64) -> (f64, f64) {
    let (lat, lon) = prob.plane.to_ll(e, n);
    let pos = ecef(lat, lon, prob.cfg.alt_m);
    let east = enu_to_ecef(lat, lon, [1.0, 0.0, 0.0]);
    let north = enu_to_ecef(lat, lon, [0.0, 1.0, 0.0]);
    let mut ata = vec![vec![0.0; 2]; 2];
    let mut atb = vec![0.0; 2];
    for o in prob.obs {
        let pair = &prob.pairs[o.site];
        let (to_tx, to_rx) = (sub(pos, pair.tx), sub(pos, pair.rx));
        let g: Vec<f64> = (0..3)
            .map(|i| to_tx[i] / norm(to_tx) + to_rx[i] / norm(to_rx))
            .collect();
        let g = [g[0], g[1], g[2]];
        let row = [-dot(g, east) / pair.lambda, -dot(g, north) / pair.lambda];
        for a in 0..2 {
            for b in 0..2 {
                ata[a][b] += row[a] * row[b];
            }
            atb[a] += row[a] * o.doppler_hz;
        }
    }
    solve_lin(ata, atb).map_or((0.0, 0.0), |v| (v[0], v[1]))
}

fn invert(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let cols: Option<Vec<Vec<f64>>> = (0..n)
        .map(|c| solve_lin(a.to_vec(), (0..n).map(|i| (i == c) as u8 as f64).collect()))
        .collect();
    let cols = cols?;
    Some(
        (0..n)
            .map(|i| cols.iter().map(|c| c[i]).collect())
            .collect(),
    )
}

/// The position of the aircraft behind `obs` at `t_ref`, if the detections fit one.
pub fn solve(plane: Plane, pairs: &[Pair], obs: &[Obs], t_ref: i64, cfg: &Cfg) -> Option<Fix> {
    best_fit(plane, pairs, obs, t_ref, cfg, true).filter(|f| good(f, cfg))
}

fn good(f: &Fix, cfg: &Cfg) -> bool {
    f.chi2 <= cfg.max_chi2 && f.major_km <= cfg.max_error_km
}

/// The best fit to `obs`, however poor, as long as it is a possible aircraft. With
/// `unique`, also only if no other place fits about as well.
fn best_fit(
    plane: Plane,
    pairs: &[Pair],
    obs: &[Obs],
    t_ref: i64,
    cfg: &Cfg,
    unique: bool,
) -> Option<Fix> {
    let mut sites: Vec<usize> = obs.iter().map(|o| o.site).collect();
    sites.sort_unstable();
    sites.dedup();
    if sites.len() < 2 {
        return None;
    }
    let prob = Problem {
        plane,
        pairs,
        obs,
        t_ref,
        cfg: *cfg,
    };
    // the height prior is one more measurement, and the height one more parameter
    let dof = (2 * obs.len()).checked_sub(4).filter(|d| *d > 0)?;
    let mut fits: Vec<(Vec<f64>, f64)> = starts(&prob)
        .into_iter()
        .map(|(e, n)| {
            let (ve, vn) = start_velocity(&prob, e, n);
            fit(&prob, vec![e, n, ve, vn, cfg.alt_m / 1000.0])
        })
        .collect();
    fits.sort_by(|a, b| a.1.total_cmp(&b.1));
    let (p, cost) = fits.first()?.clone();
    // two ellipses usually cross twice; when both crossings fit about as well, the data
    // can't say which one the aircraft is at, so there is no fix
    let rival = fits[1..]
        .iter()
        .any(|(q, c)| (q[0] - p[0]).hypot(q[1] - p[1]) > AMBIGUOUS_KM && c - cost < AMBIGUOUS_COST);
    if unique && rival {
        return None;
    }
    let chi2 = cost / dof as f64;
    let fast = p[2].hypot(p[3]) > MAX_SPEED;
    if fast || p[0].abs() > cfg.area_km * 1.5 || p[1].abs() > cfg.area_km * 1.5 {
        return None;
    }
    let alt_m = p[4] * 1000.0;
    if !(-500.0..=16_000.0).contains(&alt_m) {
        return None;
    }
    let cov = invert(&normal(&prob.jacobian(&p)))?;
    let k = chi2.max(1.0);
    let (a, b, c) = (cov[0][0] * k, cov[0][1] * k, cov[1][1] * k);
    let mid = (a + c) / 2.0;
    let half = (((a - c) / 2.0).powi(2) + b * b).sqrt();
    let major_km = (mid + half).max(0.0).sqrt();
    if !major_km.is_finite() {
        return None;
    }
    let (lat, lon) = plane.to_ll(p[0], p[1]);
    Some(Fix {
        ts: t_ref,
        lat,
        lon,
        alt_m,
        alt_sigma_m: (cov[4][4] * k).sqrt() * 1000.0,
        ve: p[2],
        vn: p[3],
        major_km,
        minor_km: (mid - half).max(0.0).sqrt(),
        angle_deg: 0.5 * (2.0 * b).atan2(a - c).to_degrees(),
        chi2,
        sites,
    })
}

/// One site's track, as the detections that joined it near the fix time.
#[derive(Clone, Debug)]
pub struct TrackObs {
    pub site: usize,
    pub obs: Vec<Obs>,
}

/// At most `n` of `obs`, evenly spread and always keeping the newest.
fn spread(obs: &[Obs], n: usize) -> impl Iterator<Item = Obs> + '_ {
    let step = obs.len().div_ceil(n).max(1);
    let last = obs.len().saturating_sub(1);
    obs.iter()
        .enumerate()
        .filter(move |(i, _)| (last - i).is_multiple_of(step))
        .map(|(_, o)| *o)
}

/// Fixes for every group of tracks, from different sites, that fit one aircraft. Each
/// track is used once; groups with more sites, then better fits, win.
pub fn associate(
    plane: Plane,
    pairs: &[Pair],
    tracks: &[TrackObs],
    t_ref: i64,
    cfg: &Cfg,
    deadline: Instant,
) -> Vec<(Fix, Vec<usize>)> {
    let gather = |members: &[usize]| -> Vec<Obs> {
        members
            .iter()
            .flat_map(|&m| spread(&tracks[m].obs, PER_TRACK))
            .collect()
    };
    let attempt = |members: &[usize]| solve(plane, pairs, &gather(members), t_ref, cfg);
    let mut cands: Vec<(Fix, Vec<usize>)> = vec![];
    for i in 0..tracks.len() {
        for j in i + 1..tracks.len() {
            if tracks[i].site == tracks[j].site || Instant::now() > deadline {
                continue;
            }
            // with only two sites the height is a guess and the crossing may be either
            // one, so a pair that is really one aircraft can fit badly or ambiguously
            // until a third site settles it
            let seed = best_fit(plane, pairs, &gather(&[i, j]), t_ref, cfg, false);
            let Some(mut fix) = seed.filter(|f| f.chi2 <= cfg.max_chi2 * SEED_SLACK) else {
                continue;
            };
            let mut members = vec![i, j];
            let mut sites: Vec<usize> = vec![tracks[i].site, tracks[j].site];
            loop {
                let best = (0..tracks.len())
                    .filter(|k| !sites.contains(&tracks[*k].site))
                    .filter_map(|k| {
                        let mut m = members.clone();
                        m.push(k);
                        attempt(&m).map(|f| (f, k))
                    })
                    .min_by(|a, b| a.0.chi2.total_cmp(&b.0.chi2));
                let Some((f, k)) = best else { break };
                fix = f;
                members.push(k);
                sites.push(tracks[k].site);
            }
            if members.len() == 2 {
                match attempt(&members) {
                    Some(f) => fix = f,
                    None => continue,
                }
            }
            if good(&fix, cfg) {
                cands.push((fix, members));
            }
        }
    }
    cands.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then(a.0.chi2.total_cmp(&b.0.chi2))
    });
    let mut used = vec![false; tracks.len()];
    let mut out = vec![];
    for (fix, members) in cands {
        if members.iter().any(|m| used[*m]) {
            continue;
        }
        for m in &members {
            used[*m] = true;
        }
        out.push((fix, members));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{ecef, horizontal_km, velocity};
    use crate::sim::Rng;

    const CFG: Cfg = Cfg {
        alt_m: 6000.0,
        alt_sigma_m: 3000.0,
        range_sigma_km: 0.3,
        doppler_sigma_hz: 1.0,
        area_km: 80.0,
        max_chi2: 2.0,
        max_error_km: 5.0,
    };

    fn plane() -> Plane {
        Plane {
            lat0: 39.5,
            lon0: -104.8,
        }
    }

    fn pairs() -> Vec<Pair> {
        let tx = ecef(39.62, -104.95, 2100.0);
        vec![
            Pair::new(tx, ecef(39.50, -104.80, 1650.0), 96.9e6),
            Pair::new(
                ecef(39.30, -104.55, 1800.0),
                ecef(39.45, -104.40, 1700.0),
                99.5e6,
            ),
            Pair::new(tx, ecef(39.75, -104.70, 1600.0), 96.9e6),
        ]
    }

    /// Detections of an aircraft at `lat`, `lon`, `alt` flying `track` at 400 kt, every
    /// 0.5 s for 30 s up to `t_ref`, from the given sites.
    fn observe(
        sites: &[usize],
        lat: f64,
        lon: f64,
        alt: f64,
        track: f64,
        noise: f64,
        rng: &mut Rng,
    ) -> Vec<TrackObs> {
        let pairs = pairs();
        let v = velocity(lat, lon, 400.0, track, 0.0);
        let p0 = ecef(lat, lon, alt);
        sites
            .iter()
            .map(|&s| TrackObs {
                site: s,
                obs: (0..60)
                    .map(|k| {
                        let ts = -30_000 + k * 500;
                        let dt = ts as f64 / 1000.0;
                        let p = [p0[0] + v[0] * dt, p0[1] + v[1] * dt, p0[2] + v[2] * dt];
                        Obs {
                            site: s,
                            ts,
                            range_km: pairs[s].range(p) / 1000.0 + noise * 0.3 * rng.gauss(),
                            doppler_hz: pairs[s].doppler(p, v) + noise * rng.gauss(),
                        }
                    })
                    .collect(),
            })
            .collect()
    }

    fn error_km(f: &Fix, lat: f64, lon: f64, alt: f64) -> f64 {
        horizontal_km(lat, lon, ecef(f.lat, f.lon, f.alt_m), ecef(lat, lon, alt))
    }

    #[test]
    fn plane_round_trips() {
        let p = plane();
        let (lat, lon) = p.to_ll(12.5, -40.0);
        let (e, n) = p.to_en(lat, lon);
        assert!((e - 12.5).abs() < 1e-9 && (n + 40.0).abs() < 1e-9);
    }

    #[test]
    fn two_sites_at_the_assumed_height_find_it_exactly() {
        let mut rng = Rng::new(1);
        let t = observe(&[0, 1], 39.6, -104.6, 6000.0, 250.0, 0.0, &mut rng);
        let obs: Vec<Obs> = t.iter().flat_map(|t| t.obs.clone()).collect();
        let f = solve(plane(), &pairs(), &obs, 0, &CFG).unwrap();
        assert!(error_km(&f, 39.6, -104.6, 6000.0) < 0.05, "{f:?}");
        assert!((f.alt_m - 6000.0).abs() < 3.0 * f.alt_sigma_m, "{f:?}");
        let v = 400.0 * crate::geo::KT;
        let (s, c) = 250f64.to_radians().sin_cos();
        assert!(
            (f.ve - v * s).abs() < 1.0 && (f.vn - v * c).abs() < 1.0,
            "{f:?}"
        );
        assert_eq!(f.sites, [0, 1]);
    }

    #[test]
    fn three_sites_solve_the_height_and_noise_gives_an_honest_ellipse() {
        let mut errs = vec![];
        let mut inside = 0;
        for seed in 0..12 {
            let mut rng = Rng::new(seed);
            let t = observe(&[0, 1, 2], 39.55, -104.65, 9500.0, 120.0, 1.0, &mut rng);
            let obs: Vec<Obs> = t.iter().flat_map(|t| t.obs.clone()).collect();
            let f = solve(plane(), &pairs(), &obs, 0, &CFG).unwrap();
            assert!((f.alt_m - 9500.0).abs() < 3.0 * f.alt_sigma_m, "{f:?}");
            assert!(f.alt_sigma_m < 1500.0, "{f:?}");
            let e = error_km(&f, 39.55, -104.65, 9500.0);
            errs.push(e);
            if e <= 3.0 * f.major_km {
                inside += 1;
            }
        }
        errs.sort_by(f64::total_cmp);
        assert!(errs[6] < 1.0, "median error {} km", errs[6]);
        assert!(inside >= 10, "{inside} of 12 inside 3 sigma");
    }

    #[test]
    fn one_site_or_a_mismatch_gives_no_fix() {
        let mut rng = Rng::new(2);
        let one = observe(&[0], 39.6, -104.6, 6000.0, 250.0, 0.0, &mut rng);
        assert!(solve(plane(), &pairs(), &one[0].obs, 0, &CFG).is_none());
        // site 0 sees one aircraft, site 1 a different one far away
        let a = observe(&[0], 39.6, -104.6, 6000.0, 250.0, 1.0, &mut rng);
        let b = observe(&[1], 39.2, -104.9, 6000.0, 40.0, 1.0, &mut rng);
        let obs: Vec<Obs> = a[0].obs.iter().chain(&b[0].obs).copied().collect();
        let f = solve(plane(), &pairs(), &obs, 0, &CFG);
        assert!(f.is_none(), "{f:?}");
    }

    #[test]
    fn two_crossings_that_fit_alike_give_no_fix() {
        // with noise, two sites can't tell this aircraft from its mirror image across
        // the other crossing of their ellipses; a third site can
        let mut rng = Rng::new(4);
        let t = observe(&[0, 1, 2], 39.35, -104.75, 6000.0, 30.0, 1.0, &mut rng);
        let two: Vec<Obs> = t[..2].iter().flat_map(|t| t.obs.clone()).collect();
        assert!(solve(plane(), &pairs(), &two, 0, &CFG).is_none());
        let three: Vec<Obs> = t.iter().flat_map(|t| t.obs.clone()).collect();
        let f = solve(plane(), &pairs(), &three, 0, &CFG).unwrap();
        assert!(error_km(&f, 39.35, -104.75, 6000.0) < 1.0, "{f:?}");
    }

    #[test]
    fn association_pairs_the_right_tracks() {
        let mut rng = Rng::new(4);
        let a = observe(&[0, 1, 2], 39.6, -104.6, 6000.0, 250.0, 1.0, &mut rng);
        let b = observe(&[0, 1, 2], 39.35, -104.75, 6000.0, 30.0, 1.0, &mut rng);
        // site by site, so the right groups are (0, 2, 4) and (1, 3, 5)
        let tracks: Vec<TrackObs> = (0..3).flat_map(|i| [a[i].clone(), b[i].clone()]).collect();
        let deadline = Instant::now() + std::time::Duration::from_secs(600);
        let fixes = associate(plane(), &pairs(), &tracks, 0, &CFG, deadline);
        let mut groups: Vec<Vec<usize>> = fixes.iter().map(|f| f.1.clone()).collect();
        for g in &mut groups {
            g.sort_unstable();
        }
        groups.sort();
        assert_eq!(groups, [vec![0, 2, 4], vec![1, 3, 5]]);
        for (f, m) in &fixes {
            let (lat, lon) = if m.contains(&0) {
                (39.6, -104.6)
            } else {
                (39.35, -104.75)
            };
            assert!(error_km(f, lat, lon, 6000.0) < 1.0, "{f:?}");
        }
    }
}
