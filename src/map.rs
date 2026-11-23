//! The map: each site's transmitter and receiver, the bistatic ellipse of every track it
//! is following (somewhere on that ellipse is an aircraft), the aircraft located from two
//! or more sites with their error ellipses, and the ADS-B aircraft at the same moment.
//! Tracks and fixes no ADS-B aircraft explains are left off unless asked for.
//! Ellipses are drawn on the ground; an aircraft's height puts it inside its own, by a
//! kilometre or more for one at cruising height near the sites.

use std::fmt::Write as _;

use ridgewarden_client::util::esc;
use serde_json::{Value, json};

use crate::adsb::Track as Aircraft;
use crate::fuse::Plane;
use crate::geo::{dot, enu_to_ecef, scale};
use crate::hub::{Ellipse, View};
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

/// The same for tracks: two sites' ellipses cross where the hidden fix would be.
pub fn drawn(show_unmatched: bool, e: &Ellipse) -> bool {
    show_unmatched || e.adsb
}

/// The line under the map saying what it left out and why, if anything.
pub fn left_off(v: &View, show_unmatched: bool) -> Option<String> {
    let fixes = v.fixes.iter().filter(|r| !shown(show_unmatched, r)).count();
    let tracks = v
        .sites
        .iter()
        .flat_map(|s| &s.tracks)
        .filter(|e| !drawn(show_unmatched, e))
        .count();
    let parts: Vec<String> = [
        (fixes, "fix", "fixes"),
        (tracks, "track ellipse", "track ellipses"),
    ]
    .into_iter()
    .filter(|&(k, ..)| k > 0)
    .map(|(k, one, many)| format!("{k} {}", if k == 1 { one } else { many }))
    .collect();
    if parts.is_empty() {
        return None;
    }
    let why = if v.truth.is_none() {
        "there was no current ADS-B to check them against"
    } else {
        "no ADS-B aircraft is on them"
    };
    Some(format!(
        "Left off the map, since {why}: {}. They may track aircraft public ADS-B doesn't show \
         (set fusion.show_unmatched to draw them).",
        parts.join(" and ")
    ))
}

/// The map's moment as JSON, leaving out what the map leaves out, with ADS-B aircraft
/// ids only if `ids`.
pub fn api(v: &View, ids: bool, show_unmatched: bool) -> Value {
    let fixes: Vec<Value> = v
        .fixes
        .iter()
        .filter(|r| shown(show_unmatched, r))
        .map(|r| {
            let f = &r.fix;
            let adsb = match &r.truth {
                Truth::Unknown => json!(null),
                Truth::Nothing => json!({"near": false}),
                Truth::Aircraft {
                    hex,
                    flight,
                    err_km,
                } if ids => json!({"near": true, "hex": hex, "flight": flight, "err_km": err_km}),
                Truth::Aircraft { err_km, .. } => json!({"near": true, "err_km": err_km}),
            };
            json!({"track": r.track, "sites": r.sites, "lat": f.lat, "lon": f.lon, "alt_m": f.alt_m,
                "alt_sigma_m": f.alt_sigma_m, "ve": f.ve, "vn": f.vn, "major_km": f.major_km,
                "minor_km": f.minor_km, "angle_deg": f.angle_deg, "chi2": f.chi2, "adsb": adsb})
        })
        .collect();
    let sites: Vec<Value> = v
        .sites
        .iter()
        .map(|s| {
            let tracks: Vec<Value> = s
                .tracks
                .iter()
                .filter(|e| drawn(show_unmatched, e))
                .map(|e| {
                    json!({"id": e.id, "range_km": e.range_km, "doppler_hz": e.doppler_hz,
                        "adsb": e.adsb})
                })
                .collect();
            json!({"call": s.call, "tracks": tracks})
        })
        .collect();
    json!({"experimental": true, "ts": v.ts, "sites": sites, "fixes": fixes})
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
        for e in site.tracks.iter().filter(|e| drawn(o.show_unmatched, e)) {
            let a = (e.range_km + baseline) / 2.0;
            let b2 = a * a - (baseline / 2.0).powi(2);
            if b2 <= 0.0 {
                continue;
            }
            let tip = format!(
                "{} track {}: {:.1} km, {:.0} Hz",
                site.call, e.id, e.range_km, e.doppler_hz
            );
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

    fn ellipse(id: u32, range_km: f64, adsb: bool) -> Ellipse {
        Ellipse {
            id,
            range_km,
            doppler_hz: 50.0,
            adsb,
        }
    }

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
                tracks: vec![
                    ellipse(4, 20.0, true),
                    ellipse(5, -1.0, true),
                    ellipse(6, 30.0, false),
                ],
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
        // one track ellipse (the negative range can't be one, and no ADS-B aircraft is on
        // track 6), one error ellipse
        assert_eq!(svg.matches("<ellipse class=\"s0\"").count(), 1);
        assert!(svg.contains("track 4:") && !svg.contains("track 6:"));
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
        let svg_all = render(&view(Truth::Nothing), &all);
        assert!(svg_all.contains("#12"));
        assert!(
            svg_all.contains("track 6:"),
            "asked for, tracks without ADS-B are drawn"
        );
        let unknown = &view(Truth::Unknown).fixes[0];
        assert!(!shown(false, unknown) && shown(true, unknown));
        // served on its own as image/svg+xml, so every attribute value is quoted
        assert!(
            !svg.contains("=none") && !svg.contains("=currentColor") && !svg.contains("=chart")
        );
    }

    fn seen() -> Truth {
        Truth::Aircraft {
            hex: "a00001".into(),
            flight: "TST1".into(),
            err_km: 0.2,
        }
    }

    #[test]
    fn says_what_it_left_off_and_why() {
        // tracks 4 and 5 have an ADS-B aircraft on them and 6 hasn't; the fix has none near
        let mut v = view(Truth::Nothing);
        v.truth = Some(vec![]);
        let note = left_off(&v, false).unwrap();
        assert!(
            note.contains("no ADS-B aircraft is on them: 1 fix and 1 track ellipse."),
            "{note}"
        );
        assert_eq!(left_off(&v, true), None, "asked for, nothing is left off");
        v.sites[0].tracks.push(ellipse(7, 25.0, false));
        let note = left_off(&v, false).unwrap();
        assert!(note.contains("1 fix and 2 track ellipses"), "{note}");
        v.truth = None;
        let note = left_off(&v, false).unwrap();
        assert!(note.contains("no current ADS-B"), "{note}");
        // only what was left off is listed
        let mut fix_seen = view(seen());
        fix_seen.truth = Some(vec![]);
        let note = left_off(&fix_seen, false).unwrap();
        assert!(note.contains("on them: 1 track ellipse. They"), "{note}");
        let mut tracks_seen = view(Truth::Nothing);
        tracks_seen.truth = Some(vec![]);
        tracks_seen.sites[0].tracks.retain(|e| e.adsb);
        let note = left_off(&tracks_seen, false).unwrap();
        assert!(note.contains("on them: 1 fix. They"), "{note}");
        fix_seen.sites[0].tracks.retain(|e| e.adsb);
        assert_eq!(left_off(&fix_seen, false), None);
    }

    #[test]
    fn api_leaves_out_what_the_map_does() {
        let ids = |j: &Value| -> Vec<u64> {
            j["sites"][0]["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_u64().unwrap())
                .collect()
        };
        let v = view(Truth::Nothing);
        let j = api(&v, true, false);
        assert_eq!(ids(&j), [4, 5]);
        assert_eq!(j["fixes"], json!([]));
        let all = api(&v, true, true);
        assert_eq!(ids(&all), [4, 5, 6]);
        assert_eq!(all["sites"][0]["tracks"][2]["adsb"], false);
        assert_eq!(all["fixes"][0]["adsb"], json!({"near": false}));
        // aircraft ids only when they are shown
        assert_eq!(
            api(&view(seen()), true, false)["fixes"][0]["adsb"]["hex"],
            "a00001"
        );
        assert_eq!(
            api(&view(seen()), false, false)["fixes"][0]["adsb"],
            json!({"near": true, "err_km": 0.2})
        );
    }
}
