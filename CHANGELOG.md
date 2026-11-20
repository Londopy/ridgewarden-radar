# Changelog

All notable changes to ridgewarden-radar are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-radar`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Experimental single-site passive radar: reads blah2's detections, config and timing from its web API.
- ADS-B truth from dump1090-fa, readsb or tar1090 `aircraft.json`, with each aircraft's expected bistatic range and Doppler worked out for blah2's transmitter and receiver.
- Detections matched to aircraft within configurable range and Doppler gates; CPIs are not counted while the ADS-B list is stale or blah2's clock disagrees with this box's.
- Statistics start again from the next hour when blah2's positions, frequency, CPI or search area change.
- Performance page: detection probability by bistatic range and by aircraft with the share chance matches alone would give, aircraft seen against the 80% goal, unmatched detections per CPI and per minute, and the reliable range, over the last hour, day, week or 30 days.
- Range-Doppler plot of the last 1, 5 or 15 minutes of detections against ADS-B tracks.
- Setup page describing the geometry, processing, feed health and whether blah2 keeps up with its CPI.
- Detection reports over the mesh as `radar.detections` telemetry (range, Doppler, SNR and time only), and a list of every station whose reports arrive.
- `show_aircraft_ids` to hide callsigns and ICAO addresses.
- `simulate` subcommand that serves blah2's API and an `aircraft.json` for synthetic aircraft, with misses, false alarms and aircraft without ADS-B.
- `/api/stats` with the statistics as JSON.
- A tracker for each site's detections (Kalman filter on bistatic range and range rate, gated nearest-neighbour association, M-of-N confirmation), run on every site's reports from the mesh.
- Locating aircraft from two or more sites without ADS-B: least-squares position, velocity and height with a 1-sigma error ellipse, association of tracks across sites, and fused tracks followed from moment to moment.
- Multi-site accuracy on the performance page: fixes scored against ADS-B for two-site and three-site fixes, median and 90% error, and fused tracks with no ADS-B. Two-site fixes where both crossings of the ellipses fit alike are dropped.
- Map page with each site's track ellipses, the fixes and their error ellipses, and ADS-B aircraft, refreshed every 10 seconds; `/api/map` as JSON.
- `radar.site` events carrying each site's geometry, and each CPI's time in `radar.detections` reports.
- Pd by hour of the day.
- `simulate` serves several sites seeing the same aircraft (`--listen` and `--rx` once per site), and `--radius-km`.

[Unreleased]: https://github.com/Londopy/ridgewarden-radar/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-radar/releases/tag/v0.1.0
