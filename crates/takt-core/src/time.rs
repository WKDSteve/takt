// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Rationale Zeitbasis und exakte Zeitstempel.

use std::cmp::Ordering;
use std::fmt;

/// Ein Bruch aus zwei ganzen Zahlen, immer gekuerzt und mit positivem Nenner.
///
/// Entspricht konzeptionell `AVRational` aus FFmpeg. Wird fuer Zeitbasen
/// (`1/48000`), Bildwiederholraten (`24000/1001`) und Seitenverhaeltnisse
/// verwendet.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rational {
    num: i64,
    den: i64,
}

impl Rational {
    /// Millisekunden — die Zeitbasis von Matroska und vielen UI-Schichten.
    pub const MILLIS: Self = Self { num: 1, den: 1_000 };
    /// 90 kHz — die Zeitbasis von MPEG-TS und RTP.
    pub const MPEG: Self = Self {
        num: 1,
        den: 90_000,
    };
    /// Mikrosekunden — die interne Zeitbasis von FFmpeg (`AV_TIME_BASE`).
    pub const MICROS: Self = Self {
        num: 1,
        den: 1_000_000,
    };
    /// Nanosekunden — die Zeitbasis der meisten Betriebssystem-Uhren.
    pub const NANOS: Self = Self {
        num: 1,
        den: 1_000_000_000,
    };

    /// Erzeugt einen gekuerzten Bruch.
    ///
    /// Gibt `None` zurueck, wenn der Nenner null ist oder die Normalisierung
    /// ueberlaufen wuerde (`i64::MIN`).
    #[must_use]
    pub const fn new(num: i64, den: i64) -> Option<Self> {
        if den == 0 || num == i64::MIN || den == i64::MIN {
            return None;
        }
        // Nenner immer positiv, damit Vergleiche vorzeichensicher sind.
        let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
        let g = gcd(num.unsigned_abs(), den.unsigned_abs()) as i64;
        Some(Self {
            num: num / g,
            den: den / g,
        })
    }

    /// Wie [`Rational::new`], bricht aber bei ungueltigen Werten ab.
    ///
    /// Nur fuer Konstanten im Quelltext gedacht, nie fuer Daten aus Dateien.
    ///
    /// # Panics
    /// Wenn `den == 0` ist oder ein Wert `i64::MIN` betraegt.
    #[must_use]
    pub const fn lit(num: i64, den: i64) -> Self {
        match Self::new(num, den) {
            Some(r) => r,
            None => panic!("ungueltige Rational-Konstante"),
        }
    }

    /// Zaehler des gekuerzten Bruchs.
    #[must_use]
    pub const fn num(self) -> i64 {
        self.num
    }

    /// Nenner des gekuerzten Bruchs, immer positiv.
    #[must_use]
    pub const fn den(self) -> i64 {
        self.den
    }

    /// Kehrwert. `None`, wenn der Zaehler null ist.
    #[must_use]
    pub const fn recip(self) -> Option<Self> {
        Self::new(self.den, self.num)
    }

    /// Zeitbasis, die zu dieser Bildwiederholrate gehoert.
    ///
    /// `24000/1001 fps` ergibt die Zeitbasis `1001/24000`.
    #[must_use]
    pub const fn as_time_base(self) -> Option<Self> {
        self.recip()
    }

    /// Naeherung als Gleitkommazahl. **Nur zur Anzeige.**
    ///
    /// Das Ergebnis darf nicht in eine Zeitrechnung zurueckfliessen.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

impl fmt::Debug for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// Groesster gemeinsamer Teiler, binaerer Algorithmus ohne Division.
const fn gcd(mut a: u64, mut b: u64) -> u64 {
    if a == 0 {
        return if b == 0 { 1 } else { b };
    }
    if b == 0 {
        return a;
    }
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Greatest common divisor for the reduction in `rescale`; `1` for `(0, 0)`.
fn gcd128(mut a: u128, mut b: u128) -> i128 {
    if a == 0 && b == 0 {
        return 1;
    }
    while b != 0 {
        (a, b) = (b, a % b);
    }
    // Inputs are at most 2^63 in magnitude, so the result fits.
    a as i128
}

/// Ein exakter Zeitpunkt: ganzzahlige Ticks in einer rationalen Zeitbasis.
///
/// Zwei Zeitstempel mit unterschiedlicher Basis sind vergleichbar und
/// verrechenbar. Der Vergleich ([`Ord`]) ist fuer jedes Paar von Zeitstempeln
/// exakt und ueberlaeuft nie (192-Bit-Zwischenwerte). [`Timestamp::rescale`]
/// und [`Timestamp::diff`] rechnen in `i128` und liefern `None`, wo ein
/// Zwischenwert oder das Ergebnis nicht darstellbar ist.
#[derive(Clone, Copy, Eq)]
pub struct Timestamp {
    ticks: i64,
    base: Rational,
}

impl Timestamp {
    /// Neuer Zeitstempel.
    #[must_use]
    pub const fn new(ticks: i64, base: Rational) -> Self {
        Self { ticks, base }
    }

    /// Nullpunkt in der angegebenen Basis.
    #[must_use]
    pub const fn zero(base: Rational) -> Self {
        Self { ticks: 0, base }
    }

    /// Zeitstempel aus Millisekunden.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self::new(ms, Rational::MILLIS)
    }

    /// Zeitstempel aus Nanosekunden.
    #[must_use]
    pub const fn from_nanos(ns: i64) -> Self {
        Self::new(ns, Rational::NANOS)
    }

    /// Zeitstempel aus einer Sample-Position bei gegebener Abtastrate.
    ///
    /// Die natuerliche Zeitbasis von Audio: Sample 480 bei 48 kHz ist exakt
    /// 10 ms, ohne jede Rundung.
    #[must_use]
    pub const fn from_samples(sample: i64, sample_rate: u32) -> Option<Self> {
        match Rational::new(1, sample_rate as i64) {
            Some(base) => Some(Self::new(sample, base)),
            None => None,
        }
    }

    /// Rohe Tick-Anzahl in der eigenen Basis.
    #[must_use]
    pub const fn ticks(self) -> i64 {
        self.ticks
    }

    /// Zeitbasis dieses Zeitstempels.
    #[must_use]
    pub const fn base(self) -> Rational {
        self.base
    }

    /// Rechnet in eine andere Zeitbasis um, kaufmaennisch gerundet.
    ///
    /// Gibt `None` zurueck, wenn das Ergebnis nicht in `i64` passt oder ein
    /// Zwischenwert `i128` sprengt (nur bei Zeitbasen nahe `i64::MAX`). Vor der
    /// Multiplikation werden gemeinsame Teiler gekuerzt, der Wert bleibt exakt.
    #[must_use]
    pub fn rescale(self, to: Rational) -> Option<Self> {
        if self.base == to {
            return Some(self);
        }
        // ticks * (base.num/base.den) / (to.num/to.den)
        //   = ticks * base.num * to.den / (base.den * to.num)
        let mut n = [
            i128::from(self.ticks),
            i128::from(self.base.num()),
            i128::from(to.den()),
        ];
        let mut d = [i128::from(self.base.den()), i128::from(to.num())];
        for x in &mut n {
            for y in &mut d {
                let g = gcd128(x.unsigned_abs(), y.unsigned_abs());
                *x /= g;
                *y /= g;
            }
        }
        let numer = n[0].checked_mul(n[1])?.checked_mul(n[2])?;
        let denom = d[0].checked_mul(d[1])?;
        let rounded = div_round_nearest(numer, denom)?;
        let ticks = i64::try_from(rounded).ok()?;
        Some(Self::new(ticks, to))
    }

    /// Differenz zu `other`, ausgedrueckt in der Basis von `self`.
    #[must_use]
    pub fn diff(self, other: Self) -> Option<i64> {
        let o = other.rescale(self.base)?;
        self.ticks.checked_sub(o.ticks)
    }

    /// Verschiebt den Zeitstempel um `ticks` in seiner eigenen Basis.
    #[must_use]
    pub fn offset(self, ticks: i64) -> Option<Self> {
        Some(Self::new(self.ticks.checked_add(ticks)?, self.base))
    }

    /// Naeherung in Sekunden. **Nur zur Anzeige.**
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_secs_f64(self) -> f64 {
        self.ticks as f64 * self.base.as_f64()
    }

    /// Formatiert als `H:MM:SS.mmm`, fuer Oberflaeche und Protokoll.
    #[must_use]
    pub fn format_hms(self) -> String {
        let ms = self.rescale(Rational::MILLIS).map_or(0, Timestamp::ticks);
        let neg = ms < 0;
        let ms = ms.unsigned_abs();
        let (h, rem) = (ms / 3_600_000, ms % 3_600_000);
        let (m, rem) = (rem / 60_000, rem % 60_000);
        let (s, milli) = (rem / 1_000, rem % 1_000);
        let sign = if neg { "-" } else { "" };
        format!("{sign}{h}:{m:02}:{s:02}.{milli:03}")
    }
}

/// Ganzzahlige Division mit Rundung zur naechsten Zahl, halbe weg von Null.
fn div_round_nearest(numer: i128, denom: i128) -> Option<i128> {
    if denom == 0 {
        return None;
    }
    let (numer, denom) = if denom < 0 {
        (numer.checked_neg()?, denom.checked_neg()?)
    } else {
        (numer, denom)
    };
    let half = denom / 2;
    if numer >= 0 {
        numer.checked_add(half)?.checked_div(denom)
    } else {
        numer.checked_sub(half)?.checked_div(denom)
    }
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        // Cross-multiply instead of converting: exact, no rounding.
        // self.ticks * self.base  <=>  other.ticks * other.base
        // Denominators are positive, so only ticks and numerators carry sign.
        let sl = self.ticks.signum() * self.base.num().signum();
        let sr = other.ticks.signum() * other.base.num().signum();
        if sl != sr {
            return sl.cmp(&sr);
        }
        if sl == 0 {
            return Ordering::Equal;
        }
        let lhs = mul_wide(
            u128::from(self.ticks.unsigned_abs()) * u128::from(self.base.num().unsigned_abs()),
            other.base.den().unsigned_abs(),
        );
        let rhs = mul_wide(
            u128::from(other.ticks.unsigned_abs()) * u128::from(other.base.num().unsigned_abs()),
            self.base.den().unsigned_abs(),
        );
        if sl > 0 { lhs.cmp(&rhs) } else { rhs.cmp(&lhs) }
    }
}

#[allow(clippy::cast_possible_truncation)] // keeping the low 64 bits is the point
/// `a * b` as a 192-bit value `(high, low)`; tuple order is numeric order.
fn mul_wide(a: u128, b: u64) -> (u128, u64) {
    let b = u128::from(b);
    let p0 = (a & u128::from(u64::MAX)) * b;
    let p1 = (a >> 64) * b;
    ((p0 >> 64) + p1, p0 as u64)
}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{:?}", self.ticks, self.base)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_hms())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_kuerzt_und_normalisiert_vorzeichen() {
        let r = Rational::new(1000, 24_000).unwrap();
        assert_eq!((r.num(), r.den()), (1, 24));

        // Negativer Nenner wandert ins Vorzeichen des Zaehlers.
        let r = Rational::new(3, -9).unwrap();
        assert_eq!((r.num(), r.den()), (-1, 3));
    }

    #[test]
    fn rational_lehnt_nulldivision_ab() {
        assert!(Rational::new(1, 0).is_none());
    }

    #[test]
    fn ntsc_zeitbasis_ist_exakt() {
        // 24000/1001 fps ist die klassische Falle: als f64 nie exakt.
        let fps = Rational::new(24_000, 1_001).unwrap();
        let base = fps.as_time_base().unwrap();
        assert_eq!((base.num(), base.den()), (1_001, 24_000));

        // Frame 24000 liegt bei exakt 1001 Sekunden.
        let t = Timestamp::new(24_000, base);
        assert_eq!(t.rescale(Rational::MILLIS).unwrap().ticks(), 1_001_000);
    }

    #[test]
    fn samples_rechnen_verlustfrei() {
        // 480 Samples bei 48 kHz sind exakt 10 ms.
        let t = Timestamp::from_samples(480, 48_000).unwrap();
        assert_eq!(t.rescale(Rational::MILLIS).unwrap().ticks(), 10);

        // 44,1 kHz: eine Stunde Audio, hin und zurueck ohne Drift.
        let samples = 44_100 * 3_600;
        let t = Timestamp::from_samples(samples, 44_100).unwrap();
        let via_micros = t.rescale(Rational::MICROS).unwrap();
        let back = via_micros
            .rescale(Rational::new(1, 44_100).unwrap())
            .unwrap();
        assert_eq!(back.ticks(), samples);
    }

    #[test]
    fn vergleich_ueber_verschiedene_basen() {
        let a = Timestamp::from_millis(1_000);
        let b = Timestamp::from_samples(48_000, 48_000).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);

        let c = Timestamp::from_samples(48_001, 48_000).unwrap();
        assert!(c > a);
    }

    #[test]
    fn diff_in_eigener_basis() {
        let a = Timestamp::from_millis(1_500);
        let b = Timestamp::from_millis(500);
        assert_eq!(a.diff(b), Some(1_000));
        assert_eq!(b.diff(a), Some(-1_000));
    }

    #[test]
    fn rundung_geht_von_null_weg() {
        // 1 Tick bei 1/3 s in Millisekunden: 333,33... -> 333
        let t = Timestamp::new(1, Rational::new(1, 3).unwrap());
        assert_eq!(t.rescale(Rational::MILLIS).unwrap().ticks(), 333);
        // 2/3 s -> 666,66... -> 667
        let t = Timestamp::new(2, Rational::new(1, 3).unwrap());
        assert_eq!(t.rescale(Rational::MILLIS).unwrap().ticks(), 667);
        // Negativ symmetrisch.
        let t = Timestamp::new(-2, Rational::new(1, 3).unwrap());
        assert_eq!(t.rescale(Rational::MILLIS).unwrap().ticks(), -667);
    }

    #[test]
    fn kein_ueberlauf_bei_langen_laufzeiten() {
        // 24 Stunden in Nanosekunden, umgerechnet nach 90 kHz.
        let ns = 24 * 3_600 * 1_000_000_000_i64;
        let t = Timestamp::from_nanos(ns);
        let mpeg = t.rescale(Rational::MPEG).unwrap();
        assert_eq!(mpeg.ticks(), 24 * 3_600 * 90_000);
    }

    #[test]
    fn hms_formatierung() {
        assert_eq!(Timestamp::from_millis(0).format_hms(), "0:00:00.000");
        assert_eq!(
            Timestamp::from_millis(3_723_456).format_hms(),
            "1:02:03.456"
        );
        assert_eq!(Timestamp::from_millis(-1_500).format_hms(), "-0:00:01.500");
    }

    fn big(ticks: i64, num: i64, den: i64) -> Timestamp {
        Timestamp::new(ticks, Rational::new(num, den).unwrap())
    }

    #[allow(clippy::cast_possible_truncation)] // splitting into 32-bit limbs
    /// Reference: schoolbook product on 32-bit limbs, little endian.
    fn limbs_mul(factors: &[u64]) -> Vec<u32> {
        let mut acc: Vec<u32> = vec![1];
        for &f in factors {
            let f = [f as u32, (f >> 32) as u32];
            let mut out = vec![0u32; acc.len() + 2];
            for (i, &a) in acc.iter().enumerate() {
                let mut carry = 0u64;
                for (j, &b) in f.iter().enumerate() {
                    let t = u64::from(out[i + j]) + u64::from(a) * u64::from(b) + carry;
                    out[i + j] = t as u32;
                    carry = t >> 32;
                }
                let mut k = i + 2;
                while carry != 0 {
                    let t = u64::from(out[k]) + carry;
                    out[k] = t as u32;
                    carry = t >> 32;
                    k += 1;
                }
            }
            acc = out;
        }
        while acc.len() > 1 && acc.last() == Some(&0) {
            acc.pop();
        }
        acc
    }

    fn reference_cmp(a: Timestamp, b: Timestamp) -> Ordering {
        let sign = |t: Timestamp| t.ticks().signum() * t.base().num().signum();
        let (sa, sb) = (sign(a), sign(b));
        if sa != sb {
            return sa.cmp(&sb);
        }
        let la = limbs_mul(&[
            a.ticks().unsigned_abs(),
            a.base().num().unsigned_abs(),
            b.base().den().unsigned_abs(),
        ]);
        let lb = limbs_mul(&[
            b.ticks().unsigned_abs(),
            b.base().num().unsigned_abs(),
            a.base().den().unsigned_abs(),
        ]);
        let mag = la
            .len()
            .cmp(&lb.len())
            .then_with(|| la.iter().rev().cmp(lb.iter().rev()));
        if sa < 0 { mag.reverse() } else { mag }
    }

    #[test]
    fn cmp_does_not_overflow_at_i64_limits() {
        // 2^63 * 2^63 * 2^63 does not fit i128; the old code panicked in debug.
        let max = i64::MAX;
        let a = big(max, max, max - 1);
        let b = big(max, max - 1, max);
        assert_eq!(a.cmp(&b), Ordering::Greater);
        assert_eq!(b.cmp(&a), Ordering::Less);
        assert_eq!(a.cmp(&a), Ordering::Equal);
        let n = big(-max, max, max - 1);
        assert_eq!(n.cmp(&a), Ordering::Less);
        assert_eq!(big(i64::MIN, 1, 1).cmp(&n), Ordering::Greater);
    }

    #[test]
    fn cmp_is_exact_where_i128_would_wrap() {
        let max = i64::MAX;
        let a = big(max, max, max - 1);
        let b = big(max - 1, max, max - 1);
        assert_eq!(a.cmp(&b), Ordering::Greater);
    }

    #[test]
    fn rescale_and_diff_return_none_instead_of_overflowing() {
        let max = i64::MAX;
        let t = big(max, max, max - 1);
        let to = Rational::new(1, max).unwrap();
        assert_eq!(t.rescale(to), None);
        let u = big(-max, max, max - 1);
        assert_eq!(t.diff(u), None);
        // Intermediate 2^189 without reduction, representable result with it.
        let k = 1_i64 << 50;
        let v = big(k, k + 1, k);
        assert_eq!(
            v.rescale(Rational::new(k + 1, k + 3).unwrap())
                .map(Timestamp::ticks),
            Some(k + 3)
        );
    }

    #[test]
    fn cmp_matches_reference_on_random_and_edge_values() {
        let edges = [
            0,
            1,
            -1,
            2,
            3,
            i64::MAX,
            i64::MAX - 1,
            -i64::MAX,
            i64::MIN,
            1 << 62,
            1 << 31,
        ];
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut pick = |allow_zero: bool, allow_min: bool| loop {
            let r = next();
            let v = if r % 3 == 0 {
                edges[(r >> 8) as usize % edges.len()]
            } else {
                (next() as i64) >> (r % 64)
            };
            if (v == 0 && !allow_zero) || (v == i64::MIN && !allow_min) {
                continue;
            }
            return v;
        };
        let mut stamps = Vec::new();
        for _ in 0..400 {
            let (n, d) = (pick(true, false), pick(false, false));
            stamps.push(Timestamp::new(
                pick(true, true),
                Rational::new(n, d).unwrap(),
            ));
        }
        for &a in &stamps {
            for &b in &stamps {
                assert_eq!(a.cmp(&b), reference_cmp(a, b), "{a:?} vs {b:?}");
            }
        }
    }
}
