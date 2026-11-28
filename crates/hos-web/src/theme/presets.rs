//! Customer-side design templates ("presets"), selectable in
//! `/admin/settings/appearance`.
//!
//! A template is a `data-template="<key>"` attribute on `<html>` (customer
//! surfaces only: storefront, cart, auth, client area), one stylesheet
//! `static/css/themes/<key>.css` loaded only while it is active (tokens in
//! `@layer tokens`, component variants in `@layer template`), and — where
//! the markup must differ — Askama partials under `templates/themes/<key>/`
//! chosen with `{% if tpl() == "<key>" %}`. Page templates exist once.
//! Each template also brings default fonts, corner radius, colour mode and
//! brand palette; the tenant can override every one of them.

/// Corner radius scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Radius {
    /// Nearly square.
    Sharp,
    /// The Classic scale.
    Soft,
    /// Generous, pill-like controls.
    Round,
}

impl Radius {
    /// All values, in picker order.
    pub const ALL: [Radius; 3] = [Radius::Sharp, Radius::Soft, Radius::Round];

    /// Stored key.
    pub fn key(self) -> &'static str {
        match self {
            Radius::Sharp => "sharp",
            Radius::Soft => "soft",
            Radius::Round => "round",
        }
    }

    /// Parses a stored key.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.key() == s)
    }

    /// `--radius-sm`, `--radius`, `--radius-lg`, `--radius-xl` (px).
    pub fn scale(self) -> [u8; 4] {
        match self {
            Radius::Sharp => [2, 3, 4, 4],
            Radius::Soft => [4, 6, 8, 8],
            Radius::Round => [8, 12, 18, 26],
        }
    }
}

/// Default colour mode for visitors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Follow the operating system.
    System,
    /// Light.
    Light,
    /// Dark.
    Dark,
}

impl Mode {
    /// All values, in picker order.
    pub const ALL: [Mode; 3] = [Mode::System, Mode::Light, Mode::Dark];

    /// Stored key.
    pub fn key(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }

    /// Parses a stored key.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.key() == s)
    }
}

/// One template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Key (`data-template`, stored in `appearance.template`).
    pub key: &'static str,
    /// Template stylesheet (relative to `static/`); `None` for Classic.
    pub css: Option<&'static str>,
    /// Picker thumbnail (relative to `static/`).
    pub thumbnail: &'static str,
    /// Default body font key.
    pub body_font: &'static str,
    /// Default heading font key.
    pub heading_font: &'static str,
    /// Default radius.
    pub radius: Radius,
    /// Default colour mode.
    pub mode: Mode,
    /// Default brand colour (when the tenant has none).
    pub brand: &'static str,
    /// Default accent colour.
    pub accent: &'static str,
    /// Composition of the generated share image (`/og/*.png`).
    pub share_style: hos_seo::og::Style,
}

/// The templates, Classic (the default) first.
pub const PRESETS: &[Preset] = &[
    Preset {
        key: "classic",
        css: None,
        thumbnail: "img/themes/classic.webp",
        body_font: "geist",
        heading_font: "geist",
        radius: Radius::Soft,
        mode: Mode::System,
        brand: "#2563eb",
        accent: "#f59e0b",
        share_style: hos_seo::og::Style::Grid,
    },
    Preset {
        key: "cloud",
        css: Some("css/themes/cloud.css"),
        thumbnail: "img/themes/cloud.webp",
        body_font: "inter",
        heading_font: "plus-jakarta-sans",
        radius: Radius::Round,
        mode: Mode::Light,
        brand: "#5b47f5",
        accent: "#06b6d4",
        share_style: hos_seo::og::Style::Orbs,
    },
    Preset {
        key: "corporate",
        css: Some("css/themes/corporate.css"),
        thumbnail: "img/themes/corporate.webp",
        body_font: "ibm-plex-sans",
        heading_font: "ibm-plex-sans",
        radius: Radius::Sharp,
        mode: Mode::Light,
        brand: "#1d5bd6",
        accent: "#0f9d6b",
        share_style: hos_seo::og::Style::Grid,
    },
    Preset {
        key: "terminal",
        css: Some("css/themes/terminal.css"),
        thumbnail: "img/themes/terminal.webp",
        body_font: "inter",
        heading_font: "space-grotesk",
        radius: Radius::Sharp,
        mode: Mode::Dark,
        brand: "#3ddc84",
        accent: "#f5b43c",
        share_style: hos_seo::og::Style::Terminal,
    },
    Preset {
        key: "studio",
        css: Some("css/themes/studio.css"),
        thumbnail: "img/themes/studio.webp",
        body_font: "figtree",
        heading_font: "outfit",
        radius: Radius::Round,
        mode: Mode::Light,
        brand: "#e8590c",
        accent: "#0b7a75",
        share_style: hos_seo::og::Style::Blocks,
    },
    Preset {
        key: "editorial",
        css: Some("css/themes/editorial.css"),
        thumbnail: "img/themes/editorial.webp",
        body_font: "dm-sans",
        heading_font: "fraunces",
        radius: Radius::Sharp,
        mode: Mode::Light,
        brand: "#1f3a2e",
        accent: "#b4532a",
        share_style: hos_seo::og::Style::Paper,
    },
    Preset {
        key: "atlas",
        css: Some("css/themes/atlas.css"),
        thumbnail: "img/themes/atlas.webp",
        body_font: "manrope",
        heading_font: "manrope",
        radius: Radius::Sharp,
        mode: Mode::Light,
        brand: "#0b6bd3",
        accent: "#0fa3b1",
        share_style: hos_seo::og::Style::Rack,
    },
];

/// The default template (Classic).
pub fn default_preset() -> &'static Preset {
    &PRESETS[0]
}

/// Template by key.
pub fn by_key(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_reference_real_fonts_and_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static");
        assert_eq!(default_preset().key, "classic");
        for p in PRESETS {
            assert!(
                super::super::fonts::by_key(p.body_font).is_some(),
                "{}",
                p.key
            );
            assert!(
                super::super::fonts::by_key(p.heading_font).is_some(),
                "{}",
                p.key
            );
            assert!(hos_core::tenant::HexColor::new(p.brand).is_ok());
            assert!(hos_core::tenant::HexColor::new(p.accent).is_ok());
            if let Some(css) = p.css {
                assert!(dir.join(css).is_file(), "{css}");
            }
            assert!(dir.join(p.thumbnail).is_file(), "{}", p.thumbnail);
        }
        assert_eq!(Radius::parse("round"), Some(Radius::Round));
        assert_eq!(Mode::parse("dark"), Some(Mode::Dark));
        assert_eq!(Radius::Soft.scale(), [4, 6, 8, 8]);
    }
}
