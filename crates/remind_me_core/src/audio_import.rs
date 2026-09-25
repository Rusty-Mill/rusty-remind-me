//! Audio import: transcribe `.mp3`/`.m4a`/`.wav`/`.ogg` into timestamped text.
//!
//! # Behind an optional feature
//!
//! `whisper-rs` compiles whisper.cpp from source, which most builds do not
//! want to pay for. The `audio` feature gates it, mirroring the reference's
//! lazily-imported `audio` extra. Feature-off, an `.mp3` import reports the
//! format as unavailable and names the flag.
//!
//! # No system `ffmpeg`, decided the same way the reference decided it
//!
//! whisper.cpp takes 16 kHz mono `f32` samples and decodes nothing, so a
//! decoder is needed. `symphonia` is pure Rust and handles all four required
//! containers in-process. That matters for the same reason it mattered in the
//! reference, which rejected `pywhispercpp` precisely because it shelled out
//! to a system `ffmpeg` binary: a dependency that is not installable by the
//! build is a dependency that is absent exactly when someone needs it.
//!
//! # Resampling is done here, and deliberately not naively
//!
//! Almost no real recording is already 16 kHz. Dropping samples to get there
//! folds everything above 8 kHz back down into the speech band as noise, which
//! degrades transcription in a way that looks like a bad model rather than a
//! bad resampler. [`resample`] low-passes with a windowed sinc first.
//!
//! It is also the one part of this module CI can genuinely test: decoding and
//! resampling are deterministic arithmetic over synthesised audio, needing no
//! model and no network. The transcription itself cannot be tested that way,
//! so the parts that *can* be are worth keeping honest.
//!
//! # The model is explicit, and there is no download
//!
//! The reference downloads a Whisper model from HuggingFace on first use.
//! whisper.cpp takes a GGML file path instead, and that is the better
//! contract: [`MODEL_ENV`] points at the file, and an import never fetches
//! several hundred megabytes on someone's behalf as a side effect of reading
//! a voice memo.
//!
//! # Per segment, with its timestamps
//!
//! Whisper's own output unit is a timestamped segment, and each becomes a
//! chunk carrying `{"start": <seconds>, "end": <seconds>}` — the positional
//! anchor that lets a search hit be found again in the recording, exactly as a
//! PDF chunk carries its page. An oversized segment is split by the shared
//! chunker with every part keeping the whole segment's range, matching what
//! the PDF path does for an oversized page.
//!
//! # Silence is refused, not imported as nothing
//!
//! A recording with no speech transcribes to no segments. Recorded as a
//! successful import of zero memories, that is indistinguishable from an empty
//! file — #147's failure mode again, and refused here as it is there.

/// What whisper.cpp requires: 16 kHz, mono.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

/// Default category, kept distinct so a search can filter on recordings.
pub const AUDIO_CATEGORY: &str = "audio";
/// `memories.source` for transcription imports.
pub const AUDIO_SOURCE: &str = "audio_import";

/// Path to a Whisper GGML model file.
pub const MODEL_ENV: &str = "REMIND_ME_AUDIO_MODEL_PATH";

/// Told to the caller when the feature is compiled out.
pub const AUDIO_UNAVAILABLE: &str =
    "Audio import is not available in this build: rebuild with the `audio` \
     feature enabled (cargo build --features audio) to import .mp3, .m4a, .wav \
     and .ogg files.";

/// Told to the caller when the feature is on but no model is configured.
pub const AUDIO_NO_MODEL: &str = "Audio import needs a Whisper model, and it is not downloaded \
     automatically. Set REMIND_ME_AUDIO_MODEL_PATH to a GGML model file (the \
     ggml-base.bin published by the whisper.cpp project is a good default). \
     Downloading a model is an explicit step, never something an import does \
     on your behalf.";

/// Told to the caller when a file decodes but holds no speech.
pub const AUDIO_NO_SPEECH: &str =
    "No speech was transcribed from this recording. It may be silent, or too \
     quiet or noisy to transcribe.";

/// One transcribed segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    /// Seconds from the start of the recording.
    pub start: f64,
    pub end: f64,
}

/// Whether this build can transcribe at all.
pub fn available() -> bool {
    cfg!(feature = "audio")
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// The configured model path, or the reason it is unusable.
///
/// A pure function of the environment, so the "no model configured" case is
/// testable without the feature compiled in and without a model on disk.
pub fn model_path() -> Result<std::path::PathBuf, String> {
    let configured = env(MODEL_ENV);
    if configured.is_empty() {
        return Err(AUDIO_NO_MODEL.to_string());
    }
    let path = std::path::PathBuf::from(configured);
    if !path.exists() {
        return Err(format!(
            "{} points at {}, which does not exist.",
            MODEL_ENV,
            path.display()
        ));
    }
    Ok(path)
}

/// Average interleaved channels down to one.
///
/// Whisper wants mono. Averaging rather than taking the first channel because
/// a voice can sit entirely in one side of a stereo recording, and taking
/// channel 0 would then transcribe silence.
pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Resample mono audio to [`WHISPER_SAMPLE_RATE`].
///
/// A windowed-sinc kernel, evaluated per output sample. When downsampling the
/// cutoff follows the *output* rate, which is what actually suppresses the
/// aliasing; when upsampling it follows the input rate, since there is nothing
/// above it to fold down.
pub fn resample(samples: &[f32], from_rate: u32) -> Vec<f32> {
    if from_rate == WHISPER_SAMPLE_RATE || samples.is_empty() || from_rate == 0 {
        return samples.to_vec();
    }

    let ratio = WHISPER_SAMPLE_RATE as f64 / from_rate as f64;
    let out_len = ((samples.len() as f64) * ratio).round().max(1.0) as usize;

    // Normalised cutoff, in cycles per *input* sample.
    let cutoff = 0.5 * ratio.min(1.0);
    // Widened as the cutoff falls, so a heavy downsample still gets enough
    // taps for the filter to actually be one.
    let half_width = (4.0 / cutoff).ceil() as isize;

    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let centre = i as f64 / ratio;
        let first = centre.floor() as isize - half_width;
        let last = centre.floor() as isize + half_width;

        let mut acc = 0.0f64;
        let mut weight_sum = 0.0f64;
        for n in first..=last {
            if n < 0 || n as usize >= samples.len() {
                continue;
            }
            let offset = centre - n as f64;
            let sinc = {
                let x = 2.0 * std::f64::consts::PI * cutoff * offset;
                if x.abs() < 1e-9 {
                    1.0
                } else {
                    x.sin() / x
                }
            };
            // Blackman window over the tap range, so the truncated kernel
            // does not ring.
            let position = (offset + half_width as f64) / (2.0 * half_width as f64);
            let window = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * position).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * position).cos();
            let tap = sinc * window;
            acc += tap * samples[n as usize] as f64;
            weight_sum += tap;
        }
        // Normalised by the taps that actually landed inside the signal, so
        // the first and last few samples are not attenuated toward silence.
        out.push(if weight_sum.abs() > 1e-12 {
            (acc / weight_sum) as f32
        } else {
            0.0
        });
    }
    out
}

#[cfg(feature = "audio")]
mod decode {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    /// Decode any supported container to interleaved `f32`, with its rate and
    /// channel count.
    pub fn to_samples(bytes: &[u8]) -> Result<(Vec<f32>, u32, usize), String> {
        let stream = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(bytes.to_vec())),
            Default::default(),
        );
        let mut format = symphonia::default::get_probe()
            .probe(
                &Hint::new(),
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| format!("Could not read audio: {}", e))?;

        let track = format
            .first_track(TrackType::Audio)
            .ok_or_else(|| "Could not read audio: the file has no audio track".to_string())?;
        let track_id = track.id;
        let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
            return Err(
                "Could not read audio: the audio track has no codec parameters".to_string(),
            );
        };

        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| format!("Could not decode audio: {}", e))?;

        let mut samples: Vec<f32> = Vec::new();
        let mut rate = 0u32;
        let mut channels = 0usize;
        let mut frame: Vec<f32> = Vec::new();

        while let Some(packet) = format
            .next_packet()
            .map_err(|e| format!("Could not read audio: {}", e))?
        {
            if packet.track_id != track_id {
                continue;
            }
            let decoded = decoder
                .decode(&packet)
                .map_err(|e| format!("Could not decode audio: {}", e))?;
            let spec = decoded.spec();
            rate = spec.rate();
            channels = spec.channels().count();
            decoded.copy_to_vec_interleaved(&mut frame);
            samples.extend_from_slice(&frame);
        }

        if rate == 0 || channels == 0 {
            return Err("Could not decode audio: the file holds no audio frames".to_string());
        }
        Ok((samples, rate, channels))
    }
}

#[cfg(feature = "audio")]
mod backend {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    /// The loaded model, remembered along with the path it came from.
    ///
    /// A Whisper model is hundreds of megabytes; reloading it per file would
    /// dominate a directory sweep. Keyed on the path rather than loaded once
    /// forever, because the path is configuration and can change.
    type Cached = (std::path::PathBuf, Arc<WhisperContext>);
    fn cache() -> &'static Mutex<Option<Cached>> {
        static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(None))
    }

    fn context() -> Result<Arc<WhisperContext>, String> {
        let path = model_path()?;

        let mut cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_path, context)) = cached.as_ref() {
            if cached_path == &path {
                return Ok(Arc::clone(context));
            }
        }

        let context = WhisperContext::new_with_params(&path, WhisperContextParameters::default())
            .map_err(|e| {
            format!(
                "could not load the Whisper model at {}: {}",
                path.display(),
                e
            )
        })?;

        let context = Arc::new(context);
        *cached = Some((path, Arc::clone(&context)));
        Ok(context)
    }

    pub fn transcribe(bytes: &[u8]) -> Result<Vec<Segment>, String> {
        // The model is resolved before the file is decoded: a missing model is
        // the caller's most likely mistake and should not cost a decode of a
        // large recording to discover.
        let context = context()?;

        let (interleaved, rate, channels) = super::decode::to_samples(bytes)?;
        let mono = to_mono(&interleaved, channels);
        let samples = resample(&mono, rate);

        let mut state = context
            .create_state()
            .map_err(|e| format!("could not start transcription: {}", e))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        // whisper.cpp otherwise writes progress and the transcript itself to
        // stdout, which on an MCP server is the protocol stream.
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);

        state
            .full(params, &samples)
            .map_err(|e| format!("could not transcribe audio: {}", e))?;

        let mut segments = Vec::new();
        for index in 0..state.full_n_segments() {
            let Some(segment) = state.get_segment(index) else {
                continue;
            };
            let text = segment
                .to_str_lossy()
                .map_err(|e| format!("could not read a transcript segment: {}", e))?
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }
            // whisper.cpp reports timestamps in centiseconds.
            segments.push(Segment {
                text,
                start: segment.start_timestamp() as f64 / 100.0,
                end: segment.end_timestamp() as f64 / 100.0,
            });
        }
        Ok(segments)
    }
}

#[cfg(not(feature = "audio"))]
mod backend {
    use super::*;

    pub fn transcribe(_bytes: &[u8]) -> Result<Vec<Segment>, String> {
        Err(AUDIO_UNAVAILABLE.to_string())
    }
}

/// One chunk of a transcribed recording.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub content: String,
    pub start: f64,
    pub end: f64,
}

/// Transcribe a recording into chunks, each tagged with its time range.
///
/// Returns the chunks and the number of segments Whisper produced — distinct
/// from how many chunks the chunker made, and the honest answer to "how much
/// speech was in this file".
pub fn parse_audio(bytes: &[u8], max_length: usize) -> Result<(Vec<AudioChunk>, usize), String> {
    let segments = backend::transcribe(bytes)?;
    if segments.is_empty() {
        return Err(AUDIO_NO_SPEECH.to_string());
    }

    let mut chunks = Vec::new();
    for segment in &segments {
        for chunk in crate::importer::chunk_text(&segment.text, max_length) {
            chunks.push(AudioChunk {
                content: chunk,
                start: segment.start,
                end: segment.end,
            });
        }
    }
    if chunks.is_empty() {
        return Err(AUDIO_NO_SPEECH.to_string());
    }
    Ok((chunks, segments.len()))
}
