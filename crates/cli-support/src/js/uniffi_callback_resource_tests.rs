use super::{Context, ExportDefinition, ExportEntry, NonstandardWitSection, WasmBindgenAux};
use crate::{
    Bindgen, BindingSurface, ClosePolicy, DeadlineAction, UniFfiBackendAsyncKind,
    UniFfiBackendCarrier, UniFfiBackendConfig, UniFfiBackendDispatch, UniFfiBackendOperation,
    UniFfiBackendOperationKind, UniFfiBackendResourceExports, UniFfiBackendResourceOwnership,
    UniFfiBackendResourceUseSite, UniFfiBackendValuePath, UniFfiBackendValuePathSegment,
};
use std::io::Write;
use std::process::{Command, Stdio};
use walrus::Module;

#[test]
fn callback_argument_resource_leases_cover_all_host_branches() {
    use UniFfiBackendAsyncKind::{Async, Sync};
    use UniFfiBackendCarrier::{LocalAdapter, OpaqueHandle, Primitive};
    use UniFfiBackendOperationKind::{CallbackMethod, Function};
    use UniFfiBackendResourceOwnership::{Borrowed, Owned};
    use UniFfiBackendValuePathSegment::{
        Argument, Field, MapKey, MapValue, Optional, Return, SequenceItem, SetItem, Variant,
    };

    let object = |path: Vec<UniFfiBackendValuePathSegment>, type_id, ownership| {
        UniFfiBackendResourceUseSite::object(UniFfiBackendValuePath::new(path), type_id, ownership)
    };
    let mut operations = vec![
        UniFfiBackendOperation::new(
            0,
            "captureHost",
            Sync,
            Function,
            false,
            vec![],
            Some(LocalAdapter),
        )
        .unwrap()
        .with_host_argument(true),
        UniFfiBackendOperation::new(
            1,
            "readObject",
            Sync,
            Function,
            false,
            vec![OpaqueHandle],
            Some(Primitive),
        )
        .unwrap()
        .with_resource_use_sites(vec![object(vec![Argument(0)], 12, Borrowed)]),
    ];
    for (id, name, type_id) in [(2, "makeObject", 12), (3, "makeOtherObject", 13)] {
        operations.push(
            UniFfiBackendOperation::new(
                id,
                name,
                Sync,
                Function,
                false,
                vec![Primitive],
                Some(OpaqueHandle),
            )
            .unwrap()
            .with_resource_use_sites(vec![object(vec![Return], type_id, Owned)]),
        );
    }
    for (method_id, async_kind, fallible) in [
        (0, Sync, false),
        (1, Sync, true),
        (2, Async, false),
        (3, Async, true),
    ] {
        let mut resources = vec![object(vec![Argument(2)], 12, Borrowed)];
        for selectors in [
            vec![Field("optional".into()), Optional],
            vec![Field("sequence".into()), SequenceItem],
            vec![Field("set".into()), SetItem],
            vec![Field("map".into()), MapKey],
            vec![Field("map".into()), MapValue],
            vec![
                Field("variant".into()),
                Variant("resource".into()),
                Field("object".into()),
            ],
        ] {
            let mut path = vec![Argument(3)];
            path.extend(selectors);
            resources.push(object(path, 12, Borrowed));
        }
        resources.push(object(vec![Return, Optional], 12, Owned));
        operations.push(
            UniFfiBackendOperation::new(
                method_id + 4,
                format!("callback_{method_id}"),
                async_kind,
                CallbackMethod,
                fallible,
                vec![Primitive, Primitive, OpaqueHandle, LocalAdapter],
                Some(OpaqueHandle),
            )
            .unwrap()
            .with_dispatch(UniFfiBackendDispatch::HostDispatched)
            .with_callback_dispatch(6, method_id)
            .with_resource_use_sites(resources),
        );
    }
    operations.push(
        UniFfiBackendOperation::new(
            8,
            "pendingCallback",
            Async,
            Function,
            false,
            vec![LocalAdapter],
            Some(LocalAdapter),
        )
        .unwrap()
        .with_host_argument(true),
    );
    let backend = UniFfiBackendConfig::new(
        "callback_resource_factory",
        operations,
        UniFfiBackendResourceExports::new(Some("releaseObjectHook".into()), None).unwrap(),
        ClosePolicy {
            grace_ms: 25,
            on_deadline: DeadlineAction::Detach,
        },
    )
    .unwrap();
    let mut config = Bindgen::new();
    config.binding_surface(BindingSurface::UniFfiBackend(backend));
    let mut module = Module::default();
    let wit = NonstandardWitSection::default();
    let aux = WasmBindgenAux::default();
    let mut cx = Context::new(&mut module, &config, &wit, &aux).unwrap();
    for (name, argument, body) in [
        ("captureHost", "host", "return host;"),
        ("readObject", "handle", "return handle;"),
        ("makeObject", "handle", "return handle;"),
        ("makeOtherObject", "handle", "return handle;"),
        (
            "releaseObjectHook",
            "handle",
            "return recordRelease(handle);",
        ),
        (
            "pendingCallback",
            "host, invoke",
            "return callbackGate.then(() => invoke(host));",
        ),
    ] {
        cx.exports.insert(
            name.to_owned(),
            ExportEntry::Definition(ExportDefinition {
                identifier: name.to_owned(),
                comments: None,
                definition: format!("function {name}({argument}) {{ {body} }}\n"),
                ts_comments: None,
                ts_definition: String::new(),
                private: false,
                parent_identifier: None,
            }),
        );
    }
    // Execute the real Context emission, including its typed plan and private
    // operation table. The companion contract never copies the JS template.
    cx.write_exports().unwrap();
    let script = format!(
        "{}\n{}",
        cx.globals,
        include_str!("uniffi_callback_resources.mjs")
    );
    let mut child = Command::new("node")
        .arg("--input-type=module")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("callback resource contract requires Node.js");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "callback resource contract failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
