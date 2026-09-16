# tanukistore

A self-hosted update server for Electron applications, compatible with Electron's
native `autoUpdater` module (Squirrel.Mac on macOS, Squirrel.Windows on Windows).

## Status

Early scaffold. The architecture and wire-protocol analysis live in
[`electron-update-server-design_2.md`](./electron-update-server-design_2.md).

## Design summary

- **Storage:** S3-compatible object storage (AWS S3 or MinIO) as the source of truth.
- **Caching:** in-memory tier plus local disk in front of object storage, with
  explicit invalidation.
- **Observability:** OpenTelemetry tracing and structured, user-attributable logging
  of every version check.
- **Routes:** `/update/:platform/:version[/:channel]` for update checks,
  `/download/...` for assets.

## Development

The repository is a virtual workspace: `tanukistore-core` holds the pure
protocol logic, and `tanukistore-server` and `tanukistore-publish` are the two
binaries. Because there are two binaries, the root commands name their scope
explicitly.

```sh
cargo run -p tanukistore-server        # start the server
cargo test --workspace                 # run tests
cargo clippy --workspace --all-targets # lint
```

Scope is spelled out rather than set once via `default-members`, which would
make bare `cargo run` work but would also narrow `cargo test` and `cargo clippy`
to that same member — quietly dropping every test in `tanukistore-core`, which
is where all of them currently live.
