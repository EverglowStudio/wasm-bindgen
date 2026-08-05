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

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, ToTokens};
use wasm_bindgen_cli_support::{
    Bindgen, BindingSurface, Output as CliOutput, UniFfiBackendAsyncKind,
    UniFfiBackendCallbackContract, UniFfiBackendCallbackReentrancy, UniFfiBackendCallbackRetention,
    UniFfiBackendCallbackThreading, UniFfiBackendCallbackUseSite, UniFfiBackendCarrier,
    UniFfiBackendConfig, UniFfiBackendOperation, UniFfiBackendOperationKind, UniFfiBackendResource,
    UniFfiBackendResourceExports, UniFfiBackendResourceHook, UniFfiBackendResourceOwnership,
    UniFfiBackendResourceUseSite, UniFfiBackendStreamDirection, UniFfiBackendStreamGroup,
    UniFfiBackendStreamSlot, UniFfiBackendValuePath, UniFfiBackendValuePathSegment,
};
use wasm_bindgen_macro_support::ExpansionBuilder;
/// Explicit context required by the public in-process expansion methods.
///
/// Re-exporting the type keeps consumers on the engine boundary; they do not
/// need to depend on the macro-support implementation crate directly.
pub use wasm_bindgen_macro_support::ExpansionContext;

/// Engine-owned projection of the canonical UniFFI teardown policy.  The
/// definition lives in cli-support because the generated JS session consumes
/// it directly; re-exporting it here keeps callers on the single in-memory
/// engine boundary and does not introduce a serialized schema.
pub use wasm_bindgen_cli_support::{ClosePolicy, DeadlineAction};

pub const DEFAULT_BACKEND_FACTORY: &str = "__uniffi_backend_factory";
const RELEASE_OBJECT_EXPORT: &str = "__uniffi_release_object";
const CLOSE_OUTPUT_STREAM_EXPORT: &str = "__uniffi_close_output_stream";

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

/// Mechanical operation kind copied from the canonical engine plan.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WasmOperationKind {
    Function,
    Constructor,
    Method,
    CallbackMethod,
    OutputStreamStart,
    OutputStreamNext,
    OutputStreamCancel,
    InputStreamPull,
    InputStreamCancel,
}

/// Dispatch class for a canonical Wasm operation.  `HostDispatched` entries
/// are represented in the UniFFI backend descriptor table but intentionally
/// do not expand a Rust/wasm-bindgen raw shim; the JavaScript session owns the
/// callback or foreign-input-stream protocol for those IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmOperationDispatch {
    NativeCall,
    HostDispatched,
}

impl WasmOperationDispatch {
    fn is_host_dispatched(self) -> bool {
        matches!(self, Self::HostDispatched)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmOwnership {
    Borrowed,
    Owned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmRustCarrier {
    Primitive,
    BigInt,
    Bytes,
    Timestamp,
    Duration,
    LocalAdapter,
    OpaqueHandle,
    CallbackProxy,
    InputStream,
    OutputStream,
    StreamStep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmScalarType {
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WasmRustType {
    Unit,
    Scalar(WasmScalarType),
    Timestamp,
    Duration,
    Path(RustPath),
    Option(Box<Self>),
    Sequence(Box<Self>),
    Map(Box<Self>, Box<Self>),
    Set(Box<Self>),
    Stream {
        item: Box<Self>,
        error: Box<Self>,
        is_send: bool,
    },
    InputStream {
        item: Box<Self>,
        error: Box<Self>,
        is_send: bool,
    },
    StreamStep {
        item: Box<Self>,
        error: Box<Self>,
    },
    Custom(Box<Self>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WasmConversionRecipe {
    Identity,
    Timestamp,
    Duration,
    BigInt,
    Bytes,
    Optional(Box<Self>),
    Sequence(Box<Self>),
    Map(Box<Self>, Box<Self>),
    Set(Box<Self>),
    Record(u32),
    Enum(u32),
    Error(u32),
    Object(u32),
    Custom(u32, Box<Self>),
    Callback(u32),
    InputStream { item: Box<Self>, error: Box<Self> },
    OutputStream { item: Box<Self>, error: Box<Self> },
    StreamStep { item: Box<Self>, error: Box<Self> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmValueBinding {
    pub rust_type: WasmRustType,
    pub carrier: WasmRustCarrier,
    pub abi_carrier: WasmCarrier,
    pub conversion: WasmConversionRecipe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmArgumentBinding {
    pub public_name: String,
    pub rust_name: String,
    pub rust_type: WasmRustType,
    pub carrier: WasmRustCarrier,
    pub abi_carrier: WasmCarrier,
    pub ownership: WasmOwnership,
    pub conversion: WasmConversionRecipe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmReceiverBinding {
    pub rust_type: WasmRustType,
    pub carrier: WasmRustCarrier,
    pub abi_carrier: WasmCarrier,
    pub ownership: WasmOwnership,
    pub conversion: WasmConversionRecipe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmReturnBinding {
    pub rust_type: WasmRustType,
    pub carrier: WasmRustCarrier,
    pub abi_carrier: WasmCarrier,
    pub ownership: WasmOwnership,
    pub conversion: WasmConversionRecipe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmObjectKind {
    Struct,
    TraitRustOnly,
    TraitBoth,
    TraitForeignOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WasmCallTarget {
    FreeFunction {
        module: RustPath,
        item: String,
    },
    Constructor {
        object: RustPath,
        object_kind: WasmObjectKind,
        item: String,
    },
    Method {
        object: RustPath,
        object_kind: WasmObjectKind,
        callback_method_id: Option<u32>,
        item: String,
    },
    CallbackMethod {
        callback: RustPath,
        callback_type_id: u32,
        method_id: u32,
        item: String,
    },
    StreamHook {
        parent_operation_id: u32,
        use_site_id: u32,
        hook: WasmResourceHook,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WasmResourceHook {
    None,
    AcquireObject,
    ReleaseObject,
    StartInputStream,
    PullInputStream,
    CancelInputStream,
    CloseInputStream,
    StartOutputStream,
    PullOutputStream,
    CancelOutputStream,
    CloseOutputStream,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WasmValuePathSegment {
    Argument(u32),
    Receiver,
    Return,
    /// Select the `value` payload of a canonical output stream `item` step.
    ///
    /// This segment is only valid immediately below a `Return` root on an
    /// output-stream-next operation.  Keeping it in the canonical path
    /// model means object resources in stream items use the same metadata and
    /// lease walker as ordinary operation returns.
    StreamItem,
    /// Select the `error` payload of a canonical output stream `error` step.
    ///
    /// This segment is only valid immediately below a `Return` root on an
    /// output-stream-next operation.
    StreamError,
    Field(String),
    Variant(String),
    Optional,
    SequenceItem,
    SetItem,
    MapKey,
    MapValue,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WasmValuePath(Vec<WasmValuePathSegment>);

impl WasmValuePath {
    pub fn new(segments: impl Into<Vec<WasmValuePathSegment>>) -> Self {
        Self(segments.into())
    }

    pub fn argument(index: u32) -> Self {
        Self::new(vec![WasmValuePathSegment::Argument(index)])
    }

    pub fn return_value() -> Self {
        Self::new(vec![WasmValuePathSegment::Return])
    }

    pub fn receiver() -> Self {
        Self::new(vec![WasmValuePathSegment::Receiver])
    }

    pub fn segments(&self) -> &[WasmValuePathSegment] {
        &self.0
    }
}

/// The resource category carried by an engine-owned object use-site.
/// Streams retain their dedicated stream-group contract; this DTO is for
/// object leases embedded anywhere in an operation value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmResourceKind {
    Object,
}

/// Ownership expected at an object resource use-site.  Argument/receiver
/// positions borrow an existing facade lease; return positions own a newly
/// acquired native handle and therefore publish a lease on success.
pub type WasmResourceOwnership = WasmOwnership;

/// Complete object-resource path metadata owned by the wasm engine boundary.
/// It intentionally contains no UniFFI or serialized-schema types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmResourceUseSite {
    pub path: WasmValuePath,
    pub kind: WasmResourceKind,
    pub type_id: u32,
    pub ownership: WasmResourceOwnership,
}

impl WasmResourceUseSite {
    pub fn object(path: WasmValuePath, type_id: u32, ownership: WasmResourceOwnership) -> Self {
        Self {
            path,
            kind: WasmResourceKind::Object,
            type_id,
            ownership,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmCallbackRetention {
    Scoped,
    Retained,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmCallbackThreading {
    CallingThread,
    MayCrossThread,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmCallbackReentrancy {
    Forbidden,
    Allowed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasmCallbackContract {
    pub retention: WasmCallbackRetention,
    pub threading: WasmCallbackThreading,
    pub reentrancy: WasmCallbackReentrancy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmCallbackUseSite {
    pub operation_id: u32,
    pub callback_type_id: u32,
    pub path: WasmValuePath,
    pub contract: WasmCallbackContract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmStreamDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasmStreamContract {
    pub direction: WasmStreamDirection,
    pub lazy_start: bool,
    pub single_consumer: bool,
    pub serial_pull: bool,
    pub exactly_once_cleanup: bool,
    pub explicit_cancel: bool,
    pub eof_is_distinct_from_item: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmStreamUseSite {
    pub id: u32,
    pub operation_id: u32,
    pub path: WasmValuePath,
    pub contract: WasmStreamContract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmStreamResourceGroup {
    pub use_site: WasmStreamUseSite,
    pub item: WasmValueBinding,
    pub error: WasmValueBinding,
    pub is_send: bool,
    pub hooks: Vec<WasmResourceHook>,
    pub slot_operation_ids: BTreeMap<WasmOperationKind, u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmTypeSourceKey {
    pub component: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WasmOperationOwner {
    Namespace,
    Object(WasmTypeSourceKey),
    Value(WasmTypeSourceKey),
    Callback(WasmTypeSourceKey),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmOperationSourceKey {
    pub component: String,
    pub owner: WasmOperationOwner,
    pub kind: WasmOperationKind,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmEngineResourceHook {
    pub rust_call: RustPath,
    pub async_kind: WasmAsyncKind,
    pub fallible: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WasmEngineResourceHooks {
    pub release_object: Option<WasmEngineResourceHook>,
    pub close_output_stream: Option<WasmEngineResourceHook>,
}

/// One already-lowered local adapter.
///
/// `operation_id` is intentionally a plain dense `u32`.  The UniFFI frontend
/// assigns it before entering this crate; this crate validates density and
/// uniqueness without knowing how that assignment was produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmOperationPlan {
    pub operation_id: u32,
    pub source_key: WasmOperationSourceKey,
    pub component_id: u32,
    pub owner: WasmOperationOwner,
    pub kind: WasmOperationKind,
    pub callback_method_id: Option<u32>,
    pub call_target: WasmCallTarget,
    /// The generated local adapter selected by the UniFFI frontend.  The
    /// call target above remains the lossless core identity; this path is the
    /// wasm-carrier bridge that can legally implement wasm-bindgen ABI traits.
    pub rust_call: RustPath,
    pub receiver: Option<WasmReceiverBinding>,
    pub arguments: Vec<WasmArgumentBinding>,
    pub return_value: Option<WasmReturnBinding>,
    pub async_kind: WasmAsyncKind,
    pub throws: Option<u32>,
    pub callback_use_sites: Vec<WasmCallbackUseSite>,
    /// Every object lease position, including direct receiver/argument/
    /// return values and nested record/variant/container selectors.
    pub resource_use_sites: Vec<WasmResourceUseSite>,
    pub resource_hooks: Vec<WasmResourceHook>,
    pub stream_resources: Vec<WasmStreamResourceGroup>,
}

impl WasmOperationPlan {
    /// Return the dispatch class without requiring callers to duplicate the
    /// callback/input-stream target matrix.
    pub fn dispatch(&self) -> WasmOperationDispatch {
        match (&self.call_target, self.kind) {
            (WasmCallTarget::CallbackMethod { .. }, WasmOperationKind::CallbackMethod)
            | (
                WasmCallTarget::StreamHook {
                    hook: WasmResourceHook::PullInputStream | WasmResourceHook::CancelInputStream,
                    ..
                },
                WasmOperationKind::InputStreamPull | WasmOperationKind::InputStreamCancel,
            ) => WasmOperationDispatch::HostDispatched,
            _ => WasmOperationDispatch::NativeCall,
        }
    }
}

impl WasmOperationPlan {
    pub fn fallible(&self) -> bool {
        self.throws.is_some()
    }
}

#[derive(Clone, Debug)]
struct ValidatedOperation {
    plan: WasmOperationPlan,
    raw_export_name: String,
    stream_slot: Option<UniFfiBackendStreamSlot>,
}

/// Fully validated programmatic input for one wasm engine surface.
#[derive(Clone, Debug)]
pub struct WasmEnginePlan {
    factory_export_name: String,
    operations: Vec<ValidatedOperation>,
    resource_hooks: WasmEngineResourceHooks,
    close_policy: ClosePolicy,
}

impl WasmEnginePlan {
    pub fn build(
        close_policy: ClosePolicy,
        operations: Vec<WasmOperationPlan>,
    ) -> Result<Self, EngineError> {
        Self::with_factory_and_resource_hooks(
            DEFAULT_BACKEND_FACTORY,
            close_policy,
            operations,
            WasmEngineResourceHooks::default(),
        )
    }

    pub fn build_with_resource_hooks(
        close_policy: ClosePolicy,
        operations: Vec<WasmOperationPlan>,
        resource_hooks: WasmEngineResourceHooks,
    ) -> Result<Self, EngineError> {
        Self::with_factory_and_resource_hooks(
            DEFAULT_BACKEND_FACTORY,
            close_policy,
            operations,
            resource_hooks,
        )
    }

    pub fn with_factory(
        factory_export_name: impl Into<String>,
        close_policy: ClosePolicy,
        operations: Vec<WasmOperationPlan>,
    ) -> Result<Self, EngineError> {
        Self::with_factory_and_resource_hooks(
            factory_export_name,
            close_policy,
            operations,
            WasmEngineResourceHooks::default(),
        )
    }

    pub fn with_factory_and_resource_hooks(
        factory_export_name: impl Into<String>,
        close_policy: ClosePolicy,
        operations: Vec<WasmOperationPlan>,
        resource_hooks: WasmEngineResourceHooks,
    ) -> Result<Self, EngineError> {
        close_policy
            .validate()
            .map_err(|error| EngineError::InvalidPlan(error.to_string()))?;
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
                stream_slot: None,
            });
        }

        validate_executable_plan(&mut validated)?;

        // Reuse cli-support's identifier/dense-table validation for the one
        // backend factory, without exposing its config or Bindgen type.
        backend_config(
            &factory_export_name,
            close_policy,
            &validated,
            &resource_hooks,
        )?;
        Ok(Self {
            factory_export_name,
            operations: validated,
            resource_hooks,
            close_policy,
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
            backend: backend_config(
                &self.factory_export_name,
                self.close_policy,
                &self.operations,
                &self.resource_hooks,
            )
            .expect("validated engine plan has a valid backend surface"),
        }
    }

    pub fn expand(&self, context: ExpansionContext) -> Result<Vec<ExpandedOperation>, EngineError> {
        let builder = ExpansionBuilder::new(context);
        self.operations
            .iter()
            .filter(|operation| !operation.plan.dispatch().is_host_dispatched())
            .map(|operation| expand_operation(&builder, operation))
            .collect()
    }

    pub fn expand_resource_hooks(
        &self,
        context: ExpansionContext,
    ) -> Result<Vec<ExpandedResourceHook>, EngineError> {
        let builder = ExpansionBuilder::new(context);
        [
            (
                WasmResourceHook::ReleaseObject,
                RELEASE_OBJECT_EXPORT,
                self.resource_hooks.release_object.as_ref(),
            ),
            (
                WasmResourceHook::CloseOutputStream,
                CLOSE_OUTPUT_STREAM_EXPORT,
                self.resource_hooks.close_output_stream.as_ref(),
            ),
        ]
        .into_iter()
        .filter_map(|(hook, raw_export_name, plan)| {
            plan.map(|plan| expand_resource_hook(&builder, hook, raw_export_name, plan))
        })
        .collect()
    }

    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    pub fn close_policy(&self) -> ClosePolicy {
        self.close_policy
    }

    pub fn operation_plans(&self) -> impl Iterator<Item = &WasmOperationPlan> {
        self.operations.iter().map(|operation| &operation.plan)
    }
}

fn validate_executable_plan(operations: &mut [ValidatedOperation]) -> Result<(), EngineError> {
    let operation_count = operations.len();
    let mut stream_use_sites = BTreeMap::new();
    let mut slots = BTreeMap::new();
    for operation in operations.iter() {
        if operation.plan.source_key.owner != operation.plan.owner {
            return Err(EngineError::InvalidPlan(format!(
                "operation {} has an inconsistent source owner",
                operation.plan.operation_id
            )));
        }
        if operation.plan.source_key.kind != operation.plan.kind {
            return Err(EngineError::InvalidPlan(format!(
                "operation {} has an inconsistent source kind",
                operation.plan.operation_id
            )));
        }
        for callback in &operation.plan.callback_use_sites {
            if callback.operation_id != operation.plan.operation_id {
                return Err(EngineError::InvalidPlan(format!(
                    "callback use-site operation {} does not match operation {}",
                    callback.operation_id, operation.plan.operation_id
                )));
            }
            validate_value_path(&operation.plan, &callback.path, "callback")?;
        }
        let mut resource_paths = BTreeSet::new();
        for use_site in &operation.plan.resource_use_sites {
            if !resource_paths.insert(use_site.path.clone()) {
                return Err(EngineError::InvalidPlan(format!(
                    "duplicate object resource use-site path in operation {}",
                    operation.plan.operation_id
                )));
            }
            validate_value_path(&operation.plan, &use_site.path, "resource")?;
            if use_site.kind != WasmResourceKind::Object {
                return Err(EngineError::InvalidPlan(format!(
                    "resource use-site in operation {} is not an object",
                    operation.plan.operation_id
                )));
            }
            if use_site.path.segments().iter().skip(1).any(|segment| {
                matches!(segment, WasmValuePathSegment::Field(name) | WasmValuePathSegment::Variant(name) if name.is_empty())
            }) {
                return Err(EngineError::InvalidPlan(format!(
                    "resource use-site in operation {} has an empty nested selector",
                    operation.plan.operation_id
                )));
            }
            let root = use_site.path.segments().first().expect("validated path");
            let is_return = matches!(root, WasmValuePathSegment::Return);
            for (index, segment) in use_site.path.segments().iter().skip(1).enumerate() {
                if matches!(
                    segment,
                    WasmValuePathSegment::StreamItem | WasmValuePathSegment::StreamError
                ) {
                    if !is_return
                        || index != 0
                        || operation.plan.kind != WasmOperationKind::OutputStreamNext
                    {
                        return Err(EngineError::InvalidPlan(format!(
                            "stream step resource path in operation {} must belong to OutputStreamNext and start at Return",
                            operation.plan.operation_id
                        )));
                    }
                }
            }
            let (binding_ownership, conversion, expected_ownership) = match root {
                WasmValuePathSegment::Receiver => {
                    let Some(receiver) = operation.plan.receiver.as_ref() else {
                        return Err(EngineError::InvalidPlan(format!(
                            "resource receiver path in operation {} has no receiver",
                            operation.plan.operation_id
                        )));
                    };
                    (
                        receiver.ownership,
                        &receiver.conversion,
                        WasmOwnership::Borrowed,
                    )
                }
                WasmValuePathSegment::Argument(index) => {
                    let argument = &operation.plan.arguments[*index as usize];
                    (
                        argument.ownership,
                        &argument.conversion,
                        WasmOwnership::Borrowed,
                    )
                }
                WasmValuePathSegment::Return => {
                    let Some(return_value) = operation.plan.return_value.as_ref() else {
                        return Err(EngineError::InvalidPlan(format!(
                            "resource return path in operation {} has no return value",
                            operation.plan.operation_id
                        )));
                    };
                    (
                        return_value.ownership,
                        &return_value.conversion,
                        WasmOwnership::Owned,
                    )
                }
                _ => unreachable!("validate_value_path checked resource root"),
            };
            if binding_ownership != expected_ownership || use_site.ownership != expected_ownership {
                return Err(EngineError::InvalidPlan(format!(
                    "resource use-site in operation {} has inconsistent {:?} ownership",
                    operation.plan.operation_id, root
                )));
            }
            let object_ids = conversion_object_ids(conversion);
            let direct_object = use_site.path.segments().len() == 1;
            if (direct_object && (object_ids.is_empty() || !object_ids.contains(&use_site.type_id)))
                || (!object_ids.is_empty() && !object_ids.contains(&use_site.type_id))
            {
                return Err(EngineError::InvalidPlan(format!(
                    "resource use-site in operation {} has object type ID {} not present in its binding",
                    operation.plan.operation_id, use_site.type_id
                )));
            }
        }
        let has_direct_resource = |root: WasmValuePathSegment, type_id: u32| {
            operation.plan.resource_use_sites.iter().any(|use_site| {
                use_site.path.segments() == [root.clone()]
                    && use_site.kind == WasmResourceKind::Object
                    && use_site.type_id == type_id
            })
        };
        if let Some(receiver) = operation.plan.receiver.as_ref() {
            if let WasmConversionRecipe::Object(type_id) = &receiver.conversion {
                if !has_direct_resource(WasmValuePathSegment::Receiver, *type_id) {
                    return Err(EngineError::InvalidPlan(format!(
                        "object receiver in operation {} is missing its resource use-site",
                        operation.plan.operation_id
                    )));
                }
            }
        }
        for (index, argument) in operation.plan.arguments.iter().enumerate() {
            if let WasmConversionRecipe::Object(type_id) = &argument.conversion {
                let index = u32::try_from(index).map_err(|_| EngineError::TooManyOperations)?;
                if !has_direct_resource(WasmValuePathSegment::Argument(index), *type_id) {
                    return Err(EngineError::InvalidPlan(format!(
                        "object argument {} in operation {} is missing its resource use-site",
                        index, operation.plan.operation_id
                    )));
                }
            }
        }
        if let Some(return_value) = operation.plan.return_value.as_ref() {
            if let WasmConversionRecipe::Object(type_id) = &return_value.conversion {
                if !has_direct_resource(WasmValuePathSegment::Return, *type_id) {
                    return Err(EngineError::InvalidPlan(format!(
                        "object return in operation {} is missing its resource use-site",
                        operation.plan.operation_id
                    )));
                }
            }
        }
        for group in &operation.plan.stream_resources {
            let use_site = &group.use_site;
            if use_site.operation_id != operation.plan.operation_id {
                return Err(EngineError::InvalidPlan(format!(
                    "stream use-site operation {} does not match operation {}",
                    use_site.operation_id, operation.plan.operation_id
                )));
            }
            validate_value_path(&operation.plan, &use_site.path, "stream")?;
            if stream_use_sites
                .insert(use_site.id, operation.plan.operation_id)
                .is_some()
            {
                return Err(EngineError::InvalidPlan(format!(
                    "duplicate stream use-site {}",
                    use_site.id
                )));
            }
            let contract = use_site.contract;
            let has_output_start = group
                .slot_operation_ids
                .contains_key(&WasmOperationKind::OutputStreamStart);
            // A direct output stream has a real start operation.  A stream
            // nested in a record/container is created as part of its parent
            // operation's return value, so only its pull/cancel operations
            // are represented in the engine slot table.  The canonical
            // contract direction is authoritative in both cases; deriving
            // it from the presence of a start slot incorrectly classified a
            // nested output as an input stream.
            let nested_output = contract.direction == WasmStreamDirection::Output
                && !has_output_start
                && use_site.path.segments().len() > 1;
            if (contract.direction == WasmStreamDirection::Input && has_output_start)
                || (contract.direction == WasmStreamDirection::Output
                    && !has_output_start
                    && !nested_output)
                || !contract.lazy_start
                || !contract.single_consumer
                || !contract.serial_pull
                || !contract.exactly_once_cleanup
                || !contract.explicit_cancel
                || !contract.eof_is_distinct_from_item
            {
                return Err(EngineError::InvalidPlan(format!(
                    "stream use-site {} has a non-canonical contract",
                    use_site.id
                )));
            }
            let expected: &[WasmOperationKind] = match contract.direction {
                WasmStreamDirection::Input => &[
                    WasmOperationKind::InputStreamPull,
                    WasmOperationKind::InputStreamCancel,
                ],
                WasmStreamDirection::Output if nested_output => &[
                    WasmOperationKind::OutputStreamNext,
                    WasmOperationKind::OutputStreamCancel,
                ],
                WasmStreamDirection::Output => &[
                    WasmOperationKind::OutputStreamStart,
                    WasmOperationKind::OutputStreamNext,
                    WasmOperationKind::OutputStreamCancel,
                ],
            };
            if group.slot_operation_ids.len() != expected.len()
                || expected
                    .iter()
                    .any(|kind| !group.slot_operation_ids.contains_key(kind))
            {
                return Err(EngineError::InvalidPlan(format!(
                    "stream use-site {} does not contain the complete canonical slot set",
                    use_site.id
                )));
            }
            for (kind, operation_id) in &group.slot_operation_ids {
                if (*operation_id as usize) >= operation_count {
                    return Err(EngineError::InvalidPlan(format!(
                        "stream use-site {} references unknown operation {}",
                        use_site.id, operation_id
                    )));
                }
                if slots
                    .insert(
                        *operation_id,
                        UniFfiBackendStreamSlot {
                            use_site_id: use_site.id,
                            operation_id: *operation_id,
                            kind: backend_operation_kind(*kind),
                        },
                    )
                    .is_some()
                {
                    return Err(EngineError::InvalidPlan(format!(
                        "operation {} belongs to more than one stream slot",
                        operation_id
                    )));
                }
            }
        }
    }
    for (operation_id, slot) in slots {
        let operation = &mut operations[operation_id as usize];
        let slot_kind = operation_kind_from_backend(slot.kind);
        if slot_kind != WasmOperationKind::OutputStreamStart && operation.plan.kind != slot_kind {
            return Err(EngineError::InvalidPlan(format!(
                "stream slot operation {} has kind {:?}, expected {:?}",
                operation_id, operation.plan.kind, slot_kind
            )));
        }
        operation.stream_slot = Some(slot);
    }
    for operation in operations.iter() {
        if let Some(slot) = &operation.stream_slot {
            let slot_kind = operation_kind_from_backend(slot.kind);
            if slot_kind == WasmOperationKind::OutputStreamStart {
                if operation
                    .plan
                    .return_value
                    .as_ref()
                    .map(|binding| binding.carrier)
                    != Some(WasmRustCarrier::OutputStream)
                {
                    return Err(EngineError::InvalidPlan(format!(
                        "output stream start operation {} has no output-stream result carrier",
                        operation.plan.operation_id
                    )));
                }
            } else {
                let expected_hook = match slot_kind {
                    WasmOperationKind::InputStreamPull => WasmResourceHook::PullInputStream,
                    WasmOperationKind::InputStreamCancel => WasmResourceHook::CancelInputStream,
                    WasmOperationKind::OutputStreamNext => WasmResourceHook::PullOutputStream,
                    WasmOperationKind::OutputStreamCancel => WasmResourceHook::CancelOutputStream,
                    _ => unreachable!("non-start stream slot kind"),
                };
                let expected_parent = stream_use_sites[&slot.use_site_id];
                if !matches!(
                    operation.plan.call_target,
                    WasmCallTarget::StreamHook {
                        parent_operation_id,
                        use_site_id,
                        hook,
                    } if parent_operation_id == expected_parent
                        && use_site_id == slot.use_site_id
                        && hook == expected_hook
                ) {
                    return Err(EngineError::InvalidPlan(format!(
                        "stream slot operation {} has an inconsistent call target",
                        operation.plan.operation_id
                    )));
                }
                if operation.plan.async_kind != WasmAsyncKind::Async
                    || !operation.plan.arguments.is_empty()
                {
                    return Err(EngineError::InvalidPlan(format!(
                        "stream slot operation {} must be async with no payload arguments",
                        operation.plan.operation_id
                    )));
                }
                let expected_receiver =
                    match slot_kind {
                        WasmOperationKind::InputStreamPull
                        | WasmOperationKind::InputStreamCancel => WasmRustCarrier::InputStream,
                        WasmOperationKind::OutputStreamNext
                        | WasmOperationKind::OutputStreamCancel => WasmRustCarrier::OutputStream,
                        _ => unreachable!("non-start stream slot kind"),
                    };
                if operation
                    .plan
                    .receiver
                    .as_ref()
                    .map(|binding| binding.carrier)
                    != Some(expected_receiver)
                {
                    return Err(EngineError::InvalidPlan(format!(
                        "stream slot operation {} has an invalid receiver carrier",
                        operation.plan.operation_id
                    )));
                }
                let cancel = matches!(
                    slot_kind,
                    WasmOperationKind::InputStreamCancel | WasmOperationKind::OutputStreamCancel
                );
                if cancel != operation.plan.return_value.is_none() {
                    return Err(EngineError::InvalidPlan(format!(
                        "stream slot operation {} has an invalid return carrier",
                        operation.plan.operation_id
                    )));
                }
                if !cancel
                    && operation
                        .plan
                        .return_value
                        .as_ref()
                        .map(|binding| binding.carrier)
                        != Some(WasmRustCarrier::StreamStep)
                {
                    return Err(EngineError::InvalidPlan(format!(
                        "stream pull operation {} must return a tagged StreamStep",
                        operation.plan.operation_id
                    )));
                }
            }
        }
        match (&operation.plan.call_target, operation.plan.kind) {
            (
                WasmCallTarget::CallbackMethod {
                    method_id,
                    callback_type_id: _,
                    ..
                },
                WasmOperationKind::CallbackMethod,
            ) if operation.plan.callback_method_id == Some(*method_id) => {}
            (WasmCallTarget::CallbackMethod { .. }, _) | (_, WasmOperationKind::CallbackMethod) => {
                return Err(EngineError::InvalidPlan(format!(
                    "callback operation {} has inconsistent method dispatch",
                    operation.plan.operation_id
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_value_path(
    operation: &WasmOperationPlan,
    path: &WasmValuePath,
    role: &str,
) -> Result<(), EngineError> {
    let Some(root) = path.segments().first() else {
        return Err(EngineError::InvalidPlan(format!(
            "{role} use-site for operation {} has an empty path",
            operation.operation_id
        )));
    };
    match root {
        WasmValuePathSegment::Argument(index) if (*index as usize) < operation.arguments.len() => {}
        WasmValuePathSegment::Receiver if operation.receiver.is_some() => {}
        WasmValuePathSegment::Return => {}
        WasmValuePathSegment::Argument(index) => {
            return Err(EngineError::InvalidPlan(format!(
                "{role} use-site argument {} is out of range for operation {}",
                index, operation.operation_id
            )))
        }
        _ => {
            return Err(EngineError::InvalidPlan(format!(
                "{role} use-site for operation {} has an invalid path root",
                operation.operation_id
            )))
        }
    }
    if path.segments().iter().skip(1).any(|segment| {
        matches!(
            segment,
            WasmValuePathSegment::Argument(_)
                | WasmValuePathSegment::Return
                | WasmValuePathSegment::Receiver
        )
    }) {
        return Err(EngineError::InvalidPlan(format!(
            "{role} use-site for operation {} has a nested path root",
            operation.operation_id
        )));
    }
    Ok(())
}

fn conversion_object_ids(conversion: &WasmConversionRecipe) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    fn visit(conversion: &WasmConversionRecipe, ids: &mut BTreeSet<u32>) {
        match conversion {
            WasmConversionRecipe::Object(type_id) => {
                ids.insert(*type_id);
            }
            WasmConversionRecipe::Optional(inner)
            | WasmConversionRecipe::Sequence(inner)
            | WasmConversionRecipe::Set(inner)
            | WasmConversionRecipe::Custom(_, inner) => visit(inner, ids),
            WasmConversionRecipe::InputStream { item, error }
            | WasmConversionRecipe::OutputStream { item, error } => {
                visit(item, ids);
                visit(error, ids);
            }
            WasmConversionRecipe::Map(key, value)
            | WasmConversionRecipe::StreamStep {
                item: key,
                error: value,
            } => {
                visit(key, ids);
                visit(value, ids);
            }
            WasmConversionRecipe::Identity
            | WasmConversionRecipe::Timestamp
            | WasmConversionRecipe::Duration
            | WasmConversionRecipe::BigInt
            | WasmConversionRecipe::Bytes
            | WasmConversionRecipe::Record(_)
            | WasmConversionRecipe::Enum(_)
            | WasmConversionRecipe::Error(_)
            | WasmConversionRecipe::Callback(_) => {}
        }
    }
    visit(conversion, &mut ids);
    ids
}

fn backend_config(
    factory_export_name: &str,
    close_policy: ClosePolicy,
    operations: &[ValidatedOperation],
    resource_hooks: &WasmEngineResourceHooks,
) -> Result<UniFfiBackendConfig, EngineError> {
    let operations = operations
        .iter()
        .map(|operation| {
            let callback_dispatch = match &operation.plan.call_target {
                WasmCallTarget::CallbackMethod {
                    callback_type_id,
                    method_id,
                    ..
                } => Some((*callback_type_id, *method_id)),
                _ => None,
            };
            let callback_use_sites = operation
                .plan
                .callback_use_sites
                .iter()
                .map(backend_callback_use_site)
                .collect();
            let stream_groups = operation
                .plan
                .stream_resources
                .iter()
                .map(backend_stream_group)
                .collect();
            let resource_use_sites = operation
                .plan
                .resource_use_sites
                .iter()
                .map(backend_resource_use_site)
                .collect();
            UniFfiBackendOperation::new(
                operation.plan.operation_id,
                operation.raw_export_name.clone(),
                match operation.plan.async_kind {
                    WasmAsyncKind::Sync => UniFfiBackendAsyncKind::Sync,
                    WasmAsyncKind::Async => UniFfiBackendAsyncKind::Async,
                },
                backend_operation_kind(operation.plan.kind),
                operation.plan.fallible(),
                operation
                    .plan
                    .arguments
                    .iter()
                    .map(|argument| backend_carrier(argument.carrier))
                    .collect(),
                operation
                    .plan
                    .return_value
                    .as_ref()
                    .map(|binding| backend_carrier(binding.carrier)),
            )
            .map(|backend| {
                let backend = backend.with_dispatch(match operation.plan.dispatch() {
                    WasmOperationDispatch::NativeCall => {
                        wasm_bindgen_cli_support::UniFfiBackendDispatch::NativeCall
                    }
                    WasmOperationDispatch::HostDispatched => {
                        wasm_bindgen_cli_support::UniFfiBackendDispatch::HostDispatched
                    }
                });
                let backend = if let Some((callback_type_id, method_id)) = callback_dispatch {
                    backend.with_callback_dispatch(callback_type_id, method_id)
                } else {
                    backend
                };
                backend
                    .with_host_argument(operation_requires_host(&operation.plan))
                    .with_receiver(operation.plan.receiver.is_some())
                    .with_callback_use_sites(callback_use_sites)
                    .with_stream_groups(stream_groups)
                    .with_stream_slot(operation.stream_slot.clone())
                    .with_resource_use_sites(resource_use_sites)
                    .with_resource_hooks(
                        operation
                            .plan
                            .resource_hooks
                            .iter()
                            .copied()
                            .map(backend_resource_hook)
                            .collect(),
                    )
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| EngineError::Surface(error.to_string()))?;
    let resource_exports = UniFfiBackendResourceExports::new(
        resource_hooks
            .release_object
            .as_ref()
            .map(|_| RELEASE_OBJECT_EXPORT.to_owned()),
        resource_hooks
            .close_output_stream
            .as_ref()
            .map(|_| CLOSE_OUTPUT_STREAM_EXPORT.to_owned()),
    )
    .map_err(|error| EngineError::Surface(error.to_string()))?;
    UniFfiBackendConfig::new(
        factory_export_name,
        operations,
        resource_exports,
        close_policy,
    )
    .map_err(|error| EngineError::Surface(error.to_string()))
}

fn operation_requires_host(operation: &WasmOperationPlan) -> bool {
    !matches!(operation.call_target, WasmCallTarget::CallbackMethod { .. })
        && (!operation.callback_use_sites.is_empty()
            || operation
                .stream_resources
                .iter()
                .any(|group| group.use_site.contract.direction == WasmStreamDirection::Input))
}

fn backend_operation_kind(kind: WasmOperationKind) -> UniFfiBackendOperationKind {
    match kind {
        WasmOperationKind::Function => UniFfiBackendOperationKind::Function,
        WasmOperationKind::Constructor => UniFfiBackendOperationKind::Constructor,
        WasmOperationKind::Method => UniFfiBackendOperationKind::Method,
        WasmOperationKind::CallbackMethod => UniFfiBackendOperationKind::CallbackMethod,
        WasmOperationKind::OutputStreamStart => UniFfiBackendOperationKind::OutputStreamStart,
        WasmOperationKind::OutputStreamNext => UniFfiBackendOperationKind::OutputStreamNext,
        WasmOperationKind::OutputStreamCancel => UniFfiBackendOperationKind::OutputStreamCancel,
        WasmOperationKind::InputStreamPull => UniFfiBackendOperationKind::InputStreamPull,
        WasmOperationKind::InputStreamCancel => UniFfiBackendOperationKind::InputStreamCancel,
    }
}

fn operation_kind_from_backend(kind: UniFfiBackendOperationKind) -> WasmOperationKind {
    match kind {
        UniFfiBackendOperationKind::Function => WasmOperationKind::Function,
        UniFfiBackendOperationKind::Constructor => WasmOperationKind::Constructor,
        UniFfiBackendOperationKind::Method => WasmOperationKind::Method,
        UniFfiBackendOperationKind::CallbackMethod => WasmOperationKind::CallbackMethod,
        UniFfiBackendOperationKind::OutputStreamStart => WasmOperationKind::OutputStreamStart,
        UniFfiBackendOperationKind::OutputStreamNext => WasmOperationKind::OutputStreamNext,
        UniFfiBackendOperationKind::OutputStreamCancel => WasmOperationKind::OutputStreamCancel,
        UniFfiBackendOperationKind::InputStreamPull => WasmOperationKind::InputStreamPull,
        UniFfiBackendOperationKind::InputStreamCancel => WasmOperationKind::InputStreamCancel,
    }
}

fn backend_carrier(carrier: WasmRustCarrier) -> UniFfiBackendCarrier {
    match carrier {
        WasmRustCarrier::Primitive => UniFfiBackendCarrier::Primitive,
        WasmRustCarrier::BigInt => UniFfiBackendCarrier::BigInt,
        WasmRustCarrier::Bytes => UniFfiBackendCarrier::Bytes,
        WasmRustCarrier::Timestamp => UniFfiBackendCarrier::Timestamp,
        WasmRustCarrier::Duration => UniFfiBackendCarrier::Duration,
        WasmRustCarrier::LocalAdapter => UniFfiBackendCarrier::LocalAdapter,
        WasmRustCarrier::OpaqueHandle => UniFfiBackendCarrier::OpaqueHandle,
        WasmRustCarrier::CallbackProxy => UniFfiBackendCarrier::CallbackProxy,
        WasmRustCarrier::InputStream => UniFfiBackendCarrier::InputStream,
        WasmRustCarrier::OutputStream => UniFfiBackendCarrier::OutputStream,
        WasmRustCarrier::StreamStep => UniFfiBackendCarrier::StreamStep,
    }
}

fn backend_resource_hook(hook: WasmResourceHook) -> UniFfiBackendResourceHook {
    match hook {
        WasmResourceHook::None => UniFfiBackendResourceHook::None,
        WasmResourceHook::AcquireObject => UniFfiBackendResourceHook::AcquireObject,
        WasmResourceHook::ReleaseObject => UniFfiBackendResourceHook::ReleaseObject,
        WasmResourceHook::StartInputStream => UniFfiBackendResourceHook::StartInputStream,
        WasmResourceHook::PullInputStream => UniFfiBackendResourceHook::PullInputStream,
        WasmResourceHook::CancelInputStream => UniFfiBackendResourceHook::CancelInputStream,
        WasmResourceHook::CloseInputStream => UniFfiBackendResourceHook::CloseInputStream,
        WasmResourceHook::StartOutputStream => UniFfiBackendResourceHook::StartOutputStream,
        WasmResourceHook::PullOutputStream => UniFfiBackendResourceHook::PullOutputStream,
        WasmResourceHook::CancelOutputStream => UniFfiBackendResourceHook::CancelOutputStream,
        WasmResourceHook::CloseOutputStream => UniFfiBackendResourceHook::CloseOutputStream,
    }
}

fn backend_path(path: &WasmValuePath) -> UniFfiBackendValuePath {
    UniFfiBackendValuePath::new(
        path.segments()
            .iter()
            .map(|segment| match segment {
                WasmValuePathSegment::Argument(index) => {
                    UniFfiBackendValuePathSegment::Argument(*index)
                }
                WasmValuePathSegment::Receiver => UniFfiBackendValuePathSegment::Receiver,
                WasmValuePathSegment::Return => UniFfiBackendValuePathSegment::Return,
                WasmValuePathSegment::StreamItem => UniFfiBackendValuePathSegment::StreamItem,
                WasmValuePathSegment::StreamError => UniFfiBackendValuePathSegment::StreamError,
                WasmValuePathSegment::Field(name) => {
                    UniFfiBackendValuePathSegment::Field(name.clone())
                }
                WasmValuePathSegment::Variant(name) => {
                    UniFfiBackendValuePathSegment::Variant(name.clone())
                }
                WasmValuePathSegment::Optional => UniFfiBackendValuePathSegment::Optional,
                WasmValuePathSegment::SequenceItem => UniFfiBackendValuePathSegment::SequenceItem,
                WasmValuePathSegment::SetItem => UniFfiBackendValuePathSegment::SetItem,
                WasmValuePathSegment::MapKey => UniFfiBackendValuePathSegment::MapKey,
                WasmValuePathSegment::MapValue => UniFfiBackendValuePathSegment::MapValue,
            })
            .collect::<Vec<_>>(),
    )
}

fn backend_callback_use_site(use_site: &WasmCallbackUseSite) -> UniFfiBackendCallbackUseSite {
    UniFfiBackendCallbackUseSite {
        operation_id: use_site.operation_id,
        callback_type_id: use_site.callback_type_id,
        path: backend_path(&use_site.path),
        contract: UniFfiBackendCallbackContract {
            retention: match use_site.contract.retention {
                WasmCallbackRetention::Scoped => UniFfiBackendCallbackRetention::Scoped,
                WasmCallbackRetention::Retained => UniFfiBackendCallbackRetention::Retained,
            },
            threading: match use_site.contract.threading {
                WasmCallbackThreading::CallingThread => {
                    UniFfiBackendCallbackThreading::CallingThread
                }
                WasmCallbackThreading::MayCrossThread => {
                    UniFfiBackendCallbackThreading::MayCrossThread
                }
            },
            reentrancy: match use_site.contract.reentrancy {
                WasmCallbackReentrancy::Forbidden => UniFfiBackendCallbackReentrancy::Forbidden,
                WasmCallbackReentrancy::Allowed => UniFfiBackendCallbackReentrancy::Allowed,
            },
        },
    }
}

fn backend_stream_group(group: &WasmStreamResourceGroup) -> UniFfiBackendStreamGroup {
    UniFfiBackendStreamGroup {
        operation_id: group.use_site.operation_id,
        use_site_id: group.use_site.id,
        path: backend_path(&group.use_site.path),
        direction: match group.use_site.contract.direction {
            WasmStreamDirection::Input => UniFfiBackendStreamDirection::Input,
            WasmStreamDirection::Output => UniFfiBackendStreamDirection::Output,
        },
        item_carrier: backend_carrier(group.item.carrier),
        error_carrier: backend_carrier(group.error.carrier),
        is_send: group.is_send,
        slots: group
            .slot_operation_ids
            .iter()
            .map(|(kind, operation_id)| UniFfiBackendStreamSlot {
                use_site_id: group.use_site.id,
                operation_id: *operation_id,
                kind: backend_operation_kind(*kind),
            })
            .collect(),
        resource_hooks: group
            .hooks
            .iter()
            .copied()
            .map(backend_resource_hook)
            .collect(),
    }
}

fn backend_resource_use_site(use_site: &WasmResourceUseSite) -> UniFfiBackendResourceUseSite {
    UniFfiBackendResourceUseSite {
        path: backend_path(&use_site.path),
        kind: match use_site.kind {
            WasmResourceKind::Object => UniFfiBackendResource::Object(use_site.type_id),
        },
        ownership: match use_site.ownership {
            WasmOwnership::Borrowed => UniFfiBackendResourceOwnership::Borrowed,
            WasmOwnership::Owned => UniFfiBackendResourceOwnership::Owned,
        },
    }
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
    let host = operation_requires_host(&operation.plan).then(|| {
        let name = Ident::new("host", Span::call_site());
        (name, quote!(wasm_bindgen::JsValue))
    });
    let receiver = operation.plan.receiver.as_ref().map(|binding| {
        let name = Ident::new("receiver", Span::call_site());
        let ty = binding.abi_carrier.rust_type();
        (name, ty)
    });
    let arguments = host
        .into_iter()
        .chain(receiver)
        .into_iter()
        .chain(
            operation
                .plan
                .arguments
                .iter()
                .enumerate()
                .map(|(index, binding)| {
                    let name = Ident::new(&format!("arg{index}"), Span::call_site());
                    let ty = binding.abi_carrier.rust_type();
                    (name, ty)
                }),
        )
        .collect::<Vec<_>>();
    let argument_names = arguments.iter().map(|(name, _)| name);
    let argument_declarations = arguments.iter().map(|(name, ty)| quote!(#name: #ty));
    let return_type = operation
        .plan
        .return_value
        .as_ref()
        .map(|binding| binding.abi_carrier.rust_type())
        .unwrap_or_else(|| quote!(()));
    let return_type = if operation.plan.fallible() {
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

fn expand_resource_hook(
    builder: &ExpansionBuilder,
    hook: WasmResourceHook,
    raw_export_name: &str,
    plan: &WasmEngineResourceHook,
) -> Result<ExpandedResourceHook, EngineError> {
    let rust_name = Ident::new(
        match hook {
            WasmResourceHook::ReleaseObject => "__uniffi_wasm_release_object",
            WasmResourceHook::CloseOutputStream => "__uniffi_wasm_close_output_stream",
            _ => unreachable!("only standalone resource hooks are expanded"),
        },
        Span::call_site(),
    );
    let raw_export = Ident::new(raw_export_name, Span::call_site());
    let call = plan.rust_call.tokens();
    let return_type = if plan.fallible {
        quote!(Result<(), wasm_bindgen::JsValue>)
    } else {
        quote!(())
    };
    let await_call = (plan.async_kind == WasmAsyncKind::Async).then(|| quote!(.await));
    let async_token = (plan.async_kind == WasmAsyncKind::Async).then(|| quote!(async));
    let input = quote! {
        pub #async_token fn #rust_name(handle: u32) -> #return_type {
            #call(handle)#await_call
        }
    };
    let attr = quote!(js_name = #raw_export, skip_typescript);
    let tokens = builder
        .expand(attr, input)
        .map_err(|diagnostic| EngineError::Expansion(diagnostic.into_token_stream().to_string()))?;
    Ok(ExpandedResourceHook {
        hook,
        raw_export_name: raw_export_name.to_owned(),
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

#[derive(Clone, Debug)]
pub struct ExpandedResourceHook {
    pub hook: WasmResourceHook,
    pub raw_export_name: String,
    pub tokens: TokenStream,
}

#[derive(Debug, Eq, PartialEq)]
pub enum EngineError {
    InvalidRustPath(String),
    InvalidPlan(String),
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
            Self::InvalidPlan(message) => write!(formatter, "invalid wasm engine plan: {message}"),
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

    #[test]
    fn stream_binding_preserves_item_error_and_sendability() {
        let binding = WasmValueBinding {
            rust_type: WasmRustType::InputStream {
                item: Box::new(WasmRustType::Scalar(WasmScalarType::U32)),
                error: Box::new(WasmRustType::Scalar(WasmScalarType::String)),
                is_send: false,
            },
            carrier: WasmRustCarrier::InputStream,
            abi_carrier: WasmCarrier::OpaqueHandle,
            conversion: WasmConversionRecipe::InputStream {
                item: Box::new(WasmConversionRecipe::Identity),
                error: Box::new(WasmConversionRecipe::Identity),
            },
        };
        let WasmRustType::InputStream {
            item,
            error,
            is_send,
        } = &binding.rust_type
        else {
            panic!("expected input stream binding");
        };
        assert!(matches!(**item, WasmRustType::Scalar(WasmScalarType::U32)));
        assert!(matches!(
            **error,
            WasmRustType::Scalar(WasmScalarType::String)
        ));
        assert!(!*is_send);
        assert!(matches!(
            &binding.conversion,
            WasmConversionRecipe::InputStream { .. }
        ));
    }

    fn policy() -> ClosePolicy {
        ClosePolicy {
            grace_ms: 5_000,
            on_deadline: DeadlineAction::Detach,
        }
    }

    fn operation(id: u32) -> WasmOperationPlan {
        WasmOperationPlan {
            operation_id: id,
            source_key: WasmOperationSourceKey {
                component: "fixture".to_owned(),
                owner: WasmOperationOwner::Namespace,
                kind: WasmOperationKind::Function,
                name: "increment".to_owned(),
            },
            component_id: 0,
            owner: WasmOperationOwner::Namespace,
            kind: WasmOperationKind::Function,
            callback_method_id: None,
            call_target: WasmCallTarget::FreeFunction {
                module: RustPath::new(["fixture".to_owned()]).unwrap(),
                item: "increment".to_owned(),
            },
            rust_call: RustPath::new(["fixture".to_owned(), "increment".to_owned()]).unwrap(),
            receiver: None,
            arguments: vec![WasmArgumentBinding {
                public_name: "value".to_owned(),
                rust_name: "value".to_owned(),
                rust_type: WasmRustType::Scalar(WasmScalarType::I32),
                carrier: WasmRustCarrier::Primitive,
                abi_carrier: WasmCarrier::I32,
                ownership: WasmOwnership::Owned,
                conversion: WasmConversionRecipe::Identity,
            }],
            return_value: Some(WasmReturnBinding {
                rust_type: WasmRustType::Scalar(WasmScalarType::I32),
                carrier: WasmRustCarrier::Primitive,
                abi_carrier: WasmCarrier::I32,
                ownership: WasmOwnership::Owned,
                conversion: WasmConversionRecipe::Identity,
            }),
            async_kind: WasmAsyncKind::Sync,
            throws: None,
            callback_use_sites: Vec::new(),
            resource_use_sites: Vec::new(),
            resource_hooks: Vec::new(),
            stream_resources: Vec::new(),
        }
    }

    #[test]
    fn engine_plan_owns_dense_operation_validation() {
        let plan = WasmEnginePlan::build(policy(), vec![operation(1)]).unwrap_err();
        assert_eq!(
            plan,
            EngineError::NonDenseOperation {
                expected: 0,
                found: 1
            }
        );

        let duplicate =
            WasmEnginePlan::build(policy(), vec![operation(0), operation(0)]).unwrap_err();
        assert_eq!(duplicate, EngineError::DuplicateOperation(0));
    }

    #[test]
    fn structured_plan_expands_real_wasm_descriptors() {
        let plan = WasmEnginePlan::build(policy(), vec![operation(0)]).unwrap();
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
    fn host_dispatched_callback_has_no_rust_raw_expansion() {
        let mut callback = operation(0);
        callback.source_key.kind = WasmOperationKind::CallbackMethod;
        let callback_owner = WasmTypeSourceKey {
            component: "fixture".to_owned(),
            name: "Listener".to_owned(),
        };
        callback.source_key.owner = WasmOperationOwner::Callback(callback_owner.clone());
        callback.owner = WasmOperationOwner::Callback(callback_owner);
        callback.kind = WasmOperationKind::CallbackMethod;
        callback.callback_method_id = Some(0);
        callback.call_target = WasmCallTarget::CallbackMethod {
            callback: RustPath::new(["fixture".to_owned(), "Listener".to_owned()]).unwrap(),
            callback_type_id: 7,
            method_id: 0,
            item: "on_value".to_owned(),
        };
        callback.arguments.clear();
        callback.return_value = None;
        let plan = WasmEnginePlan::build(policy(), vec![callback]).unwrap();
        assert_eq!(
            plan.operations[0].plan.dispatch(),
            WasmOperationDispatch::HostDispatched
        );
        let context = ExpansionContext::new(
            std::env::current_dir().unwrap(),
            "fixture",
            "1.0.0",
            Vec::<String>::new(),
            "wasm32-unknown-unknown",
        )
        .unwrap();
        assert!(plan.expand(context).unwrap().is_empty());
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

    #[test]
    fn executable_plan_rejects_invalid_use_sites_and_incomplete_slots() {
        let mut invalid_callback = operation(0);
        invalid_callback
            .callback_use_sites
            .push(WasmCallbackUseSite {
                operation_id: 1,
                callback_type_id: 0,
                path: WasmValuePath::argument(0),
                contract: WasmCallbackContract {
                    retention: WasmCallbackRetention::Scoped,
                    threading: WasmCallbackThreading::CallingThread,
                    reentrancy: WasmCallbackReentrancy::Allowed,
                },
            });
        assert!(matches!(
            WasmEnginePlan::build(policy(), vec![invalid_callback]),
            Err(EngineError::InvalidPlan(message)) if message.contains("callback use-site")
        ));

        let mut incomplete_stream = operation(0);
        incomplete_stream
            .stream_resources
            .push(WasmStreamResourceGroup {
                use_site: WasmStreamUseSite {
                    id: 0,
                    operation_id: 0,
                    path: WasmValuePath::return_value(),
                    contract: WasmStreamContract {
                        direction: WasmStreamDirection::Output,
                        lazy_start: true,
                        single_consumer: true,
                        serial_pull: true,
                        exactly_once_cleanup: true,
                        explicit_cancel: true,
                        eof_is_distinct_from_item: true,
                    },
                },
                item: WasmValueBinding {
                    rust_type: WasmRustType::Scalar(WasmScalarType::I32),
                    carrier: WasmRustCarrier::Primitive,
                    abi_carrier: WasmCarrier::I32,
                    conversion: WasmConversionRecipe::Identity,
                },
                error: WasmValueBinding {
                    rust_type: WasmRustType::Scalar(WasmScalarType::I32),
                    carrier: WasmRustCarrier::Primitive,
                    abi_carrier: WasmCarrier::I32,
                    conversion: WasmConversionRecipe::Identity,
                },
                is_send: true,
                hooks: vec![WasmResourceHook::StartOutputStream],
                slot_operation_ids: BTreeMap::from([(WasmOperationKind::OutputStreamStart, 0)]),
            });
        assert!(matches!(
            WasmEnginePlan::build(policy(), vec![incomplete_stream]),
            Err(EngineError::InvalidPlan(message)) if message.contains("complete canonical slot")
        ));
    }

    fn output_stream_slot_operation(
        id: u32,
        kind: WasmOperationKind,
        hook: WasmResourceHook,
    ) -> WasmOperationPlan {
        let mut operation = operation(id);
        operation.kind = kind;
        operation.source_key.kind = kind;
        operation.async_kind = WasmAsyncKind::Async;
        operation.arguments.clear();
        operation.receiver = Some(WasmReceiverBinding {
            rust_type: WasmRustType::Path(
                RustPath::new(["uniffi".to_owned(), "Handle".to_owned()]).unwrap(),
            ),
            carrier: WasmRustCarrier::OutputStream,
            abi_carrier: WasmCarrier::OpaqueHandle,
            ownership: WasmOwnership::Borrowed,
            conversion: WasmConversionRecipe::Identity,
        });
        operation.return_value = if kind == WasmOperationKind::OutputStreamCancel {
            None
        } else {
            Some(WasmReturnBinding {
                rust_type: WasmRustType::StreamStep {
                    item: Box::new(WasmRustType::Scalar(WasmScalarType::U32)),
                    error: Box::new(WasmRustType::Scalar(WasmScalarType::String)),
                },
                carrier: WasmRustCarrier::StreamStep,
                abi_carrier: WasmCarrier::JsValue,
                ownership: WasmOwnership::Owned,
                conversion: WasmConversionRecipe::StreamStep {
                    item: Box::new(WasmConversionRecipe::Identity),
                    error: Box::new(WasmConversionRecipe::Identity),
                },
            })
        };
        operation.call_target = WasmCallTarget::StreamHook {
            parent_operation_id: 0,
            use_site_id: 0,
            hook,
        };
        operation.resource_hooks = vec![hook];
        operation
    }

    fn nested_output_group(path: WasmValuePath) -> WasmStreamResourceGroup {
        let standard = WasmStreamContract {
            direction: WasmStreamDirection::Output,
            lazy_start: true,
            single_consumer: true,
            serial_pull: true,
            exactly_once_cleanup: true,
            explicit_cancel: true,
            eof_is_distinct_from_item: true,
        };
        let scalar = |rust_type, carrier, abi_carrier| WasmValueBinding {
            rust_type,
            carrier,
            abi_carrier,
            conversion: WasmConversionRecipe::Identity,
        };
        WasmStreamResourceGroup {
            use_site: WasmStreamUseSite {
                id: 0,
                operation_id: 0,
                path,
                contract: standard,
            },
            item: scalar(
                WasmRustType::Scalar(WasmScalarType::U32),
                WasmRustCarrier::Primitive,
                WasmCarrier::U32,
            ),
            error: scalar(
                WasmRustType::Scalar(WasmScalarType::String),
                WasmRustCarrier::Primitive,
                WasmCarrier::String,
            ),
            is_send: true,
            hooks: vec![
                WasmResourceHook::PullOutputStream,
                WasmResourceHook::CancelOutputStream,
            ],
            slot_operation_ids: BTreeMap::from([
                (WasmOperationKind::OutputStreamNext, 1),
                (WasmOperationKind::OutputStreamCancel, 2),
            ]),
        }
    }

    #[test]
    fn nested_output_stream_uses_contract_direction_without_start_slot() {
        let mut parent = operation(0);
        parent.stream_resources = vec![nested_output_group(WasmValuePath::new(vec![
            WasmValuePathSegment::Argument(0),
            WasmValuePathSegment::Field("payload".to_owned()),
        ]))];
        let next = output_stream_slot_operation(
            1,
            WasmOperationKind::OutputStreamNext,
            WasmResourceHook::PullOutputStream,
        );
        let cancel = output_stream_slot_operation(
            2,
            WasmOperationKind::OutputStreamCancel,
            WasmResourceHook::CancelOutputStream,
        );
        let plan = WasmEnginePlan::build_with_resource_hooks(
            policy(),
            vec![parent, next, cancel],
            WasmEngineResourceHooks {
                close_output_stream: Some(WasmEngineResourceHook {
                    rust_call: RustPath::new([
                        "crate".to_owned(),
                        "__uniffi_close_output_stream".to_owned(),
                    ])
                    .unwrap(),
                    async_kind: WasmAsyncKind::Sync,
                    fallible: true,
                }),
                ..WasmEngineResourceHooks::default()
            },
        )
        .expect("nested output streams have a canonical two-slot lifecycle");
        assert_eq!(plan.operation_count(), 3);
    }

    #[test]
    fn direct_output_stream_without_start_slot_is_rejected() {
        let mut parent = operation(0);
        parent.stream_resources = vec![nested_output_group(WasmValuePath::return_value())];
        let next = output_stream_slot_operation(
            1,
            WasmOperationKind::OutputStreamNext,
            WasmResourceHook::PullOutputStream,
        );
        let cancel = output_stream_slot_operation(
            2,
            WasmOperationKind::OutputStreamCancel,
            WasmResourceHook::CancelOutputStream,
        );
        assert!(matches!(
            WasmEnginePlan::build(policy(), vec![parent, next, cancel]),
            Err(EngineError::InvalidPlan(message)) if message.contains("non-canonical contract")
        ));
    }

    #[test]
    fn object_result_requires_a_separate_release_hook() {
        let mut object = operation(0);
        let result = object.return_value.as_mut().unwrap();
        result.carrier = WasmRustCarrier::OpaqueHandle;
        result.abi_carrier = WasmCarrier::OpaqueHandle;
        result.conversion = WasmConversionRecipe::Object(9);
        object.resource_use_sites.push(WasmResourceUseSite {
            path: WasmValuePath::return_value(),
            kind: WasmResourceKind::Object,
            type_id: 9,
            ownership: WasmOwnership::Owned,
        });
        assert!(matches!(
            WasmEnginePlan::build(policy(), vec![object]),
            Err(EngineError::Surface(message)) if message.contains("explicit object release export")
        ));
    }
}
