pub mod compare;
pub mod model;
pub mod runner;

use std::{fs, path::Path};

use anyhow::{Context, Result};
use model::BenchmarkReport;

pub fn read_report(path: &Path) -> Result<BenchmarkReport> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read benchmark report {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse benchmark report {}", path.display()))
}

pub fn write_report(path: &Path, report: &BenchmarkReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create benchmark output directory {}",
                parent.display()
            )
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    fs::write(path, bytes)
        .with_context(|| format!("failed to write benchmark report {}", path.display()))
}
