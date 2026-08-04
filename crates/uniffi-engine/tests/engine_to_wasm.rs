use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use uniffi_js_abi::{
    assign_component_ids, assign_operation_ids, assign_type_ids, ArgumentDefinition, AsyncKind,
    ComponentDefinition, ComponentKey, NamedTypeKind, OperationDefinition, OperationId,
    OperationKind, OperationOwner, OperationSignature, OperationSourceKey, Ownership, ScalarType,
    TypeDefinition, TypeSourceKey, ValueType,
};
use uniffi_js_engine_schema::{
    BridgePlan, BridgePlanInput, CallbackCallStyle, CallbackContract, CallbackErrorStyle,
    CallbackReentrancy, CallbackRetention, CallbackThreading, CallbackUseSite, Capability,
    EngineCapabilities, EngineKind, PlannedOperation, ValuePath,
};
use wasm_bindgen_cli_support::Bindgen;
use wasm_bindgen_macro_support::ExpansionContext;
use wasm_bindgen_uniffi_engine::{
    RustPath, WasmCarrier, WasmEnginePlan, WasmOperationPlan, DEFAULT_BACKEND_FACTORY,
};

const FIXTURE_NAME: &str = "uniffi_wasm_engine_fixture";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_owned()
}

fn operation(
    component: &ComponentKey,
    name: &str,
    arguments: Vec<ArgumentDefinition>,
    return_type: ValueType,
    async_kind: AsyncKind,
) -> OperationDefinition {
    OperationDefinition::new(
        OperationSourceKey::new(
            component.clone(),
            OperationOwner::Namespace,
            OperationKind::Function,
            name,
        )
        .unwrap(),
        name,
        format!("fixture.{name}"),
        format!("uniffi_fixture_{name}"),
        OperationSignature {
            arguments,
            return_type: Some(return_type),
            async_kind,
            throws: None,
        },
    )
    .unwrap()
}

fn argument(name: &str, ty: ValueType) -> ArgumentDefinition {
    ArgumentDefinition::new(name, ty, Ownership::Owned).unwrap()
}

fn engine_plan() -> WasmEnginePlan {
    let component = ComponentKey::new("fixture").unwrap();
    let callback_key = TypeSourceKey::new(component.clone(), "TextCallback").unwrap();
    let components =
        assign_component_ids([ComponentDefinition::new(component.clone(), "fixture").unwrap()])
            .unwrap();
    let types = assign_type_ids([TypeDefinition::new(
        callback_key.clone(),
        "TextCallback",
        NamedTypeKind::Callback,
    )
    .unwrap()])
    .unwrap();
    let operations = assign_operation_ids([
        operation(
            &component,
            "a_roundtrip",
            vec![
                argument("text", ValueType::Scalar(ScalarType::String)),
                argument("bytes", ValueType::Scalar(ScalarType::Bytes)),
            ],
            ValueType::Scalar(ScalarType::String),
            AsyncKind::Sync,
        ),
        operation(
            &component,
            "b_async_bytes",
            vec![argument("text", ValueType::Scalar(ScalarType::String))],
            ValueType::Scalar(ScalarType::Bytes),
            AsyncKind::Async,
        ),
        operation(
            &component,
            "c_make_callback",
            vec![argument("prefix", ValueType::Scalar(ScalarType::String))],
            ValueType::Named(callback_key),
            AsyncKind::Sync,
        ),
        OperationDefinition::new(
            OperationSourceKey::new(
                component.clone(),
                OperationOwner::Callback(
                    TypeSourceKey::new(component.clone(), "TextCallback").unwrap(),
                ),
                OperationKind::CallbackMethod,
                "call",
            )
            .unwrap(),
            "call",
            "fixture.TextCallback.call",
            "uniffi_fixture_text_callback_call",
            OperationSignature {
                arguments: vec![argument("text", ValueType::Scalar(ScalarType::String))],
                return_type: Some(ValueType::Scalar(ScalarType::String)),
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap(),
    ])
    .unwrap();
    assert_eq!(
        operations
            .iter()
            .map(|operation| operation.definition.public_name.as_str())
            .collect::<Vec<_>>(),
        ["a_roundtrip", "b_async_bytes", "c_make_callback", "call"]
    );

    let bridge_plan = BridgePlan::build(BridgePlanInput {
        components,
        types,
        operations: operations.into_iter().map(PlannedOperation::new).collect(),
        callbacks: vec![CallbackUseSite {
            operation_id: OperationId::new(2),
            callback_type: uniffi_js_abi::TypeId::new(0),
            path: ValuePath::return_value(),
            contract: CallbackContract {
                retention: CallbackRetention::Retained,
                threading: CallbackThreading::CallingThread,
                call_style: CallbackCallStyle::Sync,
                error_style: CallbackErrorStyle::Infallible,
                reentrancy: CallbackReentrancy::Allowed,
            },
        }],
        streams: vec![],
        targets: vec![EngineCapabilities::new(
            EngineKind::WasmBindgen,
            [
                Capability::Primitive,
                Capability::String,
                Capability::Bytes,
                Capability::SyncCall,
                Capability::AsyncCall,
                Capability::Callback,
                Capability::RetainedCallback,
                Capability::CallbackReentrancy,
            ],
        )],
    })
    .unwrap();

    WasmEnginePlan::build(
        &bridge_plan,
        vec![
            WasmOperationPlan {
                operation_id: OperationId::new(0),
                rust_call: RustPath::new(["fixture".to_owned(), "a_roundtrip".to_owned()]).unwrap(),
                arguments: vec![WasmCarrier::String, WasmCarrier::Bytes],
                return_carrier: Some(WasmCarrier::String),
                fallible: false,
            },
            WasmOperationPlan {
                operation_id: OperationId::new(1),
                rust_call: RustPath::new(["fixture".to_owned(), "b_async_bytes".to_owned()])
                    .unwrap(),
                arguments: vec![WasmCarrier::String],
                return_carrier: Some(WasmCarrier::Bytes),
                fallible: false,
            },
            WasmOperationPlan {
                operation_id: OperationId::new(2),
                rust_call: RustPath::new(["fixture".to_owned(), "c_make_callback".to_owned()])
                    .unwrap(),
                arguments: vec![WasmCarrier::String],
                return_carrier: Some(WasmCarrier::JsValue),
                fallible: false,
            },
            WasmOperationPlan {
                operation_id: OperationId::new(3),
                rust_call: RustPath::new(["fixture".to_owned(), "d_callback_method".to_owned()])
                    .unwrap(),
                arguments: vec![WasmCarrier::String],
                return_carrier: Some(WasmCarrier::String),
                fallible: false,
            },
        ],
    )
    .unwrap()
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

    pub fn d_callback_method(text: String) -> String {{
        text
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

for (const raw of ['__uniffi_operation_0', '__uniffi_operation_1', '__uniffi_operation_2', '__uniffi_operation_3']) {{
    assert.equal(Object.hasOwn(api, raw), false);
}}
assert.equal(typeof api.{DEFAULT_BACKEND_FACTORY}, 'function');
const uniffiExports = Object.keys(api).filter((name) => name.includes('uniffi'));
assert.deepEqual(uniffiExports, ['{DEFAULT_BACKEND_FACTORY}']);

const backend = api.{DEFAULT_BACKEND_FACTORY}();
assert.equal(backend.operationCount, 4);
assert.equal(backend.call(0, 'sum', new Uint8Array([1, 2, 3, 4])), 'sum:10');
const pending = backend.call(1, 'bytes');
assert.equal(typeof pending.then, 'function');
assert.deepEqual(Array.from(await pending), [98, 121, 116, 101, 115]);
const callback = backend.call(2, 'prefix:');
assert.equal(typeof callback, 'function');
assert.equal(callback('value'), 'prefix:value');
"#
    )
}

fn generate_and_run_target(temp: &TempDir, plan: &WasmEnginePlan, wasm: &Path, target: &str) {
    let module_name = format!("{FIXTURE_NAME}_{target}");
    let output_dir = temp.path().join(target);
    let mut bindgen = Bindgen::new();
    bindgen.input_path(wasm).out_name(&module_name);
    plan.configure_bindgen(&mut bindgen).unwrap();
    match target {
        "web" => {
            bindgen.web(true).unwrap();
        }
        "bundler" => {
            bindgen.bundler(true).unwrap();
        }
        "node" => {
            bindgen.nodejs(true).unwrap();
        }
        _ => unreachable!(),
    }

    let mut output = bindgen.generate_output().unwrap();
    assert!(output.ts().is_none());
    assert!(!output
        .wasm()
        .exports
        .iter()
        .any(|export| export.name.starts_with("__wbindgen_describe")));
    assert!(!output.js().contains("export function __uniffi_operation_"));
    assert!(!output.js().contains("exports.__uniffi_operation_"));
    output.emit(&output_dir).unwrap();

    let module_type = if target == "node" {
        r#"{"type":"commonjs"}"#
    } else {
        r#"{"type":"module"}"#
    };
    fs::write(output_dir.join("package.json"), module_type).unwrap();
    fs::write(
        output_dir.join("run.mjs"),
        runtime_script(&module_name, target == "web"),
    )
    .unwrap();
    let mut command = Command::new("node");
    if target == "bundler" {
        command.arg("--experimental-wasm-modules");
    }
    let result = command
        .arg("run.mjs")
        .current_dir(&output_dir)
        .output()
        .unwrap();
    assert_command_success(&format!("{target} factory runtime"), &result);
}

#[test]
fn engine_tokens_compile_postlink_and_run_for_every_loader_target() {
    let temp = tempfile::tempdir().unwrap();
    let plan = engine_plan();
    let wasm = build_fixture(&temp, &plan);

    for target in ["web", "bundler", "node"] {
        generate_and_run_target(&temp, &plan, &wasm, target);
    }
}
