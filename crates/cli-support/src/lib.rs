use anyhow::{bail, Context, Error};
use serde::Serialize;
use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::str;
use walrus::Module;
use wasm_bindgen_shared::identifier::is_valid_ident;

pub(crate) const PLACEHOLDER_MODULE: &str = "__wbindgen_placeholder__";

/// Whether an engine-private operation is synchronous or asynchronous.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendAsyncKind {
    Sync,
    Async,
}

/// Mechanical operation kind projected from the canonical UniFFI plan.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendOperationKind {
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

/// Carrier categories needed by the generated session.  This is not a
/// public JavaScript type graph; the UniFFI frontend has already selected the
/// concrete wasm carrier before reaching cli-support.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendCarrier {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendResourceHook {
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

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum UniFfiBackendValuePathSegment {
    Argument(u32),
    /// The implicit receiver slot that precedes ordinary arguments.
    ///
    /// Keeping the receiver as a path root is important for object methods:
    /// resource conversion must not infer a special top-level field from the
    /// carrier.  The backend session can therefore apply the same recursive
    /// path walker to receivers and ordinary arguments.
    Receiver,
    Return,
    Field(String),
    Variant(String),
    Optional,
    SequenceItem,
    SetItem,
    MapKey,
    MapValue,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct UniFfiBackendValuePath(Vec<UniFfiBackendValuePathSegment>);

impl UniFfiBackendValuePath {
    pub fn new(segments: impl Into<Vec<UniFfiBackendValuePathSegment>>) -> Self {
        Self(segments.into())
    }

    pub fn argument(index: u32) -> Self {
        Self::new(vec![UniFfiBackendValuePathSegment::Argument(index)])
    }

    pub fn receiver() -> Self {
        Self::new(vec![UniFfiBackendValuePathSegment::Receiver])
    }

    pub fn return_value() -> Self {
        Self::new(vec![UniFfiBackendValuePathSegment::Return])
    }

    pub fn segments(&self) -> &[UniFfiBackendValuePathSegment] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendCallbackRetention {
    Scoped,
    Retained,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendCallbackThreading {
    CallingThread,
    MayCrossThread,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendCallbackReentrancy {
    Forbidden,
    Allowed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendCallbackContract {
    pub retention: UniFfiBackendCallbackRetention,
    pub threading: UniFfiBackendCallbackThreading,
    pub reentrancy: UniFfiBackendCallbackReentrancy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendCallbackUseSite {
    pub operation_id: u32,
    pub callback_type_id: u32,
    pub path: UniFfiBackendValuePath,
    pub contract: UniFfiBackendCallbackContract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendStreamDirection {
    Input,
    Output,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendStreamSlot {
    pub use_site_id: u32,
    pub operation_id: u32,
    pub kind: UniFfiBackendOperationKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendStreamGroup {
    pub operation_id: u32,
    pub use_site_id: u32,
    pub path: UniFfiBackendValuePath,
    pub direction: UniFfiBackendStreamDirection,
    pub item_carrier: UniFfiBackendCarrier,
    pub error_carrier: UniFfiBackendCarrier,
    pub is_send: bool,
    pub slots: Vec<UniFfiBackendStreamSlot>,
    pub resource_hooks: Vec<UniFfiBackendResourceHook>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "camelCase")]
pub enum UniFfiBackendResource {
    Object(u32),
    InputStream(u32),
    OutputStream(u32),
}

/// Whether an object use-site borrows an existing session lease or owns a
/// newly returned native handle.  This is deliberately a two-value DTO: the
/// engine/frontend has already selected the concrete ownership semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendResourceOwnership {
    Borrowed,
    Owned,
}

/// One object resource position in an operation value.  `path` is complete,
/// including nested record/variant/optional/sequence/set/map selectors; there
/// are no receiver/return shortcut fields on the operation itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendResourceUseSite {
    pub path: UniFfiBackendValuePath,
    pub kind: UniFfiBackendResource,
    pub ownership: UniFfiBackendResourceOwnership,
}

impl UniFfiBackendResourceUseSite {
    pub fn object(
        path: UniFfiBackendValuePath,
        type_id: u32,
        ownership: UniFfiBackendResourceOwnership,
    ) -> Self {
        Self {
            path,
            kind: UniFfiBackendResource::Object(type_id),
            ownership,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendCallbackDispatch {
    pub callback_type_id: u32,
    pub method_id: u32,
}

/// Whether an operation has a Rust wasm-bindgen shim or is dispatched wholly
/// by the UniFFI host/session runtime.  Host-dispatched entries still occupy
/// their canonical operation ID and descriptor slot, but deliberately have no
/// executable raw export in the generated wasm module.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UniFfiBackendDispatch {
    NativeCall,
    HostDispatched,
}

/// One raw operation exposed only through the UniFFI backend table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniFfiBackendOperation {
    operation_id: u32,
    #[serde(skip)]
    raw_export_name: String,
    async_kind: UniFfiBackendAsyncKind,
    kind: UniFfiBackendOperationKind,
    fallible: bool,
    argument_count: usize,
    argument_carriers: Vec<UniFfiBackendCarrier>,
    return_carrier: Option<UniFfiBackendCarrier>,
    dispatch: UniFfiBackendDispatch,
    host_argument: bool,
    callback_dispatch: Option<UniFfiBackendCallbackDispatch>,
    callback_use_sites: Vec<UniFfiBackendCallbackUseSite>,
    stream_groups: Vec<UniFfiBackendStreamGroup>,
    stream_slot: Option<UniFfiBackendStreamSlot>,
    has_receiver: bool,
    resource_use_sites: Vec<UniFfiBackendResourceUseSite>,
    resource_hooks: Vec<UniFfiBackendResourceHook>,
}

impl UniFfiBackendOperation {
    pub fn new(
        operation_id: u32,
        raw_export_name: impl Into<String>,
        async_kind: UniFfiBackendAsyncKind,
        kind: UniFfiBackendOperationKind,
        fallible: bool,
        argument_carriers: Vec<UniFfiBackendCarrier>,
        return_carrier: Option<UniFfiBackendCarrier>,
    ) -> Result<Self, Error> {
        let raw_export_name = raw_export_name.into();
        if !is_valid_ident(&raw_export_name) {
            bail!("UniFFI raw export `{raw_export_name}` is not a JavaScript identifier");
        }
        let argument_count = argument_carriers.len();
        Ok(Self {
            operation_id,
            raw_export_name,
            async_kind,
            kind,
            fallible,
            argument_count,
            argument_carriers,
            return_carrier,
            dispatch: UniFfiBackendDispatch::NativeCall,
            host_argument: false,
            callback_dispatch: None,
            callback_use_sites: Vec::new(),
            stream_groups: Vec::new(),
            stream_slot: None,
            has_receiver: false,
            resource_use_sites: Vec::new(),
            resource_hooks: Vec::new(),
        })
    }

    pub fn with_callback_dispatch(mut self, callback_type_id: u32, method_id: u32) -> Self {
        self.callback_dispatch = Some(UniFfiBackendCallbackDispatch {
            callback_type_id,
            method_id,
        });
        self
    }

    pub fn with_dispatch(mut self, dispatch: UniFfiBackendDispatch) -> Self {
        self.dispatch = dispatch;
        self
    }

    pub fn with_host_argument(mut self, host_argument: bool) -> Self {
        self.host_argument = host_argument;
        self
    }

    pub fn with_callback_use_sites(
        mut self,
        callback_use_sites: Vec<UniFfiBackendCallbackUseSite>,
    ) -> Self {
        self.callback_use_sites = callback_use_sites;
        self
    }

    pub fn with_stream_groups(mut self, stream_groups: Vec<UniFfiBackendStreamGroup>) -> Self {
        self.stream_groups = stream_groups;
        self
    }

    pub fn with_stream_slot(mut self, stream_slot: Option<UniFfiBackendStreamSlot>) -> Self {
        self.stream_slot = stream_slot;
        self
    }

    pub fn with_receiver(mut self, has_receiver: bool) -> Self {
        self.has_receiver = has_receiver;
        self
    }

    pub fn with_resource_use_sites(
        mut self,
        resource_use_sites: Vec<UniFfiBackendResourceUseSite>,
    ) -> Self {
        self.resource_use_sites = resource_use_sites;
        self
    }

    pub fn with_resource_hooks(mut self, resource_hooks: Vec<UniFfiBackendResourceHook>) -> Self {
        self.resource_hooks = resource_hooks;
        self
    }

    pub fn operation_id(&self) -> u32 {
        self.operation_id
    }

    pub fn raw_export_name(&self) -> &str {
        &self.raw_export_name
    }

    pub fn dispatch(&self) -> UniFfiBackendDispatch {
        self.dispatch
    }

    pub fn has_receiver(&self) -> bool {
        self.has_receiver
    }

    pub fn resource_use_sites(&self) -> &[UniFfiBackendResourceUseSite] {
        &self.resource_use_sites
    }
}

/// Engine-private resource hooks are separate from the canonical operation
/// table.  A release/close hook must never be inferred from a business
/// operation ID.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UniFfiBackendResourceExports {
    release_object_export_name: Option<String>,
    close_output_stream_export_name: Option<String>,
}

/// The engine-owned teardown policy projected from UniFFI's canonical bridge
/// plan.  This type is deliberately tiny and in-memory: it has no parser,
/// default, version, identity, or persisted representation.  Callers must
/// supply the canonical values when constructing a backend surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineAction {
    /// Invalidate the current generation and detach late JavaScript results.
    Detach,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClosePolicy {
    pub grace_ms: u32,
    pub on_deadline: DeadlineAction,
}

impl ClosePolicy {
    /// Validate the small subset of policy values that can safely be
    /// represented by JavaScript's Number without rounding.
    pub fn validate(&self) -> Result<(), Error> {
        // `u32` is already below Number.MAX_SAFE_INTEGER.  Keep the explicit
        // bound here so changing the carrier later cannot silently weaken the
        // JavaScript contract.
        if u64::from(self.grace_ms) > 9_007_199_254_740_991 {
            bail!("UniFFI close policy grace_ms is not exactly representable in JavaScript");
        }
        match self.on_deadline {
            DeadlineAction::Detach => Ok(()),
        }
    }

    pub fn grace_ms(&self) -> u32 {
        self.grace_ms
    }

    pub fn on_deadline(&self) -> DeadlineAction {
        self.on_deadline
    }
}

impl UniFfiBackendResourceExports {
    pub fn new(
        release_object_export_name: Option<String>,
        close_output_stream_export_name: Option<String>,
    ) -> Result<Self, Error> {
        for (role, name) in [
            ("object release", release_object_export_name.as_deref()),
            (
                "output stream close",
                close_output_stream_export_name.as_deref(),
            ),
        ] {
            if let Some(name) = name {
                if !is_valid_ident(name) {
                    bail!("UniFFI {role} export `{name}` is not a JavaScript identifier");
                }
            }
        }
        if release_object_export_name == close_output_stream_export_name
            && release_object_export_name.is_some()
        {
            bail!("UniFFI resource hook export names must be unique");
        }
        Ok(Self {
            release_object_export_name,
            close_output_stream_export_name,
        })
    }

    pub fn release_object_export_name(&self) -> Option<&str> {
        self.release_object_export_name.as_deref()
    }

    pub fn close_output_stream_export_name(&self) -> Option<&str> {
        self.close_output_stream_export_name.as_deref()
    }
}

/// In-memory configuration for the private UniFFI backend surface.
///
/// This is deliberately not serializable and does not contain an artifact
/// identity, schema version, digest, or manifest field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UniFfiBackendConfig {
    factory_export_name: String,
    operations: Vec<UniFfiBackendOperation>,
    resource_exports: UniFfiBackendResourceExports,
    close_policy: ClosePolicy,
}

impl UniFfiBackendConfig {
    pub fn new(
        factory_export_name: impl Into<String>,
        mut operations: Vec<UniFfiBackendOperation>,
        resource_exports: UniFfiBackendResourceExports,
        close_policy: ClosePolicy,
    ) -> Result<Self, Error> {
        close_policy.validate()?;
        let factory_export_name = factory_export_name.into();
        if !is_valid_ident(&factory_export_name) {
            bail!("UniFFI backend factory `{factory_export_name}` is not a JavaScript identifier");
        }
        operations.sort_by_key(UniFfiBackendOperation::operation_id);
        let mut names = HashSet::new();
        for (expected, operation) in operations.iter().enumerate() {
            let expected = u32::try_from(expected).context("too many UniFFI backend operations")?;
            if operation.operation_id != expected {
                bail!(
                    "UniFFI backend operation IDs must be dense: expected {expected}, found {}",
                    operation.operation_id
                );
            }
            if operation.dispatch == UniFfiBackendDispatch::NativeCall
                && operation.raw_export_name == factory_export_name
            {
                bail!("UniFFI backend factory name collides with a raw operation export");
            }
            if operation.dispatch == UniFfiBackendDispatch::NativeCall
                && !names.insert(operation.raw_export_name.clone())
            {
                bail!(
                    "duplicate UniFFI raw operation export `{}`",
                    operation.raw_export_name
                );
            }
            validate_uniffi_backend_operation(operation, &operations)?;
        }
        for name in [
            resource_exports.release_object_export_name(),
            resource_exports.close_output_stream_export_name(),
        ]
        .into_iter()
        .flatten()
        {
            if name == factory_export_name || names.contains(name) {
                bail!("UniFFI resource hook export `{name}` collides with another backend export");
            }
        }
        let needs_object_release = operations.iter().any(|operation| {
            operation.resource_use_sites.iter().any(|use_site| {
                use_site.ownership == UniFfiBackendResourceOwnership::Owned
                    && matches!(use_site.kind, UniFfiBackendResource::Object(_))
            })
        });
        if needs_object_release && resource_exports.release_object_export_name().is_none() {
            bail!("UniFFI object results require an explicit object release export");
        }
        let needs_output_close = operations.iter().any(|operation| {
            operation
                .stream_groups
                .iter()
                .any(|group| group.direction == UniFfiBackendStreamDirection::Output)
        });
        if needs_output_close && resource_exports.close_output_stream_export_name().is_none() {
            bail!("UniFFI output streams require an explicit close export");
        }
        Ok(Self {
            factory_export_name,
            operations,
            resource_exports,
            close_policy,
        })
    }

    pub fn factory_export_name(&self) -> &str {
        &self.factory_export_name
    }

    pub fn operations(&self) -> &[UniFfiBackendOperation] {
        &self.operations
    }

    pub fn resource_exports(&self) -> &UniFfiBackendResourceExports {
        &self.resource_exports
    }

    pub fn close_policy(&self) -> ClosePolicy {
        self.close_policy
    }

    pub(crate) fn operation_metadata_json(&self) -> Result<String, Error> {
        serde_json::to_string(&self.operations).context("serialize UniFFI backend operation plan")
    }
}

fn validate_uniffi_backend_operation(
    operation: &UniFfiBackendOperation,
    operations: &[UniFfiBackendOperation],
) -> Result<(), Error> {
    if operation.argument_count != operation.argument_carriers.len() {
        bail!(
            "UniFFI operation {} has an inconsistent argument carrier table",
            operation.operation_id
        );
    }
    let callback_target = operation.callback_dispatch.is_some();
    if callback_target != (operation.kind == UniFfiBackendOperationKind::CallbackMethod) {
        bail!(
            "UniFFI operation {} has an invalid callback Host dispatch",
            operation.operation_id
        );
    }
    if callback_target && operation.host_argument {
        bail!(
            "UniFFI callback Host operation {} cannot also receive the engine Host argument",
            operation.operation_id
        );
    }
    if operation.dispatch == UniFfiBackendDispatch::HostDispatched && operation.host_argument {
        bail!(
            "UniFFI host-dispatched operation {} cannot also receive the engine Host argument",
            operation.operation_id
        );
    }
    if callback_target && operation.dispatch != UniFfiBackendDispatch::HostDispatched {
        bail!(
            "UniFFI callback operation {} must be host-dispatched",
            operation.operation_id
        );
    }
    if operation
        .resource_use_sites
        .iter()
        .filter(|use_site| use_site.path.segments().is_empty())
        .count()
        != 0
    {
        bail!(
            "UniFFI operation {} has an empty object resource value path",
            operation.operation_id
        );
    }
    let mut resource_paths = HashSet::new();
    for use_site in &operation.resource_use_sites {
        if !resource_paths.insert(use_site.path.clone()) {
            bail!(
                "UniFFI operation {} has duplicate object resource use-site paths",
                operation.operation_id
            );
        }
        let root = use_site.path.segments().first().expect("checked above");
        match root {
            UniFfiBackendValuePathSegment::Receiver if !operation.has_receiver => bail!(
                "UniFFI object resource receiver path is invalid for operation {}",
                operation.operation_id
            ),
            UniFfiBackendValuePathSegment::Argument(index)
                if (*index as usize) >= operation.argument_count =>
            {
                bail!(
                    "UniFFI object resource argument {} is out of range for operation {}",
                    index,
                    operation.operation_id
                )
            }
            UniFfiBackendValuePathSegment::Argument(_)
            | UniFfiBackendValuePathSegment::Receiver
            | UniFfiBackendValuePathSegment::Return => {}
            _ => bail!(
                "UniFFI object resource path for operation {} has an invalid root",
                operation.operation_id
            ),
        }
        if !matches!(use_site.kind, UniFfiBackendResource::Object(_)) {
            bail!(
                "UniFFI operation {} has a non-object resource use-site",
                operation.operation_id
            );
        }
        if use_site.path.segments().iter().skip(1).any(|segment| {
            matches!(segment, UniFfiBackendValuePathSegment::Field(name) | UniFfiBackendValuePathSegment::Variant(name) if name.is_empty())
        }) {
            bail!(
                "UniFFI operation {} has an empty object resource path selector",
                operation.operation_id
            );
        }
        let is_return = matches!(root, UniFfiBackendValuePathSegment::Return);
        let expected = if is_return {
            UniFfiBackendResourceOwnership::Owned
        } else {
            UniFfiBackendResourceOwnership::Borrowed
        };
        if use_site.ownership != expected {
            bail!(
                "UniFFI object resource use-site in operation {} has invalid {:?} ownership for its root",
                operation.operation_id,
                root
            );
        }
        if use_site.path.segments().iter().skip(1).any(|segment| {
            matches!(
                segment,
                UniFfiBackendValuePathSegment::Argument(_)
                    | UniFfiBackendValuePathSegment::Receiver
                    | UniFfiBackendValuePathSegment::Return
            )
        }) {
            bail!(
                "UniFFI object resource path for operation {} has a nested root segment",
                operation.operation_id
            );
        }
    }
    if matches!(
        operation.kind,
        UniFfiBackendOperationKind::InputStreamPull
            | UniFfiBackendOperationKind::InputStreamCancel
            | UniFfiBackendOperationKind::OutputStreamNext
            | UniFfiBackendOperationKind::OutputStreamCancel
    ) && operation.async_kind != UniFfiBackendAsyncKind::Async
    {
        bail!(
            "UniFFI stream operation {} must be asynchronous",
            operation.operation_id
        );
    }
    if let Some(slot) = &operation.stream_slot {
        let start_alias = slot.kind == UniFfiBackendOperationKind::OutputStreamStart
            && operation.stream_groups.iter().any(|group| {
                group.direction == UniFfiBackendStreamDirection::Output
                    && group.use_site_id == slot.use_site_id
            });
        if slot.operation_id != operation.operation_id
            || (slot.kind != operation.kind && !start_alias)
        {
            bail!(
                "UniFFI operation {} has a mismatched stream slot identity",
                operation.operation_id
            );
        }
    }
    for callback in &operation.callback_use_sites {
        if callback.operation_id != operation.operation_id {
            bail!(
                "UniFFI callback use-site references operation {}, expected {}",
                callback.operation_id,
                operation.operation_id
            );
        }
        validate_uniffi_value_path(operation, &callback.path, "callback")?;
    }
    for stream in &operation.stream_groups {
        if stream.operation_id != operation.operation_id {
            bail!(
                "UniFFI stream use-site references operation {}, expected {}",
                stream.operation_id,
                operation.operation_id
            );
        }
        validate_uniffi_value_path(operation, &stream.path, "stream")?;
        let expected = match stream.direction {
            UniFfiBackendStreamDirection::Input => [
                UniFfiBackendOperationKind::InputStreamPull,
                UniFfiBackendOperationKind::InputStreamCancel,
            ]
            .as_slice(),
            UniFfiBackendStreamDirection::Output => [
                UniFfiBackendOperationKind::OutputStreamStart,
                UniFfiBackendOperationKind::OutputStreamNext,
                UniFfiBackendOperationKind::OutputStreamCancel,
            ]
            .as_slice(),
        };
        if stream.slots.len() != expected.len()
            || stream
                .slots
                .iter()
                .any(|slot| !expected.contains(&slot.kind))
        {
            bail!(
                "UniFFI stream use-site {} has a non-canonical slot set",
                stream.use_site_id
            );
        }
        let mut slot_kinds = HashSet::new();
        let mut slot_ids = HashSet::new();
        for kind in expected {
            if !stream.slots.iter().any(|slot| slot.kind == *kind) {
                bail!(
                    "UniFFI stream use-site {} is missing its {:?} slot",
                    stream.use_site_id,
                    kind
                );
            }
        }
        for slot in &stream.slots {
            if !slot_kinds.insert(slot.kind) || !slot_ids.insert(slot.operation_id) {
                bail!(
                    "UniFFI stream use-site {} has duplicate slots",
                    stream.use_site_id
                );
            }
            if slot.use_site_id != stream.use_site_id {
                bail!(
                    "UniFFI stream slot {} has a mismatched use-site ID",
                    slot.operation_id
                );
            }
            let Some(slot_operation) = operations.get(slot.operation_id as usize) else {
                bail!(
                    "UniFFI stream use-site {} references unknown operation {}",
                    stream.use_site_id,
                    slot.operation_id
                );
            };
            if slot_operation.stream_slot.as_ref() != Some(slot) {
                bail!(
                    "UniFFI stream use-site {} has a mismatched slot operation {}",
                    stream.use_site_id,
                    slot.operation_id
                );
            }
        }
    }
    Ok(())
}

fn validate_uniffi_value_path(
    operation: &UniFfiBackendOperation,
    path: &UniFfiBackendValuePath,
    role: &str,
) -> Result<(), Error> {
    let Some(root) = path.segments().first() else {
        bail!(
            "UniFFI {role} use-site for operation {} has an empty value path",
            operation.operation_id
        );
    };
    match root {
        UniFfiBackendValuePathSegment::Argument(index)
            if (*index as usize) < operation.argument_count => {}
        UniFfiBackendValuePathSegment::Receiver if operation.has_receiver => {}
        UniFfiBackendValuePathSegment::Return => {}
        UniFfiBackendValuePathSegment::Argument(index) => bail!(
            "UniFFI {role} use-site argument {} is out of range for operation {}",
            index,
            operation.operation_id
        ),
        _ => bail!(
            "UniFFI {role} use-site for operation {} has a non-root first segment",
            operation.operation_id
        ),
    }
    if path.segments().iter().skip(1).any(|segment| {
        matches!(
            segment,
            UniFfiBackendValuePathSegment::Argument(_)
                | UniFfiBackendValuePathSegment::Receiver
                | UniFfiBackendValuePathSegment::Return
        )
    }) {
        bail!(
            "UniFFI {role} use-site for operation {} has a nested root segment",
            operation.operation_id
        );
    }
    Ok(())
}

/// Public wasm-bindgen bindings or the private UniFFI operation-table surface.
/// This axis is independent of [`OutputMode`], so the same backend can use the
/// Web, bundler, Node, or no-modules loader policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingSurface {
    Public,
    UniFfiBackend(UniFfiBackendConfig),
}

mod decode;
mod descriptor;
mod descriptors;
mod externref;
mod interpreter;
mod intrinsic;
mod js;
mod multivalue;
mod suggest;
mod transforms;
pub mod wasm2es6js;
mod wasm_conventions;
mod wit;

pub struct Bindgen {
    input: Input,
    out_name: Option<String>,
    mode: OutputMode,
    debug: bool,
    typescript: bool,
    omit_imports: bool,
    demangle: bool,
    keep_lld_exports: bool,
    keep_debug: bool,
    remove_name_section: bool,
    remove_producers_section: bool,
    omit_default_module_path: bool,
    emit_start: bool,
    externref: bool,
    multi_value: bool,
    encode_into: EncodeInto,
    split_linked_modules: bool,
    generate_reset_state: bool,
    force_enable_abort_handler: bool,
    binding_surface: BindingSurface,
}

pub struct Output {
    module: walrus::Module,
    stem: String,
    generated: Generated,
}

struct Generated {
    mode: OutputMode,
    js: String,
    ts: String,
    start: Option<String>,
    snippets: BTreeMap<String, Vec<String>>,
    local_modules: HashMap<String, String>,
    npm_dependencies: HashMap<String, (PathBuf, String)>,
    typescript: bool,
    /// For `OutputMode::Emscripten` only: the contents of a sidecar file
    /// emcc loads via `--extern-pre-js`, containing ESM `import` statements
    /// that must live at module top-level. Empty for other modes.
    emscripten_extern_pre_js: String,
}

#[derive(Clone)]
enum OutputMode {
    Bundler { browser_only: bool },
    Web,
    NoModules { global: String },
    Node { module: bool },
    Deno,
    Module,
    Emscripten,
}

enum Input {
    Path(PathBuf),
    Module(Module, String),
    Bytes(Vec<u8>, String),
    None,
}

#[derive(Debug, Clone, Copy)]
pub enum EncodeInto {
    Test,
    Always,
    Never,
}

impl Bindgen {
    pub fn new() -> Bindgen {
        let externref =
            env::var("WASM_BINDGEN_ANYREF").is_ok() || env::var("WASM_BINDGEN_EXTERNREF").is_ok();
        let multi_value = env::var("WASM_BINDGEN_MULTI_VALUE").is_ok();
        Bindgen {
            input: Input::None,
            out_name: None,
            mode: OutputMode::Bundler {
                browser_only: false,
            },
            debug: false,
            typescript: false,
            omit_imports: false,
            demangle: true,
            keep_lld_exports: false,
            keep_debug: false,
            remove_name_section: false,
            remove_producers_section: false,
            emit_start: true,
            externref,
            multi_value,
            encode_into: EncodeInto::Test,
            omit_default_module_path: true,
            split_linked_modules: false,
            generate_reset_state: false,
            force_enable_abort_handler: false,
            binding_surface: BindingSurface::Public,
        }
    }

    pub fn input_path<P: AsRef<Path>>(&mut self, path: P) -> &mut Bindgen {
        self.input = Input::Path(path.as_ref().to_path_buf());
        self
    }

    pub fn out_name(&mut self, name: &str) -> &mut Bindgen {
        self.out_name = Some(name.to_string());
        self
    }

    #[deprecated = "automatically detected via `-Ctarget-feature=+reference-types`"]
    pub fn reference_types(&mut self, enable: bool) -> &mut Bindgen {
        self.externref = enable;
        self
    }

    /// Explicitly specify the already parsed input module.
    pub fn input_module(&mut self, name: &str, module: Module) -> &mut Bindgen {
        let name = name.to_string();
        self.input = Input::Module(module, name);
        self
    }

    /// Specify the input as the provided Wasm bytes.
    pub fn input_bytes(&mut self, name: &str, bytes: Vec<u8>) -> &mut Bindgen {
        let name = name.to_string();
        self.input = Input::Bytes(bytes, name);
        self
    }

    fn switch_mode(&mut self, mode: OutputMode, flag: &str) -> Result<(), Error> {
        match self.mode {
            OutputMode::Bundler { .. } => self.mode = mode,
            _ => bail!("cannot specify `{flag}` with another output mode already specified"),
        }
        Ok(())
    }

    pub fn nodejs(&mut self, node: bool) -> Result<&mut Bindgen, Error> {
        if node {
            self.switch_mode(OutputMode::Node { module: false }, "--target nodejs")?;
        }
        Ok(self)
    }

    pub fn nodejs_module(&mut self, node: bool) -> Result<&mut Bindgen, Error> {
        if node {
            self.switch_mode(
                OutputMode::Node { module: true },
                "--target experimental-nodejs-module",
            )?;
        }
        Ok(self)
    }

    pub fn bundler(&mut self, bundler: bool) -> Result<&mut Bindgen, Error> {
        if bundler {
            self.switch_mode(
                OutputMode::Bundler {
                    browser_only: false,
                },
                "--target bundler",
            )?;
        }
        Ok(self)
    }

    pub fn web(&mut self, web: bool) -> Result<&mut Bindgen, Error> {
        if web {
            self.switch_mode(OutputMode::Web, "--target web")?;
        }
        Ok(self)
    }

    pub fn no_modules(&mut self, no_modules: bool) -> Result<&mut Bindgen, Error> {
        if no_modules {
            self.switch_mode(
                OutputMode::NoModules {
                    global: "wasm_bindgen".to_string(),
                },
                "--target no-modules",
            )?;
        }
        Ok(self)
    }

    pub fn browser(&mut self, browser: bool) -> Result<&mut Bindgen, Error> {
        if browser {
            match &mut self.mode {
                OutputMode::Bundler { browser_only } => *browser_only = true,
                _ => bail!("cannot specify `--browser` with other output types"),
            }
        }
        Ok(self)
    }

    pub fn deno(&mut self, deno: bool) -> Result<&mut Bindgen, Error> {
        if deno {
            self.switch_mode(OutputMode::Deno, "--target deno")?;
            self.encode_into(EncodeInto::Always);
        }
        Ok(self)
    }

    pub fn module(&mut self, source_phase: bool) -> Result<&mut Bindgen, Error> {
        if source_phase {
            self.switch_mode(OutputMode::Module, "--target module")?;
        }
        Ok(self)
    }

    pub fn no_modules_global(&mut self, name: &str) -> Result<&mut Bindgen, Error> {
        match &mut self.mode {
            OutputMode::NoModules { global } => *global = name.to_string(),
            _ => bail!("can only specify `--no-modules-global` with `--target no-modules`"),
        }
        Ok(self)
    }

    pub fn debug(&mut self, debug: bool) -> &mut Bindgen {
        self.debug = debug;
        self
    }

    pub fn typescript(&mut self, typescript: bool) -> &mut Bindgen {
        self.typescript = typescript;
        self
    }

    /// Select the export surface independently from the target loader mode.
    /// No CLI flag is provided; engine integrations use this library API.
    pub fn binding_surface(&mut self, surface: BindingSurface) -> &mut Bindgen {
        self.binding_surface = surface;
        self
    }

    pub fn selected_binding_surface(&self) -> &BindingSurface {
        &self.binding_surface
    }

    pub(crate) fn typescript_enabled(&self) -> bool {
        self.typescript && matches!(self.binding_surface, BindingSurface::Public)
    }

    pub fn omit_imports(&mut self, omit_imports: bool) -> &mut Bindgen {
        self.omit_imports = omit_imports;
        self
    }

    pub fn demangle(&mut self, demangle: bool) -> &mut Bindgen {
        self.demangle = demangle;
        self
    }

    pub fn keep_lld_exports(&mut self, keep_lld_exports: bool) -> &mut Bindgen {
        self.keep_lld_exports = keep_lld_exports;
        self
    }

    pub fn keep_debug(&mut self, keep_debug: bool) -> &mut Bindgen {
        self.keep_debug = keep_debug;
        self
    }

    pub fn remove_name_section(&mut self, remove: bool) -> &mut Bindgen {
        self.remove_name_section = remove;
        self
    }

    pub fn remove_producers_section(&mut self, remove: bool) -> &mut Bindgen {
        self.remove_producers_section = remove;
        self
    }

    pub fn emit_start(&mut self, emit: bool) -> &mut Bindgen {
        self.emit_start = emit;
        self
    }

    pub fn encode_into(&mut self, mode: EncodeInto) -> &mut Bindgen {
        self.encode_into = mode;
        self
    }

    pub fn omit_default_module_path(&mut self, omit_default_module_path: bool) -> &mut Bindgen {
        self.omit_default_module_path = omit_default_module_path;
        self
    }

    pub fn split_linked_modules(&mut self, split_linked_modules: bool) -> &mut Bindgen {
        self.split_linked_modules = split_linked_modules;
        self
    }

    pub fn reset_state_function(&mut self, generate_reset_state: bool) -> &mut Bindgen {
        self.generate_reset_state = generate_reset_state;
        self
    }

    pub fn force_enable_abort_handler(&mut self, force_enable_abort_handler: bool) -> &mut Self {
        self.force_enable_abort_handler = force_enable_abort_handler;
        self
    }

    pub fn generate<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        self.generate_output()?.emit(path.as_ref())
    }

    pub fn stem(&self) -> Result<&str, Error> {
        Ok(match &self.input {
            Input::None => bail!("must have an input by now"),
            Input::Module(_, name) | Input::Bytes(_, name) => name,
            Input::Path(path) => match &self.out_name {
                Some(name) => name,
                None => path.file_stem().unwrap().to_str().unwrap(),
            },
        })
    }

    pub fn generate_output(&mut self) -> Result<Output, Error> {
        let mut module = match self.input {
            Input::None => bail!("must have an input by now"),
            Input::Module(ref mut m, _) => {
                let blank_module = Module::default();
                mem::replace(m, blank_module)
            }
            Input::Path(ref path) => {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("failed reading '{}'", path.display()))?;
                self.module_from_bytes(&bytes).with_context(|| {
                    format!("failed getting Wasm module for '{}'", path.display())
                })?
            }
            Input::Bytes(ref bytes, _) => self
                .module_from_bytes(bytes)
                .context("failed getting Wasm module")?,
        };

        if module
            .customs
            .remove_raw("__wasm_bindgen_emscripten_marker")
            .is_some()
        {
            // Force the internal configuration to Emscripten mode.
            self.mode = OutputMode::Emscripten;
        }

        // Enable reference type transformations if the module is already using it.
        if let Ok(true) = wasm_conventions::target_feature(&module, "reference-types") {
            self.externref = true;
        }

        // Enable multivalue transformations if the module is already using it.
        if let Ok(true) = wasm_conventions::target_feature(&module, "multivalue") {
            self.multi_value = true;
        }

        // Check that no exported symbol is called "default" if we target web.
        if matches!(self.mode, OutputMode::Web)
            && module.exports.iter().any(|export| export.name == "default")
        {
            bail!("exported symbol \"default\" not allowed for --target web")
        }

        // Check that reset_state is only used with --target module, web, or node
        if self.generate_reset_state
            && !matches!(
                self.mode,
                OutputMode::Module | OutputMode::Web | OutputMode::Node { module: false }
            )
        {
            bail!("--experimental-reset-state-function is only supported for --target module, --target web, or --target nodejs")
        }

        let thread_count = transforms::threads::run(&mut module)
            .with_context(|| "failed to prepare module for threading")?;

        // If requested, turn all mangled symbols into prettier unmangled
        // symbols with the help of `rustc-demangle`.
        if self.demangle {
            demangle(&mut module);
        }
        if !self.keep_lld_exports && !self.mode.emscripten() {
            unexported_unused_lld_things(&mut module);
        }
        // Quick fix for https://github.com/wasm-bindgen/wasm-bindgen/pull/4931
        // which is likely a compiler bug
        {
            let exn_import = module.imports.iter().find_map(|impt| match impt.kind {
                walrus::ImportKind::Tag(id)
                    if impt.module == "env" && impt.name == "__cpp_exception" =>
                {
                    Some((impt, id))
                }
                _ => None,
            });
            if let Some((import, id)) = exn_import {
                let original_import_id = import.id();
                let tag = module.tags.get_mut(id);
                tag.kind = walrus::TagKind::Local;
                module.imports.delete(original_import_id);
                module.exports.add("__cpp_exception", tag.id);
            }

            // We're making quite a few changes, list ourselves as a producer.
            module
                .producers
                .add_processed_by("wasm-bindgen", &wasm_bindgen_shared::version());
        }
        // Parse and remove our custom section before executing descriptors.
        // That includes checking that the binary has the same schema version
        // as this version of the CLI, which is why we do it first - to make
        // sure that this binary was produced by a compatible version of the
        // wasm-bindgen macro before attempting to interpret our unstable
        // descriptor format. That way, we give a more helpful version mismatch
        // error instead of an unhelpful panic if an incompatible descriptor is
        // found.
        let mut storage = Vec::new();
        let programs = wit::extract_programs(&mut module, &mut storage)?;

        // Learn about the type signatures of all wasm-bindgen imports and
        // exports by executing `__wbindgen_describe_*` functions. This'll
        // effectively move all the descriptor functions to their own custom
        // sections.
        descriptors::execute(&mut module)?;

        // Process the custom section we extracted earlier. In its stead insert
        // a forward-compatible Wasm interface types section as well as an
        // auxiliary section for all sorts of miscellaneous information and
        // features #[wasm_bindgen] supports that aren't covered by wasm
        // interface types.
        wit::process(self, &mut module, programs, thread_count)?;

        // Now that we've got type information from the webidl processing pass,
        // touch up the output of rustc to insert externref shims where necessary.
        // This is only done if the externref pass is enabled, which it's
        // currently off-by-default since `externref` is still in development in
        // engines.
        //
        // If the externref pass isn't necessary, then we blanket delete the
        // export of all our externref intrinsics which will get cleaned up in the
        // GC pass before JS generation.
        if self.externref {
            externref::process(&mut module)?;
        } else {
            let ids = module
                .exports
                .iter()
                .filter(|e| e.name.starts_with("__externref"))
                .map(|e| e.id())
                .collect::<Vec<_>>();
            for id in ids {
                module.exports.delete(id);
            }
            // Clean up element segments as well if they have holes in them
            // after some of our transformations, because non-externref engines
            // only support contiguous arrays of function references in element
            // segments.
            externref::force_contiguous_elements(&mut module)?;
        }

        // Using all of our metadata convert our module to a multi-value using
        // module if applicable.
        if self.multi_value {
            multivalue::run(&mut module)
                .context("failed to transform return pointers into multi-value Wasm")?;
        }

        // Generate Wasm catch wrappers for imports with #[wasm_bindgen(catch)].
        // This runs after externref processing so that we have access to the
        // externref table and allocation function.
        //
        // Emscripten output may contain wasm exception-handling instructions
        // from linked libc++ / embind that have no relation to wasm-bindgen's
        // `#[wasm_bindgen(catch)]` machinery, and the wasm-bindgen runtime
        // intrinsics (`__externref_table`, `__externref_table_alloc`,
        // `__wbindgen_exn_store`) may be absent. Skip the transform until
        // proper emscripten-mode catch support lands.
        if !matches!(self.mode, OutputMode::Emscripten) {
            generate_wasm_catch_wrappers(&mut module, self.force_enable_abort_handler)?;
        }

        // We've done a whole bunch of transformations to the Wasm module, many
        // of which leave "garbage" lying around, so let's prune out all our
        // unnecessary things here.
        gc_module_and_adapters(&mut module);

        let stem = self.stem()?;

        // Now we execute the JS generation passes to actually emit JS/TypeScript/etc.
        let aux = module
            .customs
            .delete_typed::<wit::WasmBindgenAux>()
            .expect("aux section should be present");
        let adapters = module
            .customs
            .delete_typed::<wit::NonstandardWitSection>()
            .unwrap();
        let mut cx = js::Context::new(&mut module, self, &adapters, &aux)?;
        cx.generate()?;
        let js::FinalizedOutput {
            js,
            ts,
            start,
            emscripten_extern_pre_js,
        } = cx.finalize(stem)?;
        let generated = Generated {
            snippets: aux.snippets.clone(),
            local_modules: aux.local_modules.clone(),
            mode: self.mode.clone(),
            typescript: self.typescript_enabled(),
            npm_dependencies: cx.npm_dependencies.clone(),
            js,
            ts,
            start,
            emscripten_extern_pre_js,
        };

        Ok(Output {
            module,
            stem: stem.to_string(),
            generated,
        })
    }

    fn module_from_bytes(&self, bytes: &[u8]) -> Result<Module, Error> {
        walrus::ModuleConfig::new()
            // Skip validation of the module as LLVM's output is
            // generally already well-formed and so we won't gain much
            // from re-validating. Additionally LLVM's current output
            // for threads includes atomic instructions but doesn't
            // include shared memory, so it fails that part of
            // validation!
            .strict_validate(false)
            .generate_dwarf(self.keep_debug)
            .generate_name_section(!self.remove_name_section)
            .generate_producers_section(!self.remove_producers_section)
            .parse(bytes)
            .context("failed to parse input as wasm")
    }

    fn local_module_name(&self, module: &str) -> String {
        format!("./snippets/{module}")
    }

    fn inline_js_module_name(
        &self,
        unique_crate_identifier: &str,
        snippet_idx_in_crate: usize,
    ) -> String {
        format!("./snippets/{unique_crate_identifier}/inline{snippet_idx_in_crate}.js",)
    }
}

fn reset_indentation(s: &str) -> String {
    let mut indent: u32 = 0;
    let mut dst = String::new();

    fn is_doc_comment(line: &str) -> bool {
        line.starts_with("*")
    }

    static TAB: &str = "    ";

    for line in s.trim().lines() {
        let line = line.trim();

        // handle doc comments separately
        if is_doc_comment(line) {
            for _ in 0..indent {
                dst.push_str(TAB);
            }
            dst.push(' ');
            dst.push_str(line);
            dst.push('\n');
            continue;
        }

        if line.starts_with('}') {
            indent = indent.saturating_sub(1);
        }

        let extra = if line.starts_with(':') || line.starts_with('?') {
            1
        } else {
            0
        };
        if !line.is_empty() {
            for _ in 0..indent + extra {
                dst.push_str(TAB);
            }
            dst.push_str(line);
        }
        dst.push('\n');

        if line.ends_with('{') {
            indent += 1;
        }
    }
    dst
}

/// Since Rust will soon adopt v0 mangling as the default,
/// and the `rustc_demangle` crate doesn't output closure disambiguators,
/// duplicate symbols can appear. We handle this case manually.
///
/// issue: <https://github.com/wasm-bindgen/wasm-bindgen/issues/4820>
fn demangle(module: &mut Module) {
    let (lower, upper) = module.funcs.iter().size_hint();
    let mut counter: HashMap<String, i32> = HashMap::with_capacity(upper.unwrap_or(lower));

    for func in module.funcs.iter_mut() {
        let Some(name) = &func.name else {
            continue;
        };

        let Ok(sym) = rustc_demangle::try_demangle(name) else {
            continue;
        };

        let demangled = sym.to_string();
        match counter.entry(demangled) {
            Entry::Occupied(mut entry) => {
                func.name = Some(format!("{}[{}]", entry.key(), entry.get()));
                *entry.get_mut() += 1;
            }
            Entry::Vacant(entry) => {
                func.name = Some(entry.key().clone());
                entry.insert(1);
            }
        }
    }
}

impl OutputMode {
    fn uses_es_modules(&self) -> bool {
        matches!(
            self,
            OutputMode::Bundler { .. }
                | OutputMode::Web
                | OutputMode::Node { module: true }
                | OutputMode::Deno
                | OutputMode::Module
        )
    }

    fn nodejs(&self) -> bool {
        matches!(self, OutputMode::Node { .. })
    }

    fn no_modules(&self) -> bool {
        matches!(self, OutputMode::NoModules { .. })
    }

    fn bundler(&self) -> bool {
        matches!(self, OutputMode::Bundler { .. })
    }

    fn emscripten(&self) -> bool {
        matches!(self, OutputMode::Emscripten)
    }
}

/// Remove a number of internal exports that are synthesized by Rust's linker,
/// LLD. These exports aren't typically ever needed and just add extra space to
/// the binary.
fn unexported_unused_lld_things(module: &mut Module) {
    let mut to_remove = Vec::new();
    for export in module.exports.iter() {
        match export.name.as_str() {
            "__heap_base" | "__data_end" | "__indirect_function_table" => {
                to_remove.push(export.id());
            }
            _ => {}
        }
    }
    for id in to_remove {
        module.exports.delete(id);
    }
}

impl Output {
    pub fn js(&self) -> &str {
        &self.generated.js
    }

    pub fn ts(&self) -> Option<&str> {
        if self.generated.typescript {
            Some(&self.generated.ts)
        } else {
            None
        }
    }

    pub fn start(&self) -> Option<&String> {
        self.generated.start.as_ref()
    }

    pub fn snippets(&self) -> &BTreeMap<String, Vec<String>> {
        &self.generated.snippets
    }

    pub fn local_modules(&self) -> &HashMap<String, String> {
        &self.generated.local_modules
    }

    pub fn npm_dependencies(&self) -> &HashMap<String, (PathBuf, String)> {
        &self.generated.npm_dependencies
    }

    pub fn wasm(&self) -> &walrus::Module {
        &self.module
    }

    pub fn wasm_mut(&mut self) -> &mut walrus::Module {
        &mut self.module
    }

    pub fn emit(&mut self, out_dir: impl AsRef<Path>) -> Result<(), Error> {
        self._emit(out_dir.as_ref())
    }

    fn _emit(&mut self, out_dir: &Path) -> Result<(), Error> {
        let wasm_name = format!("{}_bg", self.stem);
        let wasm_path = out_dir.join(&wasm_name).with_extension("wasm");
        fs::create_dir_all(out_dir)?;

        let wasm_bytes = self.module.emit_wasm();
        fs::write(&wasm_path, wasm_bytes)
            .with_context(|| format!("failed to write `{}`", wasm_path.display()))?;

        let gen = &self.generated;

        // Write out all local JS snippets to the final destination now that
        // we've collected them from all the programs.
        for (identifier, list) in gen.snippets.iter() {
            for (i, js) in list.iter().enumerate() {
                let name = format!("inline{i}.js");
                let path = out_dir.join("snippets").join(identifier).join(name);
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(&path, js)
                    .with_context(|| format!("failed to write `{}`", path.display()))?;
            }
        }

        for (path, contents) in gen.local_modules.iter() {
            let path = out_dir.join("snippets").join(path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, contents)
                .with_context(|| format!("failed to write `{}`", path.display()))?;
        }

        let is_genmode_nodemodule = matches!(gen.mode, OutputMode::Node { module: true });
        if !gen.npm_dependencies.is_empty() || is_genmode_nodemodule {
            #[derive(serde::Serialize)]
            struct PackageJson<'a> {
                #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
                ty: Option<&'static str>,
                dependencies: BTreeMap<&'a str, &'a str>,
            }
            let pj = PackageJson {
                ty: is_genmode_nodemodule.then_some("module"),
                dependencies: gen
                    .npm_dependencies
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.1.as_str()))
                    .collect(),
            };
            let json = serde_json::to_string_pretty(&pj)?;
            fs::write(out_dir.join("package.json"), json)?;
        }

        // And now that we've got all our JS and TypeScript, actually write it
        // out to the filesystem.
        let extension = "js";

        fn write<P, C>(path: P, contents: C) -> Result<(), anyhow::Error>
        where
            P: AsRef<Path>,
            C: AsRef<[u8]>,
        {
            fs::write(&path, contents)
                .with_context(|| format!("failed to write `{}`", path.as_ref().display()))
        }

        let js_path = out_dir.join(&self.stem).with_extension(extension);
        if matches!(self.generated.mode, OutputMode::Emscripten) {
            let emscripten_js_path = out_dir.join("library_bindgen.js");
            write(&emscripten_js_path, reset_indentation(&gen.js))?;
            // When the user crate imports from an ESM module
            // (`#[wasm_bindgen(module = "...")]`), we emit those imports to a
            // sidecar `library_bindgen.extern-pre.js`. Consumers pass it to
            // emcc with `--extern-pre-js`, which prepends it before emcc's
            // modularize wrapper — ESM imports can only legally live there.
            // Skip writing when empty so consumers don't accidentally pick
            // up a stale file from a previous build.
            let extern_pre_js_path = out_dir.join("library_bindgen.extern-pre.js");
            if gen.emscripten_extern_pre_js.is_empty() {
                let _ = fs::remove_file(&extern_pre_js_path);
            } else {
                write(
                    &extern_pre_js_path,
                    reset_indentation(&gen.emscripten_extern_pre_js),
                )?;
            }
        } else {
            write(&js_path, reset_indentation(&gen.js))?;
        }

        if let Some(start) = &gen.start {
            let js_path = out_dir.join(wasm_name).with_extension(extension);
            write(&js_path, reset_indentation(start))?;
        }

        if gen.typescript {
            let ts_path = js_path.with_extension("d.ts");
            fs::write(&ts_path, reset_indentation(&gen.ts))
                .with_context(|| format!("failed to write `{}`", ts_path.display()))?;
        }

        if gen.typescript {
            let ts_path = wasm_path.with_extension("wasm.d.ts");
            let ts = wasm2es6js::typescript(&self.module)?;
            fs::write(&ts_path, reset_indentation(&ts))
                .with_context(|| format!("failed to write `{}`", ts_path.display()))?;
        }

        Ok(())
    }
}

/// Generate Wasm catch wrappers for imports marked with `#[wasm_bindgen(catch)]`.
///
/// When exception handling instructions are available in the module, this generates
/// Wasm wrapper functions that catch JavaScript exceptions using `WebAssembly.JSTag`
/// instead of relying on JS `handleError` wrappers.
fn generate_wasm_catch_wrappers(
    module: &mut Module,
    enable_abort_handler: bool,
) -> Result<(), Error> {
    let eh_version = transforms::detect_exception_handling_version(module, enable_abort_handler);
    log::debug!("Exception handling version: {eh_version:?}");

    if eh_version == transforms::ExceptionHandlingVersion::None {
        return Ok(());
    }

    // We need to temporarily remove the custom sections to avoid borrow issues
    let mut aux = module
        .customs
        .delete_typed::<wit::WasmBindgenAux>()
        .expect("aux section should exist");
    let wit = module
        .customs
        .delete_typed::<wit::NonstandardWitSection>()
        .expect("wit section should exist");

    log::debug!(
        "Running catch handler: imports_with_catch={}, externref_table={:?}, externref_alloc={:?}, exn_store={:?}",
        aux.imports_with_catch.len(),
        aux.externref_table,
        aux.externref_alloc,
        aux.exn_store
    );

    let result = transforms::catch_handler::run(module, &mut aux, &wit, eh_version)
        .context("failed to generate catch wrappers");

    // Re-add the custom sections
    module.customs.add(*wit);
    module.customs.add(*aux);

    result?;

    Ok(())
}

fn gc_module_and_adapters(module: &mut Module) {
    loop {
        // Fist up, cleanup the native Wasm module. Note that roots can come
        // from custom sections, namely our Wasm interface types custom section
        // as well as the aux section.
        walrus::passes::gc::run(module);

        // ... and afterwards we can delete any `implements` directives for any
        // imports that have been deleted.
        let imports_remaining = module
            .imports
            .iter()
            .map(|i| i.id())
            .collect::<HashSet<_>>();
        let mut section = module
            .customs
            .delete_typed::<wit::NonstandardWitSection>()
            .unwrap();
        section
            .implements
            .retain(|pair| imports_remaining.contains(&pair.0));

        // ... and after we delete the `implements` directive we try to
        // delete some adapters themselves. If nothing is deleted, then we're
        // good to go. If something is deleted though then we may have free'd up
        // some functions in the main module to get deleted, so go again to gc
        // things.
        let any_removed = section.gc();
        module.customs.add(*section);
        if !any_removed {
            break;
        }
    }
}

/// Returns a sorted iterator over a hash map, sorted based on key.
///
/// The intention of this API is to be used whenever the iteration order of a
/// `HashMap` might affect the generated JS bindings. We want to ensure that the
/// generated output is deterministic and we do so by ensuring that iteration of
/// hash maps is consistently sorted.
fn sorted_iter<K, V>(map: &HashMap<K, V>) -> impl Iterator<Item = (&K, &V)>
where
    K: Ord,
{
    let mut pairs = map.iter().collect::<Vec<_>>();
    pairs.sort_by_key(|(k, _)| *k);
    pairs.into_iter()
}

#[cfg(test)]
mod uniffi_surface_api_tests {
    use super::*;

    fn operation(id: u32, name: &str) -> UniFfiBackendOperation {
        UniFfiBackendOperation::new(
            id,
            name,
            UniFfiBackendAsyncKind::Sync,
            UniFfiBackendOperationKind::Function,
            false,
            vec![UniFfiBackendCarrier::Primitive],
            Some(UniFfiBackendCarrier::Primitive),
        )
        .unwrap()
    }

    fn surface() -> BindingSurface {
        BindingSurface::UniFfiBackend(
            UniFfiBackendConfig::new(
                "__uniffi_backend_factory",
                vec![operation(0, "__uniffi_op_0")],
                UniFfiBackendResourceExports::default(),
                ClosePolicy {
                    grace_ms: 5_000,
                    on_deadline: DeadlineAction::Detach,
                },
            )
            .unwrap(),
        )
    }

    #[test]
    fn backend_surface_is_orthogonal_to_loader_target() {
        for target in ["bundler", "web", "node"] {
            let mut bindgen = Bindgen::new();
            match target {
                "bundler" => {
                    bindgen.bundler(true).unwrap();
                }
                "web" => {
                    bindgen.web(true).unwrap();
                }
                "node" => {
                    bindgen.nodejs(true).unwrap();
                }
                _ => unreachable!(),
            }
            bindgen.binding_surface(surface()).typescript(true);
            assert!(matches!(
                bindgen.selected_binding_surface(),
                BindingSurface::UniFfiBackend(_)
            ));
            assert!(!bindgen.typescript_enabled());
        }
    }

    #[test]
    fn backend_operation_ids_are_dense_and_names_are_unique() {
        let sparse = UniFfiBackendConfig::new(
            "__uniffi_backend_factory",
            vec![operation(1, "__uniffi_op_1")],
            UniFfiBackendResourceExports::default(),
            ClosePolicy {
                grace_ms: 5_000,
                on_deadline: DeadlineAction::Detach,
            },
        );
        assert!(sparse.is_err());

        let duplicate = UniFfiBackendConfig::new(
            "__uniffi_backend_factory",
            vec![operation(0, "__uniffi_op"), operation(1, "__uniffi_op")],
            UniFfiBackendResourceExports::default(),
            ClosePolicy {
                grace_ms: 5_000,
                on_deadline: DeadlineAction::Detach,
            },
        );
        assert!(duplicate.is_err());
    }
}
