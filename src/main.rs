use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ridgewarden_radar::app::App;
use ridgewarden_radar::config::{Config, EXAMPLE};
use ridgewarden_radar::sim::{self, Plan, SitePlan};
use ridgewarden_radar::{spawn_loops, web};

#[derive(Parser)]
#[command(
    version,
    about = "Experimental passive radar for AREDN meshes: blah2 detections checked against ADS-B (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/radar.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Read blah2 and the ADS-B feed, keep the statistics, serve the pages
    Run,
    /// Stand in for blah2 and an ADS-B receiver with synthetic aircraft (no config needed)
    Simulate {
        /// a site's blah2 API and aircraft.json; repeat for more sites
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: Vec<SocketAddr>,
        /// each site's receiver, lat,lon,alt_m, in the same order as --listen
        #[arg(long, default_value = "39.50,-104.80,1650")]
        rx: Vec<String>,
        /// transmitter, lat,lon,alt_m: once for all sites, or once per site
        #[arg(long, default_value = "39.62,-104.95,2100")]
        tx: Vec<String>,
        /// illuminator frequency, MHz: once for all sites, or once per site
        #[arg(long, default_value = "96.9")]
        fc_mhz: Vec<f64>,
        /// CPI length, seconds
        #[arg(long, default_value_t = 0.5)]
        cpi: f64,
        /// aircraft with ADS-B
        #[arg(long, default_value_t = 12)]
        aircraft: usize,
        /// aircraft without ADS-B (detected, never in aircraft.json)
        #[arg(long, default_value_t = 2)]
        dark: usize,
        /// mean false alarms per CPI
        #[arg(long, default_value_t = 0.5)]
        false_per_cpi: f64,
        /// bistatic range, km, where the chance of detection drops to about half
        #[arg(long, default_value_t = 35.0)]
        r50_km: f64,
        /// aircraft fly within about this many km of the receivers' middle
        #[arg(long, default_value_t = 60.0)]
        radius_km: f64,
        /// seed for the randomness; random if not given
        #[arg(long)]
        seed: Option<u64>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_radar=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Simulate {
            listen,
            rx,
            tx,
            fc_mhz,
            cpi,
            aircraft,
            dark,
            false_per_cpi,
            r50_km,
            radius_km,
            seed,
        } => {
            if !(0.05..=5.0).contains(&cpi)
                || fc_mhz.iter().any(|f| *f <= 0.0)
                || r50_km <= 0.0
                || !(5.0..=300.0).contains(&radius_km)
                || false_per_cpi < 0.0
            {
                bail!(
                    "--cpi must be 0.05 to 5 s, --radius-km 5 to 300, and --fc-mhz, --r50-km and --false-per-cpi positive"
                );
            }
            let n = listen.len();
            if rx.len() != n {
                bail!("give --rx once per --listen: each site has its own receiver");
            }
            let tx = per_site(tx, n, "--tx")?;
            let fc = per_site(fc_mhz, n, "--fc-mhz")?;
            let sites = (0..n)
                .map(|i| {
                    Ok(SitePlan {
                        rx: sim::parse_point(&rx[i])?,
                        tx: sim::parse_point(&tx[i])?,
                        fc_hz: fc[i] * 1e6,
                    })
                })
                .collect::<Result<_>>()?;
            let plan = Plan {
                sites,
                cpi_s: cpi,
                aircraft,
                dark,
                false_per_cpi,
                r50_km,
                radius_km,
            };
            let seed = match seed {
                Some(s) => s,
                None => getrandom::u64().context("no OS randomness")?,
            };
            tokio::runtime::Runtime::new()?.block_on(sim::serve(plan, seed, &listen))
        }
    }
}

/// A per-site option given either once for every site or once for each.
fn per_site<T: Clone>(v: Vec<T>, sites: usize, name: &str) -> Result<Vec<T>> {
    match v.len() {
        1 => Ok(vec![v[0].clone(); sites]),
        n if n == sites => Ok(v),
        n => bail!("{name} given {n} times for {sites} sites; give it once or once per --listen"),
    }
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-radar {} (experimental) reading blah2 at {} and ADS-B at {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.cfg.blah2.url,
        app.cfg.adsb.url
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
