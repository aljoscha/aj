//! Regenerate the bundled models.dev seed at `data/models.json`.
//!
//! The fetch is kept explicit so the step is reproducible and works
//! offline against a saved payload:
//!
//! ```sh
//! curl -fsSL --compressed https://models.dev/api.json -o /tmp/models-dev.json
//! cargo run -p aj-models --example regen_seed -- /tmp/models-dev.json
//! ```
//!
//! Writes the models.dev-only baseline (no OpenRouter rows, no Codex
//! splice; Codex is spliced at load time) to `<crate>/data/models.json`.
//! An optional second payload, Codex's pinned models-manager/models.json,
//! updates speed support and defaults in data/codex.json. Mode prices come
//! from matching models.dev OpenAI entries.

use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: regen_seed <models.dev api.json> [pinned Codex models.json]");
    let body = std::fs::read_to_string(&path).expect("read models.dev payload");
    let catalog =
        aj_models::refresh::build_seed_from_models_dev(&body).expect("build seed catalog");
    if let Some(codex_path) = args.next() {
        let body = std::fs::read_to_string(codex_path).expect("read pinned Codex payload");
        let mut seed = aj_models::registry::CodexSeedFile {
            models: aj_models::registry::bundled_codex_seed(),
        };
        aj_models::refresh::update_codex_speed_metadata(&mut seed.models, &body, &catalog.models)
            .expect("normalize Codex speed metadata");
        let json = serde_json::to_string_pretty(&seed).expect("serialize Codex seed");
        let dest = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/codex.json");
        std::fs::write(dest, format!("{json}\n")).expect("write Codex seed");
    }
    let json = serde_json::to_string_pretty(&catalog).expect("serialize catalog");
    let dest = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/models.json");
    std::fs::write(&dest, format!("{json}\n")).expect("write seed");
    eprintln!(
        "wrote {} models to {}",
        catalog.models.len(),
        dest.display()
    );
}
