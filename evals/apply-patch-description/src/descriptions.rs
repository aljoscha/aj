//! Frozen treatment descriptions and their recorded identities.

use aj_agent::tool::ToolDefinition;
use aj_tools::tools::apply_patch::ApplyPatchTool;
use serde::{Deserialize, Serialize};

use crate::sha256_hex;

const COMPACT_V1: &[u8] = include_bytes!("../descriptions/compact-v1.txt");

/// Stable name of a treatment description.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DescriptionVariant {
    Current,
    CompactV1,
}

/// Exact description content and its recorded identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrozenDescription {
    pub variant: DescriptionVariant,
    pub sha256: String,
    pub byte_length: u64,
    pub content: String,
}

/// Loads the candidate bytes without trimming or newline conversion.
pub fn compact_v1_bytes() -> &'static [u8] {
    COMPACT_V1
}

/// Obtains the current production description through the tool trait.
pub fn current_bytes() -> &'static [u8] {
    ToolDefinition::description(&ApplyPatchTool).as_bytes()
}

/// Returns the exact content and identity for a description variant.
pub fn load(variant: DescriptionVariant) -> FrozenDescription {
    let bytes = match variant {
        DescriptionVariant::Current => current_bytes(),
        DescriptionVariant::CompactV1 => compact_v1_bytes(),
    };
    FrozenDescription {
        variant,
        sha256: sha256_hex(bytes),
        byte_length: u64::try_from(bytes.len()).expect("description length fits u64"),
        content: String::from_utf8(bytes.to_vec()).expect("frozen descriptions must be UTF-8"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_has_exactly_one_trailing_lf() {
        assert!(compact_v1_bytes().ends_with(b"```\n"));
        assert!(!compact_v1_bytes().ends_with(b"```\n\n"));
    }

    #[test]
    fn current_is_read_from_production_tool() {
        assert_eq!(
            load(DescriptionVariant::Current).content,
            ApplyPatchTool.description()
        );
    }
}
