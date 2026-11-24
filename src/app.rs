//! The running app: its config, its database, its line to the core, and what it last
//! heard from blah2 and the ADS-B receiver.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;

use crate::adsb::Track;
use crate::blah2::{Setup, Timing};
use crate::config::Config;
use crate::reports::{Batch, Reports};

pub const PROJECTIONS: &[&dyn Projection] = &[&Reports];
const HISTORY_EVERY_MS: i64 = 10_000;

/// The latest state of the two feeds, for the loops and the pages.
#[derive(Default)]
pub struct Live {
    pub setup: Option<Setup>,
    /// why blah2's config or timing couldn't be read
    pub setup_err: Option<String>,
    /// when the statistics for the current setup start, unix ms
    pub since: i64,
    pub timing: Option<Timing>,
    /// blah2's timestamp on the last CPI read
    pub last_cpi: Option<i64>,
    pub blah2_err: Option<String>,
    pub tracks: Vec<Track>,
    /// when an up-to-date aircraft.json was last read, by our clock
    pub adsb_ok: Option<i64>,
    pub adsb_err: Option<String>,
    pub mesh_err: Option<String>,
    /// when this site's last report went out, and how many detections it kept and left out
    pub last_report: Option<(i64, usize, usize)>,
    pub site_err: Option<String>,
    pub db_err: Option<String>,
    /// the last moment worked out from every site's reports, for the map
    pub view: Option<crate::hub::View>,
    pub hub_err: Option<String>,
}

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// for blah2 and the ADS-B receiver, both on the local network
    pub http: reqwest::Client,
    pub live: Mutex<Live>,
    pub batch: Mutex<Batch>,
    /// ADS-B lists from the last few minutes, oldest first, to score multi-site fixes
    /// that are worked out a while after the fact
    pub history: Mutex<VecDeque<(i64, Vec<Track>)>>,
    /// the setup fingerprint this site last logged as `radar.site`
    pub site_logged: Mutex<Option<String>>,
    who: Mutex<Option<Whoami>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let token = cfg
            .core
            .token_file
            .exists()
            .then_some(cfg.core.token_file.as_path());
        if token.is_none() && cfg.mesh.enabled {
            tracing::warn!(
                "core token {} not found; detection reports can't be sent",
                cfg.core.token_file.display()
            );
        }
        let core = CoreClient::new(&cfg.core.url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        crate::reports::migrate(&db)?;
        db.execute_batch(crate::store::SCHEMA)?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("ridgewarden-radar/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            cfg,
            core,
            db: Mutex::new(db),
            http,
            live: Mutex::new(Live::default()),
            batch: Mutex::new(Batch::default()),
            history: Mutex::new(VecDeque::new()),
            site_logged: Mutex::new(None),
            who: Mutex::new(None),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep an ADS-B list for later scoring, one every `HISTORY_EVERY_MS`, and only as
    /// far back as fixes are worked out.
    pub fn remember(&self, now: i64, tracks: &[Track]) {
        let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        if h.back().is_some_and(|(t, _)| now - t < HISTORY_EVERY_MS) {
            return;
        }
        h.push_back((now, tracks.to_vec()));
        let keep = (self.cfg.fusion.settle_s as i64 + 120) * 1000;
        while h.front().is_some_and(|(t, _)| now - t > keep) {
            h.pop_front();
        }
    }

    /// The ADS-B list nearest `t`, if one is within `HISTORY_EVERY_MS`.
    pub fn adsb_at(&self, t: i64) -> Option<Vec<Track>> {
        let h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        h.iter()
            .min_by_key(|(ts, _)| (ts - t).abs())
            .filter(|(ts, _)| (ts - t).abs() <= HISTORY_EVERY_MS)
            .map(|(_, l)| l.clone())
    }

    /// The core's identity, cached once it knows its node.
    pub async fn whoami(&self) -> Result<Whoami> {
        if let Some(w) = self.who.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && !w.node.is_empty()
        {
            return Ok(w);
        }
        let w = self.core.whoami().await?;
        *self.who.lock().unwrap_or_else(|e| e.into_inner()) = Some(w.clone());
        Ok(w)
    }

    pub async fn get(&self, url: &str) -> Result<String> {
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }
}

/// A short reason for a failed fetch, fit for a page.
pub fn short_err(e: &anyhow::Error) -> String {
    if let Some(r) = e.downcast_ref::<reqwest::Error>() {
        if r.is_timeout() {
            return "timed out".into();
        }
        if r.is_connect() {
            return "could not connect".into();
        }
        if let Some(s) = r.status() {
            return format!("HTTP {}", s.as_u16());
        }
    }
    format!("{e:#}").chars().take(160).collect()
}
