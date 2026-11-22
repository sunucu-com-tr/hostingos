//! Two passes: [`lay_out`] flows the document into pages of drawing
//! operations (so the page count is known), [`write`] draws them with krilla.

use krilla::Document;
use krilla::color::rgb;
use krilla::geom::{PathBuilder, Point, Rect, Size};
use krilla::image::Image;
use krilla::metadata::Metadata;
use krilla::num::NormalizedF32;
use krilla::page::PageSettings;
use krilla::paint::{Fill, Stroke};
use krilla::surface::Surface;
use krilla::text::TextDirection;

use crate::font::Fonts;
use crate::{InvoicePdf, LogoFormat, PdfError, StampTone};

const PAGE_W: f32 = 595.28;
const PAGE_H: f32 = 841.89;
const MARGIN_X: f32 = 42.0;
const MARGIN_TOP: f32 = 42.0;
/// Content stops here; the footer sits below.
const CONTENT_BOTTOM: f32 = PAGE_H - 58.0;
const CONTENT_W: f32 = PAGE_W - 2.0 * MARGIN_X;

const BODY: f32 = 9.0;
const SMALL: f32 = 7.5;
const LINE: f32 = 12.5;

// Column geometry of the item table (right edges of the numeric columns).
const COL_AMOUNT_R: f32 = PAGE_W - MARGIN_X;
const COL_UNIT_R: f32 = COL_AMOUNT_R - 92.0;
const COL_QTY_R: f32 = COL_UNIT_R - 92.0;
const COL_DESC_W: f32 = COL_QTY_R - 52.0 - MARGIN_X;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Tone {
    Ink,
    Muted,
    Line,
    Rgb(u8, u8, u8),
}

impl Tone {
    fn color(self) -> rgb::Color {
        match self {
            Self::Ink => rgb::Color::new(28, 25, 23),
            Self::Muted => rgb::Color::new(110, 104, 98),
            Self::Line => rgb::Color::new(214, 209, 202),
            Self::Rgb(r, g, b) => rgb::Color::new(r, g, b),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Op {
    Text {
        x: f32,
        y: f32,
        size: f32,
        bold: bool,
        tone: Tone,
        text: String,
    },
    Rule {
        x1: f32,
        x2: f32,
        y: f32,
        width: f32,
        tone: Tone,
    },
    Box {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        stroke: Tone,
        width: f32,
    },
    Logo {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
}

/// Upper-cases a label; Turkish maps `i` → `İ` and `ı` → `I`.
pub(crate) fn upper(lang: &str, text: &str) -> String {
    if lang.starts_with("tr") {
        text.chars()
            .map(|c| match c {
                'i' => "İ".to_owned(),
                'ı' => "I".to_owned(),
                c => c.to_uppercase().collect(),
            })
            .collect()
    } else {
        text.to_uppercase()
    }
}

struct Flow<'a> {
    fonts: &'a Fonts,
    pages: Vec<Vec<Op>>,
    y: f32,
}

impl<'a> Flow<'a> {
    fn new(fonts: &'a Fonts) -> Self {
        Self {
            fonts,
            pages: vec![Vec::new()],
            y: MARGIN_TOP,
        }
    }

    fn ops(&mut self) -> &mut Vec<Op> {
        // `pages` always has at least one page.
        let last = self.pages.len() - 1;
        &mut self.pages[last]
    }

    fn new_page(&mut self) {
        self.pages.push(Vec::new());
        self.y = MARGIN_TOP;
    }

    /// Starts a new page when `h` more points do not fit; `true` if it did.
    fn ensure(&mut self, h: f32) -> bool {
        if self.y + h > CONTENT_BOTTOM && self.y > MARGIN_TOP {
            self.new_page();
            true
        } else {
            false
        }
    }

    fn text(&mut self, x: f32, baseline: f32, size: f32, bold: bool, tone: Tone, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ops().push(Op::Text {
            x,
            y: baseline,
            size,
            bold,
            tone,
            text: text.to_owned(),
        });
    }

    fn text_right(
        &mut self,
        right: f32,
        baseline: f32,
        size: f32,
        bold: bool,
        tone: Tone,
        text: &str,
    ) {
        let w = self.fonts.width(text, size, bold);
        self.text(right - w, baseline, size, bold, tone, text);
    }

    fn rule(&mut self, x1: f32, x2: f32, y: f32, width: f32, tone: Tone) {
        self.ops().push(Op::Rule {
            x1,
            x2,
            y,
            width,
            tone,
        });
    }
}

/// Lays the document out into pages.
pub(crate) fn lay_out(doc: &InvoicePdf, fonts: &Fonts) -> Vec<Vec<Op>> {
    let mut f = Flow::new(fonts);
    header(doc, &mut f);
    parties(doc, &mut f);
    legal(doc, &mut f);
    table(doc, &mut f);
    totals(doc, &mut f);
    paragraphs(&mut f, &upper(&doc.lang, &doc.notes_heading), &doc.notes);
    paragraphs(&mut f, &upper(&doc.lang, &doc.bank_heading), &doc.bank);
    if !doc.disclaimer.is_empty() {
        let lines = fonts.wrap(&doc.disclaimer, SMALL, false, CONTENT_W);
        f.ensure(lines.len() as f32 * 10.0 + 8.0);
        f.y += 8.0;
        for l in lines {
            f.y += 10.0;
            let y = f.y;
            f.text(MARGIN_X, y, SMALL, false, Tone::Muted, &l);
        }
    }
    f.pages
}

fn header(doc: &InvoicePdf, f: &mut Flow<'_>) {
    let top = f.y;
    // Left: logo or brand name.
    let mut left_bottom = top;
    match &doc.logo {
        Some(logo) => match image(logo) {
            Some(img) => {
                let (iw, ih) = img.size();
                let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
                let scale = (170.0 / iw).min(52.0 / ih);
                let (w, h) = (iw * scale, ih * scale);
                f.ops().push(Op::Logo {
                    x: MARGIN_X,
                    y: top,
                    w,
                    h,
                });
                left_bottom = top + h;
            }
            None => {
                f.text(MARGIN_X, top + 16.0, 16.0, true, Tone::Ink, &doc.brand);
                left_bottom = top + 22.0;
            }
        },
        None => {
            f.text(MARGIN_X, top + 16.0, 16.0, true, Tone::Ink, &doc.brand);
            left_bottom = left_bottom.max(top + 22.0);
        }
    }
    // Right: heading, number, meta.
    let right = PAGE_W - MARGIN_X;
    let mut y = top + 20.0;
    f.text_right(right, y, 20.0, true, Tone::Ink, &doc.heading);
    y += 16.0;
    f.text_right(right, y, 10.5, true, Tone::Ink, &doc.number);
    for (label, value) in &doc.meta {
        y += LINE;
        let v_w = f.fonts.width(value, BODY, false);
        f.text_right(right, y, BODY, false, Tone::Ink, value);
        f.text_right(
            right - v_w - 6.0,
            y,
            BODY,
            false,
            Tone::Muted,
            &format!("{label}:"),
        );
    }
    f.y = left_bottom.max(y) + 18.0;
    f.rule(MARGIN_X, PAGE_W - MARGIN_X, f.y, 0.75, Tone::Line);
    f.y += 14.0;
}

fn block_lines(f: &Flow<'_>, lines: &[String], width: f32) -> Vec<String> {
    lines
        .iter()
        .flat_map(|l| f.fonts.wrap(l, BODY, false, width))
        .collect()
}

fn parties(doc: &InvoicePdf, f: &mut Flow<'_>) {
    let col_w = CONTENT_W / 2.0 - 12.0;
    let left = block_lines(f, &doc.seller, col_w);
    let right = block_lines(f, &doc.buyer, col_w);
    let rows = left.len().max(right.len());
    f.ensure(14.0 + rows as f32 * LINE);
    let top = f.y;
    let x_right = MARGIN_X + CONTENT_W / 2.0 + 12.0;
    f.text(
        MARGIN_X,
        top + 8.0,
        SMALL,
        true,
        Tone::Muted,
        &upper(&doc.lang, &doc.seller_heading),
    );
    f.text(
        x_right,
        top + 8.0,
        SMALL,
        true,
        Tone::Muted,
        &upper(&doc.lang, &doc.buyer_heading),
    );
    for (i, l) in left.iter().enumerate() {
        let y = top + 8.0 + (i + 1) as f32 * LINE;
        f.text(MARGIN_X, y, BODY, i == 0, Tone::Ink, l);
    }
    for (i, l) in right.iter().enumerate() {
        let y = top + 8.0 + (i + 1) as f32 * LINE;
        f.text(x_right, y, BODY, i == 0, Tone::Ink, l);
    }
    f.y = top + 8.0 + rows as f32 * LINE + 16.0;
}

fn legal(doc: &InvoicePdf, f: &mut Flow<'_>) {
    if doc.legal.is_empty() {
        return;
    }
    let lines = block_lines(f, &doc.legal, CONTENT_W - 16.0);
    let h = lines.len() as f32 * LINE + 10.0;
    f.ensure(h + 8.0);
    let top = f.y;
    f.ops().push(Op::Box {
        x: MARGIN_X,
        y: top,
        w: CONTENT_W,
        h,
        stroke: Tone::Line,
        width: 0.75,
    });
    for (i, l) in lines.iter().enumerate() {
        let y = top + 4.0 + (i + 1) as f32 * LINE - 2.0;
        f.text(MARGIN_X + 8.0, y, BODY, i == 0, Tone::Ink, l);
    }
    f.y = top + h + 14.0;
}

fn table_head(doc: &InvoicePdf, f: &mut Flow<'_>, continued: bool) {
    let y = f.y + 10.0;
    let desc = if continued && !doc.continued_label.is_empty() {
        format!("{} ({})", doc.columns[0], doc.continued_label)
    } else {
        doc.columns[0].clone()
    };
    f.text(
        MARGIN_X,
        y,
        SMALL,
        true,
        Tone::Muted,
        &upper(&doc.lang, &desc),
    );
    f.text_right(
        COL_QTY_R,
        y,
        SMALL,
        true,
        Tone::Muted,
        &upper(&doc.lang, &doc.columns[1]),
    );
    f.text_right(
        COL_UNIT_R,
        y,
        SMALL,
        true,
        Tone::Muted,
        &upper(&doc.lang, &doc.columns[2]),
    );
    f.text_right(
        COL_AMOUNT_R,
        y,
        SMALL,
        true,
        Tone::Muted,
        &upper(&doc.lang, &doc.columns[3]),
    );
    f.y = y + 6.0;
    f.rule(MARGIN_X, PAGE_W - MARGIN_X, f.y, 0.9, Tone::Ink);
    f.y += 4.0;
}

fn table(doc: &InvoicePdf, f: &mut Flow<'_>) {
    f.ensure(40.0);
    table_head(doc, f, false);
    for line in &doc.lines {
        let desc = f.fonts.wrap(&line.description, BODY, false, COL_DESC_W);
        let detail = if line.detail.is_empty() {
            Vec::new()
        } else {
            f.fonts.wrap(&line.detail, SMALL, false, COL_DESC_W)
        };
        let h = desc.len() as f32 * LINE + detail.len() as f32 * 10.0 + 8.0;
        if f.ensure(h) {
            table_head(doc, f, true);
        }
        let top = f.y;
        let first = top + LINE;
        for (i, l) in desc.iter().enumerate() {
            let y = top + (i + 1) as f32 * LINE;
            f.text(MARGIN_X, y, BODY, false, Tone::Ink, l);
        }
        let mut y = top + desc.len() as f32 * LINE;
        for l in &detail {
            y += 10.0;
            f.text(MARGIN_X, y, SMALL, false, Tone::Muted, l);
        }
        f.text_right(COL_QTY_R, first, BODY, false, Tone::Ink, &line.quantity);
        f.text_right(COL_UNIT_R, first, BODY, false, Tone::Ink, &line.unit_price);
        f.text_right(COL_AMOUNT_R, first, BODY, false, Tone::Ink, &line.amount);
        f.y = top + h;
        f.rule(MARGIN_X, PAGE_W - MARGIN_X, f.y - 2.0, 0.4, Tone::Line);
    }
    f.y += 8.0;
}

fn stamp_color(tone: StampTone) -> Tone {
    match tone {
        StampTone::Success => Tone::Rgb(22, 128, 61),
        StampTone::Danger => Tone::Rgb(185, 28, 28),
        StampTone::Warning => Tone::Rgb(180, 110, 0),
        StampTone::Neutral => Tone::Rgb(110, 104, 98),
    }
}

fn totals(doc: &InvoicePdf, f: &mut Flow<'_>) {
    let rows = doc.totals.len() as f32;
    let needed = rows * (LINE + 2.0) + 16.0;
    f.ensure(needed.max(if doc.stamp.is_some() { 48.0 } else { 0.0 }));
    let top = f.y;
    let label_r = COL_UNIT_R;
    let mut y = top;
    for t in &doc.totals {
        if t.emphasis {
            y += 4.0;
            f.rule(COL_QTY_R - 40.0, PAGE_W - MARGIN_X, y, 0.9, Tone::Ink);
            y += 2.0;
        }
        y += LINE + 2.0;
        let (size, tone) = if t.emphasis {
            (10.5, Tone::Ink)
        } else {
            (BODY, Tone::Muted)
        };
        f.text_right(label_r, y, size, t.emphasis, tone, &t.label);
        f.text_right(COL_AMOUNT_R, y, size, t.emphasis, Tone::Ink, &t.value);
    }
    if let Some(stamp) = &doc.stamp {
        let size = 14.0;
        let w = f.fonts.width(&stamp.text, size, true) + 24.0;
        let x = MARGIN_X;
        let sy = top + 6.0;
        let color = stamp_color(stamp.tone);
        f.ops().push(Op::Box {
            x,
            y: sy,
            w,
            h: 28.0,
            stroke: color,
            width: 2.0,
        });
        f.text(x + 12.0, sy + 19.0, size, true, color, &stamp.text);
        y = y.max(sy + 28.0);
    }
    f.y = y + 18.0;
}

fn paragraphs(f: &mut Flow<'_>, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    f.ensure(LINE * 2.0 + 10.0);
    f.y += 6.0;
    let y = f.y + 8.0;
    f.text(MARGIN_X, y, SMALL, true, Tone::Muted, heading);
    f.y = y + 2.0;
    for p in items {
        for l in f.fonts.wrap(p, BODY, false, CONTENT_W) {
            f.ensure(LINE);
            f.y += LINE;
            let y = f.y;
            f.text(MARGIN_X, y, BODY, false, Tone::Ink, &l);
        }
        f.y += 3.0;
    }
    f.y += 8.0;
}

/// The logo for embedding, or `None` (the brand name is printed instead)
/// when its size is unknown from the headers or above
/// [`hos_core::image::MAX_SIDE`]: krilla decodes the whole bitmap, so an
/// oversized logo (uploaded before uploads were size-checked) must never
/// reach it in a process every tenant shares.
pub(crate) fn image(logo: &crate::Logo) -> Option<Image> {
    if !hos_core::image::fits(&logo.data, hos_core::image::MAX_SIDE) {
        return None;
    }
    let data: krilla::Data = logo.data.clone().into();
    match logo.format {
        LogoFormat::Png => Image::from_png(data, true),
        LogoFormat::Jpeg => Image::from_jpeg(data, true),
        LogoFormat::Gif => Image::from_gif(data, true),
        LogoFormat::Webp => Image::from_webp(data, true),
    }
    .ok()
}

fn fill(tone: Tone) -> Fill {
    Fill {
        paint: tone.color().into(),
        opacity: NormalizedF32::ONE,
        ..Fill::default()
    }
}

fn stroke(tone: Tone, width: f32) -> Stroke {
    Stroke {
        paint: tone.color().into(),
        width,
        ..Stroke::default()
    }
}

fn draw(surface: &mut Surface<'_>, fonts: &Fonts, logo: Option<&Image>, op: &Op) {
    match op {
        Op::Text {
            x,
            y,
            size,
            bold,
            tone,
            text,
        } => {
            surface.set_fill(Some(fill(*tone)));
            surface.set_stroke(None);
            let mut cursor = *x;
            for run in fonts.runs(text, *bold) {
                // Trailing spaces are advanced over, not drawn, so text
                // extraction sees one gap at a font switch, not two.
                let visible = run.text.trim_end();
                if !visible.is_empty() {
                    surface.draw_text(
                        Point::from_xy(cursor, *y),
                        fonts.font(*bold, run.fallback).clone(),
                        *size,
                        visible,
                        false,
                        TextDirection::Auto,
                    );
                }
                cursor += fonts.run_width(&run, *size, *bold);
            }
        }
        Op::Rule {
            x1,
            x2,
            y,
            width,
            tone,
        } => {
            let mut pb = PathBuilder::new();
            pb.move_to(*x1, *y);
            pb.line_to(*x2, *y);
            if let Some(path) = pb.finish() {
                surface.set_fill(None);
                surface.set_stroke(Some(stroke(*tone, *width)));
                surface.draw_path(&path);
            }
        }
        Op::Box {
            x,
            y,
            w,
            h,
            stroke: tone,
            width,
        } => {
            if let Some(rect) = Rect::from_xywh(*x, *y, *w, *h) {
                let mut pb = PathBuilder::new();
                pb.push_rect(rect);
                if let Some(path) = pb.finish() {
                    surface.set_fill(None);
                    surface.set_stroke(Some(stroke(*tone, *width)));
                    surface.draw_path(&path);
                }
            }
        }
        Op::Logo { x, y, w, h } => {
            if let (Some(img), Some(size)) = (logo, Size::from_wh(*w, *h)) {
                surface.push_transform(&krilla::geom::Transform::from_translate(*x, *y));
                surface.draw_image(img.clone(), size);
                surface.pop();
            }
        }
    }
}

/// Draws the laid-out pages (footer with `Page n / m` on every page).
pub(crate) fn write(
    doc: &InvoicePdf,
    fonts: &Fonts,
    pages: &[Vec<Op>],
) -> Result<Vec<u8>, PdfError> {
    let mut document = Document::new();
    let mut metadata = Metadata::new().creator("HostingOS".to_owned());
    if !doc.title.is_empty() {
        metadata = metadata.title(doc.title.clone());
    }
    if !doc.lang.is_empty() {
        metadata = metadata.language(doc.lang.clone());
    }
    document.set_metadata(metadata);
    let logo = doc.logo.as_ref().and_then(image);
    let total = pages.len();
    for (i, ops) in pages.iter().enumerate() {
        let settings = PageSettings::from_wh(PAGE_W, PAGE_H)
            .ok_or_else(|| PdfError::Write("invalid page size".into()))?;
        let mut page = document.start_page_with(settings);
        let mut surface = page.surface();
        for op in ops {
            draw(&mut surface, fonts, logo.as_ref(), op);
        }
        // Footer.
        let fy = PAGE_H - 30.0;
        draw(
            &mut surface,
            fonts,
            None,
            &Op::Rule {
                x1: MARGIN_X,
                x2: PAGE_W - MARGIN_X,
                y: fy - 12.0,
                width: 0.5,
                tone: Tone::Line,
            },
        );
        if !doc.footer.is_empty() {
            let max = CONTENT_W - 90.0;
            let text = fonts
                .wrap(&doc.footer, SMALL, false, max)
                .into_iter()
                .next()
                .unwrap_or_default();
            draw(
                &mut surface,
                fonts,
                None,
                &Op::Text {
                    x: MARGIN_X,
                    y: fy,
                    size: SMALL,
                    bold: false,
                    tone: Tone::Muted,
                    text,
                },
            );
        }
        let counter = format!("{} {} / {}", doc.page_label, i + 1, total);
        let w = fonts.width(&counter, SMALL, false);
        draw(
            &mut surface,
            fonts,
            None,
            &Op::Text {
                x: PAGE_W - MARGIN_X - w,
                y: fy,
                size: SMALL,
                bold: false,
                tone: Tone::Muted,
                text: counter,
            },
        );
        surface.finish();
        page.finish();
    }
    document
        .finish()
        .map_err(|e| PdfError::Write(format!("{e:?}")))
}
