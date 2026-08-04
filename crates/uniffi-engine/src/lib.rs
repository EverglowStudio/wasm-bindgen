//! Programmatic wasm-bindgen frontend for UniFFI's validated bridge plan.
//!
//! This crate accepts only the engine-neutral, in-memory [`BridgePlan`] and a
//! structured wasm carrier plan.  It does not parse UniFFI component metadata,
//! scan or rewrite Rust source, invoke an external CLI, or persist artifact
//! metadata.  Generated local adapters still pass through wasm-bindgen's real
//! ABI traits, descriptor functions, rustc/linking, and cli-support post-link
//! pipeline.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, ToTokens};
use uniffi_js_abi::{AsyncKind, NamedTypeKind, OperationId, ScalarType, TypeSourceKey, ValueType};
use uniffi_js_engine_schema::{BridgePlan, EngineKind};
use wasm_bindgen_cli_support::{
    Bindgen, BindingSurface, UniFfiBackendConfig, UniFfiBackendOperation,
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
                .any(|segment| !valid_rust_identifier(segment))
        {
            return Err(EngineError::InvalidRustPath(segments.join("::")));
        }
        Ok(Self(segments))
    }

    fn tokens(&self) -> TokenStream {
        let segments = self
            .0
            .iter()
            .map(|segment| Ident::new(segment, Span::call_site()))
            .collect::<Vec<_>>();
        quote!(#(#segments)::* )
    }
}

fn valid_rust_identifier(value: &str) -> bool {
    syn_identifier(value).is_some_and(|identifier| identifier.to_string() == value)
}

fn syn_identifier(value: &str) -> Option<Ident> {
    if value.is_empty() {
        return None;
    }
    std::panic::catch_unwind(|| Ident::new(value, Span::call_site())).ok()
}

/// A carrier with a real wasm-bindgen ABI implementation.
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

/// One already-lowered local adapter from the UniFFI Rust bridge plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmOperationPlan {
    pub operation_id: OperationId,
    pub rust_call: RustPath,
    pub arguments: Vec<WasmCarrier>,
    pub return_carrier: Option<WasmCarrier>,
    pub fallible: bool,
}

#[derive(Clone, Debug)]
struct ValidatedOperation {
    plan: WasmOperationPlan,
    async_kind: AsyncKind,
    raw_export_name: String,
}

/// Fully validated programmatic input for one wasm engine surface.
#[derive(Clone, Debug)]
pub struct WasmEnginePlan {
    factory_export_name: String,
    operations: Vec<ValidatedOperation>,
}

impl WasmEnginePlan {
    pub fn build(
        bridge_plan: &BridgePlan,
        operations: Vec<WasmOperationPlan>,
    ) -> Result<Self, EngineError> {
        Self::with_factory(bridge_plan, DEFAULT_BACKEND_FACTORY, operations)
    }

    pub fn with_factory(
        bridge_plan: &BridgePlan,
        factory_export_name: impl Into<String>,
        operations: Vec<WasmOperationPlan>,
    ) -> Result<Self, EngineError> {
        if !bridge_plan
            .targets()
            .iter()
            .any(|target| target.engine == EngineKind::WasmBindgen)
        {
            return Err(EngineError::MissingWasmTarget);
        }

        let mut supplied = BTreeMap::new();
        for operation in operations {
            let operation_id = operation.operation_id;
            if supplied.insert(operation_id, operation).is_some() {
                return Err(EngineError::DuplicateOperation(operation_id.index()));
            }
        }

        let named_types = bridge_plan
            .types()
            .iter()
            .map(|ty| (ty.definition.source_key.clone(), &ty.definition.kind))
            .collect::<BTreeMap<_, _>>();
        let mut validated = Vec::with_capacity(bridge_plan.operations().len());
        for bridge_operation in bridge_plan.operations() {
            let operation_id = bridge_operation.operation.id;
            let plan = supplied
                .remove(&operation_id)
                .ok_or(EngineError::MissingOperation(operation_id.index()))?;
            let signature = &bridge_operation.operation.definition.signature;
            if plan.arguments.len() != signature.arguments.len() {
                return Err(EngineError::ArgumentCount {
                    operation_id: operation_id.index(),
                    expected: signature.arguments.len(),
                    actual: plan.arguments.len(),
                });
            }
            for (index, (carrier, argument)) in
                plan.arguments.iter().zip(&signature.arguments).enumerate()
            {
                validate_carrier(*carrier, &argument.ty, &named_types).map_err(|reason| {
                    EngineError::CarrierMismatch {
                        operation_id: operation_id.index(),
                        position: format!("argument[{index}]"),
                        reason,
                    }
                })?;
            }
            match (&plan.return_carrier, &signature.return_type) {
                (None, None) => {}
                (Some(carrier), Some(value)) => {
                    validate_carrier(*carrier, value, &named_types).map_err(|reason| {
                        EngineError::CarrierMismatch {
                            operation_id: operation_id.index(),
                            position: "return".to_owned(),
                            reason,
                        }
                    })?;
                }
                _ => {
                    return Err(EngineError::CarrierMismatch {
                        operation_id: operation_id.index(),
                        position: "return".to_owned(),
                        reason: "return carrier presence differs from BridgePlan".to_owned(),
                    });
                }
            }
            if plan.fallible != signature.throws.is_some() {
                return Err(EngineError::FallibilityMismatch(operation_id.index()));
            }
            validated.push(ValidatedOperation {
                plan,
                async_kind: signature.async_kind,
                raw_export_name: format!("__uniffi_operation_{}", operation_id.index()),
            });
        }
        if let Some(extra) = supplied.keys().next() {
            return Err(EngineError::UnknownOperation(extra.index()));
        }

        // Reuse cli-support's identifier/dense-table validation rather than
        // maintaining a second export-policy implementation here.
        let factory_export_name = factory_export_name.into();
        backend_config(&factory_export_name, &validated)?;
        Ok(Self {
            factory_export_name,
            operations: validated,
        })
    }

    pub fn configure_bindgen(&self, bindgen: &mut Bindgen) -> Result<(), EngineError> {
        let config = backend_config(&self.factory_export_name, &self.operations)?;
        bindgen
            .typescript(false)
            .binding_surface(BindingSurface::UniFfiBackend(config));
        Ok(())
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
                operation.plan.operation_id.index(),
                operation.raw_export_name.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| EngineError::Surface(error.to_string()))?;
    UniFfiBackendConfig::new(factory_export_name, operations)
        .map_err(|error| EngineError::Surface(error.to_string()))
}

fn validate_carrier(
    carrier: WasmCarrier,
    value: &ValueType,
    named_types: &BTreeMap<TypeSourceKey, &NamedTypeKind>,
) -> Result<(), String> {
    if carrier == WasmCarrier::JsValue {
        return Ok(());
    }
    let expected = match value {
        ValueType::Scalar(scalar) => match scalar {
            ScalarType::Bool => WasmCarrier::Bool,
            ScalarType::I8 => WasmCarrier::I8,
            ScalarType::U8 => WasmCarrier::U8,
            ScalarType::I16 => WasmCarrier::I16,
            ScalarType::U16 => WasmCarrier::U16,
            ScalarType::I32 => WasmCarrier::I32,
            ScalarType::U32 => WasmCarrier::U32,
            ScalarType::I64 => WasmCarrier::I64,
            ScalarType::U64 => WasmCarrier::U64,
            ScalarType::F32 => WasmCarrier::F32,
            ScalarType::F64 => WasmCarrier::F64,
            ScalarType::String => WasmCarrier::String,
            ScalarType::Bytes => WasmCarrier::Bytes,
        },
        ValueType::Named(key) if matches!(named_types.get(key), Some(NamedTypeKind::Object)) => {
            WasmCarrier::OpaqueHandle
        }
        ValueType::InputStream(_) | ValueType::OutputStream(_) => WasmCarrier::OpaqueHandle,
        _ => {
            return Err(
                "structured/optional collection values must use a real JsValue carrier".to_owned(),
            );
        }
    };
    if carrier == expected {
        Ok(())
    } else {
        Err(format!("expected {expected:?}, found {carrier:?}"))
    }
}

/// Tokens emitted by macro-support for one operation.  These tokens include
/// the real raw shim, custom-section bytes, and `WasmDescribe` descriptor.
#[derive(Clone, Debug)]
pub struct ExpandedOperation {
    pub operation_id: OperationId,
    pub raw_export_name: String,
    pub tokens: TokenStream,
}

fn expand_operation(
    builder: &ExpansionBuilder,
    operation: &ValidatedOperation,
) -> Result<ExpandedOperation, EngineError> {
    let operation_id = operation.plan.operation_id.index();
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
    let await_call = (operation.async_kind == AsyncKind::Async).then(|| quote!(.await));
    let async_token = (operation.async_kind == AsyncKind::Async).then(|| quote!(async));
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
        operation_id: operation.plan.operation_id,
        raw_export_name: operation.raw_export_name.clone(),
        tokens,
    })
}

#[derive(Debug, Eq, PartialEq)]
pub enum EngineError {
    MissingWasmTarget,
    InvalidRustPath(String),
    DuplicateOperation(u32),
    MissingOperation(u32),
    UnknownOperation(u32),
    ArgumentCount {
        operation_id: u32,
        expected: usize,
        actual: usize,
    },
    CarrierMismatch {
        operation_id: u32,
        position: String,
        reason: String,
    },
    FallibilityMismatch(u32),
    Surface(String),
    Expansion(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingWasmTarget => {
                formatter.write_str("BridgePlan does not target wasm-bindgen")
            }
            Self::InvalidRustPath(path) => {
                write!(formatter, "invalid structured Rust path `{path}`")
            }
            Self::DuplicateOperation(id) => write!(formatter, "duplicate wasm operation {id}"),
            Self::MissingOperation(id) => write!(formatter, "missing wasm operation {id}"),
            Self::UnknownOperation(id) => write!(formatter, "unknown wasm operation {id}"),
            Self::ArgumentCount {
                operation_id,
                expected,
                actual,
            } => write!(
                formatter,
                "operation {operation_id} expects {expected} carriers, found {actual}"
            ),
            Self::CarrierMismatch {
                operation_id,
                position,
                reason,
            } => write!(
                formatter,
                "operation {operation_id} {position} carrier mismatch: {reason}"
            ),
            Self::FallibilityMismatch(id) => {
                write!(
                    formatter,
                    "operation {id} fallibility differs from BridgePlan"
                )
            }
            Self::Surface(message) => write!(formatter, "invalid backend surface: {message}"),
            Self::Expansion(message) => write!(formatter, "macro expansion failed: {message}"),
        }
    }
}

impl Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;
    use uniffi_js_abi::{
        assign_component_ids, assign_operation_ids, ArgumentDefinition, ComponentDefinition,
        ComponentKey, OperationDefinition, OperationKind, OperationOwner, OperationSignature,
        OperationSourceKey, Ownership,
    };
    use uniffi_js_engine_schema::{
        BridgePlanInput, Capability, EngineCapabilities, PlannedOperation,
    };

    fn bridge_plan() -> BridgePlan {
        let component = ComponentKey::new("fixture").unwrap();
        let components =
            assign_component_ids([ComponentDefinition::new(component.clone(), "fixture").unwrap()])
                .unwrap();
        let operations = assign_operation_ids([OperationDefinition::new(
            OperationSourceKey::new(
                component,
                OperationOwner::Namespace,
                OperationKind::Function,
                "increment",
            )
            .unwrap(),
            "increment",
            "fixture.increment",
            "uniffi_fixture_increment",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "value",
                    ValueType::Scalar(ScalarType::I32),
                    Ownership::Owned,
                )
                .unwrap()],
                return_type: Some(ValueType::Scalar(ScalarType::I32)),
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap()])
        .unwrap();
        BridgePlan::build(BridgePlanInput {
            components,
            types: vec![],
            operations: operations.into_iter().map(PlannedOperation::new).collect(),
            callbacks: vec![],
            streams: vec![],
            targets: vec![EngineCapabilities::new(
                EngineKind::WasmBindgen,
                [Capability::Primitive, Capability::SyncCall],
            )],
        })
        .unwrap()
    }

    fn operation(carrier: WasmCarrier) -> WasmOperationPlan {
        WasmOperationPlan {
            operation_id: OperationId::new(0),
            rust_call: RustPath::new(["fixture".to_owned(), "increment".to_owned()]).unwrap(),
            arguments: vec![carrier],
            return_carrier: Some(WasmCarrier::I32),
            fallible: false,
        }
    }

    #[test]
    fn structured_plan_expands_real_wasm_descriptors_and_configures_factory() {
        let plan =
            WasmEnginePlan::build(&bridge_plan(), vec![operation(WasmCarrier::I32)]).unwrap();
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

        let mut bindgen = Bindgen::new();
        bindgen.web(true).unwrap();
        plan.configure_bindgen(&mut bindgen).unwrap();
        assert!(matches!(
            bindgen.selected_binding_surface(),
            BindingSurface::UniFfiBackend(_)
        ));
    }

    #[test]
    fn carrier_plan_cannot_bypass_wasm_abi_shape() {
        let error = WasmEnginePlan::build(&bridge_plan(), vec![operation(WasmCarrier::String)])
            .unwrap_err();
        assert!(matches!(error, EngineError::CarrierMismatch { .. }));
    }
}
