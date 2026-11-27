use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use image::{
    ExtendedColorType, ImageEncoder as _,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};

/// Compound File Binary files, used by Word 97-2003 `.doc` files, start with this signature.
const COMPOUND_FILE_SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// rdocx needs more stack than the 512 KiB that the dispatch queue threads running background
/// tasks have on macOS, so its work runs on a thread of its own. Measured use is about 1 MiB for
/// simple documents in debug builds; deeply nested tables need more. Untouched stack pages are
/// never committed, so a generous size costs no memory.
const RDOCX_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Runs `work` on a thread with [`RDOCX_STACK_SIZE`] of stack and waits for it.
fn with_rdocx_stack<T: Send>(work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("document_viewer".into())
            .stack_size(RDOCX_STACK_SIZE)
            .spawn_scoped(scope, work)
            .context("Failed to start document thread")?
            .join()
            .map_err(|_| anyhow!("Document thread panicked"))?
    })
}

/// Size of a laid out page, in points (1/72 inch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// Colors to show a page with. rdocx always renders black text on white paper, so rendered pages
/// are recolored: white becomes `background`, black becomes `text`, and colors in between keep
/// their hue while their lightness follows the same mapping. Values are RGB from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageColors {
    pub background: [f32; 3],
    pub text: [f32; 3],
}

impl PageColors {
    /// Colors the page as printed, leaving rendered pages unchanged.
    pub const PAPER: Self = Self {
        background: [1.0, 1.0, 1.0],
        text: [0.0, 0.0, 0.0],
    };

    fn map(&self, rgb: [u8; 3]) -> [u8; 3] {
        let channels = rgb.map(|channel| f32::from(channel) / 255.0);
        let lightness = 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
        let mut mapped = [0; 3];
        for (index, mapped) in mapped.iter_mut().enumerate() {
            let text = self.text[index];
            let gray = text + (self.background[index] - text) * lightness;
            let channel = gray + channels[index] - lightness;
            *mapped = (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        mapped
    }

    fn recolor(&self, png: Vec<u8>) -> Result<Vec<u8>> {
        if *self == Self::PAPER {
            return Ok(png);
        }
        let mut image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .context("Failed to decode rendered page")?
            .into_rgba8();
        let mut cache: HashMap<[u8; 3], [u8; 3]> = HashMap::new();
        for pixel in image.pixels_mut() {
            let [red, green, blue, alpha] = pixel.0;
            // Pages are mostly a few colors, so mapping each color once is much faster.
            let [red, green, blue] = *cache
                .entry([red, green, blue])
                .or_insert_with(|| self.map([red, green, blue]));
            pixel.0 = [red, green, blue, alpha];
        }

        let mut recolored = Vec::new();
        PngEncoder::new_with_quality(&mut recolored, CompressionType::Fast, FilterType::NoFilter)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .context("Failed to encode rendered page")?;
        Ok(recolored)
    }
}

/// A Word document that has been laid out into pages. Pages are rendered on demand, because a
/// long document has far more pages than are ever on screen at once.
pub struct WordDocument {
    /// Boxed so that moving a `WordDocument` between threads stays cheap; see
    /// `test_document_is_cheap_to_move`.
    document: Box<rdocx::Document>,
    pages: Vec<PageSize>,
    warnings: Vec<String>,
}

impl WordDocument {
    /// Parses and lays out a `.docx` file, or a Word 97-2003 `.doc` file. The format is detected
    /// from the content rather than the file extension, so a misnamed file still opens.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        with_rdocx_stack(|| Self::parse_on_current_thread(bytes))
    }

    fn parse_on_current_thread(bytes: &[u8]) -> Result<Self> {
        let document = if bytes.starts_with(&COMPOUND_FILE_SIGNATURE) {
            // The layout engine reads only `.docx`, so legacy documents are converted first.
            let legacy = rwml::Document::open(bytes)
                .map_err(|error| anyhow!("{error}"))
                .context("Failed to read Word 97-2003 document")?;
            rdocx::Document::from_bytes(&legacy.to_docx())
                .map(Box::new)
                .map_err(|error| anyhow!("{error}"))
                .context("Failed to convert Word 97-2003 document")?
        } else {
            rdocx::Document::from_bytes(bytes)
                .map(Box::new)
                .map_err(|error| anyhow!("{error}"))
                .context("Failed to read Word document")?
        };

        let layout = document
            .layout()
            .map_err(|error| anyhow!("{error}"))
            .context("Failed to lay out Word document")?;
        let pages = layout
            .layout
            .pages
            .iter()
            .map(|page| PageSize {
                width: page.width as f32,
                height: page.height as f32,
            })
            .collect();
        let warnings = layout
            .layout
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect();

        Ok(Self {
            document,
            pages,
            warnings,
        })
    }

    pub fn pages(&self) -> &[PageSize] {
        &self.pages
    }

    /// Parts of the document that could only be shown approximately, or not at all.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Renders one page to PNG bytes at `scale` pixels per point, in `colors`. Slow for large
    /// scales, so call it on a background thread.
    pub fn render_page(
        &self,
        page_index: usize,
        scale: f32,
        colors: PageColors,
    ) -> Result<Vec<u8>> {
        let dots_per_inch = f64::from(scale) * 72.0;
        let png = with_rdocx_stack(|| {
            self.document
                .render_page_to_png(page_index, dots_per_inch)
                .map_err(|error| anyhow!("{error}"))
                .with_context(|| format!("Failed to render page {}", page_index + 1))?
                .with_context(|| format!("Page {} does not exist", page_index + 1))
        })?;
        colors.recolor(png)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DOCX: &[u8] = include_bytes!("../test_data/sample.docx");
    const SAMPLE_DOC: &[u8] = include_bytes!("../test_data/sample.doc");

    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    /// Reads the width and height from the IHDR chunk that follows the PNG signature.
    fn png_size(png: &[u8]) -> Option<(u32, u32)> {
        let width = png.get(16..20)?.try_into().ok()?;
        let height = png.get(20..24)?.try_into().ok()?;
        Some((u32::from_be_bytes(width), u32::from_be_bytes(height)))
    }

    #[test]
    fn test_lays_out_docx_into_pages() -> Result<()> {
        let document = WordDocument::parse(SAMPLE_DOCX)?;
        assert_eq!(document.pages().len(), 2);
        for page in document.pages() {
            // A4, which the fixture sets in its section properties.
            assert!((page.width - 595.3).abs() < 1.0, "{page:?}");
            assert!((page.height - 841.9).abs() < 1.0, "{page:?}");
        }
        Ok(())
    }

    #[test]
    fn test_renders_pages_at_the_requested_scale() -> Result<()> {
        let document = WordDocument::parse(SAMPLE_DOCX)?;
        let page = document.pages()[0];

        for scale in [1.0, 2.0] {
            let png = document.render_page(0, scale, PageColors::PAPER)?;
            assert!(png.starts_with(&PNG_SIGNATURE));
            let (width, height) = png_size(&png).context("missing PNG header")?;
            assert!((width as f32 - page.width * scale).abs() <= 2.0, "{width}");
            assert!(
                (height as f32 - page.height * scale).abs() <= 2.0,
                "{height}"
            );
        }

        assert!(document.render_page(2, 1.0, PageColors::PAPER).is_err());
        Ok(())
    }

    fn pixel(png: &[u8], x: u32, y: u32) -> Result<[u8; 3]> {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgb8();
        Ok(image.get_pixel(x, y).0)
    }

    #[test]
    fn test_recolors_pages_with_the_theme_colors() -> Result<()> {
        let document = WordDocument::parse(SAMPLE_DOCX)?;
        let dark = PageColors {
            background: [0.2, 0.4, 0.6],
            text: [0.9, 0.9, 0.8],
        };
        let png = document.render_page(0, 1.0, dark)?;
        // The top left corner is in the page margin, which Word documents leave white.
        assert_eq!(pixel(&png, 2, 2)?, [51, 102, 153]);

        let png = document.render_page(0, 1.0, PageColors::PAPER)?;
        assert_eq!(pixel(&png, 2, 2)?, [255, 255, 255]);
        Ok(())
    }

    #[test]
    fn test_recoloring_maps_white_to_background_and_black_to_text() {
        let colors = PageColors {
            background: [0.0, 0.0, 0.2],
            text: [1.0, 0.8, 0.6],
        };
        assert_eq!(colors.map([255, 255, 255]), [0, 0, 51]);
        assert_eq!(colors.map([0, 0, 0]), [255, 204, 153]);
        assert_eq!(PageColors::PAPER.map([5, 99, 193]), [5, 99, 193]);
    }

    #[test]
    fn test_opens_word_97_documents() -> Result<()> {
        let document = WordDocument::parse(SAMPLE_DOC)?;
        assert_eq!(document.pages().len(), 2);
        assert!(
            document
                .render_page(1, 1.0, PageColors::PAPER)?
                .starts_with(&PNG_SIGNATURE)
        );
        Ok(())
    }

    /// `rdocx::Document` is about 28 KiB, and debug builds keep a copy of a returned value in
    /// each frame it passes through. Moved by value through the background executor, that alone
    /// overflowed the 512 KiB stack of a dispatch queue thread, so it must stay boxed.
    #[test]
    fn test_document_is_cheap_to_move() {
        assert!(std::mem::size_of::<WordDocument>() <= 256);
    }

    /// Background tasks run on dispatch queue threads, which on macOS have 512 KiB stacks, much
    /// less than the threads tests run on.
    #[test]
    fn test_works_on_threads_with_small_stacks() -> Result<()> {
        let result = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| -> Result<()> {
                for bytes in [SAMPLE_DOCX, SAMPLE_DOC] {
                    let document = WordDocument::parse(bytes)?;
                    document.render_page(0, 2.0, PageColors::PAPER)?;
                }
                Ok(())
            })?
            .join();
        result.map_err(|_| anyhow!("the small stack thread panicked"))?
    }

    #[test]
    fn test_rejects_invalid_documents() {
        assert!(WordDocument::parse(b"not a document").is_err());

        let mut broken_doc = COMPOUND_FILE_SIGNATURE.to_vec();
        broken_doc.extend_from_slice(&[0; 64]);
        assert!(WordDocument::parse(&broken_doc).is_err());
    }
}
