import assert from 'node:assert/strict';

// Appended to Context::write_exports() output by the Rust contract. These are
// the native operation/release test doubles, not a copy of the session engine.
const releases = new Map();
let releaseBarrier = null;
let callbackGate = null;
let nextHandle = 1000;
const freshHandle = () => nextHandle++;
function recordRelease(handle) {
    releases.set(handle, (releases.get(handle) ?? 0) + 1);
    return releaseBarrier;
}
const releaseCount = (handle) => releases.get(handle) ?? 0;
const emptyNested = () => ({
    optional: null,
    sequence: [],
    set: new Set(),
    map: new Map(),
    variant: { tag: 'empty', object: 'unselected' },
});
const argsFor = (token, nested = emptyNested()) => [17, 23, token, nested];
function resources(args) {
    const nested = args[3];
    return [
        args[2],
        ...(nested.optional == null ? [] : [nested.optional]),
        ...nested.sequence,
        ...nested.set,
        ...nested.map.keys(),
        ...nested.map.values(),
        ...(nested.variant.tag === 'resource' ? [nested.variant.object] : []),
    ];
}
function harness(method) {
    const state = { received: [], before: () => {}, returnValue: null, errorEnvelope: null, rejection: null, checkObjectCalls: true };
    const receive = (typeId, callbackId, methodId, args) => {
        state.before();
        assert.equal(typeId, 6);
        assert.equal(callbackId, 44);
        assert.equal(methodId, method.id);
        state.received.push(args);
        for (const lease of resources(args)) {
            assert.ok(Object.isFrozen(lease));
            assert.equal(lease.typeId, 12);
            // Exercise the strict engine unwrap path, not just lease fields.
            if (state.checkObjectCalls) assert.equal(state.backend.invokeSync(1, [lease]).value, lease.handle);
        }
        return state.returnValue;
    };
    const settle = (value) => state.rejection ? Promise.reject(state.rejection) : Promise.resolve(value);
    const host = {
        invokeCallbackSync: receive,
        invokeCallbackAsync(typeId, callbackId, methodId, invocationId, args) {
            assert.ok(Number.isInteger(invocationId));
            return settle(receive(typeId, callbackId, methodId, args));
        },
        invokeCallbackSyncResult(...args) {
            const value = receive(...args);
            return state.errorEnvelope ?? { ok: true, value };
        },
        invokeCallbackAsyncResult(typeId, callbackId, methodId, invocationId, args) {
            assert.equal(invocationId, 77);
            const value = receive(typeId, callbackId, methodId, args);
            return settle(state.errorEnvelope ?? { ok: true, value });
        },
        retainCallback() {},
        releaseCallback() {},
        async pullInputStream() { return { kind: 'done' }; },
        async cancelInputStream() {},
        releaseInputStream() {},
    };
    state.backend = callback_resource_factory_impl(host);
    const engineHost = state.backend.invokeSync(0, []).value;
    state.invokeWith = (engineHost, args) => {
        const prefix = [6, 44, method.id];
        if (method.async && method.fallible) prefix.push(77);
        return engineHost[method.name](...prefix, args);
    };
    state.invoke = (args) => state.invokeWith(engineHost, args);
    state.makeObject = (handle = freshHandle(), operation = 2) => state.backend.invokeSync(operation, [handle]).value;
    state.dispose = (lease) => {
        state.backend.releaseObject(lease);
        state.backend.releaseObject(lease);
        assert.equal(releaseCount(lease.handle), 1);
    };
    return state;
}

for (const method of [
    { id: 0, name: 'invokeCallbackSync', async: false, fallible: false },
    { id: 1, name: 'invokeCallbackSyncResult', async: false, fallible: true },
    { id: 2, name: 'invokeCallbackAsync', async: true, fallible: false },
    { id: 3, name: 'invokeCallbackAsyncResult', async: true, fallible: true },
]) {
    const h = harness(method);
    const rawArgs = argsFor(freshHandle(), {
        optional: freshHandle(),
        sequence: [freshHandle(), freshHandle()],
        set: new Set([freshHandle()]),
        map: new Map([[freshHandle(), freshHandle()]]),
        variant: { tag: 'resource', object: freshHandle() },
    });
    const handles = resources(rawArgs);
    const result = await h.invoke(rawArgs);
    assert.deepEqual(result, method.fallible ? { ok: true, value: null } : null);
    const received = h.received.at(-1);
    assert.deepEqual(resources(received).map((lease) => lease.handle), handles);
    assert.deepEqual(resources(rawArgs), handles, 'conversion must not mutate raw containers');
    for (const lease of resources(received)) {
        assert.equal(releaseCount(lease.handle), 0, 'callback completion must not dispose its arguments');
        assert.equal(h.backend.invokeSync(1, [lease]).value, lease.handle);
        h.dispose(lease);
    }

    // Already-owned public leases must be validated and reused through both
    // the Rust-facing Host and the public host-dispatched operation entry.
    const existing = h.makeObject();
    const existingArgs = argsFor(existing);
    await h.invoke(existingArgs);
    assert.equal(h.received.at(-1)[2], existing);
    assert.equal(h.received.at(-1)[3].optional, null);
    assert.equal(h.received.at(-1)[3].variant.object, 'unselected');
    existingArgs[3].optional = undefined;
    const operationArgs = [44, ...existingArgs];
    const publicResult = method.async
        ? await h.backend.invokeAsync(method.id + 4, operationArgs)
        : h.backend.invokeSync(method.id + 4, operationArgs);
    assert.deepEqual(publicResult, { kind: 'value', value: null });
    assert.equal(h.received.at(-1)[2], existing);
    assert.equal(h.received.at(-1)[3].optional, undefined);
    h.dispose(existing);

    // Keep strict session/type/identity/stale checks; the ordinary object
    // entry still rejects a numeric carrier instead of accepting two ABIs.
    assert.throws(() => h.backend.invokeSync(1, [freshHandle()]), /resource lease/);
    const sibling = harness(method);
    const foreign = sibling.makeObject();
    const wrongType = h.makeObject(freshHandle(), 3);
    const valid = h.makeObject();
    for (const invalid of [foreign, wrongType, existing, { ...valid }]) {
        const calls = h.received.length;
        await assert.rejects(async () => h.invoke(argsFor(invalid)), /resource lease/);
        assert.equal(h.received.length, calls);
    }
    assert.equal(releaseCount(valid.handle), 0);
    assert.equal(releaseCount(foreign.handle), 0);
    assert.equal(releaseCount(wrongType.handle), 0);
    sibling.dispose(foreign);
    h.dispose(wrongType);
    await sibling.backend.close();

    // Roll back partial nested conversion, never a pre-existing lease. Both
    // the new root and a new sequence element precede the invalid handle.
    for (const invalid of [-1, 0x100000000, 1.5, NaN, {}, null]) {
        const root = freshHandle();
        const element = freshHandle();
        const calls = h.received.length;
        await assert.rejects(async () => h.invoke(argsFor(root, {
            ...emptyNested(), optional: valid, sequence: [element, invalid],
        })), /raw handle/);
        assert.equal(h.received.length, calls);
        assert.equal(releaseCount(root), 1);
        assert.equal(releaseCount(element), 1);
        assert.equal(releaseCount(valid.handle), 0);
    }
    const malformedRoot = freshHandle();
    await assert.rejects(async () => h.invoke(argsFor(malformedRoot, {})), /field resource path/);
    assert.equal(releaseCount(malformedRoot), 1);

    // A synchronous dispatch failure must propagate its original error and
    // release new arguments. Existing public arguments stay owned by callers.
    const dispatchError = new Error('dispatch failed before delivery');
    h.before = () => { throw dispatchError; };
    const failedHandle = freshHandle();
    await assert.rejects(async () => h.invoke(argsFor(failedHandle, {
        ...emptyNested(), optional: valid,
    })), (error) => error === dispatchError || error.cause === dispatchError);
    assert.equal(releaseCount(failedHandle), 1);
    assert.equal(releaseCount(valid.handle), 0);
    h.before = () => {};
    h.dispose(valid);

    if (method.fallible) {
        h.errorEnvelope = { ok: false, error: { tag: 'Io', message: 'typed callback failure' } };
        const handle = freshHandle();
        assert.equal(await h.invoke(argsFor(handle)), h.errorEnvelope);
        assert.equal(releaseCount(handle), 0, 'an error envelope still delivered its arguments');
        h.dispose(h.received.at(-1)[2]);
        h.errorEnvelope = null;
    }
    if (method.async) {
        h.rejection = new Error('callback rejected after delivery');
        const handle = freshHandle();
        await assert.rejects(async () => h.invoke(argsFor(handle)),
            (error) => error === h.rejection || error.cause === h.rejection);
        assert.equal(releaseCount(handle), 0, 'a rejected callback still owns delivered arguments');
        h.dispose(h.received.at(-1)[2]);
        h.rejection = null;
    }

    // The opposite direction remains lease -> raw handle, and remains strict.
    h.returnValue = h.makeObject();
    const returned = await h.invoke(argsFor(freshHandle()));
    assert.equal(method.fallible ? returned.value : returned, h.returnValue.handle);
    assert.equal(releaseCount(h.returnValue.handle), 0);
    h.dispose(h.received.at(-1)[2]);
    h.dispose(h.returnValue);
    h.returnValue = freshHandle();
    await assert.rejects(async () => h.invoke(argsFor(freshHandle())), /must be an engine lease/);
    assert.equal(releaseCount(h.received.at(-1)[2].handle), 0);
    h.dispose(h.received.at(-1)[2]);
    h.returnValue = null;

    const closeHandle = freshHandle();
    await h.invoke(argsFor(closeHandle));
    assert.equal(releaseCount(closeHandle), 0);
    await h.backend.close();
    assert.equal(releaseCount(closeHandle), 1);
    await h.backend.close();
    assert.equal(releaseCount(closeHandle), 1);
    await assert.rejects(async () => h.invoke(argsFor(freshHandle())), /closed/);

    // Failed-dispatch cleanup participates in close draining without replacing
    // the dispatch error or requiring callback completion to own the lease.
    const cleanup = harness(method);
    let finishRelease;
    releaseBarrier = new Promise((resolve) => { finishRelease = resolve; });
    cleanup.before = () => { throw dispatchError; };
    const pendingRelease = freshHandle();
    await assert.rejects(async () => cleanup.invoke(argsFor(pendingRelease)),
        (error) => error === dispatchError || error.cause === dispatchError);
    let closed = false;
    const close = cleanup.backend.close().then(() => { closed = true; });
    await Promise.resolve();
    assert.equal(closed, false);
    finishRelease();
    await close;
    releaseBarrier = null;
    assert.equal(releaseCount(pendingRelease), 1);

    // An in-flight Rust call may first enter its callback after close starts.
    // Its new arguments are delivered as leases and swept during drain. New
    // public method calls are closed, so this case only inspects the carrier.
    const late = harness(method);
    late.checkObjectCalls = false;
    let resumeCallback;
    callbackGate = new Promise((resolve) => { resumeCallback = resolve; });
    const lateHandle = freshHandle();
    const pending = late.backend.invokeAsync(8, [
        (engineHost) => late.invokeWith(engineHost, argsFor(lateHandle)),
    ]);
    const lateClose = late.backend.close();
    resumeCallback();
    await pending;
    await lateClose;
    assert.equal(late.received.length, 1);
    assert.equal(late.received[0][2].handle, lateHandle);
    assert.equal(releaseCount(lateHandle), 1);
    callbackGate = null;
}
