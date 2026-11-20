//! Matching one CPI's detections to ADS-B aircraft: where each aircraft should show up in
//! bistatic range and Doppler, and which detection, if any, is it.

use crate::adsb::Track;
use crate::blah2::{Frame, Window};
use crate::geo::{Pair, add, scale};

#[derive(Clone, Copy, Debug)]
pub struct Gates {
    pub range_km: f64,
    pub doppler_hz: f64,
    /// an ADS-B position further than this from the CPI's time is not used
    pub max_age_ms: i64,
}

/// Where an aircraft should be in one CPI, and whether it was seen there.
#[derive(Clone, Debug, PartialEq)]
pub struct Expected {
    pub hex: String,
    pub flight: String,
    pub range_km: f64,
    pub doppler_hz: f64,
    /// inside blah2's detection window, so a miss counts against it
    pub counted: bool,
    /// index of the detection matched to it
    pub hit: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub aircraft: Vec<Expected>,
    /// for each detection, the index of the aircraft it was matched to
    pub matched: Vec<Option<usize>>,
}

impl Outcome {
    pub fn unmatched(&self) -> usize {
        self.matched.iter().filter(|m| m.is_none()).count()
    }
}

/// Bistatic range (km) and Doppler (Hz) of `t` at `ts`, moving it on in a straight line
/// from where ADS-B last put it.
pub fn expect(pair: &Pair, t: &Track, ts: i64) -> (f64, f64) {
    let dt = (ts - t.ts) as f64 / 1000.0;
    let p = add(t.pos, scale(t.vel, dt));
    (pair.range(p) / 1000.0, pair.doppler(p, t.vel))
}

/// Pair each aircraft with at most one detection inside the gates, closest pairs first.
pub fn match_frame(
    pair: &Pair,
    window: &Window,
    gates: &Gates,
    tracks: &[Track],
    frame: &Frame,
) -> Outcome {
    let mut aircraft: Vec<Expected> = tracks
        .iter()
        .filter(|t| (frame.ts - t.ts).abs() <= gates.max_age_ms)
        .map(|t| {
            let (range_km, doppler_hz) = expect(pair, t, frame.ts);
            Expected {
                hex: t.hex.clone(),
                flight: t.flight.clone(),
                range_km,
                doppler_hz,
                counted: window.contains(range_km, doppler_hz),
                hit: None,
            }
        })
        .collect();
    let mut cands = vec![];
    for (i, a) in aircraft.iter().enumerate() {
        for (j, d) in frame.dets.iter().enumerate() {
            let dr = (d.range_km - a.range_km) / gates.range_km;
            let df = (d.doppler_hz - a.doppler_hz) / gates.doppler_hz;
            if dr.abs() <= 1.0 && df.abs() <= 1.0 {
                cands.push((dr * dr + df * df, i, j));
            }
        }
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut matched = vec![None; frame.dets.len()];
    for (_, i, j) in cands {
        if aircraft[i].hit.is_none() && matched[j].is_none() {
            aircraft[i].hit = Some(j);
            matched[j] = Some(i);
        }
    }
    Outcome { aircraft, matched }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blah2::Det;
    use crate::geo::{FT, ecef, velocity};

    fn pair() -> Pair {
        Pair::new(
            ecef(39.6, -104.95, 1800.0),
            ecef(39.5, -104.8, 1650.0),
            96.9e6,
        )
    }

    fn window() -> Window {
        Window {
            min_km: 0.75,
            max_km: 60.0,
            dop_min: -200.0,
            dop_max: 200.0,
            notch_hz: 15.0,
        }
    }

    const GATES: Gates = Gates {
        range_km: 2.0,
        doppler_hz: 8.0,
        max_age_ms: 10_000,
    };

    fn plane(hex: &str, lat: f64, lon: f64, track: f64, ts: i64) -> Track {
        Track {
            hex: hex.into(),
            flight: String::new(),
            ts,
            lat,
            lon,
            pos: ecef(lat, lon, 20_000.0 * FT),
            vel: velocity(lat, lon, 300.0, track, 0.0),
        }
    }

    fn det(r: f64, f: f64) -> Det {
        Det {
            range_km: r,
            doppler_hz: f,
            snr_db: 15.0,
        }
    }

    #[test]
    fn a_detection_where_ads_b_says_is_a_hit() {
        let a = plane("a00001", 39.7, -104.7, 200.0, 1_000);
        let (r, f) = expect(&pair(), &a, 2_000);
        assert!(window().contains(r, f), "{r} km {f} Hz");
        let frame = Frame {
            ts: 2_000,
            dets: vec![det(r + 0.8, f - 3.0), det(r + 15.0, f)],
        };
        let o = match_frame(&pair(), &window(), &GATES, &[a], &frame);
        assert_eq!(o.aircraft[0].hit, Some(0));
        assert!(o.aircraft[0].counted);
        assert_eq!(o.matched, [Some(0), None]);
        assert_eq!(o.unmatched(), 1);
    }

    #[test]
    fn dead_reckoning_moves_the_aircraft_on() {
        let a = plane("a00001", 39.7, -104.7, 200.0, 0);
        let (r0, f0) = expect(&pair(), &a, 0);
        let (r5, _) = expect(&pair(), &a, 5_000);
        // 300 kt for 5 s is 772 m; positive Doppler means the path is shortening
        assert!(f0 > 0.0 && r5 < r0, "{r0} {r5} {f0}");
        let lambda = pair().lambda;
        let predicted = r0 - f0 * lambda * 5.0 / 1000.0;
        assert!((r5 - predicted).abs() < 0.03, "{r5} vs {predicted}");
    }

    #[test]
    fn outside_the_gates_is_a_miss() {
        let a = plane("a00001", 39.7, -104.7, 200.0, 1_000);
        let (r, f) = expect(&pair(), &a, 1_000);
        let frame = Frame {
            ts: 1_000,
            dets: vec![det(r + 2.5, f), det(r, f + 9.0)],
        };
        let o = match_frame(&pair(), &window(), &GATES, &[a], &frame);
        assert_eq!(o.aircraft[0].hit, None);
        assert_eq!(o.unmatched(), 2);
    }

    #[test]
    fn closest_pair_wins_and_each_detection_is_used_once() {
        let a = plane("a00001", 39.7, -104.7, 200.0, 1_000);
        let mut b = a.clone();
        b.hex = "a00002".into();
        let (r, f) = expect(&pair(), &a, 1_000);
        // two aircraft in the same spot and one detection: only one of them gets it
        let frame = Frame {
            ts: 1_000,
            dets: vec![det(r + 0.1, f)],
        };
        let o = match_frame(&pair(), &window(), &GATES, &[a.clone(), b], &frame);
        let hits = o.aircraft.iter().filter(|x| x.hit.is_some()).count();
        assert_eq!(hits, 1);
        assert_eq!(o.unmatched(), 0);

        let c = plane("a00003", 39.8, -104.6, 90.0, 1_000);
        let (rc, fc) = expect(&pair(), &c, 1_000);
        let frame = Frame {
            ts: 1_000,
            dets: vec![det(rc, fc), det(r, f)],
        };
        let o = match_frame(&pair(), &window(), &GATES, &[a, c], &frame);
        assert_eq!(o.aircraft[0].hit, Some(1));
        assert_eq!(o.aircraft[1].hit, Some(0));
    }

    #[test]
    fn a_contested_detection_goes_to_the_nearer_aircraft() {
        let a = plane("a00001", 39.7, -104.7, 200.0, 1_000);
        let (r, f) = expect(&pair(), &a, 1_000);
        // b flies the same path a little behind a: about 1 km further out in bistatic
        // range now, since a positive Doppler means the range is shrinking
        let mut b = a.clone();
        b.hex = "a00002".into();
        b.ts += (1000.0 / (f * pair().lambda) * 1000.0) as i64;
        let gates = Gates {
            max_age_ms: 60_000,
            ..GATES
        };
        let (rb, _) = expect(&pair(), &b, 1_000);
        assert!((rb - r - 1.0).abs() < 0.05, "{r} {rb}");
        let frame = Frame {
            ts: 1_000,
            dets: vec![det(rb - 0.1, f)],
        };
        for tracks in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let o = match_frame(&pair(), &window(), &gates, &tracks, &frame);
            let winner = o.aircraft.iter().find(|x| x.hit.is_some()).unwrap();
            assert_eq!(winner.hex, "a00002");
        }
    }

    #[test]
    fn notch_and_stale_positions_are_not_counted() {
        // flying along the baseline's perpendicular bisector far out: slow Doppler
        let mut slow = plane("a00001", 39.7, -104.7, 0.0, 1_000);
        slow.vel = [0.0; 3];
        let o = match_frame(
            &pair(),
            &window(),
            &GATES,
            &[slow],
            &Frame {
                ts: 1_000,
                dets: vec![],
            },
        );
        assert!(
            !o.aircraft[0].counted,
            "zero Doppler is in the clutter notch"
        );

        let old = plane("a00002", 39.7, -104.7, 200.0, 0);
        let o = match_frame(
            &pair(),
            &window(),
            &GATES,
            &[old],
            &Frame {
                ts: 20_000,
                dets: vec![],
            },
        );
        assert!(o.aircraft.is_empty());
    }
}
