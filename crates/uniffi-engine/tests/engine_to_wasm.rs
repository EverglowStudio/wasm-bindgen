use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use wasm_bindgen_macro_support::ExpansionContext;
use wasm_bindgen_uniffi_engine::{
    PostLinkTarget, RustPath, WasmAsyncKind, WasmCarrier, WasmEnginePlan, WasmOperationPlan,
    DEFAULT_BACKEND_FACTORY,
};

const FIXTURE_NAME: &str = "uniffi_wasm_engine_fixture";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_owned()
}

fn engine_plan() -> WasmEnginePlan {
    let operation = |operation_id: u32,
                     name: &str,
                     arguments: Vec<WasmCarrier>,
                     return_carrier: Option<WasmCarrier>,
                     async_kind: WasmAsyncKind,
                     fallible: bool| WasmOperationPlan {
        operation_id,
        rust_call: RustPath::new(["fixture".to_owned(), name.to_owned()]).unwrap(),
        arguments,
        return_carrier,
        async_kind,
        fallible,
    };

    let mut operations = vec![WasmOperationPlan {
        operation_id: 0,
        // Compile a path containing both the legal `crate` root and raw
        // identifiers.  This keeps raw-name handling covered by the real
        // wasm32 fixture rather than only by token-string assertions.
        rust_call: RustPath::new([
            "crate".to_owned(),
            "r#type".to_owned(),
            "r#Trait".to_owned(),
        ])
        .unwrap(),
        arguments: vec![WasmCarrier::String, WasmCarrier::Bytes],
        return_carrier: Some(WasmCarrier::String),
        async_kind: WasmAsyncKind::Sync,
        fallible: false,
    }];
    operations.extend([
        operation(
            1,
            "b_async_bytes",
            vec![WasmCarrier::String],
            Some(WasmCarrier::Bytes),
            WasmAsyncKind::Async,
            false,
        ),
        operation(
            2,
            "c_make_callback",
            vec![WasmCarrier::String],
            Some(WasmCarrier::JsValue),
            WasmAsyncKind::Sync,
            false,
        ),
        operation(
            3,
            "d_sync_infallible",
            vec![WasmCarrier::String],
            Some(WasmCarrier::String),
            WasmAsyncKind::Sync,
            false,
        ),
        operation(
            4,
            "e_sync_fallible",
            vec![WasmCarrier::String],
            Some(WasmCarrier::String),
            WasmAsyncKind::Sync,
            true,
        ),
        operation(
            5,
            "f_async_infallible",
            vec![WasmCarrier::String],
            Some(WasmCarrier::String),
            WasmAsyncKind::Async,
            false,
        ),
        operation(
            6,
            "g_async_fallible",
            vec![WasmCarrier::String],
            Some(WasmCarrier::String),
            WasmAsyncKind::Async,
            true,
        ),
    ]);
    WasmEnginePlan::build(operations).unwrap()
}

fn fixture_source(plan: &WasmEnginePlan) -> String {
    let context = ExpansionContext::new(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        FIXTURE_NAME,
        "1.0.0",
        Vec::<String>::new(),
        "wasm32-unknown-unknown",
    )
    .unwrap();
    let adapters = plan
        .expand(context)
        .unwrap()
        .into_iter()
        .map(|operation| operation.tokens.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"
use wasm_bindgen::prelude::*;

mod fixture {{
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsValue;

    pub fn a_roundtrip(text: String, bytes: Vec<u8>) -> String {{
        format!("{{text}}:{{}}", bytes.iter().copied().map(u32::from).sum::<u32>())
    }}

    pub async fn b_async_bytes(text: String) -> Vec<u8> {{
        text.into_bytes()
    }}

    pub fn c_make_callback(prefix: String) -> JsValue {{
        Closure::<dyn FnMut(String) -> String>::new(move |input| format!("{{prefix}}{{input}}"))
            .into_js_value()
    }}

    pub fn d_sync_infallible(text: String) -> String {{
        format!("sync:{{text}}")
    }}

    pub fn e_sync_fallible(text: String) -> Result<String, JsValue> {{
        if text == "reject" {{
            Err(JsValue::from_str("sync callback rejected"))
        }} else {{
            Ok(format!("sync-fallible:{{text}}"))
        }}
    }}

    pub async fn f_async_infallible(text: String) -> String {{
        format!("async:{{text}}")
    }}

    pub async fn g_async_fallible(text: String) -> Result<String, JsValue> {{
        if text == "reject" {{
            Err(JsValue::from_str("async callback rejected"))
        }} else {{
            Ok(format!("async-fallible:{{text}}"))
        }}
    }}
}}

mod r#type {{
    pub fn r#Trait(text: String, bytes: Vec<u8>) -> String {{
        format!("{{text}}:{{}}", bytes.iter().copied().map(u32::from).sum::<u32>())
    }}
}}

{adapters}
"#
    )
}

fn build_fixture(temp: &TempDir, plan: &WasmEnginePlan) -> PathBuf {
    let root = repo_root();
    let project = temp.path().join("fixture");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/lib.rs"), fixture_source(plan)).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        format!(
            r#"
[package]
name = "{FIXTURE_NAME}"
version = "1.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = {{ path = "{}" }}
wasm-bindgen-futures = {{ path = "{}" }}
js-sys = {{ path = "{}" }}

[workspace]
"#,
            root.display(),
            root.join("crates/futures").display(),
            root.join("crates/js-sys").display(),
        ),
    )
    .unwrap();

    let output = Command::new("cargo")
        .current_dir(&project)
        .arg("build")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    assert_command_success("fixture cargo build", &output);
    root.join("target/wasm32-unknown-unknown/debug")
        .join(FIXTURE_NAME)
        .with_extension("wasm")
}

fn assert_command_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn runtime_script(module_name: &str, web: bool) -> String {
    let imports = if web {
        format!(
            "import init, * as api from './{module_name}.js';\n\
             import {{ readFileSync }} from 'node:fs';\n\
             await init(readFileSync(new URL('./{module_name}_bg.wasm', import.meta.url)));"
        )
    } else {
        format!("import * as api from './{module_name}.js';")
    };
    format!(
        r#"
import assert from 'node:assert/strict';
{imports}

for (const raw of [
    '__uniffi_operation_0',
    '__uniffi_operation_1',
    '__uniffi_operation_2',
    '__uniffi_operation_3',
    '__uniffi_operation_4',
    '__uniffi_operation_5',
    '__uniffi_operation_6',
]) {{
    assert.equal(Object.hasOwn(api, raw), false);
}}
assert.equal(typeof api.{DEFAULT_BACKEND_FACTORY}, 'function');
const uniffiExports = Object.keys(api).filter((name) => name.includes('uniffi'));
assert.deepEqual(uniffiExports, ['{DEFAULT_BACKEND_FACTORY}']);

const backend = api.{DEFAULT_BACKEND_FACTORY}();
assert.equal(backend.operationCount, 7);
assert.equal(backend.call(0, 'sum', new Uint8Array([1, 2, 3, 4])), 'sum:10');
const pending = backend.call(1, 'bytes');
assert.equal(typeof pending.then, 'function');
assert.deepEqual(Array.from(await pending), [98, 121, 116, 101, 115]);
const callback = backend.call(2, 'prefix:');
assert.equal(typeof callback, 'function');
assert.equal(callback('value'), 'prefix:value');
assert.equal(backend.call(3, 'value'), 'sync:value');
assert.equal(backend.call(4, 'value'), 'sync-fallible:value');
assert.throws(() => backend.call(4, 'reject'));
assert.equal(await backend.call(5, 'value'), 'async:value');
assert.equal(await backend.call(6, 'value'), 'async-fallible:value');
await assert.rejects(backend.call(6, 'reject'));
"#
    )
}

fn generate_and_run_target(
    temp: &TempDir,
    plan: &WasmEnginePlan,
    wasm: &Path,
    target: PostLinkTarget,
) {
    let target_name = match target {
        PostLinkTarget::Web => "web",
        PostLinkTarget::Bundler => "bundler",
        PostLinkTarget::Node => "node",
    };
    let module_name = format!("{FIXTURE_NAME}_{target_name}");
    let output_dir = temp.path().join(target_name);
    let output = plan.post_link(wasm, &module_name, target).run().unwrap();
    assert!(output.typescript().is_none());
    assert!(!output
        .wasm_export_names()
        .iter()
        .any(|export| export.starts_with("__wbindgen_describe")));
    assert!(!output.js().contains("export function __uniffi_operation_"));
    assert!(!output.js().contains("exports.__uniffi_operation_"));
    output.emit(&output_dir).unwrap();

    let module_type = if target == PostLinkTarget::Node {
        r#"{"type":"commonjs"}"#
    } else {
        r#"{"type":"module"}"#
    };
    fs::write(output_dir.join("package.json"), module_type).unwrap();
    fs::write(
        output_dir.join("run.mjs"),
        runtime_script(&module_name, target == PostLinkTarget::Web),
    )
    .unwrap();
    let mut command = Command::new("node");
    if target == PostLinkTarget::Bundler {
        command.arg("--experimental-wasm-modules");
    }
    let result = command
        .arg("run.mjs")
        .current_dir(&output_dir)
        .output()
        .unwrap();
    assert_command_success(&format!("{target_name} factory runtime"), &result);
}

#[test]
fn engine_tokens_compile_postlink_and_run_for_every_loader_target() {
    let temp = tempfile::tempdir().unwrap();
    let plan = engine_plan();

    let context = ExpansionContext::new(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        FIXTURE_NAME,
        "1.0.0",
        Vec::<String>::new(),
        "wasm32-unknown-unknown",
    )
    .unwrap();
    let expanded = plan.expand(context).unwrap();
    let tokens = |operation_id: u32| {
        expanded
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .unwrap()
            .tokens
            .to_string()
    };
    let sync_infallible = tokens(3);
    assert!(sync_infallible.contains("pub fn"));
    assert!(!sync_infallible.contains("pub async fn"));
    assert!(sync_infallible.contains("-> String {"));
    assert!(!sync_infallible.contains("-> Result < String"));
    let sync_fallible = tokens(4);
    assert!(sync_fallible.contains("pub fn"));
    assert!(sync_fallible.contains("-> Result < String"));
    let async_infallible = tokens(5);
    assert!(async_infallible.contains("pub async fn"));
    assert!(async_infallible.contains("-> String {"));
    assert!(!async_infallible.contains("-> Result < String"));
    let async_fallible = tokens(6);
    assert!(async_fallible.contains("pub async fn"));
    assert!(async_fallible.contains("-> Result < String"));

    let wasm = build_fixture(&temp, &plan);

    for target in [
        PostLinkTarget::Web,
        PostLinkTarget::Bundler,
        PostLinkTarget::Node,
    ] {
        generate_and_run_target(&temp, &plan, &wasm, target);
    }
}
