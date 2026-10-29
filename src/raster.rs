//! Page rasterization: render every page to a single JPEG.
//!
//! Fallback for scanned documents whose images use formats we cannot
//! recompress directly (JBIG2, CCITT, CMYK, masked layers, ...).
//! Text and vector content become part of the bitmap.

use anyhow::{Result, bail};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use image::{DynamicImage, GrayImage, ImageFormat, RgbImage};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};
use rayon::prelude::*;

use crate::{encode, serialize};

/// Resolution pages are rendered at; levels scale down from it.
const RENDER_DPI: f32 = 200.0;

/// Least to most aggressive: (DPI, JPEG quality).
pub const LEVELS: &[(f32, u8)] = &[
    (200.0, 80),
    (200.0, 65),
    (170.0, 60),
    (150.0, 55),
    (150.0, 45),
    (130.0, 45),
    (110.0, 40),
    (100.0, 35),
    (90.0, 35),
    (80.0, 30),
    (72.0, 30),
    (60.0, 25),
    (50.0, 25),
];

struct RenderedPage {
    /// Page size in points.
    width_pt: f32,
    height_pt: f32,
    /// High-quality JPEG rendered at `RENDER_DPI`.
    jpeg: Vec<u8>,
}

pub struct Rasterizer {
    base: Document,
    page_ids: Vec<ObjectId>,
    pages: Vec<RenderedPage>,
}

impl Rasterizer {
    /// Render all pages of `data` and prepare `doc` for page replacement.
    pub fn new(data: Vec<u8>, doc: &Document) -> Result<Self> {
        let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
        let page_count = match Pdf::new(data.clone()) {
            Ok(pdf) => pdf.pages().len(),
            Err(e) => bail!("renderer cannot open PDF: {e:?}"),
        };
        if page_count != page_ids.len() {
            bail!("page count mismatch ({page_count} vs {})", page_ids.len());
        }

        let pages = (0..page_count)
            .into_par_iter()
            .map_init(|| Pdf::new(data.clone()).ok(), |pdf, idx| render_page(pdf.as_ref()?, idx))
            .collect::<Option<Vec<_>>>();
        let Some(pages) = pages else {
            bail!("failed to render one or more pages");
        };

        Ok(Self { base: strip_pages(doc, &page_ids), page_ids, pages })
    }

    pub fn build(&self, dpi: f32, quality: u8) -> Result<Vec<u8>> {
        let scale = dpi / RENDER_DPI;
        let encoded = self
            .pages
            .par_iter()
            .map(|page| {
                let master = image::load_from_memory_with_format(&page.jpeg, ImageFormat::Jpeg)?;
                encode(&master, quality, scale)
            })
            .collect::<Result<Vec<_>>>()?;

        let mut doc = self.base.clone();
        for ((&page_id, page), enc) in self.page_ids.iter().zip(&self.pages).zip(encoded) {
            let cs: &[u8] = if enc.gray { b"DeviceGray" } else { b"DeviceRGB" };
            let mut image = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => enc.width as i64,
                    "Height" => enc.height as i64,
                    "ColorSpace" => Object::Name(cs.to_vec()),
                    "BitsPerComponent" => 8,
                    "Filter" => "DCTDecode",
                },
                enc.data,
            );
            image.allows_compression = false;
            let image_id = doc.add_object(image);

            let (w, h) = (page.width_pt, page.height_pt);
            let ops = format!("q {w} 0 0 {h} 0 0 cm /Im0 Do Q");
            let contents_id = doc.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));

            if let Ok(dict) = doc.get_dictionary_mut(page_id) {
                dict.set("MediaBox", vec![0.into(), 0.into(), w.into(), h.into()]);
                dict.set("Contents", contents_id);
                dict.set(
                    "Resources",
                    dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
                );
            }
        }
        serialize(&mut doc)
    }
}

fn render_page(pdf: &Pdf, idx: usize) -> Option<RenderedPage> {
    let page = pdf.pages().get(idx)?;
    let (width_pt, height_pt) = page.render_dimensions();
    let scale = RENDER_DPI / 72.0;
    let pixmap = render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE },
    );
    let (w, h) = (pixmap.width() as u32, pixmap.height() as u32);

    // Background is opaque white, so premultiplied RGBA equals straight RGBA.
    let rgb: Vec<u8> = pixmap
        .data_as_u8_slice()
        .chunks_exact(4)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();

    let image = if is_grayscale(&rgb) {
        DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, rgb.iter().step_by(3).copied().collect())?)
    } else {
        DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, rgb)?)
    };
    let jpeg = encode(&image, 92, 1.0).ok()?.data;
    Some(RenderedPage { width_pt, height_pt, jpeg })
}

/// A page counts as grayscale when few pixels have visible color,
/// which tolerates scanner noise and tinted paper.
fn is_grayscale(rgb: &[u8]) -> bool {
    let colored = rgb
        .chunks_exact(3)
        .filter(|px| {
            let (r, g, b) = (px[0] as i16, px[1] as i16, px[2] as i16);
            (r - g).abs().max((g - b).abs()).max((r - b).abs()) > 28
        })
        .count();
    colored * 20 < rgb.len() / 3
}

/// Clear every page's content and geometry entries (rewritten in `build`), then
/// drop everything no longer referenced. Outlines and metadata are preserved.
fn strip_pages(doc: &Document, page_ids: &[ObjectId]) -> Document {
    let mut doc = doc.clone();
    for &id in page_ids {
        if let Ok(dict) = doc.get_dictionary_mut(id) {
            for key in [
                "Contents", "Resources", "Annots", "CropBox", "BleedBox", "TrimBox", "ArtBox",
                "Group", "Thumb", "PieceInfo", "StructParents",
            ] {
                dict.remove(key.as_bytes());
            }
            dict.set("Rotate", 0);
        }
    }
    // Structure tree and form fields point into the removed content.
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"StructTreeRoot");
        catalog.remove(b"AcroForm");
        catalog.remove(b"MarkInfo");
    }
    doc.prune_objects();
    doc
}
