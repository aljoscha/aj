use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::Value;

/// Derive a JSON schema that is useful as the `input_schema` of a Claude tool
/// definition.
pub(crate) fn derive_schema<T: JsonSchema>() -> Value {
    let generator = SchemaSettings::default()
        .with(|s| {
            // Don't need the meta schema link, keeping it minimal.
            s.meta_schema = None;
        })
        .into_generator();
    let mut schema = generator.into_root_schema_for::<T>();

    // We don't want the title in there, keep it minimal.
    schema.remove("title");

    serde_json::to_value(&schema).expect("invalid input object")
}
