//! Generates compact per-codepoint Unicode property tables from the vendored
//! UCD files and writes them to `OUT_DIR/tables.rs` for the crate to
//! `include!`.
//!
//! Each table is a slice of `(start, end, value)` ranges sorted by `start`
//! with disjoint ranges, so the crate looks values up with a binary search.
//! Codepoints not covered by any range fall back to the property's documented
//! default (Neutral for East Asian Width, Unassigned for General Category,
//! false for Emoji_Presentation).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// A half-open-free inclusive codepoint range carrying a parsed property value.
struct Range {
    start: u32,
    end: u32,
    value: String,
}

/// Parses a UCD property file of the common `codepoints ; value # comment`
/// form into inclusive ranges, keeping only rows whose value passes `keep`.
///
/// The returned ranges are sorted by `start`. UCD derivation guarantees one
/// value per codepoint within a single property, so the ranges are disjoint.
fn parse_ucd(path: &Path, keep: impl Fn(&str) -> bool) -> Vec<Range> {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    let mut ranges = Vec::new();
    for line in text.lines() {
        // Strip the trailing comment, then split into the codepoint and value
        // fields. Blank and comment-only lines collapse to empty here.
        let data = line.split('#').next().unwrap().trim();
        if data.is_empty() {
            continue;
        }

        let mut fields = data.split(';');
        let cps = fields.next().unwrap().trim();
        let value = match fields.next() {
            Some(v) => v.trim(),
            None => continue,
        };
        if !keep(value) {
            continue;
        }

        let (start, end) = match cps.split_once("..") {
            Some((lo, hi)) => (parse_hex(lo), parse_hex(hi)),
            None => {
                let cp = parse_hex(cps);
                (cp, cp)
            }
        };

        ranges.push(Range {
            start,
            end,
            value: value.to_string(),
        });
    }

    ranges.sort_by_key(|r| r.start);
    ranges
}

fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 16).unwrap_or_else(|e| panic!("invalid codepoint {s:?}: {e}"))
}

/// Emits `name: &[(u32, u32, <enum>)]` mapping each value token through
/// `variant` to an enum variant of `enum_ty`.
fn emit_enum_table(
    out: &mut String,
    name: &str,
    enum_ty: &str,
    ranges: &[Range],
    variant: impl Fn(&str) -> &'static str,
) {
    writeln!(
        out,
        "pub(crate) static {name}: &[(u32, u32, {enum_ty})] = &[",
    )
    .unwrap();
    for r in ranges {
        writeln!(
            out,
            "    ({:#x}, {:#x}, {enum_ty}::{}),",
            r.start,
            r.end,
            variant(&r.value),
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();
}

/// Emits `name: &[(u32, u32)]`, a membership table with no associated value.
fn emit_set_table(out: &mut String, name: &str, ranges: &[Range]) {
    writeln!(out, "pub(crate) static {name}: &[(u32, u32)] = &[").unwrap();
    for r in ranges {
        writeln!(out, "    ({:#x}, {:#x}),", r.start, r.end).unwrap();
    }
    writeln!(out, "];").unwrap();
}

/// Maps a UCD East_Asian_Width abbreviation to an `EastAsianWidth` variant.
fn eaw_variant(value: &str) -> &'static str {
    match value {
        "N" => "Neutral",
        "A" => "Ambiguous",
        "H" => "Halfwidth",
        "F" => "Fullwidth",
        "Na" => "Narrow",
        "W" => "Wide",
        other => panic!("unknown East_Asian_Width value {other:?}"),
    }
}

/// Maps a UCD General_Category abbreviation to a `GeneralCategory` variant.
fn gc_variant(value: &str) -> &'static str {
    match value {
        "Lu" => "UppercaseLetter",
        "Ll" => "LowercaseLetter",
        "Lt" => "TitlecaseLetter",
        "Lm" => "ModifierLetter",
        "Lo" => "OtherLetter",
        "Mn" => "NonspacingMark",
        "Mc" => "SpacingMark",
        "Me" => "EnclosingMark",
        "Nd" => "DecimalNumber",
        "Nl" => "LetterNumber",
        "No" => "OtherNumber",
        "Pc" => "ConnectorPunctuation",
        "Pd" => "DashPunctuation",
        "Ps" => "OpenPunctuation",
        "Pe" => "ClosePunctuation",
        "Pi" => "InitialPunctuation",
        "Pf" => "FinalPunctuation",
        "Po" => "OtherPunctuation",
        "Sm" => "MathSymbol",
        "Sc" => "CurrencySymbol",
        "Sk" => "ModifierSymbol",
        "So" => "OtherSymbol",
        "Zs" => "SpaceSeparator",
        "Zl" => "LineSeparator",
        "Zp" => "ParagraphSeparator",
        "Cc" => "Control",
        "Cf" => "Format",
        "Cs" => "Surrogate",
        "Co" => "PrivateUse",
        "Cn" => "Unassigned",
        other => panic!("unknown General_Category value {other:?}"),
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ucd = manifest.join("ucd");
    let eaw_path = ucd.join("EastAsianWidth.txt");
    let gc_path = ucd.join("extracted/DerivedGeneralCategory.txt");
    let emoji_path = ucd.join("emoji/emoji-data.txt");

    for p in [&eaw_path, &gc_path, &emoji_path] {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed=build.rs");

    let eaw = parse_ucd(&eaw_path, |_| true);
    let gc = parse_ucd(&gc_path, |_| true);
    let emoji = parse_ucd(&emoji_path, |v| v == "Emoji_Presentation");

    let mut out = String::new();
    out.push_str("// @generated by build.rs from the vendored UCD files. Do not edit.\n\n");
    emit_enum_table(
        &mut out,
        "EAST_ASIAN_WIDTH",
        "EastAsianWidth",
        &eaw,
        eaw_variant,
    );
    out.push('\n');
    emit_enum_table(
        &mut out,
        "GENERAL_CATEGORY",
        "GeneralCategory",
        &gc,
        gc_variant,
    );
    out.push('\n');
    emit_set_table(&mut out, "EMOJI_PRESENTATION", &emoji);

    let dest = Path::new(&env::var("OUT_DIR").unwrap()).join("tables.rs");
    fs::write(&dest, out).unwrap_or_else(|e| panic!("writing {}: {e}", dest.display()));
}
