# Fixtures: unverified belief, not yet an oracle

`latest-darwin-arm64.json` and `RELEASES-win32-x64` were derived from **spec §4.7 and
the protocol-core plan** — from our reading of the Squirrel wire formats. **Neither has
been validated against a real Squirrel client.**

Spec §12 item 1 is explicit about what that means:

> Golden fixtures alone would only encode our *belief* about the wire format; validating
> against a real client first is what makes them an oracle.

So these two files are currently on the *belief* side of that line. Every test that
asserts `derive_latest` / `derive_releases` against them proves only self-consistency:
if our reading of the format is wrong, the tests pass and the fleet silently never
updates. That failure mode is the whole reason the gate is explicit.

**The outstanding human gate is plan Task 8 / spec §12 item 1**, which requires a human
with a signed Electron app pointing both real Squirrel clients at these bytes. An
agentic worker cannot close it. Until it is closed, treat a fixture mismatch as "the
fixture may be wrong" and not only as "the code may be wrong", and do not cite
`derive.rs`'s "the golden fixture asserts on it, so do not reorder" as evidence that the
field order is protocol-correct — it is evidence only that it is *frozen*.

## What that gate must check, in order

### 1. SHA-1 letter case in `RELEASES` — check this first

Real Squirrel.Windows writes `RELEASES` using
`BitConverter.ToString(hash).Replace("-", "")`, which produces **UPPERCASE** hex.
Rust's `sha2`/hex conventions are lowercase, and `RELEASES-win32-x64` in this directory
is **lowercase**.

Whether Squirrel.Windows' entry-parsing regex and its post-download checksum comparison
are case-insensitive **could not be verified offline**. This README deliberately does
not assert either way — it is an open question, not a settled one.

The stakes: if *either* the parse or the comparison is case-sensitive on the uppercase
form, **every Windows client rejects every package**, silently, forever. If it turns out
case-sensitive, the fix is in the publisher (emit uppercase) and in this fixture — not in
`derive_releases`, which passes the `sha1` field through verbatim from `index.json`.

### 2. The rest

- `latest.json` field names and JSON shape: does Squirrel.Mac accept `url` / `name` /
  `notes` / `pub_date`, and is the field order irrelevant to it as we assume?
- `pub_date` format: we emit `%Y-%m-%dT%H:%M:%SZ`. Confirm the client parses it.
- The absolute `url` (spec §4.7) is fetched successfully by `NSURLSession`, including
  the percent/form-encoded query values (`+` for space, `%2B` for `+`).
- `RELEASES` line shape `{sha1} {filename} {size}` with a single space separator, a
  trailing newline after the final entry, and relative filenames resolved against the
  feed base URL.
- Ascending-by-version line order: confirm Squirrel.Windows does not depend on a
  different order.

Once a real client has accepted the bytes, record the evidence in
`docs/protocol-verification.md` (plan Task 8) and replace this README's opening
paragraph with what was verified, by which client version, on which date.
