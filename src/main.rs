use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

/// Shrink a PDF (including scanned documents) or an image to a target size,
/// or merge several PDFs and images into one PDF of that size.
#[derive(Parser)]
#[command(name = "docsqueeze_cli", version)]
struct Args {
    /// Input PDF or image files (JPEG, PNG, WebP, GIF, BMP, TIFF). Multiple
    /// inputs are merged into one PDF in the order given, unless --batch.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Target size in MB (1 MB = 1,000,000 bytes); per file with --batch
    target: f64,

    /// Shrink each input to the target separately instead of merging
    #[arg(short, long)]
    batch: bool,

    /// Output file (default: <input>-shrunk.pdf, <input>-shrunk.jpg for an
    /// image, or <first input>-merged.pdf). A .pdf output converts an image to
    /// PDF. With --batch, a folder for the results (default: next to each input).
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let Args { inputs, target, batch, output } = Args::parse();
    if target <= 0.0 {
        bail!("target must be greater than 0");
    }
    let target = (target * 1_000_000.0) as usize;
    let log = |msg: &str| println!("{msg}");

    if !batch {
        let report = docsqueeze::run(&inputs, target, output, &log)?;
        println!("{}", report.summary());
        return Ok(());
    }

    if let Some(dir) = &output {
        std::fs::create_dir_all(dir)?;
    }
    let outputs = docsqueeze::batch_outputs(&inputs, target, output.as_deref(), false)?;
    let mut failed = 0;
    for (input, output) in inputs.iter().zip(outputs) {
        match docsqueeze::run(std::slice::from_ref(input), target, Some(output), &log) {
            Ok(report) => println!("{}", report.summary()),
            Err(e) => {
                failed += 1;
                eprintln!("Error: {}: {e:#}", input.display());
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} files failed", inputs.len());
    }
    Ok(())
}
