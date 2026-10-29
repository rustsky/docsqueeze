//! Merging: combine PDFs and images into one document, in input order.
//!
//! Images become one page each, sized to fit A4. Pages from PDFs keep their
//! content; catalog-level data (outlines, forms, structure tree) is dropped.

use std::io::Cursor;

use anyhow::{Context, Result};
use image::codecs::jpeg::JpegDecoder;
use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageFormat};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::{load_image, serialize};

/// A4 in points.
const PAGE_SHORT: f32 = 595.0;
const PAGE_LONG: f32 = 842.0;

/// Page attributes a page may inherit from its ancestors in the page tree.
const INHERITABLE: &[&[u8]] = &[b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

pub struct Input {
    pub data: Vec<u8>,
    /// `None` for PDFs.
    pub format: Option<ImageFormat>,
}

pub fn merge(inputs: &[Input]) -> Result<Vec<u8>> {
    let mut out = Document::with_version("1.7");
    let pages_id = out.new_object_id();
    let mut kids = Vec::new();

    for (n, input) in inputs.iter().enumerate() {
        let n = n + 1;
        match input.format {
            None => append_pdf(&mut out, pages_id, &mut kids, &input.data)
                .with_context(|| format!("failed to merge input #{n}"))?,
            Some(format) => kids.push(
                append_image(&mut out, pages_id, &input.data, format)
                    .with_context(|| format!("failed to merge input #{n}"))?,
            ),
        }
    }

    let count = kids.len() as i64;
    out.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => count,
        }),
    );
    let catalog_id = out.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    out.trailer.set("Root", catalog_id);
    serialize(&mut out)
}

fn append_pdf(out: &mut Document, parent: ObjectId, kids: &mut Vec<ObjectId>, data: &[u8]) -> Result<()> {
    let mut doc = Document::load_mem(data)?;

    // Pages are reparented below, so copy inherited attributes onto each page.
    for id in doc.get_pages().into_values() {
        let found: Vec<_> = INHERITABLE
            .iter()
            .filter_map(|&key| Some((key, inherited(&doc, id, key)?)))
            .collect();
        if let Ok(page) = doc.get_dictionary_mut(id) {
            for (key, value) in found {
                page.set(key, value);
            }
        }
    }

    doc.renumber_objects_with(out.max_id + 1);
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for &id in &page_ids {
        if let Ok(page) = doc.get_dictionary_mut(id) {
            page.set("Parent", parent);
        }
    }
    kids.extend(&page_ids);

    // Old catalog and page tree nodes are replaced by the merged ones.
    for (id, obj) in doc.objects {
        let kind = obj.as_dict().ok().and_then(|d| d.get(b"Type").ok()?.as_name().ok());
        if !matches!(kind, Some(b"Catalog" | b"Pages")) {
            out.objects.insert(id, obj);
        }
    }
    out.max_id = out.max_id.max(doc.max_id);
    Ok(())
}

/// Look `key` up on the page, then on its ancestors.
fn inherited(doc: &Document, page: ObjectId, key: &[u8]) -> Option<Object> {
    let mut dict = doc.get_dictionary(page).ok()?;
    // Bounded to survive malformed (cyclic) page trees.
    for _ in 0..64 {
        if let Ok(value) = dict.get(key) {
            return Some(value.clone());
        }
        let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
        dict = doc.get_dictionary(parent).ok()?;
    }
    None
}

fn append_image(out: &mut Document, parent: ObjectId, data: &[u8], format: ImageFormat) -> Result<ObjectId> {
    let (image, width, height) = image_xobject(data, format)?;
    let image_id = out.add_object(image);

    // Fit inside A4, rotated to match the image's orientation.
    let (box_w, box_h) =
        if width > height { (PAGE_LONG, PAGE_SHORT) } else { (PAGE_SHORT, PAGE_LONG) };
    let scale = (box_w / width as f32).min(box_h / height as f32);
    let (w, h) = (width as f32 * scale, height as f32 * scale);

    let ops = format!("q {w} 0 0 {h} 0 0 cm /Im0 Do Q");
    let contents_id = out.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));
    Ok(out.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => parent,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
    }))
}

/// Embed JPEGs as-is when the PDF can show them unchanged; otherwise store
/// decoded pixels, flate-compressed later, and leave lossy work to the shrinker.
fn image_xobject(data: &[u8], format: ImageFormat) -> Result<(Stream, u32, u32)> {
    if format == ImageFormat::Jpeg {
        let mut decoder = JpegDecoder::new(Cursor::new(data))?;
        let cs: Option<&[u8]> = match decoder.color_type() {
            ColorType::L8 => Some(b"DeviceGray"),
            ColorType::Rgb8 => Some(b"DeviceRGB"),
            _ => None,
        };
        if let (Some(cs), Orientation::NoTransforms) = (cs, decoder.orientation()?) {
            let (w, h) = decoder.dimensions();
            let mut stream = Stream::new(image_dict(w, h, cs), data.to_vec());
            stream.dict.set("Filter", "DCTDecode");
            stream.allows_compression = false;
            return Ok((stream, w, h));
        }
    }

    let image = load_image(data, format)?;
    let (w, h) = (image.width(), image.height());
    let cs: &[u8] =
        if matches!(image, DynamicImage::ImageLuma8(_)) { b"DeviceGray" } else { b"DeviceRGB" };
    Ok((Stream::new(image_dict(w, h, cs), image.into_bytes()), w, h))
}

fn image_dict(width: u32, height: u32, color_space: &[u8]) -> Dictionary {
    dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width as i64,
        "Height" => height as i64,
        "ColorSpace" => Object::Name(color_space.to_vec()),
        "BitsPerComponent" => 8,
    }
}
