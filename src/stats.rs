//! The performance numbers, summed from the hourly counts over a window: detection
//! probability by bistatic range and by aircraft, unmatched detections, and how far out
//! detection stays reliable.

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::blah2::Window;

/// Opportunities and hits for one bin, `from` up to `to` (km of bistatic range, or hours).
#[derive(Clone, Debug, PartialEq)]
pub struct Bin {
    pub from: i64,
    pub to: i64,
    pub opps: i64,
    pub hits: i64,
}

pub fn pd(hits: i64, opps: i64) -> Option<f64> {
    (opps > 0).then(|| hits as f64 / opps as f64)
}

impl Bin {
    pub fn pd(&self) -> Option<f64> {
        pd(self.hits, self.opps)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plane {
    pub hex: String,
    pub flight: String,
    pub opps: i64,
    pub hits: i64,
    pub min_km: f64,
    pub max_km: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub cpis: i64,
    pub dets: i64,
    pub unmatched: i64,
    pub opps: i64,
    pub hits: i64,
    pub bins: Vec<Bin>,
    /// most opportunities first
    pub planes: Vec<Plane>,
}

/// Everything from `from_hour` (unix ms / 3600000) on, with ranges in `bin_km` bins.
pub fn summary(conn: &Connection, from_hour: i64, bin_km: i64) -> Result<Summary> {
    let (cpis, dets, unmatched) = conn.query_row(
        "SELECT coalesce(sum(cpis), 0), coalesce(sum(dets), 0), coalesce(sum(unmatched), 0)
         FROM hour_frames WHERE hour >= ?1",
        [from_hour],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut st = conn.prepare(
        "SELECT km / ?2, sum(opps), sum(hits) FROM hour_range WHERE hour >= ?1
         GROUP BY km / ?2 ORDER BY km / ?2",
    )?;
    let bins: Vec<Bin> = st
        .query_map(params![from_hour, bin_km], |r| {
            let b: i64 = r.get(0)?;
            Ok(Bin {
                from: b * bin_km,
                to: (b + 1) * bin_km,
                opps: r.get(1)?,
                hits: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut st = conn.prepare(
        "SELECT hex, max(flight), sum(opps), sum(hits), min(min_km), max(max_km)
         FROM hour_aircraft WHERE hour >= ?1 GROUP BY hex ORDER BY sum(opps) DESC, hex",
    )?;
    let planes: Vec<Plane> = st
        .query_map([from_hour], |r| {
            Ok(Plane {
                hex: r.get(0)?,
                flight: r.get(1)?,
                opps: r.get(2)?,
                hits: r.get(3)?,
                min_km: r.get(4)?,
                max_km: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Summary {
        cpis,
        dets,
        unmatched,
        opps: bins.iter().map(|b| b.opps).sum(),
        hits: bins.iter().map(|b| b.hits).sum(),
        bins,
        planes,
    })
}

/// The far edge of the last bin, counting out from the baseline, before Pd first drops
/// below `threshold`. Bins with fewer than `min_opps` opportunities say nothing either
/// way: they neither stop the walk nor extend the range.
pub fn reliable_km(bins: &[Bin], threshold: f64, min_opps: i64) -> Option<i64> {
    let mut best = None;
    for b in bins.iter().filter(|b| b.opps >= min_opps) {
        if (b.hits as f64) < threshold * b.opps as f64 {
            break;
        }
        best = Some(b.to);
    }
    best
}

/// The Pd a radar that saw nothing at all would still score: the chance that one of
/// `per_cpi` unmatched detections, spread evenly over the search area outside the notch,
/// lands inside an aircraft's gates.
pub fn chance_pd(per_cpi: f64, range_gate_km: f64, doppler_gate_hz: f64, w: &Window) -> f64 {
    let notch = (2.0 * w.notch_hz).min(w.dop_max - w.dop_min);
    let area = (w.max_km - w.min_km) * (w.dop_max - w.dop_min - notch);
    if area <= 0.0 {
        return 1.0;
    }
    let p = (4.0 * range_gate_km * doppler_gate_hz / area).min(1.0);
    1.0 - (1.0 - p).powf(per_cpi)
}

/// Aircraft with enough opportunities to judge, and how many of them were seen at least
/// `min_hits` times.
pub fn aircraft_seen(planes: &[Plane], min_opps: i64, min_hits: i64) -> (usize, usize) {
    let judged: Vec<&Plane> = planes.iter().filter(|p| p.opps >= min_opps).collect();
    let seen = judged.iter().filter(|p| p.hits >= min_hits).count();
    (judged.len(), seen)
}

/// How one group of multi-site fixes compares with ADS-B.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FixGroup {
    pub fixes: i64,
    /// fixes made while ADS-B was current
    pub scored: i64,
    /// scored fixes with an ADS-B aircraft within `truth_km`
    pub matched: i64,
    /// across the ground from the ADS-B position, among the matched fixes only
    pub median_km: Option<f64>,
    pub p90_km: Option<f64>,
    /// fused tracks followed for a minute or more with no ADS-B aircraft near any fix:
    /// aircraft without ADS-B, or a ghost that kept fitting
    pub dark_tracks: i64,
}

/// Multi-site fixes by how many sites saw them: two, or three and more. A fused track
/// counts with the most sites any of its fixes had.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FixStats {
    pub two: FixGroup,
    pub more: FixGroup,
    pub tracks: i64,
}

fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    (!sorted.is_empty()).then(|| sorted[((sorted.len() - 1) as f64 * q).round() as usize])
}

#[derive(Default)]
struct Fused {
    first: i64,
    last: i64,
    scored: i64,
    matched: bool,
    sites: i64,
}

impl FixStats {
    fn group(&mut self, sites: i64) -> &mut FixGroup {
        if sites >= 3 {
            &mut self.more
        } else {
            &mut self.two
        }
    }

    fn close(&mut self, t: Fused) {
        self.tracks += 1;
        if !t.matched && t.scored >= 3 && t.last - t.first >= 60_000 {
            self.group(t.sites).dark_tracks += 1;
        }
    }
}

pub fn fix_stats(conn: &Connection, from: i64) -> Result<FixStats> {
    let mut st = conn.prepare(
        "SELECT track, ts, nsites, scored, err_km FROM fixes WHERE ts >= ?1 ORDER BY track, ts",
    )?;
    let mut rows = st.query([from])?;
    let mut out = FixStats::default();
    let mut errs: [Vec<f64>; 2] = [vec![], vec![]];
    let mut cur: Option<(i64, Fused)> = None;
    while let Some(r) = rows.next()? {
        let (track, ts, nsites): (i64, i64, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let scored: bool = r.get(3)?;
        let err: Option<f64> = r.get(4)?;
        let g = out.group(nsites);
        g.fixes += 1;
        g.scored += scored as i64;
        if let Some(e) = err {
            g.matched += 1;
            errs[(nsites >= 3) as usize].push(e);
        }
        if cur.as_ref().is_none_or(|c| c.0 != track) {
            if let Some((_, t)) = cur.take() {
                out.close(t);
            }
            cur = Some((
                track,
                Fused {
                    first: ts,
                    ..Default::default()
                },
            ));
        }
        let t = &mut cur.as_mut().expect("set above").1;
        t.last = ts;
        t.scored += scored as i64;
        t.matched |= err.is_some();
        t.sites = t.sites.max(nsites);
    }
    if let Some((_, t)) = cur {
        out.close(t);
    }
    for (k, e) in errs.iter_mut().enumerate() {
        e.sort_by(f64::total_cmp);
        let g = out.group(if k == 1 { 3 } else { 2 });
        g.median_km = quantile(e, 0.5);
        g.p90_km = quantile(e, 0.9);
    }
    Ok(out)
}

/// Pd for each UTC hour of the day, over every day since `from_hour`. A broadcast that
/// turns to talk or silence at certain hours shows up here as a dip.
pub fn by_hour_of_day(conn: &Connection, from_hour: i64) -> Result<Vec<Bin>> {
    let mut st = conn.prepare(
        "SELECT hour % 24, sum(opps), sum(hits) FROM hour_range WHERE hour >= ?1
         GROUP BY hour % 24 ORDER BY hour % 24",
    )?;
    let rows = st.query_map([from_hour], |r| {
        let h: i64 = r.get(0)?;
        Ok(Bin {
            from: h,
            to: h + 1,
            opps: r.get(1)?,
            hits: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One hour's totals, for the charts.
#[derive(Clone, Debug, PartialEq)]
pub struct Hour {
    /// unix ms at the start of the hour
    pub ts: i64,
    pub cpis: i64,
    pub unmatched: i64,
    pub opps: i64,
    pub hits: i64,
}

pub fn hours(conn: &Connection, from_hour: i64) -> Result<Vec<Hour>> {
    let mut st = conn.prepare(
        "SELECT f.hour, f.cpis, f.unmatched,
                coalesce((SELECT sum(opps) FROM hour_range r WHERE r.hour = f.hour), 0),
                coalesce((SELECT sum(hits) FROM hour_range r WHERE r.hour = f.hour), 0)
         FROM hour_frames f WHERE f.hour >= ?1 ORDER BY f.hour",
    )?;
    let rows = st.query_map([from_hour], |r| {
        Ok(Hour {
            ts: r.get::<_, i64>(0)? * crate::store::HOUR,
            cpis: r.get(1)?,
            unmatched: r.get(2)?,
            opps: r.get(3)?,
            hits: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(from: i64, opps: i64, hits: i64) -> Bin {
        Bin {
            from,
            to: from + 5,
            opps,
            hits,
        }
    }

    #[test]
    fn reliable_range_stops_at_the_first_weak_bin() {
        let bins = [
            bin(0, 100, 95),
            bin(5, 100, 90),
            bin(10, 3, 0),
            bin(15, 100, 81),
            bin(20, 100, 60),
            bin(25, 100, 99),
        ];
        // the thin 10-15 km bin is skipped, 20-25 km breaks the run, 25-30 km never counts
        assert_eq!(reliable_km(&bins, 0.8, 20), Some(20));
        assert_eq!(reliable_km(&bins, 0.96, 20), None);
        assert_eq!(reliable_km(&[], 0.8, 20), None);
        assert_eq!(reliable_km(&bins[2..3], 0.8, 20), None);
    }

    #[test]
    fn fix_accuracy_and_tracks_without_ads_b() {
        use crate::fuse::Fix;
        use crate::store::{FixRow, Truth, record_fixes};
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::store::SCHEMA).unwrap();
        let fix = |ts: i64, nsites: usize| Fix {
            ts,
            lat: 39.5,
            lon: -104.8,
            alt_m: 6000.0,
            alt_sigma_m: 500.0,
            ve: 0.0,
            vn: 0.0,
            major_km: 0.5,
            minor_km: 0.2,
            angle_deg: 0.0,
            chi2: 1.0,
            sites: (0..nsites).collect(),
        };
        let seen = |e: f64| Truth::Aircraft {
            hex: "a00001".into(),
            flight: String::new(),
            err_km: e,
        };
        let mut rows = vec![];
        // track 1: three sites, matched, errors 0.2, 0.4, 2.0 km
        for (i, e) in [0.2, 0.4, 2.0].into_iter().enumerate() {
            rows.push((1, fix(i as i64 * 10_000, 3), seen(e)));
        }
        // track 2: two sites, nothing near it for 70 s, with an ADS-B outage in the middle
        for i in 0..8 {
            let truth = if i == 4 {
                Truth::Unknown
            } else {
                Truth::Nothing
            };
            rows.push((2, fix(i * 10_000, 2), truth));
        }
        // track 3: two sites, nothing near it but only 20 s long
        for i in 0..3 {
            rows.push((3, fix(i * 10_000, 2), Truth::Nothing));
        }
        // track 4: two sites, ADS-B down
        rows.push((4, fix(0, 2), Truth::Unknown));
        let rows: Vec<FixRow> = rows
            .into_iter()
            .map(|(track, fix, truth)| FixRow {
                track,
                fix,
                sites: "A, B".into(),
                truth,
            })
            .collect();
        record_fixes(&mut c, &rows).unwrap();
        let s = fix_stats(&c, 0).unwrap();
        assert_eq!(s.tracks, 4);
        assert_eq!((s.two.dark_tracks, s.more.dark_tracks), (1, 0));
        assert_eq!((s.more.fixes, s.more.scored, s.more.matched), (3, 3, 3));
        assert_eq!(s.more.median_km, Some(0.4));
        assert_eq!(s.more.p90_km, Some(2.0));
        assert_eq!((s.two.fixes, s.two.scored, s.two.matched), (12, 10, 0));
        assert_eq!(s.two.median_km, None);
        assert_eq!(fix_stats(&c, 75_000).unwrap().tracks, 0);
    }

    #[test]
    fn chance_matches_from_false_alarms() {
        let w = Window {
            min_km: 0.0,
            max_km: 60.0,
            dop_min: -200.0,
            dop_max: 200.0,
            notch_hz: 15.0,
        };
        // a 4 km x 16 Hz gate box in a 60 km x 370 Hz area: 64 / 22200
        let one = chance_pd(1.0, 2.0, 8.0, &w);
        assert!((one - 64.0 / 22_200.0).abs() < 1e-9, "{one}");
        let twenty = chance_pd(20.0, 2.0, 8.0, &w);
        assert!((twenty - (1.0 - (1.0 - 64.0 / 22_200.0f64).powi(20))).abs() < 1e-9);
        assert_eq!(chance_pd(0.0, 2.0, 8.0, &w), 0.0);
    }

    #[test]
    fn sums_over_the_window_in_wider_bins() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::store::SCHEMA).unwrap();
        c.execute_batch(
            "INSERT INTO hour_frames VALUES (1, 100, 50, 10), (2, 100, 40, 5), (3, 100, 30, 1);
             INSERT INTO hour_range VALUES (1, 3, 1000, 1000), (2, 3, 10, 9), (2, 7, 10, 1),
                 (3, 12, 4, 2);
             INSERT INTO hour_aircraft VALUES (2, 'a00001', 'TST1', 15, 9, 3.0, 9.5),
                 (3, 'a00001', '', 4, 2, 12.0, 12.9), (3, 'a00002', '', 5, 0, 7.0, 8.0);",
        )
        .unwrap();
        let s = summary(&c, 2, 5).unwrap();
        assert_eq!((s.cpis, s.dets, s.unmatched), (200, 70, 6));
        assert_eq!(
            s.bins,
            [
                Bin {
                    from: 0,
                    to: 5,
                    opps: 10,
                    hits: 9
                },
                bin(5, 10, 1),
                bin(10, 4, 2)
            ]
        );
        assert_eq!((s.opps, s.hits), (24, 12));
        assert_eq!(s.planes[0].hex, "a00001");
        assert_eq!(s.planes[0].flight, "TST1");
        assert_eq!((s.planes[0].opps, s.planes[0].hits), (19, 11));
        assert_eq!((s.planes[0].min_km, s.planes[0].max_km), (3.0, 12.9));
        assert_eq!(aircraft_seen(&s.planes, 5, 3), (2, 1));
        assert_eq!(aircraft_seen(&s.planes, 6, 3), (1, 1));
        let hod = by_hour_of_day(&c, 0).unwrap();
        assert_eq!(hod.len(), 3);
        assert_eq!((hod[1].from, hod[1].opps, hod[1].hits), (2, 20, 10));
        // hour 26 is 2 AM on the next day: it adds to the same bar
        c.execute("INSERT INTO hour_range VALUES (26, 3, 10, 0)", [])
            .unwrap();
        let hod = by_hour_of_day(&c, 0).unwrap();
        assert_eq!((hod[1].opps, hod[1].hits), (30, 10));
        c.execute("DELETE FROM hour_range WHERE hour = 26", [])
            .unwrap();
        let h = hours(&c, 2).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].opps, h[0].hits, h[1].opps), (20, 10, 4));
        assert_eq!(h[0].ts, 2 * crate::store::HOUR);
    }
}
