use std::{collections::BTreeMap, fmt::Write};

use anyhow::{Context, Result, bail};

use crate::model::{
    BenchmarkReport, Metric, MetricGroup, MetricUnit, PairOrder, REPORT_SCHEMA_VERSION, ReportKind,
    median, median_absolute_deviation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComparisonMode {
    Paired,
    Unpaired,
}

struct ChangeSummary {
    change: String,
    noise: String,
}

pub fn markdown(baseline: &BenchmarkReport, candidate: &BenchmarkReport) -> Result<String> {
    let comparison_mode = validate_reports(baseline, candidate)?;
    let candidate_metrics = candidate
        .metrics
        .iter()
        .map(|metric| (metric.name.as_str(), metric))
        .collect::<BTreeMap<_, _>>();

    let mut output = String::new();
    writeln!(output, "<!-- graft-benchmark-report -->")?;
    writeln!(output, "## Graft performance")?;
    writeln!(output)?;
    match comparison_mode {
        ComparisonMode::Paired => writeln!(
            output,
            "Fixed `{}` dataset, {} aligned base/candidate pairs after {} warmup pair(s). Negative paired change is better for every metric.",
            candidate.parameters.profile, candidate.sample_count, candidate.warmup_count
        )?,
        ComparisonMode::Unpaired => writeln!(
            output,
            "⚠️ **Unpaired comparison.** These reports came from separate runs; host drift can dominate small changes. Fixed `{}` dataset with {} samples after {} warmup sample(s).",
            candidate.parameters.profile, candidate.sample_count, candidate.warmup_count
        )?,
    }
    write_metric_group(
        &mut output,
        "Speed",
        MetricGroup::Speed,
        baseline,
        &candidate_metrics,
        comparison_mode,
    )?;
    write_metric_group(
        &mut output,
        "Storage",
        MetricGroup::Storage,
        baseline,
        &candidate_metrics,
        comparison_mode,
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "Baseline: `{}` (`{}`) · Candidate: `{}` (`{}`). {} Storage uses apparent file bytes.",
        baseline.label,
        baseline.graft_version,
        candidate.label,
        candidate.graft_version,
        match comparison_mode {
            ComparisonMode::Paired =>
                "Change is the median of aligned per-pair percentages; noise is their median absolute deviation.",
            ComparisonMode::Unpaired =>
                "Change is the ratio of marginal medians; noise shows each report's relative median absolute deviation.",
        }
    )?;
    Ok(output)
}

fn validate_reports(
    baseline: &BenchmarkReport,
    candidate: &BenchmarkReport,
) -> Result<ComparisonMode> {
    if baseline.schema_version != REPORT_SCHEMA_VERSION
        || candidate.schema_version != REPORT_SCHEMA_VERSION
    {
        bail!(
            "unsupported report schema: baseline={}, candidate={}, expected={}",
            baseline.schema_version,
            candidate.schema_version,
            REPORT_SCHEMA_VERSION
        );
    }
    if baseline.parameters != candidate.parameters {
        bail!("benchmark dataset parameters do not match");
    }
    if baseline.sample_count != candidate.sample_count
        || baseline.warmup_count != candidate.warmup_count
    {
        bail!(
            "benchmark sampling does not match: baseline={}/{} samples/warmups, candidate={}/{} samples/warmups",
            baseline.sample_count,
            baseline.warmup_count,
            candidate.sample_count,
            candidate.warmup_count
        );
    }
    if baseline.metrics.len() != candidate.metrics.len() {
        bail!("benchmark metric counts do not match");
    }
    for report in [baseline, candidate] {
        for metric in &report.metrics {
            if metric.samples.len() != report.sample_count {
                bail!(
                    "metric {} has {} samples but report declares {}",
                    metric.name,
                    metric.samples.len(),
                    report.sample_count
                );
            }
            if metric.samples.iter().any(|sample| !sample.is_finite()) {
                bail!("metric {} contains a non-finite sample", metric.name);
            }
        }
    }

    match (
        baseline.provenance.report_kind,
        candidate.provenance.report_kind,
    ) {
        (ReportKind::Independent, ReportKind::Independent) => Ok(ComparisonMode::Unpaired),
        (ReportKind::PairedBaseline, ReportKind::PairedCandidate) => {
            validate_paired_provenance(baseline, candidate)?;
            Ok(ComparisonMode::Paired)
        }
        _ => bail!("reports do not form an independent or baseline/candidate paired comparison"),
    }
}

fn validate_paired_provenance(
    baseline: &BenchmarkReport,
    candidate: &BenchmarkReport,
) -> Result<()> {
    let baseline_provenance = &baseline.provenance;
    let candidate_provenance = &candidate.provenance;
    if baseline_provenance.run_id.is_empty()
        || baseline_provenance.run_id != candidate_provenance.run_id
        || baseline_provenance.harness_label != candidate_provenance.harness_label
        || baseline_provenance.build_profile != candidate_provenance.build_profile
        || baseline_provenance.os != candidate_provenance.os
        || baseline_provenance.arch != candidate_provenance.arch
        || baseline_provenance.runner_image != candidate_provenance.runner_image
    {
        bail!("paired reports have mismatched run, harness, build, or host provenance");
    }
    let order = &baseline_provenance.pair_order;
    if order != &candidate_provenance.pair_order
        || order.len() != baseline.sample_count
        || !order.len().is_multiple_of(2)
    {
        bail!("paired reports have invalid or mismatched sample order");
    }
    let baseline_first = order
        .iter()
        .filter(|order| **order == PairOrder::BaselineFirst)
        .count();
    let candidate_first = order.len() - baseline_first;
    if baseline_first != candidate_first {
        bail!("paired sample order is not balanced");
    }
    Ok(())
}

fn write_metric_group(
    output: &mut String,
    heading: &str,
    group: MetricGroup,
    baseline: &BenchmarkReport,
    candidate_metrics: &BTreeMap<&str, &Metric>,
    comparison_mode: ComparisonMode,
) -> Result<()> {
    writeln!(output)?;
    writeln!(output, "### {heading}")?;
    writeln!(output)?;
    let (change_heading, noise_heading) = match comparison_mode {
        ComparisonMode::Paired => ("Paired change", "Paired MAD"),
        ComparisonMode::Unpaired => ("Change", "Noise (base / candidate)"),
    };
    writeln!(
        output,
        "| Metric | Baseline | Candidate | {change_heading} | {noise_heading} |"
    )?;
    writeln!(output, "|---|---:|---:|---:|---:|")?;
    for baseline_metric in baseline
        .metrics
        .iter()
        .filter(|metric| metric.group == group)
    {
        let candidate_metric = candidate_metrics
            .get(baseline_metric.name.as_str())
            .with_context(|| format!("candidate is missing metric {}", baseline_metric.name))?;
        if baseline_metric.unit != candidate_metric.unit
            || baseline_metric.lower_is_better != candidate_metric.lower_is_better
        {
            bail!("metric definition changed for {}", baseline_metric.name);
        }
        let summary = summarize_change(baseline_metric, candidate_metric, comparison_mode);
        writeln!(
            output,
            "| {} | {} | {} | {} | {} |",
            baseline_metric.display_name,
            format_value(baseline_metric.median, baseline_metric.unit),
            format_value(candidate_metric.median, candidate_metric.unit),
            summary.change,
            summary.noise,
        )?;
    }
    Ok(())
}

fn summarize_change(
    baseline: &Metric,
    candidate: &Metric,
    comparison_mode: ComparisonMode,
) -> ChangeSummary {
    match comparison_mode {
        ComparisonMode::Paired => summarize_paired_change(baseline, candidate),
        ComparisonMode::Unpaired => summarize_unpaired_change(baseline, candidate),
    }
}

fn summarize_paired_change(baseline: &Metric, candidate: &Metric) -> ChangeSummary {
    let mut changes = Vec::with_capacity(baseline.samples.len());
    for (baseline_sample, candidate_sample) in baseline.samples.iter().zip(&candidate.samples) {
        if *baseline_sample <= 0.0 {
            return ChangeSummary {
                change: "n/a".to_string(),
                noise: "n/a".to_string(),
            };
        }
        changes.push((candidate_sample / baseline_sample - 1.0) * 100.0);
    }
    let raw_change = median(&changes);
    let paired_mad = median_absolute_deviation(&changes, raw_change);
    ChangeSummary {
        change: format_change_marker(raw_change, paired_mad, baseline.lower_is_better),
        noise: format!("{paired_mad:.1}%"),
    }
}

fn summarize_unpaired_change(baseline: &Metric, candidate: &Metric) -> ChangeSummary {
    let baseline_noise = baseline.relative_deviation_percent();
    let candidate_noise = candidate.relative_deviation_percent();
    let noise = format!("{baseline_noise:.1}% / {candidate_noise:.1}%");
    if baseline.median <= 0.0 {
        return ChangeSummary { change: "n/a".to_string(), noise };
    }
    let raw_change = (candidate.median / baseline.median - 1.0) * 100.0;
    ChangeSummary {
        change: format_change_marker(
            raw_change,
            baseline_noise + candidate_noise,
            baseline.lower_is_better,
        ),
        noise,
    }
}

fn format_change_marker(raw_change: f64, noise: f64, lower_is_better: bool) -> String {
    let directional_change = if lower_is_better {
        raw_change
    } else {
        -raw_change
    };
    let threshold = 5.0_f64.max(2.0 * noise);
    let marker = if directional_change <= -threshold {
        "🟢"
    } else if directional_change >= threshold {
        "🔴"
    } else {
        "⚪"
    };
    let displayed_change = if raw_change.abs() < 0.05 {
        0.0
    } else {
        raw_change
    };
    format!("{marker} {displayed_change:+.1}%")
}

fn format_value(value: f64, unit: MetricUnit) -> String {
    match unit {
        MetricUnit::Milliseconds if value >= 1000.0 => format!("{:.2} s", value / 1000.0),
        MetricUnit::Milliseconds => format!("{value:.2} ms"),
        MetricUnit::Bytes => format_bytes(value),
        MetricUnit::Ratio => format!("{value:.3}×"),
        MetricUnit::Count => format!("{value:.0}"),
    }
}

fn format_bytes(value: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let magnitude = value.abs();
    if magnitude >= GIB {
        format!("{sign}{:.2} GiB", magnitude / GIB)
    } else if magnitude >= MIB {
        format!("{sign}{:.2} MiB", magnitude / MIB)
    } else if magnitude >= KIB {
        format!("{sign}{:.2} KiB", magnitude / KIB)
    } else {
        format!("{value:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BenchmarkProvenance, DatasetParameters};

    fn report(label: &str, kind: ReportKind, run_id: &str, samples: Vec<f64>) -> BenchmarkReport {
        let samples_median = median(&samples);
        let pair_order = match kind {
            ReportKind::Independent => Vec::new(),
            ReportKind::PairedBaseline | ReportKind::PairedCandidate => (0..samples.len())
                .map(|index| {
                    if index.is_multiple_of(2) {
                        PairOrder::BaselineFirst
                    } else {
                        PairOrder::CandidateFirst
                    }
                })
                .collect(),
        };
        BenchmarkReport {
            schema_version: REPORT_SCHEMA_VERSION,
            label: label.to_string(),
            graft_version: "graft 0.0.0".to_string(),
            provenance: BenchmarkProvenance {
                run_id: run_id.to_string(),
                report_kind: kind,
                harness_label: "harness".to_string(),
                build_profile: "release".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                runner_image: Some("test".to_string()),
                pair_order,
            },
            parameters: DatasetParameters {
                profile: "test".to_string(),
                sqlite_rows: 1,
                updated_rows: 1,
                row_payload_bytes: 1,
                text_file_count: 1,
                text_file_bytes: 1,
                binary_file_count: 1,
                binary_file_bytes: 1,
            },
            sample_count: samples.len(),
            warmup_count: 1,
            metrics: vec![Metric {
                name: "speed.example".to_string(),
                display_name: "Example".to_string(),
                group: MetricGroup::Speed,
                unit: MetricUnit::Milliseconds,
                lower_is_better: true,
                median: samples_median,
                median_absolute_deviation: median_absolute_deviation(&samples, samples_median),
                samples,
            }],
        }
    }

    fn paired_reports(
        baseline_samples: Vec<f64>,
        candidate_samples: Vec<f64>,
    ) -> (BenchmarkReport, BenchmarkReport) {
        (
            report(
                "base",
                ReportKind::PairedBaseline,
                "paired-run",
                baseline_samples,
            ),
            report(
                "head",
                ReportKind::PairedCandidate,
                "paired-run",
                candidate_samples,
            ),
        )
    }

    #[test]
    fn markdown_marks_a_lower_duration_as_improvement() {
        let (baseline, candidate) = paired_reports(
            vec![100.0, 200.0, 300.0, 400.0],
            vec![90.0, 180.0, 270.0, 360.0],
        );
        let output = markdown(&baseline, &candidate).unwrap();
        assert!(output.contains("🟢 -10.0%"));
        assert!(output.contains("Example"));
    }

    #[test]
    fn markdown_uses_aligned_pair_changes_instead_of_marginal_medians() {
        let (baseline, candidate) =
            paired_reports(vec![1.0, 1.0, 2.0, 10.0], vec![1.0, 5.0, 1.0, 5.0]);
        let output = markdown(&baseline, &candidate).unwrap();
        assert!(output.contains("-25.0%"));
        assert!(!output.contains("+100.0%"));
    }

    #[test]
    fn markdown_rejects_different_sampling_parameters() {
        let (baseline, mut candidate) = paired_reports(
            vec![100.0, 100.0, 100.0, 100.0],
            vec![90.0, 90.0, 90.0, 90.0],
        );
        candidate.sample_count += 1;
        assert!(markdown(&baseline, &candidate).is_err());
    }

    #[test]
    fn markdown_does_not_color_changes_smaller_than_noise() {
        let (baseline, candidate) = paired_reports(
            vec![100.0, 100.0, 100.0, 100.0],
            vec![90.0, 130.0, 90.0, 130.0],
        );
        let output = markdown(&baseline, &candidate).unwrap();
        assert!(output.contains("⚪ +10.0%"));
    }

    #[test]
    fn markdown_rejects_mismatched_paired_run_ids() {
        let (baseline, mut candidate) = paired_reports(
            vec![100.0, 100.0, 100.0, 100.0],
            vec![90.0, 90.0, 90.0, 90.0],
        );
        candidate.provenance.run_id = "different-run".to_string();
        assert!(markdown(&baseline, &candidate).is_err());
    }

    #[test]
    fn markdown_rejects_unbalanced_pair_order() {
        let (mut baseline, mut candidate) = paired_reports(
            vec![100.0, 100.0, 100.0, 100.0],
            vec![90.0, 90.0, 90.0, 90.0],
        );
        baseline.provenance.pair_order[1] = PairOrder::BaselineFirst;
        candidate.provenance.pair_order[1] = PairOrder::BaselineFirst;
        assert!(markdown(&baseline, &candidate).is_err());
    }

    #[test]
    fn markdown_clearly_labels_independent_reports() {
        let baseline = report(
            "base",
            ReportKind::Independent,
            "base-run",
            vec![100.0, 101.0, 99.0],
        );
        let candidate = report(
            "head",
            ReportKind::Independent,
            "head-run",
            vec![90.0, 91.0, 89.0],
        );
        let output = markdown(&baseline, &candidate).unwrap();
        assert!(output.contains("Unpaired comparison"));
    }

    #[test]
    fn format_bytes_preserves_negative_growth() {
        assert_eq!(format_bytes(-2048.0), "-2.00 KiB");
    }
}
