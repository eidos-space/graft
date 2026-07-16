use serde::{Deserialize, Serialize};

pub const REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetParameters {
    pub profile: String,
    pub sqlite_rows: u32,
    pub updated_rows: u32,
    pub row_payload_bytes: usize,
    pub text_file_count: u32,
    pub text_file_bytes: usize,
    pub binary_file_count: u32,
    pub binary_file_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricGroup {
    Speed,
    Storage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Milliseconds,
    Bytes,
    Ratio,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Independent,
    PairedBaseline,
    PairedCandidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairOrder {
    BaselineFirst,
    CandidateFirst,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkProvenance {
    pub run_id: String,
    pub report_kind: ReportKind,
    pub harness_label: String,
    pub build_profile: String,
    pub os: String,
    pub arch: String,
    pub runner_image: Option<String>,
    pub pair_order: Vec<PairOrder>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub display_name: String,
    pub group: MetricGroup,
    pub unit: MetricUnit,
    pub lower_is_better: bool,
    pub median: f64,
    pub median_absolute_deviation: f64,
    pub samples: Vec<f64>,
}

impl Metric {
    pub fn relative_deviation_percent(&self) -> f64 {
        if self.median == 0.0 {
            return 0.0;
        }
        self.median_absolute_deviation / self.median * 100.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub label: String,
    pub graft_version: String,
    pub provenance: BenchmarkProvenance,
    pub parameters: DatasetParameters,
    pub sample_count: usize,
    pub warmup_count: usize,
    pub metrics: Vec<Metric>,
}

pub fn median(values: &[f64]) -> f64 {
    assert!(!values.is_empty(), "median requires at least one value");
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

pub fn median_absolute_deviation(values: &[f64], values_median: f64) -> f64 {
    let deviations = values
        .iter()
        .map(|value| (value - values_median).abs())
        .collect::<Vec<_>>();
    median(&deviations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_handles_odd_and_even_samples() {
        assert_eq!(median(&[9.0, 1.0, 5.0]), 5.0);
        assert_eq!(median(&[8.0, 2.0, 6.0, 4.0]), 5.0);
    }

    #[test]
    fn median_deviation_is_robust_to_outlier() {
        let values = [10.0, 11.0, 12.0, 1000.0];
        let values_median = median(&values);
        assert_eq!(values_median, 11.5);
        assert_eq!(median_absolute_deviation(&values, values_median), 1.0);
    }
}
