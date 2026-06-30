# Vendored Unicode Character Database (UCD) files

These files are the authoritative source data from which `vaxis-ucd`'s
`build.rs` generates its per-codepoint property tables. They are committed so
the build is hermetic and reproducible.

## Pinned version

Unicode **17.0.0**.

We pin to the Unicode version implemented by the workspace's
`unicode-segmentation` dependency (1.13.x, which reports `UNICODE_VERSION =
(17, 0, 0)`). `vaxis-ucd` and `unicode-segmentation` together cover the Unicode
properties `vaxis` needs (width properties here, UAX#29 grapheme segmentation
there), so both must agree on the Unicode version.

## Files and sources

Downloaded from `https://www.unicode.org/Public/17.0.0/ucd/`:

| File | Source URL | Property generated |
|---|---|---|
| `EastAsianWidth.txt` | `.../ucd/EastAsianWidth.txt` | `EastAsianWidth` |
| `extracted/DerivedGeneralCategory.txt` | `.../ucd/extracted/DerivedGeneralCategory.txt` | `GeneralCategory` |
| `emoji/emoji-data.txt` | `.../ucd/emoji/emoji-data.txt` | `is_emoji_presentation` (the `Emoji_Presentation` rows) |

## Refreshing

To move to a new Unicode version, re-download the three files from
`https://www.unicode.org/Public/<VERSION>/ucd/`, update the version above and
in `unicode-segmentation`, and rebuild. `build.rs` reparses on any change to
these files.
