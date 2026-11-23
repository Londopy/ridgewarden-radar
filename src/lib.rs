//! ridgewarden-radar: an experimental passive radar for AREDN meshes.
//!
//! blah2 does the radar work (an RSPduo or KrakenSDR listening to an FM, DAB or TV
//! transmitter and its echoes). This app reads blah2's detections, checks every CPI
//! against the aircraft an ADS-B receiver reports, and keeps honest statistics of what the
//! site really detects. Detections, never signals, go to the mesh as telemetry, and with two
//! or more sites the app tracks and locates aircraft from everyone's reports.

pub mod adsb;
pub mod app;
pub mod blah2;
pub mod config;
pub mod fuse;
pub mod geo;
pub mod hub;
pub mod map;
pub mod plot;
pub mod reports;
pub mod sim;
pub mod stats;
pub mod store;
pub mod track;
pub mod truth;
pub mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use ridgewarden_client::util::now_ms;
use tokio::time::MissedTickBehavior;

use crate::app::{App, short_err};
use crate::truth::Gates;

/// Keep a feed's last error, logging only changes: a dead feed polled four times a
/// second would otherwise fill the log.
fn note(slot: &mut Option<String>, what: &str, err: Option<String>) {
    if *slot != err {
        match &err {
            Some(e) => tracing::warn!("{what}: {e}"),
            None => tracing::info!("{what}: working"),
        }
        *slot = err;
    }
}

async fn read_setup(app: &App, with_config: bool) -> Result<()> {
    let base = &app.cfg.blah2.url;
    if with_config {
        let setup = blah2::parse_config(&app.get(&format!("{base}/api/config")).await?)?;
        if !setup.detection {
            tracing::warn!(
                "blah2 has process.detection.enable = false; there will be nothing to check"
            );
        }
        let since = store::setup_since(&app.db(), &setup.fingerprint(), now_ms())?;
        let mut live = app.live();
        if live.setup.as_ref().is_some_and(|s| *s != setup) {
            tracing::info!("blah2's setup changed; statistics start again from the next hour");
        }
        live.setup = Some(setup);
        live.since = since;
    }
    let timing = blah2::parse_timing(&app.get(&format!("{base}/api/timing")).await?)?;
    app.live().timing = Some(timing);
    Ok(())
}

async fn read_adsb(app: &App) -> Result<()> {
    let body = app.get(&app.cfg.adsb.url).await?;
    let list = adsb::parse(&body, app.cfg.adsb.max_age_s as f64)?;
    // a dead receiver can leave its web server handing out the last file it wrote
    let now = now_ms();
    let off = (now - list.now) as f64 / 1000.0;
    if off.abs() > app.cfg.adsb.max_age_s as f64 / 2.0 {
        bail!(
            "aircraft.json is {off:.0} s out of step with this box's clock: the receiver has \
             stopped updating it, or one of the clocks is wrong"
        );
    }
    app.remember(now, &list.tracks);
    let mut live = app.live();
    live.tracks = list.tracks;
    live.adsb_ok = Some(now);
    Ok(())
}

/// Whether a CPI can be judged: the ADS-B list is current and blah2's clock agrees with
/// ours. Otherwise every detection would look like a false alarm, or match the wrong place.
fn judgeable(adsb_ok: Option<i64>, cpi_ts: i64, now: i64, max_age_ms: i64) -> bool {
    adsb_ok.is_some_and(|t| now - t <= max_age_ms) && (cpi_ts - now).abs() <= max_age_ms
}

/// Read blah2's latest CPI and, if it is new, match and record it. `last_truth` is when
/// expected positions were last stored for the plot.
async fn read_cpi(app: &App, last_truth: &mut i64) -> Result<()> {
    let body = app
        .get(&format!("{}/api/detection", app.cfg.blah2.url))
        .await?;
    let Some(frame) = blah2::parse_detection(&body)? else {
        return Ok(());
    };
    let max_age_ms = app.cfg.adsb.max_age_s as i64 * 1000;
    let (setup, tracks, judge) = {
        let mut live = app.live();
        if live.last_cpi == Some(frame.ts) {
            return Ok(());
        }
        live.last_cpi = Some(frame.ts);
        let Some(setup) = live.setup.clone() else {
            return Ok(());
        };
        let judge = judgeable(live.adsb_ok, frame.ts, now_ms(), max_age_ms);
        (setup, live.tracks.clone(), judge)
    };
    if app.cfg.mesh.enabled {
        app.batch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(&frame);
    }
    if !judge {
        return Ok(());
    }
    let gates = Gates {
        range_km: app.cfg.gates.range_gate_km,
        doppler_hz: app.cfg.gates.doppler_gate_hz,
        max_age_ms,
    };
    let o = truth::match_frame(&setup.pair(), &setup.window, &gates, &tracks, &frame);
    let with_truth = frame.ts - *last_truth >= store::TRUTH_EVERY_MS;
    if with_truth {
        *last_truth = frame.ts;
    }
    let err = store::record(&mut app.db(), &frame, &o, with_truth)
        .err()
        .map(|e| format!("{e:#}"));
    note(&mut app.live().db_err, "recording CPIs", err);
    Ok(())
}

async fn send_report(app: &App) -> Result<()> {
    let fc = app.live().setup.as_ref().map(|s| s.fc);
    let mesh = &app.cfg.mesh;
    let payload = fc.and_then(|fc| {
        app.batch.lock().unwrap_or_else(|e| e.into_inner()).take(
            fc,
            mesh.max_per_batch,
            reports::budget(mesh.batch_s, mesh.max_kbps),
        )
    });
    if let Some(p) = payload {
        let (kept, left) = reports::kept_and_left_out(&p);
        app.core.append_telemetry(reports::KIND, p).await?;
        app.live().last_report = Some((now_ms(), kept, left));
    }
    Ok(())
}

/// Log this site's geometry as `radar.site` when the mesh hasn't got the current one yet,
/// so other sites can use its reports. Runs after following the feed, which is where an
/// earlier one would show up.
async fn log_site(app: &App) -> Result<()> {
    let Some(setup) = app.live().setup.clone() else {
        return Ok(());
    };
    let fp = setup.fingerprint();
    let sent = app
        .site_logged
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if sent.as_deref() == Some(fp.as_str()) {
        return Ok(());
    }
    let key = app.whoami().await?.key;
    let logged = reports::logged_fingerprint(&app.db(), &key)?;
    if logged.as_deref() != Some(fp.as_str()) {
        app.core
            .append(reports::SITE_KIND, reports::site_payload(&setup))
            .await?;
        tracing::info!("logged this site's geometry as {}", reports::SITE_KIND);
    }
    *app.site_logged.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
    Ok(())
}

/// Where fused tracks stand between rounds.
struct HubState {
    next: i64,
    prev: Vec<(u64, fuse::Fix)>,
    last_track: u64,
}

/// Work out every moment that has settled since the last round.
async fn hub_round(app: &Arc<App>, st: &mut HubState) -> Result<()> {
    let every = app.cfg.fusion.every_s as i64 * 1000;
    let due = (now_ms() - app.cfg.fusion.settle_s as i64 * 1000).div_euclid(every) * every;
    // after a long stop, start from now rather than replaying hours of moments
    if st.next == 0 || due - st.next > 10 * every {
        st.next = due;
    }
    while st.next <= due {
        let t = st.next;
        st.next += every;
        hub_moment(app, st, t).await?;
    }
    Ok(())
}

async fn hub_moment(app: &Arc<App>, st: &mut HubState, t: i64) -> Result<()> {
    let cfg = app.cfg.hub();
    let (sites, frames) = {
        let db = app.db();
        let sites = reports::sites(&db)?;
        let frames = sites
            .iter()
            .map(|s| reports::frames(&db, &s.key, s.since.max(t - cfg.history_ms), t))
            .collect::<Result<Vec<_>>>()?;
        (sites, frames)
    };
    let Some(first) = sites.first() else {
        return Ok(());
    };
    let rx = app
        .live()
        .setup
        .as_ref()
        .map_or(first.setup.rx.clone(), |s| s.rx.clone());
    let plane = fuse::Plane {
        lat0: rx.latitude,
        lon0: rx.longitude,
    };
    let calls: Vec<String> = sites.iter().map(|s| s.call.clone()).collect();
    let (mut views, fixes) =
        tokio::task::spawn_blocking(move || hub::epoch(&sites, &frames, plane, &cfg, t)).await?;
    let truth = app.adsb_at(t);
    let g = &app.cfg.gates;
    hub::explain(
        &mut views,
        truth.as_deref(),
        g.range_gate_km,
        g.doppler_gate_hz,
        t,
    );
    let ids = hub::link(&st.prev, &fixes, &mut st.last_track);
    let rows: Vec<store::FixRow> = fixes
        .into_iter()
        .zip(ids)
        .map(|(fix, track)| store::FixRow {
            track,
            sites: fix
                .sites
                .iter()
                .map(|i| calls[*i].as_str())
                .collect::<Vec<_>>()
                .join(", "),
            truth: hub::score(&fix, truth.as_deref(), app.cfg.fusion.truth_km),
            fix,
        })
        .collect();
    store::record_fixes(&mut app.db(), &rows)?;
    let now: Vec<(u64, fuse::Fix)> = rows.iter().map(|r| (r.track, r.fix.clone())).collect();
    st.prev = hub::carry(std::mem::take(&mut st.prev), now, t);
    app.live().view = Some(hub::View {
        ts: t,
        plane,
        sites: views,
        fixes: rows,
        truth,
    });
    Ok(())
}

fn prune(app: &App) -> Result<()> {
    let db = app.db();
    let now = now_ms();
    store::prune(&db, now)?;
    reports::prune(&db, now)
}

/// Follow the core, read blah2 and the ADS-B feed, and send reports, forever.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            match ridgewarden_client::follow(&a.core, &a.db, app::PROJECTIONS).await {
                Ok(_) if a.cfg.mesh.enabled => {
                    let err = log_site(&a).await.err().map(|e| short_err(&e));
                    note(&mut a.live().site_err, "logging this site", err);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("following the core: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    if app.cfg.fusion.enabled {
        let a = app.clone();
        tokio::spawn(async move {
            let last_track = match store::last_track(&a.db()) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(
                        "reading the last fused-track id, so not locating aircraft: {e:#}"
                    );
                    return;
                }
            };
            let mut st = HubState {
                next: 0,
                prev: vec![],
                last_track,
            };
            loop {
                let err = hub_round(&a, &mut st).await.err().map(|e| format!("{e:#}"));
                note(&mut a.live().hub_err, "locating aircraft", err);
                tokio::time::sleep(Duration::from_secs(a.cfg.fusion.every_s)).await;
            }
        });
    }
    let a = app.clone();
    tokio::spawn(async move {
        let mut n = 0u64;
        loop {
            // the config rarely changes; reread it every 5 minutes, or until it is in
            let with_config = n.is_multiple_of(10) || a.live().setup.is_none();
            n += 1;
            let err = read_setup(&a, with_config)
                .await
                .err()
                .map(|e| short_err(&e));
            note(&mut a.live().setup_err, "blah2 config", err);
            if let Err(e) = prune(&a) {
                tracing::warn!("pruning old data: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(a.cfg.adsb.poll_s));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let err = read_adsb(&a).await.err().map(|e| short_err(&e));
            note(&mut a.live().adsb_err, "ADS-B feed", err);
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(a.cfg.blah2.poll_ms));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_truth = 0;
        loop {
            tick.tick().await;
            let err = read_cpi(&a, &mut last_truth)
                .await
                .err()
                .map(|e| short_err(&e));
            note(&mut a.live().blah2_err, "blah2", err);
        }
    });
    if app.cfg.mesh.enabled {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(app.cfg.mesh.batch_s));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let err = send_report(&app).await.err().map(|e| short_err(&e));
                note(&mut app.live().mesh_err, "detection reports", err);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpis_are_judged_only_against_current_truth() {
        let now = 1_000_000;
        assert!(judgeable(Some(now - 2_000), now - 300, now, 10_000));
        assert!(!judgeable(None, now, now, 10_000), "never read");
        assert!(
            !judgeable(Some(now - 11_000), now, now, 10_000),
            "feed went quiet"
        );
        assert!(
            !judgeable(Some(now), now + 15_000, now, 10_000),
            "blah2 clock ahead"
        );
        assert!(
            !judgeable(Some(now), now - 15_000, now, 10_000),
            "blah2 clock behind"
        );
    }
}
