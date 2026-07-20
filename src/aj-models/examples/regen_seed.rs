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

use std::path::Path;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: regen_seed <path to models.dev api.json>");
    let body = std::fs::read_to_string(&path).expect("read models.dev payload");
    let catalog =
        aj_models::refresh::build_seed_from_models_dev(&body).expect("build seed catalog");
    let json = serde_json::to_string_pretty(&catalog).expect("serialize catalog");
    let dest = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/models.json");
    std::fs::write(&dest, format!("{json}\n")).expect("write seed");
    eprintln!(
        "wrote {} models to {}",
        catalog.models.len(),
        dest.display()
    );
}
