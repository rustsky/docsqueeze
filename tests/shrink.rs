//! End-to-end tests of the library API on generated PDFs and images.

mod common;

use std::cell::RefCell;
use std::path::PathBuf;

use common::*;
use docsqueeze::{Report, batch_outputs, default_output, run};
use image::ImageFormat;
use tempfile::TempDir;

fn quiet(_: &str) {}

/// Run and also return the log, to check which strategy was used.
fn run_logged(inputs: &[PathBuf], target: usize, output: Option<PathBuf>) -> (Report, Vec<String>) {
    let log = RefCell::new(Vec::new());
    let report = run(inputs, target, output, &|m| log.borrow_mut().push(m.to_owned())).unwrap();
    (report, log.into_inner())
}

fn assert_report_matches_file(report: &Report) {
    assert_eq!(report.size, file_size(&report.output), "report size matches the written file");
}

#[test]
fn shrinks_image_to_jpeg_under_target() {
    let dir = TempDir::new().unwrap();
    let input = write_noise_image(dir.path(), "photo.png", 600, 400, ImageFormat::Png);
    let target = 100_000;
    assert!(file_size(&input) > target);

    let report = run(std::slice::from_ref(&input), target, None, &quiet).unwrap();

    assert_eq!(report.output, dir.path().join("photo-shrunk.jpg"));
    assert!(report.met_target(), "{}", report.summary());
    assert_report_matches_file(&report);
    assert_eq!(report.original_size, file_size(&input));
    let data = std::fs::read(&report.output).unwrap();
    assert_eq!(image::guess_format(&data).unwrap(), ImageFormat::Jpeg);
    let decoded = image::load_from_memory(&data).unwrap();
    assert!(decoded.width() <= 600 && decoded.width() > 0);
}

#[test]
fn image_already_under_target_is_copied_unchanged() {
    let dir = TempDir::new().unwrap();
    let input = write_noise_image(dir.path(), "small.png", 40, 40, ImageFormat::Png);

    let report = run(std::slice::from_ref(&input), 1_000_000, None, &quiet).unwrap();

    assert_eq!(report.output, dir.path().join("small-shrunk.png"));
    assert_eq!(std::fs::read(&report.output).unwrap(), std::fs::read(&input).unwrap());
}

#[test]
fn small_image_with_mismatched_extension_is_reencoded() {
    let dir = TempDir::new().unwrap();
    let input = write_noise_image(dir.path(), "small.png", 40, 40, ImageFormat::Png);
    let output = dir.path().join("out.jpg");

    run(&[input], 1_000_000, Some(output.clone()), &quiet).unwrap();

    let data = std::fs::read(&output).unwrap();
    assert_eq!(image::guess_format(&data).unwrap(), ImageFormat::Jpeg);
}

#[test]
fn transparent_pixels_become_white() {
    let dir = TempDir::new().unwrap();
    let input = write_half_transparent_png(dir.path(), "logo.png");
    let output = dir.path().join("logo.jpg");

    run(&[input], 1_000_000, Some(output.clone()), &quiet).unwrap();

    let image = image::open(&output).unwrap().to_rgb8();
    let left = image.get_pixel(8, 16);
    let right = image.get_pixel(56, 16);
    assert!(left.0.iter().all(|&c| c > 240), "transparent area is white, got {left:?}");
    assert!(right[0] > 200 && right[1] < 60 && right[2] < 60, "opaque area stays red, got {right:?}");
}

#[test]
fn image_to_pdf_output_converts() {
    let dir = TempDir::new().unwrap();
    let input = write_noise_image(dir.path(), "id.jpg", 300, 200, ImageFormat::Jpeg);
    let output = dir.path().join("id.pdf");

    let report = run(&[input], 5_000_000, Some(output.clone()), &quiet).unwrap();

    assert!(report.met_target());
    let sizes = page_sizes(&std::fs::read(&output).unwrap());
    assert_eq!(sizes.len(), 1);
    let (w, h) = sizes[0];
    assert!(w > h, "landscape image gives a landscape page, got {w}x{h}");
}

#[test]
fn pdf_already_small_stays_valid() {
    let dir = TempDir::new().unwrap();
    let input = write_pdf(dir.path(), "letter.pdf", &[(612.0, 792.0, None)]);

    let report = run(&[input], 1_000_000, None, &quiet).unwrap();

    assert_eq!(report.output, dir.path().join("letter-shrunk.pdf"));
    assert!(report.met_target());
    assert_eq!(page_sizes(&std::fs::read(&report.output).unwrap()), [(612.0, 792.0)]);
}

#[test]
fn pdf_images_are_recompressed_without_rasterizing() {
    let dir = TempDir::new().unwrap();
    let input = write_pdf(
        dir.path(),
        "photos.pdf",
        &[(300.0, 300.0, Some(PageImage::Rgb(400, 400))), (300.0, 300.0, Some(PageImage::Rgb(400, 400)))],
    );
    let target = file_size(&input) / 4;

    let (report, log) = run_logged(&[input], target, None);

    assert!(report.met_target(), "{}", report.summary());
    assert_report_matches_file(&report);
    assert!(log.iter().any(|m| m.starts_with("Recompressing 2 image")), "{log:?}");
    assert!(!log.iter().any(|m| m.contains("Rasterizing")), "{log:?}");
    let data = std::fs::read(&report.output).unwrap();
    assert_eq!(page_sizes(&data).len(), 2);
    let doc = lopdf::Document::load_mem(&data).unwrap();
    let text = doc.extract_text(&[1]).unwrap();
    assert!(text.contains("Page 1"), "text is kept, got {text:?}");
}

#[test]
fn pdf_with_unsupported_images_is_rasterized() {
    let dir = TempDir::new().unwrap();
    let input = write_pdf(dir.path(), "scan.pdf", &[(200.0, 200.0, Some(PageImage::Cmyk(300, 300)))]);
    let target = file_size(&input) / 4;

    let (report, log) = run_logged(&[input], target, None);

    assert!(report.met_target(), "{}", report.summary());
    assert!(log.iter().any(|m| m.contains("Rasterizing")), "{log:?}");
    assert_eq!(page_sizes(&std::fs::read(&report.output).unwrap()), [(200.0, 200.0)]);
}

#[test]
fn unreachable_target_writes_smallest_result() {
    let dir = TempDir::new().unwrap();
    let input = write_noise_image(dir.path(), "photo.png", 200, 200, ImageFormat::Png);

    let (report, log) = run_logged(std::slice::from_ref(&input), 1, None);

    assert!(!report.met_target());
    assert!(report.size < file_size(&input));
    assert!(report.summary().ends_with("-- above target"), "{}", report.summary());
    assert!(log.iter().any(|m| m.contains("Could not reach target")), "{log:?}");
    assert_report_matches_file(&report);
}

#[test]
fn merges_pdfs_and_images_in_order() {
    let dir = TempDir::new().unwrap();
    let a = write_pdf(dir.path(), "a.pdf", &[(300.0, 400.0, None), (310.0, 400.0, None)]);
    let portrait = write_noise_image(dir.path(), "b.png", 100, 200, ImageFormat::Png);
    let landscape = write_noise_image(dir.path(), "c.jpg", 200, 100, ImageFormat::Jpeg);
    let d = write_pdf(dir.path(), "d.pdf", &[(320.0, 400.0, None)]);

    let report = run(&[a, portrait, landscape, d], 5_000_000, None, &quiet).unwrap();

    assert_eq!(report.output, dir.path().join("a-merged.pdf"));
    assert!(report.met_target());
    let sizes = page_sizes(&std::fs::read(&report.output).unwrap());
    assert_eq!(sizes.len(), 5);
    assert_eq!(sizes[0], (300.0, 400.0));
    assert_eq!(sizes[1], (310.0, 400.0));
    assert!(sizes[2].0 < sizes[2].1, "portrait image page, got {:?}", sizes[2]);
    assert!(sizes[3].0 > sizes[3].1, "landscape image page, got {:?}", sizes[3]);
    assert_eq!(sizes[4], (320.0, 400.0));
}

#[test]
fn merged_output_is_shrunk_to_target() {
    let dir = TempDir::new().unwrap();
    let a = write_noise_image(dir.path(), "a.png", 400, 300, ImageFormat::Png);
    let b = write_pdf(dir.path(), "b.pdf", &[(300.0, 300.0, Some(PageImage::Rgb(300, 300)))]);
    let target = (file_size(&a) + file_size(&b)) / 5;

    let report = run(&[a, b], target, None, &quiet).unwrap();

    assert!(report.met_target(), "{}", report.summary());
    assert_eq!(page_sizes(&std::fs::read(&report.output).unwrap()).len(), 2);
}

#[test]
fn rejects_bad_arguments_and_inputs() {
    let dir = TempDir::new().unwrap();
    let image = write_noise_image(dir.path(), "a.png", 10, 10, ImageFormat::Png);
    let garbage = dir.path().join("notes.pdf");
    std::fs::write(&garbage, b"not a document").unwrap();
    let missing = dir.path().join("missing.pdf");

    assert!(run(&[], 1000, None, &quiet).is_err());
    assert!(run(std::slice::from_ref(&image), 0, None, &quiet).is_err());
    let err = run(std::slice::from_ref(&missing), 1000, None, &quiet).unwrap_err();
    assert!(format!("{err:#}").contains("cannot read"), "{err:#}");
    assert!(run(std::slice::from_ref(&garbage), 1000, None, &quiet).is_err());
    assert!(run(&[image, garbage], 1000, None, &quiet).is_err(), "merge fails on a bad input");
    assert!(!dir.path().join("notes-shrunk.pdf").exists());
}

#[test]
fn default_output_names() {
    let dir = TempDir::new().unwrap();
    let png = write_noise_image(dir.path(), "scan.png", 50, 50, ImageFormat::Png);
    let pdf = write_pdf(dir.path(), "doc.pdf", &[(100.0, 100.0, None)]);
    let size = file_size(&png);

    assert_eq!(default_output(std::slice::from_ref(&png), size).unwrap(), dir.path().join("scan-shrunk.png"));
    assert_eq!(default_output(std::slice::from_ref(&png), size - 1).unwrap(), dir.path().join("scan-shrunk.jpg"));
    assert_eq!(default_output(std::slice::from_ref(&pdf), 1).unwrap(), dir.path().join("doc-shrunk.pdf"));
    assert_eq!(default_output(&[pdf, png], 1).unwrap(), dir.path().join("doc-merged.pdf"));
    assert!(default_output(&[], 1).is_err());
    assert!(default_output(&[dir.path().join("missing.png")], 1).is_err());
}

#[test]
fn batch_outputs_never_collide() {
    let dir = TempDir::new().unwrap();
    let (one, two) = (dir.path().join("one"), dir.path().join("two"));
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    let a = write_noise_image(&one, "x.png", 50, 50, ImageFormat::Png);
    let b = write_noise_image(&two, "x.png", 50, 50, ImageFormat::Png);
    let c = write_noise_image(&two, "x.jpg", 50, 50, ImageFormat::Jpeg);
    let out = dir.path().join("out");

    // Next to each input: different folders, so no clash.
    let next_to = batch_outputs(&[a.clone(), b.clone()], 1, None, false).unwrap();
    assert_eq!(next_to, [one.join("x-shrunk.jpg"), two.join("x-shrunk.jpg")]);

    // One folder: same names get numbered.
    let together = batch_outputs(&[a.clone(), b.clone(), c.clone()], 1, Some(&out), false).unwrap();
    assert_eq!(together, [out.join("x-shrunk.jpg"), out.join("x-shrunk-2.jpg"), out.join("x-shrunk-3.jpg")]);

    // As PDF.
    let pdfs = batch_outputs(&[a, c], 1, Some(&out), true).unwrap();
    assert_eq!(pdfs, [out.join("x-shrunk.pdf"), out.join("x-shrunk-2.pdf")]);
}

#[test]
fn batch_outputs_never_overwrite_an_input() {
    let dir = TempDir::new().unwrap();
    let doc = write_pdf(dir.path(), "doc.pdf", &[(100.0, 100.0, None)]);
    let already = write_pdf(dir.path(), "doc-shrunk.pdf", &[(100.0, 100.0, None)]);

    let outputs = batch_outputs(&[doc, already.clone()], 1, None, false).unwrap();

    assert_eq!(outputs[0], dir.path().join("doc-shrunk-2.pdf"));
    assert_eq!(outputs[1], dir.path().join("doc-shrunk-shrunk.pdf"));
    assert!(!outputs.contains(&already));
}

#[test]
fn report_math() {
    let report = Report { output: "x.pdf".into(), size: 250, original_size: 1000, target: 300 };
    assert!(report.met_target());
    assert!((report.reduction() - 75.0).abs() < 1e-9);
    assert!(report.summary().contains("75.0% smaller"));
    assert!(!report.summary().contains("above target"));
    let over = Report { target: 200, ..report };
    assert!(!over.met_target());
    assert_eq!(docsqueeze::human(1_234_567), "1.23 MB");
}

#[test]
fn panics_become_errors() {
    let dir = TempDir::new().unwrap();
    let input = write_noise_image(dir.path(), "a.png", 10, 10, ImageFormat::Png);

    let err = run(&[input], 1000, None, &|_| panic!("boom")).unwrap_err();

    assert!(format!("{err:#}").contains("internal error: boom"), "{err:#}");
}
