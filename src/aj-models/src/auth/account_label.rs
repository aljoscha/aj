//! Unicode validation and reversible display for account labels.

use std::fmt::Write as _;

use unicode_segmentation::UnicodeSegmentation as _;

/// The Unicode release governing every account-label property and algorithm.
pub const ACCOUNT_LABEL_UNICODE_VERSION: (u8, u8, u8) = (17, 0, 0);

/// Maximum UTF-8 size of a newly created account label.
pub const MAX_ACCOUNT_LABEL_BYTES: usize = 256;

#[allow(clippy::as_conversions)]
const _: () = {
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.0 == unicode_normalization::UNICODE_VERSION.0);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.1 == unicode_normalization::UNICODE_VERSION.1);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.2 == unicode_normalization::UNICODE_VERSION.2);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.0 as u64 == unicode_segmentation::UNICODE_VERSION.0);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.1 as u64 == unicode_segmentation::UNICODE_VERSION.1);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.2 as u64 == unicode_segmentation::UNICODE_VERSION.2);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.0 == GENERAL_CATEGORY_UNICODE_VERSION.0);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.1 == GENERAL_CATEGORY_UNICODE_VERSION.1);
    assert!(ACCOUNT_LABEL_UNICODE_VERSION.2 == GENERAL_CATEGORY_UNICODE_VERSION.2);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GeneralCategory {
    Lu,
    Ll,
    Lt,
    Lm,
    Lo,
    Mn,
    Mc,
    Me,
    Nd,
    Nl,
    No,
    Pc,
    Pd,
    Ps,
    Pe,
    Pi,
    Pf,
    Po,
    Sm,
    Sc,
    Sk,
    So,
    Zs,
    Zl,
    Zp,
    Cc,
    Cf,
    Cs,
    Co,
    Cn,
}

include!(concat!(
    env!("OUT_DIR"),
    "/account_label_general_category.rs"
));

impl GeneralCategory {
    fn is_label_scalar(self) -> bool {
        matches!(
            self,
            Self::Lu
                | Self::Ll
                | Self::Lt
                | Self::Lm
                | Self::Lo
                | Self::Mn
                | Self::Mc
                | Self::Me
                | Self::Nd
                | Self::Nl
                | Self::No
                | Self::Pc
                | Self::Pd
                | Self::Ps
                | Self::Pe
                | Self::Pi
                | Self::Pf
                | Self::Po
                | Self::Sm
                | Self::Sc
                | Self::Sk
                | Self::So
        )
    }

    fn is_grapheme_starter(self) -> bool {
        matches!(
            self,
            Self::Lu
                | Self::Ll
                | Self::Lt
                | Self::Lm
                | Self::Lo
                | Self::Nd
                | Self::Nl
                | Self::No
                | Self::Pc
                | Self::Pd
                | Self::Ps
                | Self::Pe
                | Self::Pi
                | Self::Pf
                | Self::Po
                | Self::Sm
                | Self::Sc
                | Self::Sk
                | Self::So
        )
    }
}

/// Why an exact account label is ineligible for creation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AccountLabelValidationError {
    /// The input is not already in Unicode Normalization Form C.
    #[error("account label is not NFC")]
    NotNfc,
    /// A scalar is outside U+0020 and General_Category L, M, N, P, or S.
    #[error("account label contains disallowed scalar U+{code_point:04X}")]
    DisallowedScalar { code_point: u32 },
    /// A scalar is in the residual default-ignorable repertoire.
    #[error("account label contains default-ignorable scalar U+{code_point:04X}")]
    DefaultIgnorableScalar { code_point: u32 },
    /// UAX #29 joined U+0020 to another scalar in one extended grapheme.
    #[error("account label has a space joined to another scalar")]
    JoinedSpaceGrapheme,
    /// A non-space grapheme starts with General_Category M.
    #[error("account label grapheme starts with disallowed scalar U+{code_point:04X}")]
    InvalidGraphemeStarter { code_point: u32 },
    /// The input has no grapheme other than U+0020.
    #[error("account label must contain a non-space grapheme")]
    NoNonSpaceGrapheme,
    /// The first or last scalar is U+0020.
    #[error("account label must not begin or end with a space")]
    EdgeSpace,
    /// The exact UTF-8 input exceeds [`MAX_ACCOUNT_LABEL_BYTES`].
    #[error("account label is {bytes} UTF-8 bytes; the maximum is 256")]
    TooLong { bytes: usize },
}

/// Validate an exact, case-sensitive account label for creation.
///
/// Validation never trims, normalizes, folds, or otherwise rewrites `label`.
/// Stored legacy labels are not subject to this creation-only predicate.
pub fn validate_account_label(label: &str) -> Result<(), AccountLabelValidationError> {
    if !unicode_normalization::is_nfc(label) {
        return Err(AccountLabelValidationError::NotNfc);
    }

    for scalar in label.chars() {
        validate_label_scalar(scalar)?;
    }

    let mut has_non_space_grapheme = false;
    for grapheme in label.graphemes(true) {
        if grapheme.contains(' ') {
            if grapheme != " " {
                return Err(AccountLabelValidationError::JoinedSpaceGrapheme);
            }
            continue;
        }

        has_non_space_grapheme = true;
        let starter = grapheme
            .chars()
            .next()
            .expect("a grapheme returned by unicode-segmentation is nonempty");
        if !general_category(starter).is_grapheme_starter() {
            return Err(AccountLabelValidationError::InvalidGraphemeStarter {
                code_point: u32::from(starter),
            });
        }
    }

    if !has_non_space_grapheme {
        return Err(AccountLabelValidationError::NoNonSpaceGrapheme);
    }
    if label.starts_with(' ') || label.ends_with(' ') {
        return Err(AccountLabelValidationError::EdgeSpace);
    }
    if label.len() > MAX_ACCOUNT_LABEL_BYTES {
        return Err(AccountLabelValidationError::TooLong { bytes: label.len() });
    }

    Ok(())
}

/// Validate a prospective live-edit buffer's scalar repertoire and byte bound.
///
/// This deliberately permits incomplete states such as empty, all-space,
/// edge-space, leading-mark, and non-NFC text. Call [`validate_account_label`]
/// only on submission.
pub fn validate_account_label_edit(candidate: &str) -> Result<(), AccountLabelValidationError> {
    if candidate.len() > MAX_ACCOUNT_LABEL_BYTES {
        return Err(AccountLabelValidationError::TooLong {
            bytes: candidate.len(),
        });
    }
    for scalar in candidate.chars() {
        validate_label_scalar(scalar)?;
    }
    Ok(())
}

/// Account-label representation policy for a display surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountLabelDisplayMode {
    /// Preserve creation-valid Unicode while doubling literal backslashes.
    Ordinary,
    /// Encode every scalar into the conservative ASCII grammar.
    Ascii,
}

/// Render an account label with the canonical reversible display grammar.
///
/// In ordinary mode, creation-valid labels remain raw except that each `\` is
/// doubled. Invalid and empty legacy labels, and every label in ASCII mode,
/// use `\!` followed by one canonical lowercase `\u{h}` token per scalar.
pub fn display_account_label(label: &str, mode: AccountLabelDisplayMode) -> String {
    if mode == AccountLabelDisplayMode::Ordinary && validate_account_label(label).is_ok() {
        let mut output = String::with_capacity(label.len());
        for scalar in label.chars() {
            if scalar == '\\' {
                output.push('\\');
            }
            output.push(scalar);
        }
        return output;
    }

    let mut output = String::from("\\!");
    for scalar in label.chars() {
        write!(output, "\\u{{{:x}}}", u32::from(scalar)).expect("writing to String cannot fail");
    }
    output
}

fn validate_label_scalar(scalar: char) -> Result<(), AccountLabelValidationError> {
    if scalar == ' ' {
        return Ok(());
    }

    let code_point = u32::from(scalar);
    if !general_category(scalar).is_label_scalar() {
        return Err(AccountLabelValidationError::DisallowedScalar { code_point });
    }
    if is_residual_default_ignorable(scalar) {
        return Err(AccountLabelValidationError::DefaultIgnorableScalar { code_point });
    }
    Ok(())
}

fn general_category(scalar: char) -> GeneralCategory {
    let code_point = u32::from(scalar);
    let index = GENERAL_CATEGORY_RANGES
        .partition_point(|(start, _, _)| *start <= code_point)
        .checked_sub(1)
        .expect("the generated General_Category table starts at U+0000");
    let (_, end, category) = GENERAL_CATEGORY_RANGES[index];
    debug_assert!(code_point <= end, "the generated table is complete");
    category
}

fn is_residual_default_ignorable(scalar: char) -> bool {
    matches!(
        u32::from(scalar),
        0x034f
            | 0x115f..=0x1160
            | 0x17b4..=0x17b5
            | 0x180b..=0x180d
            | 0x180f
            | 0x3164
            | 0xfe00..=0xfe0f
            | 0xffa0
            | 0xe0100..=0xe01ef
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn accepts_every_allowed_general_category_and_starter_family() {
        // Expected categories are fixed Unicode 17 fixtures, not queried from
        // the generated table under test.
        let valid = [
            "A",                // Lu
            "a",                // Ll
            "\u{01c5}",         // Lt
            "\u{02b0}",         // Lm
            "\u{05d0}",         // Lo
            "x\u{0301}",        // Mn after an L starter
            "\u{0915}\u{0903}", // Mc after an L starter
            "x\u{20dd}",        // Me after an L starter
            "0",                // Nd
            "\u{216b}",         // Nl
            "\u{00bd}",         // No
            "_",                // Pc
            "-",                // Pd
            "(",                // Ps
            ")",                // Pe
            "\u{00ab}",         // Pi
            "\u{00bb}",         // Pf
            "!",                // Po
            "+",                // Sm
            "$",                // Sc
            "^",                // Sk
            "\u{00a9}",         // So
            "A\u{0301}",        // L starter (precomposed by this fixture below)
            "0\u{20dd}",        // N starter
            "!\u{20dd}",        // P starter
            "\u{00a9}\u{20dd}", // S starter
        ];

        for label in valid {
            let label = if label == "A\u{0301}" {
                "\u{00c1}"
            } else {
                label
            };
            assert_eq!(validate_account_label(label), Ok(()), "fixture {label:?}");
        }
        assert_eq!(validate_account_label("a b"), Ok(()));
    }

    #[test]
    fn rejects_disallowed_category_families_and_terminal_controls() {
        let fixtures = [
            ("\u{00a0}", 0x00a0), // Zs, only U+0020 is admitted
            ("\u{2028}", 0x2028), // Zl
            ("\u{2029}", 0x2029), // Zp
            ("\n", 0x000a),       // Cc
            ("\u{200d}", 0x200d), // Cf
            ("\u{202e}", 0x202e), // bidi override, Cf
            ("\u{2066}", 0x2066), // bidi isolate, Cf
            ("\u{e000}", 0xe000), // Co
            ("\u{0378}", 0x0378), // Cn in Unicode 17
        ];

        for (label, code_point) in fixtures {
            assert_eq!(
                validate_account_label(label),
                Err(AccountLabelValidationError::DisallowedScalar { code_point }),
                "fixture U+{code_point:04X}"
            );
        }
    }

    #[test]
    fn requires_exact_nfc_without_rewriting() {
        assert_eq!(validate_account_label("\u{00e9}"), Ok(()));
        assert_eq!(
            validate_account_label("e\u{0301}"),
            Err(AccountLabelValidationError::NotNfc)
        );
        assert_ne!("\u{00e9}", "e\u{0301}");
    }

    #[test]
    fn enforces_space_and_grapheme_rules() {
        assert_eq!(
            validate_account_label(""),
            Err(AccountLabelValidationError::NoNonSpaceGrapheme)
        );
        assert_eq!(
            validate_account_label("   "),
            Err(AccountLabelValidationError::NoNonSpaceGrapheme)
        );
        assert_eq!(
            validate_account_label(" work"),
            Err(AccountLabelValidationError::EdgeSpace)
        );
        assert_eq!(
            validate_account_label("work "),
            Err(AccountLabelValidationError::EdgeSpace)
        );
        for (label, code_point) in [
            ("\u{0301}x", 0x0301),
            ("\u{0903}x", 0x0903),
            ("\u{20dd}x", 0x20dd),
        ] {
            assert_eq!(
                validate_account_label(label),
                Err(AccountLabelValidationError::InvalidGraphemeStarter { code_point })
            );
        }
        assert_eq!(
            validate_account_label("x \u{0301}"),
            Err(AccountLabelValidationError::JoinedSpaceGrapheme)
        );
        assert_eq!(
            validate_account_label("\u{0d4e} \u{0301}"),
            Err(AccountLabelValidationError::JoinedSpaceGrapheme)
        );
    }

    #[test]
    fn enforces_exact_utf8_byte_boundary() {
        let bytes_255 = "a".repeat(255);
        let bytes_256 = "a".repeat(256);
        let bytes_257 = "a".repeat(257);
        let multibyte_256 = format!("{}\u{00e9}", "a".repeat(254));
        let multibyte_257 = format!("{}\u{00e9}", "a".repeat(255));
        let four_byte_256 = "\u{1f600}".repeat(64);

        assert_eq!(bytes_255.len(), 255);
        assert_eq!(bytes_256.len(), 256);
        assert_eq!(bytes_257.len(), 257);
        assert_eq!(multibyte_256.len(), 256);
        assert_eq!(multibyte_257.len(), 257);
        assert_eq!(four_byte_256.len(), 256);
        assert_eq!(validate_account_label(&bytes_255), Ok(()));
        assert_eq!(validate_account_label(&bytes_256), Ok(()));
        assert_eq!(validate_account_label(&multibyte_256), Ok(()));
        assert_eq!(validate_account_label(&four_byte_256), Ok(()));
        assert_eq!(
            validate_account_label(&bytes_257),
            Err(AccountLabelValidationError::TooLong { bytes: 257 })
        );
        assert_eq!(
            validate_account_label(&multibyte_257),
            Err(AccountLabelValidationError::TooLong { bytes: 257 })
        );
    }

    #[test]
    fn rejects_every_residual_endpoint_and_samples_range_interiors() {
        let endpoints = [
            ('\u{034f}', 0x034f),
            ('\u{115f}', 0x115f),
            ('\u{1160}', 0x1160),
            ('\u{17b4}', 0x17b4),
            ('\u{17b5}', 0x17b5),
            ('\u{180b}', 0x180b),
            ('\u{180d}', 0x180d),
            ('\u{180f}', 0x180f),
            ('\u{3164}', 0x3164),
            ('\u{fe00}', 0xfe00),
            ('\u{fe0f}', 0xfe0f),
            ('\u{ffa0}', 0xffa0),
            ('\u{e0100}', 0xe0100),
            ('\u{e01ef}', 0xe01ef),
        ];
        for (scalar, code_point) in endpoints {
            let label = format!("a{scalar}");
            assert_eq!(
                validate_account_label(&label),
                Err(AccountLabelValidationError::DefaultIgnorableScalar { code_point }),
                "fixture U+{code_point:04X}"
            );
        }
        for (scalar, code_point) in [
            ('\u{180c}', 0x180c),
            ('\u{fe07}', 0xfe07),
            ('\u{e0180}', 0xe0180),
        ] {
            assert_eq!(
                validate_account_label(&format!("a{scalar}")),
                Err(AccountLabelValidationError::DefaultIgnorableScalar { code_point })
            );
        }
    }

    #[test]
    fn residual_neighbors_follow_their_independent_unicode_17_categories() {
        // Every endpoint and its immediate neighbors are explicit. Values
        // outside the residual ranges are edit-safe only when their fixed
        // Unicode 17 category is L/M/N/P/S.
        let neighbors = [
            ('\u{034e}', true),
            ('\u{034f}', false),
            ('\u{0350}', true),
            ('\u{115e}', true),
            ('\u{115f}', false),
            ('\u{1160}', false),
            ('\u{1161}', true),
            ('\u{17b3}', true),
            ('\u{17b4}', false),
            ('\u{17b5}', false),
            ('\u{17b6}', true),
            ('\u{180a}', true),
            ('\u{180b}', false),
            ('\u{180c}', false),
            ('\u{180d}', false),
            ('\u{180e}', false),
            ('\u{180f}', false),
            ('\u{1810}', true),
            ('\u{3163}', true),
            ('\u{3164}', false),
            ('\u{3165}', true),
            ('\u{fdff}', true),
            ('\u{fe00}', false),
            ('\u{fe01}', false),
            ('\u{fe0e}', false),
            ('\u{fe0f}', false),
            ('\u{fe10}', true),
            ('\u{ff9f}', true),
            ('\u{ffa0}', false),
            ('\u{ffa1}', true),
            ('\u{e00ff}', false),
            ('\u{e0100}', false),
            ('\u{e0101}', false),
            ('\u{e01ee}', false),
            ('\u{e01ef}', false),
            ('\u{e01f0}', false),
        ];
        for (scalar, expected) in neighbors {
            assert_eq!(
                validate_account_label_edit(&format!("a{scalar}")).is_ok(),
                expected,
                "neighbor U+{:04X}",
                u32::from(scalar)
            );
        }
    }

    #[test]
    fn edit_safety_allows_incomplete_states_but_enforces_repertoire_and_bytes() {
        assert_eq!(validate_account_label_edit(""), Ok(()));
        assert_eq!(validate_account_label_edit("   "), Ok(()));
        assert_eq!(validate_account_label_edit(" work "), Ok(()));
        assert_eq!(validate_account_label_edit("\u{0301}work"), Ok(()));
        assert_eq!(validate_account_label_edit("e\u{0301}"), Ok(()));
        assert_eq!(validate_account_label_edit(&"a".repeat(256)), Ok(()));
        assert_eq!(
            validate_account_label_edit(&"a".repeat(257)),
            Err(AccountLabelValidationError::TooLong { bytes: 257 })
        );
        assert_eq!(
            validate_account_label_edit("work\n"),
            Err(AccountLabelValidationError::DisallowedScalar { code_point: 10 })
        );
        assert_eq!(
            validate_account_label_edit("a\u{034f}"),
            Err(AccountLabelValidationError::DefaultIgnorableScalar { code_point: 0x034f })
        );
    }

    #[test]
    fn display_uses_the_exact_canonical_grammar() {
        assert_eq!(
            display_account_label("work\\one", AccountLabelDisplayMode::Ordinary),
            "work\\\\one"
        );
        assert_eq!(
            display_account_label("wo\nrk", AccountLabelDisplayMode::Ordinary),
            "\\!\\u{77}\\u{6f}\\u{a}\\u{72}\\u{6b}"
        );
        assert_eq!(
            display_account_label("", AccountLabelDisplayMode::Ordinary),
            "\\!"
        );
        assert_eq!(
            display_account_label("A\\", AccountLabelDisplayMode::Ascii),
            "\\!\\u{41}\\u{5c}"
        );
        assert_eq!(
            display_account_label("\u{0100}", AccountLabelDisplayMode::Ascii),
            "\\!\\u{100}"
        );
    }

    #[test]
    fn both_display_modes_round_trip_through_an_independent_decoder() {
        let labels = adversarial_labels();
        for label in &labels {
            for mode in [
                AccountLabelDisplayMode::Ordinary,
                AccountLabelDisplayMode::Ascii,
            ] {
                let representation = display_account_label(label, mode);
                assert_eq!(
                    decode_representation(&representation),
                    Some(label.clone()),
                    "mode {mode:?}, label {label:?}, representation {representation:?}"
                );
            }
        }
    }

    #[test]
    fn both_display_modes_are_injective_over_adversarial_and_generated_labels() {
        let labels = adversarial_labels();
        for mode in [
            AccountLabelDisplayMode::Ordinary,
            AccountLabelDisplayMode::Ascii,
        ] {
            let representations: Vec<_> = labels
                .iter()
                .map(|label| display_account_label(label, mode))
                .collect();
            for left in 0..labels.len() {
                for right in (left + 1)..labels.len() {
                    assert_ne!(labels[left], labels[right]);
                    assert_ne!(
                        representations[left], representations[right],
                        "mode {mode:?} collided for {:?} and {:?}",
                        labels[left], labels[right]
                    );
                }
            }
        }
    }

    #[test]
    fn representations_are_terminal_inert() {
        let mut hostile = Vec::new();
        for code_point in 0x00..=0x1f {
            hostile.push(
                char::from_u32(code_point)
                    .expect("C0 is scalar")
                    .to_string(),
            );
        }
        for code_point in 0x7f..=0x9f {
            hostile.push(
                char::from_u32(code_point)
                    .expect("C1 is scalar")
                    .to_string(),
            );
        }
        hostile.extend([
            "\u{2028}".to_string(),
            "\u{2029}".to_string(),
            "\u{202e}".to_string(),
            "\u{2066}".to_string(),
            "\u{2069}".to_string(),
            "\u{001b}[31mwork\u{001b}[0m".to_string(),
            "\u{001b}]0;title\u{0007}".to_string(),
        ]);

        for label in hostile {
            for mode in [
                AccountLabelDisplayMode::Ordinary,
                AccountLabelDisplayMode::Ascii,
            ] {
                let representation = display_account_label(&label, mode);
                assert!(
                    representation.bytes().all(|byte| byte.is_ascii_graphic()),
                    "mode {mode:?} emitted terminal-active text for {label:?}: {representation:?}"
                );
                assert_eq!(decode_representation(&representation), Some(label.clone()));
            }
        }

        let rtl = display_account_label(
            "\u{05e9}\u{05dc}\u{05d5}\u{05dd}",
            AccountLabelDisplayMode::Ordinary,
        );
        assert_eq!(rtl, "\u{05e9}\u{05dc}\u{05d5}\u{05dd}");
        assert!(!contains_terminal_control(&rtl));
    }

    #[test]
    fn representation_creation_bounds_are_exact() {
        let slashes = "\\".repeat(256);
        assert_eq!(slashes.len(), 256);
        assert_eq!(validate_account_label(&slashes), Ok(()));

        let ordinary = display_account_label(&slashes, AccountLabelDisplayMode::Ordinary);
        let ascii = display_account_label(&slashes, AccountLabelDisplayMode::Ascii);
        assert_eq!(ordinary, "\\".repeat(512));
        assert_eq!(ordinary.len(), 512);
        assert_eq!(ascii.len(), 1_538);
        assert!(ascii.starts_with("\\!\\u{5c}"));
        assert_eq!(decode_representation(&ordinary), Some(slashes.clone()));
        assert_eq!(decode_representation(&ascii), Some(slashes));
    }

    #[test]
    fn legacy_representation_boundary_fixtures_are_exact() {
        let at_limit = format!("{}\u{0100}", "a".repeat(10_921));
        let over_limit = format!("{}\u{1000}", "a".repeat(10_921));
        assert_eq!(at_limit.len(), 10_923);
        assert_eq!(over_limit.len(), 10_924);

        let represented_at_limit =
            display_account_label(&at_limit, AccountLabelDisplayMode::Ordinary);
        let represented_over_limit =
            display_account_label(&over_limit, AccountLabelDisplayMode::Ordinary);
        assert_eq!(represented_at_limit.len(), 65_535);
        assert_eq!(represented_over_limit.len(), 65_536);
        assert!(represented_at_limit.ends_with("\\u{100}"));
        assert!(represented_over_limit.ends_with("\\u{1000}"));
        assert_eq!(decode_representation(&represented_at_limit), Some(at_limit));
        assert_eq!(
            decode_representation(&represented_over_limit),
            Some(over_limit)
        );
    }

    #[test]
    fn generated_general_category_table_is_sorted_disjoint_and_complete() {
        assert_eq!(
            GENERAL_CATEGORY_RANGES.first().map(|range| range.0),
            Some(0)
        );
        assert_eq!(
            GENERAL_CATEGORY_RANGES.last().map(|range| range.1),
            Some(0x10_ffff)
        );
        for ranges in GENERAL_CATEGORY_RANGES.windows(2) {
            assert_eq!(ranges[0].1 + 1, ranges[1].0);
        }
    }

    fn adversarial_labels() -> Vec<String> {
        let mut labels = BTreeSet::from([
            "".to_string(),
            " ".to_string(),
            "   ".to_string(),
            "work".to_string(),
            "wo\nrk".to_string(),
            "\u{00e9}".to_string(),
            "e\u{0301}".to_string(),
            "\\".to_string(),
            "\\!".to_string(),
            "\\u{61}".to_string(),
            "\\!\\u{61}".to_string(),
            "a b".to_string(),
            "a    b".to_string(),
            "\u{202e}".to_string(),
            "\u{034f}".to_string(),
            "\u{e0100}".to_string(),
            "\u{e000}".to_string(),
            "\u{0378}".to_string(),
            "\u{05e9}\u{05dc}\u{05d5}\u{05dd}".to_string(),
            "a".repeat(256),
            "a".repeat(257),
        ]);

        for code_point in 0x00..=0x1f {
            labels.insert(
                char::from_u32(code_point)
                    .expect("C0 is scalar")
                    .to_string(),
            );
        }
        for code_point in 0x7f..=0x9f {
            labels.insert(
                char::from_u32(code_point)
                    .expect("C1 is scalar")
                    .to_string(),
            );
        }
        for scalar in [
            '\u{200b}', '\u{200c}', '\u{200d}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}',
            '\u{202c}', '\u{202d}', '\u{202e}', '\u{2060}', '\u{2061}', '\u{2062}', '\u{2063}',
            '\u{2064}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{feff}',
        ] {
            labels.insert(scalar.to_string());
        }
        for scalar in [
            '\u{034f}',
            '\u{115f}',
            '\u{1160}',
            '\u{17b4}',
            '\u{17b5}',
            '\u{180b}',
            '\u{180d}',
            '\u{180f}',
            '\u{3164}',
            '\u{fe00}',
            '\u{fe0f}',
            '\u{ffa0}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            labels.insert(format!("a{scalar}"));
        }

        let alphabet = ['a', ' ', '\\', '\n', '\u{00e9}', '\u{0301}', '\u{202e}'];
        for first in alphabet {
            labels.insert(first.to_string());
            for second in alphabet {
                labels.insert(format!("{first}{second}"));
                for third in alphabet {
                    labels.insert(format!("{first}{second}{third}"));
                }
            }
        }

        labels.into_iter().collect()
    }

    fn decode_representation(representation: &str) -> Option<String> {
        if let Some(mut encoded) = representation.strip_prefix("\\!") {
            let mut decoded = String::new();
            while !encoded.is_empty() {
                encoded = encoded.strip_prefix("\\u{")?;
                let close = encoded.find('}')?;
                let digits = &encoded[..close];
                if digits.is_empty()
                    || digits
                        .bytes()
                        .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
                    || (digits.len() > 1 && digits.starts_with('0'))
                {
                    return None;
                }
                let code_point = u32::from_str_radix(digits, 16).ok()?;
                decoded.push(char::from_u32(code_point)?);
                encoded = &encoded[(close + 1)..];
            }
            return Some(decoded);
        }

        let mut decoded = String::new();
        let mut scalars = representation.chars();
        while let Some(scalar) = scalars.next() {
            if scalar == '\\' {
                if scalars.next() != Some('\\') {
                    return None;
                }
                decoded.push('\\');
            } else {
                decoded.push(scalar);
            }
        }
        Some(decoded)
    }

    fn contains_terminal_control(value: &str) -> bool {
        value.chars().any(|scalar| {
            matches!(
                u32::from(scalar),
                0x00..=0x1f
                    | 0x7f..=0x9f
                    | 0x2028..=0x2029
                    | 0x202a..=0x202e
                    | 0x2066..=0x2069
            )
        })
    }
}
