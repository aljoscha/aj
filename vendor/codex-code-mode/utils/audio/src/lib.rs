// Modified for the standalone AJ extraction: retain Code Mode dependencies only (Apache-2.0).
//! Audio preparation and duration-based token estimates for model inputs.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_code_mode_protocol::MAX_PROMPT_AUDIO_INPUT_BYTES;
use codex_code_mode_protocol::models::AudioReference;
use codex_utils_string::approx_token_count;
use std::io::Cursor;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

const AUDIO_PROCESSING_ERROR_PLACEHOLDER: &str =
    "audio content omitted because it could not be processed";
const AUDIO_TOO_LARGE_PLACEHOLDER: &str =
    "audio content omitted because it exceeded the supported size limit; use a smaller audio file";
const UNSUPPORTED_AUDIO_FORMAT_PLACEHOLDER: &str =
    "audio content omitted because its format is not supported; use wav, mp3, m4a, webm, or ogg";

const MAX_PROMPT_AUDIO_BASE64_BYTES: usize = MAX_PROMPT_AUDIO_INPUT_BYTES.div_ceil(3) * 4;
const AUDIO_TOKENS_PER_SECOND: u32 = 10;

#[derive(Debug, thiserror::Error)]
pub enum AudioPreparationError {
    #[error("invalid audio data URL: {reason}")]
    InvalidDataUrl { reason: &'static str },
    #[error("unsupported audio format")]
    UnsupportedFormat,
    #[error("audio input is too large ({size} bytes; max {MAX_PROMPT_AUDIO_INPUT_BYTES} bytes)")]
    AudioTooLarge { size: usize },
}

impl AudioPreparationError {
    pub fn placeholder(&self) -> &'static str {
        match self {
            AudioPreparationError::InvalidDataUrl { .. } => AUDIO_PROCESSING_ERROR_PLACEHOLDER,
            AudioPreparationError::UnsupportedFormat => UNSUPPORTED_AUDIO_FORMAT_PLACEHOLDER,
            AudioPreparationError::AudioTooLarge { .. } => AUDIO_TOO_LARGE_PLACEHOLDER,
        }
    }
}

fn is_data_url(audio_url: &str) -> bool {
    audio_url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

fn canonical_audio_mime(mime: &str) -> Option<&'static str> {
    if mime.eq_ignore_ascii_case("audio/wav")
        || mime.eq_ignore_ascii_case("audio/x-wav")
        || mime.eq_ignore_ascii_case("audio/wave")
        || mime.eq_ignore_ascii_case("audio/vnd.wave")
    {
        Some("audio/wav")
    } else if mime.eq_ignore_ascii_case("audio/mpeg") || mime.eq_ignore_ascii_case("audio/mp3") {
        Some("audio/mpeg")
    } else if mime.eq_ignore_ascii_case("audio/mp4")
        || mime.eq_ignore_ascii_case("audio/m4a")
        || mime.eq_ignore_ascii_case("audio/x-m4a")
    {
        Some("audio/mp4")
    } else if mime.eq_ignore_ascii_case("audio/webm") {
        Some("audio/webm")
    } else if mime.eq_ignore_ascii_case("audio/ogg") {
        Some("audio/ogg")
    } else {
        None
    }
}

/// Budget one file reference at the inline audio rate. Missing or unusable metadata
/// reserves the full 20 minutes accepted by Responses. Admission belongs to Responses.
pub fn estimate_file_audio_tokens(duration: Option<std::time::Duration>) -> usize {
    const MAX_DURATION: std::time::Duration = std::time::Duration::from_secs(20 * 60);
    let duration = duration
        .filter(|value| !value.is_zero())
        .unwrap_or(MAX_DURATION)
        .min(MAX_DURATION);
    let tokens =
        (duration.as_nanos() * u128::from(AUDIO_TOKENS_PER_SECOND)).div_ceil(1_000_000_000);
    usize::try_from(tokens).unwrap_or(usize::MAX)
}

/// Code Mode can render on a single thread, where blocking cache access is unavailable.
pub fn estimate_tool_audio_tokens_without_cache(audio: &AudioReference) -> usize {
    estimate_tool_audio_tokens_with(audio, estimate_audio_token_count_without_cache)
}

fn estimate_tool_audio_tokens_with(
    audio: &AudioReference,
    inline_cost: impl FnOnce(&str) -> usize,
) -> usize {
    match audio {
        AudioReference::Inline { audio_url } => inline_cost(audio_url),
        AudioReference::File { .. } => estimate_file_audio_tokens(/*duration*/ None),
    }
}

/// The same duration-based estimate without accessing the Tokio-dependent cache.
/// Suitable for bounded output rendered on a current-thread runtime.
fn estimate_audio_token_count_without_cache(audio_url: &str) -> usize {
    let Ok((mime, bytes)) = decode_audio(audio_url) else {
        return approx_token_count(audio_url);
    };
    decoded_audio_token_count(mime, bytes, audio_url)
}

fn decoded_audio_token_count(mime: &str, bytes: Vec<u8>, audio_url: &str) -> usize {
    let Some(duration_seconds) = audio_duration_seconds(mime, bytes) else {
        return approx_token_count(audio_url);
    };
    let token_count = (duration_seconds * f64::from(AUDIO_TOKENS_PER_SECOND)).ceil();
    if token_count >= usize::MAX as f64 {
        usize::MAX
    } else {
        token_count as usize
    }
}

fn audio_duration_seconds(canonical_mime: &str, bytes: Vec<u8>) -> Option<f64> {
    let media_source = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    hint.mime_type(canonical_mime);
    let format = match symphonia::default::get_probe().probe(
        &hint,
        media_source,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(format) => format,
        Err(error) => {
            tracing::trace!(%error, "failed to read audio duration for token estimation");
            return None;
        }
    };
    let track = format.default_track(TrackType::Audio)?;
    let timing = track.time_base.zip(track.duration).or_else(|| {
        format
            .media_info()
            .time_base
            .zip(format.media_info().duration)
    });
    let (time_base, duration) = timing?;
    let duration_seconds =
        duration.get() as f64 * f64::from(time_base.numer.get()) / f64::from(time_base.denom.get());
    duration_seconds.is_finite().then_some(duration_seconds)
}

/// Validate the data URL and measure its decoded duration in the same pass.
pub fn prepare_and_measure_inline_audio(
    audio_url: &mut String,
) -> Result<usize, AudioPreparationError> {
    let (mime, bytes) = decode_audio(audio_url)?;
    normalize_validated_header(audio_url, mime);
    Ok(decoded_audio_token_count(mime, bytes, audio_url))
}

// The standard base64 decoder has already validated the payload, including padding.
fn normalize_validated_header(url: &mut String, mime: &str) {
    let canonical = format!("data:{mime};base64");
    if let Some((header, _)) = url.split_once(',')
        && header != canonical
    {
        url.replace_range(..header.len(), &canonical);
    }
}

/// Canonical inline representation for a validated payload.
pub fn audio_data_url(mime: &str, data: &[u8]) -> String {
    let mut url =
        String::with_capacity("data:;base64,".len() + mime.len() + data.len().div_ceil(3) * 4);
    url.push_str("data:");
    url.push_str(mime);
    url.push_str(";base64,");
    BASE64_STANDARD.encode_string(data, &mut url);
    url
}

/// Parses a size-bounded audio data URL. A file-writing host also verifies its media container.
pub fn decode_audio(audio_url: &str) -> Result<(&'static str, Vec<u8>), AudioPreparationError> {
    if !is_data_url(audio_url) {
        return Err(AudioPreparationError::InvalidDataUrl {
            reason: "audio input must be a data URL",
        });
    }

    let (metadata, payload) =
        audio_url
            .split_once(',')
            .ok_or(AudioPreparationError::InvalidDataUrl {
                reason: "missing payload separator",
            })?;
    let metadata = metadata
        .get("data:".len()..)
        .ok_or(AudioPreparationError::InvalidDataUrl {
            reason: "missing data URL prefix",
        })?;
    let mut metadata_parts = metadata.split(';');
    let mime = metadata_parts
        .next()
        .filter(|mime| !mime.is_empty())
        .ok_or(AudioPreparationError::InvalidDataUrl {
            reason: "missing media type",
        })?;
    let canonical_mime =
        canonical_audio_mime(mime).ok_or(AudioPreparationError::UnsupportedFormat)?;
    if !metadata_parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return Err(AudioPreparationError::InvalidDataUrl {
            reason: "audio payload is not base64 encoded",
        });
    }
    if payload.len() > MAX_PROMPT_AUDIO_BASE64_BYTES {
        return Err(AudioPreparationError::AudioTooLarge {
            size: payload.len(),
        });
    }

    let bytes =
        BASE64_STANDARD
            .decode(payload)
            .map_err(|_| AudioPreparationError::InvalidDataUrl {
                reason: "invalid base64 payload",
            })?;
    if bytes.is_empty() {
        return Err(AudioPreparationError::InvalidDataUrl {
            reason: "audio payload is empty",
        });
    }
    if bytes.len() > MAX_PROMPT_AUDIO_INPUT_BYTES {
        return Err(AudioPreparationError::AudioTooLarge { size: bytes.len() });
    }

    Ok((canonical_mime, bytes))
}

#[cfg(test)]
#[path = "audio_preparation_tests.rs"]
mod tests;
