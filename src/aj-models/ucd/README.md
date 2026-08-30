# Account-label Unicode data

`extracted/DerivedGeneralCategory.txt` is the official Unicode 17.0.0 source
downloaded from:

`https://www.unicode.org/Public/17.0.0/ucd/extracted/DerivedGeneralCategory.txt`

SHA-256: `d62e5bab70ca74f099343f71224fa051cb1fdd61a1ab45c0488c44cfc0b6102e`.

`aj-models/build.rs` parses the version from the file header and generates the
frontend-independent General_Category table used by account-label validation.
The generated version, `unicode-normalization`, and `unicode-segmentation` are
compile-time checked against the validator's single Unicode version.

An upgrade changes this file, the exact dependency pins, the validator's
version, the residual Default_Ignorable ranges, and their fixtures together.
