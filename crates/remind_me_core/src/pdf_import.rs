//! PDF import: per-page text extraction feeding the existing chunker.
//!
//! # Behind an optional feature
//!
//! A PDF parser is 71 transitive crates, and most builds do not want one. The
//! `pdf` feature gates it, mirroring the reference's own lazily-imported
//! extra. With the feature off, a `.pdf` import reports the format as
//! unavailable with an actionable message rather than failing obscurely or —
//! worse — succeeding with nothing.
//!
//! # Per page, not per document
//!
//! Each page is chunked separately and carries `{"page": N}`. A page is the
//! only positional anchor a PDF reliably has, and it is what lets a search hit
//! be found again in the original. Extracting the document as one string would
//! save a little code and lose that entirely.
//!
//! # A PDF with no extractable text is refused, not imported as nothing
//!
//! A pure scan — a photographed page, an image-only export — parses fine and
//! yields empty text on every page. Recorded as a successful import of zero
//! memories, that is indistinguishable from importing an empty file, which is
//! precisely the silent failure issue #147 fixed for JSONL transcripts. It
//! says what happened and points at OCR instead.

/// Default category, kept distinct so a search can filter on PDFs.
pub const PDF_CATEGORY: &str = "pdf";

/// `memories.source` for PDF imports.
///
/// Distinct from `document_import`, which is what this originally — and
/// wrongly — recorded. The two are not interchangeable: `normalize` selects on
/// `source IN ('document_import', 'chat_import')`, so filing extracted PDF
/// text as a document silently enrolled it in a rewriting pass the reference
/// deliberately keeps it out of.
pub const PDF_SOURCE: &str = "pdf_import";

/// Told to the caller when the feature is compiled out.
pub const PDF_UNAVAILABLE: &str =
    "PDF import is not available in this build: rebuild with the `pdf` feature \
     enabled (cargo build --features pdf) to import .pdf files.";

/// Told to the caller when a PDF parses but has no text anywhere.
pub const PDF_NO_TEXT: &str =
    "This PDF contains no extractable text — it is most likely a scan or an \
     image-only export. Text extraction cannot help here; OCR would be needed.";

/// One chunk of an imported PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfChunk {
    pub content: String,
    /// 1-based page number, the only positional anchor a PDF reliably has.
    pub page: usize,
}

/// Whether this build can import PDFs at all.
pub fn available() -> bool {
    cfg!(feature = "pdf")
}

/// Extract text per page, in page order.
///
/// A page may legitimately be empty (blank, or image-only); those are the
/// caller's to drop. An unparseable, encrypted or non-PDF file is one
/// consistent error rather than whatever the parser happened to raise.
#[cfg(feature = "pdf")]
pub fn extract_pages(bytes: &[u8]) -> Result<Vec<String>, String> {
    // The parser can panic on some malformed files rather than returning an
    // error. A corrupt attachment must not take down the process that was
    // merely asked to read it.
    let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes))
        .map_err(|_| "Could not parse PDF: the file is malformed".to_string())?;

    extracted.map_err(|e| format!("Could not parse PDF: {}", e))
}

#[cfg(not(feature = "pdf"))]
pub fn extract_pages(_bytes: &[u8]) -> Result<Vec<String>, String> {
    Err(PDF_UNAVAILABLE.to_string())
}

/// Parse a PDF into chunks, each tagged with the page it came from.
///
/// Returns the chunks and the number of pages that actually carried text —
/// distinct from how many chunks the chunker produced, and the honest answer
/// to "how much of this document was readable".
pub fn parse_pdf(bytes: &[u8], max_length: usize) -> Result<(Vec<PdfChunk>, usize), String> {
    let pages = extract_pages(bytes)?;

    let mut chunks = Vec::new();
    let mut pages_with_text = 0usize;

    for (index, page) in pages.iter().enumerate() {
        let text = page.trim();
        if text.is_empty() {
            continue;
        }
        pages_with_text += 1;
        for chunk in crate::importer::chunk_text(text, max_length) {
            chunks.push(PdfChunk {
                content: chunk,
                page: index + 1,
            });
        }
    }

    if chunks.is_empty() {
        return Err(PDF_NO_TEXT.to_string());
    }

    Ok((chunks, pages_with_text))
}
