//! PDF import (gap I1, issue #153).
//!
//! Split by feature deliberately. The **feature-off** tests run
//! unconditionally, because that is the configuration most builds ship and the
//! one whose behaviour — a clear refusal rather than a crash or a silent
//! success — matters most. The extraction tests only run when the parser is
//! actually compiled in.

use remind_me_core::models::ImportKind;
use remind_me_core::pdf_import;
use remind_me_core::Database;

#[test]
fn availability_matches_the_compiled_feature() {
    assert_eq!(pdf_import::available(), cfg!(feature = "pdf"));
}

#[test]
fn a_pdf_import_is_refused_for_a_non_pdf_file() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let outcome = remind_me_core::importer::import_bytes(
        &conn,
        b"# not a pdf",
        "notes.md",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Pdf,
    )
    .unwrap();

    match outcome {
        remind_me_core::models::ImportOutcome::Failed { reason, .. } => {
            assert!(reason.contains("pdf import does not support"), "{reason}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Feature off — the configuration most builds ship
// ---------------------------------------------------------------------------

#[cfg(not(feature = "pdf"))]
mod without_the_feature {
    use super::*;

    #[test]
    fn extraction_reports_the_feature_is_missing() {
        let err = pdf_import::extract_pages(b"%PDF-1.4").unwrap_err();

        // Actionable: it names the flag to rebuild with. "unsupported format"
        // would send someone looking at their file instead of their build.
        assert!(err.contains("--features pdf"), "{err}");
    }

    #[test]
    fn importing_a_pdf_fails_loudly_rather_than_succeeding_with_nothing() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();

        let outcome = remind_me_core::importer::import_bytes(
            &conn,
            b"%PDF-1.4 whatever",
            "paper.pdf",
            "",
            &[],
            "all_messages",
            2000,
            ImportKind::Auto,
        )
        .unwrap();

        match outcome {
            remind_me_core::models::ImportOutcome::Failed { reason, .. } => {
                assert!(reason.contains("pdf` feature"), "{reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // And nothing was stored, so a later search cannot turn up a phantom.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

// ---------------------------------------------------------------------------
// Feature on
// ---------------------------------------------------------------------------

#[cfg(feature = "pdf")]
mod with_the_feature {
    use super::*;
    use remind_me_core::pdf_import::parse_pdf;

    /// A minimal single-page PDF with one text-showing operator.
    fn one_page_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({}) Tj ET", text);
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();

        let objects = [
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_string(),
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>\nendobj\n"
                .to_string(),
            "4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n".to_string(),
            format!(
                "5 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                stream.len(),
                stream
            ),
        ];
        for object in &objects {
            offsets.push(pdf.len());
            pdf.push_str(object);
        }

        let xref_at = pdf.len();
        pdf.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in &offsets {
            pdf.push_str(&format!("{:010} 00000 n \n", offset));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_at
        ));
        pdf.into_bytes()
    }

    #[test]
    fn text_is_extracted_and_tagged_with_its_page() {
        let (chunks, pages) = parse_pdf(&one_page_pdf("Hello from page one"), 2000).unwrap();

        assert_eq!(pages, 1);
        assert!(chunks[0].content.contains("Hello from page one"));
        // A page is the only positional anchor a PDF reliably has, and it is
        // what lets a search hit be found again in the original.
        assert_eq!(chunks[0].page, 1);
    }

    #[test]
    fn a_pdf_with_no_text_is_refused_rather_than_imported_as_nothing() {
        // A pure scan parses fine and yields empty text on every page.
        // Recorded as a successful import of zero memories, that is
        // indistinguishable from importing an empty file.
        let err = parse_pdf(&one_page_pdf(""), 2000).unwrap_err();
        assert!(err.contains("no extractable text"), "{err}");
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_panic() {
        let err = parse_pdf(b"%PDF-1.4\nnot really a pdf at all", 2000).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn a_pdf_imports_end_to_end() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();

        remind_me_core::importer::import_bytes(
            &conn,
            &one_page_pdf("Imported through the real path"),
            "paper.pdf",
            "",
            &[],
            "all_messages",
            2000,
            ImportKind::Auto,
        )
        .unwrap();

        let (content, category, metadata): (String, String, String) = conn
            .query_row(
                "SELECT content, category, metadata FROM memories",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert!(content.contains("Imported through the real path"));
        assert_eq!(category, "pdf");
        let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(metadata["page"], 1);
    }
}
