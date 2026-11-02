//! One finger on the surface (Python: `class Touch`), plus the Python numerics the Engine needs
//! to come out bit for bit like v1.4.1: `math.hypot`, `sum()` over floats, `min`/`max`, `**`,
//! `round()` and an insertion-ordered dict. Owner: Henry.
use std::collections::VecDeque;

/// seconds; "is this finger moving" looks this far back
pub const MOVE_WINDOW: f64 = 0.12;

/// Python `math.hypot(x, y)` (CPython `vector_norm`: lossless scaling and squaring, then one
/// correction step). Not always the same last bit as libm's `hypot`.
pub fn hypot(x: f64, y: f64) -> f64 {
    let vec = [x.abs(), y.abs()];
    if vec[0].is_infinite() || vec[1].is_infinite() {
        return f64::INFINITY;
    }
    if vec[0].is_nan() || vec[1].is_nan() {
        return f64::NAN;
    }
    let mut max = 0.0;
    for &v in &vec {
        if v > max {
            max = v;
        }
    }
    vector_norm(vec, max)
}

fn vector_norm(mut vec: [f64; 2], max: f64) -> f64 {
    if max == 0.0 {
        return max;
    }
    let max_e = frexp_exp(max);
    if max_e < -1023 {
        for v in vec.iter_mut() {
            *v /= f64::MIN_POSITIVE;
        }
        return f64::MIN_POSITIVE * vector_norm(vec, max / f64::MIN_POSITIVE);
    }
    let scale = ldexp1(-max_e);
    let (mut csum, mut frac1, mut frac2) = (1.0f64, 0.0f64, 0.0f64);
    for &v in &vec {
        let x = v * scale;
        let (hi, lo) = dl_mul(x, x);
        let (shi, slo) = dl_fast_sum(csum, hi);
        csum = shi;
        frac1 += lo;
        frac2 += slo;
    }
    let mut h = (csum - 1.0 + (frac1 + frac2)).sqrt();
    let (hi, lo) = dl_mul(-h, h);
    let (shi, slo) = dl_fast_sum(csum, hi);
    csum = shi;
    frac1 += lo;
    frac2 += slo;
    let x = csum - 1.0 + (frac1 + frac2);
    h += x / (2.0 * h);
    h / scale
}

fn dl_mul(x: f64, y: f64) -> (f64, f64) {
    let hi = x * y;
    (hi, x.mul_add(y, -hi))
}

fn dl_fast_sum(a: f64, b: f64) -> (f64, f64) {
    let hi = a + b;
    (hi, (a - hi) + b)
}

/// The exponent of C `frexp` (x = m * 2^e, 0.5 <= m < 1) for finite x > 0.
fn frexp_exp(x: f64) -> i32 {
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32;
    if e == 0 {
        return frexp_exp(x * 18014398509481984.0) - 54; // subnormal: scale by 2^54 first
    }
    e - 1022
}

/// 2^k for -1022 <= k <= 1023 (C `ldexp(1.0, k)`).
fn ldexp1(k: i32) -> f64 {
    f64::from_bits(((k + 1023) as u64) << 52)
}

/// Python `sum()` over floats (3.12+: Neumaier compensated, starting from the int 0).
pub fn fsum<I: IntoIterator<Item = f64>>(items: I) -> f64 {
    let mut it = items.into_iter();
    let mut f = match it.next() {
        None => return 0.0,
        Some(x) => 0.0 + x,
    };
    let mut c = 0.0f64;
    for x in it {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// Python `max(a, b)`: a unless b is greater.
pub fn pmax(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Python `min(a, b)`: a unless b is smaller.
pub fn pmin(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

extern "C" {
    #[link_name = "pow"]
    fn c_pow(x: f64, y: f64) -> f64;
}

/// Python float `**` (C `pow`), called through a pointer so the compiler cannot turn
/// `pow(x, 2.0)` into `x * x` (libm's pow is not always the correctly rounded square).
pub fn ppow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    let f: unsafe extern "C" fn(f64, f64) -> f64 = std::hint::black_box(c_pow);
    // SAFETY: libm pow, pure.
    unsafe { f(x, y) }
}

/// Python `round(x)` for a float: nearest int, ties to even.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python `round(x, n)` for a float (correctly rounded decimal, then back to a float).
pub fn round_to(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// A Python dict: insertion-ordered, an update keeps the key's place, a delete closes the gap.
/// The Engine's maps hold a handful of entries; order decides ties like it does in Python.
#[derive(Clone, Debug, PartialEq)]
pub struct OMap<K, V> {
    items: Vec<(K, V)>,
}

impl<K: PartialEq + Copy, V> Default for OMap<K, V> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<K: PartialEq + Copy, V> OMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }
    fn pos(&self, k: K) -> Option<usize> {
        self.items.iter().position(|(a, _)| *a == k)
    }
    pub fn get(&self, k: K) -> Option<&V> {
        self.pos(k).map(|i| &self.items[i].1)
    }
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        self.pos(k).map(move |i| &mut self.items[i].1)
    }
    pub fn contains_key(&self, k: K) -> bool {
        self.pos(k).is_some()
    }
    /// `d[k] = v`
    pub fn insert(&mut self, k: K, v: V) {
        match self.pos(k) {
            Some(i) => self.items[i].1 = v,
            None => self.items.push((k, v)),
        }
    }
    /// `d.pop(k, None)`
    pub fn remove(&mut self, k: K) -> Option<V> {
        self.pos(k).map(|i| self.items.remove(i).1)
    }
    /// `d.setdefault(k, v)`
    pub fn setdefault(&mut self, k: K, v: V) -> &mut V {
        let i = match self.pos(k) {
            Some(i) => i,
            None => {
                self.items.push((k, v));
                self.items.len() - 1
            }
        };
        &mut self.items[i].1
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn keys(&self) -> Vec<K> {
        self.items.iter().map(|(k, _)| *k).collect()
    }
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.items.iter().map(|(k, v)| (*k, v))
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (K, &mut V)> {
        self.items.iter_mut().map(|(k, v)| (*k, v))
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.items.iter().map(|(_, v)| v)
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.items.iter_mut().map(|(_, v)| v)
    }
    /// `{k: v for k, v in d.items() if keep(k)}`
    pub fn retain(&mut self, mut keep: impl FnMut(K) -> bool) {
        self.items.retain(|(k, _)| keep(*k));
    }
}

impl<K: PartialEq + Copy + std::fmt::Debug, V> std::ops::Index<K> for OMap<K, V> {
    type Output = V;
    fn index(&self, k: K) -> &V {
        self.get(k).unwrap_or_else(|| panic!("KeyError: {k:?}"))
    }
}

/// One finger on the surface, from landing to lift.
#[derive(Clone, Debug, PartialEq)]
pub struct Touch {
    pub x: i64,
    pub y: i64,
    pub x0: i64,
    pub y0: i64,
    /// Engine.ptr_total when the finger landed
    pub ptr0: f64,
    pub grip: bool,
    pub palm: bool,
    pub hist: VecDeque<(f64, i64, i64)>,
    /// big, closely spaced reports in a row (a real touch, not a hover)
    pub firm: i64,
    /// confirmed as a touch: it may scroll
    pub ok: bool,
    pub last_rep: Option<f64>,
    pub size: i64,
    /// this finger's reports per second, smoothed (for the preview)
    pub rate: f64,
    /// distance travelled along the way, in touch units
    pub path: f64,
    /// landed in an edge zone (grip may be cleared later)
    pub edge0: bool,
    /// the mouse's own contact state when it tells one (FW_TOUCHING), else -1
    pub fw: i64,
}

impl Touch {
    pub fn new(ts: f64, x: i64, y: i64, grip: bool, palm: bool) -> Self {
        Touch {
            x,
            y,
            x0: x,
            y0: y,
            ptr0: 0.0,
            grip: grip || palm,
            palm,
            hist: VecDeque::from([(ts, x, y)]),
            firm: 0,
            ok: false,
            last_rep: None,
            size: 0,
            rate: 0.0,
            path: 0.0,
            edge0: grip && !palm,
            fw: -1,
        }
    }

    pub fn update(&mut self, ts: f64, x: i64, y: i64) {
        self.path += hypot((x - self.x) as f64, (y - self.y) as f64);
        self.x = x;
        self.y = y;
        self.hist.push_back((ts, x, y));
        while self.hist.len() > 2 && self.hist[1].0 <= ts - MOVE_WINDOW {
            self.hist.pop_front();
        }
    }

    /// Displacement in touch units over (about) the last MOVE_WINDOW.
    pub fn recent(&self) -> (i64, i64) {
        let (_, x, y) = self.hist[0];
        (self.x - x, self.y - y)
    }

    /// Distance from start to end over path length, over (about) the last MOVE_WINDOW:
    /// 1 for a straight slide, near 0 for a wobble on the spot. 1 while it has not moved.
    pub fn straightness(&self) -> f64 {
        let path = fsum(
            self.hist
                .iter()
                .zip(self.hist.iter().skip(1))
                .map(|(a, b)| hypot((b.1 - a.1) as f64, (b.2 - a.2) as f64)),
        );
        let (rx, ry) = self.recent();
        if path > 0.0 {
            hypot(rx as f64, ry as f64) / path
        } else {
            1.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hypot_like_python() {
        assert_eq!(hypot(3.0, 4.0), 5.0);
        assert_eq!(hypot(0.0, -7.0), 7.0);
        assert_eq!(hypot(0.0, 0.0), 0.0);
    }

    #[test]
    fn sum_is_compensated() {
        assert_eq!(fsum([0.1; 10]), 1.0); // plain left-to-right gives 0.9999999999999999
        assert_eq!(fsum([]), 0.0);
    }

    #[test]
    fn rounding() {
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(3.5), 4);
        assert_eq!(round_int(-0.5), 0);
        assert_eq!(round_to(100.01149999, 4), 100.0115);
        assert_eq!(ppow(3.0, 2.0), 9.0);
    }

    #[test]
    fn omap_keeps_python_order() {
        let mut m = OMap::new();
        m.insert(3, "a");
        m.insert(1, "b");
        m.insert(3, "c");
        assert_eq!(m.keys(), vec![3, 1]);
        m.remove(3);
        m.insert(3, "d");
        assert_eq!(m.keys(), vec![1, 3]);
    }

    #[test]
    fn touch_history_window() {
        let mut t = Touch::new(0.0, 0, 0, false, false);
        for i in 1..30 {
            t.update(i as f64 * 0.011, 0, 10 * i);
        }
        assert!(t.hist.len() > 2);
        assert!(t.hist[1].0 > 29.0 * 0.011 - MOVE_WINDOW);
        assert_eq!(t.straightness(), 1.0);
        assert_eq!(t.path, 290.0);
    }
}
