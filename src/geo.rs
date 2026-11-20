//! WGS84 positions as ECEF vectors, and the bistatic range and Doppler of a moving target
//! for one transmitter and receiver. The same model adsb2dd uses to check blah2.

pub type V3 = [f64; 3];

pub const C: f64 = 299_792_458.0;
const A: f64 = 6_378_137.0;
const F: f64 = 1.0 / 298.257_223_563;

pub const FT: f64 = 0.3048;
pub const KT: f64 = 1852.0 / 3600.0;

pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn scale(a: V3, k: f64) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}

pub fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}

/// Latitude and longitude in degrees, height in metres above the WGS84 ellipsoid.
pub fn ecef(lat: f64, lon: f64, h: f64) -> V3 {
    let e2 = F * (2.0 - F);
    let (sla, cla) = lat.to_radians().sin_cos();
    let (slo, clo) = lon.to_radians().sin_cos();
    let n = A / (1.0 - e2 * sla * sla).sqrt();
    [
        (n + h) * cla * clo,
        (n + h) * cla * slo,
        (n * (1.0 - e2) + h) * sla,
    ]
}

/// A local east/north/up vector at `lat`, `lon` turned into ECEF axes.
pub fn enu_to_ecef(lat: f64, lon: f64, [e, n, u]: V3) -> V3 {
    let (sla, cla) = lat.to_radians().sin_cos();
    let (slo, clo) = lon.to_radians().sin_cos();
    [
        -slo * e - sla * clo * n + cla * clo * u,
        clo * e - sla * slo * n + cla * slo * u,
        cla * n + sla * u,
    ]
}

/// ECEF velocity in m/s from ADS-B ground speed (knots), true track (degrees) and
/// vertical rate (feet per minute).
pub fn velocity(lat: f64, lon: f64, gs_kt: f64, track_deg: f64, vrate_fpm: f64) -> V3 {
    let v = gs_kt * KT;
    let (s, c) = track_deg.to_radians().sin_cos();
    enu_to_ecef(lat, lon, [v * s, v * c, vrate_fpm * FT / 60.0])
}

/// Distance in km between two ECEF points, leaving out the height difference as seen
/// from `lat`, `lon`.
pub fn horizontal_km(lat: f64, lon: f64, a: V3, b: V3) -> f64 {
    let d = sub(a, b);
    let up = enu_to_ecef(lat, lon, [0.0, 0.0, 1.0]);
    norm(sub(d, scale(up, dot(d, up)))) / 1000.0
}

/// A transmitter and receiver, and the illuminator's wavelength.
#[derive(Clone, Copy, Debug)]
pub struct Pair {
    pub tx: V3,
    pub rx: V3,
    pub lambda: f64,
}

impl Pair {
    pub fn new(tx: V3, rx: V3, fc_hz: f64) -> Self {
        Self {
            tx,
            rx,
            lambda: C / fc_hz,
        }
    }

    pub fn baseline(&self) -> f64 {
        norm(sub(self.tx, self.rx))
    }

    /// Bistatic range in metres: |Tx-P| + |P-Rx| - |Tx-Rx|, so zero on the baseline.
    pub fn range(&self, p: V3) -> f64 {
        norm(sub(p, self.tx)) + norm(sub(p, self.rx)) - self.baseline()
    }

    /// Bistatic Doppler in Hz of a target at `p` moving at `v` m/s. Positive while the
    /// path is getting shorter, the sign blah2 and adsb2dd use.
    pub fn doppler(&self, p: V3, v: V3) -> f64 {
        let to_tx = sub(p, self.tx);
        let to_rx = sub(p, self.rx);
        let rate = dot(v, to_tx) / norm(to_tx) + dot(v, to_rx) / norm(to_rx);
        -rate / self.lambda
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "{a} vs {b}");
    }

    #[test]
    fn ecef_of_known_points() {
        let p = ecef(0.0, 0.0, 0.0);
        close(p[0], 6_378_137.0, 1e-6);
        close(p[1], 0.0, 1e-6);
        let p = ecef(0.0, 90.0, 100.0);
        close(p[1], 6_378_237.0, 1e-6);
        close(p[0], 0.0, 1e-6);
        // the polar radius, b = a(1 - f)
        close(ecef(90.0, 0.0, 0.0)[2], 6_356_752.314_245, 1e-3);
        // height moves straight along the local vertical
        let a = ecef(35.0, -120.0, 0.0);
        let b = ecef(35.0, -120.0, 3048.0);
        close(norm(sub(b, a)), 3048.0, 1e-6);
    }

    #[test]
    fn horizontal_distance_ignores_height() {
        let a = ecef(40.0, -105.0, 0.0);
        let b = ecef(40.0, -105.0, 9000.0);
        assert!(horizontal_km(40.0, -105.0, a, b) < 1e-6);
        let c = ecef(40.0 + 0.1, -105.0, 9000.0);
        close(horizontal_km(40.0, -105.0, a, c), 11.10, 0.02);
    }

    #[test]
    fn enu_axes_at_the_origin() {
        // at 0,0: east is +y, north is +z, up is +x
        let v = velocity(0.0, 0.0, 100.0, 90.0, 0.0);
        close(v[0], 0.0, 1e-9);
        close(v[1], 51.444_444, 1e-5);
        close(v[2], 0.0, 1e-9);
        let v = velocity(0.0, 0.0, 0.0, 0.0, 6000.0);
        close(v[0], 30.48, 1e-9);
    }

    #[test]
    fn bistatic_range_and_doppler_by_hand() {
        // Tx at the origin, Rx 10 km along x, target 10 km along y from Tx
        let pair = Pair::new([0.0; 3], [10_000.0, 0.0, 0.0], 100e6);
        let p = [0.0, 10_000.0, 0.0];
        // 10000 + sqrt(2)*10000 - 10000
        close(pair.range(p), 14_142.135_6, 1e-3);
        close(pair.range([4_000.0, 0.0, 0.0]), 0.0, 1e-9);
        // flying at 100 m/s straight at Tx: -100 towards Tx, -100/sqrt(2) towards Rx,
        // so the path shortens at 170.71 m/s; lambda = 2.99792458 m
        let fd = pair.doppler(p, [0.0, -100.0, 0.0]);
        close(fd, 170.710_678 / 2.997_924_58, 1e-6);
        close(fd, 56.943, 1e-3);
        // crossing the baseline at right angles changes neither path
        close(
            pair.doppler([5_000.0, 0.0, 0.0], [0.0, 0.0, 50.0]),
            0.0,
            1e-9,
        );
    }

    #[test]
    fn monostatic_case_doubles_the_path() {
        let pair = Pair::new([0.0; 3], [0.0; 3], 200e6);
        let p = [10_000.0, 0.0, 0.0];
        close(pair.range(p), 20_000.0, 1e-9);
        // receding at 100 m/s: the path grows at 200 m/s
        close(
            pair.doppler(p, [100.0, 0.0, 0.0]),
            -200.0 / (C / 200e6),
            1e-9,
        );
    }

    #[test]
    fn aircraft_over_the_receiver() {
        // Tx about 20 km north of Rx on the ground, aircraft 3048 m (10000 ft) over Rx.
        // Flat-earth estimate: 3048 + sqrt(20000^2 + 3048^2) - 20000 = 3279 m; the
        // earth's curve over 20 km moves it by tens of metres at most.
        let rx = ecef(40.0, -105.0, 0.0);
        let tx = ecef(40.0 + 20_000.0 / 111_034.0, -105.0, 0.0);
        let pair = Pair::new(tx, rx, 98e6);
        close(pair.baseline(), 20_000.0, 30.0);
        let p = ecef(40.0, -105.0, 3048.0);
        close(pair.range(p), 3279.0, 40.0);
        // flying north at 400 kt, level: no change in the Rx path (perpendicular), and the
        // Tx path shortens at about v * 20000/20231
        let v = velocity(40.0, -105.0, 400.0, 0.0, 0.0);
        let expect = 400.0 * KT * 20_000.0 / 20_231.0 / (C / 98e6);
        close(pair.doppler(p, v), expect, 1.0);
    }
}
