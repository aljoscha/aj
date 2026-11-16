// Modified for the standalone AJ extraction: retain Code Mode dependencies only (Apache-2.0).
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preparation_errors_map_to_expected_placeholders() {
    let cases = [
        (
            AudioPreparationError::InvalidDataUrl {
                reason: "details remain in logs",
            },
            "audio content omitted because it could not be processed",
        ),
        (
            AudioPreparationError::UnsupportedFormat,
            "audio content omitted because its format is not supported; use wav, mp3, m4a, webm, or ogg",
        ),
        (
            AudioPreparationError::AudioTooLarge { size: usize::MAX },
            "audio content omitted because it exceeded the supported size limit; use a smaller audio file",
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.placeholder(), expected);
    }
}

#[test]
fn file_audio_uses_the_measured_length_or_the_full_file_budget() {
    use std::time::Duration;
    for (duration, expected) in [
        (None, 12_000),
        (Some(Duration::ZERO), 12_000),
        (Some(Duration::from_nanos(1)), 1),
        (Some(Duration::from_secs(2)), 20),
        (Some(Duration::from_millis(2_001)), 21),
        (Some(Duration::from_secs(20 * 60)), 12_000),
        (Some(Duration::MAX), 12_000),
    ] {
        assert_eq!(super::estimate_file_audio_tokens(duration), expected);
    }
}

#[test]
fn audio_data_url_encodes_all_padding_lengths_directly_into_the_url() {
    for (bytes, expected) in [(b"a".as_slice(), "YQ=="), (b"ab", "YWI="), (b"abc", "YWJj")] {
        assert_eq!(
            super::audio_data_url("audio/wav", bytes),
            format!("data:audio/wav;base64,{expected}")
        );
    }
}
