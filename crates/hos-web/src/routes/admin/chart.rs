//! Inline SVG charts (no JavaScript, no external library).
//!
//! Single-series column chart and horizontal bar list. Colours come from CSS
//! classes in `static/css/admin.css` (`.chart__*`), so light / dark themes and
//! tenant brand colours apply. Every mark carries a `<title>` (native
//! tooltip, also read by screen readers) inside a focusable group with a hit
//! area larger than the mark; pages show the same numbers as a table too.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive as _;

use crate::view::escape_html;

/// One data point.
#[derive(Debug, Clone)]
pub struct Point {
    /// Category label (month, product …).
    pub label: String,
    /// Value.
    pub value: Decimal,
    /// Formatted value for the tooltip.
    pub display: String,
}

fn nice_step(max: f64, ticks: f64) -> f64 {
    if max <= 0.0 {
        return 1.0;
    }
    let raw = max / ticks;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let nice = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 2.5 {
        2.5
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

/// Compact axis number (`12,5 B` / `12.5K`).
pub fn compact(v: f64, tr: bool) -> String {
    let (n, suffix) = if v.abs() >= 1_000_000.0 {
        (v / 1_000_000.0, if tr { " Mn" } else { "M" })
    } else if v.abs() >= 1_000.0 {
        (v / 1_000.0, if tr { " B" } else { "K" })
    } else {
        (v, "")
    };
    let mut s = if (n.fract()).abs() < 0.05 {
        format!("{n:.0}")
    } else {
        format!("{n:.1}")
    };
    if tr {
        s = s.replace('.', ",");
    }
    format!("{s}{suffix}")
}

/// Rounded-top bar anchored to the baseline (4 px data-end radius).
fn bar_path(x: f64, y: f64, w: f64, base: f64) -> String {
    let h = base - y;
    if h <= 0.5 {
        return format!("M{x:.1},{base:.1}h{w:.1}v-0.5h-{w:.1}z");
    }
    let r = 4f64.min(w / 2.0).min(h);
    format!(
        "M{x:.1},{base:.1}V{:.1}Q{x:.1},{y:.1} {:.1},{y:.1}H{:.1}Q{:.1},{y:.1} {:.1},{:.1}V{base:.1}z",
        y + r,
        x + r,
        x + w - r,
        x + w,
        x + w,
        y + r
    )
}

/// Column chart of `points` (single series). `title` labels the SVG.
pub fn columns(points: &[Point], title: &str, tr: bool) -> String {
    let (w, h) = (720.0, 240.0);
    let (left, right, top, bottom) = (52.0, 8.0, 12.0, 28.0);
    let plot_w = w - left - right;
    let plot_h = h - top - bottom;
    let max = points
        .iter()
        .filter_map(|p| p.value.to_f64())
        .fold(0.0_f64, f64::max);
    let step = nice_step(max, 4.0);
    let ymax = if max <= 0.0 {
        step * 4.0
    } else {
        (max / step).ceil() * step
    };
    let y = |v: f64| top + plot_h - (v / ymax) * plot_h;
    let mut svg = format!(
        r#"<svg class="chart" viewBox="0 0 {w} {h}" role="img" aria-label="{}" preserveAspectRatio="xMidYMid meet">"#,
        escape_html(title)
    );
    // Grid + y labels (recessive).
    let mut t = 0.0;
    while t <= ymax + step / 2.0 {
        let yy = y(t);
        svg.push_str(&format!(
            r#"<line class="chart__grid" x1="{left}" x2="{}" y1="{yy:.1}" y2="{yy:.1}"/><text class="chart__axis" x="{}" y="{:.1}" text-anchor="end">{}</text>"#,
            w - right,
            left - 8.0,
            yy + 4.0,
            escape_html(&compact(t, tr))
        ));
        t += step;
    }
    let n = points.len().max(1) as f64;
    let band = plot_w / n;
    let bw = (band * 0.62).clamp(4.0, 40.0);
    let base = y(0.0);
    for (i, p) in points.iter().enumerate() {
        let v = p.value.to_f64().unwrap_or(0.0).max(0.0);
        let cx = left + band * (i as f64) + band / 2.0;
        let x = cx - bw / 2.0;
        let label = format!("{}: {}", p.label, p.display);
        svg.push_str(&format!(
            r#"<g class="chart__mark" tabindex="0"><title>{}</title><rect class="chart__hit" x="{:.1}" y="{top}" width="{:.1}" height="{plot_h}"/><path class="chart__bar" d="{}"/></g>"#,
            escape_html(&label),
            left + band * (i as f64),
            band,
            bar_path(x, y(v), bw, base)
        ));
        svg.push_str(&format!(
            r#"<text class="chart__axis" x="{cx:.1}" y="{}" text-anchor="middle">{}</text>"#,
            h - 8.0,
            escape_html(&p.label)
        ));
    }
    svg.push_str(&format!(
        r#"<line class="chart__baseline" x1="{left}" x2="{}" y1="{base:.1}" y2="{base:.1}"/></svg>"#,
        w - right
    ));
    svg
}

/// Horizontal bars (ranked lists: products, gateways). Labels and values are
/// text (ink colours); the bar carries the magnitude.
pub fn hbars(points: &[Point], title: &str) -> String {
    let row = 30.0;
    let (w, label_w, value_w) = (720.0, 220.0, 120.0);
    let h = (points.len().max(1) as f64) * row + 8.0;
    let max = points
        .iter()
        .filter_map(|p| p.value.to_f64())
        .fold(0.0_f64, f64::max);
    let plot_w = w - label_w - value_w - 16.0;
    let mut svg = format!(
        r#"<svg class="chart chart--h" viewBox="0 0 {w} {h}" role="img" aria-label="{}">"#,
        escape_html(title)
    );
    for (i, p) in points.iter().enumerate() {
        let v = p.value.to_f64().unwrap_or(0.0).max(0.0);
        let y0 = 4.0 + row * (i as f64);
        let bw = if max > 0.0 { (v / max) * plot_w } else { 0.0 };
        let label: String = if p.label.chars().count() > 30 {
            p.label
                .chars()
                .take(29)
                .chain(std::iter::once('…'))
                .collect()
        } else {
            p.label.clone()
        };
        let x0 = label_w + 8.0;
        let bh = 14.0;
        let by = y0 + (row - bh) / 2.0 - 2.0;
        let r = 4f64.min(bh / 2.0).min(bw.max(0.0));
        let path = if bw < 1.0 {
            format!("M{x0:.1},{by:.1}h1v{bh}h-1z")
        } else {
            format!(
                "M{x0:.1},{by:.1}H{:.1}Q{:.1},{by:.1} {:.1},{:.1}V{:.1}Q{:.1},{:.1} {:.1},{:.1}H{x0:.1}z",
                x0 + bw - r,
                x0 + bw,
                x0 + bw,
                by + r,
                by + bh - r,
                x0 + bw,
                by + bh,
                x0 + bw - r,
                by + bh
            )
        };
        svg.push_str(&format!(
            r#"<g class="chart__mark" tabindex="0"><title>{}</title><rect class="chart__hit" x="0" y="{y0:.1}" width="{w}" height="{row}"/><text class="chart__label" x="{label_w}" y="{:.1}" text-anchor="end">{}</text><path class="chart__bar" d="{path}"/><text class="chart__value" x="{:.1}" y="{:.1}">{}</text></g>"#,
            escape_html(&format!("{}: {}", p.label, p.display)),
            by + bh - 3.0,
            escape_html(&label),
            x0 + bw + 8.0,
            by + bh - 3.0,
            escape_html(&p.display)
        ));
    }
    svg.push_str("</svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_compact() {
        assert_eq!(nice_step(1000.0, 4.0), 250.0);
        assert_eq!(nice_step(9.0, 4.0), 2.5);
        assert_eq!(compact(12_500.0, true), "12,5 B");
        assert_eq!(compact(2_000_000.0, false), "2M");
        assert_eq!(compact(40.0, false), "40");
    }

    #[test]
    fn chart_escapes_labels() {
        let svg = columns(
            &[Point {
                label: "<b>".into(),
                value: Decimal::from(5),
                display: "5".into(),
            }],
            "t\"",
            false,
        );
        assert!(svg.contains("&lt;b&gt;"));
        assert!(!svg.contains("<b>"));
        assert!(svg.starts_with("<svg"));
        let h = hbars(
            &[Point {
                label: "x".into(),
                value: Decimal::ZERO,
                display: "0".into(),
            }],
            "h",
        );
        assert!(h.contains("chart__bar"));
    }
}
