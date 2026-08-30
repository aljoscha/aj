//! Generates the account-label General_Category table from the vendored UCD
//! source.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_CODE_POINT: u32 = 0x10_ffff;

struct Range {
    start: u32,
    end: u32,
    category: &'static str,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest_dir.join("ucd/extracted/DerivedGeneralCategory.txt");

    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed=build.rs");

    let text = fs::read_to_string(&source)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", source.display()));
    let version = parse_version(&text, &source);
    let ranges = parse_ranges(&text, &source);
    validate_complete_table(&ranges, &source);

    let generated = render_table(version, &ranges);
    let destination = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"))
        .join("account_label_general_category.rs");
    fs::write(&destination, generated)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", destination.display()));
}

fn parse_version(text: &str, source: &Path) -> (u8, u8, u8) {
    let header = text
        .lines()
        .next()
        .unwrap_or_else(|| panic!("{} is empty", source.display()));
    let version = header
        .strip_prefix("# DerivedGeneralCategory-")
        .and_then(|value| value.strip_suffix(".txt"))
        .unwrap_or_else(|| {
            panic!(
                "{} has an invalid DerivedGeneralCategory header: {header:?}",
                source.display()
            )
        });
    let mut components = version.split('.').map(|component| {
        component.parse::<u8>().unwrap_or_else(|error| {
            panic!(
                "{} has an invalid Unicode version component {component:?}: {error}",
                source.display()
            )
        })
    });
    let parsed = (
        components
            .next()
            .unwrap_or_else(|| panic!("{} has no Unicode major version", source.display())),
        components
            .next()
            .unwrap_or_else(|| panic!("{} has no Unicode minor version", source.display())),
        components
            .next()
            .unwrap_or_else(|| panic!("{} has no Unicode patch version", source.display())),
    );
    assert!(
        components.next().is_none(),
        "{} has more than three Unicode version components",
        source.display()
    );
    parsed
}

fn parse_ranges(text: &str, source: &Path) -> Vec<Range> {
    let mut ranges = Vec::new();

    for (line_index, line) in text.lines().enumerate() {
        let data = line
            .split('#')
            .next()
            .expect("split always has one item")
            .trim();
        if data.is_empty() {
            continue;
        }

        let mut fields = data.split(';');
        let code_points = fields.next().expect("nonempty data has one field").trim();
        let category = fields
            .next()
            .unwrap_or_else(|| {
                panic!(
                    "{}:{} has no General_Category field",
                    source.display(),
                    line_index + 1
                )
            })
            .trim();
        assert!(
            fields.next().is_none(),
            "{}:{} has too many fields",
            source.display(),
            line_index + 1
        );

        let category = category_variant(category).unwrap_or_else(|| {
            panic!(
                "{}:{} has unknown General_Category {category:?}",
                source.display(),
                line_index + 1
            )
        });
        let (start, end) = match code_points.split_once("..") {
            Some((start, end)) => (
                parse_code_point(start, source, line_index),
                parse_code_point(end, source, line_index),
            ),
            None => {
                let code_point = parse_code_point(code_points, source, line_index);
                (code_point, code_point)
            }
        };
        assert!(
            start <= end,
            "{}:{} has a descending range",
            source.display(),
            line_index + 1
        );
        assert!(
            end <= MAX_CODE_POINT,
            "{}:{} exceeds U+10FFFF",
            source.display(),
            line_index + 1
        );
        ranges.push(Range {
            start,
            end,
            category,
        });
    }

    ranges.sort_by_key(|range| range.start);
    ranges
}

fn parse_code_point(value: &str, source: &Path, line_index: usize) -> u32 {
    u32::from_str_radix(value.trim(), 16).unwrap_or_else(|error| {
        panic!(
            "{}:{} has invalid code point {value:?}: {error}",
            source.display(),
            line_index + 1
        )
    })
}

fn validate_complete_table(ranges: &[Range], source: &Path) {
    let mut expected_start = 0;
    for range in ranges {
        assert_eq!(
            range.start,
            expected_start,
            "{} has a General_Category gap or overlap before U+{:04X}",
            source.display(),
            range.start
        );
        expected_start = range
            .end
            .checked_add(1)
            .expect("the UCD range end fits below u32::MAX");
    }
    assert_eq!(
        expected_start,
        MAX_CODE_POINT + 1,
        "{} does not cover every code point through U+10FFFF",
        source.display()
    );
}

fn category_variant(category: &str) -> Option<&'static str> {
    Some(match category {
        "Lu" => "Lu",
        "Ll" => "Ll",
        "Lt" => "Lt",
        "Lm" => "Lm",
        "Lo" => "Lo",
        "Mn" => "Mn",
        "Mc" => "Mc",
        "Me" => "Me",
        "Nd" => "Nd",
        "Nl" => "Nl",
        "No" => "No",
        "Pc" => "Pc",
        "Pd" => "Pd",
        "Ps" => "Ps",
        "Pe" => "Pe",
        "Pi" => "Pi",
        "Pf" => "Pf",
        "Po" => "Po",
        "Sm" => "Sm",
        "Sc" => "Sc",
        "Sk" => "Sk",
        "So" => "So",
        "Zs" => "Zs",
        "Zl" => "Zl",
        "Zp" => "Zp",
        "Cc" => "Cc",
        "Cf" => "Cf",
        "Cs" => "Cs",
        "Co" => "Co",
        "Cn" => "Cn",
        _ => return None,
    })
}

fn render_table(version: (u8, u8, u8), ranges: &[Range]) -> String {
    let mut output = String::new();
    output.push_str("// @generated from the vendored Unicode General_Category source.\n\n");
    writeln!(
        output,
        "const GENERAL_CATEGORY_UNICODE_VERSION: (u8, u8, u8) = ({}, {}, {});",
        version.0, version.1, version.2
    )
    .expect("writing to String cannot fail");
    output.push_str("static GENERAL_CATEGORY_RANGES: &[(u32, u32, GeneralCategory)] = &[\n");
    for range in ranges {
        writeln!(
            output,
            "    (0x{:x}, 0x{:x}, GeneralCategory::{}),",
            range.start, range.end, range.category
        )
        .expect("writing to String cannot fail");
    }
    output.push_str("];\n");
    output
}
