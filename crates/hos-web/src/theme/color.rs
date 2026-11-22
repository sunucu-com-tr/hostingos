//! Colour maths for the tenant theme: hex parsing, WCAG contrast and the
//! derived brand shades. Everything that puts text on a brand colour is
//! decided here, on the server, so a pale or neon brand never yields an
//! unreadable button: the ink is whichever of black / white has the higher
//! contrast (always ≥ 4.58:1), hover shades move *away* from the ink, and
//! text-on-surface variants are darkened / lightened in OKLab until they
//! reach AA against the lightest / darkest surfaces of every template.

/// An sRGB colour, channels in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    /// Red.
    pub r: f64,
    /// Green.
    pub g: f64,
    /// Blue.
    pub b: f64,
}

/// White.
pub const WHITE: Rgb = Rgb {
    r: 1.0,
    g: 1.0,
    b: 1.0,
};
/// Black.
pub const BLACK: Rgb = Rgb {
    r: 0.0,
    g: 0.0,
    b: 0.0,
};

impl Rgb {
    /// Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` (alpha is ignored).
    pub fn parse(hex: &str) -> Option<Self> {
        let d = hex.trim().strip_prefix('#')?;
        if !d.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let full: String = match d.len() {
            3 | 4 => d.chars().take(3).flat_map(|c| [c, c]).collect(),
            6 | 8 => d[..6].to_owned(),
            _ => return None,
        };
        let ch = |i: usize| {
            u8::from_str_radix(&full[i..i + 2], 16)
                .ok()
                .map(|v| f64::from(v) / 255.0)
        };
        Some(Self {
            r: ch(0)?,
            g: ch(2)?,
            b: ch(4)?,
        })
    }

    /// `#rrggbb`.
    pub fn hex(&self) -> String {
        let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", c(self.r), c(self.g), c(self.b))
    }

    /// WCAG relative luminance.
    pub fn luminance(&self) -> f64 {
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// WCAG contrast ratio with `other`.
    pub fn contrast(&self, other: &Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn to_oklab(self) -> [f64; 3] {
        let (r, g, b) = (lin(self.r), lin(self.g), lin(self.b));
        let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
        let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
        let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
        [
            0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s,
            1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s,
            0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s,
        ]
    }

    fn from_oklab([ll, a, b]: [f64; 3]) -> Self {
        let l = (ll + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
        let m = (ll - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
        let s = (ll - 0.089_484_177_5 * a - 1.291_485_548 * b).powi(3);
        Self {
            r: gamma(4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s),
            g: gamma(-1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s),
            b: gamma(-0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701 * s),
        }
    }

    /// OKLCH `(l, c, h°)`.
    pub fn to_oklch(self) -> (f64, f64, f64) {
        let [l, a, b] = self.to_oklab();
        let h = b.atan2(a).to_degrees().rem_euclid(360.0);
        (l, (a * a + b * b).sqrt(), h)
    }

    /// From OKLCH; the chroma is reduced until the colour fits sRGB.
    pub fn from_oklch(l: f64, c: f64, h: f64) -> Self {
        let l = l.clamp(0.0, 1.0);
        let mut c = c.max(0.0);
        loop {
            let (a, b) = (c * h.to_radians().cos(), c * h.to_radians().sin());
            let rgb = Self::from_oklab([l, a, b]);
            let ok = [rgb.r, rgb.g, rgb.b]
                .iter()
                .all(|v| (-0.0005..=1.0005).contains(v));
            if ok || c < 0.001 {
                return Self {
                    r: rgb.r.clamp(0.0, 1.0),
                    g: rgb.g.clamp(0.0, 1.0),
                    b: rgb.b.clamp(0.0, 1.0),
                };
            }
            c *= 0.94;
        }
    }

    /// Same hue / chroma with OKLCH lightness `l`.
    pub fn with_lightness(self, l: f64) -> Self {
        let (_, c, h) = self.to_oklch();
        Self::from_oklch(l, c, h)
    }
}

fn lin(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn gamma(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Text colour on a `fill`: white or black, whichever contrasts more.
pub fn ink_on(fill: Rgb) -> Rgb {
    if fill.contrast(&WHITE) >= fill.contrast(&BLACK) {
        WHITE
    } else {
        BLACK
    }
}

/// Hover / pressed shade of a fill: moves away from its ink (darker under
/// white text, lighter under black), so the contrast only grows.
pub fn hover(fill: Rgb) -> Rgb {
    let (l, c, h) = fill.to_oklch();
    if ink_on(fill) == WHITE {
        Rgb::from_oklch(l - 0.06, c, h)
    } else {
        Rgb::from_oklch(l + 0.06, c, h)
    }
}

/// `color` adjusted (lightness only) until it reaches `min` contrast with
/// `bg`: darker on light backgrounds, lighter on dark ones.
pub fn readable_on(color: Rgb, bg: Rgb, min: f64) -> Rgb {
    let (mut l, c, h) = color.to_oklch();
    let darker = bg.luminance() > 0.18;
    let mut out = color;
    for _ in 0..100 {
        if out.contrast(&bg) >= min {
            return out;
        }
        l += if darker { -0.01 } else { 0.01 };
        out = Rgb::from_oklch(l, c, h);
    }
    out
}

/// Second gradient stop for brand fills: the hue turned by 35°, with the
/// lightness nudged so the brand ink keeps ≥ 4.5:1 on it too.
pub fn gradient_partner(fill: Rgb) -> Rgb {
    let (l, c, h) = fill.to_oklch();
    let ink = ink_on(fill);
    let mut l2 = if ink == WHITE { l - 0.04 } else { l + 0.03 };
    let mut out = Rgb::from_oklch(l2, c * 1.05, (h + 35.0).rem_euclid(360.0));
    for _ in 0..60 {
        if out.contrast(&ink) >= 4.5 {
            break;
        }
        l2 += if ink == WHITE { -0.01 } else { 0.01 };
        out = Rgb::from_oklch(l2, c * 1.05, (h + 35.0).rem_euclid(360.0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_hex() {
        assert_eq!(Rgb::parse("#ABC").unwrap().hex(), "#aabbcc");
        assert_eq!(Rgb::parse("#11223344").unwrap().hex(), "#112233");
        assert!(Rgb::parse("112233").is_none());
        assert!(Rgb::parse("#12").is_none());
        assert!(Rgb::parse("#zzz").is_none());
    }

    #[test]
    fn oklch_round_trip() {
        for hex in [
            "#2563eb", "#f59e0b", "#0f5d75", "#ffffff", "#000000", "#22c55e",
        ] {
            let c = Rgb::parse(hex).unwrap();
            let (l, ch, h) = c.to_oklch();
            assert_eq!(Rgb::from_oklch(l, ch, h).hex(), hex);
        }
    }

    #[test]
    fn ink_always_reaches_aa() {
        // Sweep the RGB cube: whatever the brand, its ink is readable.
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(17) {
                for b in (0..=255).step_by(17) {
                    let fill = Rgb {
                        r: f64::from(r) / 255.0,
                        g: f64::from(g) / 255.0,
                        b: f64::from(b) / 255.0,
                    };
                    let ink = ink_on(fill);
                    assert!(fill.contrast(&ink) >= 4.5, "{}", fill.hex());
                    assert!(hover(fill).contrast(&ink) >= 4.5, "hover {}", fill.hex());
                    assert!(
                        gradient_partner(fill).contrast(&ink) >= 4.5,
                        "grad {}",
                        fill.hex()
                    );
                }
            }
        }
    }

    #[test]
    fn pale_brand_gets_dark_ink_and_readable_text() {
        let yellow = Rgb::parse("#fde047").unwrap();
        assert_eq!(ink_on(yellow), BLACK);
        let paper = Rgb::parse("#f4f4f4").unwrap();
        let text = readable_on(yellow, paper, 4.6);
        assert!(text.contrast(&paper) >= 4.6, "{}", text.hex());
        let night = Rgb::parse("#222222").unwrap();
        let navy = Rgb::parse("#1e3a8a").unwrap();
        assert!(readable_on(navy, night, 4.6).contrast(&night) >= 4.6);
    }
}
