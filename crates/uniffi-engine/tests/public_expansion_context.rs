use std::path::Path;

use wasm_bindgen_uniffi_engine::ExpansionContext;

#[test]
fn consumers_construct_an_explicit_expansion_context_from_engine_api() {
    let context = ExpansionContext::new(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        "fixture",
        "1.0.0",
        Vec::<String>::new(),
        "wasm32-unknown-unknown",
    );
    assert!(context.is_ok());
}
