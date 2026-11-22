//! Bundled fonts: Geist (primary) with Inter's `latin-ext` subset as a
//! fallback for the few characters Geist lacks (notably `₺`, U+20BA).
//! WOFF2 → TrueType once per process, two weights each, and text measurement
//! with the same shaper krilla uses. krilla subsets every font it embeds.

use std::sync::OnceLock;

use krilla::text::{Font, Tag};

use crate::PdfError;

/// `static/fonts/Geist-Variable.woff2` (SIL OFL 1.1, `static/fonts/OFL.txt`).
const GEIST_WOFF2: &[u8] = include_bytes!("../../../static/fonts/Geist-Variable.woff2");
/// `static/fonts/inter/inter-latin-ext-wght.woff2` (SIL OFL 1.1,
/// `static/fonts/inter/OFL.txt`).
const INTER_EXT_WOFF2: &[u8] =
    include_bytes!("../../../static/fonts/inter/inter-latin-ext-wght.woff2");

const REGULAR: f32 = 400.0;
const BOLD: f32 = 600.0;

static GEIST: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();
static INTER_EXT: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();

fn ttf(
    cell: &'static OnceLock<Result<Vec<u8>, String>>,
    woff2: &'static [u8],
    name: &str,
) -> Result<&'static [u8], PdfError> {
    cell.get_or_init(|| {
        wuff::decompress_woff2(woff2).map_err(|e| format!("cannot decode {name} WOFF2: {e:?}"))
    })
    .as_ref()
    .map(Vec::as_slice)
    .map_err(|e| PdfError::Font(e.clone()))
}

fn face(data: &'static [u8], weight: f32) -> Result<rustybuzz::Face<'static>, PdfError> {
    let mut face = rustybuzz::Face::from_slice(data, 0)
        .ok_or_else(|| PdfError::Font("font has no face 0".into()))?;
    face.set_variation(rustybuzz::ttf_parser::Tag::from_bytes(b"wght"), weight);
    Ok(face)
}

fn font(data: &'static [u8], weight: f32) -> Result<Font, PdfError> {
    Font::new_variable(data.into(), 0, &[(Tag::new(b"wght"), weight)])
        .ok_or_else(|| PdfError::Font("krilla cannot read the font".into()))
}

/// One family at one weight: the krilla font (drawing) and the shaping face
/// (measuring).
pub(crate) struct Variant {
    pub font: Font,
    face: rustybuzz::Face<'static>,
}

impl Variant {
    fn load(data: &'static [u8], weight: f32) -> Result<Self, PdfError> {
        Ok(Self {
            font: font(data, weight)?,
            face: face(data, weight)?,
        })
    }

    fn covers(&self, c: char) -> bool {
        self.face.glyph_index(c).is_some()
    }

    fn width(&self, text: &str, size: f32) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        let output = rustybuzz::shape(&self.face, &[], buffer);
        let units: i64 = output
            .glyph_positions()
            .iter()
            .map(|p| i64::from(p.x_advance))
            .sum();
        let upem = self.face.units_per_em().max(1) as f32;
        units as f32 / upem * size
    }
}

/// Primary + fallback, regular + bold.
pub(crate) struct Fonts {
    /// `[regular, bold]` of the primary family.
    primary: [Variant; 2],
    /// `[regular, bold]` of the fallback family.
    fallback: [Variant; 2],
}

/// A piece of text drawn with one font.
pub(crate) struct Run<'a> {
    pub text: &'a str,
    pub fallback: bool,
}

impl Fonts {
    pub fn load() -> Result<Self, PdfError> {
        let geist = ttf(&GEIST, GEIST_WOFF2, "Geist")?;
        let inter = ttf(&INTER_EXT, INTER_EXT_WOFF2, "Inter")?;
        Ok(Self {
            primary: [Variant::load(geist, REGULAR)?, Variant::load(geist, BOLD)?],
            fallback: [Variant::load(inter, REGULAR)?, Variant::load(inter, BOLD)?],
        })
    }

    fn pair(&self, bold: bool) -> (&Variant, &Variant) {
        let i = usize::from(bold);
        (&self.primary[i], &self.fallback[i])
    }

    /// The font to draw a run with.
    pub fn font(&self, bold: bool, fallback: bool) -> &Font {
        let (p, f) = self.pair(bold);
        if fallback { &f.font } else { &p.font }
    }

    /// Splits `text` into runs: characters the primary font lacks but the
    /// fallback has go to the fallback; everything else stays primary.
    pub fn runs<'a>(&self, text: &'a str, bold: bool) -> Vec<Run<'a>> {
        let (primary, fallback) = self.pair(bold);
        let mut runs: Vec<Run<'a>> = Vec::new();
        let mut start = 0;
        let mut current: Option<bool> = None;
        for (i, c) in text.char_indices() {
            // Whitespace never switches fonts: it stays with the run it
            // follows, so a run boundary is never "space then glyph".
            if c.is_whitespace() && current.is_some() {
                continue;
            }
            let use_fallback = !primary.covers(c) && fallback.covers(c);
            match current {
                Some(f) if f == use_fallback => {}
                Some(f) => {
                    runs.push(Run {
                        text: &text[start..i],
                        fallback: f,
                    });
                    start = i;
                    current = Some(use_fallback);
                }
                None => current = Some(use_fallback),
            }
        }
        if let Some(f) = current {
            runs.push(Run {
                text: &text[start..],
                fallback: f,
            });
        }
        runs
    }

    /// Advance width of a run.
    pub fn run_width(&self, run: &Run<'_>, size: f32, bold: bool) -> f32 {
        let (p, f) = self.pair(bold);
        if run.fallback {
            f.width(run.text, size)
        } else {
            p.width(run.text, size)
        }
    }

    /// Advance width of `text` at `size` points.
    pub fn width(&self, text: &str, size: f32, bold: bool) -> f32 {
        self.runs(text, bold)
            .iter()
            .map(|r| self.run_width(r, size, bold))
            .sum()
    }

    /// Splits `text` into lines no wider than `max` (word wrap; overlong
    /// words are broken between characters).
    pub fn wrap(&self, text: &str, size: f32, bold: bool, max: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_owned()
            } else {
                format!("{current} {word}")
            };
            if self.width(&candidate, size, bold) <= max {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if self.width(word, size, bold) <= max {
                current = word.to_owned();
                continue;
            }
            // Break an overlong word.
            let mut piece = String::new();
            for c in word.chars() {
                piece.push(c);
                if self.width(&piece, size, bold) > max && piece.chars().count() > 1 {
                    piece.pop();
                    lines.push(std::mem::take(&mut piece));
                    piece.push(c);
                }
            }
            current = piece;
        }
        if !current.is_empty() {
            lines.push(current);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_turkish_and_currency_signs() {
        let fonts = Fonts::load().unwrap();
        for bold in [false, true] {
            let (p, f) = fonts.pair(bold);
            for c in "çğıİöşüÇĞÖŞÜâîû₺€£$–—…%".chars() {
                assert!(p.covers(c) || f.covers(c), "no glyph for {c:?}");
            }
        }
        let runs = fonts.runs("120,00 ₺", false);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "120,00 ");
        assert!(runs[1].fallback);
        assert!(fonts.width("₺", 10.0, false) > 1.0);
    }
}
