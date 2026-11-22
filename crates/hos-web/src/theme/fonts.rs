//! The curated, self-hosted font library (`static/fonts`).
//!
//! Every family is OFL-1.1, variable on the `wght` axis and split into the
//! Google Fonts `latin` and `latin-ext` subsets (`unicode-range`), so a page
//! downloads `latin-ext` only when it shows ğ ş İ ₺ … (Turkish pages do).
//! Geist / Geist Mono are declared in `tokens.css` (the design-system
//! default); the others get their `@font-face` rules from the tenant theme
//! stylesheet, one set per *selected* family only.
//!
//! The `… Fallback` faces are metric-matched local fonts (Arial, or Georgia
//! for the serifs) so the swap from fallback to web font does not reflow
//! text. The numbers were measured in Chrome (canvas `measureText`, 400px,
//! the same TR + EN running text the Geist fallback was tuned with):
//! `size400` / `size600` = advance width of the family ÷ the local fallback
//! at weight 400 / 600, `ascent` / `descent` = font bounding box per em.
//! Adding a family: copy `<id>-latin-wght.woff2` + `<id>-latin-ext-wght.woff2`
//! (+ `OFL.txt`, `SOURCE.txt`) into `static/fonts/<id>/`, measure it with the
//! same method and add a [`Font`] entry below.

/// Generic family of a face (picks the fallback stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Sans-serif (fallback: Arial).
    Sans,
    /// Serif (fallback: Georgia).
    Serif,
}

/// Measured metrics of a family (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Width ratio to the local fallback at weight 400.
    pub size400: f64,
    /// Width ratio to the local fallback at weight 600.
    pub size600: f64,
    /// Ascent per em.
    pub ascent: f64,
    /// Descent per em.
    pub descent: f64,
}

/// One selectable family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Font {
    /// Stable key stored in `appearance.font_body` / `appearance.font_heading`.
    pub key: &'static str,
    /// CSS family name.
    pub family: &'static str,
    /// Sans or serif.
    pub category: Category,
    /// `font-weight` range of the variable font.
    pub weights: &'static str,
    /// Files relative to `static/` (`latin`, `latin-ext`); one file for Geist.
    pub files: &'static [&'static str],
    /// Fallback metrics; `None` for Geist (its fallback lives in tokens.css).
    pub metrics: Option<Metrics>,
}

/// `unicode-range` of the `latin` subset files.
pub const LATIN_RANGE: &str = "U+0000-00FF, U+0131, U+0152-0153, U+02BB-02BC, U+02C6, U+02DA, U+02DC, \
U+0304, U+0308, U+0329, U+2000-206F, U+20AC, U+2122, U+2191, U+2193, U+2212, U+2215, U+FEFF, U+FFFD";
/// `unicode-range` of the `latin-ext` subset files (ğ ş İ, ₺ …).
pub const LATIN_EXT_RANGE: &str = "U+0100-02BA, U+02BD-02C5, U+02C7-02CC, U+02CE-02D7, U+02DD-02FF, \
U+0304, U+0308, U+0329, U+1D00-1DBF, U+1E00-1E9F, U+1EF2-1EFF, U+2020, U+20A0-20AB, U+20AD-20C4, \
U+2113, U+2C60-2C7F, U+A720-A7FF";

const fn m(size400: f64, size600: f64, ascent: f64, descent: f64) -> Option<Metrics> {
    Some(Metrics {
        size400,
        size600,
        ascent,
        descent,
    })
}

/// The library, in the order the admin picker lists it.
pub const FONTS: &[Font] = &[
    Font {
        key: "geist",
        family: "Geist",
        category: Category::Sans,
        weights: "100 900",
        files: &["fonts/Geist-Variable.woff2"],
        metrics: None,
    },
    Font {
        key: "inter",
        family: "Inter",
        category: Category::Sans,
        weights: "100 900",
        files: &[
            "fonts/inter/inter-latin-wght.woff2",
            "fonts/inter/inter-latin-ext-wght.woff2",
        ],
        metrics: m(1.0674, 1.0135, 0.97, 0.24),
    },
    Font {
        key: "ibm-plex-sans",
        family: "IBM Plex Sans",
        category: Category::Sans,
        weights: "100 700",
        files: &[
            "fonts/ibm-plex-sans/ibm-plex-sans-latin-wght.woff2",
            "fonts/ibm-plex-sans/ibm-plex-sans-latin-ext-wght.woff2",
        ],
        metrics: m(1.0049, 0.9687, 1.025, 0.275),
    },
    Font {
        key: "manrope",
        family: "Manrope",
        category: Category::Sans,
        weights: "200 800",
        files: &[
            "fonts/manrope/manrope-latin-wght.woff2",
            "fonts/manrope/manrope-latin-ext-wght.woff2",
        ],
        metrics: m(1.0198, 0.9831, 1.065, 0.30),
    },
    Font {
        key: "dm-sans",
        family: "DM Sans",
        category: Category::Sans,
        weights: "100 1000",
        files: &[
            "fonts/dm-sans/dm-sans-latin-wght.woff2",
            "fonts/dm-sans/dm-sans-latin-ext-wght.woff2",
        ],
        metrics: m(1.0264, 0.9967, 0.9925, 0.31),
    },
    Font {
        key: "plus-jakarta-sans",
        family: "Plus Jakarta Sans",
        category: Category::Sans,
        weights: "200 800",
        files: &[
            "fonts/plus-jakarta-sans/plus-jakarta-sans-latin-wght.woff2",
            "fonts/plus-jakarta-sans/plus-jakarta-sans-latin-ext-wght.woff2",
        ],
        metrics: m(1.0308, 0.9823, 1.0375, 0.2225),
    },
    Font {
        key: "figtree",
        family: "Figtree",
        category: Category::Sans,
        weights: "300 900",
        files: &[
            "fonts/figtree/figtree-latin-wght.woff2",
            "fonts/figtree/figtree-latin-ext-wght.woff2",
        ],
        metrics: m(0.9983, 0.9461, 0.95, 0.25),
    },
    Font {
        key: "outfit",
        family: "Outfit",
        category: Category::Sans,
        weights: "100 900",
        files: &[
            "fonts/outfit/outfit-latin-wght.woff2",
            "fonts/outfit/outfit-latin-ext-wght.woff2",
        ],
        metrics: m(0.9826, 0.9319, 1.0, 0.26),
    },
    Font {
        key: "space-grotesk",
        family: "Space Grotesk",
        category: Category::Sans,
        weights: "300 700",
        files: &[
            "fonts/space-grotesk/space-grotesk-latin-wght.woff2",
            "fonts/space-grotesk/space-grotesk-latin-ext-wght.woff2",
        ],
        metrics: m(1.083, 1.0103, 0.985, 0.2925),
    },
    Font {
        key: "fraunces",
        family: "Fraunces",
        category: Category::Serif,
        weights: "100 900",
        files: &[
            "fonts/fraunces/fraunces-latin-wght.woff2",
            "fonts/fraunces/fraunces-latin-ext-wght.woff2",
        ],
        metrics: m(1.0553, 0.9352, 0.9775, 0.255),
    },
    Font {
        key: "source-serif-4",
        family: "Source Serif 4",
        category: Category::Serif,
        weights: "200 900",
        files: &[
            "fonts/source-serif-4/source-serif-4-latin-wght.woff2",
            "fonts/source-serif-4/source-serif-4-latin-ext-wght.woff2",
        ],
        metrics: m(1.0809, 0.9397, 1.035, 0.335),
    },
];

/// The default family (Geist).
pub fn default_font() -> &'static Font {
    &FONTS[0]
}

/// Family by key.
pub fn by_key(key: &str) -> Option<&'static Font> {
    FONTS.iter().find(|f| f.key == key)
}

impl Font {
    /// Geist is declared by tokens.css; every other family by the theme CSS.
    pub fn is_builtin(&self) -> bool {
        self.metrics.is_none()
    }

    /// The `font-family` value (family, metric-matched fallback, generics).
    pub fn stack(&self) -> String {
        let generic = match self.category {
            Category::Sans => {
                "ui-sans-serif, system-ui, -apple-system, \"Segoe UI\", Roboto, Arial, sans-serif, \
                 \"Apple Color Emoji\", \"Segoe UI Emoji\""
            }
            Category::Serif => "ui-serif, Georgia, \"Times New Roman\", serif",
        };
        format!(
            "\"{fam}\", \"{fam} Fallback\", {generic}",
            fam = self.family
        )
    }

    /// The file holding the `latin` subset (or the only file).
    pub fn latin_file(&self) -> &'static str {
        self.files[0]
    }

    /// The `latin-ext` subset file, when split.
    pub fn latin_ext_file(&self) -> Option<&'static str> {
        self.files.get(1).copied()
    }

    /// `@font-face` rules (web font + metric-matched fallbacks); empty for
    /// Geist. `url` maps a static path to its versioned URL — the same
    /// function must build the `<link rel=preload>` hrefs.
    pub fn face_css(&self, url: &dyn Fn(&str) -> String) -> String {
        let Some(m) = self.metrics else {
            return String::new();
        };
        let mut css = String::new();
        let subsets = [
            (self.latin_file(), LATIN_RANGE),
            (self.latin_ext_file().unwrap_or_default(), LATIN_EXT_RANGE),
        ];
        for (file, range) in subsets {
            if file.is_empty() {
                continue;
            }
            css.push_str(&format!(
                "@font-face {{\n  font-family: \"{fam}\";\n  src: url(\"{src}\") format(\"woff2\");\n  \
                 font-weight: {w};\n  font-style: normal;\n  font-display: swap;\n  unicode-range: {range};\n}}\n",
                fam = self.family,
                src = url(file),
                w = self.weights,
            ));
        }
        let (regular, bold) = match self.category {
            Category::Sans => (
                "local(\"Arial\"), local(\"ArialMT\"), local(\"Liberation Sans\")",
                "local(\"Arial Bold\"), local(\"Arial-BoldMT\"), local(\"Liberation Sans Bold\")",
            ),
            Category::Serif => (
                "local(\"Georgia\"), local(\"Times New Roman\"), local(\"Liberation Serif\")",
                "local(\"Georgia Bold\"), local(\"Georgia-Bold\"), local(\"Times New Roman Bold\"), \
                 local(\"Liberation Serif Bold\")",
            ),
        };
        for (src, weights, size) in [
            (regular, "100 549", m.size400),
            (bold, "550 1000", m.size600),
        ] {
            css.push_str(&format!(
                "@font-face {{\n  font-family: \"{fam} Fallback\";\n  src: {src};\n  font-weight: {weights};\n  \
                 size-adjust: {sa:.2}%;\n  ascent-override: {asc:.2}%;\n  descent-override: {desc:.2}%;\n  \
                 line-gap-override: 0%;\n}}\n",
                fam = self.family,
                sa = size * 100.0,
                asc = m.ascent / size * 100.0,
                desc = m.descent / size * 100.0,
            ));
        }
        css
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_consistent() {
        let mut keys: Vec<_> = FONTS.iter().map(|f| f.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), FONTS.len());
        assert_eq!(default_font().key, "geist");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static");
        for f in FONTS {
            for file in f.files {
                assert!(dir.join(file).is_file(), "{file} missing");
            }
            if !f.is_builtin() {
                assert_eq!(f.files.len(), 2, "{}", f.key);
                let lic = dir.join("fonts").join(f.key).join("OFL.txt");
                let text = std::fs::read_to_string(&lic).expect("licence file");
                assert!(text.to_uppercase().contains("SIL OPEN FONT LICENSE"));
            }
        }
    }

    #[test]
    fn face_css_has_fallbacks() {
        let inter = by_key("inter").unwrap();
        let css = inter.face_css(&|p| format!("/static/{p}?v=1"));
        assert!(css.contains("font-family: \"Inter\""));
        assert!(css.contains("/static/fonts/inter/inter-latin-ext-wght.woff2?v=1"));
        assert!(css.contains("font-family: \"Inter Fallback\""));
        assert!(css.contains("size-adjust: 106.74%"));
        assert!(
            by_key("geist")
                .unwrap()
                .face_css(&|p| p.to_owned())
                .is_empty()
        );
        assert!(inter.stack().starts_with("\"Inter\", \"Inter Fallback\""));
    }
}
