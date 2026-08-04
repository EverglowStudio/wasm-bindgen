//! Programmatic wasm-bindgen lowering for UniFFI's JavaScript backend.
//!
//! This crate deliberately owns only the small amount of information needed
//! by wasm-bindgen.  UniFFI's metadata, type graph, capabilities, and target
//! selection are lowered by the caller before this API is invoked.  Keeping
//! that boundary local means this crate does not need to depend on a
//! particular UniFFI revision or on a serialized interchange format.
//!
//! The [`WasmEnginePlan`] is an in-memory plan.  [`PostLinkPlan`] is the only
//! post-link entry point: it keeps the cli-support `Bindgen` value private and
//! returns an engine-owned output wrapper.  No external `wasm-bindgen`
//! executable is inspected or invoked.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, ToTokens};
use wasm_bindgen_cli_support::{
    Bindgen, BindingSurface, Output as CliOutput, UniFfiBackendConfig, UniFfiBackendOperation,
};
use wasm_bindgen_macro_support::{ExpansionBuilder, ExpansionContext};

pub const DEFAULT_BACKEND_FACTORY: &str = "__uniffi_backend_factory";

/// A Rust path represented as validated identifiers rather than source text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustPath(Vec<String>);

impl RustPath {
    pub fn new(segments: impl IntoIterator<Item = String>) -> Result<Self, EngineError> {
        let segments = segments.into_iter().collect::<Vec<_>>();
        if segments.is_empty()
            || segments
                .iter()
                .enumerate()
                .any(|(index, segment)| !valid_rust_path_segment(segment, index))
        {
            return Err(EngineError::InvalidRustPath(segments.join("::")));
        }
        Ok(Self(segments))
    }

    fn tokens(&self) -> TokenStream {
        let segments = self
            .0
            .iter()
            .enumerate()
            .map(|(index, segment)| rust_path_ident(segment, index))
            .collect::<Vec<_>>();
        quote!(#(#segments)::* )
    }
}

fn valid_rust_path_segment(value: &str, index: usize) -> bool {
    // These path keywords are useful for generated adapters that live inside
    // the same crate, but they are only valid as a path's first segment.
    if matches!(value, "crate" | "self" | "super") {
        return index == 0;
    }

    if let Some(raw) = value.strip_prefix("r#") {
        return valid_raw_identifier(raw);
    }

    valid_plain_identifier(value)
}

fn valid_plain_identifier(value: &str) -> bool {
    if value.is_empty() || rust_keyword(value) {
        return false;
    }
    std::panic::catch_unwind(|| Ident::new(value, Span::call_site()))
        .map(|identifier| identifier.to_string() == value)
        .unwrap_or(false)
}

fn valid_raw_identifier(value: &str) -> bool {
    // Rust does not permit raw spellings of the path keywords below.  The
    // remaining keywords (and ordinary names such as `Trait`) are valid raw
    // identifiers and are emitted with `Ident::new_raw`.
    if value.is_empty() || matches!(value, "crate" | "self" | "super" | "Self") {
        return false;
    }
    std::panic::catch_unwind(|| Ident::new_raw(value, Span::call_site()))
        .map(|identifier| identifier.to_string() == format!("r#{value}"))
        .unwrap_or(false)
}

fn rust_path_ident(value: &str, index: usize) -> Ident {
    if matches!(value, "crate" | "self" | "super") {
        debug_assert_eq!(index, 0);
        return Ident::new(value, Span::call_site());
    }
    if let Some(raw) = value.strip_prefix("r#") {
        return Ident::new_raw(raw, Span::call_site());
    }
    Ident::new(value, Span::call_site())
}

fn rust_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "try"
            | "union"
    )
}

/// A local wasm carrier with a real wasm-bindgen ABI implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmCarrier {
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    String,
    Bytes,
    JsValue,
    /// Stable numeric ID for an object or stream lease owned by the facade.
    OpaqueHandle,
}

impl WasmCarrier {
    fn rust_type(self) -> TokenStream {
        match self {
            Self::Bool => quote!(bool),
            Self::I8 => quote!(i8),
            Self::U8 => quote!(u8),
            Self::I16 => quote!(i16),
            Self::U16 => quote!(u16),
            Self::I32 => quote!(i32),
            Self::U32 | Self::OpaqueHandle => quote!(u32),
            Self::I64 => quote!(i64),
            Self::U64 => quote!(u64),
            Self::F32 => quote!(f32),
            Self::F64 => quote!(f64),
            Self::String => quote!(String),
            Self::Bytes => quote!(Vec<u8>),
            Self::JsValue => quote!(wasm_bindgen::JsValue),
        }
    }
}

/// Whether a lowered wasm operation is synchronous or returns a future.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmAsyncKind {
    Sync,
    Async,
}

/// One already-lowered local adapter.
///
/// `operation_id` is intentionally a plain dense `u32`.  The UniFFI frontend
/// assigns it before entering this crate; this crate validates density and
/// uniqueness without knowing how that assignment was produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmOperationPlan {
    pub operation_id: u32,
    pub rust_call: RustPath,
    pub arguments: Vec<WasmCarrier>,
    pub return_carrier: Option<WasmCarrier>,
    pub async_kind: WasmAsyncKind,
    pub fallible: bool,
}

#[derive(Clone, Debug)]
struct ValidatedOperation {
    plan: WasmOperationPlan,
    raw_export_name: String,
}

/// Fully validated programmatic input for one wasm engine surface.
#[derive(Clone, Debug)]
pub struct WasmEnginePlan {
    factory_export_name: String,
    operations: Vec<ValidatedOperation>,
}

impl WasmEnginePlan {
    pub fn build(operations: Vec<WasmOperationPlan>) -> Result<Self, EngineError> {
        Self::with_factory(DEFAULT_BACKEND_FACTORY, operations)
    }

    pub fn with_factory(
        factory_export_name: impl Into<String>,
        operations: Vec<WasmOperationPlan>,
    ) -> Result<Self, EngineError> {
        let factory_export_name = factory_export_name.into();
        let mut supplied = BTreeMap::new();
        for operation in operations {
            let operation_id = operation.operation_id;
            if supplied.insert(operation_id, operation).is_some() {
                return Err(EngineError::DuplicateOperation(operation_id));
            }
        }

        let mut validated = Vec::with_capacity(supplied.len());
        for (expected, (operation_id, plan)) in supplied.into_iter().enumerate() {
            let expected = u32::try_from(expected).map_err(|_| EngineError::TooManyOperations)?;
            if operation_id != expected {
                return Err(EngineError::NonDenseOperation {
                    expected,
                    found: operation_id,
                });
            }
            validated.push(ValidatedOperation {
                raw_export_name: format!("__uniffi_operation_{operation_id}"),
                plan,
            });
        }

        // Reuse cli-support's identifier/dense-table validation for the one
        // backend factory, without exposing its config or Bindgen type.
        backend_config(&factory_export_name, &validated)?;
        Ok(Self {
            factory_export_name,
            operations: validated,
        })
    }

    /// Prepare an in-process post-link operation for a Wasm file.
    ///
    /// The returned plan owns all cli-support configuration.  Callers do not
    /// need, and cannot access, `wasm_bindgen_cli_support::Bindgen`.
    pub fn post_link<P: AsRef<Path>>(
        &self,
        wasm_path: P,
        module_name: impl Into<String>,
        target: PostLinkTarget,
    ) -> PostLinkPlan {
        PostLinkPlan {
            wasm_path: wasm_path.as_ref().to_owned(),
            module_name: module_name.into(),
            target,
            backend: backend_config(&self.factory_export_name, &self.operations)
                .expect("validated engine plan has a valid backend surface"),
        }
    }

    pub fn expand(&self, context: ExpansionContext) -> Result<Vec<ExpandedOperation>, EngineError> {
        let builder = ExpansionBuilder::new(context);
        self.operations
            .iter()
            .map(|operation| expand_operation(&builder, operation))
            .collect()
    }

    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }
}

fn backend_config(
    factory_export_name: &str,
    operations: &[ValidatedOperation],
) -> Result<UniFfiBackendConfig, EngineError> {
    let operations = operations
        .iter()
        .map(|operation| {
            UniFfiBackendOperation::new(
                operation.plan.operation_id,
                operation.raw_export_name.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| EngineError::Surface(error.to_string()))?;
    UniFfiBackendConfig::new(factory_export_name, operations)
        .map_err(|error| EngineError::Surface(error.to_string()))
}

/// Loader mode used by [`PostLinkPlan`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostLinkTarget {
    Web,
    Bundler,
    Node,
}

/// A configured in-process wasm-bindgen post-link operation.
#[derive(Clone, Debug)]
pub struct PostLinkPlan {
    wasm_path: PathBuf,
    module_name: String,
    target: PostLinkTarget,
    backend: UniFfiBackendConfig,
}

impl PostLinkPlan {
    /// Run wasm-bindgen in process and return an engine-owned output.
    pub fn run(&self) -> Result<PostLinkOutput, EngineError> {
        let mut bindgen = Bindgen::new();
        bindgen
            .input_path(&self.wasm_path)
            .out_name(&self.module_name)
            .typescript(false)
            .binding_surface(BindingSurface::UniFfiBackend(self.backend.clone()));
        match self.target {
            PostLinkTarget::Web => bindgen
                .web(true)
                .map_err(|error| EngineError::PostLink(error.to_string()))?,
            PostLinkTarget::Bundler => bindgen
                .bundler(true)
                .map_err(|error| EngineError::PostLink(error.to_string()))?,
            PostLinkTarget::Node => bindgen
                .nodejs(true)
                .map_err(|error| EngineError::PostLink(error.to_string()))?,
        };
        bindgen
            .generate_output()
            .map(|output| PostLinkOutput { output })
            .map_err(|error| EngineError::PostLink(error.to_string()))
    }
}

/// Engine-owned result of [`PostLinkPlan::run`].
///
/// The wrapper intentionally exposes strings, bytes, and a small emit helper
/// instead of leaking cli-support's `Output`/`Bindgen` types across the engine
/// boundary.
pub struct PostLinkOutput {
    output: CliOutput,
}

impl PostLinkOutput {
    pub fn js(&self) -> &str {
        self.output.js()
    }

    pub fn typescript(&self) -> Option<&str> {
        self.output.ts()
    }

    pub fn wasm_bytes(&mut self) -> Vec<u8> {
        self.output.wasm_mut().emit_wasm()
    }

    pub fn wasm_export_names(&self) -> Vec<String> {
        self.output
            .wasm()
            .exports
            .iter()
            .map(|export| export.name.clone())
            .collect()
    }

    /// Emit the complete loader package to `out_dir`.
    pub fn emit(mut self, out_dir: impl AsRef<Path>) -> Result<(), EngineError> {
        self.output
            .emit(out_dir)
            .map_err(|error| EngineError::PostLink(error.to_string()))
    }
}

fn expand_operation(
    builder: &ExpansionBuilder,
    operation: &ValidatedOperation,
) -> Result<ExpandedOperation, EngineError> {
    let operation_id = operation.plan.operation_id;
    let rust_name = Ident::new(
        &format!("__uniffi_wasm_operation_{operation_id}"),
        Span::call_site(),
    );
    let raw_export = Ident::new(&operation.raw_export_name, Span::call_site());
    let call = operation.plan.rust_call.tokens();
    let arguments = operation
        .plan
        .arguments
        .iter()
        .enumerate()
        .map(|(index, carrier)| {
            let name = Ident::new(&format!("arg{index}"), Span::call_site());
            let ty = carrier.rust_type();
            (name, ty)
        })
        .collect::<Vec<_>>();
    let argument_names = arguments.iter().map(|(name, _)| name);
    let argument_declarations = arguments.iter().map(|(name, ty)| quote!(#name: #ty));
    let return_type = operation
        .plan
        .return_carrier
        .map(WasmCarrier::rust_type)
        .unwrap_or_else(|| quote!(()));
    let return_type = if operation.plan.fallible {
        quote!(Result<#return_type, wasm_bindgen::JsValue>)
    } else {
        return_type
    };
    let await_call = (operation.plan.async_kind == WasmAsyncKind::Async).then(|| quote!(.await));
    let async_token = (operation.plan.async_kind == WasmAsyncKind::Async).then(|| quote!(async));
    let input = quote! {
        pub #async_token fn #rust_name(#(#argument_declarations),*) -> #return_type {
            #call(#(#argument_names),*)#await_call
        }
    };
    // cli-support's BindingSurface owns visibility.  Functions do not have a
    // macro-level `private` option; `skip_typescript` prevents raw declarations
    // and the surface pass suppresses their JS module exports.
    let attr = quote!(js_name = #raw_export, skip_typescript);
    let tokens = builder
        .expand(attr, input)
        .map_err(|diagnostic| EngineError::Expansion(diagnostic.into_token_stream().to_string()))?;
    Ok(ExpandedOperation {
        operation_id,
        raw_export_name: operation.raw_export_name.clone(),
        tokens,
    })
}

/// Tokens emitted by macro-support for one operation.  These tokens include
/// the real raw shim, custom-section bytes, and `WasmDescribe` descriptor.
#[derive(Clone, Debug)]
pub struct ExpandedOperation {
    pub operation_id: u32,
    pub raw_export_name: String,
    pub tokens: TokenStream,
}

#[derive(Debug, Eq, PartialEq)]
pub enum EngineError {
    InvalidRustPath(String),
    DuplicateOperation(u32),
    NonDenseOperation { expected: u32, found: u32 },
    TooManyOperations,
    Surface(String),
    Expansion(String),
    PostLink(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRustPath(path) => {
                write!(formatter, "invalid structured Rust path `{path}`")
            }
            Self::DuplicateOperation(id) => write!(formatter, "duplicate wasm operation {id}"),
            Self::NonDenseOperation { expected, found } => write!(
                formatter,
                "wasm operation IDs must be dense: expected {expected}, found {found}"
            ),
            Self::TooManyOperations => formatter.write_str("too many wasm operations"),
            Self::Surface(message) => write!(formatter, "invalid backend surface: {message}"),
            Self::Expansion(message) => write!(formatter, "macro expansion failed: {message}"),
            Self::PostLink(message) => write!(formatter, "wasm post-link failed: {message}"),
        }
    }
}

impl Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(id: u32) -> WasmOperationPlan {
        WasmOperationPlan {
            operation_id: id,
            rust_call: RustPath::new(["fixture".to_owned(), "increment".to_owned()]).unwrap(),
            arguments: vec![WasmCarrier::I32],
            return_carrier: Some(WasmCarrier::I32),
            async_kind: WasmAsyncKind::Sync,
            fallible: false,
        }
    }

    #[test]
    fn engine_plan_owns_dense_operation_validation() {
        let plan = WasmEnginePlan::build(vec![operation(1)]).unwrap_err();
        assert_eq!(
            plan,
            EngineError::NonDenseOperation {
                expected: 0,
                found: 1
            }
        );

        let duplicate = WasmEnginePlan::build(vec![operation(0), operation(0)]).unwrap_err();
        assert_eq!(duplicate, EngineError::DuplicateOperation(0));
    }

    #[test]
    fn structured_plan_expands_real_wasm_descriptors() {
        let plan = WasmEnginePlan::build(vec![operation(0)]).unwrap();
        let context = ExpansionContext::new(
            std::env::current_dir().unwrap(),
            "fixture",
            "1.0.0",
            Vec::<String>::new(),
            "wasm32-unknown-unknown",
        )
        .unwrap();
        let expanded = plan.expand(context).unwrap();
        assert_eq!(expanded.len(), 1);
        let tokens = expanded[0].tokens.to_string();
        assert!(tokens.contains("FromWasmAbi"));
        assert!(tokens.contains("ReturnWasmAbi"));
        assert!(tokens.contains("WasmDescribe"));
        assert!(tokens.contains("__wbindgen_describe"));
        assert!(!tokens.contains("compile_error"));
    }

    #[test]
    fn paths_reject_keywords_and_non_identifiers() {
        assert!(RustPath::new(["fn".to_owned()]).is_err());
        assert!(RustPath::new(["crate".to_owned(), "fixture".to_owned()]).is_ok());
        assert!(RustPath::new(["fixture".to_owned(), "self".to_owned()]).is_err());
        assert!(RustPath::new(["r#type".to_owned(), "r#Trait".to_owned()]).is_ok());
        assert!(RustPath::new(["r#".to_owned()]).is_err());
        assert!(RustPath::new(["r#self".to_owned()]).is_err());
        let raw = RustPath::new(["r#type".to_owned(), "r#Trait".to_owned()]).unwrap();
        assert_eq!(raw.tokens().to_string(), "r#type :: r#Trait");
        assert!(RustPath::new(["1not_ident".to_owned()]).is_err());
        assert!(RustPath::new(["fixture".to_owned(), "increment".to_owned()]).is_ok());
    }
}
