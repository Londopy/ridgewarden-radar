//! The performance page, recent detections against ADS-B, the map, a plain description of
//! the setup, and the statistics and the map as JSON.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ridgewarden_client::svg::{Chart, Line};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, ms_to_rfc3339, now_ms, short_time};
use serde_json::{Value, json};

use crate::app::App;
use crate::blah2::Setup;
use crate::map;
use crate::plot::{self, Trace, pd_bars, scatter};
use crate::stats::{
    FixGroup, Summary, aircraft_seen, chance_pd, fix_stats, hours, pd, reliable_km, summary,
};
use crate::store::{HOUR, KEEP_FIXES_MS, RECENT_MS, Truth, recent_dets, recent_paths};

const NAV: &[(&str, &str)] = &[
    ("/", "Performance"),
    ("/recent", "Detections vs ADS-B"),
    ("/map", "Map"),
    ("/setup", "Setup"),
];

/// The plot draws at most this many detections.
const MAX_DOTS: usize = 6000;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(performance_page))
        .route("/recent", get(recent_page))
        .route("/setup", get(setup_page))
        .route("/map", get(map_page))
        .route("/map.svg", get(map_svg))
        .route("/api/map", get(map_json))
        .route("/api/stats", get(stats_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

async fn page(app: &App, active: &str, title: &str, body: &str) -> Html<String> {
    let who = match app.whoami().await {
        Ok(w) if !w.node.is_empty() => format!("{} at {}", w.call, w.node),
        Ok(w) => w.call,
        Err(_) => "core unreachable".into(),
    };
    let page = Page {
        app: "ridgewarden-radar (experimental)",
        nav: NAV,
        active,
        who: &who,
        refresh: true,
    };
    Html(page.render(title, body))
}

fn heading(title: &str) -> String {
    format!(
        "<h1>{} {}</h1>",
        esc(title),
        pill(Color::Yellow, "experimental")
    )
}

fn pct(x: Option<f64>) -> String {
    x.map_or("-".into(), |p| format!("{:.0}%", p * 100.0))
}

fn banner(b: &mut String, text: &str) {
    let _ = write!(b, "<div class=banner>{}</div>", esc(text));
}

/// Banners for anything that makes the numbers incomplete right now.
fn notes(app: &App, now: i64) -> String {
    let live = app.live();
    let mut b = String::new();
    let url = &app.cfg.blah2.url;
    match (&live.setup, &live.setup_err) {
        (None, Some(e)) => banner(
            &mut b,
            &format!(
                "Can't read blah2's config at {url}/api/config: {e}. Nothing is checked until it can."
            ),
        ),
        (None, None) => banner(
            &mut b,
            &format!("Waiting for blah2's config from {url}/api/config."),
        ),
        (Some(s), _) if !s.detection => banner(
            &mut b,
            "blah2's detector is turned off (process.detection.enable in its config.yml), so there are no detections to check.",
        ),
        _ => {}
    }
    if let (Some(_), Some(e)) = (&live.setup, &live.blah2_err) {
        banner(&mut b, &format!("Can't read blah2 at {url}: {e}."));
    }
    let max_age = app.cfg.adsb.max_age_s as i64 * 1000;
    match live.last_cpi {
        Some(t) if now - t > 60_000 => banner(
            &mut b,
            &format!(
                "No new CPI from blah2 for {} (last at {}).",
                human_duration(now - t),
                short_time(t)
            ),
        ),
        Some(t) if (t - now).abs() > max_age => banner(
            &mut b,
            &format!(
                "blah2's clock is {:.0} s off this box's. CPIs are not counted until the clocks agree.",
                (t - now) as f64 / 1000.0
            ),
        ),
        _ => {}
    }
    if live.adsb_ok.is_none_or(|t| now - t > max_age) {
        let why = live.adsb_err.as_deref().unwrap_or("no answer yet");
        banner(
            &mut b,
            &format!(
                "No current ADS-B data from {} ({why}). CPIs are not counted while the truth feed is down, \
                 since every detection would look like a false alarm.",
                app.cfg.adsb.url
            ),
        );
    }
    if let Some(e) = &live.db_err {
        banner(
            &mut b,
            &format!("CPIs can't be recorded, so the statistics are missing them: {e}."),
        );
    }
    if let Some(e) = &live.hub_err {
        banner(
            &mut b,
            &format!("Locating aircraft from the sites' reports failed: {e}."),
        );
    }
    if let Some(e) = &live.site_err {
        banner(
            &mut b,
            &format!(
                "This site's geometry can't be logged through the core, so other sites can't use its reports: {e}."
            ),
        );
    }
    if let Some(e) = &live.mesh_err {
        banner(
            &mut b,
            &format!("Detection reports can't be sent through the core: {e}."),
        );
    }
    b
}

/// (query value, label, hours)
const WINDOWS: &[(&str, &str, i64)] = &[
    ("1h", "1 hour", 1),
    ("24h", "24 hours", 24),
    ("7d", "7 days", 168),
    ("30d", "30 days", 720),
];

fn window(q: &HashMap<String, String>) -> (&'static str, i64) {
    let want = q.get("window").map(String::as_str).unwrap_or("24h");
    WINDOWS
        .iter()
        .find(|w| w.0 == want)
        .map_or(("24h", 24), |w| (w.0, w.2))
}

/// The first hour a window covers, never before blah2's current setup; and when that
/// cuts the window short, when the setup started.
fn first_hour(app: &App, now: i64, hours_back: i64) -> (i64, Option<i64>) {
    let from = now.div_euclid(HOUR) - hours_back;
    let since = app.live().since;
    if since.div_euclid(HOUR) > from {
        (since.div_euclid(HOUR), Some(since))
    } else {
        (from, None)
    }
}

fn window_links(active: &str) -> String {
    let mut b = String::from("<p>Window:");
    for (q, label, _) in WINDOWS {
        if *q == active {
            let _ = write!(b, " <b>{label}</b>");
        } else {
            let _ = write!(b, " <a href=\"/?window={q}\">{label}</a>");
        }
    }
    b.push_str("</p>");
    b
}

/// What an aircraft is called on the pages: its callsign or address from the ADS-B feed,
/// or just a number when the config hides them.
fn aircraft_name(show: bool, n: usize, hex: &str, flight: &str) -> String {
    if !show {
        format!("aircraft {}", n + 1)
    } else if flight.is_empty() {
        hex.to_string()
    } else {
        format!("{flight} ({hex})")
    }
}

/// The reliable range, or why there is none: the nearest judged bin is already too weak,
/// or no bin has enough data yet.
fn reliable_text(app: &App, s: &Summary) -> (String, String) {
    let st = &app.cfg.stats;
    let rule = format!(
        "Pd stays at or above {:.0}% out to here",
        st.pd_threshold * 100.0
    );
    if let Some(km) = reliable_km(&s.bins, st.pd_threshold, st.min_opportunities) {
        return (format!("{km} km"), rule);
    }
    match s.bins.iter().find(|b| b.opps >= st.min_opportunities) {
        Some(b) => (
            "none".into(),
            format!(
                "Pd is {} already at {}-{} km, under {:.0}%",
                pct(b.pd()),
                b.from,
                b.to,
                st.pd_threshold * 100.0
            ),
        ),
        None => (
            "not yet".into(),
            format!("{rule}; no range bin has enough data"),
        ),
    }
}

fn cards(app: &App, s: &Summary, setup: Option<&Setup>) -> String {
    let st = &app.cfg.stats;
    let (judged, seen) = aircraft_seen(&s.planes, st.min_opportunities, st.detected_hits);
    let (reliable, reliable_why) = reliable_text(app, s);
    let per_cpi = s.unmatched as f64 / s.cpis as f64;
    let per_min = setup.map_or(String::new(), |c| {
        format!(", about {:.1} a minute", per_cpi * 60.0 / c.cpi_s)
    });
    let chance = setup.map_or(String::new(), |c| {
        let g = &app.cfg.gates;
        format!(
            "; chance matches with false alarms alone would give about {:.1}%",
            chance_pd(per_cpi, g.range_gate_km, g.doppler_gate_hz, &c.window) * 100.0
        )
    });
    format!(
        "<div class=cards>\
         <div class=card><div class=n>{}</div>detection probability over {} aircraft-CPIs in coverage{chance}</div>\
         <div class=card><div class=n>{} of {}</div>aircraft seen in at least {} CPIs ({}; the project's goal is 80% of aircraft in range)</div>\
         <div class=card><div class=n>{}</div>reliable range: {}</div>\
         <div class=card><div class=n>{:.2}</div>unmatched detections per CPI{per_min} ({} of all detections)</div>\
         </div>",
        pct(pd(s.hits, s.opps)),
        s.opps,
        seen,
        judged,
        st.detected_hits,
        pct(pd(seen as i64, judged as i64)),
        reliable,
        esc(&reliable_why),
        per_cpi,
        pct(pd(s.unmatched, s.dets)),
    )
}

fn range_table(app: &App, s: &Summary) -> String {
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Bistatic range</th><th>Aircraft-CPIs</th><th>Detected</th><th>Pd</th></tr>",
    );
    for bin in &s.bins {
        let thin = thin(app, bin.opps);
        let _ = write!(
            b,
            "<tr><td>{}-{} km</td><td>{}</td><td>{}</td><td>{}{thin}</td></tr>",
            bin.from,
            bin.to,
            bin.opps,
            bin.hits,
            pct(bin.pd())
        );
    }
    b.push_str("</table></div>");
    b
}

fn thin(app: &App, opps: i64) -> &'static str {
    if opps < app.cfg.stats.min_opportunities {
        " <span class=\"mute small\">too few to judge</span>"
    } else {
        ""
    }
}

fn aircraft_table(app: &App, s: &Summary) -> String {
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Aircraft</th><th>CPIs in coverage</th><th>Detected</th><th>Pd</th><th>Bistatic range in coverage</th></tr>",
    );
    for (n, p) in s.planes.iter().take(50).enumerate() {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}{}</td><td>{:.0}-{:.0} km</td></tr>",
            esc(&aircraft_name(
                app.cfg.stats.show_aircraft_ids,
                n,
                &p.hex,
                &p.flight
            )),
            p.opps,
            p.hits,
            pct(pd(p.hits, p.opps)),
            thin(app, p.opps),
            p.min_km,
            p.max_km
        );
    }
    b.push_str("</table></div>");
    if s.planes.len() > 50 {
        let _ = write!(
            b,
            "<p class=\"mute small\">The 50 aircraft with the most time in coverage, of {}.</p>",
            s.planes.len()
        );
    }
    b
}

fn charts(app: &App, from_hour: i64, now: i64, cpi_s: Option<f64>) -> Result<String, Fail> {
    let h = hours(&app.db(), from_hour)?;
    if h.len() < 2 {
        return Ok(String::new());
    }
    let pd_line: Vec<(i64, f64)> = h
        .iter()
        .filter_map(|x| pd(x.hits, x.opps).map(|p| ((x.ts + HOUR / 2).min(now), p * 100.0)))
        .collect();
    let pd_chart = Chart {
        from: from_hour * HOUR,
        to: now,
        unit: "%".into(),
        lines: vec![Line {
            label: "Pd".into(),
            points: pd_line,
        }],
        rule: Some((
            app.cfg.stats.pd_threshold * 100.0,
            "reliable-range threshold".into(),
        )),
        gap_ms: 2 * HOUR,
        ..Default::default()
    };
    let (unit, scale) = match cpi_s {
        Some(c) => ("per min", 60.0 / c),
        None => ("per CPI", 1.0),
    };
    let fa_line: Vec<(i64, f64)> = h
        .iter()
        .filter(|x| x.cpis > 0)
        .map(|x| {
            let t = (x.ts + HOUR / 2).min(now);
            (t, x.unmatched as f64 / x.cpis as f64 * scale)
        })
        .collect();
    let fa_chart = Chart {
        from: from_hour * HOUR,
        to: now,
        unit: unit.into(),
        lines: vec![Line {
            label: "unmatched".into(),
            points: fa_line,
        }],
        gap_ms: 2 * HOUR,
        ..Default::default()
    };
    Ok(format!(
        "<h2>Over time</h2><p class=\"mute small\">Hourly. Detection probability:</p>{}\
         <p class=\"mute small\">Unmatched detections:</p>{}",
        pd_chart.render("Detection probability by hour"),
        fa_chart.render("Unmatched detections by hour")
    ))
}

async fn performance_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let (active, hours_back) = window(&q);
    let (from_hour, changed) = first_hour(&app, now, hours_back);
    let st = &app.cfg.stats;
    let s = summary(&app.db(), from_hour, st.bin_km)?;
    let setup = app.live().setup.clone();
    let cpi_s = setup.as_ref().map(|s| s.cpi_s);
    let mut b = heading("Detection performance");
    b.push_str(
        "<p class=\"mute small\">How often blah2 at this site detects the aircraft ADS-B says are inside its \
         coverage, checked CPI by CPI. Research numbers from an experiment, not a surveillance service.</p>",
    );
    b.push_str(&notes(&app, now));
    if let Some(t) = changed {
        banner(
            &mut b,
            &format!(
                "blah2's setup (positions, frequency, CPI or search area) changed, so these numbers start \
                 at {}, the first whole hour on the new setup.",
                short_time(t)
            ),
        );
    }
    b.push_str(&window_links(active));
    let _ = write!(
        b,
        "<p class=\"mute small\">From {} to now: {} CPIs checked, {} detections.</p>",
        esc(&short_time(from_hour * HOUR)),
        s.cpis,
        s.dets
    );
    if s.cpis == 0 {
        b.push_str(
            "<p class=mute>No CPIs checked against ADS-B in this window yet. They are counted once blah2 \
             and the ADS-B feed are both answering.</p>",
        );
        return Ok(page(&app, "/", "Performance", &b).await);
    }
    b.push_str(&cards(&app, &s, setup.as_ref()));
    b.push_str("<h2>By bistatic range</h2>");
    b.push_str(&pd_bars(
        &s.bins,
        st.pd_threshold,
        st.min_opportunities,
        &plot::RANGE,
    ));
    b.push_str(&range_table(&app, &s));
    b.push_str(&charts(&app, from_hour, now, cpi_s)?);
    if hours_back >= 24 {
        let hod = crate::stats::by_hour_of_day(&app.db(), from_hour)?;
        b.push_str(
            "<h2>By hour of the day</h2><p class=\"mute small\">A transmitter that switches to talk or \
             silence at certain hours gives the radar less to work with then; a dip here is often the \
             programme, not the radar.</p>",
        );
        b.push_str(&pd_bars(
            &hod,
            st.pd_threshold,
            st.min_opportunities,
            &plot::HOUR_OF_DAY,
        ));
    }
    b.push_str("<h2>By aircraft</h2>");
    if s.planes.is_empty() {
        b.push_str("<p class=mute>No ADS-B aircraft inside blah2's coverage in this window.</p>");
    } else {
        b.push_str(&aircraft_table(&app, &s));
    }
    if app.cfg.fusion.enabled {
        let from = (from_hour * HOUR).max(now - KEEP_FIXES_MS);
        b.push_str(&fusion_section(&app, from)?);
    }
    let g = &app.cfg.gates;
    let _ = write!(
        b,
        "<h2>How it is counted</h2><p>Every CPI, each aircraft from the ADS-B feed is moved on to the CPI's \
         time and its bistatic range and Doppler are worked out for this transmitter and receiver. If that \
         falls inside the range and Doppler blah2 searches (and outside its clutter notch), the aircraft had \
         a chance to be seen. A detection within {} km and {} Hz of it counts as seeing it; each detection \
         goes to one aircraft at most, nearest first. A false alarm that happens to land inside an \
         aircraft's gates also counts as seeing it; the first card says how much Pd that alone would give. \
         Detections that match no aircraft are counted as unmatched: false alarms, but also aircraft \
         without ADS-B, so the unmatched rate is an upper bound on false alarms. An aircraft counts as seen \
         once it has been matched in {} CPIs, a low bar; aircraft and range bins with fewer than {} CPIs \
         in coverage are not judged. CPIs are left out while the ADS-B list is not current or blah2's \
         clock disagrees with this box's, and the numbers start again when blah2's setup changes.</p>",
        g.range_gate_km, g.doppler_gate_hz, st.detected_hits, st.min_opportunities
    );
    Ok(page(&app, "/", "Performance", &b).await)
}

async fn recent_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let minutes: i64 = match q.get("minutes").map(String::as_str) {
        Some("1") => 1,
        Some("15") => 15,
        _ => 5,
    };
    let from = now - (minutes * 60_000).min(RECENT_MS);
    let mut b = heading("Detections vs ADS-B");
    b.push_str(&notes(&app, now));
    b.push_str("<p>Last:");
    for m in [1, 5, 15] {
        if m == minutes {
            let _ = write!(b, " <b>{m} min</b>");
        } else {
            let _ = write!(b, " <a href=\"/recent?minutes={m}\">{m} min</a>");
        }
    }
    b.push_str("</p>");
    let Some(setup) = app.live().setup.clone() else {
        b.push_str("<p class=mute>Nothing to plot until blah2's config is in.</p>");
        return Ok(page(&app, "/recent", "Detections vs ADS-B", &b).await);
    };
    let (dets, paths) = {
        let db = app.db();
        (recent_dets(&db, from, MAX_DOTS)?, recent_paths(&db, from)?)
    };
    let matched = dets.iter().filter(|d| d.matched).count();
    let _ = write!(
        b,
        "<p class=\"mute small\">{} detections, {} matched to an ADS-B aircraft and {} not; {} ADS-B aircraft \
         tracked.{}</p>",
        dets.len(),
        matched,
        dets.len() - matched,
        paths
            .iter()
            .map(|p| &p.hex)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        if dets.len() >= MAX_DOTS {
            " Only the most recent detections are drawn."
        } else {
            ""
        }
    );
    let traces: Vec<Trace> = paths
        .iter()
        .enumerate()
        .map(|(n, p)| Trace {
            label: aircraft_name(app.cfg.stats.show_aircraft_ids, n, &p.hex, &p.flight),
            points: &p.points,
        })
        .collect();
    b.push_str(&scatter(&setup.window, &dets, &traces));
    b.push_str(
        "<p class=\"mute small\">Each line is where an ADS-B aircraft should appear, in bistatic range and \
         Doppler, as it moves. Blue dots on a line are blah2 detecting it; orange dots match no aircraft \
         (false alarms, or aircraft without ADS-B). The shaded band is the clutter notch, where blah2 \
         doesn't look. Detections are kept for 15 minutes; only the statistics are kept longer.</p>",
    );
    Ok(page(&app, "/recent", "Detections vs ADS-B", &b).await)
}

fn site_rows(b: &mut String, s: &Setup) {
    let pair = s.pair();
    for (role, site) in [("Receiver", &s.rx), ("Transmitter (illuminator)", &s.tx)] {
        let _ = write!(
            b,
            "<tr><th>{role}</th><td>{} <span class=\"mute small\">{:.4}, {:.4}, {:.0} m</span></td></tr>",
            esc(if site.name.is_empty() {
                "(no name)"
            } else {
                &site.name
            }),
            site.latitude,
            site.longitude,
            site.altitude
        );
    }
    let w = &s.window;
    let _ = write!(
        b,
        "<tr><th>Baseline</th><td>{:.1} km</td></tr>\
         <tr><th>Illuminator frequency</th><td>{:.3} MHz (wavelength {:.2} m)</td></tr>\
         <tr><th>Radio</th><td>{} at {:.2} MHz sampling; one range bin is {:.2} km</td></tr>\
         <tr><th>CPI</th><td>{} s, so Doppler resolution is about {:.1} Hz</td></tr>\
         <tr><th>Search area</th><td>bistatic range {:.1} to {:.1} km, Doppler {} to {} Hz</td></tr>\
         <tr><th>Clutter notch</th><td>slower than {} Hz is ignored</td></tr>",
        pair.baseline() / 1000.0,
        s.fc / 1e6,
        pair.lambda,
        esc(&s.device),
        s.fs / 1e6,
        s.bin_km(),
        s.cpi_s,
        1.0 / s.cpi_s,
        w.min_km,
        w.max_km,
        w.dop_min,
        w.dop_max,
        w.notch_hz
    );
}

fn feeds_rows(app: &App, now: i64) -> String {
    let live = app.live();
    let mut b = String::new();
    let blah2 = match (&live.blah2_err, live.last_cpi) {
        (Some(e), _) => format!("{} {}", pill(Color::Red, "not answering"), esc(e)),
        (None, Some(t)) => format!(
            "{} last CPI {} ago",
            pill(Color::Green, "answering"),
            human_duration(now - t)
        ),
        (None, None) => pill(Color::Unknown, "no CPI yet"),
    };
    let _ = write!(
        b,
        "<tr><th>blah2</th><td>{}<br><span class=\"mute small\">{}</span></td></tr>",
        blah2,
        esc(&app.cfg.blah2.url)
    );
    if let (Some(t), Some(s)) = (&live.timing, &live.setup) {
        let cpi_ms = s.cpi_s * 1000.0;
        let verdict = if t.cpi_ms <= cpi_ms {
            "keeping up"
        } else {
            "falling behind: each CPI takes longer to process than to record"
        };
        let _ = write!(
            b,
            "<tr><th>Processing</th><td>{:.0} ms per {:.0} ms CPI, {verdict}. {} CPIs, up {}.</td></tr>",
            t.cpi_ms,
            cpi_ms,
            t.n_cpi,
            human_duration((t.uptime_s * 1000.0) as i64)
        );
    }
    let adsb = match (&live.adsb_err, live.adsb_ok) {
        (Some(e), _) => format!("{} {}", pill(Color::Red, "not answering"), esc(e)),
        (None, Some(_)) => format!(
            "{} {} aircraft with a current position",
            pill(Color::Green, "answering"),
            live.tracks.len()
        ),
        (None, None) => pill(Color::Unknown, "not read yet"),
    };
    let _ = write!(
        b,
        "<tr><th>ADS-B</th><td>{}<br><span class=\"mute small\">{}</span></td></tr>",
        adsb,
        esc(&app.cfg.adsb.url)
    );
    b
}

fn group_rows(b: &mut String, name: &str, g: &FixGroup) {
    let km = |x: Option<f64>| x.map_or("-".into(), |v| format!("{v:.1} km"));
    let _ = write!(
        b,
        "<tr><td>{name}</td><td>{}</td><td>{}</td><td>{} ({})</td><td>{}</td><td>{}</td><td>{}</td></tr>",
        g.fixes,
        g.scored,
        g.matched,
        pct(pd(g.matched, g.scored)),
        km(g.median_km),
        km(g.p90_km),
        g.dark_tracks,
    );
}

/// How well the multi-site fixes since `from` agree with ADS-B.
fn fusion_section(app: &App, from: i64) -> Result<String, Fail> {
    let f = fix_stats(&app.db(), from)?;
    let mut b = String::from("<h2>Located from two or more sites</h2>");
    if f.tracks == 0 {
        b.push_str(
            "<p class=mute>No aircraft located from two or more sites in this window. That takes a second \
             radar site on the mesh whose coverage overlaps this one's.</p>",
        );
        return Ok(b);
    }
    let _ = write!(
        b,
        "<p class=\"mute small\">Since {}. Each fix is a position worked out from the sites' detections alone, \
         then compared with the nearest ADS-B aircraft within {} km; the errors are among those matched \
         fixes only, so they can't exceed that. Fixes are kept for a week.</p>\
         <div class=wrap><table><tr><th>Seen by</th><th>Fixes</th><th>While ADS-B was current</th>\
         <th>Near an ADS-B aircraft</th><th>Median error</th><th>90% within</th><th>Tracks with no ADS-B</th></tr>",
        esc(&short_time(from)),
        app.cfg.fusion.truth_km
    );
    group_rows(&mut b, "two sites", &f.two);
    group_rows(&mut b, "three or more", &f.more);
    let _ = write!(
        b,
        "</table></div><p>{} fused tracks. The last column counts those followed for a minute or more \
         with no ADS-B aircraft near any fix. The project's goal is one aircraft without ADS-B tracked \
         from two or more sites, and these are the candidates, but a ghost looks the same: two sites' \
         tracks of different aircraft that happen to fit together, or one aircraft put at the wrong \
         crossing of two ellipses. Fixes where both crossings fit about equally are dropped; the rest \
         are scored here. Two-site fixes are much more prone to ghosts than three-site ones, and the \
         share near an ADS-B aircraft is the honest measure.</p>",
        f.tracks
    );
    Ok(b)
}

/// Whether a fix may be shown: only ones an ADS-B aircraft explains, unless the config
/// says otherwise, since the rest would locate aircraft public ADS-B doesn't show.
fn shown(app: &App, r: &crate::store::FixRow) -> bool {
    map::shown(app.cfg.fusion.show_unmatched, r)
}

fn clock(ms: i64) -> String {
    format!("{} UTC", &ms_to_rfc3339(ms)[11..19])
}

fn map_opts(app: &App) -> map::Opts {
    map::Opts {
        show_ids: app.cfg.stats.show_aircraft_ids,
        show_unmatched: app.cfg.fusion.show_unmatched,
        half_km: app.cfg.fusion.area_km,
    }
}

async fn map_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let mut b = heading("Map");
    b.push_str(&notes(&app, now));
    let view = app.live().view.clone();
    let Some(v) = view else {
        let why = if app.cfg.fusion.enabled {
            format!(
                "Nothing worked out yet. Each moment is worked out {} s after it, once every site's \
                 reports have arrived; this site's own reports are needed too, so blah2 must be answering.",
                app.cfg.fusion.settle_s
            )
        } else {
            "Locating aircraft is turned off (fusion.enabled in the config), so there is no map."
                .into()
        };
        let _ = write!(b, "<p class=mute>{}</p>", esc(&why));
        return Ok(page(&app, "/map", "Map", &b).await);
    };
    let _ = write!(
        b,
        "<p class=\"mute small\">As of {}, {} behind now: detection reports reach this site in batches, so the \
         map waits for every site's. Each thin ellipse is one site's track: the aircraft is somewhere on \
         it. Dots with dashed 1-sigma error ellipses are aircraft located from two or more sites; open \
         circles are ADS-B aircraft at the same moment{}.</p>",
        clock(v.ts),
        human_duration(now - v.ts),
        if v.truth.is_none() {
            " (none: the ADS-B feed was not current then)"
        } else {
            ""
        }
    );
    let _ = write!(
        b,
        "<div id=map>{}</div><script>\
         setInterval(function(){{fetch('/map.svg').then(function(r){{return r.ok?r.text():null}})\
         .then(function(t){{if(t)document.getElementById('map').innerHTML=t}})\
         .catch(function(){{}})}},10000)</script>",
        map::render(&v, &map_opts(&app))
    );
    if let Some(note) = map::left_off(&v, app.cfg.fusion.show_unmatched) {
        let _ = write!(b, "<p class=\"mute small\">{}</p>", esc(&note));
    }
    b.push_str(&fix_table(&app, &v));
    Ok(page(&app, "/map", "Map", &b).await)
}

fn fix_table(app: &App, v: &crate::hub::View) -> String {
    let mut b = String::from("<h2>Sites</h2><ul class=plain>");
    for s in &v.sites {
        let _ = write!(
            b,
            "<li>{}: {} confirmed tracks, {} with an ADS-B aircraft on them</li>",
            esc(&s.call),
            s.tracks.len(),
            s.tracks.iter().filter(|e| e.adsb).count()
        );
    }
    b.push_str("</ul>");
    if !v.fixes.iter().any(|r| shown(app, r)) {
        b.push_str(
            "<p class=mute>No aircraft to show located from two or more sites at this moment.</p>",
        );
        return b;
    }
    b.push_str(
        "<h2>Fixes</h2><div class=wrap><table><tr><th>Track</th><th>Seen by</th><th>Height</th>\
         <th>Speed</th><th>Error (1 sigma)</th><th>ADS-B</th></tr>",
    );
    for r in v.fixes.iter().filter(|r| shown(app, r)) {
        let f = &r.fix;
        let adsb = match &r.truth {
            Truth::Unknown => "not current".to_string(),
            Truth::Nothing => "none near".to_string(),
            Truth::Aircraft {
                hex,
                flight,
                err_km,
            } => {
                let who = if !app.cfg.stats.show_aircraft_ids {
                    "an aircraft"
                } else if flight.is_empty() {
                    hex.as_str()
                } else {
                    flight.as_str()
                };
                format!("{who}, {err_km:.1} km away")
            }
        };
        let _ = write!(
            b,
            "<tr><td>#{}</td><td>{}</td><td>{:.0} m, give or take {:.0}</td><td>{:.0} m/s</td>\
             <td>{:.1} by {:.1} km</td><td>{}</td></tr>",
            r.track,
            esc(&r.sites),
            f.alt_m,
            f.alt_sigma_m,
            f.ve.hypot(f.vn),
            f.major_km,
            f.minor_km,
            esc(&adsb)
        );
    }
    b.push_str("</table></div>");
    b
}

async fn map_svg(State(app): State<Arc<App>>) -> Response {
    let view = app.live().view.clone();
    match view {
        Some(v) => (
            [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
            map::render(&v, &map_opts(&app)),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "nothing worked out yet").into_response(),
    }
}

async fn map_json(State(app): State<Arc<App>>) -> Response {
    let Some(v) = app.live().view.clone() else {
        return (StatusCode::NOT_FOUND, "nothing worked out yet").into_response();
    };
    let (ids, show_unmatched) = (
        app.cfg.stats.show_aircraft_ids,
        app.cfg.fusion.show_unmatched,
    );
    Json(map::api(&v, ids, show_unmatched)).into_response()
}

fn sites_section(app: &App) -> Result<String, Fail> {
    let sites = crate::reports::sites(&app.db())?;
    let mut b = String::from("<h2>Radar sites on the mesh</h2>");
    if sites.is_empty() {
        b.push_str(
            "<p class=mute>No site has logged its geometry yet. Each site does so through its core once \
             blah2's config is in.</p>",
        );
        return Ok(b);
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Station</th><th>Receiver</th><th>Transmitter</th><th>Frequency</th>\
         <th>Baseline</th><th>Since</th></tr>",
    );
    for s in &sites {
        let su = &s.setup;
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{} <span class=\"mute small\">{:.3}, {:.3}</span></td><td>{}</td>\
             <td>{:.3} MHz</td><td>{:.1} km</td><td>{}</td></tr>",
            esc(&s.call),
            esc(&su.rx.name),
            su.rx.latitude,
            su.rx.longitude,
            esc(&su.tx.name),
            su.fc / 1e6,
            su.pair().baseline() / 1000.0,
            esc(&short_time(s.since))
        );
    }
    b.push_str(
        "</table></div><p class=\"mute small\">Locating aircraft from several sites assumes their clocks agree \
         to well under a second; run NTP or GPS time on every blah2 box.</p>",
    );
    Ok(b)
}

fn mesh_section(app: &App, now: i64) -> Result<String, Fail> {
    let mut b = String::from("<h2>Detection reports on the mesh</h2>");
    let m = &app.cfg.mesh;
    if m.enabled {
        let _ = write!(
            b,
            "<p>Every {} s this site sends its detections (bistatic range, Doppler, SNR and time, nothing \
             else) through the core as <code>{}</code> telemetry. No samples or audio ever leave the box. \
             The reports average at most {} kbit/s, signed events included ({:.1} kB a report); past \
             that, or past {} detections in one report, the weakest detections are left out.</p>",
            m.batch_s,
            crate::reports::KIND,
            m.max_kbps,
            crate::reports::bytes_allowed(m.batch_s, m.max_kbps) as f64 / 1000.0,
            m.max_per_batch
        );
        let last = app.live().last_report;
        if let Some((at, kept, left)) = last {
            let _ = write!(
                b,
                "<p>Last report, {}: {kept} detections kept, {left} left out.</p>",
                esc(&short_time(at))
            );
        }
    } else {
        b.push_str("<p>Sending detection reports is turned off (<code>mesh.enabled</code>).</p>");
    }
    let stations = crate::reports::stations(&app.db(), now - 24 * HOUR)?;
    if stations.is_empty() {
        b.push_str(
            "<p class=mute>No detection reports in the core's log in the last 24 hours.</p>",
        );
        return Ok(b);
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Station</th><th>Reports, 24 h</th><th>CPIs</th><th>Detections</th>\
         <th>Data</th><th>Per report</th><th>Last</th></tr>",
    );
    let unreadable: Vec<String> = stations
        .iter()
        .filter(|s| s.unreadable > 0)
        .map(|s| esc(&s.call))
        .collect();
    for s in stations {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1} kB</td><td>{:.1} kB</td><td>{}</td></tr>",
            esc(&s.call),
            s.reports,
            s.cpis,
            s.dets,
            s.bytes as f64 / 1000.0,
            s.bytes as f64 / 1000.0 / s.reports as f64,
            esc(&short_time(s.last))
        );
    }
    b.push_str(
        "</table></div><p class=\"mute small\">Sizes are the whole signed events, as they cross the \
         mesh.</p>",
    );
    if !unreadable.is_empty() {
        let _ = write!(
            b,
            "<p>Some reports from {} couldn't be read, so they count no CPIs or detections. They most \
             likely come from another version of this app with a different report format; upgrade \
             every site together.</p>",
            unreadable.join(", ")
        );
    }
    Ok(b)
}

async fn setup_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let mut b = heading("Setup");
    b.push_str(&notes(&app, now));
    b.push_str(
        "<p>A passive radar transmits nothing. One antenna listens to a broadcast transmitter (the \
         reference channel); a second, coherent channel listens for the same signal bouncing off aircraft \
         (the surveillance channel). <a href=\"https://github.com/30hours/blah2\">blah2</a> correlates the \
         two each CPI and reports peaks as detections: how much longer the echo's path was than the direct \
         one (bistatic range) and how fast that path is changing (Doppler). This app checks those \
         detections against the aircraft a nearby ADS-B receiver hears.</p>",
    );
    b.push_str("<h2>Geometry and processing</h2><div class=wrap><table>");
    let setup = app.live().setup.clone();
    match &setup {
        Some(s) => site_rows(&mut b, s),
        None => b.push_str("<tr><td class=mute>Waiting for blah2's config.</td></tr>"),
    }
    b.push_str("</table></div><p class=\"mute small\">From blah2's config.yml (location, capture and process).</p>");
    b.push_str("<h2>Feeds</h2><div class=wrap><table>");
    b.push_str(&feeds_rows(&app, now));
    let g = &app.cfg.gates;
    let _ = write!(
        b,
        "<tr><th>Matching</th><td>within {} km and {} Hz; ADS-B positions up to {} s old</td></tr></table></div>",
        g.range_gate_km, g.doppler_gate_hz, app.cfg.adsb.max_age_s
    );
    b.push_str(&mesh_section(&app, now)?);
    b.push_str(&sites_section(&app)?);
    b.push_str(
        "<h2>What it takes to make this real</h2><ul class=plain>\
         <li>A two-channel coherent receiver blah2 supports: an SDRplay RSPduo, or a KrakenSDR.</li>\
         <li>Two antennas: a Yagi pointed at the FM, DAB or TV transmitter, and one pointed at the sky you want to watch, ideally shielded from the transmitter.</li>\
         <li>A mini PC or similar running blah2 (a Pi 5 has not been shown to keep up; the Processing line above says whether this box does).</li>\
         <li>An ADS-B receiver (PiAware, dump1090-fa, or readsb) on the same network for the truth feed.</li>\
         <li>A site that sees the transmitter well for the reference channel without swamping the surveillance channel. Siting decides most of the result.</li>\
         </ul><p class=\"mute small\">Without that hardware, <code>ridgewarden-radar simulate</code> stands in for blah2 and the ADS-B receiver.</p>",
    );
    Ok(page(&app, "/setup", "Setup", &b).await)
}

async fn stats_json(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Fail> {
    let now = now_ms();
    let (active, hours_back) = window(&q);
    let (from_hour, _) = first_hour(&app, now, hours_back);
    let st = &app.cfg.stats;
    let s = summary(&app.db(), from_hour, st.bin_km)?;
    let (judged, seen) = aircraft_seen(&s.planes, st.min_opportunities, st.detected_hits);
    let planes: Vec<Value> = s
        .planes
        .iter()
        .enumerate()
        .map(|(n, p)| {
            let mut v = json!({"opportunities": p.opps, "hits": p.hits, "pd": pd(p.hits, p.opps),
                "min_km": p.min_km, "max_km": p.max_km});
            if st.show_aircraft_ids {
                v["hex"] = json!(p.hex);
                v["flight"] = json!(p.flight);
            } else {
                v["aircraft"] = json!(n + 1);
            }
            v
        })
        .collect();
    Ok(Json(json!({
        "experimental": true,
        "window": active,
        "from": from_hour * HOUR,
        "generated": now,
        "cpis": s.cpis,
        "detections": s.dets,
        "unmatched": s.unmatched,
        "opportunities": s.opps,
        "hits": s.hits,
        "pd": pd(s.hits, s.opps),
        "aircraft_judged": judged,
        "aircraft_detected": seen,
        "reliable_km": reliable_km(&s.bins, st.pd_threshold, st.min_opportunities),
        "pd_threshold": st.pd_threshold,
        "bins": s.bins.iter().map(|b| json!({"from_km": b.from, "to_km": b.to,
            "opportunities": b.opps, "hits": b.hits, "pd": b.pd()})).collect::<Vec<_>>(),
        "aircraft": planes,
        "multi_site": fix_json(&fix_stats(&app.db(), (from_hour * HOUR).max(now - KEEP_FIXES_MS))?),
    })))
}

fn fix_json(f: &crate::stats::FixStats) -> Value {
    let g = |g: &FixGroup| {
        json!({"fixes": g.fixes, "scored": g.scored, "matched": g.matched,
            "median_km": g.median_km, "p90_km": g.p90_km, "tracks_without_ads_b": g.dark_tracks})
    };
    json!({"two_sites": g(&f.two), "three_or_more": g(&f.more), "tracks": f.tracks})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_choice_falls_back_to_a_day() {
        let q = |v: &str| HashMap::from([("window".to_string(), v.to_string())]);
        assert_eq!(window(&q("7d")), ("7d", 168));
        assert_eq!(window(&q("<script>")), ("24h", 24));
        assert_eq!(window(&HashMap::new()), ("24h", 24));
        assert!(window_links("7d").contains("<b>7 days</b>"));
    }

    #[test]
    fn aircraft_ids_can_be_hidden() {
        assert_eq!(
            aircraft_name(true, 2, "a1b2c3", "TST101"),
            "TST101 (a1b2c3)"
        );
        assert_eq!(aircraft_name(true, 2, "a1b2c3", ""), "a1b2c3");
        assert_eq!(aircraft_name(false, 2, "a1b2c3", "TST101"), "aircraft 3");
    }
}
