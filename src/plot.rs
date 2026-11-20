//! The two plots the shared time-series chart can't draw: detection probability by range
//! as bars, and the range-Doppler scatter of detections against ADS-B. Both use the
//! shared chart CSS, so they follow light and dark mode.

use std::fmt::Write as _;

use ridgewarden_client::util::esc;

use crate::blah2::Window;
use crate::stats::Bin;
use crate::store::RecentDet;

const W: f64 = 640.0;
const LEFT: f64 = 46.0;
const RIGHT: f64 = 10.0;
const TOP: f64 = 22.0;
const BOTTOM: f64 = 34.0;

fn open(h: f64, title: &str) -> String {
    format!(
        "<svg class=chart viewBox=\"0 0 {W} {h}\" role=img aria-label=\"{t}\"><title>{t}</title>",
        t = esc(title)
    )
}

fn empty(mut s: String, h: f64, text: &str) -> String {
    let _ = write!(
        s,
        "<text x=\"{}\" y=\"{}\" class=axis text-anchor=middle>{}</text></svg>",
        W / 2.0,
        h / 2.0,
        esc(text)
    );
    s
}

fn frame(s: &mut String, h: f64, x_label: &str) {
    let _ = write!(
        s,
        "<rect class=frame x=\"{LEFT}\" y=\"{TOP}\" width=\"{}\" height=\"{}\"/>\
         <text class=axis x=\"{}\" y=\"{}\" text-anchor=middle>{}</text>",
        W - LEFT - RIGHT,
        h - TOP - BOTTOM,
        (LEFT + W - RIGHT) / 2.0,
        h - 4.0,
        esc(x_label)
    );
}

/// A step for axis ticks that gives about five to ten of them over `span`.
fn tick_step(span: f64) -> f64 {
    [1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 500.0]
        .into_iter()
        .find(|s| span / s <= 10.0)
        .unwrap_or(1000.0)
}

/// What the bars are bins of.
pub struct Axis<'a> {
    pub title: &'a str,
    pub label: &'a str,
    /// after each bin's edges in tooltips, e.g. "km"
    pub unit: &'a str,
}

pub const RANGE: Axis = Axis {
    title: "Detection probability by bistatic range",
    label: "bistatic range, km",
    unit: "km",
};

pub const HOUR_OF_DAY: Axis = Axis {
    title: "Detection probability by hour of the day",
    label: "hour of the day, UTC",
    unit: "h UTC",
};

/// Pd per bin, 0 to 100%, with `threshold` as a dashed line. Bins with fewer than
/// `min_opps` opportunities are drawn faint: too little data to judge.
pub fn pd_bars(bins: &[Bin], threshold: f64, min_opps: i64, axis: &Axis) -> String {
    let h = 210.0;
    let s = open(h, axis.title);
    let Some(last) = bins.last() else {
        return empty(s, h, "no aircraft in coverage yet");
    };
    let mut s = s;
    frame(&mut s, h, axis.label);
    let max_km = last.to as f64;
    let x = |km: f64| LEFT + km / max_km * (W - LEFT - RIGHT);
    let y = |pd: f64| TOP + (1.0 - pd) * (h - TOP - BOTTOM);
    for pd in [0.0, 0.5, 1.0] {
        let _ = write!(
            s,
            "<text class=axis x=\"{}\" y=\"{:.1}\" text-anchor=end>{:.0}%</text>",
            LEFT - 4.0,
            y(pd) + 4.0,
            pd * 100.0
        );
    }
    let step = tick_step(max_km);
    let mut km = 0.0;
    while km <= max_km {
        let _ = write!(
            s,
            "<text class=axis x=\"{:.1}\" y=\"{}\" text-anchor=middle>{km}</text>",
            x(km),
            h - BOTTOM + 13.0
        );
        km += step;
    }
    for b in bins {
        let Some(pd) = b.pd() else { continue };
        let faint = if b.opps < min_opps {
            " fill-opacity=\".3\""
        } else {
            ""
        };
        let tip = format!(
            "{}-{} {}: seen in {} of {} CPIs ({:.0}%){}",
            b.from,
            b.to,
            axis.unit,
            b.hits,
            b.opps,
            pd * 100.0,
            if b.opps < min_opps {
                ", too few to judge"
            } else {
                ""
            }
        );
        let (x0, x1) = (x(b.from as f64), x(b.to as f64));
        let _ = write!(
            s,
            "<rect class=d1{faint} x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\"><title>{}</title></rect>",
            x0 + 0.5,
            y(pd),
            (x1 - x0 - 1.0).max(0.5),
            y(0.0) - y(pd),
            esc(&tip)
        );
    }
    let _ = write!(
        s,
        "<line class=rule x1=\"{LEFT}\" x2=\"{}\" y1=\"{t:.1}\" y2=\"{t:.1}\"><title>{}</title></line></svg>",
        W - RIGHT,
        esc(&format!("threshold {:.0}%", threshold * 100.0)),
        t = y(threshold)
    );
    s
}

/// One aircraft's expected track on the plot.
pub struct Trace<'a> {
    pub label: String,
    pub points: &'a [(f64, f64)],
}

/// Detections as dots (matched to an aircraft or not) over the ADS-B aircraft's expected
/// tracks, in bistatic range against Doppler. The shaded band is blah2's clutter notch.
pub fn scatter(w: &Window, dets: &[RecentDet], traces: &[Trace]) -> String {
    let h = 380.0;
    let mut s = open(h, "Detections and ADS-B aircraft in range and Doppler");
    frame(&mut s, h, "bistatic range, km");
    let max_km = w.max_km.max(1.0);
    let (lo, hi) = (w.dop_min, w.dop_max.max(w.dop_min + 1.0));
    let x = |km: f64| LEFT + km / max_km * (W - LEFT - RIGHT);
    let y = |hz: f64| TOP + (hi - hz) / (hi - lo) * (h - TOP - BOTTOM);
    let inside = |km: f64, hz: f64| (0.0..=max_km).contains(&km) && (lo..=hi).contains(&hz);
    if w.notch_hz > 0.0 {
        let (top, bottom) = (y(w.notch_hz.min(hi)), y((-w.notch_hz).max(lo)));
        let _ = write!(
            s,
            "<rect x=\"{LEFT}\" y=\"{top:.1}\" width=\"{}\" height=\"{:.1}\" fill=\"currentColor\" fill-opacity=\".08\"><title>clutter notch: blah2 ignores anything slower than {} Hz</title></rect>",
            W - LEFT - RIGHT,
            bottom - top,
            w.notch_hz
        );
    }
    let step = tick_step(hi - lo);
    let mut hz = (lo / step).ceil() * step;
    while hz <= hi {
        let _ = write!(
            s,
            "<text class=axis x=\"{}\" y=\"{:.1}\" text-anchor=end>{hz} Hz</text>",
            LEFT - 4.0,
            y(hz) + 4.0
        );
        hz += step;
    }
    let step = tick_step(max_km);
    let mut km = 0.0;
    while km <= max_km {
        let _ = write!(
            s,
            "<text class=axis x=\"{:.1}\" y=\"{}\" text-anchor=middle>{km}</text>",
            x(km),
            h - BOTTOM + 13.0
        );
        km += step;
    }
    for t in traces {
        let pts: Vec<&(f64, f64)> = t.points.iter().filter(|p| inside(p.0, p.1)).collect();
        let Some(end) = pts.last() else { continue };
        let mut d = String::new();
        for (i, p) in pts.iter().enumerate() {
            let _ = write!(
                d,
                "{}{:.1},{:.1}",
                if i == 0 { "M" } else { "L" },
                x(p.0),
                y(p.1)
            );
        }
        // labels near the right edge go left of the point so they aren't cut off
        let (lx, anchor) = if x(end.0) > W * 0.75 {
            (x(end.0) - 3.0, "end")
        } else {
            (x(end.0) + 3.0, "start")
        };
        let _ = write!(
            s,
            "<path class=s1 d=\"{d}\"><title>{l}</title></path>\
             <text class=\"key k1\" x=\"{lx:.1}\" y=\"{:.1}\" text-anchor={anchor}>{l}</text>",
            y(end.1) - 3.0,
            l = esc(&t.label)
        );
    }
    for d in dets.iter().filter(|d| inside(d.range_km, d.doppler_hz)) {
        let _ = write!(
            s,
            "<circle class={} cx=\"{:.1}\" cy=\"{:.1}\" r=\"2\"/>",
            if d.matched { "d1" } else { "d2" },
            x(d.range_km),
            y(d.doppler_hz)
        );
    }
    let _ = write!(
        s,
        "<text class=\"key k1\" x=\"{}\" y=\"14\">lines: ADS-B aircraft, dots: detections matched to one</text>\
         <text class=\"key k2\" x=\"{}\" y=\"14\" text-anchor=end>dots: unmatched detections</text></svg>",
        LEFT,
        W - RIGHT
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_show_each_bin_and_mark_thin_ones() {
        let bins = [
            Bin {
                from: 0,
                to: 5,
                opps: 100,
                hits: 90,
            },
            Bin {
                from: 5,
                to: 10,
                opps: 4,
                hits: 1,
            },
        ];
        let svg = pd_bars(&bins, 0.8, 30, &RANGE);
        assert_eq!(svg.matches("<rect class=d1").count(), 2);
        assert_eq!(svg.matches("fill-opacity").count(), 1);
        assert!(svg.contains("0-5 km: seen in 90 of 100 CPIs (90%)"));
        assert!(svg.contains("5-10 km: seen in 1 of 4 CPIs (25%), too few to judge"));
        assert!(pd_bars(&[], 0.8, 30, &RANGE).contains("no aircraft in coverage yet"));
    }

    #[test]
    fn scatter_draws_tracks_and_both_kinds_of_dot() {
        let w = Window {
            min_km: 0.75,
            max_km: 60.0,
            dop_min: -200.0,
            dop_max: 200.0,
            notch_hz: 15.0,
        };
        let dot = |r, f, m| RecentDet {
            ts: 0,
            range_km: r,
            doppler_hz: f,
            matched: m,
        };
        let dets = [
            dot(10.0, 50.0, true),
            dot(30.0, -80.0, false),
            dot(90.0, 0.0, false),
        ];
        let pts = [(10.0, 51.0), (10.5, 49.0)];
        let svg = scatter(
            &w,
            &dets,
            &[Trace {
                label: "<TST1>".into(),
                points: &pts,
            }],
        );
        assert_eq!(svg.matches("<circle class=d1").count(), 1);
        assert_eq!(
            svg.matches("<circle class=d2").count(),
            1,
            "off the plot is skipped"
        );
        assert!(svg.contains("&lt;TST1&gt;") && !svg.contains("<TST1>"));
        assert!(svg.contains("clutter notch"));
    }
}
