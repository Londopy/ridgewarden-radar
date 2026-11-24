//! `ridgewarden-radar top`: this site and the mesh's radar sites, live in a terminal, for
//! an operator on a headless box over SSH. It reads the app's own JSON API (`/api/setup`,
//! `/api/map`, and `/api/stats` while a page shows the statistics), so it shows what the
//! pages show and needs neither the config nor the database.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use ridgewarden_client::tui::ratatui::Frame;
use ridgewarden_client::tui::ratatui::crossterm::event::{KeyCode, KeyEvent};
use ridgewarden_client::tui::ratatui::layout::{Constraint, Layout, Rect};
use ridgewarden_client::tui::ratatui::text::{Line, Span};
use ridgewarden_client::tui::ratatui::widgets::{Cell, Paragraph, TableState, Wrap};
use ridgewarden_client::tui::{
    self, Chrome, Fetch, Notice, Pill, Refresh, Theme, View, move_key, pill_table, tab_key,
};
use ridgewarden_client::util::{human_duration, now_ms, short_time};
use serde::Deserialize;

use crate::web::{WINDOWS, clock, pct};

const TITLE: &str = "ridgewarden-radar";
const EXPERIMENTAL: &str = " (experimental)";
const TABS: [&str; 5] = ["Sites", "Tracks", "Fixes", "Stats", "Aircraft"];
const HELP: &str =
    "q quit  1-5 Tab pages  Up Down PgUp PgDn move  Enter detail  w window  r reload";
const HELP_DETAIL: &str = "q quit  Esc back  Up Down next one  1-5 Tab pages  r reload";

/// Run `top` against the app at `base`, e.g. http://127.0.0.1:8802, until the user quits,
/// reading everything again every `every`.
pub fn run(base: &str, every: Duration) -> Result<()> {
    if !base.starts_with("http://") {
        bail!("--url must be an http:// address, like http://127.0.0.1:8802");
    }
    let ask = Arc::new(Ask {
        // 24 hours, as the performance page starts
        window: AtomicUsize::new(1),
        stats: AtomicBool::new(false),
    });
    let loader = Loader {
        base: base.trim_end_matches('/').to_string(),
        fetch: Fetch::new(Duration::from_secs(5))?,
        ask: ask.clone(),
    };
    let refresh = Refresh::spawn(every, async move || loader.load().await)?;
    tui::run(&mut Top::new(refresh, ask), Duration::from_millis(250))?;
    Ok(())
}

#[derive(Deserialize, Clone, Debug)]
struct Who {
    call: String,
    node: String,
}

/// A receiver or transmitter: other stations' come rounded or by name only, as the setup
/// page shows them.
#[derive(Deserialize, Clone, Debug)]
struct Place {
    name: String,
    latitude: Option<f64>,
    longitude: Option<f64>,
    altitude: Option<f64>,
}

/// A site's geometry, from its blah2 config.
#[derive(Deserialize, Clone, Debug)]
struct Geometry {
    fc_hz: f64,
    cpi_s: f64,
    rx: Place,
    tx: Place,
    baseline_km: f64,
    /// when another station logged it
    since: Option<i64>,
}

#[derive(Deserialize, Clone, Debug)]
struct Blah2 {
    url: String,
    error: Option<String>,
    config_error: Option<String>,
    last_cpi: Option<i64>,
}

#[derive(Deserialize, Clone, Debug)]
struct Timing {
    cpi_ms: f64,
}

#[derive(Deserialize, Clone, Debug)]
struct Adsb {
    url: String,
    error: Option<String>,
    last_ok: Option<i64>,
    aircraft: usize,
    max_age_s: i64,
}

#[derive(Deserialize, Clone, Debug)]
struct LastReport {
    at: i64,
    kept: i64,
    left_out: i64,
}

#[derive(Deserialize, Clone, Debug)]
struct Mesh {
    enabled: bool,
    batch_s: i64,
    max_kbps: f64,
    bytes_allowed: i64,
    max_per_batch: i64,
    error: Option<String>,
    last_report: Option<LastReport>,
}

#[derive(Deserialize, Clone, Debug)]
struct Fusion {
    enabled: bool,
    settle_s: i64,
}

/// A radar station heard on the mesh, with its reports in the last 24 hours.
#[derive(Deserialize, Clone, Debug)]
struct Station {
    call: String,
    here: bool,
    reports: i64,
    cpis: i64,
    detections: i64,
    left_out: i64,
    bytes: i64,
    unreadable: i64,
    last: Option<i64>,
    every_s: Option<f64>,
    kbps: Option<f64>,
    site: Option<Geometry>,
}

/// `/api/setup`
#[derive(Deserialize, Clone, Debug)]
struct SetupInfo {
    /// the app's clock when it answered
    generated: i64,
    who: Option<Who>,
    notes: Vec<String>,
    site: Option<Geometry>,
    blah2: Blah2,
    timing: Option<Timing>,
    adsb: Adsb,
    mesh: Mesh,
    fusion: Fusion,
    stations: Vec<Station>,
}

#[derive(Deserialize, Clone, Debug)]
struct Track {
    id: u32,
    range_km: f64,
    doppler_hz: f64,
    adsb: bool,
}

#[derive(Deserialize, Clone, Debug)]
struct SiteTracks {
    call: String,
    confirmed: usize,
    with_adsb: usize,
    tracks: Vec<Track>,
}

#[derive(Deserialize, Clone, Debug)]
struct FixAdsb {
    near: bool,
    hex: Option<String>,
    flight: Option<String>,
    err_km: Option<f64>,
}

#[derive(Deserialize, Clone, Debug)]
struct Fix {
    track: u64,
    sites: String,
    lat: f64,
    lon: f64,
    alt_m: f64,
    alt_sigma_m: f64,
    ve: f64,
    vn: f64,
    major_km: f64,
    minor_km: f64,
    angle_deg: f64,
    chi2: f64,
    /// None when there was no current ADS-B to check it against
    adsb: Option<FixAdsb>,
}

#[derive(Deserialize, Clone, Debug)]
struct LeftOff {
    tracks: usize,
    fixes: usize,
}

/// `/api/map`: one moment worked out from every site's reports.
#[derive(Deserialize, Clone, Debug)]
struct MapInfo {
    ts: i64,
    /// ADS-B aircraft then, or None if the feed wasn't current
    adsb: Option<usize>,
    left_off: LeftOff,
    sites: Vec<SiteTracks>,
    fixes: Vec<Fix>,
}

#[derive(Deserialize, Clone, Debug)]
struct Bin {
    from_km: i64,
    to_km: i64,
    opportunities: i64,
    hits: i64,
    pd: Option<f64>,
}

/// An aircraft with its ids, or only a number when the app hides them.
#[derive(Deserialize, Clone, Debug)]
struct Aircraft {
    hex: Option<String>,
    flight: Option<String>,
    aircraft: Option<usize>,
    opportunities: i64,
    hits: i64,
    pd: Option<f64>,
    min_km: f64,
    max_km: f64,
}

#[derive(Deserialize, Clone, Debug)]
struct Group {
    fixes: i64,
    scored: i64,
    matched: i64,
    median_km: Option<f64>,
}

#[derive(Deserialize, Clone, Debug)]
struct MultiSite {
    two_sites: Group,
    three_or_more: Group,
}

/// `/api/stats`
#[derive(Deserialize, Clone, Debug)]
struct StatsInfo {
    window: String,
    from: i64,
    setup_changed: Option<i64>,
    cpis: i64,
    detections: i64,
    unmatched: i64,
    unmatched_per_cpi: Option<f64>,
    unmatched_per_min: Option<f64>,
    chance_pd: Option<f64>,
    opportunities: i64,
    pd: Option<f64>,
    aircraft_judged: i64,
    aircraft_detected: i64,
    detected_hits: i64,
    min_opportunities: i64,
    reliable_km: Option<i64>,
    pd_threshold: f64,
    bins: Vec<Bin>,
    /// those with the most time in coverage, of `aircraft_total`
    aircraft: Vec<Aircraft>,
    aircraft_total: usize,
    multi_site: MultiSite,
}

/// What the view wants read, shared with the loader.
struct Ask {
    /// index into WINDOWS
    window: AtomicUsize,
    /// a page that shows the statistics is open
    stats: AtomicBool,
}

/// One round of reading: each endpoint's answer, or why it failed.
struct Read {
    /// when it was read, by this box's clock
    at: i64,
    setup: Result<SetupInfo, String>,
    /// Ok(None) until the app has worked out a moment
    map: Result<Option<MapInfo>, String>,
    /// None when no page showing needs the statistics
    stats: Option<Result<StatsInfo, String>>,
}

/// The latest good answer from each endpoint.
struct Snap {
    /// when the setup was read, by this box's clock
    at: i64,
    setup: SetupInfo,
    /// None until the app has worked out a moment from the sites' reports
    map: Option<MapInfo>,
    /// None until a page that shows them asks
    stats: Option<StatsInfo>,
}

impl Snap {
    /// The app's clock now: its time when it answered, plus what has passed here since,
    /// so a box with its clock off still shows the right ages.
    fn now(&self, here: i64) -> i64 {
        self.setup.generated + (here - self.at)
    }
}

/// Why the latest read of each endpoint failed, if it did.
#[derive(Default)]
struct Failed {
    setup: Option<String>,
    map: Option<String>,
    stats: Option<String>,
}

/// Why the latest read of what page `tab` shows failed, if it did.
fn failure(f: &Failed, tab: usize) -> Option<&str> {
    let part = match tab {
        0 => None,
        1 | 2 => f.map.as_deref(),
        _ => f.stats.as_deref(),
    };
    f.setup.as_deref().or(part)
}

struct Loader {
    base: String,
    fetch: Fetch,
    ask: Arc<Ask>,
}

impl Loader {
    async fn load(&self) -> Read {
        let b = &self.base;
        let w = WINDOWS[self.ask.window.load(Ordering::Relaxed)].0;
        let urls = [
            format!("{b}/api/setup"),
            format!("{b}/api/map"),
            format!("{b}/api/stats?window={w}"),
        ];
        let stats = async {
            if self.ask.stats.load(Ordering::Relaxed) {
                Some(self.fetch.json::<StatsInfo>(&urls[2]).await)
            } else {
                None
            }
        };
        let (setup, map, stats) = tokio::join!(
            self.fetch.json::<SetupInfo>(&urls[0]),
            self.fetch.json::<MapInfo>(&urls[1]),
            stats,
        );
        Read {
            at: now_ms(),
            setup: setup.map_err(|e| {
                if not_found(&e) {
                    // /api/setup came with this version of the app
                    format!(
                        "{e}: no /api/setup there, so it is an older ridgewarden-radar to upgrade, or not ridgewarden-radar (check --url)"
                    )
                } else if http(&e).is_some_and(reqwest::Error::is_connect) {
                    format!(
                        "{e}: if the app isn't running, start it with sudo systemctl start ridgewarden-radar; if it runs elsewhere, give its address with --url"
                    )
                } else {
                    e.to_string()
                }
            }),
            map: match map {
                Ok(m) => Ok(Some(m)),
                Err(e) if not_found(&e) => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            stats: stats.map(|r| r.map_err(|e| e.to_string())),
        }
    }
}

fn http(e: &anyhow::Error) -> Option<&reqwest::Error> {
    e.downcast_ref::<reqwest::Error>()
}

/// The app answers /api/map with a 404 until it has worked out a moment.
fn not_found(e: &anyhow::Error) -> bool {
    http(e).and_then(reqwest::Error::status) == Some(reqwest::StatusCode::NOT_FOUND)
}

/// The station or fix Enter opened, by callsign or fused-track id, so the detail stays on
/// the same one while the rows move between reads.
#[derive(Clone, Debug, PartialEq)]
enum Detail {
    Site(String),
    Fix(u64),
}

struct Top {
    theme: Theme,
    refresh: Refresh<Read>,
    ask: Arc<Ask>,
    snap: Option<Snap>,
    failed: Failed,
    tab: usize,
    /// each page's selected row
    rows: [TableState; TABS.len()],
    detail: Option<Detail>,
    /// rows a page had room for at the last draw, for PgUp and PgDn
    page: usize,
}

impl Top {
    fn new(refresh: Refresh<Read>, ask: Arc<Ask>) -> Self {
        Self {
            theme: Theme::from_env(),
            refresh,
            ask,
            snap: None,
            failed: Failed::default(),
            tab: 0,
            rows: Default::default(),
            detail: None,
            page: 0,
        }
    }

    /// Keep what each endpoint answered, and the last good answer of any that failed.
    fn take(&mut self, r: Read) {
        let mut failed = Failed::default();
        match r.setup {
            Ok(setup) => match &mut self.snap {
                Some(s) => (s.at, s.setup) = (r.at, setup),
                None => {
                    self.snap = Some(Snap {
                        at: r.at,
                        setup,
                        map: None,
                        stats: None,
                    });
                }
            },
            Err(e) => failed.setup = Some(e),
        }
        if let Some(s) = &mut self.snap {
            match r.map {
                Ok(m) => s.map = m,
                Err(e) => failed.map = Some(e),
            }
            match r.stats {
                Some(Ok(st)) => s.stats = Some(st),
                Some(Err(e)) => failed.stats = Some(e),
                // not asked for: no page shows them, and old ones would mislead later
                None => s.stats = None,
            }
        }
        self.failed = failed;
    }

    /// Rows in a page's table.
    fn len(&self, tab: usize) -> usize {
        let Some(s) = &self.snap else { return 0 };
        let (map, stats) = (s.map.as_ref(), s.stats.as_ref());
        match tab {
            0 => s.setup.stations.len(),
            1 => map.map_or(0, |m| m.sites.iter().map(|x| x.tracks.len()).sum()),
            2 => map.map_or(0, |m| m.fixes.len()),
            3 => stats.map_or(0, |st| st.bins.len()),
            _ => stats.map_or(0, |st| st.aircraft.len()),
        }
    }

    /// The pages where Enter shows more about a row: sites and fixes.
    fn has_detail(&self) -> bool {
        matches!(self.tab, 0 | 2)
    }

    /// The station or fix on this page's selected row.
    fn pick(&self) -> Option<Detail> {
        let s = self.snap.as_ref()?;
        let i = self.rows[self.tab].selected()?;
        match self.tab {
            0 => station_list(&s.setup)
                .get(i)
                .map(|st| Detail::Site(st.call.clone())),
            2 => s.map.as_ref()?.fixes.get(i).map(|x| Detail::Fix(x.track)),
            _ => None,
        }
    }

    /// Show page `t`, reading the statistics only while a page that shows them is open.
    fn go(&mut self, t: usize) {
        let wants = matches!(t, 3 | 4);
        if self.ask.stats.swap(wants, Ordering::Relaxed) != wants && wants {
            self.refresh.again();
        }
        self.tab = t;
        self.detail = None;
    }

    fn status(&self, here: i64) -> Span<'static> {
        let Some(s) = &self.snap else {
            return match self.failed.setup {
                Some(_) => Span::styled("app not answering", self.theme.bad()),
                None => Span::styled("loading", self.theme.mute()),
            };
        };
        let age = format!("updated {} ago", human_duration(here - s.at));
        let warnings = s.setup.notes.len();
        match (&self.failed.setup, warnings) {
            (Some(_), _) => Span::styled(format!("app not answering, {age}"), self.theme.bad()),
            (None, 0) => Span::styled(age, self.theme.mute()),
            (None, 1) => Span::styled(format!("1 warning, {age}"), self.theme.bad()),
            (None, n) => Span::styled(format!("{n} warnings, {age}"), self.theme.bad()),
        }
    }

    fn draw_at(&mut self, f: &mut Frame, here: i64) {
        let mut title = match self.snap.as_ref().and_then(|s| s.setup.who.as_ref()) {
            Some(w) if !w.node.is_empty() => format!("{TITLE}  {} at {}", w.call, w.node),
            Some(w) => format!("{TITLE}  {}", w.call),
            None => TITLE.into(),
        };
        let status = self.status(here);
        // as the pages' headings say, while there is room for it
        if title.len() + EXPERIMENTAL.len() + status.width() < f.area().width as usize {
            title.push_str(EXPERIMENTAL);
        }
        let chrome = Chrome {
            title: &title,
            status,
            tabs: &TABS,
            tab: self.tab,
            help: if self.detail.is_some() {
                HELP_DETAIL
            } else {
                HELP
            },
        };
        let Some(mut area) = chrome.draw(f, &self.theme) else {
            return;
        };
        let theme = &self.theme;
        let Some(s) = &self.snap else {
            let msg;
            let n = match &self.failed.setup {
                Some(e) => {
                    msg = format!("Can't read the app: {e}.");
                    Notice::Error(&msg)
                }
                None => Notice::Loading,
            };
            f.render_widget(n.paragraph(theme), area);
            return;
        };
        let failed = failure(&self.failed, self.tab);
        // a page with nothing from an earlier read says why in its place instead
        let earlier = match self.tab {
            0 => true,
            1 | 2 => s.map.is_some(),
            _ => s.stats.is_some(),
        };
        if let Some(e) = failed.filter(|_| earlier) {
            let msg = format!("Reading failed, so this is from earlier: {e}");
            let rows = wrapped_rows(&msg, area.width.into()).min(3) as u16;
            let [banner, rest] =
                Layout::vertical([Constraint::Length(rows), Constraint::Fill(1)]).areas(area);
            let text = Paragraph::new(Line::styled(msg, theme.bad())).wrap(Wrap { trim: true });
            f.render_widget(text, banner);
            area = rest;
        }
        // the header and the lines above most tables
        self.page = area.height.saturating_sub(6).into();
        let c = Ctx {
            theme,
            s,
            now: s.now(here),
            window: self.ask.window.load(Ordering::Relaxed),
            failed: failed.filter(|_| !earlier),
        };
        let rows = &mut self.rows[self.tab];
        match (self.tab, &self.detail) {
            (0, Some(Detail::Site(call))) => site_detail(&c, f, area, rows, call),
            (0, _) => sites(&c, f, area, rows),
            (1, _) => tracks(&c, f, area, rows),
            (2, Some(Detail::Fix(id))) => one_fix(&c, f, area, rows, *id),
            (2, _) => fixes(&c, f, area, rows),
            (3, _) => stats(&c, f, area, rows),
            _ => aircraft(&c, f, area, rows),
        }
    }
}

impl View for Top {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(read) = self.refresh.latest()? {
            self.take(read);
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if let Some(t) = tab_key(&k, self.tab, TABS.len()) {
            self.go(t);
            return true;
        }
        match k.code {
            KeyCode::Char('r') => self.refresh.again(),
            KeyCode::Char('w') => {
                let next = (self.ask.window.load(Ordering::Relaxed) + 1) % WINDOWS.len();
                self.ask.window.store(next, Ordering::Relaxed);
                self.refresh.again();
            }
            KeyCode::Enter if self.has_detail() => {
                if self.detail.take().is_none() {
                    let len = self.len(self.tab);
                    let rows = &mut self.rows[self.tab];
                    if rows.selected().is_none() && len > 0 {
                        rows.select(Some(0));
                    }
                    self.detail = self.pick();
                }
            }
            KeyCode::Esc => self.detail = None,
            _ => {
                let len = self.len(self.tab);
                let moved = move_key(&mut self.rows[self.tab], &k, len, self.page);
                // an open detail moves on with the selection
                if moved && self.detail.is_some() {
                    self.detail = self.pick();
                }
            }
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        self.draw_at(f, now_ms());
    }
}

/// What every page draws from.
struct Ctx<'a> {
    theme: &'a Theme,
    s: &'a Snap,
    /// the app's clock
    now: i64,
    /// the statistics window asked for, which may not have been read yet
    window: usize,
    /// why reading this page's data failed, when there is nothing earlier to show
    failed: Option<&'a str>,
}

/// "120 m", or "1.4 km" from a kilometre on.
fn dist(km: f64) -> String {
    if km < 1.0 {
        format!("{:.0} m", km * 1000.0)
    } else {
        format!("{km:.1} km")
    }
}

fn ago(now: i64, t: i64) -> String {
    format!("{} ago", human_duration(now - t))
}

/// "9999", or "40.2k" and "1.2M" from ten thousand on.
fn count(n: i64) -> String {
    match n {
        ..10_000 => n.to_string(),
        10_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Detections left out, and their share of all there were: "40.2k 77%".
fn left_out(left: i64, sent: i64) -> String {
    if left == 0 {
        return "0".into();
    }
    format!(
        "{} {}",
        count(left),
        pct(Some(left as f64 / (left + sent) as f64))
    )
}

/// Lines of text over a page's table, wrapped, with a blank line under them; returns the
/// area left for the table.
fn above(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> Rect {
    let width = area.width as usize;
    let rows: usize = lines
        .iter()
        .map(|l| wrapped_rows(&l.to_string(), width))
        .sum();
    let [head, rest] =
        Layout::vertical([Constraint::Length(rows as u16 + 1), Constraint::Fill(1)]).areas(area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), head);
    rest
}

/// Rows `text` takes when wrapped at spaces to `width` columns.
fn wrapped_rows(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let (mut rows, mut at) = (1, 0);
    for w in text.split_whitespace() {
        let n = w.chars().count();
        if at > 0 && at + 1 + n > width {
            rows += 1;
            at = 0;
        }
        at += n + usize::from(at > 0);
        // a word longer than a whole row
        while at > width {
            rows += 1;
            at -= width;
        }
    }
    rows
}

/// A page whose data has never come: why, if reading it failed, or that it is on its way.
fn waiting(c: &Ctx, f: &mut Frame, area: Rect) {
    let n = c.failed.map_or(Notice::Loading, Notice::Error);
    f.render_widget(n.paragraph(c.theme), area);
}

/// A page's whole area saying one thing, e.g. that what a detail was open on has gone.
fn say(c: &Ctx, f: &mut Frame, area: Rect, what: &str) {
    f.render_widget(Notice::Empty(what).paragraph(c.theme), area);
}

/// How blah2 is doing, for the row on the Sites page.
fn blah2_state(c: &Ctx) -> (Option<Pill>, String) {
    let s = &c.s.setup;
    let b = &s.blah2;
    // a config read failing later doesn't matter while the one before still holds
    let failed = b
        .error
        .as_ref()
        .or(b.config_error.as_ref().filter(|_| s.site.is_none()));
    if let Some(e) = failed {
        // the error first, so a narrow terminal cuts the address rather than the reason
        return (Some(Pill::Down), format!("not answering: {e} ({})", b.url));
    }
    let Some(t) = b.last_cpi else {
        let what = if s.site.is_none() {
            "waiting for its config"
        } else {
            "no CPI yet"
        };
        return (None, format!("{what} from {}", b.url));
    };
    // quiet after a minute without a new CPI, as on the pages
    let mut pill = Pill::freshness(false, Some(t), c.now, 20_000);
    let mut text = format!("last CPI {}", ago(c.now, t));
    if let (Some(tm), Some(g)) = (&s.timing, &s.site) {
        let cpi_ms = g.cpi_s * 1000.0;
        let pace = if tm.cpi_ms <= cpi_ms {
            "keeping up"
        } else {
            pill = pill.map(|p| if p == Pill::Ok { Pill::Partial } else { p });
            "falling behind"
        };
        text += &format!("; {:.0} ms per {cpi_ms:.0} ms CPI, {pace}", tm.cpi_ms);
    }
    (pill, text)
}

fn adsb_state(c: &Ctx) -> (Option<Pill>, String) {
    let a = &c.s.setup.adsb;
    match (&a.error, a.last_ok) {
        (Some(e), _) => (Some(Pill::Down), format!("not answering: {e} ({})", a.url)),
        (None, Some(t)) => {
            // stale once older than the oldest positions a CPI may be judged against
            let pill = Pill::freshness(false, Some(t), c.now, a.max_age_s * 1000 / 3);
            let text = if pill == Some(Pill::Ok) {
                format!("{} aircraft with a current position", a.aircraft)
            } else {
                format!("nothing current since {}", ago(c.now, t))
            };
            (pill, text)
        }
        (None, None) => (None, format!("not read yet from {}", a.url)),
    }
}

fn reports_state(c: &Ctx) -> (Option<Pill>, String) {
    let m = &c.s.setup.mesh;
    if !m.enabled {
        return (None, "sending them is turned off (mesh.enabled)".into());
    }
    if let Some(e) = &m.error {
        return (
            Some(Pill::Down),
            format!("can't be sent through the core: {e}"),
        );
    }
    let Some(r) = &m.last_report else {
        return (None, format!("the first goes out within {} s", m.batch_s));
    };
    let quiet = c.now - r.at;
    if quiet > 3 * m.batch_s * 1000 {
        let text = format!(
            "none for {}, though one is due every {} s",
            human_duration(quiet),
            m.batch_s
        );
        return (Some(Pill::Stale), text);
    }
    let (pill, what) = if r.left_out > 0 {
        let what = format!("{} sent, {} left out to fit the budget", r.kept, r.left_out);
        (Pill::Partial, what)
    } else {
        (Pill::Ok, format!("all {} detections sent", r.kept))
    };
    (Some(pill), format!("last {}: {what}", ago(c.now, r.at)))
}

/// Down when none of its reports can be read here, stale when they have stopped coming,
/// partial when it leaves detections out to fit its budget or some of its reports can't
/// be read.
fn station_pill(st: &Station, s: &SetupInfo, now: i64) -> Pill {
    if st.reports > 0 && st.unreadable == st.reports {
        return Pill::Down;
    }
    let every_ms = st
        .every_s
        .map_or(s.mesh.batch_s * 1000, |e| (e * 1000.0) as i64);
    // other sites' reports arrive with the core's syncs, which the map waits settle_s for
    let late = (3 * every_ms).max(s.fusion.settle_s * 1000);
    match st.last {
        Some(t) if now - t > late => Pill::Stale,
        Some(_) if st.left_out > 0 || st.unreadable > 0 => Pill::Partial,
        Some(_) => Pill::Ok,
        None => Pill::Stale,
    }
}

/// This site first, then the others by callsign.
fn station_list(s: &SetupInfo) -> Vec<&Station> {
    let mut list: Vec<&Station> = s.stations.iter().collect();
    list.sort_by_key(|st| !st.here);
    list
}

fn sites(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let s = &c.s.setup;
    let width = area.width as usize;
    let note_rows: usize = s.notes.iter().map(|n| wrapped_rows(n, width)).sum();
    let [feeds, notes, line, table] = Layout::vertical([
        Constraint::Length(3),
        // the setup page has them all
        Constraint::Length(note_rows.min(6) as u16),
        Constraint::Length(2),
        Constraint::Fill(1),
    ])
    .areas(area);
    let feed_rows = [
        ("blah2", blah2_state(c)),
        ("ADS-B", adsb_state(c)),
        ("Reports", reports_state(c)),
    ]
    .map(|(name, (pill, text))| (pill, vec![Cell::from(name), Cell::from(text)]));
    let widths = [Constraint::Length(7), Constraint::Fill(1)];
    f.render_widget(pill_table(c.theme, &[], &widths, feed_rows), feeds);
    let warnings: Vec<Line> = s
        .notes
        .iter()
        .map(|n| Line::styled(n.clone(), c.theme.bad()))
        .collect();
    f.render_widget(Paragraph::new(warnings).wrap(Wrap { trim: true }), notes);
    let m = &s.mesh;
    let budget = format!(
        "Budget here: {} kbit/s, {:.1} kB and {} detections a report. * is this site.",
        m.max_kbps,
        m.bytes_allowed as f64 / 1000.0,
        m.max_per_batch
    );
    f.render_widget(Line::styled(budget, c.theme.mute()), line);
    let list = station_list(s);
    if list.is_empty() {
        let hint = "No radar site has logged its geometry or sent a report in the last 24 hours. This site does once blah2's config is in.";
        return say(c, f, table, hint);
    }
    let items = list.iter().map(|st| {
        let mark = if st.here { " *" } else { "" };
        let every = st.every_s.map_or("-".into(), |e| format!("{e:.0} s"));
        let kbps = st.kbps.map_or("-".into(), |k| format!("{k:.2}"));
        let last = st.last.map_or("-".into(), |t| ago(c.now, t));
        (
            Some(station_pill(st, s, c.now)),
            vec![
                Cell::from(format!("{}{mark}", st.call)),
                Cell::from(st.reports.to_string()),
                Cell::from(every),
                Cell::from(kbps),
                Cell::from(count(st.detections)),
                Cell::from(left_out(st.left_out, st.detections)),
                Cell::from(last),
            ],
        )
    });
    let header = [
        "Station", "Reports", "Every", "kbit/s", "Sent", "Left out", "Last",
    ];
    let widths = [12, 7, 6, 6, 6, 10, 10].map(Constraint::Length);
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), table, rows);
}

/// The detail of the station `call`, wherever it is in the list now.
fn site_detail(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState, call: &str) {
    let list = station_list(&c.s.setup);
    let Some(i) = list.iter().position(|st| st.call == call) else {
        return say(c, f, area, &format!("{call} is not in the latest read."));
    };
    rows.select(Some(i));
    let text = Paragraph::new(station_detail(c, list[i])).wrap(Wrap { trim: false });
    f.render_widget(text, area);
}

fn place(p: &Place) -> String {
    let mut s = if p.name.is_empty() {
        "(no name)".to_string()
    } else {
        p.name.clone()
    };
    if let (Some(lat), Some(lon)) = (p.latitude, p.longitude) {
        s += &format!(", {lat:.3}, {lon:.3}");
    }
    if let Some(alt) = p.altitude {
        s += &format!(", {alt:.0} m");
    }
    s
}

/// All about one station: where its radar is, and what its reports carried.
fn station_detail(c: &Ctx, st: &Station) -> Vec<Line<'static>> {
    let who = if st.here {
        "this site"
    } else {
        "heard through the mesh"
    };
    let mut lines = vec![Line::styled(
        format!("{}, {who}", st.call),
        c.theme.heading(),
    )];
    match &st.site {
        Some(g) => {
            lines.extend([
                Line::from(format!("Receiver     {}", place(&g.rx))),
                Line::from(format!("Transmitter  {}", place(&g.tx))),
                Line::from(format!(
                    "Illuminator  {:.3} MHz, {} s CPIs, baseline {:.1} km",
                    g.fc_hz / 1e6,
                    g.cpi_s,
                    g.baseline_km
                )),
            ]);
            if let Some(t) = g.since {
                lines.push(Line::from(format!("Geometry     logged {}", short_time(t))));
            }
        }
        None => lines.push(Line::styled(
            "No geometry logged yet (radar.site), so its reports can't be used to locate aircraft.",
            c.theme.bad(),
        )),
    }
    if st.reports == 0 {
        lines.push(Line::from("Reports      none in the last 24 hours"));
        return lines;
    }
    let every = st
        .every_s
        .map_or(String::new(), |e| format!(", one every {e:.0} s"));
    let kbps = st
        .kbps
        .map_or(String::new(), |k| format!(": {k:.2} kbit/s"));
    lines.extend([
        Line::from(format!(
            "Reports      {} in the last 24 hours{every}",
            st.reports
        )),
        Line::from(format!(
            "Size         {:.1} kB each as signed events{kbps}",
            st.bytes as f64 / 1000.0 / st.reports as f64
        )),
        Line::from(format!(
            "Detections   {} sent over {} CPIs",
            st.detections, st.cpis
        )),
        Line::from(match st.left_out {
            0 => "Left out     none".to_string(),
            n => format!(
                "Left out     {n} of {} ({}), the weakest, to fit its budget",
                n + st.detections,
                pct(Some(n as f64 / (n + st.detections) as f64))
            ),
        }),
    ]);
    if st.unreadable > 0 {
        lines.push(Line::styled(
            format!(
                "Unreadable   {} of its reports this version can't read: upgrade every site together",
                st.unreadable
            ),
            c.theme.bad(),
        ));
    }
    lines
}

/// When the map's moment was, and whether ADS-B was there to check it against.
fn as_of(c: &Ctx, m: &MapInfo) -> Line<'static> {
    let adsb = match m.adsb {
        Some(n) => format!("ADS-B had {n} aircraft then"),
        None => "there was no current ADS-B then".into(),
    };
    Line::from(format!(
        "As of {}, {} behind now; {adsb}.",
        clock(m.ts),
        human_duration(c.now - m.ts)
    ))
}

/// The line saying what a page leaves out, the way the map does, if anything: `n` of
/// `what` (one and many), with no ADS-B aircraft `near` them.
fn left_off_line(
    c: &Ctx,
    m: &MapInfo,
    n: usize,
    what: (&str, &str),
    near: &str,
) -> Option<Line<'static>> {
    let (what, them) = if n == 1 {
        (what.0, "it")
    } else {
        (what.1, "them")
    };
    let why = if m.adsb.is_none() {
        "no current ADS-B to check against".to_string()
    } else {
        format!("no ADS-B aircraft {near} {them}")
    };
    (n > 0).then(|| {
        Line::styled(
            format!("{n} {what} left off, {why}; fusion.show_unmatched shows {them}."),
            c.theme.mute(),
        )
    })
}

/// The tracks and fixes pages before there is a map: why not, or why it couldn't be read.
fn nothing_yet(c: &Ctx, f: &mut Frame, area: Rect) {
    if c.failed.is_some() {
        return waiting(c, f, area);
    }
    let fu = &c.s.setup.fusion;
    let why = if fu.enabled {
        format!(
            "Nothing worked out yet. Each moment is worked out {} s after it, once every site's reports have arrived; this site's own reports are needed too, so blah2 must be answering.",
            fu.settle_s
        )
    } else {
        "Locating aircraft is turned off (fusion.enabled in the config), so there are no tracks or fixes.".into()
    };
    say(c, f, area, &why);
}

fn tracks(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let Some(m) = &c.s.map else {
        return nothing_yet(c, f, area);
    };
    let mut lines = vec![as_of(c, m)];
    for s in &m.sites {
        lines.push(Line::from(format!(
            "{}: {} confirmed tracks, {} with an ADS-B aircraft on them",
            s.call, s.confirmed, s.with_adsb
        )));
    }
    lines.extend(left_off_line(
        c,
        m,
        m.left_off.tracks,
        ("track", "tracks"),
        "on",
    ));
    let area = above(f, area, lines);
    let list: Vec<(&str, &Track)> = m
        .sites
        .iter()
        .flat_map(|s| s.tracks.iter().map(move |t| (s.call.as_str(), t)))
        .collect();
    if list.is_empty() {
        return say(c, f, area, "No confirmed tracks to show at this moment.");
    }
    let items = list.iter().map(|(call, t)| {
        (
            t.adsb.then_some(Pill::Ok),
            vec![
                Cell::from(call.to_string()),
                Cell::from(format!("#{}", t.id)),
                Cell::from(format!("{:.1} km", t.range_km)),
                Cell::from(format!("{:.1} Hz", t.doppler_hz)),
                Cell::from(if t.adsb { "on it" } else { "none on it" }),
            ],
        )
    });
    let header = ["Site", "Track", "Bistatic range", "Doppler", "ADS-B"];
    let widths = [12, 6, 14, 10, 10].map(Constraint::Length);
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}

/// What ADS-B said about a fix, in a few words.
fn fix_adsb(a: Option<&FixAdsb>) -> String {
    match a {
        None => "not current".into(),
        Some(a) if !a.near => "none near".into(),
        Some(a) => {
            let who = match (a.flight.as_deref(), a.hex.as_deref()) {
                (Some(f), _) if !f.is_empty() => f,
                (_, Some(h)) => h,
                _ => "an aircraft",
            };
            format!("{who}, {}", dist(a.err_km.unwrap_or_default()))
        }
    }
}

fn fixes(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let Some(m) = &c.s.map else {
        return nothing_yet(c, f, area);
    };
    let mut lines = vec![
        as_of(c, m),
        Line::from(format!(
            "{} aircraft located from two or more sites, without ADS-B.",
            m.fixes.len()
        )),
    ];
    lines.extend(left_off_line(
        c,
        m,
        m.left_off.fixes,
        ("fix", "fixes"),
        "near",
    ));
    let area = above(f, area, lines);
    if m.fixes.is_empty() {
        let hint = "No aircraft to show located from two or more sites at this moment. That takes a second radar site whose coverage overlaps this one's.";
        return say(c, f, area, hint);
    }
    let items = m.fixes.iter().map(|x| {
        (
            x.adsb.as_ref().is_some_and(|a| a.near).then_some(Pill::Ok),
            vec![
                Cell::from(format!("#{}", x.track)),
                Cell::from(x.sites.split(", ").count().to_string()),
                Cell::from(format!("{:.0} m", x.alt_m)),
                Cell::from(format!("{:.0} m/s", x.ve.hypot(x.vn))),
                Cell::from(dist(x.major_km)),
                Cell::from(fix_adsb(x.adsb.as_ref())),
            ],
        )
    });
    // the sites are in the detail; three of them don't fit here
    let header = ["Track", "Sites", "Height", "Speed", "Error", "ADS-B"];
    let widths = [
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Fill(1),
    ];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}

/// The detail of fix `id`, wherever it is in the list now.
fn one_fix(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState, id: u64) {
    let Some(m) = &c.s.map else {
        return nothing_yet(c, f, area);
    };
    let Some(i) = m.fixes.iter().position(|x| x.track == id) else {
        return say(
            c,
            f,
            area,
            &format!("Fix #{id} is not in the latest moment."),
        );
    };
    rows.select(Some(i));
    let text = Paragraph::new(fix_detail(c, m, &m.fixes[i])).wrap(Wrap { trim: false });
    f.render_widget(text, area);
}

/// All about one fix.
fn fix_detail(c: &Ctx, m: &MapInfo, x: &Fix) -> Vec<Line<'static>> {
    let heading = x.ve.atan2(x.vn).to_degrees().rem_euclid(360.0);
    vec![
        Line::styled(
            format!(
                "Fix #{}, seen by {}, as of {}",
                x.track,
                x.sites,
                clock(m.ts)
            ),
            c.theme.heading(),
        ),
        Line::from(format!(
            "Position  {:.4}, {:.4} at {:.0} m, give or take {:.0} m",
            x.lat, x.lon, x.alt_m, x.alt_sigma_m
        )),
        Line::from(format!(
            "Velocity  {:.0} m/s heading {heading:03.0} (east {:.0}, north {:.0})",
            x.ve.hypot(x.vn),
            x.ve,
            x.vn
        )),
        Line::from(format!(
            "Error     1-sigma ellipse {} by {}, long axis {:.0} degrees from east",
            dist(x.major_km),
            dist(x.minor_km),
            x.angle_deg.rem_euclid(180.0)
        )),
        Line::from(format!(
            "Fit       chi-square {:.2} per degree of freedom",
            x.chi2
        )),
        Line::from(format!("ADS-B     {}", fix_adsb(x.adsb.as_ref()))),
    ]
}

/// "24 hours" for "24h".
fn window_label(w: &str) -> &'static str {
    WINDOWS.iter().find(|x| x.0 == w).map_or("window", |x| x.1)
}

fn window_line(c: &Ctx, st: &StatsInfo) -> Line<'static> {
    let shown = window_label(&st.window);
    let mut spans = vec![Span::raw(format!(
        "Last {shown}, from {}: {} CPIs checked, {} detections.",
        short_time(st.from),
        st.cpis,
        st.detections
    ))];
    let asked = WINDOWS[c.window];
    if asked.0 != st.window {
        spans.push(Span::styled(
            format!(" Reading the last {}...", asked.1),
            c.theme.mute(),
        ));
    }
    Line::from(spans)
}

fn reliable_line(st: &StatsInfo) -> String {
    let at = pct(Some(st.pd_threshold));
    match st.reliable_km {
        Some(km) => format!("Reliable range: {km} km, as far as Pd stays at or above {at}."),
        None if st
            .bins
            .iter()
            .any(|b| b.opportunities >= st.min_opportunities) =>
        {
            format!(
                "Reliable range: none, Pd is under {at} already in the nearest range bin judged."
            )
        }
        None => format!(
            "Reliable range: not yet, no range bin has {} CPIs in coverage.",
            st.min_opportunities
        ),
    }
}

fn group_text(g: &Group) -> String {
    if g.fixes == 0 {
        return "none".into();
    }
    let median = g
        .median_km
        .map_or(String::new(), |m| format!(", median error {}", dist(m)));
    let near = pct((g.scored > 0).then(|| g.matched as f64 / g.scored as f64));
    format!("{} fixes, {near} near ADS-B{median}", g.fixes)
}

fn stats_lines(st: &StatsInfo) -> Vec<Line<'static>> {
    let chance = st.chance_pd.map_or(String::new(), |p| {
        format!("; chance alone would give {:.1}%", p * 100.0)
    });
    let per_min = st
        .unmatched_per_min
        .map_or(String::new(), |m| format!(", about {m:.0} a minute"));
    let seen =
        pct((st.aircraft_judged > 0)
            .then(|| st.aircraft_detected as f64 / st.aircraft_judged as f64));
    let all = pct((st.detections > 0).then(|| st.unmatched as f64 / st.detections as f64));
    let multi = &st.multi_site;
    vec![
        Line::from(format!(
            "Pd {} over {} aircraft-CPIs in coverage{chance}.",
            pct(st.pd),
            st.opportunities
        )),
        Line::from(format!(
            "Aircraft seen in at least {} CPIs: {} of {} ({seen}); the goal is 80%.",
            st.detected_hits, st.aircraft_detected, st.aircraft_judged
        )),
        Line::from(reliable_line(st)),
        Line::from(format!(
            "Unmatched: {:.2} a CPI{per_min} ({all} of all detections).",
            st.unmatched_per_cpi.unwrap_or_default()
        )),
        Line::from(format!(
            "Located from 2 sites: {}; from 3 or more: {}.",
            group_text(&multi.two_sites),
            group_text(&multi.three_or_more)
        )),
    ]
}

/// Judged bins are OK at or above the threshold and partial below it; thin ones have no
/// state.
fn bin_pill(b: &Bin, st: &StatsInfo) -> Option<Pill> {
    let pd = b.pd.filter(|_| b.opportunities >= st.min_opportunities)?;
    Some(if pd >= st.pd_threshold {
        Pill::Ok
    } else {
        Pill::Partial
    })
}

fn stats(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let Some(st) = &c.s.stats else {
        return waiting(c, f, area);
    };
    let mut lines = vec![window_line(c, st)];
    if let Some(t) = st.setup_changed {
        lines.push(Line::styled(
            format!(
                "blah2's setup changed, so these numbers start at {}.",
                short_time(t)
            ),
            c.theme.bad(),
        ));
    }
    if st.cpis == 0 {
        let area = above(f, area, lines);
        let hint = "No CPIs checked against ADS-B in this window yet. They are counted once blah2 and the ADS-B feed are both answering.";
        return say(c, f, area, hint);
    }
    lines.extend(stats_lines(st));
    let area = above(f, area, lines);
    let items = st.bins.iter().map(|b| {
        let thin = if b.opportunities < st.min_opportunities {
            "  too few to judge"
        } else {
            ""
        };
        (
            bin_pill(b, st),
            vec![
                Cell::from(format!("{}-{} km", b.from_km, b.to_km)),
                Cell::from(b.opportunities.to_string()),
                Cell::from(b.hits.to_string()),
                Cell::from(format!("{}{thin}", pct(b.pd))),
            ],
        )
    });
    let header = ["Bistatic range", "Aircraft-CPIs", "Detected", "Pd"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Fill(1),
    ];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}

/// Its callsign and address from the ADS-B feed, or only a number when the app hides them.
fn aircraft_name(a: &Aircraft) -> String {
    match (a.flight.as_deref(), a.hex.as_deref(), a.aircraft) {
        (Some(f), Some(h), _) if !f.is_empty() => format!("{f} ({h})"),
        (_, Some(h), _) => h.to_string(),
        (_, None, Some(n)) => format!("aircraft {n}"),
        _ => "?".into(),
    }
}

/// Seen when matched in enough CPIs, missed when judged and not; no state with too few
/// CPIs to judge.
fn aircraft_pill(a: &Aircraft, st: &StatsInfo) -> Option<Pill> {
    (a.opportunities >= st.min_opportunities).then_some(if a.hits >= st.detected_hits {
        Pill::Ok
    } else {
        Pill::Down
    })
}

fn aircraft(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) {
    let Some(st) = &c.s.stats else {
        return waiting(c, f, area);
    };
    let listed = if st.aircraft_total > st.aircraft.len() {
        format!(
            " The {} with the most time in coverage are listed.",
            st.aircraft.len()
        )
    } else {
        String::new()
    };
    let lines = vec![
        window_line(c, st),
        Line::from(format!(
            "{} aircraft in coverage; seen in {} or more CPIs: {} of the {} judged.{listed}",
            st.aircraft_total, st.detected_hits, st.aircraft_detected, st.aircraft_judged
        )),
    ];
    let area = above(f, area, lines);
    if st.aircraft.is_empty() {
        let hint = "No ADS-B aircraft inside blah2's coverage in this window.";
        return say(c, f, area, hint);
    }
    let items = st.aircraft.iter().map(|a| {
        (
            aircraft_pill(a, st),
            vec![
                Cell::from(aircraft_name(a)),
                Cell::from(a.opportunities.to_string()),
                Cell::from(a.hits.to_string()),
                Cell::from(pct(a.pd)),
                Cell::from(format!("{:.0}-{:.0} km", a.min_km, a.max_km)),
            ],
        )
    });
    let header = [
        "Aircraft",
        "In coverage",
        "Detected",
        "Pd",
        "Bistatic range",
    ];
    let widths = [
        Constraint::Length(18),
        Constraint::Length(11),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Fill(1),
    ];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
}
#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::tui::MIN_SIZE;
    use ridgewarden_client::tui::ratatui::Terminal;
    use ridgewarden_client::tui::ratatui::backend::TestBackend;
    use ridgewarden_client::tui::ratatui::buffer::Buffer;
    use ridgewarden_client::tui::ratatui::crossterm::event::KeyModifiers;
    use serde_json::Value;

    /// What the app returned on the simulator: three sites seeing the same made-up
    /// aircraft, with this one, N0CALL-2, sending its reports at 1 kbit/s.
    fn fixture(name: &str) -> Value {
        let text = match name {
            "setup" => include_str!("../fixtures/top-setup.json"),
            "map" => include_str!("../fixtures/top-map.json"),
            _ => include_str!("../fixtures/top-stats.json"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn snap() -> Snap {
        let setup: SetupInfo = serde_json::from_value(fixture("setup")).unwrap();
        Snap {
            at: setup.generated,
            setup,
            map: Some(serde_json::from_value(fixture("map")).unwrap()),
            stats: Some(serde_json::from_value(fixture("stats")).unwrap()),
        }
    }

    /// A read where every endpoint failed with `e`.
    fn failing(e: &str) -> Read {
        Read {
            at: 0,
            setup: Err(e.into()),
            map: Err(e.into()),
            stats: None,
        }
    }

    /// A read where every endpoint answered with what `s` holds.
    fn answered(s: Snap) -> Read {
        Read {
            at: s.at,
            setup: Ok(s.setup),
            map: Ok(s.map),
            stats: s.stats.map(Ok),
        }
    }

    fn ask(window: usize, stats: bool) -> Arc<Ask> {
        Arc::new(Ask {
            window: AtomicUsize::new(window),
            stats: AtomicBool::new(stats),
        })
    }

    fn top(snap: Option<Snap>) -> Top {
        let refresh = Refresh::spawn(Duration::from_secs(3600), async || {
            failing("not read in this test")
        })
        .unwrap();
        let mut t = Top::new(refresh, ask(1, false));
        t.theme = Theme { color: false };
        t.snap = snap;
        t
    }

    fn text(b: &Buffer) -> String {
        let a = b.area;
        let rows = (0..a.height).map(|y| {
            let row: String = (0..a.width).map(|x| b[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        });
        rows.collect::<Vec<_>>().join("\n")
    }

    /// The screen of a `w` by `h` terminal after one draw, at the moment the app answered.
    fn screen(t: &mut Top, w: u16, h: u16) -> String {
        let at = t.snap.as_ref().map_or(0, |s| s.at);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| t.draw_at(f, at)).unwrap();
        text(term.backend().buffer())
    }

    /// The screen's words with single spaces, so wrapped text reads as one line.
    fn flat(screen: &str) -> String {
        screen.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctx(s: &Snap) -> Ctx<'_> {
        static PLAIN: Theme = Theme { color: false };
        Ctx {
            theme: &PLAIN,
            s,
            now: s.setup.generated,
            window: 1,
            failed: None,
        }
    }

    fn texts(lines: Vec<Line>) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn before_the_first_read_it_says_loading_or_why_the_app_cant_be_reached() {
        let mut t = top(None);
        assert!(screen(&mut t, 80, 24).contains("Loading..."));
        t.take(failing(
            "http://127.0.0.1:8802/api/setup: could not connect",
        ));
        let sc = screen(&mut t, 80, 24);
        let s = flat(&sc);
        assert!(
            s.contains(
                "ERROR Can't read the app: http://127.0.0.1:8802/api/setup: could not connect."
            ),
            "{s}"
        );
        let title = sc.lines().next().unwrap();
        assert!(title.ends_with("app not answering"), "{title}");
        // a read that works, then one that fails: the last good one stays, marked
        t.take(answered(snap()));
        t.take(failing("http://127.0.0.1:8802/api/setup: timed out"));
        let s = screen(&mut t, 80, 24);
        assert!(
            flat(&s).contains(
                "Reading failed, so this is from earlier: http://127.0.0.1:8802/api/setup: timed out"
            ),
            "{s}"
        );
        let title = s.lines().next().unwrap();
        assert!(
            title.ends_with("app not answering, updated 0 s ago"),
            "{title}"
        );
        assert!(title.contains("N0CALL-2 at tst-mesa"), "{title}");
        assert!(
            s.contains("N0CALL-2 *"),
            "the earlier read is still shown: {s}"
        );
    }

    #[test]
    fn keys_move_between_pages_rows_and_details() {
        let mut t = top(Some(snap()));
        let fixes: Vec<u64> = t
            .snap
            .as_ref()
            .unwrap()
            .map
            .as_ref()
            .unwrap()
            .fixes
            .iter()
            .map(|x| x.track)
            .collect();
        t.key(key(KeyCode::Char('3')));
        assert_eq!(t.tab, 2);
        assert!(
            !t.ask.stats.load(Ordering::Relaxed),
            "fixes need no statistics"
        );
        t.key(key(KeyCode::Enter));
        assert_eq!(
            t.detail,
            Some(Detail::Fix(fixes[0])),
            "the first, when none was picked"
        );
        assert!(screen(&mut t, 80, 24).contains("Esc back"));
        t.key(key(KeyCode::Down));
        assert_eq!(
            t.detail,
            Some(Detail::Fix(fixes[1])),
            "the detail moves on with the selection"
        );
        t.key(key(KeyCode::Esc));
        assert_eq!(t.detail, None);
        t.key(key(KeyCode::Char('2')));
        t.key(key(KeyCode::Enter));
        assert_eq!(t.detail, None, "tracks have nothing more to show");
        t.key(key(KeyCode::Char('1')));
        t.key(key(KeyCode::Down));
        t.key(key(KeyCode::Down));
        assert_eq!(t.rows[0].selected(), Some(1));
        t.key(key(KeyCode::Enter));
        assert!(screen(&mut t, 80, 24).contains("heard through the mesh"));
        t.key(key(KeyCode::Tab));
        assert_eq!(
            (t.tab, &t.detail),
            (1, &None),
            "another page closes the detail"
        );
        // the statistics are read only while a page shows them
        for (page, wanted) in [('4', true), ('5', true), ('2', false)] {
            t.key(key(KeyCode::Char(page)));
            assert_eq!(t.ask.stats.load(Ordering::Relaxed), wanted, "page {page}");
        }
        let asked = |t: &Top| t.ask.window.load(Ordering::Relaxed);
        for want in [2, 3, 0, 1] {
            t.key(key(KeyCode::Char('w')));
            assert_eq!(asked(&t), want, "w steps through the windows and wraps");
        }
        t.key(key(KeyCode::Char('w')));
        t.tab = 3;
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("Reading the last 7 days..."), "{s}");
    }

    #[test]
    fn a_detail_stays_on_its_fix_or_site_while_the_rows_move() {
        let mut t = top(Some(snap()));
        t.key(key(KeyCode::Char('3')));
        t.key(key(KeyCode::Enter));
        let fixes = t.snap.as_ref().unwrap().map.as_ref().unwrap().fixes.clone();
        let id = fixes[0].track;
        // the next moment lists them the other way round
        let mut next = snap();
        next.map.as_mut().unwrap().fixes.reverse();
        t.take(answered(next));
        let s = screen(&mut t, 80, 24);
        assert!(s.contains(&format!("Fix #{id}, seen by")), "{s}");
        assert_eq!(
            t.rows[2].selected(),
            Some(fixes.len() - 1),
            "the selection follows it"
        );
        let mut gone = snap();
        gone.map.as_mut().unwrap().fixes.retain(|x| x.track != id);
        t.take(answered(gone));
        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains(&format!("Fix #{id} is not in the latest moment.")),
            "{s}"
        );
        t.key(key(KeyCode::Esc));
        assert!(screen(&mut t, 80, 24).contains("located from two or more sites"));
        // a station, by callsign
        t.key(key(KeyCode::Char('1')));
        t.key(key(KeyCode::End));
        t.key(key(KeyCode::Enter));
        let Some(Detail::Site(call)) = t.detail.clone() else {
            panic!("{:?}", t.detail)
        };
        let mut fewer = snap();
        fewer.setup.stations.retain(|st| st.call != call);
        t.take(answered(fewer));
        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains(&format!("{call} is not in the latest read.")),
            "{s}"
        );
    }

    #[test]
    fn what_answered_is_kept_when_another_endpoint_fails() {
        let mut t = top(None);
        t.take(answered(snap()));
        let mut r = answered(snap());
        r.map = Err("http://127.0.0.1:8802/api/map: HTTP 500".into());
        r.setup.as_mut().unwrap().notes = vec!["A fresh warning.".into()];
        t.take(r);
        let s = t.snap.as_ref().unwrap();
        assert_eq!(
            s.setup.notes,
            ["A fresh warning."],
            "the setup that answered is used"
        );
        assert!(s.map.is_some(), "the map from before is kept");
        t.tab = 1;
        let sc = flat(&screen(&mut t, 80, 24));
        assert!(
            sc.contains(
                "Reading failed, so this is from earlier: http://127.0.0.1:8802/api/map: HTTP 500"
            ),
            "{sc}"
        );
        t.tab = 0;
        assert!(
            !screen(&mut t, 80, 24).contains("Reading failed"),
            "the setup read fine"
        );
        // statistics never read say why in their place
        let mut fresh = top(None);
        let mut r = answered(snap());
        r.stats = Some(Err(
            "http://127.0.0.1:8802/api/stats?window=24h: timed out".into()
        ));
        fresh.take(r);
        fresh.tab = 3;
        let sc = flat(&screen(&mut fresh, 80, 24));
        assert!(
            sc.contains("ERROR http://127.0.0.1:8802/api/stats?window=24h: timed out"),
            "{sc}"
        );
        assert!(!sc.contains("Reading failed"), "{sc}");
        // and when nobody asked for them, old ones go rather than show later
        let mut r = answered(snap());
        r.stats = None;
        fresh.take(r);
        assert!(fresh.snap.as_ref().unwrap().stats.is_none());
        assert!(screen(&mut fresh, 80, 24).contains("Loading..."));
    }

    #[test]
    fn each_page_says_why_it_is_empty() {
        let mut s = snap();
        s.map = None;
        let mut t = top(Some(s));
        for tab in [1, 2] {
            t.tab = tab;
            let sc = flat(&screen(&mut t, 80, 24));
            assert!(
                sc.contains("Nothing worked out yet. Each moment is worked out 300 s after it"),
                "{sc}"
            );
        }
        t.snap.as_mut().unwrap().setup.fusion.enabled = false;
        let sc = flat(&screen(&mut t, 80, 24));
        assert!(
            sc.contains("Locating aircraft is turned off (fusion.enabled in the config), so there are no tracks or fixes."),
            "{sc}"
        );
        let mut s = snap();
        let m = s.map.as_mut().unwrap();
        m.fixes.clear();
        m.left_off = LeftOff {
            tracks: 0,
            fixes: 0,
        };
        for site in &mut m.sites {
            site.tracks.clear();
        }
        let st = s.stats.as_mut().unwrap();
        (st.cpis, st.aircraft_total) = (0, 0);
        st.aircraft.clear();
        s.setup.stations.clear();
        let mut t = top(Some(s));
        for (tab, want) in [
            (
                0,
                "No radar site has logged its geometry or sent a report in the last 24 hours.",
            ),
            (1, "No confirmed tracks to show at this moment."),
            (
                2,
                "No aircraft to show located from two or more sites at this moment.",
            ),
            (3, "No CPIs checked against ADS-B in this window yet."),
            (
                4,
                "No ADS-B aircraft inside blah2's coverage in this window.",
            ),
        ] {
            t.tab = tab;
            let sc = flat(&screen(&mut t, 80, 24));
            assert!(sc.contains(want), "page {tab}: {sc}");
        }
    }

    #[test]
    fn the_sites_page_shows_the_warnings_over_the_stations() {
        let notes = [
            "Can't read blah2 at http://127.0.0.1:3000: could not connect.",
            "No current ADS-B data from http://127.0.0.1:8080/data/aircraft.json (could not \
             connect). CPIs are not counted while the truth feed is down, since every detection \
             would look like a false alarm.",
        ];
        let mut s = snap();
        s.setup.notes = notes.map(str::to_string).to_vec();
        let mut t = top(Some(s));
        let sc = screen(&mut t, 80, 24);
        let title = sc.lines().next().unwrap();
        assert!(title.ends_with("2 warnings, updated 0 s ago"), "{title}");
        for n in notes {
            assert!(flat(&sc).contains(n), "{n}: {sc}");
        }
        assert!(
            sc.contains("N0CALL-2 *"),
            "the stations still fit under them: {sc}"
        );
    }

    #[test]
    fn ages_go_by_the_apps_clock_not_this_boxs() {
        let plain = snap();
        let app_now = plain.setup.generated;
        let mut skewed = snap();
        // this box's clock is an hour behind the app's
        skewed.at = app_now - 3_600_000;
        assert_eq!(skewed.now(skewed.at), app_now);
        assert_eq!(skewed.now(skewed.at + 10_000), app_now + 10_000);
        let (mut a, mut b) = (top(Some(plain)), top(Some(skewed)));
        for tab in 0..TABS.len() {
            (a.tab, b.tab) = (tab, tab);
            assert_eq!(screen(&mut a, 80, 24), screen(&mut b, 80, 24), "page {tab}");
        }
    }

    #[test]
    fn a_fix_detail_says_where_it_is_how_fast_and_how_sure() {
        let s = snap();
        let mut m = s.map.clone().unwrap();
        m.ts = 1_795_478_660_000;
        let x = Fix {
            track: 7,
            sites: "N0CALL-1, N0CALL-2, N0CALL-3".into(),
            lat: 39.6,
            lon: -104.9,
            alt_m: 9100.0,
            alt_sigma_m: 240.0,
            ve: 150.0,
            vn: -150.0,
            major_km: 0.42,
            minor_km: 0.06,
            angle_deg: -30.0,
            chi2: 0.8,
            adsb: Some(FixAdsb {
                near: true,
                hex: Some("~f0002b".into()),
                flight: Some("TST143".into()),
                err_km: Some(1.3),
            }),
        };
        assert_eq!(
            texts(fix_detail(&ctx(&s), &m, &x)),
            [
                "Fix #7, seen by N0CALL-1, N0CALL-2, N0CALL-3, as of 00:04:20 UTC",
                "Position  39.6000, -104.9000 at 9100 m, give or take 240 m",
                "Velocity  212 m/s heading 135 (east 150, north -150)",
                "Error     1-sigma ellipse 420 m by 60 m, long axis 150 degrees from east",
                "Fit       chi-square 0.80 per degree of freedom",
                "ADS-B     TST143, 1.3 km",
            ]
        );
    }

    #[test]
    fn a_station_detail_says_what_it_knows_of_it() {
        let s = snap();
        let c = ctx(&s);
        let mut st = s.setup.stations[0].clone();
        st.site = Some(Geometry {
            fc_hz: 96.9e6,
            cpi_s: 0.5,
            baseline_km: 18.5,
            since: Some(1_795_478_000_000),
            rx: Place {
                name: "Simulated receiver 2".into(),
                latitude: Some(39.5),
                longitude: Some(-104.8),
                altitude: None,
            },
            tx: Place {
                name: "Simulated transmitter 2".into(),
                latitude: None,
                longitude: None,
                altitude: None,
            },
        });
        (st.call, st.here) = ("N0CALL-9".into(), false);
        (st.reports, st.cpis, st.detections, st.left_out, st.bytes) = (3, 360, 900, 100, 22_500);
        (st.unreadable, st.every_s, st.kbps) = (0, Some(60.0), Some(1.0));
        assert_eq!(
            texts(station_detail(&c, &st)),
            [
                "N0CALL-9, heard through the mesh",
                "Receiver     Simulated receiver 2, 39.500, -104.800",
                "Transmitter  Simulated transmitter 2",
                "Illuminator  96.900 MHz, 0.5 s CPIs, baseline 18.5 km",
                "Geometry     logged 2026-11-23 23:53 UTC",
                "Reports      3 in the last 24 hours, one every 60 s",
                "Size         7.5 kB each as signed events: 1.00 kbit/s",
                "Detections   900 sent over 360 CPIs",
                "Left out     100 of 1000 (10%), the weakest, to fit its budget",
            ]
        );
        // one report so far, nothing left out, and one this version can't read
        (st.reports, st.every_s, st.kbps, st.left_out, st.unreadable) = (1, None, None, 0, 1);
        let one = texts(station_detail(&c, &st));
        assert_eq!(
            one[5..],
            [
                "Reports      1 in the last 24 hours",
                "Size         22.5 kB each as signed events",
                "Detections   900 sent over 360 CPIs",
                "Left out     none",
                "Unreadable   1 of its reports this version can't read: upgrade every site together",
            ]
        );
        // no geometry logged and no reports
        (st.site, st.reports) = (None, 0);
        assert_eq!(
            texts(station_detail(&c, &st))[1..],
            [
                "No geometry logged yet (radar.site), so its reports can't be used to locate aircraft.",
                "Reports      none in the last 24 hours",
            ]
        );
        st.here = true;
        assert_eq!(texts(station_detail(&c, &st))[0], "N0CALL-9, this site");
    }

    #[test]
    fn a_station_is_late_after_the_map_waits_and_down_when_none_of_it_reads() {
        let s = snap();
        let (setup, now) = (&s.setup, s.setup.generated);
        assert_eq!(setup.fusion.settle_s, 300);
        let mut st = setup.stations[0].clone();
        st.reports = 24;
        (st.left_out, st.unreadable) = (0, 0);
        (st.last, st.every_s) = (Some(now - 30_000), Some(60.0));
        assert_eq!(station_pill(&st, setup, now), Pill::Ok);
        st.left_out = 12;
        assert_eq!(
            station_pill(&st, setup, now),
            Pill::Partial,
            "left some out"
        );
        // other sites' reports come with the core's syncs, so wait as long as the map does
        st.last = Some(now - 300_000);
        assert_eq!(station_pill(&st, setup, now), Pill::Partial);
        st.last = Some(now - 300_001);
        assert_eq!(station_pill(&st, setup, now), Pill::Stale);
        // or for three reports, from a site that sends them further apart
        st.every_s = Some(600.0);
        assert_eq!(station_pill(&st, setup, now), Pill::Partial);
        st.last = Some(now - 1_800_001);
        assert_eq!(station_pill(&st, setup, now), Pill::Stale);
        st.last = Some(now - 1_000);
        st.unreadable = 24;
        assert_eq!(
            station_pill(&st, setup, now),
            Pill::Down,
            "another version's reports"
        );
        (st.unreadable, st.left_out) = (1, 0);
        assert_eq!(
            station_pill(&st, setup, now),
            Pill::Partial,
            "some unreadable"
        );
        (st.reports, st.unreadable, st.last) = (0, 0, None);
        assert_eq!(
            station_pill(&st, setup, now),
            Pill::Stale,
            "a site, but no reports"
        );
        let order: Vec<bool> = station_list(setup).iter().map(|x| x.here).collect();
        assert!(
            order[0] && !order[1..].iter().any(|h| *h),
            "this site first"
        );
    }

    #[test]
    fn the_feed_rows_say_what_is_wrong() {
        let mut s = snap();
        let now = s.setup.generated;
        s.setup.blah2.last_cpi = Some(now - 1_000);
        s.setup.timing = Some(Timing { cpi_ms: 140.0 });
        let (p, text) = blah2_state(&ctx(&s));
        assert_eq!(p, Some(Pill::Ok));
        assert_eq!(text, "last CPI 1 s ago; 140 ms per 500 ms CPI, keeping up");
        s.setup.timing = Some(Timing { cpi_ms: 620.0 });
        let (p, text) = blah2_state(&ctx(&s));
        assert_eq!(p, Some(Pill::Partial));
        assert!(
            text.ends_with("620 ms per 500 ms CPI, falling behind"),
            "{text}"
        );
        s.setup.blah2.last_cpi = Some(now - 61_000);
        assert_eq!(
            blah2_state(&ctx(&s)).0,
            Some(Pill::Stale),
            "quiet for a minute"
        );
        // its config failing again later doesn't matter while the one read holds
        s.setup.blah2.config_error = Some("HTTP 500".into());
        assert_ne!(blah2_state(&ctx(&s)).0, Some(Pill::Down));
        s.setup.site = None;
        assert_eq!(blah2_state(&ctx(&s)).0, Some(Pill::Down));
        s.setup.blah2.config_error = None;
        s.setup.blah2.last_cpi = None;
        let (p, text) = blah2_state(&ctx(&s));
        assert_eq!(p, None);
        assert!(
            text.starts_with("waiting for its config from http://"),
            "{text}"
        );
        s.setup.blah2.error = Some("could not connect".into());
        let (p, text) = blah2_state(&ctx(&s));
        assert_eq!(p, Some(Pill::Down));
        assert!(
            text.starts_with("not answering: could not connect (http://"),
            "{text}"
        );

        s.setup.adsb.last_ok = Some(now - 9_000);
        assert_eq!(adsb_state(&ctx(&s)).0, Some(Pill::Ok));
        s.setup.adsb.last_ok = Some(now - 11_000);
        let (p, text) = adsb_state(&ctx(&s));
        assert_eq!(p, Some(Pill::Stale), "older than the 10 s max_age_s");
        assert_eq!(text, "nothing current since 11 s ago");
        s.setup.adsb.error = Some("HTTP 404".into());
        assert_eq!(adsb_state(&ctx(&s)).0, Some(Pill::Down));

        s.setup.mesh.last_report = Some(LastReport {
            at: now - 20_000,
            kept: 512,
            left_out: 316,
        });
        let (p, text) = reports_state(&ctx(&s));
        assert_eq!(p, Some(Pill::Partial));
        assert_eq!(
            text,
            "last 20 s ago: 512 sent, 316 left out to fit the budget"
        );
        s.setup.mesh.last_report = Some(LastReport {
            at: now - 181_000,
            kept: 90,
            left_out: 0,
        });
        assert_eq!(
            reports_state(&ctx(&s)),
            (
                Some(Pill::Stale),
                "none for 3 min, though one is due every 60 s".to_string()
            ),
            "three batches late"
        );
        s.setup.mesh.error = Some("could not connect".into());
        assert_eq!(reports_state(&ctx(&s)).0, Some(Pill::Down));
        s.setup.mesh.enabled = false;
        assert_eq!(reports_state(&ctx(&s)).0, None);
    }

    #[test]
    fn range_bins_and_aircraft_are_judged_like_the_pages() {
        let s = snap();
        let st = s.stats.as_ref().unwrap();
        assert_eq!(
            (st.min_opportunities, st.pd_threshold, st.detected_hits),
            (30, 0.8, 3)
        );
        let bin = |opportunities, hits| Bin {
            from_km: 0,
            to_km: 5,
            opportunities,
            hits,
            pd: Some(hits as f64 / opportunities as f64),
        };
        assert_eq!(bin_pill(&bin(100, 80), st), Some(Pill::Ok));
        assert_eq!(bin_pill(&bin(100, 79), st), Some(Pill::Partial));
        assert_eq!(bin_pill(&bin(29, 29), st), None, "too few to judge");
        let plane = |opportunities, hits| Aircraft {
            hex: Some("~f00005".into()),
            flight: Some("TST105".into()),
            aircraft: None,
            opportunities,
            hits,
            pd: None,
            min_km: 3.0,
            max_km: 35.0,
        };
        assert_eq!(aircraft_pill(&plane(30, 3), st), Some(Pill::Ok));
        assert_eq!(
            aircraft_pill(&plane(300, 2), st),
            Some(Pill::Down),
            "seen too seldom"
        );
        assert_eq!(aircraft_pill(&plane(29, 0), st), None);
        assert_eq!(aircraft_name(&plane(1, 1)), "TST105 (~f00005)");
        let hidden = Aircraft {
            hex: None,
            flight: None,
            aircraft: Some(4),
            ..plane(1, 1)
        };
        assert_eq!(
            aircraft_name(&hidden),
            "aircraft 4",
            "show_aircraft_ids = false"
        );
        let mut st = st.clone();
        st.reliable_km = None;
        st.bins = vec![bin(10, 9)];
        assert_eq!(
            reliable_line(&st),
            "Reliable range: not yet, no range bin has 30 CPIs in coverage."
        );
        st.bins = vec![bin(100, 50)];
        assert!(reliable_line(&st).starts_with("Reliable range: none, Pd is under 80%"));
        st.reliable_km = Some(25);
        assert_eq!(
            reliable_line(&st),
            "Reliable range: 25 km, as far as Pd stays at or above 80%."
        );
    }

    #[test]
    fn the_aircraft_page_says_when_it_lists_only_some() {
        let mut s = snap();
        let st = s.stats.as_mut().unwrap();
        let listed = st.aircraft.len();
        st.aircraft_total = listed + 20;
        let mut t = top(Some(s));
        t.tab = 4;
        let sc = flat(&screen(&mut t, 80, 24));
        assert!(
            sc.contains(&format!(
                "{} aircraft in coverage; seen in 3 or more CPIs:",
                listed + 20
            )),
            "{sc}"
        );
        assert!(
            sc.contains(&format!(
                "The {listed} with the most time in coverage are listed."
            )),
            "{sc}"
        );
    }

    #[test]
    fn numbers_read_short() {
        assert_eq!(count(9_999), "9999");
        assert_eq!(count(40_213), "40.2k");
        assert_eq!(count(1_234_567), "1.2M");
        assert_eq!(left_out(0, 500), "0");
        assert_eq!(left_out(1216, 1025), "1216 54%");
        assert_eq!(dist(0.25), "250 m");
        assert_eq!(dist(1.44), "1.4 km");
        assert_eq!(wrapped_rows("", 80), 1);
        assert_eq!(wrapped_rows("aaaa bbbb", 9), 1);
        assert_eq!(wrapped_rows("aaaa bbbb", 8), 2);
        assert_eq!(
            wrapped_rows(&"x".repeat(25), 10),
            3,
            "a word longer than a row"
        );
        let near = |hex: Option<&str>, flight: Option<&str>| FixAdsb {
            near: true,
            hex: hex.map(str::to_string),
            flight: flight.map(str::to_string),
            err_km: Some(0.25),
        };
        let named = near(Some("~f0000b"), Some("TST111"));
        assert_eq!(fix_adsb(Some(&named)), "TST111, 250 m");
        let no_flight = near(Some("~f0000b"), Some(""));
        assert_eq!(fix_adsb(Some(&no_flight)), "~f0000b, 250 m");
        assert_eq!(fix_adsb(Some(&near(None, None))), "an aircraft, 250 m");
        let far = FixAdsb {
            near: false,
            ..near(None, None)
        };
        assert_eq!(fix_adsb(Some(&far)), "none near");
        assert_eq!(fix_adsb(None), "not current");
        let g = Group {
            fixes: 40,
            scored: 30,
            matched: 27,
            median_km: Some(0.42),
        };
        assert_eq!(
            group_text(&g),
            "40 fixes, 90% near ADS-B, median error 420 m"
        );
        assert_eq!(group_text(&Group { fixes: 0, ..g }), "none");
        let place = Place {
            name: String::new(),
            latitude: Some(39.5),
            longitude: Some(-104.8),
            altitude: Some(1650.0),
        };
        assert_eq!(super::place(&place), "(no name), 39.500, -104.800, 1650 m");
    }

    #[test]
    fn too_small_a_terminal_says_how_big_to_make_it() {
        let mut t = top(Some(snap()));
        for tab in 0..TABS.len() {
            t.tab = tab;
            assert!(!screen(&mut t, MIN_SIZE.0, MIN_SIZE.1).contains("Make the terminal"));
        }
        let s = screen(&mut t, MIN_SIZE.0 - 1, MIN_SIZE.1);
        assert!(s.contains("Make the terminal at least"), "{s}");
    }

    /// Each row of a screen with single spaces, from the first one holding `from`.
    fn rows_from(screen: &str, from: &str, n: usize) -> Vec<String> {
        let start = screen.lines().position(|l| l.contains(from)).unwrap();
        screen.lines().skip(start).take(n).map(flat).collect()
    }

    #[test]
    fn each_page_shows_what_the_app_returned_at_80_by_24() {
        let mut t = top(Some(snap()));
        let s = screen(&mut t, 80, 24);
        assert_eq!(
            rows_from(&s, " blah2 ", 3),
            [
                "OK blah2 last CPI 0 s ago; 0 ms per 500 ms CPI, keeping up",
                "OK ADS-B 14 aircraft with a current position",
                "PARTIAL Reports last 50 s ago: 473 sent, 408 left out to fit the budget",
            ]
        );
        // this site first, then the others by callsign, all leaving detections out at 1 kbit/s
        assert_eq!(
            rows_from(&s, "N0CALL-2 *", 3),
            [
                "PARTIAL N0CALL-2 * 22 62 s 0.95 10.9k 8740 45% 50 s ago",
                "PARTIAL N0CALL-1 22 62 s 0.95 10.9k 9217 46% 50 s ago",
                "PARTIAL N0CALL-3 22 62 s 0.94 10.7k 8569 44% 50 s ago",
            ]
        );
        t.tab = 1;
        let s = screen(&mut t, 80, 24);
        for want in [
            "As of 00:04:20 UTC, 5 min behind now; ADS-B had 14 aircraft then.",
            "N0CALL-1: 4 confirmed tracks, 3 with an ADS-B aircraft on them",
            "1 track left off, no ADS-B aircraft on it; fusion.show_unmatched shows it.",
        ] {
            assert!(s.contains(want), "{want}: {s}");
        }
        assert_eq!(
            rows_from(&s, "#3 ", 2),
            [
                "OK N0CALL-1 #3 19.3 km -62.1 Hz on it",
                "OK N0CALL-1 #16 8.5 km 58.5 Hz on it"
            ]
        );
        assert!(
            s.lines().last().unwrap().starts_with("q quit"),
            "13 tracks and the help line fit: {s}"
        );
        t.tab = 2;
        let s = screen(&mut t, 80, 24);
        assert_eq!(
            rows_from(&s, "#69", 3),
            [
                "OK #69 2 5119 m 150 m/s 111 m TST143, 137 m",
                "OK #61 2 3596 m 129 m/s 113 m TST141, 25 m",
                "OK #71 2 3450 m 209 m/s 201 m TST146, 99 m",
            ]
        );
        t.tab = 3;
        let s = flat(&screen(&mut t, 80, 24));
        for want in [
            "Last 24 hours, from 2026-11-23 00:00 UTC: 1062 CPIs checked, 7647 detections.",
            "Pd 52% over 11984 aircraft-CPIs in coverage; chance alone would give 0.4%.",
            "Aircraft seen in at least 3 CPIs: 28 of 28 (100%); the goal is 80%.",
            "Reliable range: 25 km, as far as Pd stays at or above 80%.",
            "Unmatched: 1.39 a CPI, about 166 a minute (19% of all detections).",
            "OK 20-25 km 763 631 83% PARTIAL 25-30 km 1019 681 67%",
        ] {
            assert!(s.contains(want), "{want}: {s}");
        }
        t.tab = 4;
        let s = screen(&mut t, 80, 24);
        assert_eq!(
            rows_from(&s, "TST142", 1),
            ["OK TST142 (~f0002a) 897 691 77% 7-46 km"]
        );
    }

    /// The app's API on a local port, answering with the fixtures and noting each query
    /// /api/stats gets. Without `setup` it is an app from before /api/setup, and without
    /// `map` one that hasn't worked out a moment yet.
    async fn serve(setup: bool, map: bool, asked: Arc<std::sync::Mutex<Vec<String>>>) -> String {
        use axum::extract::RawQuery;
        use axum::http::{StatusCode, header};
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        let json = |body: &'static str| ([(header::CONTENT_TYPE, "application/json")], body);
        let answer = move |on: bool, body: &'static str| -> Response {
            if on {
                json(body).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        };
        let router = axum::Router::new()
            .route(
                "/api/setup",
                get(move || async move { answer(setup, include_str!("../fixtures/top-setup.json")) }),
            )
            .route(
                "/api/stats",
                get(move |RawQuery(q): RawQuery| async move {
                    asked.lock().unwrap().push(q.unwrap_or_default());
                    json(include_str!("../fixtures/top-stats.json"))
                }),
            )
            .route(
                "/api/map",
                get(move || async move { answer(map, include_str!("../fixtures/top-map.json")) }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, router).await });
        url
    }

    fn loader(base: String, window: usize, stats: bool) -> Loader {
        Loader {
            base,
            // Windows takes a couple of seconds to give up on a closed port
            fetch: Fetch::new(Duration::from_secs(20)).unwrap(),
            ask: ask(window, stats),
        }
    }

    #[tokio::test]
    async fn the_loader_reads_each_endpoint_on_its_own() {
        let asked = Arc::new(std::sync::Mutex::new(vec![]));
        let r = loader(serve(true, false, asked.clone()).await, 0, false)
            .load()
            .await;
        assert!(r.setup.is_ok());
        assert!(
            matches!(r.map, Ok(None)),
            "a 404 from /api/map is nothing worked out yet"
        );
        assert!(
            r.stats.is_none() && asked.lock().unwrap().is_empty(),
            "not asked for"
        );
        let r = loader(serve(true, true, asked.clone()).await, 3, true)
            .load()
            .await;
        assert!(matches!(r.map, Ok(Some(_))));
        assert!(matches!(r.stats, Some(Ok(_))));
        assert_eq!(
            *asked.lock().unwrap(),
            ["window=30d"],
            "the window asked for"
        );
        let r = loader(serve(false, true, asked.clone()).await, 1, false)
            .load()
            .await;
        let e = r.setup.err().unwrap();
        assert!(
            e.ends_with(
                "/api/setup: HTTP 404: no /api/setup there, so it is an older ridgewarden-radar \
                 to upgrade, or not ridgewarden-radar (check --url)"
            ),
            "{e}"
        );
        assert!(matches!(r.map, Ok(Some(_))), "what answered is still read");
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let r = loader(closed.clone(), 1, true).load().await;
        assert_eq!(
            r.setup.err().unwrap(),
            format!(
                "{closed}/api/setup: could not connect: if the app isn't running, start it with \
                 sudo systemctl start ridgewarden-radar; if it runs elsewhere, give its address \
                 with --url"
            )
        );
        assert_eq!(
            r.map.err().unwrap(),
            format!("{closed}/api/map: could not connect")
        );
        assert_eq!(
            r.stats.unwrap().err().unwrap(),
            format!("{closed}/api/stats?window=24h: could not connect")
        );
    }

    #[tokio::test]
    async fn the_view_reads_a_real_app_whose_setup_carries_no_paths() {
        use crate::app::App;
        use crate::config::{Config, EXAMPLE};
        let dir = std::env::temp_dir().join(format!(
            "ridgewarden-radar-top-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let core = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.data_dir = dir.clone();
        cfg.core.url = core;
        cfg.core.token_file = dir.join("local.token");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&cfg.core.token_file, "a-test-token").unwrap();
        let app = Arc::new(App::new(cfg).unwrap());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(l, crate::web::router(app)).await });
        let r = loader(base.clone(), 1, true).load().await;
        let setup = r.setup.unwrap();
        assert!(setup.who.is_none(), "no core is there");
        assert!(setup.stations.is_empty() && setup.site.is_none());
        assert!(matches!(r.map, Ok(None)), "nothing worked out yet");
        assert_eq!(r.stats.unwrap().unwrap().cpis, 0);
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("{base}/api/setup");
        let body = http.get(url).send().await.unwrap().text().await.unwrap();
        let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
        for private in ["local.token", "a-test-token", "radar.db", dir_name.as_str()] {
            assert!(!body.contains(private), "{private} in {body}");
        }
        server.abort();
        // the app, and with it the database, close with the server
        assert!(server.await.unwrap_err().is_cancelled());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The screen as an HTML page of coloured monospace cells, as a dark terminal shows
    /// it (Windows Terminal's default colours), for the README's picture of the view.
    fn html(b: &Buffer) -> String {
        use ridgewarden_client::tui::ratatui::style::{Color as C, Modifier};
        use ridgewarden_client::util::esc;
        let rgb = |c: C, fg: bool| match c {
            C::Reset if fg => "#cccccc",
            C::Reset | C::Black => "#0c0c0c",
            C::Red => "#c50f1f",
            C::Green => "#13a10e",
            C::Yellow => "#c19c00",
            C::Gray => "#cccccc",
            C::White => "#f2f2f2",
            _ => "#cccccc",
        };
        let mut out = String::from(
            "<!doctype html><meta charset=utf-8><style>body{margin:0;background:#0c0c0c}\
             pre{margin:0;padding:14px 16px;display:inline-block;background:#0c0c0c;\
             font:15px/1.3 'Cascadia Mono',Consolas,monospace}</style><pre>",
        );
        let a = b.area;
        for y in 0..a.height {
            let mut x = 0;
            while x < a.width {
                let first = &b[(x, y)];
                let mut run = String::new();
                while x < a.width {
                    let c = &b[(x, y)];
                    if (c.fg, c.bg, c.modifier) != (first.fg, first.bg, first.modifier) {
                        break;
                    }
                    run.push_str(c.symbol());
                    x += 1;
                }
                let (mut fg, mut bg) = (rgb(first.fg, true), rgb(first.bg, false));
                if first.modifier.contains(Modifier::REVERSED) {
                    (fg, bg) = (bg, fg);
                }
                if first.modifier.contains(Modifier::DIM) {
                    fg = "#8a8a8a";
                }
                let bold = if first.modifier.contains(Modifier::BOLD) {
                    ";font-weight:bold"
                } else {
                    ""
                };
                out += &format!(
                    "<span style=\"color:{fg};background:{bg}{bold}\">{}</span>",
                    esc(&run)
                );
            }
            out.push('\n');
        }
        out + "</pre>"
    }

    /// With README_SHOTS=1, writes each page at 80x24, in colour, as
    /// ridgewarden-radar-top-<n>.html in the temp folder, for the README's picture.
    #[test]
    fn readme_shot() {
        let mut t = top(Some(snap()));
        t.theme = Theme { color: true };
        for tab in 0..TABS.len() {
            t.tab = tab;
            let at = t.snap.as_ref().unwrap().at;
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|f| t.draw_at(f, at)).unwrap();
            let page = html(term.backend().buffer());
            assert!(page.contains("ridgewarden-radar"));
            if std::env::var_os("README_SHOTS").is_some_and(|v| v == "1") {
                let file =
                    std::env::temp_dir().join(format!("ridgewarden-radar-top-{}.html", tab + 1));
                std::fs::write(file, page).unwrap();
            }
        }
    }
}
