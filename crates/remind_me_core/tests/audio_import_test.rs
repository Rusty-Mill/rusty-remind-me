//! Audio import via transcription (gap I3, issue #156).
//!
//! CI cannot assert a real transcription — that needs a model CI has no
//! business downloading. What it *can* assert is everything on either side of
//! the model: routing, the refusal paths, and the decode/resample arithmetic
//! that turns a file into the 16 kHz mono signal whisper.cpp requires. That
//! last part is deterministic maths over synthesised audio, so it is tested
//! properly rather than waved through as "part of the untestable feature".

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::audio_import::{self, WHISPER_SAMPLE_RATE};
use remind_me_core::models::{ImportKind, ImportOutcome};
use remind_me_core::Database;

/// The model path is a process-wide env var, so these run one at a time.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// A sine wave, as interleaved mono `f32`.
fn sine(frequency: f64, rate: u32, seconds: f64) -> Vec<f32> {
    let count = (rate as f64 * seconds) as usize;
    (0..count)
        .map(|n| {
            (2.0 * std::f64::consts::PI * frequency * n as f64 / rate as f64).sin() as f32 * 0.5
        })
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

/// How many times the signal crosses zero — a frequency estimate that needs no
/// FFT: a clean sine crosses twice per cycle.
fn zero_crossings(samples: &[f32]) -> usize {
    samples
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count()
}

#[test]
fn availability_matches_the_compiled_feature() {
    assert_eq!(audio_import::available(), cfg!(feature = "audio"));
}

// ---------------------------------------------------------------------------
// Routing: the suffix decides, in both directions
// ---------------------------------------------------------------------------

#[test]
fn every_audio_suffix_is_accepted_as_a_supported_format() {
    for name in ["memo.mp3", "memo.m4a", "memo.wav", "memo.ogg"] {
        let reason = refusal(import(b"not really audio", name, ImportKind::Auto));
        assert!(
            !reason.contains("unsupported format"),
            "{name} was rejected as an unsupported format: {reason}"
        );
    }
}

#[test]
fn an_audio_import_is_refused_for_a_non_audio_file() {
    let reason = refusal(import(b"# notes", "notes.md", ImportKind::Audio));
    assert!(reason.contains("audio import does not support"), "{reason}");
}

#[test]
fn a_recording_cannot_be_forced_through_a_text_parser() {
    let reason = refusal(import(b"ID3\x04", "memo.mp3", ImportKind::Chat));
    assert!(reason.contains("must be imported as audio"), "{reason}");
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn an_unconfigured_model_names_the_variable_and_refuses_to_download() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(audio_import::MODEL_ENV);

    let err = audio_import::model_path().unwrap_err();

    assert!(err.contains(audio_import::MODEL_ENV), "{err}");
    assert!(err.contains("explicit step"), "{err}");
}

#[test]
fn a_model_path_that_does_not_exist_names_the_variable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(audio_import::MODEL_ENV, "/nonexistent/ggml-base.bin");

    let err = audio_import::model_path().unwrap_err();
    assert!(err.contains(audio_import::MODEL_ENV), "{err}");
    assert!(err.contains("/nonexistent/ggml-base.bin"), "{err}");

    crate::test_env::remove_var(audio_import::MODEL_ENV);
}

// ---------------------------------------------------------------------------
// Downmixing and resampling — real behaviour CI can hold to account
// ---------------------------------------------------------------------------

#[test]
fn a_voice_in_one_stereo_channel_survives_the_downmix() {
    // Interleaved stereo: left silent, right carrying the signal. Taking
    // channel 0 rather than averaging would transcribe pure silence.
    let interleaved: Vec<f32> = sine(440.0, 16_000, 0.05)
        .into_iter()
        .flat_map(|s| [0.0, s])
        .collect();

    let mono = audio_import::to_mono(&interleaved, 2);

    assert_eq!(mono.len(), interleaved.len() / 2);
    assert!(rms(&mono) > 0.1, "the downmix silenced the only voice");
}

#[test]
fn audio_already_at_the_target_rate_is_untouched() {
    let samples = sine(440.0, WHISPER_SAMPLE_RATE, 0.05);
    assert_eq!(
        audio_import::resample(&samples, WHISPER_SAMPLE_RATE),
        samples
    );
}

#[test]
fn resampling_produces_the_target_rates_worth_of_samples() {
    let one_second = sine(440.0, 48_000, 1.0);
    let resampled = audio_import::resample(&one_second, 48_000);

    // A second of audio is a second of audio; whisper.cpp derives its
    // timestamps from the sample count, so a length that drifts would move
    // every timestamp in the transcript.
    let drift = (resampled.len() as i64 - WHISPER_SAMPLE_RATE as i64).abs();
    assert!(drift <= 1, "got {} samples", resampled.len());
}

#[test]
fn speech_range_audio_keeps_its_frequency_and_its_level() {
    let original = sine(1_000.0, 48_000, 0.5);
    let resampled = audio_import::resample(&original, 48_000);

    // A 1 kHz tone is well inside the speech band and must come through
    // unharmed: ~1000 cycles in half a second, so ~1000 zero crossings.
    let crossings = zero_crossings(&resampled);
    assert!(
        (950..=1050).contains(&crossings),
        "1 kHz became {crossings} crossings in 0.5s"
    );
    // And it must not be quietly attenuated on the way.
    let ratio = rms(&resampled) / rms(&original);
    assert!(ratio > 0.9 && ratio < 1.1, "level moved by {ratio}");
}

#[test]
fn content_above_the_new_nyquist_is_filtered_out_rather_than_folded_down() {
    // 20 kHz cannot be represented at 16 kHz. Dropping samples to resample
    // would not remove it — it would *alias* it down to 4 kHz, straight into
    // the middle of the speech band as a loud tone that was never in the
    // recording. This is the entire reason resample() filters first.
    let ultrasonic = sine(20_000.0, 48_000, 0.5);
    let resampled = audio_import::resample(&ultrasonic, 48_000);

    let naive: Vec<f32> = ultrasonic.iter().step_by(3).copied().collect();

    assert!(
        rms(&resampled) < 0.05 * rms(&ultrasonic),
        "inaudible content survived at {} of its original level",
        rms(&resampled) / rms(&ultrasonic)
    );
    // The contrast is the point: naive decimation keeps it at full strength,
    // so this test would pass trivially against a filter that did nothing.
    assert!(
        rms(&naive) > 0.5 * rms(&ultrasonic),
        "the comparison case is not actually aliasing"
    );
}

#[test]
fn upsampling_works_too() {
    // A phone recording at 8 kHz is common, and has to be stretched rather
    // than squeezed.
    let original = sine(500.0, 8_000, 0.5);
    let resampled = audio_import::resample(&original, 8_000);

    assert!((resampled.len() as i64 - 8_000).abs() <= 1);
    let crossings = zero_crossings(&resampled);
    assert!(
        (450..=550).contains(&crossings),
        "500 Hz became {crossings} crossings in 0.5s"
    );
}

// ---------------------------------------------------------------------------
// Feature off — the configuration most builds ship
// ---------------------------------------------------------------------------

#[cfg(not(feature = "audio"))]
mod without_the_feature {
    use super::*;

    #[test]
    fn transcription_reports_the_feature_is_missing_and_names_the_flag() {
        let err = audio_import::parse_audio(b"RIFF", 2000).unwrap_err();
        assert!(err.contains("--features audio"), "{err}");
    }

    #[test]
    fn importing_a_recording_fails_loudly_rather_than_succeeding_with_nothing() {
        let reason = refusal(import(b"RIFF....WAVE", "memo.wav", ImportKind::Auto));
        assert!(reason.contains("--features audio"), "{reason}");
    }
}

// ---------------------------------------------------------------------------
// Feature on — everything checkable without a downloaded model
// ---------------------------------------------------------------------------

#[cfg(feature = "audio")]
mod with_the_feature {
    use super::*;

    /// A 16-bit PCM WAV, built by hand so the decode path can be tested
    /// without shipping a binary fixture.
    fn wav(samples: &[f32], rate: u32, channels: u16) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s * i16::MAX as f32) as i16).to_le_bytes())
            .collect();
        let block_align = channels * 2;
        let byte_rate = rate * block_align as u32;

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn an_import_with_no_model_configured_refuses_rather_than_downloading_one() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::remove_var(audio_import::MODEL_ENV);

        // The most important feature-on assertion CI can make: turning audio
        // import on must not turn reading a voice memo into a several-hundred-
        // megabyte download nobody asked for.
        let bytes = wav(&sine(440.0, 44_100, 0.1), 44_100, 1);
        let reason = refusal(import(&bytes, "memo.wav", ImportKind::Auto));

        assert!(reason.contains(audio_import::MODEL_ENV), "{reason}");
        assert!(reason.contains("explicit step"), "{reason}");
    }

    #[test]
    fn a_model_path_that_is_not_a_model_is_an_error_about_the_model() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join("remind_me_audio_bad_model.bin");
        std::fs::write(&path, b"not a whisper model").unwrap();
        crate::test_env::set_var(audio_import::MODEL_ENV, &path);

        let bytes = wav(&sine(440.0, 16_000, 0.1), 16_000, 1);
        let err = audio_import::parse_audio(&bytes, 2000).unwrap_err();
        assert!(err.to_lowercase().contains("model"), "{err}");

        crate::test_env::remove_var(audio_import::MODEL_ENV);
        let _ = std::fs::remove_file(&path);
    }
}
