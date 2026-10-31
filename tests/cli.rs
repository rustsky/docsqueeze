//! Tests of the `docsqueeze_cli` binary.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::*;
use image::ImageFormat;
use tempfile::TempDir;

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_docsqueeze_cli"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn prints_help_and_version() {
    let dir = TempDir::new().unwrap();
    let help = cli(dir.path(), &["--help"]);
    assert!(help.status.success());
    assert!(stdout(&help).contains("docsqueeze_cli"), "{}", stdout(&help));
    assert!(stdout(&help).contains("--batch"));

    let version = cli(dir.path(), &["--version"]);
    assert!(version.status.success());
    assert!(stdout(&version).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn shrinks_one_file() {
    let dir = TempDir::new().unwrap();
    write_noise_image(dir.path(), "photo.png", 400, 300, ImageFormat::Png);

    let out = cli(dir.path(), &["photo.png", "0.1"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Output: photo-shrunk.jpg"), "{}", stdout(&out));
    assert!(file_size(&dir.path().join("photo-shrunk.jpg")) <= 100_000);
}

#[test]
fn merges_with_explicit_output() {
    let dir = TempDir::new().unwrap();
    write_pdf(dir.path(), "a.pdf", &[(300.0, 400.0, None)]);
    write_noise_image(dir.path(), "b.jpg", 200, 100, ImageFormat::Jpeg);

    let out = cli(dir.path(), &["a.pdf", "b.jpg", "2", "-o", "all.pdf"]);

    assert!(out.status.success(), "{}", stderr(&out));
    let sizes = page_sizes(&std::fs::read(dir.path().join("all.pdf")).unwrap());
    assert_eq!(sizes.len(), 2);
}

#[test]
fn batch_writes_each_file_into_folder() {
    let dir = TempDir::new().unwrap();
    write_noise_image(dir.path(), "a.png", 300, 300, ImageFormat::Png);
    write_pdf(dir.path(), "b.pdf", &[(100.0, 100.0, None)]);

    let out = cli(dir.path(), &["--batch", "a.png", "b.pdf", "0.1", "-o", "upload"]);

    assert!(out.status.success(), "{}", stderr(&out));
    let upload = dir.path().join("upload");
    assert!(file_size(&upload.join("a-shrunk.jpg")) <= 100_000);
    assert_eq!(page_sizes(&std::fs::read(upload.join("b-shrunk.pdf")).unwrap()).len(), 1);
}

#[test]
fn batch_continues_past_failures_and_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("bad.pdf"), b"not a document").unwrap();
    write_noise_image(dir.path(), "good.png", 50, 50, ImageFormat::Png);

    let out = cli(dir.path(), &["-b", "bad.pdf", "good.png", "1"]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("bad.pdf"), "{}", stderr(&out));
    assert!(stderr(&out).contains("1 of 2 files failed"), "{}", stderr(&out));
    assert!(dir.path().join("good-shrunk.png").exists(), "later files still processed");
}

#[test]
fn rejects_invalid_targets() {
    let dir = TempDir::new().unwrap();
    write_noise_image(dir.path(), "a.png", 10, 10, ImageFormat::Png);

    for target in ["0", "-1", "abc"] {
        let out = cli(dir.path(), &["a.png", "--", target]);
        assert!(!out.status.success(), "target {target} accepted");
    }
    let missing = cli(dir.path(), &["missing.pdf", "1"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("cannot read"), "{}", stderr(&missing));
}
