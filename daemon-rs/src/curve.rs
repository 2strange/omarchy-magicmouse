//! Apple's pointer acceleration (Python: `APPLE_CURVES_FIXED`, `AppleCurve`), straight from
//! IOHIDFamily (IOHIDAccelerationAlgorithm.cpp, IOHIDParametricAcceleration). Owner: Henry.
use crate::touch::{pmax, ppow};

/// Ten stock curves indexed by the "Tracking speed" setting; the driver linearly interpolates
/// between the two nearest. IOFixed (x / 65536):
/// (Index, GainLinear, GainParabolic, GainCubic, GainQuartic, TangentSpeedLinear, TangentSpeedParabolicRoot)
pub const APPLE_CURVES_FIXED: [[i64; 7]; 10] = [
    [0, 65536, 0, 0, 0, 524288, 0],
    [8192, 60293, 26214, 5243, 0, 537395, 1245184],
    [32768, 60948, 36045, 6554, 0, 543949, 1179648],
    [45056, 61604, 46531, 7864, 0, 550502, 1114112],
    [57344, 62259, 57672, 9830, 0, 557056, 1048576],
    [65536, 62915, 69468, 11796, 0, 563610, 983040],
    [98304, 63570, 81920, 14418, 0, 570163, 917504],
    [131072, 64225, 95027, 17695, 0, 576717, 851968],
    [163840, 64881, 108790, 21627, 0, 583270, 786432],
    [196608, 65536, 123208, 26214, 0, 589824, 786432],
];
/// Apple maps curve output (cursor inches/s) to pixels at 96 px per inch
pub const MAC_CURSOR_PPI: f64 = 96.0;

/// f(x): mouse speed in inches/s -> cursor speed in inches/s (before the 96 ppi scale).
/// Three segments, exactly as Apple's driver: polynomial up to TangentSpeedLinear,
/// then its tangent line up to TangentSpeedParabolicRoot, then a square-root tail.
#[derive(Clone, Debug, PartialEq)]
pub struct AppleCurve {
    pub gl: f64,
    gp: f64,
    gc: f64,
    gq: f64,
    pub tl: f64,
    pub t0: f64,
    pub t1: f64,
    pub m0: f64,
    pub b0: f64,
    pub m1: f64,
    pub b1: f64,
}

impl AppleCurve {
    pub fn new(tracking: f64) -> Self {
        let curves: Vec<[f64; 7]> = APPLE_CURVES_FIXED
            .iter()
            .map(|c| c.map(|v| v as f64 / 65536.0))
            .collect();
        let tracking = pmax(0.0, tracking);
        let mut cur = 0;
        for (i, c) in curves.iter().enumerate() {
            if tracking >= c[0] {
                cur = i;
            }
        }
        let c0 = curves[cur];
        let c = if c0[0] < tracking && cur + 1 < curves.len() {
            let c1 = curves[cur + 1];
            let r = (tracking - c0[0]) / (c1[0] - c0[0]);
            let mut c = [0.0; 7];
            for i in 0..7 {
                c[i] = c0[i] + r * (c1[i] - c0[i]);
            }
            c
        } else {
            c0
        };
        let [_, gl, gp, gc, gq, tl, tp] = c;
        let mut s = AppleCurve {
            gl,
            gp,
            gc,
            gq,
            tl,
            t0: f64::INFINITY,
            t1: f64::INFINITY,
            m0: 0.0,
            b0: 0.0,
            m1: 0.0,
            b1: 0.0,
        };
        if tl != 0.0 {
            let y0 = s.poly(tl);
            s.m0 = s.dpoly(tl);
            s.b0 = y0 - s.m0 * tl;
            s.t0 = tl;
            if tp != 0.0 {
                let y1 = s.m0 * tp + s.b0;
                s.m1 = 2.0 * y1 * s.m0;
                s.b1 = y1 * y1 - s.m1 * tp;
                s.t1 = tp;
            }
        } else if tp != 0.0 {
            let y0 = s.poly(tp);
            s.m1 = s.dpoly(tp);
            s.b1 = y0 * y0 - s.m1 * tp;
            s.t0 = tp;
        }
        s
    }

    /// gl * x + (gp * x) ** 2 + (gc * x) ** 3 + (gq * x) ** 4
    pub fn poly(&self, x: f64) -> f64 {
        self.gl * x
            + ppow(self.gp * x, 2.0)
            + ppow(self.gc * x, 3.0)
            + ppow(self.gq * x, 4.0)
    }

    /// gl + 2 * x * gp**2 + 3 * x * x * gc**3 + 4 * x**3 * gq**4
    fn dpoly(&self, x: f64) -> f64 {
        self.gl
            + 2.0 * x * ppow(self.gp, 2.0)
            + 3.0 * x * x * ppow(self.gc, 3.0)
            + 4.0 * ppow(x, 3.0) * ppow(self.gq, 4.0)
    }

    pub fn f(&self, x: f64) -> f64 {
        if x <= self.t0 {
            return self.poly(x);
        }
        if x <= self.t1 && self.t0 == self.tl {
            return self.m0 * x + self.b0;
        }
        pmax(self.m1 * x + self.b1, 0.0).sqrt()
    }

    /// Cursor inches per mouse inch at mouse speed x (inches/s).
    pub fn gain(&self, x: f64) -> f64 {
        if x < 1e-6 {
            return self.gl;
        }
        self.f(x) / x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tracking_is_the_stock_curve() {
        let c = AppleCurve::new(0.6875);
        // 0.6875 * 65536 = 45056: exactly curve 3, no interpolation
        assert_eq!(c.gl, 61604.0 / 65536.0);
        assert_eq!(c.t0, 550502.0 / 65536.0);
        assert_eq!(c.t1, 1114112.0 / 65536.0);
        assert_eq!(c.gain(0.0), c.gl);
        // continuous at the segment joints
        let e = 1e-9;
        assert!((c.f(c.t0) - c.f(c.t0 + e)).abs() < 1e-6);
        assert!((c.f(c.t1) - c.f(c.t1 + e)).abs() < 1e-6);
    }

    #[test]
    fn zero_tracking_is_linear() {
        let c = AppleCurve::new(-1.0);
        assert_eq!(c.gl, 1.0);
        assert_eq!(c.gain(3.0), 1.0);
        assert!(c.t1.is_infinite());
    }

    #[test]
    fn interpolates_between_curves() {
        let a = AppleCurve::new(0.5);
        let b = AppleCurve::new(0.6875);
        let m = AppleCurve::new(0.6);
        assert!(m.gain(5.0) > a.gain(5.0) && m.gain(5.0) < b.gain(5.0));
    }
}
