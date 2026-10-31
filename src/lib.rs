//! Shrink PDFs (including scans) and images to a target size, or merge several
//! of them into one PDF of that size.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Cursor, Read};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use image::{
    DynamicImage, ExtendedColorType, GrayImage, ImageDecoder, ImageFormat, ImageReader, RgbImage,
};
use lopdf::{Document, Object, ObjectId};
use pic_scale::{ImageSize, ImageStore, ImageStoreMut, ResamplingFunction, Scaler};
use rayon::prelude::*;

mod merge;
mod raster;

/// Input formats accepted besides PDF, as file extensions.
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

/// Least to most aggressive: (JPEG quality, scale factor).
const LEVELS: &[(u8, f32)] = &[
    (90, 1.0),
    (80, 1.0),
    (70, 1.0),
    (60, 0.9),
    (50, 0.8),
    (45, 0.7),
    (40, 0.6),
    (35, 0.5),
    (30, 0.42),
    (25, 0.35),
    (20, 0.28),
    (15, 0.22),
    (10, 0.16),
];

/// Images smaller than this are not worth recompressing.
const MIN_IMAGE_BYTES: usize = 8 * 1024;

/// Receives progress messages.
pub type Log<'a> = &'a dyn Fn(&str);

/// Outcome of a successful [`run`].
#[derive(Clone, Debug)]
pub struct Report {
    pub output: PathBuf,
    /// Output size in bytes.
    pub size: usize,
    /// Total size of all inputs in bytes.
    pub original_size: usize,
    /// Target size in bytes.
    pub target: usize,
}

impl Report {
    pub fn met_target(&self) -> bool {
        self.size <= self.target
    }

    /// Percentage saved relative to the inputs.
    pub fn reduction(&self) -> f64 {
        100.0 * (1.0 - self.size as f64 / self.original_size as f64)
    }

    pub fn summary(&self) -> String {
        format!(
            "Output: {} ({}, {:.1}% smaller){}",
            self.output.display(),
            human(self.size),
            self.reduction(),
            if self.met_target() { "" } else { " -- above target" }
        )
    }
}

struct SourceImage {
    id: ObjectId,
    image: DynamicImage,
    original_len: usize,
}

struct Encoded {
    data: Vec<u8>,
    width: u32,
    height: u32,
    gray: bool,
}

/// Shrink `inputs` to at most `target` bytes and write the result to `output`
/// (or [`default_output`]). Several inputs, or one image with a `.pdf` output,
/// are merged into a single PDF in order. When the target cannot be reached,
/// the smallest achievable result is written; check [`Report::met_target`].
pub fn run(inputs: &[PathBuf], target: usize, output: Option<PathBuf>, log: Log) -> Result<Report> {
    // A malformed file can trip a bug in a parser or renderer. Report it as this
    // file's error rather than killing the GUI worker or the rest of a batch.
    std::panic::catch_unwind(AssertUnwindSafe(|| shrink(inputs, target, output, log))).unwrap_or_else(
        |panic| {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            bail!("internal error: {msg}")
        },
    )
}

fn shrink(inputs: &[PathBuf], target: usize, output: Option<PathBuf>, log: Log) -> Result<Report> {
    let Some(first) = inputs.first() else {
        bail!("no input files");
    };
    if target == 0 {
        bail!("target must be greater than 0");
    }
    let output = match output {
        Some(output) => output,
        None => default_output(inputs, target)?,
    };

    let mut loaded = Vec::with_capacity(inputs.len());
    for input in inputs {
        let data =
            std::fs::read(input).with_context(|| format!("cannot read {}", input.display()))?;
        log(&format!("Input:  {} ({})", input.display(), human(data.len())));
        let format = detect_format(&data);
        loaded.push(merge::Input { data, format });
    }
    log(&format!("Target: {}", human(target)));
    let original_size = loaded.iter().map(|i| i.data.len()).sum();

    let bytes = if loaded.len() == 1 && !(has_extension(&output, "pdf") && loaded[0].format.is_some())
    {
        let merge::Input { data, format } = loaded.pop().unwrap();
        match format {
            // Copy as-is only when the output's extension still describes the content.
            Some(format)
                if data.len() <= target
                    && format.extensions_str().iter().any(|e| has_extension(&output, e)) =>
            {
                data
            }
            Some(format) => shrink_image(first, data, format, target, log)?,
            None => shrink_pdf(first, data, target, log)?,
        }
    } else {
        if loaded.len() == 1 {
            log("Converting to PDF...");
        } else {
            log(&format!("Merging {} files...", loaded.len()));
        }
        let merged = merge::merge(&loaded)?;
        drop(loaded);
        shrink_pdf(&output, merged, target, log)?
    };

    std::fs::write(&output, &bytes)
        .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(Report { output, size: bytes.len(), original_size, target })
}

/// Where [`run`] writes when no output is given: `<input>-shrunk.pdf`,
/// `<input>-shrunk.jpg` for an image (original extension if it already fits),
/// or `<first input>-merged.pdf` for several inputs.
pub fn default_output(inputs: &[PathBuf], target: usize) -> Result<PathBuf> {
    let Some(first) = inputs.first() else {
        bail!("no input files");
    };
    let stem = first.file_stem().unwrap_or_default().to_string_lossy();
    if inputs.len() > 1 {
        return Ok(first.with_file_name(format!("{stem}-merged.pdf")));
    }

    // Sniffing needs only the header.
    let mut file = File::open(first).with_context(|| format!("cannot read {}", first.display()))?;
    let size = file.metadata()?.len();
    let mut header = Vec::with_capacity(64);
    file.by_ref().take(64).read_to_end(&mut header)?;
    let ext = match detect_format(&header) {
        Some(f) if size <= target as u64 => f.extensions_str()[0],
        Some(_) => "jpg",
        None => "pdf",
    };
    Ok(first.with_file_name(format!("{stem}-shrunk.{ext}")))
}

/// Outputs for shrinking each input on its own: [`default_output`] for each,
/// placed in `dir` when given, with images saved as PDF when `as_pdf`.
/// Paths never collide with each other or with an input.
pub fn batch_outputs(
    inputs: &[PathBuf],
    target: usize,
    dir: Option<&Path>,
    as_pdf: bool,
) -> Result<Vec<PathBuf>> {
    let mut taken: HashSet<PathBuf> = inputs.iter().cloned().collect();
    let mut outputs = Vec::with_capacity(inputs.len());
    for input in inputs {
        let mut path = default_output(std::slice::from_ref(input), target)?;
        if as_pdf {
            path.set_extension("pdf");
        }
        if let (Some(dir), Some(name)) = (dir, path.file_name()) {
            path = dir.join(name);
        }
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let ext = path.extension().unwrap_or_default().to_string_lossy().into_owned();
        let mut n = 2;
        while taken.contains(&path) {
            path.set_file_name(format!("{stem}-{n}.{ext}"));
            n += 1;
        }
        taken.insert(path.clone());
        outputs.push(path);
    }
    Ok(outputs)
}

/// Image format from content rather than extension; `None` means PDF.
fn detect_format(data: &[u8]) -> Option<ImageFormat> {
    (!data.starts_with(b"%PDF")).then(|| image::guess_format(data).ok()).flatten()
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Re-encode an image as JPEG at the least aggressive level that fits `target`.
fn shrink_image(
    input: &Path,
    data: Vec<u8>,
    format: ImageFormat,
    target: usize,
    log: Log,
) -> Result<Vec<u8>> {
    let image = load_image(&data, format)
        .with_context(|| format!("failed to decode {}", input.display()))?;
    drop(data);

    log("Recompressing image...");
    let mut smallest = Vec::new();
    let best = search(LEVELS.len(), target, &mut smallest, |i| {
        let (quality, scale) = LEVELS[i];
        Ok(encode(&image, quality, scale)?.data)
    })?;
    Ok(best.unwrap_or_else(|| {
        log("Could not reach target; writing smallest achievable result.");
        smallest
    }))
}

/// Decode to 8-bit gray or RGB, applying EXIF orientation (dropped on re-encode)
/// and flattening any alpha onto white, since JPEG has no transparency.
fn load_image(data: &[u8], format: ImageFormat) -> Result<DynamicImage> {
    let mut decoder = ImageReader::with_format(Cursor::new(data), format).into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);

    let gray = !image.color().has_color();
    if image.color().has_alpha() {
        let mut rgba = image.to_rgba8();
        for px in rgba.pixels_mut() {
            let a = px[3] as u16;
            for c in &mut px.0[..3] {
                *c = ((*c as u16 * a + 255 * (255 - a) + 127) / 255) as u8;
            }
            px[3] = 255;
        }
        image = DynamicImage::ImageRgba8(rgba);
    }
    Ok(if gray {
        DynamicImage::ImageLuma8(image.to_luma8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    })
}

fn shrink_pdf(input: &Path, data: Vec<u8>, target: usize, log: Log) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem(&data)
        .with_context(|| format!("failed to parse {}", input.display()))?;

    // Lossless pass: drop unreferenced objects and flate-compress uncompressed streams.
    doc.prune_objects();
    doc.delete_zero_length_streams();
    doc.compress();
    let mut smallest = serialize(&mut doc.clone())?;
    if smallest.len() <= target {
        return Ok(smallest);
    }

    // 1. Recompress embedded images; keeps text selectable.
    let images = collect_images(&doc);
    if !images.is_empty() {
        log(&format!("Recompressing {} image(s)...", images.len()));
        let best = search(LEVELS.len(), target, &mut smallest, |i| {
            let (quality, scale) = LEVELS[i];
            build_level(&doc, &images, quality, scale)
        })?;
        if let Some(bytes) = best {
            return Ok(bytes);
        }
    }
    drop(images);

    // 2. Rasterize whole pages; works for any scan format.
    log("Rasterizing pages...");
    match raster::Rasterizer::new(data, &doc) {
        Ok(rasterizer) => {
            let best = search(raster::LEVELS.len(), target, &mut smallest, |i| {
                let (dpi, quality) = raster::LEVELS[i];
                rasterizer.build(dpi, quality)
            })?;
            if let Some(bytes) = best {
                return Ok(bytes);
            }
        }
        Err(e) => log(&format!("Rasterization failed: {e:#}")),
    }

    log("Could not reach target; writing smallest achievable result.");
    Ok(smallest)
}

/// Binary search over levels ordered from least to most aggressive for the
/// first one whose output fits `target`. Tracks the smallest output seen.
fn search(
    levels: usize,
    target: usize,
    smallest: &mut Vec<u8>,
    mut build: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let mut best = None;
    let (mut lo, mut hi) = (0, levels);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let bytes = build(mid)?;
        if smallest.is_empty() || bytes.len() < smallest.len() {
            *smallest = bytes.clone();
        }
        if bytes.len() <= target {
            best = Some(bytes);
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(best)
}


fn serialize(doc: &mut Document) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    Ok(buf)
}

fn build_level(doc: &Document, images: &[SourceImage], quality: u8, scale: f32) -> Result<Vec<u8>> {
    let encoded: Vec<(ObjectId, Encoded)> = images
        .par_iter()
        .filter_map(|src| {
            let enc = encode(&src.image, quality, scale).ok()?;
            // Never replace an image with a larger one.
            (enc.data.len() < src.original_len).then_some((src.id, enc))
        })
        .collect();

    let mut doc = doc.clone();
    for (id, enc) in encoded {
        let Ok(Object::Stream(stream)) = doc.get_object_mut(id) else {
            continue;
        };
        let dict = &mut stream.dict;
        dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
        dict.remove(b"DecodeParms");
        dict.set("Width", enc.width as i64);
        dict.set("Height", enc.height as i64);
        dict.set("BitsPerComponent", 8);
        let cs: &[u8] = if enc.gray { b"DeviceGray" } else { b"DeviceRGB" };
        dict.set("ColorSpace", Object::Name(cs.to_vec()));
        stream.set_content(enc.data);
        stream.allows_compression = false;
    }
    serialize(&mut doc)
}

fn encode(image: &DynamicImage, quality: u8, scale: f32) -> Result<Encoded> {
    let resized;
    let image = if scale < 1.0 {
        let w = ((image.width() as f32 * scale).round() as u32).max(1);
        let h = ((image.height() as f32 * scale).round() as u32).max(1);
        resized = resize(image, w, h)?;
        &resized
    } else {
        image
    };

    let mut data = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut data, quality);
    let gray = matches!(image, DynamicImage::ImageLuma8(_));
    if gray {
        let buf = image.to_luma8();
        encoder.encode(&buf, buf.width(), buf.height(), ExtendedColorType::L8)?;
    } else {
        let buf = image.to_rgb8();
        encoder.encode(&buf, buf.width(), buf.height(), ExtendedColorType::Rgb8)?;
    }
    Ok(Encoded { data, width: image.width(), height: image.height(), gray })
}

/// Downscale a gray or RGB image (as produced by [`load_image`]).
fn resize(image: &DynamicImage, width: u32, height: u32) -> Result<DynamicImage> {
    let scaler = Scaler::new(ResamplingFunction::Bilinear);
    let (sw, sh, w, h) = (image.width() as usize, image.height() as usize, width as usize, height as usize);
    let (src_size, dst_size) = (ImageSize::new(sw, sh), ImageSize::new(w, h));
    Ok(match image {
        DynamicImage::ImageLuma8(src) => {
            let mut out = vec![0; w * h];
            let plan = scaler.plan_planar_resampling(src_size, dst_size)?;
            let src = ImageStore::<u8, 1>::from_slice(src.as_raw(), sw, sh)?;
            plan.resample(&src, &mut ImageStoreMut::<u8, 1>::from_slice(&mut out, w, h)?)?;
            DynamicImage::ImageLuma8(GrayImage::from_raw(width, height, out).unwrap())
        }
        other => {
            let rgb = other.to_rgb8();
            let mut out = vec![0; w * h * 3];
            let plan = scaler.plan_rgb_resampling(src_size, dst_size)?;
            let src = ImageStore::<u8, 3>::from_slice(rgb.as_raw(), sw, sh)?;
            plan.resample(&src, &mut ImageStoreMut::<u8, 3>::from_slice(&mut out, w, h)?)?;
            DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, out).unwrap())
        }
    })
}

fn collect_images(doc: &Document) -> Vec<SourceImage> {
    // Soft masks are alpha channels; lossy JPEG would damage them, so skip.
    let smasks: HashSet<ObjectId> = doc
        .objects
        .values()
        .filter_map(|obj| match obj {
            Object::Stream(s) => s.dict.get(b"SMask").ok()?.as_reference().ok(),
            _ => None,
        })
        .collect();

    let candidates: Vec<(ObjectId, &lopdf::Stream)> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| match obj {
            Object::Stream(s) if !smasks.contains(id) && s.content.len() >= MIN_IMAGE_BYTES => {
                Some((*id, s))
            }
            _ => None,
        })
        .collect();

    candidates
        .into_par_iter()
        .filter_map(|(id, stream)| {
            let image = decode_image(doc, stream)?;
            Some(SourceImage { id, image, original_len: stream.content.len() })
        })
        .collect()
}

fn decode_image(doc: &Document, stream: &lopdf::Stream) -> Option<DynamicImage> {
    let dict = &stream.dict;
    if dict.get(b"Subtype").ok()?.as_name().ok()? != b"Image" {
        return None;
    }
    let is_mask = dict.get(b"ImageMask").ok().and_then(|o| o.as_bool().ok()).unwrap_or(false);
    if is_mask || dict.has(b"Decode") || dict.has(b"Mask") {
        return None;
    }
    let filter = match dict.get(b"Filter").ok()? {
        Object::Name(n) => n.clone(),
        Object::Array(a) if a.len() == 1 => a[0].as_name().ok()?.to_vec(),
        _ => return None,
    };

    // JPX may omit /ColorSpace and carries its own; all other filters need it.
    if filter == b"JPXDecode" {
        if dict.has(b"SMaskInData") {
            return None;
        }
        if let Ok(cs) = dict.get(b"ColorSpace") {
            color_channels(doc, cs)?;
        }
        return decode_jpx(&stream.content);
    }
    let channels = color_channels(doc, dict.get(b"ColorSpace").ok()?)?;

    match filter.as_slice() {
        b"DCTDecode" => {
            let img = image::load_from_memory_with_format(&stream.content, ImageFormat::Jpeg).ok()?;
            match (&img, channels) {
                (DynamicImage::ImageLuma8(_), 1) | (DynamicImage::ImageRgb8(_), 3) => Some(img),
                _ => None,
            }
        }
        b"FlateDecode" => {
            if dict.get(b"BitsPerComponent").ok()?.as_i64().ok()? != 8 {
                return None;
            }
            let w = dict.get(b"Width").ok()?.as_i64().ok()? as u32;
            let h = dict.get(b"Height").ok()?.as_i64().ok()? as u32;
            let raw = stream.decompressed_content().ok()?;
            if raw.len() != (w * h * channels) as usize {
                return None;
            }
            match channels {
                1 => GrayImage::from_raw(w, h, raw).map(DynamicImage::ImageLuma8),
                3 => RgbImage::from_raw(w, h, raw).map(DynamicImage::ImageRgb8),
                _ => None,
            }
        }
        _ => None,
    }
}

fn decode_jpx(data: &[u8]) -> Option<DynamicImage> {
    use hayro_jpeg2000::{ColorSpace, DecodeSettings, DecoderContext, Image};

    let image = Image::new(data, &DecodeSettings::default()).ok()?;
    if image.has_alpha() {
        return None;
    }
    let channels = match image.color_space() {
        ColorSpace::Gray => 1,
        ColorSpace::RGB => 3,
        ColorSpace::Icc { num_channels: n @ (1 | 3), .. } => *n as u32,
        _ => return None,
    };
    let (w, h) = (image.width(), image.height());
    let mut ctx = DecoderContext::default();
    let raw = image.decode(&mut ctx).ok()?.data_u8();
    if raw.len() != (w * h * channels) as usize {
        return None;
    }
    match channels {
        1 => GrayImage::from_raw(w, h, raw).map(DynamicImage::ImageLuma8),
        _ => RgbImage::from_raw(w, h, raw).map(DynamicImage::ImageRgb8),
    }
}

/// Number of channels for color spaces we can safely convert to DeviceGray/DeviceRGB.
fn color_channels(doc: &Document, cs: &Object) -> Option<u32> {
    let cs = match cs {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    match cs {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"CalGray" => Some(1),
            b"DeviceRGB" | b"CalRGB" => Some(3),
            _ => None,
        },
        Object::Array(a) if a.len() == 2 && a[0].as_name().ok()? == b"ICCBased" => {
            let profile = doc.get_object(a[1].as_reference().ok()?).ok()?.as_stream().ok()?;
            match profile.dict.get(b"N").ok()?.as_i64().ok()? {
                1 => Some(1),
                3 => Some(3),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Format a byte count as decimal megabytes.
pub fn human(bytes: usize) -> String {
    format!("{:.2} MB", bytes as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Dictionary, Stream, dictionary};

    fn pixels(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7919 % 251) as u8).collect()
    }

    #[test]
    fn search_finds_least_aggressive_fitting_level() {
        let sizes = [100, 90, 80, 70, 60, 50];
        let mut smallest = Vec::new();
        let mut calls = 0;
        let best = search(sizes.len(), 75, &mut smallest, |i| {
            calls += 1;
            Ok(vec![0; sizes[i]])
        })
        .unwrap();
        assert_eq!(best.map(|b| b.len()), Some(70));
        assert!(calls <= 3, "binary search, got {calls} calls");
        assert!(smallest.len() <= 70);
    }

    #[test]
    fn search_reports_none_but_tracks_smallest_when_nothing_fits() {
        let sizes = [100, 90, 80];
        let mut smallest = Vec::new();
        let best = search(sizes.len(), 10, &mut smallest, |i| Ok(vec![0; sizes[i]])).unwrap();
        assert!(best.is_none());
        assert_eq!(smallest.len(), 80);
    }

    #[test]
    fn search_propagates_errors() {
        let mut smallest = Vec::new();
        assert!(search(3, 10, &mut smallest, |_| bail!("boom")).is_err());
    }

    #[test]
    fn detects_format_from_content() {
        assert_eq!(detect_format(b"%PDF-1.7\n..."), None);
        assert_eq!(detect_format(b"\x89PNG\r\n\x1a\n...."), Some(ImageFormat::Png));
        assert_eq!(detect_format(b"\xff\xd8\xff\xe0...."), Some(ImageFormat::Jpeg));
        assert_eq!(detect_format(b"garbage"), None);
    }

    #[test]
    fn extension_check_ignores_case() {
        assert!(has_extension(Path::new("a/B.PDF"), "pdf"));
        assert!(!has_extension(Path::new("a/b.pdf.jpg"), "pdf"));
        assert!(!has_extension(Path::new("pdf"), "pdf"));
    }

    #[test]
    fn encode_scales_and_keeps_gray() {
        let gray = DynamicImage::ImageLuma8(GrayImage::from_raw(100, 50, pixels(5000)).unwrap());
        let enc = encode(&gray, 80, 0.5).unwrap();
        assert_eq!((enc.width, enc.height, enc.gray), (50, 25, true));
        let decoded = image::load_from_memory(&enc.data).unwrap();
        assert_eq!(decoded.color(), image::ColorType::L8);

        let rgb = DynamicImage::ImageRgb8(RgbImage::from_raw(3, 3, pixels(27)).unwrap());
        let tiny = encode(&rgb, 50, 0.01).unwrap();
        assert_eq!((tiny.width, tiny.height, tiny.gray), (1, 1, false));
    }

    #[test]
    fn resize_keeps_colors_and_layout() {
        // Left half black, right half white; RGB right half red.
        let gray = GrayImage::from_fn(200, 100, |x, _| image::Luma([if x < 100 { 0 } else { 255 }]));
        let DynamicImage::ImageLuma8(small) = resize(&DynamicImage::ImageLuma8(gray), 50, 25).unwrap()
        else {
            panic!("gray stays gray")
        };
        assert_eq!(small.dimensions(), (50, 25));
        assert!(small.get_pixel(5, 12)[0] < 10 && small.get_pixel(45, 12)[0] > 245);

        let rgb = RgbImage::from_fn(90, 60, |x, _| image::Rgb(if x < 45 { [0, 0, 255] } else { [255, 0, 0] }));
        let DynamicImage::ImageRgb8(small) = resize(&DynamicImage::ImageRgb8(rgb), 31, 7).unwrap() else {
            panic!("rgb stays rgb")
        };
        assert_eq!(small.dimensions(), (31, 7));
        assert_eq!(small.get_pixel(1, 3).0, [0, 0, 255]);
        assert_eq!(small.get_pixel(29, 3).0, [255, 0, 0]);

        let tiny = resize(&DynamicImage::ImageRgb8(RgbImage::new(3, 3)), 1, 1).unwrap();
        assert_eq!((tiny.width(), tiny.height()), (1, 1));
    }

    #[test]
    fn lower_quality_is_smaller() {
        let rgb = DynamicImage::ImageRgb8(RgbImage::from_raw(200, 200, pixels(120_000)).unwrap());
        let sizes: Vec<usize> =
            LEVELS.iter().map(|&(q, s)| encode(&rgb, q, s).unwrap().data.len()).collect();
        assert!(sizes.first() > sizes.last(), "{sizes:?}");
    }

    #[test]
    fn load_image_normalizes_color_types() {
        let mut png = Cursor::new(Vec::new());
        image::GrayAlphaImage::from_raw(2, 1, vec![0, 0, 0, 255])
            .unwrap()
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let image = load_image(png.get_ref(), ImageFormat::Png).unwrap();
        let DynamicImage::ImageLuma8(gray) = image else { panic!("expected gray, got {image:?}") };
        assert_eq!(gray.as_raw(), &[255, 0], "transparent becomes white, opaque keeps value");
    }

    /// Pseudo-random 4-bit values: flate roughly halves them.
    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9u32;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8 & 0x0f
            })
            .collect()
    }

    fn image_stream(dict: Dictionary, raw: Vec<u8>) -> Stream {
        let mut stream = Stream::new(dict, raw);
        stream.compress().unwrap();
        assert!(stream.dict.has(b"Filter"), "fixture is flate-compressed");
        stream
    }

    #[test]
    fn decodes_flate_images_with_supported_color_spaces() {
        let mut doc = Document::with_version("1.7");
        let icc = doc.add_object(Stream::new(dictionary! { "N" => 3 }, vec![]));
        let base = |cs: Object| {
            dictionary! {
                "Subtype" => "Image", "Width" => 40, "Height" => 20,
                "BitsPerComponent" => 8, "ColorSpace" => cs,
            }
        };

        let rgb = image_stream(base("DeviceRGB".into()), vec![128; 2400]);
        assert!(matches!(decode_image(&doc, &rgb), Some(DynamicImage::ImageRgb8(_))));

        let gray = image_stream(base("DeviceGray".into()), vec![128; 800]);
        assert!(matches!(decode_image(&doc, &gray), Some(DynamicImage::ImageLuma8(_))));

        let iccbased = vec![Object::Name(b"ICCBased".to_vec()), icc.into()];
        let icc_rgb = image_stream(base(iccbased.into()), vec![128; 2400]);
        assert!(decode_image(&doc, &icc_rgb).is_some());

        let cmyk = image_stream(base("DeviceCMYK".into()), vec![128; 3200]);
        assert!(decode_image(&doc, &cmyk).is_none(), "CMYK is left to rasterization");

        let short = image_stream(base("DeviceRGB".into()), vec![128; 100]);
        assert!(decode_image(&doc, &short).is_none(), "wrong data length");

        let mut masked = base("DeviceRGB".into());
        masked.set("Decode", vec![1.into(), 0.into()]);
        assert!(decode_image(&doc, &image_stream(masked, vec![128; 2400])).is_none());

        let mut form = base("DeviceRGB".into());
        form.set("Subtype", "Form");
        assert!(decode_image(&doc, &image_stream(form, vec![128; 2400])).is_none());
    }

    #[test]
    fn collect_images_skips_small_images_and_soft_masks() {
        let mut doc = Document::with_version("1.7");
        let dict = |w: i64, h: i64, cs: &str| {
            dictionary! {
                "Subtype" => "Image", "Width" => w, "Height" => h,
                "BitsPerComponent" => 8, "ColorSpace" => cs,
            }
        };
        // Decodable like any image, so only the SMask reference excludes it.
        let mask = doc.add_object(image_stream(dict(200, 150, "DeviceGray"), noise(30_000)));
        let mut with_mask = dict(200, 150, "DeviceGray");
        with_mask.set("SMask", mask);
        with_mask.set("Filter", "FlateDecode");
        let big = image_stream(dict(200, 150, "DeviceGray"), noise(30_000));
        assert!(big.content.len() >= MIN_IMAGE_BYTES);
        let big_id = doc.add_object(Stream::new(with_mask, big.content.clone()).with_compression(false));
        doc.add_object(image_stream(dict(40, 40, "DeviceGray"), vec![0; 1600]));

        let found: Vec<ObjectId> = collect_images(&doc).iter().map(|s| s.id).collect();
        assert_eq!(found, [big_id]);
    }
}
