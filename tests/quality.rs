//! Output quality guards: shrinking must stay under the target without
//! degrading the picture more than needed. Thresholds sit a little below
//! measured values, so faster or smaller code that hurts quality fails here.

mod common;

use common::*;
use docsqueeze::run;
use image::{DynamicImage, ImageFormat};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};
use tempfile::TempDir;

fn quiet(_: &str) {}

fn write_photo(dir: &std::path::Path, name: &str, format: ImageFormat) -> (std::path::PathBuf, image::RgbImage) {
    let photo = photo_like(1600, 1200);
    let path = dir.join(name);
    std::fs::write(&path, encode_image(DynamicImage::ImageRgb8(photo.clone()), format)).unwrap();
    (path, photo)
}

#[test]
fn shrunk_photo_keeps_quality_and_shape() {
    let dir = TempDir::new().unwrap();
    let (input, source) = write_photo(dir.path(), "photo.png", ImageFormat::Png);

    for (target, min_psnr) in [(400_000, 38.0), (150_000, 36.0), (40_000, 34.0)] {
        let output = dir.path().join(format!("out-{target}.jpg"));
        let report = run(std::slice::from_ref(&input), target, Some(output.clone()), &quiet).unwrap();
        assert!(report.met_target(), "{}", report.summary());
        // Search should not overshoot wildly: at least a quarter of the budget is used.
        assert!(report.size * 4 >= target, "{target}: only {} bytes used", report.size);

        let out = image::open(&output).unwrap().to_rgb8();
        let (w, h) = out.dimensions();
        let aspect = (w as f64 / h as f64) / (1600.0 / 1200.0);
        assert!((aspect - 1.0).abs() < 0.01, "{target}: aspect changed to {w}x{h}");
        let q = psnr(&source, &out);
        eprintln!("photo {target}: {q:.2} dB, {} bytes, {w}x{h}", report.size);
        assert!(q >= min_psnr, "{target}: PSNR {q:.2} dB < {min_psnr} ({w}x{h})");
    }
}

#[test]
fn gray_scan_stays_gray() {
    let dir = TempDir::new().unwrap();
    let gray = DynamicImage::ImageRgb8(photo_like(1200, 1600)).to_luma8();
    let input = dir.path().join("scan.png");
    std::fs::write(&input, encode_image(DynamicImage::ImageLuma8(gray.clone()), ImageFormat::Png)).unwrap();
    let output = dir.path().join("scan.jpg");

    run(&[input], 100_000, Some(output.clone()), &quiet).unwrap();

    let out = image::open(&output).unwrap();
    assert_eq!(out.color(), image::ColorType::L8, "gray stored as one channel");
    let q = psnr(&DynamicImage::ImageLuma8(gray).to_rgb8(), &out.to_rgb8());
    eprintln!("gray: {q:.2} dB");
    assert!(q >= 36.5, "PSNR {q:.2} dB");
}

#[test]
fn recompressed_pdf_images_keep_quality() {
    let dir = TempDir::new().unwrap();
    let (a, photo) = write_photo(dir.path(), "a.png", ImageFormat::Png);
    let (b, _) = write_photo(dir.path(), "b.png", ImageFormat::Png);
    let merged = dir.path().join("merged.pdf");
    run(&[a, b], 50_000_000, Some(merged.clone()), &quiet).unwrap();
    let target = 300_000;

    let output = dir.path().join("small.pdf");
    let report = run(&[merged], target, Some(output.clone()), &quiet).unwrap();

    assert!(report.met_target(), "{}", report.summary());
    let images = page_images(&std::fs::read(&output).unwrap());
    assert_eq!(images.len(), 2);
    for image in &images {
        let q = psnr(&photo, image);
        eprintln!("pdf page: {q:.2} dB");
        assert!(q >= 36.0, "PSNR {q:.2} dB ({}x{})", image.width(), image.height());
    }
}

/// CMYK forces rasterization; colors must come through the renderer intact.
#[test]
fn rasterized_pages_keep_colors() {
    let (w, h) = (400u32, 400u32);
    // Left half cyan, right half magenta (CMYK), with grain in K so lossless
    // compression alone cannot reach the target.
    let grain = noise((w * h) as usize, 5);
    let pixels: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let k = grain[i as usize] & 0x1f;
            if i % w < w / 2 { [255, 0, 0, k] } else { [0, 255, 0, k] }
        })
        .collect();
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let image_id = doc.add_object(
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => w as i64, "Height" => h as i64,
                "ColorSpace" => "DeviceCMYK", "BitsPerComponent" => 8,
            },
            pixels,
        )
        .with_compression(false),
    );
    let contents = doc.add_object(Stream::new(Dictionary::new(), b"q 300 0 0 300 0 0 cm /Im0 Do Q".to_vec()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => contents,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
    });
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
        "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
    }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("cmyk.pdf");
    doc.save(&input).unwrap();
    let target = std::fs::metadata(&input).unwrap().len() as usize / 10;

    let output = dir.path().join("out.pdf");
    let report = run(&[input], target, Some(output.clone()), &quiet).unwrap();

    assert!(report.met_target(), "{}", report.summary());
    let images = page_images(&std::fs::read(&output).unwrap());
    assert_eq!(images.len(), 1);
    let img = &images[0];
    let (iw, ih) = img.dimensions();
    let left = img.get_pixel(iw / 4, ih / 2);
    let right = img.get_pixel(iw * 3 / 4, ih / 2);
    assert!(left[0] < 80 && left[1] > 150 && left[2] > 150, "cyan, got {left:?}");
    assert!(left[0] < 80, "cyan has little red");
    assert!(right[1] < 80 && right[0] > 150 && right[2] > 100, "magenta, got {right:?}");
}
