//! The map: each site's transmitter and receiver, the bistatic ellipse of every track it
//! is following (somewhere on that ellipse is an aircraft), the aircraft located from two
//! or more sites with their error ellipses, and the ADS-B aircraft at the same moment.
//! Ellipses are drawn on the ground; an aircraft's height puts it inside its own, by a
//! kilometre or more for one at cruising height near the sites.

use std::fmt::Write as _;

use ridgewarden_client::util::esc;

use crate::adsb::Track as Aircraft;
use crate::fuse::Plane;
use crate::geo::{dot, enu_to_ecef, scale};
use crate::hub::View;
use crate::store::{FixRow, Truth};

const W: f64 = 600.0;

pub struct Opts {
    pub show_ids: bool,
    pub show_unmatched: bool,
    /// km from the centre to the edge
    pub half_km: f64,
}

struct Frame {
    k: f64,
}

impl Frame {
    fn pt(&self, e: f64, n: f64) -> (f64, f64) {
        (W / 2.0 + e * self.k, W / 2.0 - n * self.k)
    }
}

/// Where an ADS-B aircraft was at `ts`, on the map's grid.
fn aircraft_at(plane: &Plane, a: &Aircraft, ts: i64) -> (f64, f64) {
    let d = scale(a.vel, (ts - a.ts) as f64 / 1000.0);
    let east = enu_to_ecef(a.lat, a.lon, [1.0, 0.0, 0.0]);
    let north = enu_to_ecef(a.lat, a.lon, [0.0, 1.0, 0.0]);
    let (e, n) = plane.to_en(a.lat, a.lon);
    (e + dot(d, east) / 1000.0, n + dot(d, north) / 1000.0)
}

fn ellipse(
    s: &mut String,
    f: &Frame,
    centre: (f64, f64),
    axes: (f64, f64),
    angle_deg: f64,
    attrs: &str,
    tip: &str,
) {
    let (cx, cy) = f.pt(centre.0, centre.1);
    let _ = write!(
        s,
        "<ellipse {attrs} cx=\"{cx:.1}\" cy=\"{cy:.1}\" rx=\"{:.1}\" ry=\"{:.1}\" fill=\"none\" \
         transform=\"rotate({:.1} {cx:.1} {cy:.1})\"><title>{}</title></ellipse>",
        axes.0 * f.k,
        axes.1 * f.k,
        -angle_deg,
        esc(tip)
    );
}

fn label(s: &mut String, at: (f64, f64), class: &str, text: &str) {
    let _ = write!(
        s,
        "<text class=\"key {class}\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
        at.0 + 5.0,
        at.1 - 5.0,
        esc(text)
    );
}

/// Fixes no ADS-B aircraft explains locate aircraft public ADS-B doesn't show, so they
/// are left out unless `show_unmatched`.
pub fn shown(show_unmatched: bool, r: &FixRow) -> bool {
    show_unmatched || matches!(r.truth, Truth::Aircraft { .. })
}

pub fn render(v: &View, o: &Opts) -> String {
    let f = Frame {
        k: (W / 2.0 - 10.0) / o.half_km,
    };
    let mut s = format!(
        "<svg class=\"chart\" xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {W} {W}\" role=\"img\" \
         aria-label=\"Map of radar sites, tracks and located aircraft\">\
         <rect class=\"frame\" x=\"0.5\" y=\"0.5\" width=\"{}\" height=\"{}\"/>",
        W - 1.0,
        W - 1.0
    );
    for (i, site) in v.sites.iter().enumerate() {
        let c = i.min(2);
        let su = &site.setup;
        let tx = v.plane.to_en(su.tx.latitude, su.tx.longitude);
        let rx = v.plane.to_en(su.rx.latitude, su.rx.longitude);
        let baseline = (tx.0 - rx.0).hypot(tx.1 - rx.1);
        let mid = ((tx.0 + rx.0) / 2.0, (tx.1 + rx.1) / 2.0);
        let angle = (rx.1 - tx.1).atan2(rx.0 - tx.0).to_degrees();
        for &(id, range, doppler) in &site.tracks {
            let a = (range + baseline) / 2.0;
            let b2 = a * a - (baseline / 2.0).powi(2);
            if b2 <= 0.0 {
                continue;
            }
            let tip = format!("{} track {id}: {range:.1} km, {doppler:.0} Hz", site.call);
            ellipse(
                &mut s,
                &f,
                mid,
                (a, b2.sqrt()),
                angle,
                &format!("class=\"s{c}\" stroke-opacity=\".55\""),
                &tip,
            );
        }
        let (x, y) = f.pt(rx.0, rx.1);
        let _ = write!(
            s,
            "<rect class=\"d{c}\" x=\"{:.1}\" y=\"{:.1}\" width=\"8\" height=\"8\"><title>{} receiver</title></rect>",
            x - 4.0,
            y - 4.0,
            esc(&site.call)
        );
        label(&mut s, (x, y), &format!("k{c}"), &site.call);
        let (x, y) = f.pt(tx.0, tx.1);
        let _ = write!(
            s,
            "<path class=\"d{c}\" d=\"M{x:.1},{:.1}L{:.1},{:.1}L{:.1},{:.1}Z\"><title>{} transmitter: {}</title></path>",
            y - 6.0,
            x + 5.0,
            y + 4.0,
            x - 5.0,
            y + 4.0,
            esc(&site.call),
            esc(&su.tx.name)
        );
    }
    for a in v.truth.iter().flatten() {
        let (e, n) = aircraft_at(&v.plane, a, v.ts);
        let (x, y) = f.pt(e, n);
        let _ = write!(
            s,
            "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"6\" fill=\"none\" stroke=\"currentColor\" stroke-opacity=\".5\"><title>ADS-B</title></circle>"
        );
        if o.show_ids {
            let id = if a.flight.is_empty() {
                &a.hex
            } else {
                &a.flight
            };
            let _ = write!(
                s,
                "<text class=\"axis\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                x + 7.0,
                y + 12.0,
                esc(id)
            );
        }
    }
    for r in v.fixes.iter().filter(|r| shown(o.show_unmatched, r)) {
        let fx = &r.fix;
        let (e, n) = v.plane.to_en(fx.lat, fx.lon);
        ellipse(
            &mut s,
            &f,
            (e, n),
            (fx.major_km.max(0.05), fx.minor_km.max(0.05)),
            fx.angle_deg,
            "class=\"rule\"",
            "1-sigma error",
        );
        let (x, y) = f.pt(e, n);
        let _ = write!(
            s,
            "<circle class=\"d0\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"3.5\"><title>fix {}: seen by {}</title></circle>",
            r.track,
            esc(&r.sites)
        );
        label(&mut s, (x, y), "k0", &format!("#{}", r.track));
    }
    let bar = [5.0, 10.0, 20.0, 50.0, 100.0]
        .into_iter()
        .find(|b| b * f.k >= 60.0)
        .unwrap_or(100.0);
    let _ = write!(
        s,
        "<line x1=\"12\" x2=\"{:.1}\" y1=\"{y}\" y2=\"{y}\" stroke=\"currentColor\" stroke-width=\"2\"/>\
         <text class=\"axis\" x=\"12\" y=\"{}\">{bar} km</text>\
         <text class=\"axis\" x=\"{}\" y=\"18\" text-anchor=\"middle\">N</text>\
         <path d=\"M{n},22L{n},40\" stroke=\"currentColor\"/></svg>",
        12.0 + bar * f.k,
        W - 20.0,
        W - 20.0,
        y = W - 14.0,
        n = W - 20.0
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blah2::parse_config;
    use crate::fuse::Fix;
    use crate::hub::SiteTracks;

    fn view(truth: Truth) -> View {
        let setup = parse_config(
            &crate::sim::Plan {
                sites: vec![crate::sim::SitePlan {
                    rx: [39.5, -104.8, 1650.0],
                    tx: [39.62, -104.95, 2100.0],
                    fc_hz: 96.9e6,
                }],
                cpi_s: 0.5,
                aircraft: 0,
                dark: 0,
                false_per_cpi: 0.0,
                r50_km: 35.0,
                radius_km: 60.0,
            }
            .config_json(0)
            .to_string(),
        )
        .unwrap();
        View {
            ts: 0,
            plane: Plane {
                lat0: 39.5,
                lon0: -104.8,
            },
            sites: vec![SiteTracks {
                call: "<N0SIM>".into(),
                setup,
                tracks: vec![(4, 20.0, 50.0), (5, -1.0, 10.0)],
            }],
            fixes: vec![FixRow {
                track: 12,
                fix: Fix {
                    ts: 0,
                    lat: 39.55,
                    lon: -104.7,
                    alt_m: 6000.0,
                    alt_sigma_m: 300.0,
                    ve: 0.0,
                    vn: 0.0,
                    major_km: 0.4,
                    minor_km: 0.1,
                    angle_deg: 30.0,
                    chi2: 1.0,
                    sites: vec![0, 1],
                },
                sites: "A, B".into(),
                truth,
            }],
            truth: None,
        }
    }

    #[test]
    fn draws_track_ellipses_sites_and_fixes() {
        let o = Opts {
            show_ids: true,
            show_unmatched: false,
            half_km: 80.0,
        };
        let seen = Truth::Aircraft {
            hex: "a00001".into(),
            flight: String::new(),
            err_km: 0.2,
        };
        let svg = render(&view(seen), &o);
        // one track ellipse (the negative range can't be one), one error ellipse
        assert_eq!(svg.matches("<ellipse class=\"s0\"").count(), 1);
        assert_eq!(svg.matches("<ellipse class=\"rule\"").count(), 1);
        assert!(svg.contains("#12"));
        assert!(svg.contains("&lt;N0SIM&gt;") && !svg.contains("<N0SIM>"));
        // a fix with no ADS-B aircraft near it is left off unless asked for
        let svg = render(&view(Truth::Nothing), &o);
        assert!(!svg.contains("#12"));
        let all = Opts {
            show_unmatched: true,
            ..o
        };
        assert!(render(&view(Truth::Nothing), &all).contains("#12"));
        let unknown = &view(Truth::Unknown).fixes[0];
        assert!(!shown(false, unknown) && shown(true, unknown));
        // served on its own as image/svg+xml, so every attribute value is quoted
        assert!(
            !svg.contains("=none") && !svg.contains("=currentColor") && !svg.contains("=chart")
        );
    }
}
