use anyhow::{Context as _, Result, anyhow};

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

    /// Renders one page to PNG bytes at `scale` pixels per point. Slow for large scales, so call
    /// it on a background thread.
    pub fn render_page(&self, page_index: usize, scale: f32) -> Result<Vec<u8>> {
        let dots_per_inch = f64::from(scale) * 72.0;
        with_rdocx_stack(|| {
            self.document
                .render_page_to_png(page_index, dots_per_inch)
                .map_err(|error| anyhow!("{error}"))
                .with_context(|| format!("Failed to render page {}", page_index + 1))?
                .with_context(|| format!("Page {} does not exist", page_index + 1))
        })
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
            let png = document.render_page(0, scale)?;
            assert!(png.starts_with(&PNG_SIGNATURE));
            let (width, height) = png_size(&png).context("missing PNG header")?;
            assert!((width as f32 - page.width * scale).abs() <= 2.0, "{width}");
            assert!(
                (height as f32 - page.height * scale).abs() <= 2.0,
                "{height}"
            );
        }

        assert!(document.render_page(2, 1.0).is_err());
        Ok(())
    }

    #[test]
    fn test_opens_word_97_documents() -> Result<()> {
        let document = WordDocument::parse(SAMPLE_DOC)?;
        assert_eq!(document.pages().len(), 2);
        assert!(document.render_page(1, 1.0)?.starts_with(&PNG_SIGNATURE));
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
                    document.render_page(0, 2.0)?;
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
