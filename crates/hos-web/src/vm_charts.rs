//! Server-rendered SVG charts of VM usage history (client and admin VM
//! pages): CPU, memory, disk I/O and network from
//! [`MetricsHistory`], plus the bandwidth-vs-quota bar. No chart library,
//! no script: polylines in a fixed `viewBox`, styled by `vm-graph` CSS.

use chrono::{DateTime, Utc};
use hos_billing::vm_ops::BandwidthView;
use hos_core::integrations::virt::{MetricSample, MetricsHistory, MetricsRange};

/// Chart width / height (SVG user units).
const W: f64 = 600.0;
const H: f64 = 160.0;
const PAD: f64 = 6.0;

/// One line of a chart.
#[derive(Debug, Clone)]
pub struct ChartLine {
    /// Legend label.
    pub label: String,
    /// CSS modifier (`a`, `b`).
    pub class: &'static str,
    /// `points` of the polyline.
    pub points: String,
    /// Latest value, formatted.
    pub latest: String,
}

/// One chart.
#[derive(Debug, Clone)]
pub struct Chart {
    /// Title.
    pub title: String,
    /// Formatted top of the y axis.
    pub max: String,
    /// Lines (1–2).
    pub lines: Vec<ChartLine>,
    /// Range start / end labels.
    pub start: String,
    /// End label.
    pub end: String,
    /// No data in the range.
    pub empty: bool,
}

type Pick = fn(&MetricSample) -> Option<f64>;

/// Range from a query value (`hour|day|week|month`, default day).
pub fn parse_range(raw: Option<&str>) -> MetricsRange {
    match raw.unwrap_or("day") {
        "hour" => MetricsRange::Hour,
        "week" => MetricsRange::Week,
        "month" => MetricsRange::Month,
        _ => MetricsRange::Day,
    }
}

/// Labels: `t(key)` lookups by the caller.
pub struct Labels {
    pub cpu: String,
    pub memory: String,
    pub disk: String,
    pub network: String,
    pub read: String,
    pub write: String,
    pub inbound: String,
    pub outbound: String,
}

/// The four charts of a history.
pub fn charts(
    h: &MetricsHistory,
    l: &Labels,
    fmt_time: impl Fn(DateTime<Utc>) -> String,
) -> Vec<Chart> {
    let end = h
        .samples
        .iter()
        .rev()
        .find_map(|s| s.at)
        .unwrap_or_else(Utc::now);
    let start = end - h.range.duration();
    let span = (end - start).num_seconds().max(1) as f64;
    let mk = |title: &str,
              lines: &[(&str, &'static str, Pick)],
              fixed: Option<f64>,
              fmt: fn(f64) -> String| {
        let max_seen = lines
            .iter()
            .flat_map(|(_, _, pick)| h.samples.iter().filter_map(*pick))
            .filter(|v| v.is_finite())
            .fold(0.0_f64, f64::max);
        let max = fixed.unwrap_or_else(|| nice_max(max_seen));
        let mut empty = true;
        let lines = lines
            .iter()
            .map(|(label, class, pick)| {
                let mut pts = Vec::new();
                let mut latest = None;
                for s in &h.samples {
                    let (Some(at), Some(v)) = (s.at, pick(s).filter(|v| v.is_finite())) else {
                        continue;
                    };
                    empty = false;
                    latest = Some(v);
                    let x = ((at - start).num_seconds() as f64 / span).clamp(0.0, 1.0) * W;
                    let y = H - PAD - (v / max).clamp(0.0, 1.0) * (H - 2.0 * PAD);
                    pts.push(format!("{x:.1},{y:.1}"));
                }
                ChartLine {
                    label: (*label).to_owned(),
                    class,
                    points: pts.join(" "),
                    latest: latest.map_or_else(|| "—".to_owned(), fmt),
                }
            })
            .collect();
        Chart {
            title: title.to_owned(),
            max: fmt(max),
            lines,
            start: fmt_time(start),
            end: fmt_time(end),
            empty,
        }
    };
    let memory_max = h
        .samples
        .iter()
        .filter_map(|s| s.memory_total_mb)
        .fold(0.0_f64, f64::max);
    vec![
        mk(
            &l.cpu,
            &[(&l.cpu, "a", |s| s.cpu_percent)],
            Some(100.0),
            percent,
        ),
        mk(
            &l.memory,
            &[(&l.memory, "a", |s| s.memory_used_mb)],
            (memory_max > 0.0).then_some(memory_max),
            megabytes,
        ),
        mk(
            &l.disk,
            &[
                (&l.read, "a", |s| s.disk_read_bps),
                (&l.write, "b", |s| s.disk_write_bps),
            ],
            None,
            bytes_rate,
        ),
        mk(
            &l.network,
            &[
                (&l.inbound, "a", |s| s.net_in_bps),
                (&l.outbound, "b", |s| s.net_out_bps),
            ],
            None,
            bytes_rate,
        ),
    ]
}

/// A round axis maximum above `v` (1 / 2 / 5 × 10ⁿ).
fn nice_max(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let exp = v.log10().floor();
    let base = 10f64.powf(exp);
    for m in [1.0, 2.0, 5.0, 10.0] {
        if v <= m * base {
            return m * base;
        }
    }
    10.0 * base
}

fn percent(v: f64) -> String {
    format!("%{v:.0}")
}

fn megabytes(v: f64) -> String {
    if v >= 1024.0 {
        format!("{:.1} GB", v / 1024.0)
    } else {
        format!("{v:.0} MB")
    }
}

fn bytes_rate(v: f64) -> String {
    let (n, unit) = if v >= 1e9 {
        (v / 1e9, "GB/s")
    } else if v >= 1e6 {
        (v / 1e6, "MB/s")
    } else if v >= 1e3 {
        (v / 1e3, "kB/s")
    } else {
        (v, "B/s")
    };
    format!("{n:.1} {unit}")
}

/// The bandwidth bar.
#[derive(Debug, Clone)]
pub struct BandwidthBar {
    /// "123.4 GB".
    pub used: String,
    /// "1000 GB" or empty without a quota.
    pub quota: String,
    /// Bar width 0–100.
    pub percent: u8,
    /// Over the quota.
    pub over: bool,
    /// "12.3 GB" over the quota (empty when within).
    pub overage: String,
    /// Inbound / outbound split, when reported.
    pub split: String,
    /// The provider only estimates the figure.
    pub estimated: bool,
}

/// Bar figures of a [`BandwidthView`].
pub fn bandwidth_bar(v: &BandwidthView) -> BandwidthBar {
    let gb = |g: f64| {
        if g >= 1000.0 {
            format!("{:.2} TB", g / 1000.0)
        } else {
            format!("{g:.1} GB")
        }
    };
    let split = match (v.usage.incoming_gb, v.usage.outgoing_gb) {
        (Some(i), Some(o)) => format!("↓ {} · ↑ {}", gb(i), gb(o)),
        (None, Some(o)) => format!("↑ {}", gb(o)),
        (Some(i), None) => format!("↓ {}", gb(i)),
        (None, None) => String::new(),
    };
    BandwidthBar {
        used: gb(v.usage.counted_gb),
        quota: v.quota_gb.map(gb).unwrap_or_default(),
        // Bar width in steps of 5 (.vm-w-N); any use shows at least one step.
        percent: v
            .used_percent
            .map_or(0, |p| ((p.clamp(0.0, 100.0) / 5.0).ceil() * 5.0) as u8),
        over: v.overage_gb > 0.0,
        overage: if v.overage_gb > 0.0 {
            gb(v.overage_gb)
        } else {
            String::new()
        },
        split,
        estimated: v.usage.estimated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_and_points() {
        assert_eq!(nice_max(0.0), 1.0);
        assert_eq!(nice_max(3.2), 5.0);
        assert_eq!(nice_max(870.0), 1000.0);
        let now = Utc::now();
        let h = MetricsHistory {
            range: MetricsRange::Hour,
            step_secs: 60,
            samples: (0..3)
                .map(|i| MetricSample {
                    at: Some(now - chrono::Duration::minutes(2 - i)),
                    cpu_percent: Some(50.0),
                    ..MetricSample::default()
                })
                .collect(),
        };
        let l = Labels {
            cpu: "CPU".into(),
            memory: "RAM".into(),
            disk: "Disk".into(),
            network: "Net".into(),
            read: "r".into(),
            write: "w".into(),
            inbound: "in".into(),
            outbound: "out".into(),
        };
        let c = charts(&h, &l, |t| t.to_rfc3339());
        assert_eq!(c.len(), 4);
        assert!(!c[0].empty);
        assert_eq!(c[0].lines[0].points.split(' ').count(), 3);
        assert!(c[0].lines[0].points.ends_with(",80.0"));
        assert!(c[1].empty);
    }
}
