//! Image import via OCR (gap I2, issue #156).
//!
//! Split by feature the same way the PDF tests are: the **feature-off**
//! behaviour is what most builds ship and is asserted unconditionally. What
//! CI cannot assert either way is a real recognition — that needs a model
//! CI has no business downloading — so the feature-on tests here cover the
//! configuration surface and the refusal paths, which is honestly the whole
//! of what is checkable without one.

use remind_me_core::image_import;
use remind_me_core::models::{ImportKind, ImportOutcome};
use remind_me_core::Database;

/// Model paths are process-wide env vars, so these run one at a time.
///
/// Every test that *reads* the environment takes this too, not just the
/// ones that write it: `import()` opens a `Database`, which reads its
/// configuration from env vars, and glibc's `getenv` racing another
/// thread's `setenv`/`unsetenv` can read a freed `environ` and SIGSEGV the
/// whole test binary (seen in CI, and about 1 run in 400 locally).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_model_env() {
    std::env::remove_var(image_import::DETECTION_MODEL_ENV);
    std::env::remove_var(image_import::RECOGNITION_MODEL_ENV);
}

fn import(bytes: &[u8], filename: &str, kind: ImportKind) -> ImportOutcome {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    remind_me_core::importer::import_bytes(
        &conn,
        bytes,
        filename,
        "",
        &[],
        "all_messages",
        2000,
        kind,
    )
    .unwrap()
}

fn refusal(outcome: ImportOutcome) -> String {
    match outcome {
        ImportOutcome::Failed { reason, .. } => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn availability_matches_the_compiled_feature() {
    assert_eq!(image_import::available(), cfg!(feature = "ocr"));
}

// ---------------------------------------------------------------------------
// Routing: the suffix decides, in both directions
// ---------------------------------------------------------------------------

#[test]
fn every_image_suffix_is_accepted_as_a_supported_format() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for name in ["shot.png", "shot.jpg", "shot.jpeg"] {
        let reason = refusal(import(b"not really an image", name, ImportKind::Auto));
        // Whatever else goes wrong, it must not be "we don't handle .png".
        assert!(
            !reason.contains("unsupported format"),
            "{name} was rejected as an unsupported format: {reason}"
        );
    }
}

#[test]
fn an_image_import_is_refused_for_a_non_image_file() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let reason = refusal(import(b"# notes", "notes.md", ImportKind::Image));
    assert!(reason.contains("image import does not support"), "{reason}");
}

#[test]
fn an_image_cannot_be_forced_through_a_text_parser() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The bytes have no text reading at all, so parsing them as prose would
    // produce a memory full of lossily-decoded binary rather than an error.
    let reason = refusal(import(b"\x89PNG\r\n", "shot.png", ImportKind::Document));
    assert!(reason.contains("must be imported as an image"), "{reason}");
}

// ---------------------------------------------------------------------------
// Configuration — checkable with no model and no feature
// ---------------------------------------------------------------------------

#[test]
fn unconfigured_models_name_both_variables_and_refuse_to_download() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_model_env();

    let err = image_import::model_paths().unwrap_err();

    // Someone who has just enabled the feature has no way to guess that two
    // separate models are needed, so the message has to say so.
    assert!(err.contains(image_import::DETECTION_MODEL_ENV), "{err}");
    assert!(err.contains(image_import::RECOGNITION_MODEL_ENV), "{err}");
    // And it must not read as though the import will sort itself out.
    assert!(err.contains("explicit step"), "{err}");
}

#[test]
fn half_configured_models_are_as_refused_as_none() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_model_env();
    std::env::set_var(image_import::DETECTION_MODEL_ENV, "/tmp/detection.rten");

    // Detection alone finds where text is and cannot read a character of it.
    // Proceeding on a partial configuration would OCR every image to nothing,
    // which looks exactly like an image with no text in it.
    assert!(image_import::model_paths().is_err());

    clear_model_env();
}

#[test]
fn a_model_path_that_does_not_exist_names_the_variable_not_just_the_file() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_model_env();
    std::env::set_var(image_import::DETECTION_MODEL_ENV, "/nonexistent/det.rten");
    std::env::set_var(image_import::RECOGNITION_MODEL_ENV, "/nonexistent/rec.rten");

    let err = image_import::model_paths().unwrap_err();
    assert!(err.contains(image_import::DETECTION_MODEL_ENV), "{err}");
    assert!(err.contains("/nonexistent/det.rten"), "{err}");

    clear_model_env();
}

#[test]
fn a_configured_pair_of_real_files_is_accepted() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_model_env();

    // Not real models — this asserts the *configuration* check passes them
    // through to the loader, which is where a fake model is caught.
    let dir = std::env::temp_dir().join("remind_me_ocr_model_paths_test");
    std::fs::create_dir_all(&dir).unwrap();
    let detection = dir.join("det.rten");
    let recognition = dir.join("rec.rten");
    std::fs::write(&detection, b"placeholder").unwrap();
    std::fs::write(&recognition, b"placeholder").unwrap();

    std::env::set_var(image_import::DETECTION_MODEL_ENV, &detection);
    std::env::set_var(image_import::RECOGNITION_MODEL_ENV, &recognition);

    let (got_detection, got_recognition) = image_import::model_paths().unwrap();
    assert_eq!(got_detection, detection);
    assert_eq!(got_recognition, recognition);

    clear_model_env();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Feature off — the configuration most builds ship
// ---------------------------------------------------------------------------

#[cfg(not(feature = "ocr"))]
mod without_the_feature {
    use super::*;

    #[test]
    fn ocr_reports_the_feature_is_missing_and_names_the_flag() {
        let err = image_import::parse_image(b"\x89PNG\r\n", 2000).unwrap_err();

        // Actionable: it points at the build, not at the file. "unsupported
        // format" would send someone looking for a different image.
        assert!(err.contains("--features ocr"), "{err}");
    }

    #[test]
    fn importing_an_image_fails_loudly_rather_than_succeeding_with_nothing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let reason = refusal(import(
            b"\x89PNG\r\n\x1a\n",
            "receipt.png",
            ImportKind::Auto,
        ));

        assert!(reason.contains("--features ocr"), "{reason}");
    }
}

// ---------------------------------------------------------------------------
// Feature on — everything checkable without a downloaded model
// ---------------------------------------------------------------------------

#[cfg(feature = "ocr")]
mod with_the_feature {
    use super::*;

    #[test]
    fn an_import_with_no_model_configured_refuses_rather_than_downloading_one() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_model_env();

        // The single most important feature-on assertion CI can make: turning
        // OCR on must not turn an import into a several-hundred-megabyte
        // download nobody asked for.
        let reason = refusal(import(
            b"\x89PNG\r\n\x1a\n",
            "receipt.png",
            ImportKind::Auto,
        ));

        assert!(
            reason.contains(image_import::DETECTION_MODEL_ENV),
            "{reason}"
        );
        assert!(reason.contains("explicit step"), "{reason}");
    }

    #[test]
    fn a_bad_model_file_is_an_error_about_the_model() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_model_env();

        let dir = std::env::temp_dir().join("remind_me_ocr_bad_model_test");
        std::fs::create_dir_all(&dir).unwrap();
        let detection = dir.join("det.rten");
        let recognition = dir.join("rec.rten");
        std::fs::write(&detection, b"not a model").unwrap();
        std::fs::write(&recognition, b"not a model").unwrap();
        std::env::set_var(image_import::DETECTION_MODEL_ENV, &detection);
        std::env::set_var(image_import::RECOGNITION_MODEL_ENV, &recognition);

        let err = image_import::parse_image(b"\x89PNG\r\n\x1a\n", 2000).unwrap_err();
        // Names the file that is wrong, rather than blaming the image.
        assert!(err.contains("det.rten"), "{err}");

        clear_model_env();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
