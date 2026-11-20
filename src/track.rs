//! A tracker for one site's detections, the piece blah2 leaves as future work. Each track
//! is a Kalman filter on bistatic range and its rate of change; blah2's Doppler measures
//! that rate directly, so one detection is enough to start a track. Detections join tracks
//! by gated nearest neighbour, a track is confirmed after M hits in its first N CPIs, and a
//! confirmed track ends after a run of misses.

use crate::blah2::Frame;
use crate::geo::C;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub range_sigma_km: f64,
    pub doppler_sigma_hz: f64,
    /// how hard targets can manoeuvre, m/s^2
    pub accel: f64,
    /// squared Mahalanobis distance a detection may be from a track's prediction
    pub gate: f64,
    pub confirm_m: u32,
    pub confirm_n: u32,
    /// consecutive missed CPIs that end a confirmed track
    pub drop_after: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            range_sigma_km: 0.3,
            doppler_sigma_hz: 1.0,
            accel: 5.0,
            // 99% for two degrees of freedom
            gate: 9.21,
            confirm_m: 3,
            confirm_n: 5,
            drop_after: 6,
        }
    }
}

/// A detection that joined a track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub ts: i64,
    pub range_km: f64,
    pub doppler_hz: f64,
}

type M2 = [[f64; 2]; 2];

fn add(a: M2, b: M2) -> M2 {
    [
        [a[0][0] + b[0][0], a[0][1] + b[0][1]],
        [a[1][0] + b[1][0], a[1][1] + b[1][1]],
    ]
}

fn mul(a: M2, b: M2) -> M2 {
    let mut o = [[0.0; 2]; 2];
    for (i, row) in o.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = a[i][0] * b[0][j] + a[i][1] * b[1][j];
        }
    }
    o
}

fn inv(a: M2) -> M2 {
    let det = a[0][0] * a[1][1] - a[0][1] * a[1][0];
    [
        [a[1][1] / det, -a[0][1] / det],
        [-a[1][0] / det, a[0][0] / det],
    ]
}

fn mv(a: M2, v: [f64; 2]) -> [f64; 2] {
    [
        a[0][0] * v[0] + a[0][1] * v[1],
        a[1][0] * v[0] + a[1][1] * v[1],
    ]
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u32,
    pub confirmed: bool,
    pub hits: Vec<Hit>,
    /// bistatic range km, and its rate km/s
    x: [f64; 2],
    p: M2,
    t: i64,
    /// the last N CPIs, newest in bit 0: 1 for a hit
    recent: u32,
    age: u32,
    misses: u32,
}

impl Track {
    pub fn range_km(&self) -> f64 {
        self.x[0]
    }

    /// Doppler of the filtered range rate for wavelength `lambda` (metres).
    pub fn doppler_hz(&self, lambda: f64) -> f64 {
        -self.x[1] * 1000.0 / lambda
    }
}

pub struct Tracker {
    params: Params,
    lambda: f64,
    noise: M2,
    next_id: u32,
    live: Vec<Track>,
    ended: Vec<Track>,
}

impl Tracker {
    pub fn new(params: Params, fc_hz: f64) -> Self {
        let lambda = C / fc_hz;
        let rate_sigma = params.doppler_sigma_hz * lambda / 1000.0;
        Self {
            params,
            lambda,
            noise: [
                [params.range_sigma_km.powi(2), 0.0],
                [0.0, rate_sigma.powi(2)],
            ],
            next_id: 1,
            live: vec![],
            ended: vec![],
        }
    }

    /// Tracks still running, confirmed or not.
    pub fn live(&self) -> &[Track] {
        &self.live
    }

    /// Every confirmed track, running or ended.
    pub fn confirmed(&self) -> impl Iterator<Item = &Track> {
        self.ended
            .iter()
            .chain(self.live.iter().filter(|t| t.confirmed))
    }

    fn predict(&self, t: &Track, ts: i64) -> ([f64; 2], M2) {
        let dt = (ts - t.t) as f64 / 1000.0;
        let f = [[1.0, dt], [0.0, 1.0]];
        let ft = [[1.0, 0.0], [dt, 1.0]];
        let q = (self.params.accel / 1000.0).powi(2);
        let qm = [
            [q * dt.powi(4) / 4.0, q * dt.powi(3) / 2.0],
            [q * dt.powi(3) / 2.0, q * dt * dt],
        ];
        (mv(f, t.x), add(mul(mul(f, t.p), ft), qm))
    }

    pub fn step(&mut self, frame: &Frame) {
        let z: Vec<[f64; 2]> = frame
            .dets
            .iter()
            .map(|d| [d.range_km, -d.doppler_hz * self.lambda / 1000.0])
            .collect();
        let predicted: Vec<([f64; 2], M2)> = self
            .live
            .iter()
            .map(|t| self.predict(t, frame.ts))
            .collect();
        let mut cands = vec![];
        for (i, (x, p)) in predicted.iter().enumerate() {
            let s_inv = inv(add(*p, self.noise));
            for (j, zj) in z.iter().enumerate() {
                let y = [zj[0] - x[0], zj[1] - x[1]];
                let sy = mv(s_inv, y);
                let d2 = y[0] * sy[0] + y[1] * sy[1];
                if d2 <= self.params.gate {
                    cands.push((d2, i, j));
                }
            }
        }
        cands.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut track_hit = vec![None; self.live.len()];
        let mut det_used = vec![false; z.len()];
        for (_, i, j) in cands {
            if track_hit[i].is_none() && !det_used[j] {
                track_hit[i] = Some(j);
                det_used[j] = true;
            }
        }
        let mask = (1u32 << self.params.confirm_n) - 1;
        for (i, t) in self.live.iter_mut().enumerate() {
            let (x, p) = predicted[i];
            t.t = frame.ts;
            t.age += 1;
            t.recent <<= 1;
            match track_hit[i] {
                Some(j) => {
                    let s_inv = inv(add(p, self.noise));
                    let k = mul(p, s_inv);
                    let y = [z[j][0] - x[0], z[j][1] - x[1]];
                    let ky = mv(k, y);
                    t.x = [x[0] + ky[0], x[1] + ky[1]];
                    let ikp = [[1.0 - k[0][0], -k[0][1]], [-k[1][0], 1.0 - k[1][1]]];
                    t.p = mul(ikp, p);
                    t.recent |= 1;
                    t.misses = 0;
                    let d = frame.dets[j];
                    t.hits.push(Hit {
                        ts: frame.ts,
                        range_km: d.range_km,
                        doppler_hz: d.doppler_hz,
                    });
                }
                None => {
                    t.x = x;
                    t.p = p;
                    t.misses += 1;
                }
            }
            t.recent &= mask;
            if !t.confirmed && t.recent.count_ones() >= self.params.confirm_m {
                t.confirmed = true;
            }
        }
        let (n, drop) = (self.params.confirm_n, self.params.drop_after);
        let mut keep = vec![];
        for t in self.live.drain(..) {
            if t.confirmed && t.misses >= drop {
                self.ended.push(t);
            } else if t.confirmed || t.age < n {
                keep.push(t);
            }
        }
        self.live = keep;
        for (j, zj) in z.iter().enumerate().filter(|(j, _)| !det_used[*j]) {
            let d = frame.dets[j];
            self.live.push(Track {
                id: self.next_id,
                confirmed: false,
                hits: vec![Hit {
                    ts: frame.ts,
                    range_km: d.range_km,
                    doppler_hz: d.doppler_hz,
                }],
                x: *zj,
                p: self.noise,
                t: frame.ts,
                recent: 1,
                age: 1,
                misses: 0,
            });
            self.next_id += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blah2::Det;
    use crate::sim::Rng;

    const FC: f64 = 96.9e6;

    fn det(r: f64, f: f64) -> Det {
        Det {
            range_km: r,
            doppler_hz: f,
            snr_db: 15.0,
        }
    }

    /// A target closing at a steady rate: range shrinks by fd * lambda per second.
    fn target(t_s: f64) -> (f64, f64) {
        let lambda = C / FC;
        let fd = 60.0;
        (40.0 - fd * lambda * t_s / 1000.0, fd)
    }

    #[test]
    fn follows_a_target_through_clutter() {
        let mut rng = Rng::new(3);
        let mut tr = Tracker::new(Params::default(), FC);
        for k in 0..240 {
            let t = k as f64 * 0.5;
            let (r, f) = target(t);
            let mut dets = vec![];
            // seen 80% of the time, with noise
            if rng.unit() < 0.8 {
                dets.push(det(r + 0.2 * rng.gauss(), f + 0.6 * rng.gauss()));
            }
            for _ in 0..rng.poisson(1.0) {
                dets.push(det(rng.between(1.0, 60.0), rng.between(-200.0, 200.0)));
            }
            tr.step(&Frame {
                ts: (t * 1000.0) as i64,
                dets,
            });
        }
        let tracks: Vec<&Track> = tr.confirmed().collect();
        let real = tracks
            .iter()
            .max_by_key(|t| t.hits.len())
            .expect("a confirmed track");
        assert!(real.hits.len() > 150, "{} hits", real.hits.len());
        let (r_end, f_end) = target(119.5);
        assert!((real.range_km() - r_end).abs() < 0.5, "{}", real.range_km());
        assert!((real.doppler_hz(C / FC) - f_end).abs() < 2.0);
        // about 240 clutter detections, but few line up three times in five CPIs
        let ghosts = tracks.len() - 1;
        assert!(ghosts <= 3, "{ghosts} clutter tracks confirmed");
    }

    #[test]
    fn confirmation_and_ending() {
        let p = Params::default();
        let mut tr = Tracker::new(p, FC);
        let at = |k: i64| target(k as f64 * 0.5);
        let step = |tr: &mut Tracker, k: i64, seen: bool| {
            let (r, f) = at(k);
            let dets = if seen { vec![det(r, f)] } else { vec![] };
            tr.step(&Frame { ts: k * 500, dets });
        };
        step(&mut tr, 0, true);
        step(&mut tr, 1, true);
        assert!(!tr.live()[0].confirmed, "two hits are not enough");
        step(&mut tr, 2, true);
        assert!(tr.live()[0].confirmed);
        // a confirmed track coasts through drop_after - 1 misses
        for k in 3..3 + p.drop_after as i64 - 1 {
            step(&mut tr, k, false);
        }
        assert_eq!(tr.live().len(), 1);
        let k = 3 + p.drop_after as i64 - 1;
        step(&mut tr, k, true);
        assert_eq!(tr.live()[0].hits.len(), 4, "picked up again after coasting");
        for j in 1..=p.drop_after as i64 {
            step(&mut tr, k + j, false);
        }
        assert!(tr.live().is_empty());
        assert_eq!(tr.confirmed().count(), 1);

        // a lone detection never confirms and is dropped after N CPIs
        let mut tr = Tracker::new(p, FC);
        tr.step(&Frame {
            ts: 0,
            dets: vec![det(10.0, 50.0)],
        });
        for k in 1..p.confirm_n as i64 {
            tr.step(&Frame {
                ts: k * 500,
                dets: vec![],
            });
        }
        assert!(tr.live().is_empty());
        assert_eq!(tr.confirmed().count(), 0);
    }

    #[test]
    fn crossing_targets_keep_their_tracks() {
        // same range at t = 30 s, Doppler 40 Hz apart, so the gate keeps them apart
        let lambda = C / FC;
        let mut tr = Tracker::new(Params::default(), FC);
        for k in 0..120 {
            let t = k as f64 * 0.5;
            let a = (30.0 - 20.0 * lambda * (t - 30.0) / 1000.0, 20.0);
            let b = (30.0 + 20.0 * lambda * (t - 30.0) / 1000.0, -20.0);
            tr.step(&Frame {
                ts: (t * 1000.0) as i64,
                dets: vec![det(a.0, a.1), det(b.0, b.1)],
            });
        }
        let tracks: Vec<&Track> = tr.confirmed().collect();
        assert_eq!(tracks.len(), 2);
        for t in tracks {
            let sign = t.hits[0].doppler_hz.signum();
            assert!(t.hits.iter().all(|h| h.doppler_hz.signum() == sign));
            assert_eq!(t.hits.len(), 120);
        }
    }
}
