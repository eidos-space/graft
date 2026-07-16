use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use graft_bench::{
    compare, read_report,
    runner::{self, PairedRunConfig, Profile, RunConfig},
    write_report,
};

#[derive(Debug, Parser)]
#[command(about = "Reproducible end-to-end performance benchmarks for Graft")]
struct Cli {
    #[command(subcommand)]
    command: BenchmarkCommand,
}

#[derive(Debug, Subcommand)]
enum BenchmarkCommand {
    /// Run the fixed benchmark dataset against a graft executable.
    Run {
        #[arg(long)]
        graft_bin: PathBuf,

        #[arg(long)]
        output: PathBuf,

        #[arg(long, default_value = "candidate")]
        label: String,

        #[arg(long, value_enum, default_value = "ci")]
        profile: Profile,

        #[arg(long, default_value_t = 5)]
        samples: usize,

        #[arg(long, default_value_t = 1)]
        warmups: usize,
    },

    /// Run base and candidate samples in alternating order.
    RunPaired {
        #[arg(long)]
        baseline_graft_bin: PathBuf,

        #[arg(long)]
        candidate_graft_bin: PathBuf,

        #[arg(long)]
        baseline_output: PathBuf,

        #[arg(long)]
        candidate_output: PathBuf,

        #[arg(long, default_value = "baseline")]
        baseline_label: String,

        #[arg(long, default_value = "candidate")]
        candidate_label: String,

        #[arg(long, value_enum, default_value = "ci")]
        profile: Profile,

        #[arg(long, default_value_t = 6)]
        samples: usize,

        #[arg(long, default_value_t = 1)]
        warmups: usize,
    },

    /// Compare two JSON reports and write a Markdown summary.
    Compare {
        #[arg(long)]
        baseline: PathBuf,

        #[arg(long)]
        candidate: PathBuf,

        #[arg(long)]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        BenchmarkCommand::Run {
            graft_bin,
            output,
            label,
            profile,
            samples,
            warmups,
        } => {
            let report = runner::run(&RunConfig {
                graft_bin,
                label,
                profile,
                samples,
                warmups,
            })?;
            write_report(&output, &report)?;
        }
        BenchmarkCommand::RunPaired {
            baseline_graft_bin,
            candidate_graft_bin,
            baseline_output,
            candidate_output,
            baseline_label,
            candidate_label,
            profile,
            samples,
            warmups,
        } => {
            let (baseline, candidate) = runner::run_paired(&PairedRunConfig {
                baseline_graft_bin,
                candidate_graft_bin,
                baseline_label,
                candidate_label,
                profile,
                samples,
                warmups,
            })?;
            write_report(&baseline_output, &baseline)?;
            write_report(&candidate_output, &candidate)?;
        }
        BenchmarkCommand::Compare { baseline, candidate, output } => {
            let markdown = compare::markdown(&read_report(&baseline)?, &read_report(&candidate)?)?;
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&output, &markdown)?;
            print!("{markdown}");
        }
    }
    Ok(())
}
