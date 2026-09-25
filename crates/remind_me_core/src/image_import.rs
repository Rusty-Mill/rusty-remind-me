//! Image import: OCR a `.png`/`.jpg`/`.jpeg` into text for the normal chunker.
//!
//! # Behind an optional feature, like every other model runtime here
//!
//! OCR means shipping a neural-network runtime and its weights, and most
//! builds do not want one. The `ocr` feature gates it, mirroring the
//! reference's lazily-imported `image` extra. Feature-off, a `.png` import
//! reports the format as unavailable and names the flag, rather than failing
//! obscurely or — worse — succeeding with nothing.
//!
//! # Why `ocrs`/`rten` rather than an ONNX Runtime binding
//!
//! The reference uses RapidOCR because this codebase already had ONNX
//! Runtime present for its embedder. That reasoning does not transfer: the
//! Rust ONNX binding does not carry a runtime, it **downloads one at run
//! time** on first use. This module must never trigger an implicit download,
//! so a runtime whose whole install strategy is an implicit download is the
//! wrong shape regardless of its merits.
//!
//! `ocrs` is pure Rust, compiles in under a minute with no C++ toolchain, and
//! takes its two models as **explicit file paths** — which is exactly the
//! contract wanted here rather than something worked around.
//!
//! # The models are configuration, and their absence is a clear error
//!
//! This is the one place this port is materially less convenient than the
//! reference: RapidOCR's models ship inside its Python wheel, so the reference
//! needs no configuration at all. `ocrs`'s models are separate files, so
//! [`DETECTION_MODEL_ENV`] and [`RECOGNITION_MODEL_ENV`] must point at them.
//!
//! Unset, an import fails with a message that says which variables to set and
//! where the models come from. Unreadable, it says which path failed. Neither
//! case downloads anything, and neither is silent — an OCR feature that
//! quietly returns nothing when its model is missing looks exactly like an
//! image that genuinely had no text in it.
//!
//! # One memory per image
//!
//! The recognised lines are joined into a single blob and chunked as one
//! document, matching the reference's deliberate choice not to make a memory
//! per detected text region. A text region is a layout artefact — a line, a
//! word, a table cell — not a unit of meaning, and one memory per region
//! would shred a paragraph into unsearchable fragments.
//!
//! # An image with no text is refused, not imported as nothing
//!
//! A photograph with no writing in it OCRs successfully to an empty string.
//! Recorded as a successful import of zero memories, that is indistinguishable
//! from importing an empty file — the silent failure #147 fixed for JSONL
//! transcripts and #153 fixed for scanned PDFs. It is refused here for the
//! same reason.

/// Default category, kept distinct so a search can filter on images.
pub const IMAGE_CATEGORY: &str = "image";
/// `memories.source` for OCR imports.
pub const IMAGE_SOURCE: &str = "image_import";

/// Path to the `ocrs` text **detection** model — finds where the words are.
pub const DETECTION_MODEL_ENV: &str = "REMIND_ME_OCR_DET_MODEL_PATH";
/// Path to the `ocrs` text **recognition** model — reads what they say.
pub const RECOGNITION_MODEL_ENV: &str = "REMIND_ME_OCR_REC_MODEL_PATH";

/// Told to the caller when the feature is compiled out.
pub const IMAGE_UNAVAILABLE: &str =
    "Image import (OCR) is not available in this build: rebuild with the `ocr` \
     feature enabled (cargo build --features ocr) to import .png, .jpg and \
     .jpeg files.";

/// Told to the caller when the feature is on but the models are not configured.
///
/// Deliberately names both variables and says where the files come from: the
/// most likely reader of this message has just turned the feature on and has
/// no way to guess that two separate models are needed.
pub const IMAGE_NO_MODEL: &str =
    "Image import (OCR) needs its two models, and neither is downloaded \
     automatically. Set REMIND_ME_OCR_DET_MODEL_PATH to a text-detection model \
     and REMIND_ME_OCR_REC_MODEL_PATH to a text-recognition model — the .rten \
     models published by the ocrs project. Downloading a model is an explicit \
     step, never something an import does on your behalf.";

/// Told to the caller when an image parses but holds no recognisable text.
pub const IMAGE_NO_TEXT: &str =
    "No text was recognised in this image. If it does contain writing, it may \
     be too small, too low-contrast or at too steep an angle to read.";

/// Whether this build can OCR at all.
pub fn available() -> bool {
    cfg!(feature = "ocr")
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// The configured model paths, or the reason they are unusable.
///
/// A pure function of the environment, so the "models not configured" decision
/// — the one a user is most likely to hit — is testable without the feature
/// compiled in, without a model on disk, and without an image.
pub fn model_paths() -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let detection = env(DETECTION_MODEL_ENV);
    let recognition = env(RECOGNITION_MODEL_ENV);
    if detection.is_empty() || recognition.is_empty() {
        return Err(IMAGE_NO_MODEL.to_string());
    }
    let (detection, recognition) = (
        std::path::PathBuf::from(detection),
        std::path::PathBuf::from(recognition),
    );
    // Checked here rather than left to the loader so the message names the
    // variable that is wrong, not just the file that is missing.
    for (var, path) in [
        (DETECTION_MODEL_ENV, &detection),
        (RECOGNITION_MODEL_ENV, &recognition),
    ] {
        if !path.exists() {
            return Err(format!(
                "{} points at {}, which does not exist.",
                var,
                path.display()
            ));
        }
    }
    Ok((detection, recognition))
}

#[cfg(feature = "ocr")]
mod backend {
    use super::*;
    use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
    use std::sync::{Arc, Mutex, OnceLock};

    /// The loaded engine, remembered along with the paths it was built from.
    ///
    /// Loading two models per image would dominate the cost of a directory
    /// sweep — the case this most needs to be quick. Keyed on the paths rather
    /// than loaded once forever (as the reference's singleton is) because
    /// here the models *are* configuration and can legitimately change; a
    /// singleton would silently keep using the old one.
    type Cached = (std::path::PathBuf, std::path::PathBuf, Arc<OcrEngine>);
    fn cache() -> &'static Mutex<Option<Cached>> {
        static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(None))
    }

    fn engine() -> Result<Arc<OcrEngine>, String> {
        let (detection_path, recognition_path) = model_paths()?;

        let mut cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((det, rec, engine)) = cached.as_ref() {
            if det == &detection_path && rec == &recognition_path {
                return Ok(Arc::clone(engine));
            }
        }

        let load = |path: &std::path::Path| {
            rten::Model::load_file(path)
                .map_err(|e| format!("could not load OCR model {}: {}", path.display(), e))
        };
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(load(&detection_path)?),
            recognition_model: Some(load(&recognition_path)?),
            ..Default::default()
        })
        .map_err(|e| format!("could not start the OCR engine: {}", e))?;

        let engine = Arc::new(engine);
        *cached = Some((detection_path, recognition_path, Arc::clone(&engine)));
        Ok(engine)
    }

    /// OCR an image's bytes into one text blob.
    pub fn extract_text(bytes: &[u8]) -> Result<String, String> {
        let engine = engine()?;

        // Decoded before the engine is asked for anything, so "this is not a
        // decodable image" is its own error rather than an opaque OCR failure.
        let image = image::load_from_memory(bytes)
            .map_err(|e| format!("Could not read image: {}", e))?
            .into_rgb8();
        let source = ImageSource::from_bytes(image.as_raw(), image.dimensions())
            .map_err(|e| format!("Could not read image: {}", e))?;

        let input = engine
            .prepare_input(source)
            .map_err(|e| format!("Could not OCR image: {}", e))?;
        engine
            .get_text(&input)
            .map_err(|e| format!("Could not OCR image: {}", e))
    }
}

#[cfg(not(feature = "ocr"))]
mod backend {
    pub fn extract_text(_bytes: &[u8]) -> Result<String, String> {
        Err(super::IMAGE_UNAVAILABLE.to_string())
    }
}

/// OCR an image into chunks ready for the importer.
///
/// Returns the chunks and the number of recognised lines — the honest answer
/// to "how much of this image was readable", and the counterpart of the page
/// count a PDF import reports.
pub fn parse_image(bytes: &[u8], max_length: usize) -> Result<(Vec<String>, usize), String> {
    let text = backend::extract_text(bytes)?;

    let text = text.trim();
    if text.is_empty() {
        return Err(IMAGE_NO_TEXT.to_string());
    }
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();

    let chunks = crate::importer::chunk_text(text, max_length);
    if chunks.is_empty() {
        return Err(IMAGE_NO_TEXT.to_string());
    }
    Ok((chunks, lines))
}
