use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

/// Shrink a PDF (including scanned documents) or an image to a target size,
/// or merge several PDFs and images into one PDF of that size.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Input PDF or image files (JPEG, PNG, WebP, GIF, BMP, TIFF). Multiple
    /// inputs are merged into one PDF in the order given.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Target size in MB (1 MB = 1,000,000 bytes)
    target: f64,

    /// Output file (default: <input>-shrunk.pdf, <input>-shrunk.jpg for an
    /// image, or <first input>-merged.pdf). A .pdf output converts an image to PDF.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let Args { inputs, target, output } = Args::parse();
    if target <= 0.0 {
        bail!("target must be greater than 0");
    }
    let target = (target * 1_000_000.0) as usize;
    let report = docsqueeze::run(&inputs, target, output, &|msg| println!("{msg}"))?;
    println!("{}", report.summary());
    Ok(())
}
