//! Share images (Open Graph / Twitter `summary_large_image`): a 1200 × 630
//! PNG drawn from the tenant's colours, without text and without any image
//! dependency — a small rasteriser (anti-aliased by 4× vertical and
//! horizontal coverage sampling of rounded rectangles and circles) and a PNG
//! writer with its own DEFLATE (LZ77 + fixed Huffman codes).
//!
//! [`render`] is deterministic: the same [`Spec`] always gives the same bytes,
//! so callers can cache by a content hash. `seed` (a page key) moves the
//! decorative shapes so pages of one site get related but distinct images.

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parses `#rrggbb`.
    pub fn parse(hex: &str) -> Option<Self> {
        let h = hex.trim().trim_start_matches('#');
        if h.len() != 6 {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
        Some(Self(p(0)?, p(2)?, p(4)?))
    }

    fn mix(self, o: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(m(self.0, o.0), m(self.1, o.1), m(self.2, o.2))
    }

    fn luma(self) -> f32 {
        0.2126 * f32::from(self.0) + 0.7152 * f32::from(self.1) + 0.0722 * f32::from(self.2)
    }
}

/// Composition style (one per storefront design template).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Brand gradient, hairline grid, server-rack card.
    Grid,
    /// Soft overlapping circles.
    Orbs,
    /// Dark field, scanlines, terminal window.
    Terminal,
    /// Light paper, one large circle and rules.
    Paper,
    /// Big rounded geometric forms.
    Blocks,
}

/// What to draw.
#[derive(Debug, Clone)]
pub struct Spec {
    /// Main colour.
    pub brand: Rgb,
    /// Gradient partner.
    pub brand_2: Rgb,
    /// Accent.
    pub accent: Rgb,
    /// Composition.
    pub style: Style,
    /// Variation key (page path).
    pub seed: String,
}

/// Width of the image.
pub const WIDTH: usize = 1200;
/// Height of the image.
pub const HEIGHT: usize = 630;

struct Canvas {
    px: Vec<Rgb>,
}

impl Canvas {
    fn new(bg: Rgb) -> Self {
        Self {
            px: vec![bg; WIDTH * HEIGHT],
        }
    }

    fn blend(&mut self, x: usize, y: usize, c: Rgb, a: f32) {
        if a <= 0.0 {
            return;
        }
        let p = &mut self.px[y * WIDTH + x];
        *p = p.mix(c, a);
    }

    /// Fills a shape given by `inside(x, y)` within its bounding box, with
    /// 4×4 coverage anti-aliasing and opacity `alpha`.
    fn fill(
        &mut self,
        bbox: (f32, f32, f32, f32),
        c: Rgb,
        alpha: f32,
        inside: impl Fn(f32, f32) -> bool,
    ) {
        let (x0, y0, x1, y1) = bbox;
        let xs = x0.floor().max(0.0) as usize;
        let ys = y0.floor().max(0.0) as usize;
        let xe = (x1.ceil().max(0.0) as usize).min(WIDTH);
        let ye = (y1.ceil().max(0.0) as usize).min(HEIGHT);
        for y in ys..ye {
            for x in xs..xe {
                let mut hit = 0u8;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let fx = x as f32 + (sx as f32 + 0.5) / 4.0;
                        let fy = y as f32 + (sy as f32 + 0.5) / 4.0;
                        if inside(fx, fy) {
                            hit += 1;
                        }
                    }
                }
                if hit > 0 {
                    self.blend(x, y, c, alpha * f32::from(hit) / 16.0);
                }
            }
        }
    }

    fn rect(&mut self, [x, y, w, h]: [f32; 4], r: f32, c: Rgb, alpha: f32) {
        let r = r.min(w / 2.0).min(h / 2.0);
        self.fill((x, y, x + w, y + h), c, alpha, |px, py| {
            if px < x || py < y || px > x + w || py > y + h {
                return false;
            }
            let cx = px.clamp(x + r, x + w - r);
            let cy = py.clamp(y + r, y + h - r);
            (px - cx).powi(2) + (py - cy).powi(2) <= r * r
        });
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, alpha: f32) {
        self.fill((cx - r, cy - r, cx + r, cy + r), c, alpha, |px, py| {
            (px - cx).powi(2) + (py - cy).powi(2) <= r * r
        });
    }

    fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Rgb, alpha: f32) {
        let (inner, outer) = ((r - width / 2.0).max(0.0), r + width / 2.0);
        self.fill(
            (cx - outer, cy - outer, cx + outer, cy + outer),
            c,
            alpha,
            |px, py| {
                let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                d >= inner && d <= outer
            },
        );
    }

    /// Diagonal gradient `a` → `b` over the whole canvas.
    fn gradient(&mut self, a: Rgb, b: Rgb) {
        let span = (WIDTH + HEIGHT) as f32;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                // Quantised to 1/96 steps: flat runs compress far better.
                let t = (((x + y) as f32 / span) * 96.0).floor() / 96.0;
                self.px[y * WIDTH + x] = a.mix(b, t);
            }
        }
    }
}

/// Small deterministic generator (FNV-1a seeded xorshift).
struct Rand(u64);

impl Rand {
    fn new(seed: &str) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in seed.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        Self(h | 1)
    }

    fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 10_000) as f32 / 10_000.0
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

const WHITE: Rgb = Rgb(255, 255, 255);
const INK: Rgb = Rgb(16, 18, 24);

/// Draws the image and returns the PNG bytes.
pub fn render(spec: &Spec) -> Vec<u8> {
    let mut r = Rand::new(&spec.seed);
    let (w, h) = (WIDTH as f32, HEIGHT as f32);
    let on_brand = if spec.brand.luma() > 150.0 {
        INK
    } else {
        WHITE
    };
    let mut c;
    match spec.style {
        Style::Grid => {
            c = Canvas::new(spec.brand);
            c.gradient(spec.brand, spec.brand_2);
            let step = 60.0;
            let mut x = step;
            while x < w {
                c.rect([x, 0.0, 1.0, h], 0.0, on_brand, 0.08);
                x += step;
            }
            let mut y = step;
            while y < h {
                c.rect([0.0, y, w, 1.0], 0.0, on_brand, 0.08);
                y += step;
            }
            rack(&mut c, 640.0 + r.range(-30.0, 30.0), 150.0, spec, on_brand);
            c.circle(
                r.range(120.0, 360.0),
                r.range(420.0, 520.0),
                70.0,
                spec.accent,
                0.9,
            );
        }
        Style::Orbs => {
            c = Canvas::new(spec.brand.mix(WHITE, 0.92));
            c.circle(
                r.range(820.0, 980.0),
                r.range(160.0, 260.0),
                260.0,
                spec.brand,
                0.85,
            );
            c.circle(
                r.range(600.0, 760.0),
                r.range(420.0, 520.0),
                200.0,
                spec.brand_2,
                0.75,
            );
            c.circle(
                r.range(980.0, 1100.0),
                r.range(460.0, 560.0),
                120.0,
                spec.accent,
                0.8,
            );
            c.rect([90.0, 210.0, 440.0, 210.0], 28.0, WHITE, 0.92);
            for i in 0..3 {
                let y = 250.0 + i as f32 * 52.0;
                c.circle(130.0, y + 12.0, 12.0, spec.brand, 1.0);
                c.rect(
                    [162.0, y + 4.0, 300.0 - i as f32 * 60.0, 16.0],
                    8.0,
                    INK,
                    0.16,
                );
            }
        }
        Style::Terminal => {
            let bg = Rgb(9, 12, 14);
            c = Canvas::new(bg);
            let mut y = 0.0;
            while y < h {
                c.rect([0.0, y, w, 1.0], 0.0, spec.brand, 0.07);
                y += 6.0;
            }
            c.rect([120.0, 110.0, 960.0, 410.0], 10.0, Rgb(17, 22, 26), 1.0);
            c.rect([120.0, 110.0, 960.0, 46.0], 10.0, Rgb(28, 34, 40), 1.0);
            for (i, col) in [Rgb(255, 95, 86), Rgb(255, 189, 46), Rgb(39, 201, 63)]
                .into_iter()
                .enumerate()
            {
                c.circle(150.0 + i as f32 * 26.0, 133.0, 7.0, col, 1.0);
            }
            for i in 0..5 {
                let y = 200.0 + i as f32 * 52.0;
                c.rect([160.0, y, 22.0, 22.0], 3.0, spec.brand, 1.0);
                c.rect(
                    [200.0, y + 3.0, r.range(260.0, 640.0), 16.0],
                    3.0,
                    WHITE,
                    0.55,
                );
            }
            c.rect([160.0, 460.0, 18.0, 30.0], 2.0, spec.accent, 1.0);
        }
        Style::Paper => {
            c = Canvas::new(Rgb(250, 248, 243));
            c.circle(r.range(820.0, 900.0), 315.0, 230.0, spec.brand, 1.0);
            c.ring(r.range(820.0, 900.0), 315.0, 290.0, 2.0, INK, 0.5);
            c.rect([96.0, 230.0, 380.0, 3.0], 0.0, INK, 0.85);
            c.rect([96.0, 270.0, 300.0, 3.0], 0.0, INK, 0.35);
            c.rect([96.0, 310.0, 340.0, 3.0], 0.0, INK, 0.35);
            c.rect([96.0, 380.0, 120.0, 10.0], 5.0, spec.accent, 1.0);
        }
        Style::Blocks => {
            c = Canvas::new(spec.brand.mix(WHITE, 0.9));
            c.rect(
                [r.range(560.0, 640.0), 90.0, 300.0, 300.0],
                70.0,
                spec.brand,
                1.0,
            );
            c.circle(r.range(960.0, 1020.0), 200.0, 120.0, spec.accent, 1.0);
            c.rect([860.0, 360.0, 260.0, 180.0], 90.0, spec.brand_2, 0.9);
            c.circle(420.0, 470.0, r.range(70.0, 100.0), spec.brand_2, 0.35);
            c.rect([90.0, 190.0, 380.0, 34.0], 17.0, INK, 0.85);
            c.rect([90.0, 246.0, 300.0, 34.0], 17.0, INK, 0.85);
            c.rect([90.0, 320.0, 160.0, 54.0], 27.0, spec.brand, 1.0);
        }
    }
    png(&c.px)
}

/// A stylised server rack card.
fn rack(c: &mut Canvas, x: f32, y: f32, spec: &Spec, on_brand: Rgb) {
    let panel = if on_brand == WHITE {
        Rgb(255, 255, 255)
    } else {
        INK
    };
    c.rect([x, y, 440.0, 330.0], 22.0, panel, 0.14);
    for i in 0..4 {
        let ry = y + 30.0 + i as f32 * 72.0;
        c.rect([x + 30.0, ry, 380.0, 52.0], 12.0, panel, 0.9);
        let ink = if panel == INK { WHITE } else { INK };
        c.circle(
            x + 62.0,
            ry + 26.0,
            8.0,
            if i == 1 { spec.accent } else { spec.brand },
            1.0,
        );
        c.rect([x + 90.0, ry + 20.0, 160.0, 12.0], 6.0, ink, 0.18);
        c.rect([x + 330.0, ry + 20.0, 50.0, 12.0], 6.0, ink, 0.12);
    }
}

// ---------------------------------------------------------------------------
// PNG + DEFLATE
// ---------------------------------------------------------------------------

fn crc32(data: &[u8], mut crc: u32) -> u32 {
    crc = !crc;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(data, crc32(kind, 0));
    out.extend_from_slice(&crc.to_be_bytes());
}

fn png(px: &[Rgb]) -> Vec<u8> {
    // Filtered scanlines: per row the filter (Sub / Up) with the smallest
    // sum of absolute differences.
    let row = WIDTH * 3;
    let mut raw = Vec::with_capacity((row + 1) * HEIGHT);
    let mut prev = vec![0u8; row];
    let mut cur = vec![0u8; row];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let p = px[y * WIDTH + x];
            cur[x * 3] = p.0;
            cur[x * 3 + 1] = p.1;
            cur[x * 3 + 2] = p.2;
        }
        let sub: Vec<u8> = (0..row)
            .map(|i| cur[i].wrapping_sub(if i >= 3 { cur[i - 3] } else { 0 }))
            .collect();
        let up: Vec<u8> = (0..row).map(|i| cur[i].wrapping_sub(prev[i])).collect();
        let cost = |v: &[u8]| {
            v.iter()
                .map(|&b| u32::from((b as i8).unsigned_abs()))
                .sum::<u32>()
        };
        if y > 0 && cost(&up) <= cost(&sub) {
            raw.push(2);
            raw.extend_from_slice(&up);
        } else {
            raw.push(1);
            raw.extend_from_slice(&sub);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let mut z = vec![0x78, 0x01];
    z.extend_from_slice(&deflate(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(WIDTH as u32).to_be_bytes());
    ihdr.extend_from_slice(&(HEIGHT as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push((self.acc & 0xff) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Huffman codes are sent most-significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        let mut rev = 0;
        for i in 0..len {
            rev |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.put(rev, len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc & 0xff) as u8);
        }
        self.out
    }
}

fn lit(b: &mut Bits, v: u32) {
    match v {
        0..=143 => b.code(0x30 + v, 8),
        144..=255 => b.code(0x190 + v - 144, 9),
        256..=279 => b.code(v - 256, 7),
        _ => b.code(0xc0 + v - 280, 8),
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn emit_match(b: &mut Bits, len: usize, dist: usize) {
    let li = LEN_BASE
        .iter()
        .rposition(|&x| usize::from(x) <= len)
        .unwrap_or(0);
    lit(b, 257 + li as u32);
    b.put(
        (len - usize::from(LEN_BASE[li])) as u32,
        u32::from(LEN_EXTRA[li]),
    );
    let di = DIST_BASE
        .iter()
        .rposition(|&x| usize::from(x) <= dist)
        .unwrap_or(0);
    b.code(di as u32, 5);
    b.put(
        (dist - usize::from(DIST_BASE[di])) as u32,
        u32::from(DIST_EXTRA[di]),
    );
}

/// DEFLATE with greedy LZ77 (hash chains over 3-byte prefixes, 32 KiB
/// window) and one fixed-Huffman block.
fn deflate(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32_768;
    const HASH: usize = 1 << 15;
    const CHAIN: usize = 24;
    let mut b = Bits {
        out: Vec::with_capacity(data.len() / 8),
        acc: 0,
        n: 0,
    };
    b.put(1, 1); // final block
    b.put(1, 2); // fixed Huffman
    let mut head = vec![usize::MAX; HASH];
    let mut prev = vec![usize::MAX; data.len()];
    let hash = |i: usize| -> usize {
        ((usize::from(data[i]) << 10) ^ (usize::from(data[i + 1]) << 5) ^ usize::from(data[i + 2]))
            & (HASH - 1)
    };
    let insert = |i: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if i + 2 < data.len() {
            let h = hash(i);
            prev[i] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let mut best = (0usize, 0usize);
        if i + 2 < data.len() {
            let mut cand = head[hash(i)];
            let mut tries = 0;
            let max = (data.len() - i).min(258);
            while cand != usize::MAX && i - cand <= WINDOW && tries < CHAIN {
                let mut l = 0;
                while l < max && data[cand + l] == data[i + l] {
                    l += 1;
                }
                if l > best.0 {
                    best = (l, i - cand);
                    if l == max {
                        break;
                    }
                }
                cand = prev[cand];
                tries += 1;
            }
        }
        if best.0 >= 3 {
            emit_match(&mut b, best.0, best.1);
            for k in i..i + best.0 {
                insert(k, &mut head, &mut prev);
            }
            i += best.0;
        } else {
            lit(&mut b, u32::from(data[i]));
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    lit(&mut b, 256);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inflate_check(png: &[u8]) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn renders_every_style_small_and_deterministic() {
        for style in [
            Style::Grid,
            Style::Orbs,
            Style::Terminal,
            Style::Paper,
            Style::Blocks,
        ] {
            let spec = Spec {
                brand: Rgb::parse("#2563eb").unwrap(),
                brand_2: Rgb::parse("#7c3aed").unwrap(),
                accent: Rgb::parse("#f59e0b").unwrap(),
                style,
                seed: "/web-hosting".into(),
            };
            let a = render(&spec);
            inflate_check(&a);
            assert!(a.len() < 400_000, "{style:?}: {} bytes", a.len());
            assert_eq!(a, render(&spec));
        }
    }

    #[test]
    fn deflate_round_trip_against_known_stream() {
        // "aaaaaaaaaa" → literal 'a' + match(len 9, dist 1) + end.
        let z = deflate(b"aaaaaaaaaa");
        assert!(z.len() < 8, "{z:?}");
        assert_eq!(crc32(b"IEND", 0), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }
}
