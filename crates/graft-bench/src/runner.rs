use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

use crate::model::{
    BenchmarkProvenance, BenchmarkReport, DatasetParameters, Metric, MetricGroup, MetricUnit,
    PairOrder, REPORT_SCHEMA_VERSION, ReportKind, median, median_absolute_deviation,
};

const SQLITE_FILE: &str = "app.db";
const GRAFT_DIR: &str = ".graft";
type Sample = Vec<(&'static str, f64)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    Ci,
    Smoke,
}

impl Profile {
    fn parameters(self) -> DatasetParameters {
        match self {
            Self::Ci => DatasetParameters {
                profile: "ci".to_string(),
                sqlite_rows: 20_000,
                updated_rows: 2_000,
                row_payload_bytes: 256,
                text_file_count: 64,
                text_file_bytes: 4 * 1024,
                binary_file_count: 2,
                binary_file_bytes: 2 * 1024 * 1024,
            },
            Self::Smoke => DatasetParameters {
                profile: "smoke".to_string(),
                sqlite_rows: 500,
                updated_rows: 50,
                row_payload_bytes: 128,
                text_file_count: 4,
                text_file_bytes: 512,
                binary_file_count: 1,
                binary_file_bytes: 64 * 1024,
            },
        }
    }
}

#[derive(Debug)]
pub struct RunConfig {
    pub graft_bin: PathBuf,
    pub label: String,
    pub profile: Profile,
    pub samples: usize,
    pub warmups: usize,
}

#[derive(Debug)]
pub struct PairedRunConfig {
    pub baseline_graft_bin: PathBuf,
    pub candidate_graft_bin: PathBuf,
    pub baseline_label: String,
    pub candidate_label: String,
    pub profile: Profile,
    pub samples: usize,
    pub warmups: usize,
}

#[derive(Debug, Clone, Copy)]
struct MetricDefinition {
    name: &'static str,
    display_name: &'static str,
    group: MetricGroup,
    unit: MetricUnit,
}

#[derive(Debug)]
struct SpeedMeasurements {
    repo_init: f64,
    stage_initial: f64,
    commit_initial: f64,
    stage_incremental: f64,
    commit_incremental: f64,
    row_diff: f64,
    checkout_parent: f64,
    push_fs_remote: f64,
}

#[derive(Debug)]
struct LocalStorageMeasurements {
    worktree_bytes: u64,
    sqlite_bytes: u64,
    graft_initial_bytes: u64,
    graft_incremental_bytes: u64,
    incremental_growth_bytes: f64,
    initial_amplification: f64,
    incremental_amplification: f64,
    fjall_incremental_bytes: u64,
    objects_incremental_bytes: u64,
    payloads_incremental_bytes: u64,
    metadata_incremental_bytes: u64,
    graft_file_count: u64,
    objects_file_count: u64,
}

#[derive(Debug)]
struct RemoteStorageMeasurements {
    bytes: u64,
    segments_bytes: u64,
    commits_bytes: u64,
    objects_bytes: u64,
    payloads_bytes: u64,
    metadata_bytes: u64,
    file_count: u64,
}

#[derive(Debug)]
struct SampleMeasurements {
    speed: SpeedMeasurements,
    local_storage: LocalStorageMeasurements,
    remote_storage: RemoteStorageMeasurements,
}

#[derive(Debug)]
struct CommandMeasurement {
    elapsed_ms: f64,
    stdout: Vec<u8>,
}

impl SampleMeasurements {
    fn into_metrics(self) -> Vec<(&'static str, f64)> {
        let mut metrics = self.speed_metrics();
        metrics.extend(self.local_storage_metrics());
        metrics.extend(self.remote_storage_metrics());
        metrics
    }

    fn speed_metrics(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("speed.repo_init", self.speed.repo_init),
            ("speed.stage_initial", self.speed.stage_initial),
            ("speed.commit_initial", self.speed.commit_initial),
            ("speed.stage_incremental", self.speed.stage_incremental),
            ("speed.commit_incremental", self.speed.commit_incremental),
            ("speed.row_diff", self.speed.row_diff),
            ("speed.checkout_parent", self.speed.checkout_parent),
            ("speed.push_fs_remote", self.speed.push_fs_remote),
        ]
    }

    fn local_storage_metrics(&self) -> Vec<(&'static str, f64)> {
        let storage = &self.local_storage;
        vec![
            ("storage.worktree_bytes", storage.worktree_bytes as f64),
            ("storage.sqlite_bytes", storage.sqlite_bytes as f64),
            (
                "storage.graft_initial_bytes",
                storage.graft_initial_bytes as f64,
            ),
            (
                "storage.graft_incremental_bytes",
                storage.graft_incremental_bytes as f64,
            ),
            (
                "storage.incremental_growth_bytes",
                storage.incremental_growth_bytes,
            ),
            (
                "storage.initial_amplification",
                storage.initial_amplification,
            ),
            (
                "storage.incremental_amplification",
                storage.incremental_amplification,
            ),
            (
                "storage.fjall_incremental_bytes",
                storage.fjall_incremental_bytes as f64,
            ),
            (
                "storage.objects_incremental_bytes",
                storage.objects_incremental_bytes as f64,
            ),
            (
                "storage.payloads_incremental_bytes",
                storage.payloads_incremental_bytes as f64,
            ),
            (
                "storage.metadata_incremental_bytes",
                storage.metadata_incremental_bytes as f64,
            ),
            ("storage.graft_file_count", storage.graft_file_count as f64),
            (
                "storage.objects_file_count",
                storage.objects_file_count as f64,
            ),
        ]
    }

    fn remote_storage_metrics(&self) -> Vec<(&'static str, f64)> {
        let storage = &self.remote_storage;
        vec![
            ("storage.remote_bytes", storage.bytes as f64),
            (
                "storage.remote_segments_bytes",
                storage.segments_bytes as f64,
            ),
            ("storage.remote_commits_bytes", storage.commits_bytes as f64),
            ("storage.remote_objects_bytes", storage.objects_bytes as f64),
            (
                "storage.remote_payloads_bytes",
                storage.payloads_bytes as f64,
            ),
            (
                "storage.remote_metadata_bytes",
                storage.metadata_bytes as f64,
            ),
            ("storage.remote_file_count", storage.file_count as f64),
        ]
    }
}

const METRIC_DEFINITIONS: &[MetricDefinition] = &[
    MetricDefinition {
        name: "speed.repo_init",
        display_name: "Repository init",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.stage_initial",
        display_name: "Stage initial dataset",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.commit_initial",
        display_name: "Commit initial dataset",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.stage_incremental",
        display_name: "Stage 10% row update",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.commit_incremental",
        display_name: "Commit incremental update",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.row_diff",
        display_name: "Row diff between commits",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.checkout_parent",
        display_name: "Checkout parent revision",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "speed.push_fs_remote",
        display_name: "Push to filesystem remote",
        group: MetricGroup::Speed,
        unit: MetricUnit::Milliseconds,
    },
    MetricDefinition {
        name: "storage.worktree_bytes",
        display_name: "Worktree dataset",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.sqlite_bytes",
        display_name: "Materialized SQLite database",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.graft_initial_bytes",
        display_name: ".graft after initial commit",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.graft_incremental_bytes",
        display_name: ".graft after incremental commit",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.incremental_growth_bytes",
        display_name: "Incremental history growth",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.initial_amplification",
        display_name: "Initial storage amplification",
        group: MetricGroup::Storage,
        unit: MetricUnit::Ratio,
    },
    MetricDefinition {
        name: "storage.incremental_amplification",
        display_name: "Two-commit storage amplification",
        group: MetricGroup::Storage,
        unit: MetricUnit::Ratio,
    },
    MetricDefinition {
        name: "storage.fjall_incremental_bytes",
        display_name: "SQLite snapshot store",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.objects_incremental_bytes",
        display_name: "Repository objects",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.payloads_incremental_bytes",
        display_name: "External file payloads",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.metadata_incremental_bytes",
        display_name: "Refs, index, and metadata",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.graft_file_count",
        display_name: ".graft file count",
        group: MetricGroup::Storage,
        unit: MetricUnit::Count,
    },
    MetricDefinition {
        name: "storage.objects_file_count",
        display_name: "Repository object file count",
        group: MetricGroup::Storage,
        unit: MetricUnit::Count,
    },
    MetricDefinition {
        name: "storage.remote_bytes",
        display_name: "Filesystem remote after push",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.remote_segments_bytes",
        display_name: "Remote segments",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.remote_commits_bytes",
        display_name: "Remote storage commits",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.remote_objects_bytes",
        display_name: "Remote repository objects",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.remote_payloads_bytes",
        display_name: "Remote external payloads",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.remote_metadata_bytes",
        display_name: "Remote refs and metadata",
        group: MetricGroup::Storage,
        unit: MetricUnit::Bytes,
    },
    MetricDefinition {
        name: "storage.remote_file_count",
        display_name: "Remote file count",
        group: MetricGroup::Storage,
        unit: MetricUnit::Count,
    },
];

pub fn run(config: &RunConfig) -> Result<BenchmarkReport> {
    validate_sample_count(config.samples)?;
    let (graft_bin, graft_version) = prepare_graft_binary(&config.graft_bin)?;
    let parameters = config.profile.parameters();
    let provenance = benchmark_provenance(
        ReportKind::Independent,
        &config.label,
        Vec::new(),
        new_run_id()?,
    );
    for warmup in 0..config.warmups {
        eprintln!("warmup {}/{}", warmup + 1, config.warmups);
        run_sample(&graft_bin, &parameters)?;
    }

    let mut samples_by_metric = BTreeMap::<&'static str, Vec<f64>>::new();
    for sample in 0..config.samples {
        eprintln!("sample {}/{}", sample + 1, config.samples);
        record_sample(&mut samples_by_metric, run_sample(&graft_bin, &parameters)?);
    }
    build_report(
        &config.label,
        graft_version,
        parameters,
        config.samples,
        config.warmups,
        provenance,
        &samples_by_metric,
    )
}

pub fn run_paired(config: &PairedRunConfig) -> Result<(BenchmarkReport, BenchmarkReport)> {
    validate_paired_sample_count(config.samples)?;
    let (baseline_bin, baseline_version) = prepare_graft_binary(&config.baseline_graft_bin)?;
    let (candidate_bin, candidate_version) = prepare_graft_binary(&config.candidate_graft_bin)?;
    let parameters = config.profile.parameters();
    let pair_order = (0..config.samples)
        .map(|sample| {
            if sample.is_multiple_of(2) {
                PairOrder::BaselineFirst
            } else {
                PairOrder::CandidateFirst
            }
        })
        .collect::<Vec<_>>();
    let run_id = new_run_id()?;
    run_paired_warmups(&baseline_bin, &candidate_bin, &parameters, config.warmups)?;

    let mut baseline_samples = BTreeMap::<&'static str, Vec<f64>>::new();
    let mut candidate_samples = BTreeMap::<&'static str, Vec<f64>>::new();
    for sample in 0..config.samples {
        eprintln!("paired sample {}/{}", sample + 1, config.samples);
        let (baseline, candidate) = run_sample_pair(
            &baseline_bin,
            &candidate_bin,
            &parameters,
            sample.is_multiple_of(2),
        )?;
        record_sample(&mut baseline_samples, baseline);
        record_sample(&mut candidate_samples, candidate);
    }

    let baseline = build_report(
        &config.baseline_label,
        baseline_version,
        parameters.clone(),
        config.samples,
        config.warmups,
        benchmark_provenance(
            ReportKind::PairedBaseline,
            &config.candidate_label,
            pair_order.clone(),
            run_id.clone(),
        ),
        &baseline_samples,
    )?;
    let candidate = build_report(
        &config.candidate_label,
        candidate_version,
        parameters,
        config.samples,
        config.warmups,
        benchmark_provenance(
            ReportKind::PairedCandidate,
            &config.candidate_label,
            pair_order,
            run_id,
        ),
        &candidate_samples,
    )?;
    Ok((baseline, candidate))
}

fn run_sample_pair(
    baseline_bin: &Path,
    candidate_bin: &Path,
    parameters: &DatasetParameters,
    baseline_first: bool,
) -> Result<(Sample, Sample)> {
    if baseline_first {
        let baseline = run_sample(baseline_bin, parameters)?;
        let candidate = run_sample(candidate_bin, parameters)?;
        Ok((baseline, candidate))
    } else {
        let candidate = run_sample(candidate_bin, parameters)?;
        let baseline = run_sample(baseline_bin, parameters)?;
        Ok((baseline, candidate))
    }
}

fn run_paired_warmups(
    baseline_bin: &Path,
    candidate_bin: &Path,
    parameters: &DatasetParameters,
    warmups: usize,
) -> Result<()> {
    for warmup in 0..warmups {
        eprintln!("paired warmup {}/{}", warmup + 1, warmups);
        run_sample_pair(
            baseline_bin,
            candidate_bin,
            parameters,
            warmup.is_multiple_of(2),
        )?;
    }
    Ok(())
}

fn validate_sample_count(samples: usize) -> Result<()> {
    if samples == 0 {
        bail!("samples must be greater than zero");
    }
    Ok(())
}

fn validate_paired_sample_count(samples: usize) -> Result<()> {
    validate_sample_count(samples)?;
    if !samples.is_multiple_of(2) {
        bail!("paired samples must be even so base/candidate execution order is balanced");
    }
    Ok(())
}

fn new_run_id() -> Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(format!("{timestamp:x}-{:x}", std::process::id()))
}

fn benchmark_provenance(
    report_kind: ReportKind,
    harness_label: &str,
    pair_order: Vec<PairOrder>,
    run_id: String,
) -> BenchmarkProvenance {
    let runner_image = [
        std::env::var("ImageOS").ok(),
        std::env::var("ImageVersion").ok(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    BenchmarkProvenance {
        run_id,
        report_kind,
        harness_label: harness_label.to_string(),
        build_profile: if cfg!(debug_assertions) {
            "debug".to_string()
        } else {
            "release".to_string()
        },
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        runner_image: (!runner_image.is_empty()).then(|| runner_image.join("/")),
        pair_order,
    }
}

fn prepare_graft_binary(path: &Path) -> Result<(PathBuf, String)> {
    if !path.is_file() {
        bail!("graft binary does not exist: {}", path.display());
    }
    let binary = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve graft binary {}", path.display()))?;
    let version = graft_version(&binary)?;
    Ok((binary, version))
}

fn record_sample(samples_by_metric: &mut BTreeMap<&'static str, Vec<f64>>, sample: Sample) {
    for (name, value) in sample {
        samples_by_metric.entry(name).or_default().push(value);
    }
}

fn build_report(
    label: &str,
    graft_version: String,
    parameters: DatasetParameters,
    sample_count: usize,
    warmup_count: usize,
    provenance: BenchmarkProvenance,
    samples_by_metric: &BTreeMap<&'static str, Vec<f64>>,
) -> Result<BenchmarkReport> {
    let metrics = METRIC_DEFINITIONS
        .iter()
        .map(|definition| build_metric(*definition, samples_by_metric))
        .collect::<Result<Vec<_>>>()?;
    Ok(BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        label: label.to_string(),
        graft_version,
        provenance,
        parameters,
        sample_count,
        warmup_count,
        metrics,
    })
}

fn graft_version(graft_bin: &Path) -> Result<String> {
    let output = Command::new(graft_bin)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to execute {}", graft_bin.display()))?;
    if !output.status.success() {
        bail!("{} --version failed", graft_bin.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn build_metric(
    definition: MetricDefinition,
    samples_by_metric: &BTreeMap<&'static str, Vec<f64>>,
) -> Result<Metric> {
    let samples = samples_by_metric
        .get(definition.name)
        .with_context(|| format!("missing samples for {}", definition.name))?
        .clone();
    let samples_median = median(&samples);
    Ok(Metric {
        name: definition.name.to_string(),
        display_name: definition.display_name.to_string(),
        group: definition.group,
        unit: definition.unit,
        lower_is_better: true,
        median: samples_median,
        median_absolute_deviation: median_absolute_deviation(&samples, samples_median),
        samples,
    })
}

fn run_sample(graft_bin: &Path, parameters: &DatasetParameters) -> Result<Sample> {
    let temp = tempfile::Builder::new()
        .prefix("graft-benchmark-")
        .tempdir()?;
    let worktree = temp.path().join("worktree");
    let checkout_worktree = temp.path().join("checkout-worktree");
    let remote = temp.path().join("remote");
    fs::create_dir(&worktree)?;
    fs::create_dir(&remote)?;
    create_dataset(&worktree, parameters)?;

    let repo_init = timed_graft(graft_bin, &worktree, &["init", "--json"])?;
    let stage_initial = timed_graft(graft_bin, &worktree, &["add", "--all", "--json"])?;
    let commit_initial = timed_graft(
        graft_bin,
        &worktree,
        &["commit", "--message", "benchmark initial", "--json"],
    )?;

    let graft_dir = worktree.join(GRAFT_DIR);
    let initial_graft_bytes = required_directory_size(&graft_dir)?;
    let initial_worktree_bytes = directory_size(&worktree)?
        .checked_sub(initial_graft_bytes)
        .context("initial worktree size was smaller than .graft")?;

    mutate_dataset(&worktree, parameters)?;
    let stage_incremental = timed_graft(graft_bin, &worktree, &["add", "--all", "--json"])?;
    let commit_incremental = timed_graft(
        graft_bin,
        &worktree,
        &["commit", "--message", "benchmark incremental", "--json"],
    )?;

    let local_storage = measure_local_storage(
        &worktree,
        &graft_dir,
        initial_graft_bytes,
        initial_worktree_bytes,
    )?;
    let row_diff = measured_graft(
        graft_bin,
        &worktree,
        &["diff", "--rows", "--json", "HEAD~1", "HEAD", SQLITE_FILE],
    )?;
    let (push_fs_remote, remote_storage) =
        push_to_filesystem_remote(graft_bin, &worktree, &remote)?;
    prepare_checkout_fixture(graft_bin, &checkout_worktree, parameters)?;
    let checkout_parent = timed_graft(
        graft_bin,
        &checkout_worktree,
        &["checkout", "--force", "--json", "HEAD~1"],
    )?;

    validate_row_diff(&row_diff.stdout, parameters)?;
    validate_worktree_state(&worktree, parameters, parameters.updated_rows)?;
    validate_worktree_state(&checkout_worktree, parameters, 0)?;
    validate_remote_storage(&remote, &remote_storage)?;
    validate_remote_clone(graft_bin, &remote, parameters)?;

    Ok(SampleMeasurements {
        speed: SpeedMeasurements {
            repo_init,
            stage_initial,
            commit_initial,
            stage_incremental,
            commit_incremental,
            row_diff: row_diff.elapsed_ms,
            checkout_parent,
            push_fs_remote,
        },
        local_storage,
        remote_storage,
    }
    .into_metrics())
}

fn prepare_checkout_fixture(
    graft_bin: &Path,
    worktree: &Path,
    parameters: &DatasetParameters,
) -> Result<()> {
    fs::create_dir(worktree)?;
    create_dataset(worktree, parameters)?;
    timed_graft(graft_bin, worktree, &["init", "--json"])?;
    timed_graft(graft_bin, worktree, &["add", "--all", "--json"])?;
    timed_graft(
        graft_bin,
        worktree,
        &["commit", "--message", "benchmark initial", "--json"],
    )?;
    mutate_dataset(worktree, parameters)?;
    timed_graft(graft_bin, worktree, &["add", "--all", "--json"])?;
    timed_graft(
        graft_bin,
        worktree,
        &["commit", "--message", "benchmark incremental", "--json"],
    )?;
    Ok(())
}

fn measure_local_storage(
    worktree: &Path,
    graft_dir: &Path,
    initial_graft_bytes: u64,
    initial_worktree_bytes: u64,
) -> Result<LocalStorageMeasurements> {
    let graft_incremental_bytes = required_directory_size(graft_dir)?;
    let incremental_worktree_bytes = directory_size(worktree)?
        .checked_sub(graft_incremental_bytes)
        .context("incremental worktree size was smaller than .graft")?;
    let fjall_incremental_bytes = required_nonempty_directory_size(&graft_dir.join("store/fjall"))?;
    let objects_incremental_bytes = required_nonempty_directory_size(&graft_dir.join("objects"))?;
    let payloads_incremental_bytes =
        required_nonempty_directory_size(&graft_dir.join("store/files"))?;
    let categorized_bytes = fjall_incremental_bytes
        .checked_add(objects_incremental_bytes)
        .and_then(|bytes| bytes.checked_add(payloads_incremental_bytes))
        .context("local storage category sizes overflowed u64")?;
    let metadata_incremental_bytes = graft_incremental_bytes
        .checked_sub(categorized_bytes)
        .context("local storage categories exceeded total .graft size")?;

    Ok(LocalStorageMeasurements {
        worktree_bytes: initial_worktree_bytes.max(incremental_worktree_bytes),
        sqlite_bytes: fs::metadata(worktree.join(SQLITE_FILE))?.len(),
        graft_initial_bytes: initial_graft_bytes,
        graft_incremental_bytes,
        incremental_growth_bytes: graft_incremental_bytes as f64 - initial_graft_bytes as f64,
        initial_amplification: ratio(initial_graft_bytes, initial_worktree_bytes),
        incremental_amplification: ratio(graft_incremental_bytes, incremental_worktree_bytes),
        fjall_incremental_bytes,
        objects_incremental_bytes,
        payloads_incremental_bytes,
        metadata_incremental_bytes,
        graft_file_count: directory_file_count(graft_dir)?,
        objects_file_count: directory_file_count(&graft_dir.join("objects"))?,
    })
}

fn push_to_filesystem_remote(
    graft_bin: &Path,
    worktree: &Path,
    remote: &Path,
) -> Result<(f64, RemoteStorageMeasurements)> {
    let remote_uri = format!("fs://{}", remote.display());
    timed_graft(
        graft_bin,
        worktree,
        &["remote", "add", "--json", "origin", &remote_uri],
    )?;
    let elapsed = timed_graft(graft_bin, worktree, &["push", "--json", "origin", "main"])?;
    let bytes = required_directory_size(remote)?;
    let segments_bytes = required_nonempty_directory_size(&remote.join("segments"))?;
    let commits_bytes = required_nonempty_directory_size(&remote.join("logs"))?;
    let objects_bytes = required_nonempty_directory_size(&remote.join("objects"))?;
    let payloads_bytes = required_nonempty_directory_size(&remote.join("store/files"))?;
    let categorized_bytes = segments_bytes
        .checked_add(commits_bytes)
        .and_then(|bytes| bytes.checked_add(objects_bytes))
        .and_then(|bytes| bytes.checked_add(payloads_bytes))
        .context("remote storage category sizes overflowed u64")?;
    let storage = RemoteStorageMeasurements {
        bytes,
        segments_bytes,
        commits_bytes,
        objects_bytes,
        payloads_bytes,
        metadata_bytes: bytes
            .checked_sub(categorized_bytes)
            .context("remote storage categories exceeded total remote size")?,
        file_count: directory_file_count(remote)?,
    };
    Ok((elapsed, storage))
}

fn timed_graft(graft_bin: &Path, worktree: &Path, args: &[&str]) -> Result<f64> {
    Ok(measured_graft(graft_bin, worktree, args)?.elapsed_ms)
}

fn measured_graft(graft_bin: &Path, worktree: &Path, args: &[&str]) -> Result<CommandMeasurement> {
    let started = Instant::now();
    let output = Command::new(graft_bin)
        .args(args)
        .current_dir(worktree)
        .env("NO_COLOR", "1")
        .output()
        .with_context(|| {
            format!(
                "failed to execute {} {}",
                graft_bin.display(),
                args.join(" ")
            )
        })?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "graft command failed: {} {}\nstdout:\n{}\nstderr:\n{}",
            graft_bin.display(),
            args.join(" "),
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(CommandMeasurement { elapsed_ms, stdout: output.stdout })
}

fn validate_worktree_state(
    worktree: &Path,
    parameters: &DatasetParameters,
    expected_updated_rows: u32,
) -> Result<()> {
    validate_sqlite_rows(
        &worktree.join(SQLITE_FILE),
        parameters,
        expected_updated_rows,
    )?;
    validate_artifacts(worktree, parameters, expected_updated_rows > 0)
}

fn validate_sqlite_rows(
    path: &Path,
    parameters: &DatasetParameters,
    expected_updated_rows: u32,
) -> Result<()> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement =
        connection.prepare("SELECT id, version, title, payload FROM records ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut row_count = 0_u32;
    while let Some(row) = rows.next()? {
        let id = row.get::<_, u32>(0)?;
        let version = row.get::<_, u32>(1)?;
        let title = row.get::<_, String>(2)?;
        let payload = row.get::<_, Vec<u8>>(3)?;
        validate_sqlite_row(
            parameters,
            expected_updated_rows,
            row_count,
            id,
            version,
            &title,
            &payload,
        )?;
        row_count += 1;
    }
    if row_count != parameters.sqlite_rows {
        bail!(
            "SQLite validation expected {} rows, got {row_count}",
            parameters.sqlite_rows
        );
    }
    drop(rows);
    drop(statement);
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("SQLite integrity check failed: {integrity}");
    }
    Ok(())
}

fn validate_sqlite_row(
    parameters: &DatasetParameters,
    expected_updated_rows: u32,
    expected_id: u32,
    id: u32,
    version: u32,
    title: &str,
    payload: &[u8],
) -> Result<()> {
    let stride = parameters.sqlite_rows / parameters.updated_rows;
    let updated = expected_updated_rows > 0
        && id.is_multiple_of(stride)
        && id / stride < expected_updated_rows;
    let expected_version = if updated { 2 } else { 1 };
    let seed = if updated {
        0x3c6e_f372_fe94_f82b
    } else {
        0xbb67_ae85_84ca_a73b
    } ^ u64::from(id);
    let expected_payload = deterministic_bytes(parameters.row_payload_bytes, seed);
    if id != expected_id
        || version != expected_version
        || title != format!("record-{id:08}")
        || payload != expected_payload
    {
        bail!("SQLite row {id} did not match the deterministic fixture");
    }
    Ok(())
}

fn validate_artifacts(
    worktree: &Path,
    parameters: &DatasetParameters,
    incremental: bool,
) -> Result<()> {
    for index in 0..parameters.text_file_count {
        let fixture_index = if incremental && index == 0 {
            u32::MAX
        } else {
            index
        };
        let expected = deterministic_text(fixture_index, parameters.text_file_bytes);
        let actual = fs::read(worktree.join(format!("documents/note-{index:04}.md")))?;
        if actual != expected {
            bail!("text artifact {index} did not match the deterministic fixture");
        }
    }
    for index in 0..parameters.binary_file_count {
        let expected = expected_binary_artifact(index, parameters, incremental);
        let actual = fs::read(worktree.join(format!("assets/asset-{index:04}.bin")))?;
        if actual != expected {
            bail!("binary artifact {index} did not match the deterministic fixture");
        }
    }
    Ok(())
}

fn expected_binary_artifact(
    index: u32,
    parameters: &DatasetParameters,
    incremental: bool,
) -> Vec<u8> {
    let mut expected = deterministic_bytes(
        parameters.binary_file_bytes,
        0x6a09_e667_f3bc_c909 ^ u64::from(index),
    );
    if incremental && index == 0 {
        let changed = deterministic_bytes(expected.len().min(64 * 1024), 0xa54f_f53a_5f1d_36f1);
        expected[..changed.len()].copy_from_slice(&changed);
    }
    expected
}

fn validate_row_diff(stdout: &[u8], parameters: &DatasetParameters) -> Result<()> {
    let output: Value = serde_json::from_slice(stdout).context("row diff did not return JSON")?;
    let files = output["files"]
        .as_array()
        .context("row diff JSON is missing files")?;
    if files.len() != 1 || files[0]["path"] != SQLITE_FILE || files[0]["row_diff_available"] != true
    {
        bail!("row diff validation expected exactly one available {SQLITE_FILE} result");
    }
    let tables = files[0]["tables"]
        .as_array()
        .context("row diff JSON is missing tables")?;
    if tables.len() != 1
        || tables[0]["name"] != "records"
        || tables[0]["columns"] != json!(["id", "version", "title", "payload"])
    {
        bail!("row diff validation expected exactly the records table and its four columns");
    }
    let changes = tables[0]["changes"]
        .as_array()
        .context("row diff JSON is missing record changes")?;
    if changes.len() != parameters.updated_rows as usize {
        bail!(
            "row diff validation failed: expected {} updates, got {} changes",
            parameters.updated_rows,
            changes.len()
        );
    }

    let stride = parameters.sqlite_rows / parameters.updated_rows;
    let mut seen = BTreeSet::new();
    for change in changes {
        let rowid = change["rowid"]
            .as_u64()
            .and_then(|rowid| u32::try_from(rowid).ok())
            .context("row diff update has an invalid rowid")?;
        if change["op"] != "update"
            || !rowid.is_multiple_of(stride)
            || rowid / stride >= parameters.updated_rows
            || !seen.insert(rowid)
        {
            bail!("row diff contains an unexpected or duplicate rowid {rowid}");
        }
        let old_payload = deterministic_bytes(
            parameters.row_payload_bytes,
            0xbb67_ae85_84ca_a73b ^ u64::from(rowid),
        );
        let new_payload = deterministic_bytes(
            parameters.row_payload_bytes,
            0x3c6e_f372_fe94_f82b ^ u64::from(rowid),
        );
        let title = format!("record-{rowid:08}");
        // SQLite stores an INTEGER PRIMARY KEY alias as NULL in the record body;
        // the logical id is carried separately in JsonRowChange::rowid.
        let expected_old = json!([null, 1, title, hex_bytes(&old_payload)]);
        let expected_new = json!([null, 2, title, hex_bytes(&new_payload)]);
        if change["old_values"] != expected_old || change["values"] != expected_new {
            bail!("row diff values for rowid {rowid} do not match the deterministic fixture");
        }
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn validate_remote_storage(remote: &Path, storage: &RemoteStorageMeasurements) -> Result<()> {
    let remote_head = fs::read_to_string(remote.join("refs/heads/main"))?;
    if remote_head.trim().is_empty()
        || storage.segments_bytes == 0
        || storage.commits_bytes == 0
        || storage.objects_bytes == 0
        || storage.payloads_bytes == 0
        || storage.metadata_bytes == 0
    {
        bail!("filesystem remote validation failed: pushed repository is incomplete");
    }
    Ok(())
}

fn validate_remote_clone(
    graft_bin: &Path,
    remote: &Path,
    parameters: &DatasetParameters,
) -> Result<()> {
    let parent = remote.parent().context("filesystem remote has no parent")?;
    let clone_worktree = parent.join("remote-validation");
    fs::create_dir(&clone_worktree)?;
    let remote_uri = format!("fs://{}", remote.display());
    timed_graft(
        graft_bin,
        &clone_worktree,
        &["clone", "--json", &remote_uri, "main"],
    )?;
    validate_worktree_state(&clone_worktree, parameters, parameters.updated_rows)?;
    let history_diff = measured_graft(
        graft_bin,
        &clone_worktree,
        &["diff", "--rows", "--json", "HEAD~1", "HEAD", SQLITE_FILE],
    )?;
    validate_row_diff(&history_diff.stdout, parameters)?;
    timed_graft(
        graft_bin,
        &clone_worktree,
        &["checkout", "--force", "--json", "HEAD~1"],
    )?;
    validate_worktree_state(&clone_worktree, parameters, 0)
}

fn create_dataset(worktree: &Path, parameters: &DatasetParameters) -> Result<()> {
    create_sqlite_database(&worktree.join(SQLITE_FILE), parameters)?;
    let documents = worktree.join("documents");
    fs::create_dir_all(&documents)?;
    for index in 0..parameters.text_file_count {
        let contents = deterministic_text(index, parameters.text_file_bytes);
        fs::write(documents.join(format!("note-{index:04}.md")), contents)?;
    }

    let assets = worktree.join("assets");
    fs::create_dir_all(&assets)?;
    for index in 0..parameters.binary_file_count {
        let bytes = deterministic_bytes(
            parameters.binary_file_bytes,
            0x6a09_e667_f3bc_c909 ^ u64::from(index),
        );
        fs::write(assets.join(format!("asset-{index:04}.bin")), bytes)?;
    }
    Ok(())
}

fn create_sqlite_database(path: &Path, parameters: &DatasetParameters) -> Result<()> {
    let mut connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA page_size = 4096;
         PRAGMA journal_mode = DELETE;
         PRAGMA synchronous = FULL;
         CREATE TABLE records (
             id INTEGER PRIMARY KEY,
             version INTEGER NOT NULL,
             title TEXT NOT NULL,
             payload BLOB NOT NULL
         );",
    )?;
    let transaction = connection.transaction()?;
    {
        let mut statement = transaction
            .prepare("INSERT INTO records (id, version, title, payload) VALUES (?1, 1, ?2, ?3)")?;
        for id in 0..parameters.sqlite_rows {
            let payload = deterministic_bytes(
                parameters.row_payload_bytes,
                0xbb67_ae85_84ca_a73b ^ u64::from(id),
            );
            statement.execute(params![id, format!("record-{id:08}"), payload])?;
        }
    }
    transaction.commit()?;
    connection.execute_batch("VACUUM;")?;
    Ok(())
}

fn mutate_dataset(worktree: &Path, parameters: &DatasetParameters) -> Result<()> {
    let mut connection = Connection::open(worktree.join(SQLITE_FILE))?;
    let transaction = connection.transaction()?;
    {
        let mut statement =
            transaction.prepare("UPDATE records SET version = 2, payload = ?1 WHERE id = ?2")?;
        let stride = parameters.sqlite_rows / parameters.updated_rows;
        for update in 0..parameters.updated_rows {
            let id = update * stride;
            let payload = deterministic_bytes(
                parameters.row_payload_bytes,
                0x3c6e_f372_fe94_f82b ^ u64::from(id),
            );
            statement.execute(params![payload, id])?;
        }
    }
    transaction.commit()?;
    drop(connection);

    let note = worktree.join("documents/note-0000.md");
    fs::write(
        &note,
        deterministic_text(u32::MAX, parameters.text_file_bytes),
    )?;
    let asset = worktree.join("assets/asset-0000.bin");
    let mut bytes = fs::read(&asset)?;
    let changed = deterministic_bytes(bytes.len().min(64 * 1024), 0xa54f_f53a_5f1d_36f1);
    bytes[..changed.len()].copy_from_slice(&changed);
    fs::write(asset, bytes)?;
    Ok(())
}

fn deterministic_text(index: u32, target_bytes: usize) -> Vec<u8> {
    let line = format!("document={index:010}; deterministic benchmark content\n");
    let mut contents = Vec::with_capacity(target_bytes);
    while contents.len() < target_bytes {
        let remaining = target_bytes - contents.len();
        contents.extend_from_slice(&line.as_bytes()[..remaining.min(line.len())]);
    }
    contents
}

fn deterministic_bytes(length: usize, seed: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(length);
    let mut state = seed;
    while bytes.len() < length {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        let remaining = length - bytes.len();
        bytes.extend_from_slice(&value.to_le_bytes()[..remaining.min(size_of::<u64>())]);
    }
    bytes
}

fn required_directory_size(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "required benchmark storage path is missing: {}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!(
            "required benchmark storage path is not a directory: {}",
            path.display()
        );
    }
    directory_size(path)
}

fn required_nonempty_directory_size(path: &Path) -> Result<u64> {
    let size = required_directory_size(path)?;
    if size == 0 {
        bail!(
            "required benchmark storage directory is empty: {}",
            path.display()
        );
    }
    Ok(size)
}

fn directory_size(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total
            .checked_add(directory_size(&entry.path())?)
            .context("benchmark directory size overflowed u64")?;
    }
    Ok(total)
}

fn directory_file_count(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(1);
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total
            .checked_add(directory_file_count(&entry.path())?)
            .context("benchmark file count overflowed u64")?;
    }
    Ok(total)
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_data_is_stable_and_seeded() {
        assert_eq!(deterministic_bytes(17, 42), deterministic_bytes(17, 42));
        assert_ne!(deterministic_bytes(17, 42), deterministic_bytes(17, 43));
        assert_eq!(deterministic_bytes(17, 42).len(), 17);
        assert_eq!(deterministic_text(1, 37).len(), 37);
    }

    #[test]
    fn directory_size_counts_nested_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("one"), [0_u8; 3]).unwrap();
        fs::write(temp.path().join("nested/two"), [0_u8; 5]).unwrap();
        assert_eq!(directory_size(temp.path()).unwrap(), 8);
        assert_eq!(directory_file_count(temp.path()).unwrap(), 2);
    }

    #[test]
    fn paired_samples_must_be_even() {
        assert!(validate_paired_sample_count(2).is_ok());
        assert!(validate_paired_sample_count(3).is_err());
    }

    #[test]
    fn row_diff_validation_checks_identity_and_values() {
        let parameters = DatasetParameters {
            profile: "test".to_string(),
            sqlite_rows: 2,
            updated_rows: 1,
            row_payload_bytes: 4,
            text_file_count: 1,
            text_file_bytes: 1,
            binary_file_count: 1,
            binary_file_bytes: 1,
        };
        let rowid = 0_u32;
        let old_payload = deterministic_bytes(
            parameters.row_payload_bytes,
            0xbb67_ae85_84ca_a73b ^ u64::from(rowid),
        );
        let new_payload = deterministic_bytes(
            parameters.row_payload_bytes,
            0x3c6e_f372_fe94_f82b ^ u64::from(rowid),
        );
        let output = json!({
            "files": [{
                "path": SQLITE_FILE,
                "row_diff_available": true,
                "tables": [{
                    "name": "records",
                    "columns": ["id", "version", "title", "payload"],
                    "changes": [{
                        "op": "update",
                        "rowid": rowid,
                        "old_values": [null, 1, "record-00000000", hex_bytes(&old_payload)],
                        "values": [null, 2, "record-00000000", hex_bytes(&new_payload)]
                    }]
                }]
            }]
        });
        let bytes = serde_json::to_vec(&output).unwrap();
        assert!(validate_row_diff(&bytes, &parameters).is_ok());

        let mut wrong = output;
        wrong["files"][0]["tables"][0]["changes"][0]["rowid"] = json!(1);
        assert!(validate_row_diff(&serde_json::to_vec(&wrong).unwrap(), &parameters).is_err());
    }
}
