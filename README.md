# ridgewarden-radar

[![CI](https://github.com/Londopy/ridgewarden-radar/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-radar/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-radar)](https://github.com/Londopy/ridgewarden-radar/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-radar/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-radar)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(x86__64%20%7C%20arm64)-555)](#building-from-source)

**Experimental. How well does a passive radar at a mesh site really see aircraft, and can several sites find one without ADS-B?**

A ridgewarden app for AREDN meshes, and an experiment rather than a finished tool. A passive radar transmits nothing: it listens to an FM, DAB or TV transmitter and to that signal's echoes off aircraft. [blah2](https://github.com/30hours/blah2) does all of the signal processing. This app reads blah2's detections, checks every CPI against the aircraft a nearby ADS-B receiver reports, and keeps an honest record of what the site detects: detection probability by range and by aircraft, the rate of detections that match no aircraft, and how far out detection stays reliable. Detections, never signals or audio, go to the mesh as small telemetry reports. With two or more sites on the mesh it tracks each site's detections and works out where aircraft are from the sites' tracks alone, then says how close that came to ADS-B.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core on the same box. Spec sheet 10 of the AREDN app suite.

## What you need to make it real

Without this hardware the app runs only against its simulator (see below).

- A two-channel coherent receiver that blah2 supports: an SDRplay RSPduo or a KrakenSDR.
- Two antennas: a Yagi on the transmitter for the reference channel, and one on the sky you want to watch for the surveillance channel, ideally shielded from the transmitter.
- A mini PC or similar running blah2. Nobody has shown a Raspberry Pi 5 keeping up; the setup page tells you whether your box does.
- An ADS-B receiver on the same network (PiAware, dump1090-fa, or readsb with tar1090) for the truth feed.
- A site that sees the transmitter well for the reference channel without the direct signal swamping the echoes. Siting decides most of the result, and talk radio or silence on the illuminator weakens detections.

Expect tens of kilometres at best from hobby hardware, and 1.5 to 3 km range resolution from FM.

## What it does

- Reads blah2's `/api/detection` (bistatic range in km, Doppler in Hz, SNR in dB, per CPI) every 250 ms, faster than its CPI, so none are missed, and its `/api/config` for the transmitter and receiver positions, illuminator frequency, CPI and search area, so the two programs can't disagree about the geometry.
- Reads `aircraft.json` from the ADS-B receiver every second. For every CPI it moves each aircraft on to the CPI's time and works out its bistatic range (|Tx-P| + |P-Rx| - |Tx-Rx| on the WGS84 ellipsoid) and bistatic Doppler (from its ground speed, track and vertical rate, at the illuminator's wavelength). The definitions and sign match adsb2dd's, though adsb2dd differentiates successive positions instead of using the reported velocity.
- Matches detections to aircraft inside configurable range and Doppler gates, nearest pairs first, one detection per aircraft.
- Counts an aircraft-CPI only when the aircraft was inside the area blah2 searches and outside its clutter notch, and only while the truth is current: the ADS-B list's own `now` must be within half of `max_age_s` of this box's clock (a dead receiver can leave its web server handing out the last file), and blah2's CPI timestamps must agree with it too. Otherwise every detection would look like a false alarm.
- Starts the statistics again, from the next whole hour, whenever blah2's positions, frequency, CPI or search area change, so numbers from two setups never mix.
- Performance page for the last hour, day, week or 30 days: detection probability by bistatic range bin and by aircraft, how many aircraft were matched in at least `detected_hits` CPIs (a low bar) against the 80% goal, unmatched detections per CPI and per minute, and the reliable range, the furthest range before Pd first drops below a threshold. Range bins and aircraft with too few CPIs in coverage are shown but not judged.
- A range-Doppler plot of the last 1, 5 or 15 minutes: each ADS-B aircraft's expected track, with blah2's detections on top, matched or not.
- A setup page saying in plain words how the radar works, the geometry and processing from blah2's config, whether blah2 keeps up with its CPI, the state of both feeds, and what goes over the mesh.
- Pd by hour of the day, so a transmitter that turns to talk or silence at certain hours shows up as the programme, not the radar.
- `/api/stats?window=24h` returns the statistics as JSON, and `/api/map` the map's current moment.

Unmatched detections are false alarms or aircraft without ADS-B, so the unmatched rate is an upper bound on false alarms. The other way round, a false alarm that lands inside an aircraft's gates counts as a hit; the performance page says how much Pd chance matches alone would give at the current false-alarm rate.

## Several sites: tracking without ADS-B

blah2 leaves its tracker as future work, and earlier multi-site tools matched targets using ADS-B. This app does both parts itself, on every site's reports as they come through the mesh, so any site can act as the hub:

- **A tracker per site.** Each track is a Kalman filter on bistatic range and its rate (blah2's Doppler measures the rate, so one detection starts a track). Detections join tracks by gated nearest neighbour; a track is confirmed after 3 hits in its first 5 CPIs and ends after 6 misses in a row (all in `[tracker]`).
- **Fixes from two or more sites.** A confirmed track only says the aircraft is somewhere on an ellipse around that site's transmitter and receiver. For every pairing of tracks from different sites, a least-squares fit of position, velocity and height over the last minute of their detections finds where the ellipses meet and stay met; a third site's track is added when it fits too. The fit's covariance gives a 1-sigma error ellipse. The height starts from a typical cruising height with a wide allowance (`alt_m`, `alt_sigma_m`), so two-site fixes carry the height uncertainty in their ellipse and three sites pin it down.
- **Ghosts are counted, not hidden.** Two ellipses usually cross twice, and with two sites both crossings often fit about equally well; such fixes are dropped, since the data can't say which is right. Two sites' tracks of different aircraft can also fit one made-up aircraft well. Fits worse than about chance, needing more than 350 m/s, or with a 1-sigma ellipse longer than `max_error_km` are dropped too, but some ghosts remain, mostly with two sites. The performance page scores every fix against ADS-B, separately for two-site and three-site fixes: how many had an ADS-B aircraft within `truth_km`, and the median and 90% error among those (so they can't exceed `truth_km`).
- **Fused tracks.** Fixes are followed from one moment to the next, across gaps of up to a minute; the page counts fused tracks followed for a minute or more with at least three fixes made while ADS-B was current and no ADS-B aircraft near any of them. Those are the candidates for the spec's goal of an aircraft without ADS-B tracked from two or more sites, or long-lived ghosts.
- **Map.** The map page shows each site's transmitter and receiver, the ellipse of every confirmed track, the fixes with their error ellipses, and the ADS-B aircraft at the same moment. It is inline SVG with a small script that refreshes it every 10 seconds (`/map.svg` is the map alone); without JavaScript the page reloads once a minute. Each moment is worked out `settle_s` after it (5 minutes by default, to allow for the report batch plus the core's sync interval), once every site's reports have arrived, so the map runs that far behind live. A moment is worked out once; reports that arrive later are not used for it.
- **Bounded work.** Each site offers at most 20 tracks (the longest) to the fit, reports are capped at 500 detections per CPI like blah2's own feed, and pairing stops after half of `every_s`, so a busy or misbehaving site can't stall the hub.
- Each site logs its geometry (from blah2's config) as a `radar.site` event in the core's main log, so the others can use its reports. Locating needs the sites' clocks to agree to well under a second: run NTP or GPS time on every blah2 box.

Fixes that no ADS-B aircraft explains locate aircraft that public ADS-B doesn't show. They are counted on the performance page but not drawn on the map, listed or returned by `/api/map` unless `fusion.show_unmatched = true`.

## Over the mesh

Every minute (by default) the app sends that minute's detections through the core as one `radar.detections` telemetry event: the start time, illuminator frequency, each CPI's time (so the tracker knows when a site looked and saw nothing) and a list of `[ms since start, range km, Doppler Hz, SNR dB]`. A busy minute is a few kilobytes. No samples, spectra or audio ever leave the box (Part 97.113(c)). The core prunes telemetry after 30 days. The setup page lists every trusted station whose reports reach this core.

Raw detections and expected aircraft positions are kept 15 minutes, and every site's reported detections two hours for the tracker. Multi-site fixes are kept a week. Hourly counts by range are kept 400 days. Hourly counts per aircraft, which carry the ICAO address and callsign from the ADS-B feed, are kept 31 days, just long enough for the 30-day window. Callsigns and addresses appear only as the ADS-B feed itself gives them, and `show_aircraft_ids = false` replaces them with numbers on the pages and in `/api/stats`.

## Install

On a Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-radar/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-radar/releases/latest): `sudo apt install ./ridgewarden-radar_<version>_amd64.deb` (`_arm64` on an ARM board). The install script checks the download against the release key your core already trusts. Either way `/etc/ridgewarden/radar.toml` is written if missing, and the service starts once the core is set up. The pages are on port 8802.

blah2 itself is installed separately, from its own repository (it runs in Docker).

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-radar`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-radar ridgewarden-radar --config /etc/ridgewarden/radar.toml <command>`.

## Usage

```sh
ridgewarden-radar init --config /etc/ridgewarden/radar.toml
ridgewarden-radar run  --config /etc/ridgewarden/radar.toml
```

The pages are on `http://<box>:8802/`. Add them to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

### Simulator

`simulate` stands in for both blah2 and the ADS-B receiver, so the whole app runs without hardware:

```sh
ridgewarden-radar simulate --listen 127.0.0.1:3000 --rx 39.50,-104.80,1650 --tx 39.62,-104.95,2100 --fc-mhz 96.9
```

It serves blah2's `/api/detection`, `/api/config`, `/api/timing` and `/api/timestamp`, and dump1090's `/data/aircraft.json`, in their real formats. Twelve synthetic aircraft (`--aircraft`) fly straight lines within `--radius-km` of the receivers; each CPI one is detected with a chance that falls off with bistatic range (`--r50-km`, where it is about half), with a little range and Doppler noise. There are random false alarms (`--false-per-cpi`) and two aircraft with no ADS-B (`--dark`).

For several sites, give `--listen` and `--rx` once per site (`--tx` and `--fc-mhz` once for all, or once each). Every site sees the same aircraft, so two ridgewarden cores, each with a radar app pointed at its own simulated site, exercise the whole multi-site path:

```sh
ridgewarden-radar simulate --listen 127.0.0.1:3000 --listen 127.0.0.1:3001 \
    --rx 39.50,-104.80,1650 --rx 39.75,-104.70,1600 --tx 39.62,-104.95,2100 --radius-km 30 --aircraft 25
```

Point the app at it:

```toml
[blah2]
url = "http://127.0.0.1:3000"

[adsb]
url = "http://127.0.0.1:3000/data/aircraft.json"
```

## Configuration

`/etc/ridgewarden/radar.toml` (`init` writes a commented one). The geometry, frequency and CPI are not in it; they come from blah2's `config.yml`.

```toml
[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # lets this app send detection reports

[blah2]
url = "http://127.0.0.1:3000"     # blah2's network.ports.api
poll_ms = 250

[adsb]
url = "http://piaware.local.mesh/skyaware/data/aircraft.json"   # tar1090: /tar1090/data/aircraft.json
poll_s = 1
max_age_s = 10                    # older ADS-B positions are not used

[match]
range_gate_km = 2.0
doppler_gate_hz = 8.0

[stats]
bin_km = 5
pd_threshold = 0.8                # reliable range: Pd at or above this
min_opportunities = 30            # CPIs in coverage before a bin or aircraft is judged
detected_hits = 3                 # matched CPIs before an aircraft counts as detected
show_aircraft_ids = true          # false: number the aircraft instead

[mesh]
enabled = true
batch_s = 60
max_per_batch = 1000              # past this the weakest detections are left out

[tracker]
confirm_m = 3                     # confirmed after 3 detections in its first 5 CPIs
confirm_n = 5
drop_after = 6                    # missed CPIs in a row that end a track
range_sigma_km = 0.3              # how far blah2's detections scatter
doppler_sigma_hz = 1.0

[fusion]
enabled = true
every_s = 10
settle_s = 300                    # above mesh.batch_s plus the core's sync interval
window_s = 60                     # seconds of each track one fix uses
alt_m = 6000                      # typical height, and its spread
alt_sigma_m = 3000
area_km = 100                     # search this far from this site's receiver
max_error_km = 5
truth_km = 3                      # a fix this close to an ADS-B aircraft is that aircraft
show_unmatched = false            # draw fixes no ADS-B aircraft explains
```

Both feeds are read over plain HTTP on the local network.

## Limits

- No real echoes yet. Everything is tested against blah2's and dump1090's documented formats, the formats' fixtures and the simulator; the numbers on a real site are what this experiment is for.
- Two-site fixes can be ghosts, and their height is weakly known; the page measures how often against ADS-B rather than claiming otherwise. Three sites are much better. The confirmed tracks' ellipses are drawn whether or not a fix is shown, since they are only the bistatic ranges already sent over the mesh.
- The position fit assumes level, straight flight over its one-minute window, so climbing or turning aircraft come out a little off.
- Siting (the direct signal swamping echoes) can't be fixed in software; Pd by range and by hour of the day show its effect.
- The spec's write-up or paper with a faculty partner isn't software and isn't here.
- Contributing the tracker back to blah2 upstream is for the owner to offer; the tracker here is GPL-3.0 and lives in `src/track.rs`.
- ATSC TV illuminators: blah2 takes any frequency, so they work as far as blah2 and its sample rate allow; nothing ATSC-specific is done here.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-radar
cd ridgewarden-radar
cargo build --release
cargo test
```

The tests use fixtures of blah2's and dump1090's real JSON formats in `fixtures/`.

## Contributing

Issues and pull requests are welcome, especially from anyone running blah2 against real aircraft. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
