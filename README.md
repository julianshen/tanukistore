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

```sh
cargo run     # start the binary
cargo test    # run tests
cargo clippy  # lint
```
