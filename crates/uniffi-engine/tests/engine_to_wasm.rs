use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use wasm_bindgen_macro_support::ExpansionContext;
use wasm_bindgen_uniffi_engine::{
    PostLinkTarget, RustPath, WasmArgumentBinding, WasmAsyncKind, WasmCallTarget,
    WasmCallbackContract, WasmCallbackReentrancy, WasmCallbackRetention, WasmCallbackThreading,
    WasmCallbackUseSite, WasmCarrier, WasmConversionRecipe, WasmEnginePlan, WasmEngineResourceHook,
    WasmEngineResourceHooks, WasmOperationKind, WasmOperationOwner, WasmOperationPlan,
    WasmOperationSourceKey, WasmOwnership, WasmReceiverBinding, WasmResourceHook, WasmResourceKind,
    WasmResourceUseSite, WasmReturnBinding, WasmRustCarrier, WasmRustType, WasmScalarType,
    WasmStreamContract, WasmStreamDirection, WasmStreamResourceGroup, WasmStreamUseSite,
    WasmValueBinding, WasmValuePath, WasmValuePathSegment, DEFAULT_BACKEND_FACTORY,
};

const FIXTURE_NAME: &str = "uniffi_wasm_engine_fixture";
const RESTRICTED_PATH: &str = "/usr/bin:/bin";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_owned()
}

fn tool_path(name: &str) -> PathBuf {
    let output = Command::new("/bin/sh")
        .arg("-lc")
        .arg(format!("command -v {name}"))
        .output()
        .unwrap();
    assert_command_success(&format!("locate {name}"), &output);
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn engine_plan() -> WasmEnginePlan {
    fn scalar_binding(name: &str, scalar: WasmScalarType, abi: WasmCarrier) -> WasmArgumentBinding {
        WasmArgumentBinding {
            public_name: name.to_owned(),
            rust_name: name.to_owned(),
            rust_type: WasmRustType::Scalar(scalar),
            carrier: match scalar {
                WasmScalarType::Bytes => WasmRustCarrier::Bytes,
                _ => WasmRustCarrier::Primitive,
            },
            abi_carrier: abi,
            ownership: WasmOwnership::Owned,
            conversion: match scalar {
                WasmScalarType::Bytes => WasmConversionRecipe::Bytes,
                _ => WasmConversionRecipe::Identity,
            },
        }
    }

    fn scalar_return(scalar: WasmScalarType, abi: WasmCarrier) -> WasmReturnBinding {
        let argument = scalar_binding("result", scalar, abi);
        WasmReturnBinding {
            rust_type: argument.rust_type,
            carrier: argument.carrier,
            abi_carrier: argument.abi_carrier,
            ownership: WasmOwnership::Owned,
            conversion: argument.conversion,
        }
    }

    fn js_return(carrier: WasmRustCarrier, conversion: WasmConversionRecipe) -> WasmReturnBinding {
        WasmReturnBinding {
            rust_type: WasmRustType::Path(
                RustPath::new(["wasm_bindgen".to_owned(), "JsValue".to_owned()]).unwrap(),
            ),
            carrier,
            abi_carrier: WasmCarrier::JsValue,
            ownership: WasmOwnership::Owned,
            conversion,
        }
    }

    fn js_argument(name: &str, conversion: WasmConversionRecipe) -> WasmArgumentBinding {
        WasmArgumentBinding {
            public_name: name.to_owned(),
            rust_name: name.to_owned(),
            rust_type: WasmRustType::Path(
                RustPath::new(["wasm_bindgen".to_owned(), "JsValue".to_owned()]).unwrap(),
            ),
            carrier: WasmRustCarrier::LocalAdapter,
            abi_carrier: WasmCarrier::JsValue,
            ownership: WasmOwnership::Borrowed,
            conversion,
        }
    }

    fn operation(
        operation_id: u32,
        name: &str,
        arguments: Vec<WasmArgumentBinding>,
        return_value: Option<WasmReturnBinding>,
        async_kind: WasmAsyncKind,
        throws: Option<u32>,
    ) -> WasmOperationPlan {
        WasmOperationPlan {
            operation_id,
            source_key: WasmOperationSourceKey {
                component: "fixture".to_owned(),
                owner: WasmOperationOwner::Namespace,
                kind: WasmOperationKind::Function,
                name: name.to_owned(),
            },
            component_id: 0,
            owner: WasmOperationOwner::Namespace,
            kind: WasmOperationKind::Function,
            callback_method_id: None,
            call_target: WasmCallTarget::FreeFunction {
                module: RustPath::new(["fixture".to_owned()]).unwrap(),
                item: name.to_owned(),
            },
            rust_call: RustPath::new(["fixture".to_owned(), name.to_owned()]).unwrap(),
            receiver: None,
            arguments,
            return_value,
            async_kind,
            throws,
            callback_use_sites: Vec::new(),
            resource_use_sites: Vec::new(),
            resource_hooks: Vec::new(),
            stream_resources: Vec::new(),
        }
    }

    fn nested_object_operation(
        operation_id: u32,
        name: &str,
        conversion: WasmConversionRecipe,
        selectors: Vec<WasmValuePathSegment>,
        async_kind: WasmAsyncKind,
    ) -> WasmOperationPlan {
        let mut operation = operation(
            operation_id,
            name,
            vec![js_argument("value", conversion.clone())],
            Some(js_return(WasmRustCarrier::LocalAdapter, conversion)),
            async_kind,
            None,
        );
        operation.resource_use_sites = vec![
            WasmResourceUseSite::object(
                WasmValuePath::new(
                    std::iter::once(WasmValuePathSegment::Argument(0))
                        .chain(selectors.clone())
                        .collect::<Vec<_>>(),
                ),
                42,
                WasmOwnership::Borrowed,
            ),
            WasmResourceUseSite::object(
                WasmValuePath::new(
                    std::iter::once(WasmValuePathSegment::Return)
                        .chain(selectors)
                        .collect::<Vec<_>>(),
                ),
                42,
                WasmOwnership::Owned,
            ),
        ];
        operation.resource_hooks = vec![WasmResourceHook::AcquireObject];
        operation
    }

    fn receiver(carrier: WasmRustCarrier, conversion: WasmConversionRecipe) -> WasmReceiverBinding {
        WasmReceiverBinding {
            rust_type: WasmRustType::Path(
                RustPath::new(["uniffi".to_owned(), "Handle".to_owned()]).unwrap(),
            ),
            carrier,
            abi_carrier: WasmCarrier::OpaqueHandle,
            ownership: WasmOwnership::Borrowed,
            conversion,
        }
    }

    fn stream_value() -> WasmValueBinding {
        WasmValueBinding {
            rust_type: WasmRustType::Scalar(WasmScalarType::U32),
            carrier: WasmRustCarrier::Primitive,
            abi_carrier: WasmCarrier::U32,
            conversion: WasmConversionRecipe::Identity,
        }
    }

    let string_arg = || scalar_binding("value", WasmScalarType::String, WasmCarrier::String);
    let mut operations = vec![operation(
        0,
        "a_roundtrip",
        vec![
            string_arg(),
            scalar_binding("bytes", WasmScalarType::Bytes, WasmCarrier::Bytes),
        ],
        Some(scalar_return(WasmScalarType::String, WasmCarrier::String)),
        WasmAsyncKind::Sync,
        None,
    )];
    // Keep a real raw path in the wasm32 fixture.
    operations[0].rust_call = RustPath::new([
        "crate".to_owned(),
        "r#type".to_owned(),
        "r#Trait".to_owned(),
    ])
    .unwrap();
    operations.extend([
        operation(
            1,
            "b_async_bytes",
            vec![string_arg()],
            Some(scalar_return(WasmScalarType::Bytes, WasmCarrier::Bytes)),
            WasmAsyncKind::Async,
            None,
        ),
        operation(
            2,
            "c_sync_fallible",
            vec![string_arg()],
            Some(scalar_return(WasmScalarType::String, WasmCarrier::String)),
            WasmAsyncKind::Sync,
            Some(0),
        ),
        operation(
            3,
            "d_async_fallible",
            vec![string_arg()],
            Some(scalar_return(WasmScalarType::String, WasmCarrier::String)),
            WasmAsyncKind::Async,
            Some(0),
        ),
    ]);

    let mut register_callback = operation(
        4,
        "e_register_callback",
        vec![scalar_binding(
            "callback_id",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Sync,
        None,
    );
    register_callback
        .callback_use_sites
        .push(WasmCallbackUseSite {
            operation_id: 4,
            callback_type_id: 7,
            path: WasmValuePath::new(vec![
                WasmValuePathSegment::Argument(0),
                WasmValuePathSegment::Optional,
            ]),
            contract: WasmCallbackContract {
                retention: WasmCallbackRetention::Retained,
                threading: WasmCallbackThreading::CallingThread,
                reentrancy: WasmCallbackReentrancy::Forbidden,
            },
        });
    operations.push(register_callback);

    for (id, name, method_id, async_kind, throws) in [
        (5, "f_callback_sync", 0, WasmAsyncKind::Sync, None),
        (
            6,
            "g_callback_sync_fallible",
            1,
            WasmAsyncKind::Sync,
            Some(0),
        ),
        (7, "h_callback_async", 2, WasmAsyncKind::Async, None),
        (
            8,
            "i_callback_async_fallible",
            3,
            WasmAsyncKind::Async,
            Some(0),
        ),
    ] {
        let mut callback = operation(
            id,
            name,
            vec![string_arg()],
            Some(scalar_return(WasmScalarType::String, WasmCarrier::String)),
            async_kind,
            throws,
        );
        callback.kind = WasmOperationKind::CallbackMethod;
        callback.source_key.kind = WasmOperationKind::CallbackMethod;
        callback.callback_method_id = Some(method_id);
        callback.call_target = WasmCallTarget::CallbackMethod {
            callback: RustPath::new(["fixture".to_owned(), "Observer".to_owned()]).unwrap(),
            callback_type_id: 7,
            method_id,
            item: name.to_owned(),
        };
        operations.push(callback);
    }

    let mut bidi = operation(
        9,
        "j_start_bidi",
        vec![scalar_binding(
            "input",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(WasmReturnBinding {
            rust_type: WasmRustType::Stream(Box::new(WasmRustType::Scalar(WasmScalarType::U32))),
            carrier: WasmRustCarrier::OutputStream,
            abi_carrier: WasmCarrier::OpaqueHandle,
            ownership: WasmOwnership::Owned,
            conversion: WasmConversionRecipe::OutputStream(Box::new(
                WasmConversionRecipe::Identity,
            )),
        }),
        WasmAsyncKind::Sync,
        None,
    );
    let standard = |direction| WasmStreamContract {
        direction,
        lazy_start: true,
        single_consumer: true,
        serial_pull: true,
        exactly_once_cleanup: true,
        explicit_cancel: true,
        eof_is_distinct_from_item: true,
    };
    bidi.stream_resources = vec![
        WasmStreamResourceGroup {
            use_site: WasmStreamUseSite {
                id: 0,
                operation_id: 9,
                path: WasmValuePath::argument(0),
                contract: standard(WasmStreamDirection::Input),
            },
            item: stream_value(),
            error: stream_value(),
            is_send: true,
            hooks: vec![
                WasmResourceHook::StartInputStream,
                WasmResourceHook::PullInputStream,
                WasmResourceHook::CancelInputStream,
                WasmResourceHook::CloseInputStream,
            ],
            slot_operation_ids: BTreeMap::from([
                (WasmOperationKind::InputStreamPull, 10),
                (WasmOperationKind::InputStreamCancel, 11),
            ]),
        },
        WasmStreamResourceGroup {
            use_site: WasmStreamUseSite {
                id: 1,
                operation_id: 9,
                path: WasmValuePath::return_value(),
                contract: standard(WasmStreamDirection::Output),
            },
            item: stream_value(),
            error: stream_value(),
            is_send: true,
            hooks: vec![
                WasmResourceHook::StartOutputStream,
                WasmResourceHook::PullOutputStream,
                WasmResourceHook::CancelOutputStream,
                WasmResourceHook::CloseOutputStream,
            ],
            slot_operation_ids: BTreeMap::from([
                (WasmOperationKind::OutputStreamStart, 9),
                (WasmOperationKind::OutputStreamNext, 12),
                (WasmOperationKind::OutputStreamCancel, 13),
            ]),
        },
    ];
    bidi.resource_hooks = bidi
        .stream_resources
        .iter()
        .flat_map(|group| group.hooks.iter().copied())
        .collect();
    operations.push(bidi);

    let mut input_pull = operation(
        10,
        "k_input_pull_unused",
        Vec::new(),
        Some(js_return(
            WasmRustCarrier::StreamStep,
            WasmConversionRecipe::StreamStep {
                item: Box::new(WasmConversionRecipe::Identity),
                error: Box::new(WasmConversionRecipe::Identity),
            },
        )),
        WasmAsyncKind::Async,
        None,
    );
    input_pull.kind = WasmOperationKind::InputStreamPull;
    input_pull.source_key.kind = input_pull.kind;
    input_pull.receiver = Some(receiver(
        WasmRustCarrier::InputStream,
        WasmConversionRecipe::Identity,
    ));
    input_pull.call_target = WasmCallTarget::StreamHook {
        parent_operation_id: 9,
        use_site_id: 0,
        hook: WasmResourceHook::PullInputStream,
    };
    input_pull.resource_hooks = vec![WasmResourceHook::PullInputStream];
    operations.push(input_pull);

    let mut input_cancel = operation(
        11,
        "l_input_cancel_unused",
        Vec::new(),
        None,
        WasmAsyncKind::Async,
        None,
    );
    input_cancel.kind = WasmOperationKind::InputStreamCancel;
    input_cancel.source_key.kind = input_cancel.kind;
    input_cancel.receiver = Some(receiver(
        WasmRustCarrier::InputStream,
        WasmConversionRecipe::Identity,
    ));
    input_cancel.call_target = WasmCallTarget::StreamHook {
        parent_operation_id: 9,
        use_site_id: 0,
        hook: WasmResourceHook::CancelInputStream,
    };
    input_cancel.resource_hooks = vec![WasmResourceHook::CancelInputStream];
    operations.push(input_cancel);

    let mut output_next = operation(
        12,
        "m_output_next",
        Vec::new(),
        Some(js_return(
            WasmRustCarrier::StreamStep,
            WasmConversionRecipe::StreamStep {
                item: Box::new(WasmConversionRecipe::Identity),
                error: Box::new(WasmConversionRecipe::Identity),
            },
        )),
        WasmAsyncKind::Async,
        None,
    );
    output_next.kind = WasmOperationKind::OutputStreamNext;
    output_next.source_key.kind = output_next.kind;
    output_next.receiver = Some(receiver(
        WasmRustCarrier::OutputStream,
        WasmConversionRecipe::Identity,
    ));
    output_next.call_target = WasmCallTarget::StreamHook {
        parent_operation_id: 9,
        use_site_id: 1,
        hook: WasmResourceHook::PullOutputStream,
    };
    output_next.resource_hooks = vec![WasmResourceHook::PullOutputStream];
    operations.push(output_next);

    let mut output_cancel = operation(
        13,
        "n_output_cancel",
        Vec::new(),
        None,
        WasmAsyncKind::Async,
        None,
    );
    output_cancel.kind = WasmOperationKind::OutputStreamCancel;
    output_cancel.source_key.kind = output_cancel.kind;
    output_cancel.receiver = Some(receiver(
        WasmRustCarrier::OutputStream,
        WasmConversionRecipe::Identity,
    ));
    output_cancel.call_target = WasmCallTarget::StreamHook {
        parent_operation_id: 9,
        use_site_id: 1,
        hook: WasmResourceHook::CancelOutputStream,
    };
    output_cancel.resource_hooks = vec![WasmResourceHook::CancelOutputStream];
    operations.push(output_cancel);

    let mut object = operation(
        14,
        "o_make_object",
        vec![scalar_binding(
            "handle",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(WasmReturnBinding {
            rust_type: WasmRustType::Path(
                RustPath::new(["fixture".to_owned(), "Object".to_owned()]).unwrap(),
            ),
            carrier: WasmRustCarrier::OpaqueHandle,
            abi_carrier: WasmCarrier::OpaqueHandle,
            ownership: WasmOwnership::Owned,
            conversion: WasmConversionRecipe::Object(42),
        }),
        WasmAsyncKind::Sync,
        None,
    );
    object.resource_use_sites.push(WasmResourceUseSite {
        path: WasmValuePath::return_value(),
        kind: WasmResourceKind::Object,
        type_id: 42,
        ownership: WasmOwnership::Owned,
    });
    object.resource_hooks = vec![WasmResourceHook::AcquireObject];
    operations.push(object);

    operations.push(operation(
        15,
        "p_resource_counts",
        Vec::new(),
        Some(scalar_return(WasmScalarType::String, WasmCarrier::String)),
        WasmAsyncKind::Sync,
        None,
    ));

    let mut late_object = operation(
        16,
        "q_late_object",
        vec![scalar_binding(
            "handle",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(WasmReturnBinding {
            rust_type: WasmRustType::Path(
                RustPath::new(["fixture".to_owned(), "Object".to_owned()]).unwrap(),
            ),
            carrier: WasmRustCarrier::OpaqueHandle,
            abi_carrier: WasmCarrier::OpaqueHandle,
            ownership: WasmOwnership::Owned,
            conversion: WasmConversionRecipe::Object(42),
        }),
        WasmAsyncKind::Async,
        None,
    );
    late_object.resource_use_sites.push(WasmResourceUseSite {
        path: WasmValuePath::return_value(),
        kind: WasmResourceKind::Object,
        type_id: 42,
        ownership: WasmOwnership::Owned,
    });
    late_object.resource_hooks = vec![WasmResourceHook::AcquireObject];
    operations.push(late_object);

    let mut failing_callback = operation(
        17,
        "r_register_callback_fallible",
        vec![scalar_binding(
            "callback_id",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Sync,
        Some(0),
    );
    failing_callback
        .callback_use_sites
        .push(WasmCallbackUseSite {
            operation_id: 17,
            callback_type_id: 7,
            path: WasmValuePath::argument(0),
            contract: WasmCallbackContract {
                retention: WasmCallbackRetention::Retained,
                threading: WasmCallbackThreading::CallingThread,
                reentrancy: WasmCallbackReentrancy::Allowed,
            },
        });
    operations.push(failing_callback);

    let mut async_callback = operation(
        18,
        "s_register_callback_async",
        vec![scalar_binding(
            "callback_id",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Async,
        None,
    );
    async_callback.callback_use_sites.push(WasmCallbackUseSite {
        operation_id: 18,
        callback_type_id: 7,
        path: WasmValuePath::argument(0),
        contract: WasmCallbackContract {
            retention: WasmCallbackRetention::Retained,
            threading: WasmCallbackThreading::MayCrossThread,
            reentrancy: WasmCallbackReentrancy::Allowed,
        },
    });
    operations.push(async_callback);

    let mut scoped_callback = operation(
        19,
        "t_scoped_callback",
        vec![scalar_binding(
            "callback_id",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Sync,
        None,
    );
    scoped_callback
        .callback_use_sites
        .push(WasmCallbackUseSite {
            operation_id: 19,
            callback_type_id: 7,
            path: WasmValuePath::argument(0),
            contract: WasmCallbackContract {
                retention: WasmCallbackRetention::Scoped,
                threading: WasmCallbackThreading::CallingThread,
                reentrancy: WasmCallbackReentrancy::Allowed,
            },
        });
    operations.push(scoped_callback);

    let mut input_only = operation(
        20,
        "u_consume_input",
        vec![scalar_binding(
            "input",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Async,
        None,
    );
    input_only.stream_resources.push(WasmStreamResourceGroup {
        use_site: WasmStreamUseSite {
            id: 2,
            operation_id: 20,
            path: WasmValuePath::argument(0),
            contract: standard(WasmStreamDirection::Input),
        },
        item: stream_value(),
        error: stream_value(),
        is_send: true,
        hooks: vec![
            WasmResourceHook::StartInputStream,
            WasmResourceHook::PullInputStream,
            WasmResourceHook::CancelInputStream,
            WasmResourceHook::CloseInputStream,
        ],
        slot_operation_ids: BTreeMap::from([
            (WasmOperationKind::InputStreamPull, 21),
            (WasmOperationKind::InputStreamCancel, 22),
        ]),
    });
    input_only.resource_hooks = input_only.stream_resources[0].hooks.clone();
    operations.push(input_only);

    for (id, name, kind, hook) in [
        (
            21,
            "v_input_pull_unused",
            WasmOperationKind::InputStreamPull,
            WasmResourceHook::PullInputStream,
        ),
        (
            22,
            "w_input_cancel_unused",
            WasmOperationKind::InputStreamCancel,
            WasmResourceHook::CancelInputStream,
        ),
    ] {
        let cancel = kind == WasmOperationKind::InputStreamCancel;
        let mut slot = operation(
            id,
            name,
            Vec::new(),
            (!cancel).then(|| {
                js_return(
                    WasmRustCarrier::StreamStep,
                    WasmConversionRecipe::StreamStep {
                        item: Box::new(WasmConversionRecipe::Identity),
                        error: Box::new(WasmConversionRecipe::Identity),
                    },
                )
            }),
            WasmAsyncKind::Async,
            None,
        );
        slot.kind = kind;
        slot.source_key.kind = kind;
        slot.receiver = Some(receiver(
            WasmRustCarrier::InputStream,
            WasmConversionRecipe::Identity,
        ));
        slot.call_target = WasmCallTarget::StreamHook {
            parent_operation_id: 20,
            use_site_id: 2,
            hook,
        };
        slot.resource_hooks = vec![hook];
        operations.push(slot);
    }

    let mut output_only = operation(
        23,
        "x_start_output",
        Vec::new(),
        Some(WasmReturnBinding {
            rust_type: WasmRustType::Stream(Box::new(WasmRustType::Scalar(WasmScalarType::U32))),
            carrier: WasmRustCarrier::OutputStream,
            abi_carrier: WasmCarrier::OpaqueHandle,
            ownership: WasmOwnership::Owned,
            conversion: WasmConversionRecipe::OutputStream(Box::new(
                WasmConversionRecipe::Identity,
            )),
        }),
        WasmAsyncKind::Sync,
        None,
    );
    output_only.stream_resources.push(WasmStreamResourceGroup {
        use_site: WasmStreamUseSite {
            id: 3,
            operation_id: 23,
            path: WasmValuePath::return_value(),
            contract: standard(WasmStreamDirection::Output),
        },
        item: stream_value(),
        error: stream_value(),
        is_send: true,
        hooks: vec![
            WasmResourceHook::StartOutputStream,
            WasmResourceHook::PullOutputStream,
            WasmResourceHook::CancelOutputStream,
            WasmResourceHook::CloseOutputStream,
        ],
        slot_operation_ids: BTreeMap::from([
            (WasmOperationKind::OutputStreamStart, 23),
            (WasmOperationKind::OutputStreamNext, 24),
            (WasmOperationKind::OutputStreamCancel, 25),
        ]),
    });
    output_only.resource_hooks = output_only.stream_resources[0].hooks.clone();
    operations.push(output_only);

    let mut output_only_next = operation(
        24,
        "y_output_next",
        Vec::new(),
        Some(js_return(
            WasmRustCarrier::StreamStep,
            WasmConversionRecipe::StreamStep {
                item: Box::new(WasmConversionRecipe::Identity),
                error: Box::new(WasmConversionRecipe::Identity),
            },
        )),
        WasmAsyncKind::Async,
        None,
    );
    output_only_next.kind = WasmOperationKind::OutputStreamNext;
    output_only_next.source_key.kind = output_only_next.kind;
    output_only_next.receiver = Some(receiver(
        WasmRustCarrier::OutputStream,
        WasmConversionRecipe::Identity,
    ));
    output_only_next.call_target = WasmCallTarget::StreamHook {
        parent_operation_id: 23,
        use_site_id: 3,
        hook: WasmResourceHook::PullOutputStream,
    };
    output_only_next.resource_hooks = vec![WasmResourceHook::PullOutputStream];
    operations.push(output_only_next);

    let mut output_only_cancel = operation(
        25,
        "z_output_cancel",
        Vec::new(),
        None,
        WasmAsyncKind::Async,
        None,
    );
    output_only_cancel.kind = WasmOperationKind::OutputStreamCancel;
    output_only_cancel.source_key.kind = output_only_cancel.kind;
    output_only_cancel.receiver = Some(receiver(
        WasmRustCarrier::OutputStream,
        WasmConversionRecipe::Identity,
    ));
    output_only_cancel.call_target = WasmCallTarget::StreamHook {
        parent_operation_id: 23,
        use_site_id: 3,
        hook: WasmResourceHook::CancelOutputStream,
    };
    output_only_cancel.resource_hooks = vec![WasmResourceHook::CancelOutputStream];
    operations.push(output_only_cancel);

    operations.push(nested_object_operation(
        26,
        "aa_nested_record",
        WasmConversionRecipe::Record(100),
        vec![WasmValuePathSegment::Field("object".to_owned())],
        WasmAsyncKind::Sync,
    ));
    operations.push(nested_object_operation(
        27,
        "ab_nested_optional",
        WasmConversionRecipe::Optional(Box::new(WasmConversionRecipe::Object(42))),
        vec![WasmValuePathSegment::Optional],
        WasmAsyncKind::Async,
    ));
    operations.push(nested_object_operation(
        28,
        "ac_nested_sequence",
        WasmConversionRecipe::Sequence(Box::new(WasmConversionRecipe::Object(42))),
        vec![WasmValuePathSegment::SequenceItem],
        WasmAsyncKind::Sync,
    ));
    operations.push(nested_object_operation(
        29,
        "ad_nested_map",
        WasmConversionRecipe::Map(
            Box::new(WasmConversionRecipe::Object(42)),
            Box::new(WasmConversionRecipe::Object(42)),
        ),
        vec![WasmValuePathSegment::MapKey],
        WasmAsyncKind::Sync,
    ));
    // Add the map-value use-site to the same operation so both sides of a
    // Map are lowered by the one backend walker.
    operations[29].resource_use_sites.insert(
        1,
        WasmResourceUseSite::object(
            WasmValuePath::new(vec![
                WasmValuePathSegment::Argument(0),
                WasmValuePathSegment::MapValue,
            ]),
            42,
            WasmOwnership::Borrowed,
        ),
    );
    operations[29]
        .resource_use_sites
        .push(WasmResourceUseSite::object(
            WasmValuePath::new(vec![
                WasmValuePathSegment::Return,
                WasmValuePathSegment::MapValue,
            ]),
            42,
            WasmOwnership::Owned,
        ));
    operations.push(nested_object_operation(
        30,
        "ae_nested_set",
        WasmConversionRecipe::Set(Box::new(WasmConversionRecipe::Object(42))),
        vec![WasmValuePathSegment::SetItem],
        WasmAsyncKind::Async,
    ));
    let mut object_method = operation(
        31,
        "af_object_method",
        vec![scalar_binding(
            "value",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Sync,
        None,
    );
    object_method.kind = WasmOperationKind::Method;
    object_method.source_key.kind = WasmOperationKind::Method;
    object_method.receiver = Some(receiver(
        WasmRustCarrier::OpaqueHandle,
        WasmConversionRecipe::Object(42),
    ));
    object_method.call_target = WasmCallTarget::Method {
        object: RustPath::new(["fixture".to_owned(), "Object".to_owned()]).unwrap(),
        object_kind: wasm_bindgen_uniffi_engine::WasmObjectKind::Struct,
        callback_method_id: None,
        item: "af_object_method".to_owned(),
    };
    object_method
        .resource_use_sites
        .push(WasmResourceUseSite::object(
            WasmValuePath::receiver(),
            42,
            WasmOwnership::Borrowed,
        ));
    operations.push(object_method);
    operations.push(nested_object_operation(
        32,
        "ag_nested_variant",
        WasmConversionRecipe::Enum(101),
        vec![
            WasmValuePathSegment::Variant("withObject".to_owned()),
            WasmValuePathSegment::Field("object".to_owned()),
        ],
        WasmAsyncKind::Sync,
    ));
    operations.push(nested_object_operation(
        33,
        "ah_late_nested_record",
        WasmConversionRecipe::Record(100),
        vec![WasmValuePathSegment::Field("object".to_owned())],
        WasmAsyncKind::Async,
    ));
    operations.push(operation(
        34,
        "ai_release_handle_count",
        vec![scalar_binding(
            "handle",
            WasmScalarType::U32,
            WasmCarrier::U32,
        )],
        Some(scalar_return(WasmScalarType::U32, WasmCarrier::U32)),
        WasmAsyncKind::Sync,
        None,
    ));

    WasmEnginePlan::build_with_resource_hooks(
        wasm_bindgen_uniffi_engine::ClosePolicy {
            // Real conformance uses a short policy so a deliberately stuck
            // callback/stream/cleanup fixture cannot hold the test process.
            grace_ms: 25,
            on_deadline: wasm_bindgen_uniffi_engine::DeadlineAction::Detach,
        },
        operations,
        WasmEngineResourceHooks {
            release_object: Some(WasmEngineResourceHook {
                rust_call: RustPath::new(["fixture".to_owned(), "release_object".to_owned()])
                    .unwrap(),
                async_kind: WasmAsyncKind::Async,
                fallible: false,
            }),
            close_output_stream: Some(WasmEngineResourceHook {
                rust_call: RustPath::new(["fixture".to_owned(), "close_output_stream".to_owned()])
                    .unwrap(),
                async_kind: WasmAsyncKind::Async,
                fallible: false,
            }),
        },
    )
    .unwrap()
}

fn fixture_source(plan: &WasmEnginePlan) -> String {
    let expansion_context = || {
        ExpansionContext::new(
            Path::new(env!("CARGO_MANIFEST_DIR")),
            FIXTURE_NAME,
            "1.0.0",
            Vec::<String>::new(),
            "wasm32-unknown-unknown",
        )
        .unwrap()
    };
    let adapters = plan
        .expand(expansion_context())
        .unwrap()
        .into_iter()
        .map(|operation| operation.tokens.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let resource_hooks = plan
        .expand_resource_hooks(expansion_context())
        .unwrap()
        .into_iter()
        .map(|hook| hook.tokens.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"
use wasm_bindgen::prelude::*;

mod fixture {{
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use js_sys::{{Array, Function, Object, Promise, Reflect}};
    use wasm_bindgen::{{JsCast, JsValue}};
    use wasm_bindgen_futures::JsFuture;

    #[derive(Default)]
    struct State {{
        release: u32,
        released_handles: BTreeMap<u32, u32>,
        cancel: u32,
        close: u32,
        pulls: BTreeMap<u32, u32>,
        stream_hosts: BTreeMap<u32, (JsValue, u32)>,
    }}
    thread_local! {{ static STATE: RefCell<State> = RefCell::new(State::default()); }}

    fn step(kind: &str, key: Option<&str>, value: Option<JsValue>) -> JsValue {{
        let object = Object::new();
        Reflect::set(&object, &JsValue::from_str("kind"), &JsValue::from_str(kind)).unwrap();
        if let (Some(key), Some(value)) = (key, value) {{
            Reflect::set(&object, &JsValue::from_str(key), &value).unwrap();
        }}
        object.into()
    }}

    fn host_method(host: &JsValue, name: &str) -> Function {{
        Reflect::get(host, &JsValue::from_str(name)).unwrap().dyn_into().unwrap()
    }}

    fn invoke_host_sync(host: &JsValue, callback_id: u32, value: &str) {{
        let args = Array::new();
        args.push(&JsValue::from_str(value));
        host_method(host, "invokeCallbackSync")
            .call4(
                host,
                &JsValue::from_f64(7.0),
                &JsValue::from_f64(callback_id as f64),
                &JsValue::from_f64(0.0),
                &args,
            )
            .unwrap();
    }}

    async fn invoke_host_async(host: &JsValue, callback_id: u32, value: &str) {{
        let args = Array::new();
        args.push(&JsValue::from_str(value));
        let promise: Promise = host_method(host, "invokeCallbackAsync")
            .call4(
                host,
                &JsValue::from_f64(7.0),
                &JsValue::from_f64(callback_id as f64),
                &JsValue::from_f64(2.0),
                &args,
            )
            .unwrap()
            .dyn_into()
            .unwrap();
        JsFuture::from(promise).await.unwrap();
    }}

    async fn wait_for_test_gate() {{
        let gate: Promise = Reflect::get(
            &js_sys::global(),
            &JsValue::from_str("__uniffi_test_gate"),
        )
        .unwrap()
        .dyn_into()
        .unwrap();
        JsFuture::from(gate).await.unwrap();
    }}

    async fn pull_host_input(host: &JsValue, input: u32) -> JsValue {{
        let promise: Promise = host_method(host, "pullInputStream")
            .call1(host, &JsValue::from_f64(input as f64))
            .unwrap()
            .dyn_into()
            .unwrap();
        JsFuture::from(promise).await.unwrap()
    }}

    pub fn a_roundtrip(text: String, bytes: Vec<u8>) -> String {{
        format!("{{text}}:{{}}", bytes.iter().copied().map(u32::from).sum::<u32>())
    }}

    pub async fn b_async_bytes(text: String) -> Vec<u8> {{
        text.into_bytes()
    }}

    pub fn c_sync_fallible(text: String) -> Result<String, JsValue> {{
        if text == "reject" {{
            Err(JsValue::from_str("sync declared error"))
        }} else {{
            Ok(format!("sync-fallible:{{text}}"))
        }}
    }}

    pub async fn d_async_fallible(text: String) -> Result<String, JsValue> {{
        if text == "reject" {{
            Err(JsValue::from_str("async declared error"))
        }} else {{
            Ok(format!("async-fallible:{{text}}"))
        }}
    }}

    pub fn e_register_callback(host: JsValue, callback_id: u32) -> u32 {{
        invoke_host_sync(&host, callback_id, "from-rust");
        callback_id
    }}
    pub fn f_callback_sync(value: String) -> String {{ value }}
    pub fn g_callback_sync_fallible(value: String) -> Result<String, JsValue> {{ Ok(value) }}
    pub async fn h_callback_async(value: String) -> String {{ value }}
    pub async fn i_callback_async_fallible(value: String) -> Result<String, JsValue> {{ Ok(value) }}

    pub fn j_start_bidi(host: JsValue, input: u32) -> u32 {{
        let handle = 1000 + input;
        STATE.with(|state| {{
            let mut state = state.borrow_mut();
            state.pulls.insert(handle, 0);
            state.stream_hosts.insert(handle, (host, input));
        }});
        handle
    }}

    pub async fn k_input_pull_unused(_handle: u32) -> JsValue {{ step("done", None, None) }}
    pub async fn l_input_cancel_unused(_handle: u32) {{}}

    pub async fn m_output_next(handle: u32) -> JsValue {{
        if handle == 1091 {{
            std::future::pending::<JsValue>().await;
        }}
        if handle == 1064 {{
            return step("error", Some("error"), Some(JsValue::from_str("stream failure")));
        }}
        let (pull, host_input) = STATE.with(|state| {{
            let mut state = state.borrow_mut();
            let pull = state.pulls.entry(handle).or_default();
            *pull += 1;
            (*pull, state.stream_hosts.get(&handle).cloned())
        }});
        if pull == 1 {{
            if let Some((host, input)) = host_input {{
                pull_host_input(&host, input).await;
            }}
            step("item", Some("value"), Some(JsValue::from_f64(handle as f64)))
        }} else {{
            step("done", None, None)
        }}
    }}

    pub async fn n_output_cancel(handle: u32) {{
        if handle == 1091 || handle == 1092 {{
            std::future::pending::<()>().await;
        }}
        STATE.with(|state| state.borrow_mut().cancel += 1);
    }}

    pub fn o_make_object(handle: u32) -> u32 {{ handle }}
    pub fn p_resource_counts() -> String {{
        STATE.with(|state| {{
            let state = state.borrow();
            format!("{{}},{{}},{{}}", state.release, state.cancel, state.close)
        }})
    }}
    pub async fn q_late_object(handle: u32) -> u32 {{ handle }}
    pub fn aa_nested_record(value: JsValue) -> JsValue {{ value }}
    pub async fn ab_nested_optional(value: JsValue) -> JsValue {{ value }}
    pub fn ac_nested_sequence(value: JsValue) -> JsValue {{ value }}
    pub fn ad_nested_map(value: JsValue) -> JsValue {{ value }}
    pub async fn ae_nested_set(value: JsValue) -> JsValue {{ value }}
    pub fn af_object_method(receiver: u32, value: u32) -> u32 {{ receiver + value }}
    pub fn ag_nested_variant(value: JsValue) -> JsValue {{ value }}
    pub async fn ah_late_nested_record(_value: JsValue) -> JsValue {{
        wait_for_test_gate().await;
        let object = Object::new();
        Reflect::set(&object, &JsValue::from_str("object"), &JsValue::from_f64(900.0)).unwrap();
        object.into()
    }}
    pub fn r_register_callback_fallible(_host: JsValue, callback_id: u32) -> Result<u32, JsValue> {{
        Err(JsValue::from_str(&format!("reject callback {{callback_id}}")))
    }}
    pub async fn s_register_callback_async(host: JsValue, callback_id: u32) -> u32 {{
        if callback_id == 44 {{
            wait_for_test_gate().await;
        }}
        invoke_host_async(&host, callback_id, "from-rust-async").await;
        callback_id
    }}
    pub fn t_scoped_callback(host: JsValue, callback_id: u32) -> u32 {{
        invoke_host_sync(&host, callback_id, "scoped-from-rust");
        callback_id
    }}
    pub async fn u_consume_input(host: JsValue, input: u32) -> u32 {{
        if input == 94 {{
            wait_for_test_gate().await;
        }}
        pull_host_input(&host, input).await;
        pull_host_input(&host, input).await;
        input
    }}
    pub async fn v_input_pull_unused(_handle: u32) -> JsValue {{ step("done", None, None) }}
    pub async fn w_input_cancel_unused(_handle: u32) {{}}
    pub fn x_start_output() -> u32 {{
        STATE.with(|state| state.borrow_mut().pulls.insert(2080, 0));
        2080
    }}
    pub async fn y_output_next(handle: u32) -> JsValue {{ m_output_next(handle).await }}
    pub async fn z_output_cancel(handle: u32) {{ n_output_cancel(handle).await }}

    pub async fn release_object(handle: u32) {{
        if handle == 703 {{
            std::future::pending::<()>().await;
        }}
        STATE.with(|state| {{
            let mut state = state.borrow_mut();
            state.release += 1;
            *state.released_handles.entry(handle).or_default() += 1;
        }});
    }}

    pub fn ai_release_handle_count(handle: u32) -> u32 {{
        STATE.with(|state| state.borrow().released_handles.get(&handle).copied().unwrap_or(0))
    }}

    pub async fn close_output_stream(handle: u32) {{
        if handle == 1093 {{
            std::future::pending::<()>().await;
        }}
        STATE.with(|state| state.borrow_mut().close += 1);
    }}
}}

mod r#type {{
    pub fn r#Trait(text: String, bytes: Vec<u8>) -> String {{
        format!("{{text}}:{{}}", bytes.iter().copied().map(u32::from).sum::<u32>())
    }}
}}

{adapters}
{resource_hooks}
"#
    )
}

fn build_fixture(temp: &TempDir, plan: &WasmEnginePlan, cargo: &Path, rustc: &Path) -> PathBuf {
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

    let output = Command::new(cargo)
        .current_dir(&project)
        .arg("build")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("RUSTC", rustc)
        .env("PATH", RESTRICTED_PATH)
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
    ...Array.from({{ length: 35 }}, (_, index) => `__uniffi_operation_${{index}}`),
    '__uniffi_release_object',
    '__uniffi_close_output_stream',
]) {{
    assert.equal(Object.hasOwn(api, raw), false);
}}
assert.equal(typeof api.{DEFAULT_BACKEND_FACTORY}, 'function');
const uniffiExports = Object.keys(api).filter((name) => name.includes('uniffi'));
assert.deepEqual(uniffiExports, ['{DEFAULT_BACKEND_FACTORY}']);

assert.throws(() => api.{DEFAULT_BACKEND_FACTORY}(), /Host object/);

const retained = [];
const released = [];
const callbackInvocations = [];
const inputPulls = new Map();
const inputCancelled = [];
const inputReleased = [];
let session;
let reentrantSession;
const host = {{
    invokeCallbackSync(typeId, callbackId, methodId, args) {{
        assert.equal(typeId, 7);
        assert.ok(callbackId === 41 || callbackId === 42 || callbackId === 43 || callbackId === 44);
        callbackInvocations.push(['sync', methodId, ...args]);
        if (methodId === 0 && args[0] === 'reenter') return session.invokeSync(5, [callbackId, 'nested']);
        if (methodId === 0 && args[0] === 'close') {{ reentrantSession.close(); return 'sync-0:close'; }}
        if (methodId === 0 && args[0] === 'thenable') return Promise.resolve('invalid');
        if (methodId === 0 && args[0] === 'throw-infallible') throw new Error('host failure');
        if (methodId === 1 && args[0] === 'reject') throw new Error('sync callback declared error');
        return `sync-${{methodId}}:${{args[0]}}`;
    }},
    async invokeCallbackAsync(typeId, callbackId, methodId, invocationId, args) {{
        assert.equal(typeId, 7);
        assert.ok(callbackId === 41 || callbackId === 42 || callbackId === 43 || callbackId === 44);
        callbackInvocations.push(['async', methodId, invocationId, ...args]);
        if (methodId === 2 && args[0] === 'deferred') await new Promise((resolve) => setTimeout(resolve, 5));
        if (methodId === 2 && args[0] === 'never') return new Promise(() => {{}});
        if (methodId === 2 && args[0] === 'throw-infallible') throw new Error('async host failure');
        if (methodId === 3 && args[0] === 'reject') throw new Error('async callback declared error');
        return `async-${{methodId}}:${{args[0]}}`;
    }},
    retainCallback(typeId, callbackId) {{ retained.push([typeId, callbackId]); }},
    releaseCallback(typeId, callbackId) {{ released.push([typeId, callbackId]); }},
    async pullInputStream(streamId) {{
        const count = (inputPulls.get(streamId) ?? 0) + 1;
        inputPulls.set(streamId, count);
        if (streamId === 91) return new Promise(() => {{}});
        if (streamId === 57) return {{ kind: 'error', error: 'input failure' }};
        return count === 1 ? {{ kind: 'item', value: streamId }} : {{ kind: 'done' }};
    }},
    async cancelInputStream(streamId) {{
        inputCancelled.push(streamId);
        if (streamId === 92) return new Promise(() => {{}});
        if (streamId === 94) await new Promise((resolve) => setTimeout(resolve, 8));
    }},
    releaseInputStream(streamId) {{ inputReleased.push(streamId); }},
}};

session = api.{DEFAULT_BACKEND_FACTORY}(host);
assert.deepEqual(Object.keys(session).sort(), ['cancelOutputStream', 'close', 'info', 'invokeAsync', 'invokeSync', 'releaseObject', 'releaseOutputStream'].sort());
assert.equal(session.info.operationCount, 35);
assert.equal(session.invokeSync(0, ['sum', new Uint8Array([1, 2, 3, 4])]), 'sum:10');
const pending = session.invokeAsync(1, ['bytes']);
assert.equal(typeof pending.then, 'function');
assert.deepEqual(Array.from(await pending), [98, 121, 116, 101, 115]);
assert.equal(session.invokeSync(2, ['value']), 'sync-fallible:value');
assert.throws(() => session.invokeSync(2, ['reject']));
assert.equal(await session.invokeAsync(3, ['value']), 'async-fallible:value');
await assert.rejects(session.invokeAsync(3, ['reject']));

assert.equal(session.invokeSync(4, [41]), 41);
assert.deepEqual(retained, [[7, 41]]);
assert.deepEqual(callbackInvocations[0], ['sync', 0, 'from-rust']);
assert.throws(() => session.invokeSync(17, [99]), /reject callback/);
assert.deepEqual(retained, [[7, 41], [7, 99]]);
assert.deepEqual(released, [[7, 99]]);
assert.deepEqual(await Promise.all([session.invokeAsync(18, [42]), session.invokeAsync(18, [42])]), [42, 42]);
assert.deepEqual(retained, [[7, 41], [7, 99], [7, 42]]);
assert.deepEqual(callbackInvocations.find((entry) => entry[0] === 'async'), ['async', 2, 1, 'from-rust-async']);
assert.equal(session.invokeSync(19, [43]), 43);
assert.throws(() => session.invokeSync(5, [43, 'after-scope']), /unknown UniFFI callback/);
assert.equal(session.invokeSync(5, [41, 'value']), 'sync-0:value');
assert.equal(session.invokeSync(6, [41, 'value']), 'sync-1:value');
assert.throws(() => session.invokeSync(6, [41, 'reject']), /declared error/);
assert.throws(() => session.invokeSync(5, [41, 'thenable']), /thenable/);
assert.throws(() => session.invokeSync(5, [41, 'throw-infallible']), /infallible UniFFI callback failed/);
assert.throws(() => session.invokeSync(5, [41, 'reenter']), /reentrancy/);
assert.equal(await session.invokeAsync(7, [41, 'value']), 'async-2:value');
await assert.rejects(session.invokeAsync(7, [41, 'throw-infallible']), /infallible UniFFI callback failed/);
assert.equal(await session.invokeAsync(8, [41, 'value']), 'async-3:value');
await assert.rejects(session.invokeAsync(8, [41, 'reject']), /declared error/);
assert.deepEqual(callbackInvocations.filter((entry) => entry[0] === 'async').map((entry) => entry[2]), [1, 2, 3, 4, 5, 6]);

assert.deepEqual(await session.invokeAsync(10, [55]), {{ kind: 'item', value: 55 }});
assert.deepEqual(await session.invokeAsync(10, [55]), {{ kind: 'done' }});
await session.invokeAsync(11, [56]);
assert.deepEqual(await session.invokeAsync(10, [57]), {{ kind: 'error', error: 'input failure' }});
assert.equal(await session.invokeAsync(20, [70]), 70);
assert.equal(inputPulls.get(70), 2);

const directOutput = session.invokeSync(23, []);
assert.deepEqual(await session.invokeAsync(24, [directOutput]), {{ kind: 'item', value: 2080 }});
assert.deepEqual(await session.invokeAsync(24, [directOutput]), {{ kind: 'done' }});

const natural = session.invokeSync(9, [60]);
assert.deepEqual(await session.invokeAsync(12, [natural]), {{ kind: 'item', value: 1060 }});
assert.equal(inputPulls.get(60), 1);
assert.deepEqual(await session.invokeAsync(12, [natural]), {{ kind: 'done' }});
await assert.rejects(session.invokeAsync(12, [natural]), /released/);

const cancelled = session.invokeSync(9, [61]);
await Promise.all([session.cancelOutputStream(cancelled), session.cancelOutputStream(cancelled)]);

const releasedOutput = session.invokeSync(9, [62]);
session.releaseOutputStream(releasedOutput);
session.releaseOutputStream(releasedOutput);
await Promise.resolve();

const failed = session.invokeSync(9, [64]);
assert.deepEqual(await session.invokeAsync(12, [failed]), {{ kind: 'error', error: 'stream failure' }});

const object = session.invokeSync(14, [700]);
assert.equal(object.handle, 700);
session.releaseObject(object);
session.releaseObject(object);
await Promise.resolve();

const closeOutput = session.invokeSync(9, [63]);
const closeObject = session.invokeSync(14, [701]);
assert.equal(closeObject.handle, 701);
const pendingCallback = session.invokeAsync(7, [41, 'deferred']);
const pendingNext = session.invokeAsync(12, [closeOutput]);
const lateObject = session.invokeAsync(16, [702]);
const closePromise = session.close();
assert.equal(session.close(), closePromise);
await assert.rejects(lateObject, /closed/);
assert.equal(await pendingCallback, 'async-2:deferred');
await assert.rejects(pendingNext, /stale|closed|released/);
await closePromise;
assert.throws(() => session.invokeSync(15, []), /closed/);
assert.deepEqual(released, [[7, 99], [7, 41], [7, 42]]);
assert.deepEqual(inputReleased.sort((a, b) => a - b), [55, 56, 57, 60, 61, 62, 63, 64, 70]);
assert.deepEqual(inputCancelled.sort((a, b) => a - b), [56, 61, 63]);

const inspector = api.{DEFAULT_BACKEND_FACTORY}(host);
assert.equal(inspector.invokeSync(15, []), '3,2,6');
assert.equal(inspector.invokeSync(4, [41]), 41);
assert.equal(await inspector.invokeAsync(7, [41, 'next-session']), 'async-2:next-session');
assert.equal(callbackInvocations.at(-1)[2], 1);
await inspector.close();

// Nested object resource paths use the same backend walker for records,
// optional values, sequences, maps (both keys and values), and sets.
const nestedSession = api.{DEFAULT_BACKEND_FACTORY}(host);
const recordLease = nestedSession.invokeSync(14, [710]);
const recordResult = nestedSession.invokeSync(26, [{{ object: recordLease }}]);
assert.equal(recordResult.object.handle, 710);
nestedSession.releaseObject(recordResult.object);
nestedSession.releaseObject(recordLease);
assert.equal(await nestedSession.invokeAsync(27, [null]), null);
const optionalLease = nestedSession.invokeSync(14, [711]);
const optionalResult = await nestedSession.invokeAsync(27, [optionalLease]);
assert.equal(optionalResult.handle, 711);
nestedSession.releaseObject(optionalResult);
nestedSession.releaseObject(optionalLease);
const sequenceLease = nestedSession.invokeSync(14, [712]);
const sequenceResult = nestedSession.invokeSync(28, [[sequenceLease]]);
assert.equal(sequenceResult[0].handle, 712);
nestedSession.releaseObject(sequenceResult[0]);
nestedSession.releaseObject(sequenceLease);
const mapKey = nestedSession.invokeSync(14, [713]);
const mapValue = nestedSession.invokeSync(14, [714]);
const map = new Map([[mapKey, mapValue]]);
const mapResult = nestedSession.invokeSync(29, [map]);
assert.equal(Array.from(mapResult.keys())[0].handle, 713);
assert.equal(Array.from(mapResult.values())[0].handle, 714);
nestedSession.releaseObject(Array.from(mapResult.keys())[0]);
nestedSession.releaseObject(Array.from(mapResult.values())[0]);
nestedSession.releaseObject(mapKey);
nestedSession.releaseObject(mapValue);
const setLease = nestedSession.invokeSync(14, [715]);
const setResult = await nestedSession.invokeAsync(30, [new Set([setLease])]);
assert.equal(Array.from(setResult)[0].handle, 715);
nestedSession.releaseObject(Array.from(setResult)[0]);
nestedSession.releaseObject(setLease);
const receiverLease = nestedSession.invokeSync(14, [716]);
assert.equal(nestedSession.invokeSync(31, [receiverLease, 4]), 720);
nestedSession.releaseObject(receiverLease);
const alternateVariantLease = nestedSession.invokeSync(14, [717]);
const alternateVariant = nestedSession.invokeSync(32, [{{ tag: 'other', object: alternateVariantLease }}]);
assert.equal(alternateVariant.object, alternateVariantLease);
assert.throws(() => nestedSession.invokeSync(32, [{{ object: alternateVariantLease }}]), /discriminant/);
nestedSession.releaseObject(alternateVariantLease);
const matchingVariantLease = nestedSession.invokeSync(14, [718]);
const matchingVariant = nestedSession.invokeSync(32, [{{ tag: 'withObject', object: matchingVariantLease }}]);
assert.equal(matchingVariant.object.handle, 718);
nestedSession.releaseObject(matchingVariant.object);
nestedSession.releaseObject(matchingVariantLease);
await nestedSession.close();
let releaseNestedGate;
globalThis.__uniffi_test_gate = new Promise((resolve) => {{ releaseNestedGate = resolve; }});
const lateNestedSession = api.{DEFAULT_BACKEND_FACTORY}(host);
const lateNestedArgument = lateNestedSession.invokeSync(14, [719]);
const lateNested = lateNestedSession.invokeAsync(33, [{{ object: lateNestedArgument }}]);
const lateNestedClose = lateNestedSession.close();
setTimeout(() => releaseNestedGate(), 5);
await assert.rejects(lateNested, /closed/);
await lateNestedClose;
delete globalThis.__uniffi_test_gate;
const releaseInspector = api.{DEFAULT_BACKEND_FACTORY}(host);
assert.equal(releaseInspector.invokeSync(34, [900]), 1);
assert.equal(releaseInspector.invokeSync(34, [719]), 1);
await releaseInspector.close();

// Reentrant close from a synchronous callback keeps the in-flight invocation
// on its original generation; only the next call is rejected.
reentrantSession = api.{DEFAULT_BACKEND_FACTORY}(host);
assert.equal(reentrantSession.invokeSync(4, [41]), 41);
assert.equal(reentrantSession.invokeSync(5, [41, 'close']), 'sync-0:close');
await reentrantSession.close();

// A raw async operation may suspend before it first calls Host.  Its captured
// generation-scoped proxy remains usable during closing, so the operation can
// call callback and input hooks after close starts but before the deadline.
let releaseGate;
globalThis.__uniffi_test_gate = new Promise((resolve) => {{ releaseGate = resolve; }});
const delayedSession = api.{DEFAULT_BACKEND_FACTORY}(host);
assert.equal(delayedSession.invokeSync(4, [44]), 44);
const delayedCallback = delayedSession.invokeAsync(18, [44]);
const delayedInput = delayedSession.invokeAsync(20, [94]);
const delayedClose = delayedSession.close();
setTimeout(() => releaseGate(), 5);
assert.equal(await delayedCallback, 44);
assert.equal(await delayedInput, 94);
await delayedClose;
delete globalThis.__uniffi_test_gate;

// Natural close must clear its deadline timer rather than merely completing
// after the deadline.  Intercept the host timers for one fresh session so the
// behavior is directly observable without relying on wall-clock timing.
const savedSetTimeout = globalThis.setTimeout;
const savedClearTimeout = globalThis.clearTimeout;
let createdTimers = 0;
let clearedTimers = 0;
globalThis.setTimeout = (callback, delay) => {{
    createdTimers += 1;
    return savedSetTimeout(callback, delay);
}};
globalThis.clearTimeout = (timer) => {{
    clearedTimers += 1;
    return savedClearTimeout(timer);
}};
const naturalSession = api.{DEFAULT_BACKEND_FACTORY}(host);
await naturalSession.close();
globalThis.setTimeout = savedSetTimeout;
globalThis.clearTimeout = savedClearTimeout;
assert.equal(createdTimers, 1);
assert.equal(clearedTimers, 1);

// A short-policy session must detach when callback, input, output, and
// backend cleanup promises never settle.  The late callback promise is kept
// intentionally unresolved; close must still resolve and no later callback or
// release may enter the Host.
const deadlineSession = api.{DEFAULT_BACKEND_FACTORY}(host);
assert.equal(deadlineSession.invokeSync(4, [41]), 41);
const stuckCallback = deadlineSession.invokeAsync(7, [41, 'never']);
const stuckInput = deadlineSession.invokeAsync(20, [91]);
const stuckOutputNext = deadlineSession.invokeSync(9, [91]);
const stuckNext = deadlineSession.invokeAsync(12, [stuckOutputNext]);
const stuckCancelOutput = deadlineSession.invokeSync(9, [92]);
const stuckCancel = deadlineSession.cancelOutputStream(stuckCancelOutput);
const stuckCleanupOutput = deadlineSession.invokeSync(9, [93]);
deadlineSession.releaseOutputStream(stuckCleanupOutput);
const stuckObject = deadlineSession.invokeAsync(16, [703]);
const deadlineStart = Date.now();
const deadlinePromise = deadlineSession.close();
assert.equal(deadlineSession.close(), deadlinePromise);
await deadlinePromise;
assert.ok(Date.now() - deadlineStart < 500);
assert.throws(() => deadlineSession.invokeSync(15, []), /closed/);
const remainsPending = async (promise) => Promise.race([
    promise.then(() => false, () => false),
    new Promise((resolve) => setTimeout(() => resolve(true), 60)),
]);
assert.equal(await remainsPending(stuckCallback), true);
assert.equal(await remainsPending(stuckInput), true);
assert.equal(await remainsPending(stuckNext), true);
assert.equal(await remainsPending(stuckCancel), true);
assert.equal(await remainsPending(stuckObject), true);
void closeOutput;
"#
    )
}

fn generate_and_run_target(
    temp: &TempDir,
    plan: &WasmEnginePlan,
    wasm: &Path,
    target: PostLinkTarget,
    node: &Path,
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
    let mut command = Command::new(node);
    if target == PostLinkTarget::Bundler {
        command.arg("--experimental-wasm-modules");
    }
    let result = command
        .arg("run.mjs")
        .current_dir(&output_dir)
        .env("PATH", RESTRICTED_PATH)
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
    let sync_infallible = tokens(0);
    assert!(sync_infallible.contains("pub fn"));
    assert!(!sync_infallible.contains("pub async fn"));
    assert!(sync_infallible.contains("-> String {"));
    assert!(!sync_infallible.contains("-> Result < String"));
    let sync_fallible = tokens(2);
    assert!(sync_fallible.contains("pub fn"));
    assert!(sync_fallible.contains("-> Result < String"));
    let async_fallible = tokens(3);
    assert!(async_fallible.contains("pub async fn"));
    assert!(async_fallible.contains("-> Result < String"));
    for host_operation in [5, 6, 7, 8, 10, 11] {
        assert!(
            expanded
                .iter()
                .all(|operation| operation.operation_id != host_operation),
            "host-dispatched operation {host_operation} must not expand a Rust raw shim"
        );
    }

    let cargo = tool_path("cargo");
    let rustc = tool_path("rustc");
    let node = tool_path("node");
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", RESTRICTED_PATH);
    let cli_probe = Command::new("/bin/sh")
        .arg("-c")
        .arg("command -v wasm-bindgen")
        .output()
        .unwrap();
    assert!(
        !cli_probe.status.success(),
        "external wasm-bindgen CLI must be invisible"
    );

    let wasm = build_fixture(&temp, &plan, &cargo, &rustc);

    for target in [
        PostLinkTarget::Web,
        PostLinkTarget::Bundler,
        PostLinkTarget::Node,
    ] {
        generate_and_run_target(&temp, &plan, &wasm, target, &node);
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
}
