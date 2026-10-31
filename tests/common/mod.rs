//! Fixtures generated on the fly, so tests need no binary files in the repo.
#![allow(dead_code)]

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};

/// Deterministic noise, which compresses poorly and so makes inputs large.
pub fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.max(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

pub fn noise_rgb(width: u32, height: u32, seed: u32) -> RgbImage {
    RgbImage::from_raw(width, height, noise((width * height * 3) as usize, seed)).unwrap()
}

pub fn encode_image(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    image.write_to(&mut buf, format).unwrap();
    buf.into_inner()
}

pub fn write_noise_image(dir: &Path, name: &str, width: u32, height: u32, format: ImageFormat) -> PathBuf {
    let path = dir.join(name);
    let data = encode_image(DynamicImage::ImageRgb8(noise_rgb(width, height, 7)), format);
    std::fs::write(&path, data).unwrap();
    path
}

/// A PNG whose left half is fully transparent and right half opaque red.
pub fn write_half_transparent_png(dir: &Path, name: &str) -> PathBuf {
    let image = RgbaImage::from_fn(64, 32, |x, _| {
        if x < 32 { image::Rgba([0, 0, 0, 0]) } else { image::Rgba([255, 0, 0, 255]) }
    });
    let path = dir.join(name);
    std::fs::write(&path, encode_image(DynamicImage::ImageRgba8(image), ImageFormat::Png)).unwrap();
    path
}

/// Image XObject content for one PDF page.
pub enum PageImage {
    /// Flate-compressed DeviceRGB noise, which the shrinker recompresses.
    Rgb(u32, u32),
    /// DeviceCMYK noise, which only rasterization can shrink.
    Cmyk(u32, u32),
}

/// A PDF with one page per `(width_pt, height_pt, image)`, each with a line of
/// text and, optionally, an image filling the page.
pub fn pdf(pages: &[(f32, f32, Option<PageImage>)]) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    let mut kids = Vec::new();
    for (n, (w, h, image)) in pages.iter().enumerate() {
        let mut ops = String::new();
        let mut xobjects = Dictionary::new();
        if let Some(image) = image {
            let (iw, ih, cs, channels) = match *image {
                PageImage::Rgb(iw, ih) => (iw, ih, "DeviceRGB", 3),
                PageImage::Cmyk(iw, ih) => (iw, ih, "DeviceCMYK", 4),
            };
            // 4-bit noise: still large, but flate shrinks it enough to be applied,
            // so the image is FlateDecode like in real PDFs.
            let mut pixels = noise((iw * ih * channels) as usize, n as u32 + 1);
            pixels.iter_mut().for_each(|p| *p = *p & 0x0f | 0x40);
            let image_id = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => iw as i64,
                    "Height" => ih as i64,
                    "ColorSpace" => cs,
                    "BitsPerComponent" => 8,
                },
                pixels,
            ));
            xobjects.set("Im0", image_id);
            ops.push_str(&format!("q {w} 0 0 {h} 0 0 cm /Im0 Do Q\n"));
        }
        ops.push_str(&format!("BT /F1 12 Tf 10 10 Td (Page {}) Tj ET", n + 1));
        let contents_id = doc.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));
        kids.push(doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), (*w).into(), (*h).into()],
            "Contents" => contents_id,
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => font_id },
                "XObject" => xobjects,
            },
        }));
    }

    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    doc.compress();
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

pub fn write_pdf(dir: &Path, name: &str, pages: &[(f32, f32, Option<PageImage>)]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, pdf(pages)).unwrap();
    path
}

/// MediaBox `(width, height)` of each page, in order.
pub fn page_sizes(data: &[u8]) -> Vec<(f32, f32)> {
    let doc = Document::load_mem(data).expect("output is a valid PDF");
    doc.get_pages()
        .into_values()
        .map(|id| {
            let page = doc.get_dictionary(id).unwrap();
            let mb = page.get(b"MediaBox").unwrap().as_array().unwrap();
            let num = |o: &Object| match o {
                Object::Integer(i) => *i as f32,
                Object::Real(r) => *r,
                other => panic!("bad MediaBox entry {other:?}"),
            };
            (num(&mb[2]) - num(&mb[0]), num(&mb[3]) - num(&mb[1]))
        })
        .collect()
}

pub fn file_size(path: &Path) -> usize {
    std::fs::metadata(path).unwrap().len() as usize
}

/// A photo-like image: smooth gradients, hard edges and light grain, so JPEG
/// sizes and quality behave like real scans and photos rather than noise.
pub fn photo_like(width: u32, height: u32) -> RgbImage {
    let grain = noise((width * height) as usize, 3);
    RgbImage::from_fn(width, height, |x, y| {
        let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
        let g = grain[(y * width + x) as usize] as f32 / 255.0 * 12.0 - 6.0;
        let (dx, dy) = (fx - 0.5, fy - 0.5);
        let disc = if dx * dx + dy * dy < 0.06 { 90.0 } else { 0.0 };
        let stripe = if (x / 40) % 2 == 0 && fy > 0.75 { 60.0 } else { 0.0 };
        let px = |v: f32| (v + g).clamp(0.0, 255.0) as u8;
        image::Rgb([px(40.0 + 180.0 * fx + disc), px(60.0 + 150.0 * fy - stripe), px(200.0 - 120.0 * fx + stripe)])
    })
}

/// Peak signal-to-noise ratio in dB at the reference's resolution, so lost
/// detail from downscaling counts. `test` is scaled up with the `image` crate,
/// independent of the resizer under test.
pub fn psnr(reference: &RgbImage, test: &RgbImage) -> f64 {
    let test = if reference.dimensions() == test.dimensions() {
        test.clone()
    } else {
        image::imageops::resize(test, reference.width(), reference.height(), image::imageops::FilterType::Triangle)
    };
    let test = &test;
    let sum: f64 = reference
        .as_raw()
        .iter()
        .zip(test.as_raw())
        .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
        .sum();
    let mse = sum / reference.as_raw().len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

/// Decoded image XObjects of every page, in page order.
pub fn page_images(pdf: &[u8]) -> Vec<RgbImage> {
    let doc = Document::load_mem(pdf).unwrap();
    let mut out = Vec::new();
    for page in doc.get_pages().into_values() {
        let resources = doc.get_page_resources(page).unwrap();
        let dict = match resources.0 {
            Some(d) => d.clone(),
            None => doc.get_dictionary(resources.1[0]).unwrap().clone(),
        };
        let xobjects = dict.get(b"XObject").unwrap().as_dict().unwrap();
        for (_, obj) in xobjects.iter() {
            let stream = doc.get_object(obj.as_reference().unwrap()).unwrap().as_stream().unwrap();
            assert_eq!(stream.dict.get(b"Filter").unwrap().as_name().unwrap(), b"DCTDecode");
            out.push(image::load_from_memory(&stream.content).unwrap().to_rgb8());
        }
    }
    out
}
