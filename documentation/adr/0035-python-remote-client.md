# ADR 0035: The Python remote client

Status: accepted
Date: 2026-10-04

## Context

Step 14 asks for a Python client of the server with the API of the embedded bindings ([python-api.md](../python-api.md), ADR 0013): the same names, arguments, result dicts and exceptions, so that the Python test suite runs unchanged against both. The design doc planned it as a separate package in `clients/python/`, sync and async, over gRPC.

Points to decide:

- How the client talks to the server. Values, filters and patterns cross gRPC as postcard bytes in the core's serde layout (ADR 0023): a non-Rust client needs an encoder and decoder for that layout, or a JSON form added to the protos' oneofs. REST speaks the core's JSON form, but loses gRPC's streaming and deadlines, and JSON parsers with a nesting limit can't read deep values (ADR 0030).
- How it is packaged and installed.
- What the calls that need the store's directory (`sync`, `checkpoint`, backups, imports from a local file) do remotely.
- How read-your-writes works across the network.
- What "async" means.

Options for the client:

1. **Pure Python over gRPC** (`grpcio`, generated stubs) with a postcard codec for `Value`, `Expr` and `Pattern` written in Python. It installs anywhere without a native wheel. But it is a second implementation of everything the bindings do: value conversion (bit-exact floats, datetimes with offsets, the depth rule), the error mapping, the result dicts, plus the codec. Every change of the API, and every core bump that touches the serde layout, has to be made twice, and drift shows up only where the shared suite happens to look (design rules 8 and 9).
2. **Pure Python with a JSON form** added to the protos (`string json = 2` in the oneofs, ADR 0023 left room for it). No codec, but the same duplication of conversions and dicts, plus a proto change and a second decode path in the server.
3. **Pure Python over REST.** Needs nothing new on the server, but it is not gRPC (the step asks for gRPC), has no deadlines but `timeoutMs`, and values nested more than about 60 levels need a parser without a nesting limit.
4. **Native: the bindings serve a `Store` from the Rust client.** `iwdb_server::client::Remote` already implements the `Database` trait over gRPC (ADR 0024), the trait the bindings call since step 10. The same pyo3 classes take either `iwdb::Embedded` or `Remote`.

## Decision

Option 4.

- **One `Store` class, two backends.** `iwdb.connect(endpoint)` returns an `iwdb.Store` backed by `Remote`; `iwdb.Store.open(path)` one backed by `Embedded`. The bindings hold an enum of the two that implements `Database` by delegating, so every call that goes through the trait (commits, the catalog, namespaces, every read, the change stream) is the same code for both. There is one value conversion, one error mapping and one set of result dicts. `Namespace` and `Transaction` are unchanged.
- **One wheel.** `ironweaver-db` depends on `iwdb-server` with `default-features = false, features = ["client"]`: tonic, prost and tokio, without the REST, axum, pbjson and Postgres parts. `clients/python/` is not created.
- **Local-only calls.** Calls that act on the store's directory or the `iwdb::Store` itself are not part of the trait: `sync`, `checkpoint`, `checkpoint_all`, `history`, the store-wide `status`, `import_namespace`, `import_file`, `export` and `backup`. On a remote store they raise `iwdb.InvalidError` ("… needs an embedded store"). The module functions `verify` and `restore` work on directories and have no remote form. `seq`, `synced_seq`, `read_only`, `indexes` and `Namespace.status` come from the trait's `namespace_status` on both backends.
- **Read-your-writes.** A remote `Store` remembers the seq of its last commit (data or catalog) per namespace name. A read without `min_seq` sends that seq, so a client always sees its own commits, also through a load balancer or after a reconnect. An explicit `min_seq` is sent as given. Dropping a namespace through the client forgets its seq; a namespace dropped and created again by someone else can make a read wait for a seq the new namespace hasn't reached, until its timeout. The embedded store applies every commit before it returns and tracks nothing.
- **Async: `iwdb.aio`.** `AsyncStore`, `AsyncNamespace` and `AsyncTransaction` wrap the sync objects and run each call with `asyncio.to_thread`. The calls release the GIL in Rust, so they run in parallel with the event loop. It works the same for both backends.
- **Errors.** The server's code travels in `iwdb-code` (ADR 0026) and `Remote` turns it back into an `iwdb_query::Error` with that code, so a remote call raises the class the embedded call would. A lost connection is `unavailable`: `iwdb.UnavailableError`, new in this step alongside `BudgetExceededError`, `CursorExpiredError` and `NotRetainedError`.

## Consequences

- The API, the dicts and the exceptions can't drift between the two modes: they are one implementation. The shared suite checks the transport and the trait's behaviour, not two codebases.
- No pure-Python install: a remote-only user needs a wheel for their platform (Linux and macOS, x86-64 and arm64; Windows isn't shipped, ADR 0013). If that matters later, a pure-Python package can be added, and it would need options 1 or 2's codec or JSON form. Nothing here prevents it.
- The wheel grows by tonic, prost and tokio (a few MB), and the bindings depend on the `iwdb-server` crate. ADR 0024's slim `iwdb-client` crate (protos, `convert` and `Remote`) can be split out later without changing the Python side.
- The sdist now carries `iwdb-server`, whose build script compiles the protos. maturin packs each crate's own files and refuses include patterns outside the Python crate, so `crates/iwdb-server/proto` is a symlink to the workspace's `proto/`: cargo packages its contents, and the build script reads the protos through it. The release workflow builds a wheel from the sdist to keep this working. Windows checkouts need symlinks enabled (`core.symlinks`), which only matters once Windows is supported.
- A remote `Store` owns a small tokio runtime (two threads) for its calls. Python threads can share it.
- Forking with an open remote `Store` is unsupported like the embedded one: the child inherits a runtime whose threads don't exist.
- No authentication or TLS until step 15; `connect` takes `http://` endpoints only.
