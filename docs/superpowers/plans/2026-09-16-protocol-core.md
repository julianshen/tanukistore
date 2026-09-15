# tanukistore Protocol Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the pure domain core of tanukistore — the types, key layout, rollout bucketing, version resolution, and manifest derivation that turn an `index.json` into byte-exact Squirrel.Mac and Squirrel.Windows manifests.

**Architecture:** A three-crate Cargo workspace whose `core` crate holds all protocol logic as pure functions with no I/O, no clock, and no network. Manifest derivation is a pure function of `(index, coordinate, base_url)`, which is what makes golden-fixture tests a meaningful oracle for wire-protocol correctness. Later plans layer storage, HTTP, and a publisher on top of this crate without changing it.

**Tech Stack:** Rust 1.91, edition 2024, `serde` + `serde_json`, `semver`, `sha2`, `chrono`, `thiserror`. No async runtime in this plan — nothing here does I/O.

**Spec:** `docs/superpowers/specs/2026-09-16-tanukistore-design.md`

## Global Constraints

- Rust edition **2024**; workspace `resolver = "3"`. Toolchain 1.91.0 is already installed.
- Package names `tanukistore-core`, `tanukistore-server`, `tanukistore-publish`; all carry `publish = false`.
- Every crate lives under `crates/`. Root `Cargo.toml` is a **virtual** workspace manifest with no `[package]`.
- `crates/core` performs **no I/O** in this plan: no `tokio`, no `aws-sdk-s3`, no filesystem access outside `#[cfg(test)]` fixture loading.
- The rollout bucketing hash is a **frozen wire contract** (spec §4.2): SHA-256 over `{app}:{channel}:{id}`, first 8 bytes big-endian as `u64`, `% 100`, eligible when `< rollout_pct`. Never change it.
- `latest.json` describes **only the newest eligible release**. `RELEASES` enumerates **every full nupkg in the channel's history**, ascending by version (spec §5).
- Manifests embed tanukistore's own `/download/...` and relative-filename URLs, never presigned URLs (spec §4.7).
- `rollout_pct` is constrained to `0..=100` by the type system, not by convention.
- Conventional Commits for every commit message.

---

### Task 1: Workspace restructure

**Files:**
- Modify: `Cargo.toml` (replace package manifest with virtual workspace)
- Create: `crates/core/Cargo.toml`, `crates/core/src/lib.rs`
- Create: `crates/server/Cargo.toml`, `crates/server/src/main.rs`
- Create: `crates/publish/Cargo.toml`, `crates/publish/src/main.rs`
- Delete: `src/main.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the crate `tanukistore_core` (lib target), plus binaries `tanukistore-server` and `tanukistore-publish`. Later tasks add modules to `crates/core/src/`.

- [ ] **Step 1: Replace the root manifest with a virtual workspace**

```toml
# Cargo.toml
[workspace]
members = ["crates/core", "crates/server", "crates/publish"]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"
publish = false

[workspace.dependencies]
chrono = { version = "0.4", features = ["serde"] }
semver = { version = "1", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
thiserror = "2"
```

- [ ] **Step 2: Create the three crate manifests**

```toml
# crates/core/Cargo.toml
[package]
name = "tanukistore-core"
version.workspace = true
edition.workspace = true
publish.workspace = true

[dependencies]
chrono.workspace = true
semver.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
```

```toml
# crates/server/Cargo.toml
[package]
name = "tanukistore-server"
version.workspace = true
edition.workspace = true
publish.workspace = true

[dependencies]
tanukistore-core = { path = "../core" }
```

```toml
# crates/publish/Cargo.toml
[package]
name = "tanukistore-publish"
version.workspace = true
edition.workspace = true
publish.workspace = true

[dependencies]
tanukistore-core = { path = "../core" }
```

- [ ] **Step 3: Create the crate roots**

```rust
// crates/core/src/lib.rs
//! Pure protocol core for tanukistore: types, key layout, rollout bucketing,
//! version resolution, and Squirrel manifest derivation. No I/O lives here.
```

```rust
// crates/server/src/main.rs
fn main() {
    println!("tanukistore-server: not implemented yet");
}
```

```rust
// crates/publish/src/main.rs
fn main() {
    println!("tanukistore-publish: not implemented yet");
}
```

- [ ] **Step 4: Remove the old single-crate source**

```bash
git rm src/main.rs
```

- [ ] **Step 5: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: three crates compile; no warnings about a missing workspace resolver.

Run: `cargo test --workspace`
Expected: PASS with 0 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/
git commit -m "refactor: restructure into a three-crate workspace"
```

---

### Task 2: Domain model

**Files:**
- Create: `crates/core/src/model.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's `tanukistore_core` crate.
- Produces:
  - `enum Platform { Darwin, Win32 }`, `enum Arch { X64, Arm64 }`, both `Copy` with `Display` and `FromStr<Err = ParseError>`.
  - `enum AssetKind { Zip, Nupkg, Dmg, Exe }`.
  - `struct RolloutPct(u8)` with `RolloutPct::new(u8) -> Result<Self, ParseError>`, `RolloutPct::get(&self) -> u8`, `RolloutPct::FULL`.
  - `struct Asset { kind: AssetKind, filename: String, sha1: String, sha512: String, size_bytes: u64 }`.
  - `struct Release { version: semver::Version, notes: String, pub_date: chrono::DateTime<chrono::Utc>, rollout_pct: RolloutPct, assets: Vec<Asset> }`.
  - `struct Index { releases: Vec<Release> }` serialized transparently as a JSON array.
  - `enum ParseError` (via `thiserror`).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/core/src/model.rs  (append at the bottom of the file)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_and_arch_render_as_wire_tokens() {
        assert_eq!(Platform::Darwin.to_string(), "darwin");
        assert_eq!(Platform::Win32.to_string(), "win32");
        assert_eq!(Arch::X64.to_string(), "x64");
        assert_eq!(Arch::Arm64.to_string(), "arm64");
        assert_eq!("darwin".parse::<Platform>().unwrap(), Platform::Darwin);
        assert_eq!("arm64".parse::<Arch>().unwrap(), Arch::Arm64);
        assert!("linux".parse::<Platform>().is_err());
    }

    #[test]
    fn rollout_pct_rejects_out_of_range() {
        assert_eq!(RolloutPct::new(0).unwrap().get(), 0);
        assert_eq!(RolloutPct::new(100).unwrap().get(), 100);
        assert!(RolloutPct::new(101).is_err());
        assert_eq!(RolloutPct::FULL.get(), 100);
    }

    #[test]
    fn index_round_trips_as_a_json_array() {
        let json = r#"[
          {
            "version": "1.5.0",
            "notes": "fixes",
            "pub_date": "2026-09-16T10:00:00Z",
            "rollout_pct": 10,
            "assets": [
              { "kind": "zip", "filename": "app-1.5.0-darwin-arm64.zip",
                "sha1": "aa", "sha512": "bb", "size_bytes": 1024 }
            ]
          }
        ]"#;
        let index: Index = serde_json::from_str(json).unwrap();
        assert_eq!(index.releases.len(), 1);
        let r = &index.releases[0];
        assert_eq!(r.version, semver::Version::new(1, 5, 0));
        assert_eq!(r.rollout_pct.get(), 10);
        assert_eq!(r.assets[0].kind, AssetKind::Zip);

        let back: Index = serde_json::from_str(&serde_json::to_string(&index).unwrap()).unwrap();
        assert_eq!(back.releases[0].version, r.version);
    }

    #[test]
    fn rollout_pct_out_of_range_fails_deserialization() {
        let json = r#"[{"version":"1.0.0","notes":"","pub_date":"2026-09-16T10:00:00Z",
                        "rollout_pct":150,"assets":[]}]"#;
        assert!(serde_json::from_str::<Index>(json).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tanukistore-core`
Expected: FAIL — `model` module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
// crates/core/src/model.rs  (above the test module)
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("unknown platform: {0}")]
    Platform(String),
    #[error("unknown arch: {0}")]
    Arch(String),
    #[error("rollout_pct must be 0..=100, got {0}")]
    RolloutPct(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Darwin,
    Win32,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::Darwin => "darwin",
            Platform::Win32 => "win32",
        })
    }
}

impl FromStr for Platform {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "darwin" => Ok(Platform::Darwin),
            "win32" => Ok(Platform::Win32),
            other => Err(ParseError::Platform(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    X64,
    Arm64,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        })
    }
}

impl FromStr for Arch {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x64" => Ok(Arch::X64),
            "arm64" => Ok(Arch::Arm64),
            other => Err(ParseError::Arch(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Zip,
    Nupkg,
    Dmg,
    Exe,
}

/// Percentage of the fleet eligible for a release. Constrained at the type
/// level so an out-of-range value cannot make every client eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct RolloutPct(u8);

impl RolloutPct {
    pub const FULL: RolloutPct = RolloutPct(100);

    pub fn new(value: u8) -> Result<Self, ParseError> {
        if value > 100 {
            return Err(ParseError::RolloutPct(u16::from(value)));
        }
        Ok(RolloutPct(value))
    }

    pub fn get(&self) -> u8 {
        self.0
    }
}

impl TryFrom<u16> for RolloutPct {
    type Error = ParseError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if value > 100 {
            return Err(ParseError::RolloutPct(value));
        }
        Ok(RolloutPct(value as u8))
    }
}

impl From<RolloutPct> for u16 {
    fn from(value: RolloutPct) -> u16 {
        u16::from(value.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub kind: AssetKind,
    pub filename: String,
    /// Hex SHA-1. Required by the Squirrel.Windows `RELEASES` format.
    pub sha1: String,
    /// Hex SHA-512, for integrity verification.
    pub sha512: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub version: Version,
    #[serde(default)]
    pub notes: String,
    pub pub_date: DateTime<Utc>,
    pub rollout_pct: RolloutPct,
    pub assets: Vec<Asset>,
}

/// The append-only system of record for one app/channel/platform/arch.
/// Serializes as a bare JSON array to match the on-disk `index.json` shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Index {
    pub releases: Vec<Release>,
}
```

```rust
// crates/core/src/lib.rs  (append)
pub mod model;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tanukistore-core`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/lib.rs
git commit -m "feat(core): add domain model with type-constrained rollout percentage"
```

---

### Task 3: Key layout

**Files:**
- Create: `crates/core/src/keys.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `Platform`, `Arch` from Task 2.
- Produces:
  - `struct Coordinate { app: String, channel: String, platform: Platform, arch: Arch }` with `Coordinate::new(impl Into<String>, impl Into<String>, Platform, Arch)`.
  - Methods `prefix()`, `index_key()`, `latest_key()`, `releases_key()`, `asset_key(&semver::Version, &str)`, all returning `String`.
  - `fn config_key(app: &str) -> String`.
  - `enum KeyKind { Config, Index, Latest, Releases }` with `Display`, used as the `key_kind` metric label in later plans.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/core/src/keys.rs  (append at the bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, Platform};

    fn coord() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64)
    }

    #[test]
    fn keys_match_the_documented_layout() {
        let c = coord();
        assert_eq!(c.prefix(), "myapp/stable/darwin/arm64");
        assert_eq!(c.index_key(), "myapp/stable/darwin/arm64/index.json");
        assert_eq!(c.latest_key(), "myapp/stable/darwin/arm64/latest.json");
        assert_eq!(c.releases_key(), "myapp/stable/darwin/arm64/RELEASES");
        assert_eq!(
            c.asset_key(&semver::Version::new(1, 5, 0), "app.zip"),
            "myapp/stable/darwin/arm64/1.5.0/app.zip"
        );
        assert_eq!(config_key("myapp"), "myapp/config.json");
    }

    #[test]
    fn key_kind_renders_as_a_metric_label() {
        assert_eq!(KeyKind::Index.to_string(), "index");
        assert_eq!(KeyKind::Releases.to_string(), "releases");
        assert_eq!(KeyKind::Latest.to_string(), "latest");
        assert_eq!(KeyKind::Config.to_string(), "config");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tanukistore-core keys`
Expected: FAIL — `keys` module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
// crates/core/src/keys.rs  (above the test module)
use std::fmt;

use semver::Version;

use crate::model::{Arch, Platform};

/// One app/channel/platform/arch combination — the unit that owns an
/// `index.json` and its two derived manifests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Coordinate {
    pub app: String,
    pub channel: String,
    pub platform: Platform,
    pub arch: Arch,
}

impl Coordinate {
    pub fn new(
        app: impl Into<String>,
        channel: impl Into<String>,
        platform: Platform,
        arch: Arch,
    ) -> Self {
        Coordinate {
            app: app.into(),
            channel: channel.into(),
            platform,
            arch,
        }
    }

    pub fn prefix(&self) -> String {
        format!("{}/{}/{}/{}", self.app, self.channel, self.platform, self.arch)
    }

    pub fn index_key(&self) -> String {
        format!("{}/index.json", self.prefix())
    }

    pub fn latest_key(&self) -> String {
        format!("{}/latest.json", self.prefix())
    }

    pub fn releases_key(&self) -> String {
        format!("{}/RELEASES", self.prefix())
    }

    pub fn asset_key(&self, version: &Version, filename: &str) -> String {
        format!("{}/{}/{}", self.prefix(), version, filename)
    }
}

pub fn config_key(app: &str) -> String {
    format!("{app}/config.json")
}

/// Bounded classification of cacheable objects, used as the `key_kind`
/// metric label. Deliberately low-cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Config,
    Index,
    Latest,
    Releases,
}

impl fmt::Display for KeyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            KeyKind::Config => "config",
            KeyKind::Index => "index",
            KeyKind::Latest => "latest",
            KeyKind::Releases => "releases",
        })
    }
}
```

```rust
// crates/core/src/lib.rs  (append)
pub mod keys;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tanukistore-core keys`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/keys.rs crates/core/src/lib.rs
git commit -m "feat(core): add object key layout and coordinate type"
```

---

### Task 4: Rollout bucketing

**Files:**
- Create: `crates/core/src/rollout.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `RolloutPct` from Task 2.
- Produces:
  - `fn bucket(app: &str, channel: &str, id: &str) -> u8` — the frozen hash, returning `0..=99`.
  - `fn eligible(app: &str, channel: &str, id: Option<&str>, pct: RolloutPct) -> bool`.

**Why this hash and not a `HashMap` default:** `std`'s `RandomState` seeds SipHash per process, so the same client would bucket differently on every replica and re-bucket on every restart — clients flapping between old and new versions. SHA-256 with no seed is stable across processes, machines, and releases forever.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/core/src/rollout.rs  (append at the bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RolloutPct;

    #[test]
    fn bucket_is_stable_and_in_range() {
        let first = bucket("myapp", "stable", "install-abc");
        for _ in 0..100 {
            assert_eq!(bucket("myapp", "stable", "install-abc"), first);
        }
        assert!(first < 100);
    }

    #[test]
    fn bucket_is_pinned_to_known_values() {
        // Frozen wire contract (spec 4.2). If these change, the entire fleet
        // re-buckets at once. Changing them is never a routine edit.
        assert_eq!(bucket("myapp", "stable", "install-abc"), 18);
        assert_eq!(bucket("myapp", "beta", "install-abc"), 91);
        assert_eq!(bucket("otherapp", "stable", "install-abc"), 91);
    }

    #[test]
    fn bucket_distributes_roughly_evenly() {
        let mut under_10 = 0usize;
        let total = 10_000usize;
        for i in 0..total {
            if bucket("myapp", "stable", &format!("install-{i}")) < 10 {
                under_10 += 1;
            }
        }
        // The real value for this seed set is 1034. Bounds are generous
        // because this asserts "not pathological", not a precise figure.
        assert!(
            (850..1150).contains(&under_10),
            "expected ~1000 of {total} under 10, got {under_10}"
        );
    }

    #[test]
    fn full_rollout_is_eligible_even_without_an_id() {
        assert!(eligible("myapp", "stable", None, RolloutPct::FULL));
        assert!(eligible("myapp", "stable", Some("x"), RolloutPct::FULL));
    }

    #[test]
    fn partial_rollout_excludes_clients_with_no_id() {
        let half = RolloutPct::new(50).unwrap();
        assert!(!eligible("myapp", "stable", None, half));
    }

    #[test]
    fn zero_rollout_is_never_eligible() {
        let zero = RolloutPct::new(0).unwrap();
        assert!(!eligible("myapp", "stable", Some("install-abc"), zero));
        assert!(!eligible("myapp", "stable", None, zero));
    }

    #[test]
    fn partial_rollout_tracks_the_bucket() {
        let id = "install-abc";
        let b = bucket("myapp", "stable", id); // 18
        let just_over = RolloutPct::new(b + 1).unwrap();
        let exactly_at = RolloutPct::new(b).unwrap();
        assert!(eligible("myapp", "stable", Some(id), just_over));
        assert!(!eligible("myapp", "stable", Some(id), exactly_at));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tanukistore-core rollout`
Expected: FAIL — `rollout` module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
// crates/core/src/rollout.rs  (above the test module)
use sha2::{Digest, Sha256};

use crate::model::RolloutPct;

/// Deterministically bucket a client into `0..=99`.
///
/// FROZEN WIRE CONTRACT (spec 4.2). SHA-256 over `{app}:{channel}:{id}`,
/// first 8 bytes big-endian as u64, modulo 100. Unseeded on purpose: a
/// per-process seed (as in std's RandomState) would re-bucket clients on
/// every restart and disagree between replicas, so a client would flap
/// between old and new versions. Changing this function re-buckets the
/// entire fleet simultaneously.
pub fn bucket(app: &str, channel: &str, id: &str) -> u8 {
    let mut hasher = Sha256::new();
    hasher.update(app.as_bytes());
    hasher.update(b":");
    hasher.update(channel.as_bytes());
    hasher.update(b":");
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let head: [u8; 8] = digest[..8].try_into().expect("sha256 yields 32 bytes");
    (u64::from_be_bytes(head) % 100) as u8
}

/// Whether a client is eligible for a release at `pct`.
///
/// `id` is the already-resolved bucketing identity: `cid` when present,
/// else `uid`, else `None`. A client with no stable identity is ineligible
/// for any partial rollout — failing closed, because an erroneous "no
/// update" costs one polling interval while an erroneous "update" ships a
/// build that cannot be un-shipped.
pub fn eligible(app: &str, channel: &str, id: Option<&str>, pct: RolloutPct) -> bool {
    if pct.get() >= 100 {
        return true;
    }
    match id {
        Some(id) => bucket(app, channel, id) < pct.get(),
        None => false,
    }
}
```

```rust
// crates/core/src/lib.rs  (append)
pub mod rollout;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tanukistore-core rollout`
Expected: PASS, 7 tests.

The literals `18`, `91`, `91` in `bucket_is_pinned_to_known_values` are the true outputs of
this hash, computed independently. If they fail, the implementation deviates from the frozen
contract — fix the implementation, never the literals.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/rollout.rs crates/core/src/lib.rs
git commit -m "feat(core): add fixed-seed rollout bucketing"
```

---

### Task 5: Version resolution

**Files:**
- Create: `crates/core/src/resolve.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `Index`, `Release` (Task 2); `rollout::eligible` (Task 4).
- Produces: `fn resolve_eligible<'a>(index: &'a Index, app: &str, channel: &str, id: Option<&str>) -> Option<&'a Release>` — the highest-version eligible release, or `None`.

**Why highest-version and not last-appended:** `index.json` is append-only, so a hotfix to an older line (1.4.1 published after 1.5.0) would otherwise be served as "latest" and downgrade the fleet.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/core/src/resolve.rs  (append at the bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Index, Release, RolloutPct};
    use chrono::{TimeZone, Utc};
    use semver::Version;

    fn release(version: &str, pct: u8) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            notes: String::new(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            rollout_pct: RolloutPct::new(pct).unwrap(),
            assets: vec![],
        }
    }

    #[test]
    fn empty_index_resolves_to_nothing() {
        let index = Index::default();
        assert!(resolve_eligible(&index, "myapp", "stable", Some("i")).is_none());
    }

    #[test]
    fn picks_highest_version_not_last_appended() {
        // 1.4.1 appended after 1.5.0 must not win.
        let index = Index {
            releases: vec![release("1.5.0", 100), release("1.4.1", 100)],
        };
        let got = resolve_eligible(&index, "myapp", "stable", None).unwrap();
        assert_eq!(got.version, Version::new(1, 5, 0));
    }

    #[test]
    fn skips_releases_the_client_is_not_eligible_for() {
        // 1.5.0 at 0% is never eligible; 1.4.0 at 100% always is.
        let index = Index {
            releases: vec![release("1.4.0", 100), release("1.5.0", 0)],
        };
        let got = resolve_eligible(&index, "myapp", "stable", Some("i")).unwrap();
        assert_eq!(got.version, Version::new(1, 4, 0));
    }

    #[test]
    fn resolves_to_nothing_when_no_release_is_eligible() {
        let index = Index { releases: vec![release("1.5.0", 0)] };
        assert!(resolve_eligible(&index, "myapp", "stable", Some("i")).is_none());
    }

    #[test]
    fn build_metadata_does_not_affect_ordering() {
        // Per semver, build metadata is ignored in comparison, so two
        // releases differing only by +build are ordered arbitrarily. The
        // publisher must never rely on it to distinguish releases.
        let a = release("1.5.0+build.1", 100);
        let b = release("1.5.0+build.2", 100);
        assert_eq!(a.version.cmp(&b.version), std::cmp::Ordering::Equal);
        let index = Index { releases: vec![a, b] };
        let got = resolve_eligible(&index, "myapp", "stable", None).unwrap();
        assert_eq!(got.version.major, 1);
        assert_eq!(got.version.minor, 5);
    }

    #[test]
    fn prerelease_loses_to_its_stable_counterpart() {
        let index = Index {
            releases: vec![release("1.5.0-beta.1", 100), release("1.5.0", 100)],
        };
        let got = resolve_eligible(&index, "myapp", "stable", None).unwrap();
        assert_eq!(got.version, Version::new(1, 5, 0));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tanukistore-core resolve`
Expected: FAIL — `resolve` module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
// crates/core/src/resolve.rs  (above the test module)
use crate::model::{Index, Release};
use crate::rollout;

/// Highest-version release this client is eligible for, or `None`.
///
/// Highest-version rather than last-appended: `index.json` is append-only,
/// so publishing a 1.4.1 hotfix after 1.5.0 must not downgrade the fleet.
/// semver ordering also puts `1.5.0-beta.1` below `1.5.0`, which is what a
/// channel serving both should do.
pub fn resolve_eligible<'a>(
    index: &'a Index,
    app: &str,
    channel: &str,
    id: Option<&str>,
) -> Option<&'a Release> {
    index
        .releases
        .iter()
        .filter(|release| rollout::eligible(app, channel, id, release.rollout_pct))
        .max_by(|a, b| a.version.cmp(&b.version))
}
```

```rust
// crates/core/src/lib.rs  (append)
pub mod resolve;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tanukistore-core resolve`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/resolve.rs crates/core/src/lib.rs
git commit -m "feat(core): resolve highest eligible release by semver"
```

---

### Task 6: Derive the Squirrel.Mac manifest

**Files:**
- Create: `crates/core/src/derive.rs`
- Create: `crates/core/tests/fixtures/latest-darwin-arm64.json`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `Index`, `Release`, `AssetKind` (Task 2); `Coordinate` (Task 3); `resolve_eligible` (Task 5).
- Produces:
  - `struct LatestManifest { url: String, name: String, notes: String, pub_date: String }` (field order is the serialized order, and the golden fixture depends on it).
  - `enum DeriveError` (via `thiserror`) with variant `NoAsset { version: String, kind: &'static str }`.
  - `fn derive_latest(index: &Index, coord: &Coordinate, base_url: &str) -> Result<Option<Vec<u8>>, DeriveError>` — `Ok(None)` when no release is eligible at 100%.

**Why `url` points at tanukistore and not at MinIO:** spec §4.7. A presigned URL is per-request and time-limited, so embedding one would force re-serializing the manifest on every request. Pointing at our own `/download/...` route keeps the manifest bytes static and moves presigning to the moment a download starts.

**Why `rollout_pct` is ignored here:** `latest.json` is a single stored object shared by every client, so it can only describe the release available to *all* of them. Per-client rollout filtering happens in the server handler against `index.json`, not in this derivation.

- [ ] **Step 1: Write the failing test**

```rust
// crates/core/src/derive.rs  (append at the bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, Asset, AssetKind, Index, Platform, Release, RolloutPct};
    use chrono::{TimeZone, Utc};
    use semver::Version;

    fn coord() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64)
    }

    fn zip_release(version: &str, pct: u8) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            notes: "Fixes a crash on launch.".to_owned(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            rollout_pct: RolloutPct::new(pct).unwrap(),
            assets: vec![Asset {
                kind: AssetKind::Zip,
                filename: format!("myapp-{version}-darwin-arm64.zip"),
                sha1: "da39a3ee5e6b4b0d3255bfef95601890afd80709".to_owned(),
                sha512: "cf83e1357eefb8bd".to_owned(),
                size_bytes: 92_341_120,
            }],
        }
    }

    #[test]
    fn matches_the_golden_fixture_byte_for_byte() {
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com")
            .unwrap()
            .expect("a release is eligible");
        let expected = include_str!("../tests/fixtures/latest-darwin-arm64.json");
        assert_eq!(String::from_utf8(bytes).unwrap(), expected.trim_end());
    }

    #[test]
    fn describes_only_the_newest_release() {
        let index = Index {
            releases: vec![zip_release("1.4.0", 100), zip_release("1.5.0", 100)],
        };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("1.5.0"), "{text}");
        assert!(!text.contains("1.4.0"), "{text}");
    }

    #[test]
    fn a_trailing_slash_on_base_url_does_not_double_up() {
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com/")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("com//"), "{text}");
    }

    #[test]
    fn a_partial_rollout_is_not_published_to_latest_json() {
        let index = Index { releases: vec![zip_release("1.5.0", 10)] };
        assert!(
            derive_latest(&index, &coord(), "https://updates.example.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn staged_release_falls_back_to_the_newest_full_release() {
        // 1.5.0 is staged at 10%, so latest.json must still describe 1.4.0 —
        // the newest release every client may have. Eligible clients are
        // upgraded by the server from index.json, not from latest.json.
        let index = Index {
            releases: vec![zip_release("1.4.0", 100), zip_release("1.5.0", 10)],
        };
        let bytes = derive_latest(&index, &coord(), "https://updates.example.com")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("1.4.0"), "{text}");
        assert!(!text.contains("1.5.0"), "{text}");
    }

    #[test]
    fn empty_index_derives_nothing() {
        let index = Index::default();
        assert!(
            derive_latest(&index, &coord(), "https://updates.example.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_release_with_no_zip_is_an_error() {
        let mut release = zip_release("1.5.0", 100);
        release.assets[0].kind = AssetKind::Dmg;
        let index = Index { releases: vec![release] };
        assert!(derive_latest(&index, &coord(), "https://updates.example.com").is_err());
    }
}
```

- [ ] **Step 2: Create the golden fixture**

```json
{"url":"https://updates.example.com/download/myapp/1.5.0?platform=darwin&arch=arm64&channel=stable&filename=myapp-1.5.0-darwin-arm64.zip","name":"1.5.0","notes":"Fixes a crash on launch.","pub_date":"2026-09-16T10:00:00Z"}
```

Save exactly that single line as `crates/core/tests/fixtures/latest-darwin-arm64.json`, with one trailing newline.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p tanukistore-core derive`
Expected: FAIL — `derive` module does not exist.

- [ ] **Step 4: Write the implementation**

```rust
// crates/core/src/derive.rs  (above the test module)
use serde::Serialize;

use crate::keys::Coordinate;
use crate::model::{AssetKind, Index, Release};
use crate::resolve;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DeriveError {
    #[error("release {version} has no {kind} asset")]
    NoAsset { version: String, kind: &'static str },
}

/// The Squirrel.Mac manifest. Field declaration order IS the serialized
/// order, and the golden fixture asserts on it, so do not reorder.
#[derive(Debug, Serialize)]
pub struct LatestManifest {
    pub url: String,
    pub name: String,
    pub notes: String,
    pub pub_date: String,
}

/// Derive `latest.json` for a coordinate, or `Ok(None)` when no release is
/// available to every client.
///
/// `rollout_pct` is deliberately not honoured per-client here: `latest.json`
/// is one stored object shared by the whole fleet, so it can only describe a
/// release at 100%. Per-client rollout filtering happens in the server
/// handler against `index.json`.
///
/// `url` points at tanukistore's own download route rather than a presigned
/// MinIO URL (spec 4.7), which is what keeps these bytes static and lets the
/// server return them without re-serializing.
pub fn derive_latest(
    index: &Index,
    coord: &Coordinate,
    base_url: &str,
) -> Result<Option<Vec<u8>>, DeriveError> {
    let Some(release) = resolve::resolve_eligible(index, &coord.app, &coord.channel, None) else {
        return Ok(None);
    };
    let asset = pick(release, AssetKind::Zip, "zip")?;
    let base = base_url.trim_end_matches('/');
    let manifest = LatestManifest {
        url: format!(
            "{base}/download/{app}/{version}?platform={platform}&arch={arch}&channel={channel}&filename={filename}",
            app = coord.app,
            version = release.version,
            platform = coord.platform,
            arch = coord.arch,
            channel = coord.channel,
            filename = asset.filename,
        ),
        name: release.version.to_string(),
        notes: release.notes.clone(),
        pub_date: release
            .pub_date
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    };
    Ok(Some(serde_json::to_vec(&manifest).expect(
        "LatestManifest is plain owned data and cannot fail to serialize",
    )))
}

fn pick<'a>(
    release: &'a Release,
    kind: AssetKind,
    label: &'static str,
) -> Result<&'a crate::model::Asset, DeriveError> {
    release
        .assets
        .iter()
        .find(|asset| asset.kind == kind)
        .ok_or_else(|| DeriveError::NoAsset {
            version: release.version.to_string(),
            kind: label,
        })
}
```

```rust
// crates/core/src/lib.rs  (append)
pub mod derive;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p tanukistore-core derive`
Expected: PASS, 7 tests. If `matches_the_golden_fixture_byte_for_byte` fails on a whitespace or ordering difference, correct the **fixture** to the emitted bytes — but only after confirming the emitted JSON has exactly the four keys `url`, `name`, `notes`, `pub_date` in that order.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/derive.rs crates/core/src/lib.rs crates/core/tests/fixtures/
git commit -m "feat(core): derive Squirrel.Mac latest.json manifest"
```

---

### Task 7: Derive the Squirrel.Windows manifest

**Files:**
- Modify: `crates/core/src/derive.rs`
- Create: `crates/core/tests/fixtures/RELEASES-win32-x64`

**Interfaces:**
- Consumes: everything from Task 6.
- Produces: `fn derive_releases(index: &Index) -> Result<Vec<u8>, DeriveError>` — takes no coordinate, because relative filenames come from each asset and need no key context.

**Why this walks the whole index while Task 6 takes only the newest:** Squirrel.Windows performs its own version comparison against `RELEASES` and needs a line for whatever version the client happens to be running. Emitting only the newest produces a feed that works for recent clients and silently fails for older ones — the worst failure shape for an installed fleet.

**Why filenames are relative and take no `base_url`:** classic `RELEASES` lines are `{SHA1} {filename} {size}`, resolved by the client against the feed base URL. That is the most widely compatible form and is why Squirrel.Windows works against static hosting. The server serves those filenames beneath the feed path (spec §6).

- [ ] **Step 1: Write the failing test**

```rust
// crates/core/src/derive.rs  (add inside the existing `mod tests`)
    fn nupkg_release(version: &str, sha1: &str, size: u64) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            notes: String::new(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap(),
            rollout_pct: RolloutPct::FULL,
            assets: vec![Asset {
                kind: AssetKind::Nupkg,
                filename: format!("myapp-{version}-full.nupkg"),
                sha1: sha1.to_owned(),
                sha512: "cf83e1357eefb8bd".to_owned(),
                size_bytes: size,
            }],
        }
    }

    #[test]
    fn releases_matches_the_golden_fixture_byte_for_byte() {
        let index = Index {
            releases: vec![
                nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 94_371_840),
                nupkg_release("1.4.0", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", 93_323_264),
            ],
        };
        let bytes = derive_releases(&index).unwrap();
        let expected = include_str!("../tests/fixtures/RELEASES-win32-x64");
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn releases_lists_the_whole_history_ascending() {
        let index = Index {
            releases: vec![
                nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 3),
                nupkg_release("1.4.0", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d", 2),
                nupkg_release("1.3.0", "0beec7b5ea3f0fdbc95d0dd47f3c5bc275da8a33", 1),
            ],
        };
        let text = String::from_utf8(derive_releases(&index).unwrap()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "every version must appear: {text}");
        assert!(lines[0].contains("1.3.0"), "ascending order: {text}");
        assert!(lines[2].contains("1.5.0"), "ascending order: {text}");
    }

    #[test]
    fn releases_includes_partial_rollout_entries() {
        // Squirrel.Windows does its own comparison, so withholding a line
        // cannot implement a rollout. The line must be present.
        let mut staged = nupkg_release("1.5.0", "b858cb282617fb0956d960215c8e84d1ccf909c6", 3);
        staged.rollout_pct = RolloutPct::new(10).unwrap();
        let index = Index { releases: vec![staged] };
        let text = String::from_utf8(derive_releases(&index).unwrap()).unwrap();
        assert!(text.contains("1.5.0"), "{text}");
    }

    #[test]
    fn releases_is_empty_for_an_empty_index() {
        let index = Index::default();
        assert!(derive_releases(&index).unwrap().is_empty());
    }

    #[test]
    fn releases_errors_when_a_release_has_no_nupkg() {
        let index = Index { releases: vec![zip_release("1.5.0", 100)] };
        assert!(derive_releases(&index).is_err());
    }
```

- [ ] **Step 2: Create the golden fixture**

Save as `crates/core/tests/fixtures/RELEASES-win32-x64`, exactly two lines each ending in `\n`:

```
aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d myapp-1.4.0-full.nupkg 93323264
b858cb282617fb0956d960215c8e84d1ccf909c6 myapp-1.5.0-full.nupkg 94371840
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p tanukistore-core derive_releases`
Expected: FAIL — `derive_releases` not found.

- [ ] **Step 4: Write the implementation**

```rust
// crates/core/src/derive.rs  (append above the test module)
/// Derive the Squirrel.Windows `RELEASES` manifest: one
/// `{sha1} {filename} {size}` line per full nupkg, ascending by version.
///
/// Unlike `derive_latest`, this walks the ENTIRE index. Squirrel.Windows
/// compares versions itself and needs a line for whatever version the client
/// is currently running; emitting only the newest yields a feed that works
/// for recent clients and silently fails for older ones.
///
/// Rollout percentages are ignored for the same reason — withholding a line
/// cannot stage a rollout on Windows, it only breaks clients on that version.
/// Windows rollout staging is out of scope for v1.
///
/// Filenames are relative, resolved by the client against the feed base URL.
pub fn derive_releases(index: &Index) -> Result<Vec<u8>, DeriveError> {
    let mut releases: Vec<&Release> = index.releases.iter().collect();
    releases.sort_by(|a, b| a.version.cmp(&b.version));

    let mut out = String::new();
    for release in releases {
        let asset = pick(release, AssetKind::Nupkg, "nupkg")?;
        out.push_str(&format!(
            "{} {} {}\n",
            asset.sha1, asset.filename, asset.size_bytes
        ));
    }
    Ok(out.into_bytes())
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p tanukistore-core`
Expected: PASS, all tests across the crate (about 29).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/derive.rs crates/core/tests/fixtures/
git commit -m "feat(core): derive Squirrel.Windows RELEASES manifest"
```

---

### Task 8: Verify both manifests against a real Squirrel client

**Files:**
- Create: `docs/protocol-verification.md`
- Modify: `crates/core/tests/fixtures/latest-darwin-arm64.json` (only if verification demands it)
- Modify: `crates/core/tests/fixtures/RELEASES-win32-x64` (only if verification demands it)

**Interfaces:**
- Consumes: `derive_latest` and `derive_releases` from Tasks 6 and 7.
- Produces: no code. It converts the fixtures from *our belief about the protocol* into *bytes a real client accepted*.

**Why this task exists and cannot be automated:** every preceding test asserts that `derive()` matches a fixture we wrote ourselves. If our reading of the Squirrel wire format is wrong, those tests pass and the fleet still never updates. Only a real Squirrel client can falsify the fixtures, and its failure mode is silent — which is exactly why this gate is explicit rather than assumed. **This task requires a human with a signed Electron app; an agentic worker must stop here and hand back.**

- [ ] **Step 1: Serve the derived manifests statically**

```bash
mkdir -p /tmp/tanuki-verify/update/myapp/darwin/arm64/1.0.0
mkdir -p /tmp/tanuki-verify/update/myapp/win32/x64
# Write the derive_latest output to .../darwin/arm64/1.0.0/index.json
# and the derive_releases output to .../win32/x64/RELEASES,
# with the real asset files alongside and base_url http://localhost:8000
cd /tmp/tanuki-verify && python3 -m http.server 8000
```

- [ ] **Step 2: Point a real macOS build at it**

In the Electron app: `autoUpdater.setFeedURL({ url: "http://localhost:8000/update/myapp/darwin/arm64/1.0.0" })`, then `autoUpdater.checkForUpdates()`. Build and run a version *older* than the manifest's.

Expected: `update-available` fires, the `.zip` downloads, and `update-downloaded` fires.
Record: whether `pub_date` was accepted in the emitted format, and whether the `url` redirect chain was followed.

- [ ] **Step 3: Point a real Windows build at it**

Set the Squirrel.Windows feed URL to `http://localhost:8000/update/myapp/win32/x64`, so the client fetches `.../RELEASES` itself. Install an older version first, then trigger the update check.

Expected: the nupkg named in `RELEASES` downloads and applies.
Record: whether relative filenames resolved correctly, and whether the SHA-1 check passed.

- [ ] **Step 4: Reconcile the fixtures**

If a client rejected anything, change the **fixture and the implementation** to match what the client accepted — never the reverse. Re-run `cargo test -p tanukistore-core` until green against the corrected fixtures.

- [ ] **Step 5: Record the verification**

Write `docs/protocol-verification.md` capturing: date, Electron version, Squirrel.Mac and Squirrel.Windows versions, OS versions, the exact bytes served, and the observed client behaviour for each platform. Later plans treat these fixtures as authoritative, so this file is the evidence for that claim.

- [ ] **Step 6: Commit**

```bash
git add docs/protocol-verification.md crates/core/tests/fixtures/
git commit -m "test(core): verify derived manifests against real Squirrel clients"
```

---

## Roadmap

This plan is Plan 1 of 6. The remaining plans each get their own document, written when their predecessor lands:

2. **Storage + resilience** — `ObjectStore` trait, `InMemoryStore`, `S3CompatibleStore`, `CircuitBreakerStore`, bulkhead semaphore, `ManifestCache` with single-flight, freshness window, and stale-on-error. (Spec milestones 3–4.)
3. **Server read path** — axum, both feeds, the nupkg-under-feed route, downloads, notes, and the admin listener. (Milestone 5.)
4. **Publisher CLI** — `release`, `advance-rollout`, `verify`, conditional writes with bounded retry. (Milestone 6.)
5. **Telemetry** — tracing, OTel metrics, `/metrics`, cardinality enforcement, `NoopSink`. (Milestone 8.)
6. **Events + integration** — `JetStreamSink`, bounded MPSC, NATS invalidation, testcontainers suites, load baseline. (Milestones 7, 9–11.)
