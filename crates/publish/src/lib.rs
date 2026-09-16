//! The write side (spec 8, 8.1). Everything here is generic over
//! `ObjectStore`, so the ordering and concurrency rules are tested against
//! `InMemoryStore`.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use rand::Rng;
use semver::Version;
use sha1::Sha1;
use sha2::{Digest, Sha512};
use tanukistore_core::derive::{derive_latest, derive_releases, validate_base_url};
use tanukistore_core::keys::{Coordinate, config_key};
use tanukistore_core::model::{
    AppConfig, Arch, Asset, AssetKind, Index, Platform, Release, RolloutPct,
};
use tanukistore_core::store::{ObjectStore, PutCondition, StoreError};

/// Spec 8: bounded, then fail loudly.
const MAX_COMMIT_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone)]
pub struct Target {
    pub platform: Platform,
    pub arch: Arch,
    pub path: PathBuf,
}

impl std::str::FromStr for Target {
    type Err = anyhow::Error;

    /// `darwin/arm64=dist/app.zip`
    fn from_str(s: &str) -> Result<Self> {
        let (coord, path) = s
            .split_once('=')
            .context("target must look like platform/arch=path")?;
        let (platform, arch) = coord
            .split_once('/')
            .context("target must look like platform/arch=path")?;
        Ok(Target {
            platform: platform.parse()?,
            arch: arch.parse()?,
            path: PathBuf::from(path),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseRequest {
    pub app: String,
    pub channel: String,
    pub version: Version,
    pub rollout_pct: RolloutPct,
    pub notes: String,
    pub pub_date: DateTime<Utc>,
    pub targets: Vec<Target>,
    pub base_url: String,
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReleaseReport {
    pub committed: Vec<String>,
    pub dry_run: bool,
}

struct Prepared {
    target: Target,
    coord: Coordinate,
    asset: Asset,
}

pub async fn release<S: ObjectStore>(store: &S, request: &ReleaseRequest) -> Result<ReleaseReport> {
    validate_base_url(&request.base_url)?;
    let prepared = prepare(request)?;

    // Immutability is checked BEFORE any byte is uploaded. Checking only at
    // commit time would already have overwritten the published asset, changing
    // the bytes behind a SHA-1 that clients have fetched (spec 8.1).
    for p in &prepared {
        let (index, _) = read_index(store, &p.coord).await?;
        if !request.force && index.releases.iter().any(|r| r.version == request.version) {
            bail!(
                "{} already has version {} - versions are immutable; pass --force only to repair a broken publish",
                p.coord.prefix(),
                request.version
            );
        }
        // Derive against the would-be index now, so a release the manifests
        // cannot represent fails before anything is written.
        let candidate = with_release(index, release_entry(request, &p.asset), request.force);
        derive_for(&candidate, &p.coord, &request.base_url)?;
    }

    if request.dry_run {
        for p in &prepared {
            println!(
                "would publish {} -> {} ({} bytes, sha1 {})",
                p.target.path.display(),
                p.coord.asset_key(&request.version, &p.asset.filename),
                p.asset.size_bytes,
                p.asset.sha1
            );
        }
        return Ok(ReleaseReport {
            committed: Vec::new(),
            dry_run: true,
        });
    }

    ensure_channel(store, &request.app, &request.channel).await?;

    // Every asset lands before any manifest is committed, so a failed upload
    // publishes nothing (spec 8.1).
    for p in &prepared {
        let key = p.coord.asset_key(&request.version, &p.asset.filename);
        tracing::info!(key, bytes = p.asset.size_bytes, "uploading asset");
        store
            .put_file(&key, &p.target.path, content_type(p.asset.kind))
            .await
            .with_context(|| format!("uploading {key}"))?;
        let stored = store.head(&key).await?;
        if stored != p.asset.size_bytes {
            bail!(
                "{key}: uploaded {stored} bytes, expected {}",
                p.asset.size_bytes
            );
        }
    }

    let mut report = ReleaseReport::default();
    for p in &prepared {
        if let Err(error) = commit(store, request, p).await {
            // Not transactional across targets: say exactly what is live.
            bail!(
                "{error:#}\ncommitted before the failure: [{}] - re-run with --force to finish",
                report.committed.join(", ")
            );
        }
        report.committed.push(p.coord.prefix());
    }
    Ok(report)
}

fn prepare(request: &ReleaseRequest) -> Result<Vec<Prepared>> {
    if request.targets.is_empty() {
        bail!("at least one --target is required");
    }
    let mut seen = HashSet::new();
    let mut prepared = Vec::new();
    for target in &request.targets {
        if !seen.insert((target.platform, target.arch)) {
            bail!("duplicate target {}/{}", target.platform, target.arch);
        }
        let coord = Coordinate::new(&request.app, &request.channel, target.platform, target.arch)?;
        let filename = target
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("{} has no usable file name", target.path.display()))?
            .to_owned();
        let kind = asset_kind(target.platform, &filename)?;
        if filename.chars().any(|c| c.is_whitespace() || c == '/') {
            bail!("{filename:?}: asset filenames must not contain whitespace");
        }
        // Squirrel.Windows cannot stage a rollout (derive.rs); refuse before
        // hashing 200MB rather than after.
        if target.platform == Platform::Win32 && request.rollout_pct.get() < 100 {
            bail!(
                "win32 cannot be published below 100% rollout - publish it at 100 or hold it back"
            );
        }
        let (sha1, sha512, size_bytes) = hash_file(&target.path)?;
        prepared.push(Prepared {
            target: target.clone(),
            coord,
            asset: Asset {
                kind,
                filename,
                sha1,
                sha512,
                size_bytes,
            },
        });
    }
    Ok(prepared)
}

fn asset_kind(platform: Platform, filename: &str) -> Result<AssetKind> {
    let lower = filename.to_ascii_lowercase();
    match platform {
        Platform::Darwin if lower.ends_with(".zip") => Ok(AssetKind::Zip),
        Platform::Win32 if lower.ends_with(".nupkg") => Ok(AssetKind::Nupkg),
        Platform::Darwin => bail!("{filename}: darwin targets must be the Squirrel.Mac .zip"),
        Platform::Win32 => bail!("{filename}: win32 targets must be the full .nupkg"),
    }
}

fn content_type(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Zip => "application/zip",
        _ => "application/octet-stream",
    }
}

/// SHA-1 exists only for the RELEASES format; SHA-512 is the real integrity
/// hash (spec 5). Streamed, so a 200MB bundle is never held in memory.
pub fn hash_file(path: &Path) -> Result<(String, String, u64)> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut sha1 = Sha1::new();
    let mut sha512 = Sha512::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sha1.update(&buffer[..read]);
        sha512.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((
        hex::encode(sha1.finalize()),
        hex::encode(sha512.finalize()),
        size,
    ))
}

fn release_entry(request: &ReleaseRequest, asset: &Asset) -> Release {
    Release {
        version: request.version.clone(),
        notes: request.notes.clone(),
        pub_date: request.pub_date,
        rollout_pct: request.rollout_pct,
        assets: vec![asset.clone()],
    }
}

fn with_release(mut index: Index, entry: Release, replace: bool) -> Index {
    if replace {
        index.releases.retain(|r| r.version != entry.version);
    }
    index.releases.push(entry);
    index
}

/// Each platform reads exactly one derived manifest, so only that one is
/// derived: RELEASES for a darwin index would demand a nupkg it never has.
fn derive_for(
    index: &Index,
    coord: &Coordinate,
    base_url: &str,
) -> Result<(String, Bytes, &'static str)> {
    Ok(match coord.platform() {
        Platform::Darwin => {
            let bytes = derive_latest(index, coord, base_url)?
                .context("no release at 100% yet, so latest.json would be empty")
                .map(Bytes::from);
            (coord.latest_key(), bytes?, "application/json")
        }
        Platform::Win32 => (
            coord.releases_key(),
            Bytes::from(derive_releases(index)?),
            "text/plain; charset=utf-8",
        ),
    })
}

async fn read_index<S: ObjectStore>(
    store: &S,
    coord: &Coordinate,
) -> Result<(Index, PutCondition)> {
    match store.get(&coord.index_key()).await {
        Ok(object) => {
            let index = serde_json::from_slice(&object.bytes)
                .with_context(|| format!("{} is corrupt", coord.index_key()))?;
            let condition = match object.etag {
                Some(etag) => PutCondition::IfMatch(etag),
                None => bail!(
                    "{}: backend returned no ETag, cannot write safely",
                    coord.index_key()
                ),
            };
            Ok((index, condition))
        }
        Err(StoreError::NotFound(_)) => Ok((Index::default(), PutCondition::IfNoneMatch)),
        Err(other) => Err(other.into()),
    }
}

async fn commit<S: ObjectStore>(store: &S, request: &ReleaseRequest, p: &Prepared) -> Result<()> {
    let key = p.coord.index_key();
    for attempt in 1..=MAX_COMMIT_ATTEMPTS {
        let (index, condition) = read_index(store, &p.coord).await?;
        if !request.force && index.releases.iter().any(|r| r.version == request.version) {
            bail!(
                "{key}: version {} was published concurrently",
                request.version
            );
        }
        let index = with_release(index, release_entry(request, &p.asset), request.force);
        let (manifest_key, manifest, manifest_type) =
            derive_for(&index, &p.coord, &request.base_url)?;
        let body = Bytes::from(serde_json::to_vec_pretty(&index)?);
        match store.put(&key, body, "application/json", condition).await {
            Ok(_) => {
                // The derived manifest is the commit point clients observe: it
                // lands last, after both the asset and the index exist.
                store
                    .put(&manifest_key, manifest, manifest_type, PutCondition::None)
                    .await
                    .with_context(|| format!("writing {manifest_key}"))?;
                tracing::info!(key, manifest_key, "committed");
                return Ok(());
            }
            Err(StoreError::PreconditionFailed(_)) if attempt < MAX_COMMIT_ATTEMPTS => {
                let backoff = backoff(attempt);
                tracing::warn!(
                    key,
                    attempt,
                    ?backoff,
                    "index changed underneath us, retrying"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(error) => return Err(error).with_context(|| format!("writing {key}")),
        }
    }
    bail!("{key}: gave up after {MAX_COMMIT_ATTEMPTS} conflicting writes")
}

/// Exponential with full jitter, so publishers that collided do not collide
/// again in lockstep.
fn backoff(attempt: u32) -> Duration {
    let ceiling = 100u64 << attempt.min(6);
    Duration::from_millis(rand::rng().random_range(ceiling / 2..=ceiling))
}

async fn ensure_channel<S: ObjectStore>(store: &S, app: &str, channel: &str) -> Result<()> {
    let key = config_key(app)?;
    for attempt in 1..=MAX_COMMIT_ATTEMPTS {
        let (config, condition) = match store.get(&key).await {
            Ok(object) => {
                let config: AppConfig = serde_json::from_slice(&object.bytes)
                    .with_context(|| format!("{key} is corrupt"))?;
                if config.channels.iter().any(|c| c == channel) {
                    return Ok(());
                }
                let etag = object.etag.context("backend returned no ETag")?;
                let mut config = config;
                config.channels.push(channel.to_owned());
                (config, PutCondition::IfMatch(etag))
            }
            // The first channel an app publishes to becomes its default.
            Err(StoreError::NotFound(_)) => (
                AppConfig {
                    channels: vec![channel.to_owned()],
                    default_channel: channel.to_owned(),
                },
                PutCondition::IfNoneMatch,
            ),
            Err(other) => return Err(other.into()),
        };
        let body = Bytes::from(serde_json::to_vec_pretty(&config)?);
        match store.put(&key, body, "application/json", condition).await {
            Ok(_) => return Ok(()),
            Err(StoreError::PreconditionFailed(_)) => tokio::time::sleep(backoff(attempt)).await,
            Err(other) => return Err(other.into()),
        }
    }
    bail!("{key}: gave up after {MAX_COMMIT_ATTEMPTS} conflicting writes")
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use chrono::TimeZone;
    use tanukistore_core::store::InMemoryStore;

    use super::*;

    fn file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        path
    }

    fn request(dir: &tempfile::TempDir, version: &str, pct: u8) -> ReleaseRequest {
        ReleaseRequest {
            app: "myapp".into(),
            channel: "stable".into(),
            version: Version::parse(version).unwrap(),
            rollout_pct: RolloutPct::new(pct).unwrap(),
            notes: "notes".into(),
            pub_date: Utc.with_ymd_and_hms(2026, 9, 17, 0, 0, 0).unwrap(),
            targets: vec![
                format!(
                    "darwin/arm64={}",
                    file(dir, &format!("MyApp-{version}-mac.zip"), b"mac").display()
                )
                .parse()
                .unwrap(),
                format!(
                    "win32/x64={}",
                    file(dir, &format!("MyApp-{version}-full.nupkg"), b"win").display()
                )
                .parse()
                .unwrap(),
            ],
            base_url: "https://updates.example.com".into(),
            force: false,
            dry_run: false,
        }
    }

    fn mac() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Darwin, Arch::Arm64).unwrap()
    }

    fn win() -> Coordinate {
        Coordinate::new("myapp", "stable", Platform::Win32, Arch::X64).unwrap()
    }

    #[tokio::test]
    async fn a_release_writes_assets_indexes_manifests_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        let report = release(&store, &request(&dir, "1.0.0", 100)).await.unwrap();
        assert_eq!(report.committed, vec![mac().prefix(), win().prefix()]);

        let version = Version::new(1, 0, 0);
        assert!(store.contains(&mac().asset_key(&version, "MyApp-1.0.0-mac.zip")));
        assert!(store.contains(&win().asset_key(&version, "MyApp-1.0.0-full.nupkg")));
        assert!(store.contains(&mac().latest_key()));
        assert!(store.contains(&win().releases_key()));
        // Each platform gets only the manifest it reads.
        assert!(!store.contains(&mac().releases_key()));
        assert!(!store.contains(&win().latest_key()));

        let releases = store.get(&win().releases_key()).await.unwrap().bytes;
        // sha1("win")
        assert_eq!(
            std::str::from_utf8(&releases).unwrap(),
            "c76ac2e9cd86441713c7dfae92c451f3e6d17fdc MyApp-1.0.0-full.nupkg 3\n"
        );
        let config: AppConfig =
            serde_json::from_slice(&store.get("myapp/config.json").await.unwrap().bytes).unwrap();
        assert_eq!(config.default_channel, "stable");
    }

    #[tokio::test]
    async fn a_second_release_appends_and_republishing_is_refused_before_upload() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        release(&store, &request(&dir, "1.0.0", 100)).await.unwrap();
        release(&store, &request(&dir, "1.1.0", 100)).await.unwrap();

        let index: Index =
            serde_json::from_slice(&store.get(&mac().index_key()).await.unwrap().bytes).unwrap();
        assert_eq!(index.releases.len(), 2);

        // Replace 1.0.0's bytes on disk, then try to republish without --force.
        let mut again = request(&dir, "1.0.0", 100);
        std::fs::write(&again.targets[0].path, b"DIFFERENT").unwrap();
        let before = store
            .get(&mac().asset_key(&Version::new(1, 0, 0), "MyApp-1.0.0-mac.zip"))
            .await
            .unwrap()
            .bytes;
        assert!(release(&store, &again).await.is_err());
        let after = store
            .get(&mac().asset_key(&Version::new(1, 0, 0), "MyApp-1.0.0-mac.zip"))
            .await
            .unwrap()
            .bytes;
        assert_eq!(
            before, after,
            "a refused publish must not have touched the asset"
        );

        again.force = true;
        release(&store, &again).await.unwrap();
        let index: Index =
            serde_json::from_slice(&store.get(&mac().index_key()).await.unwrap().bytes).unwrap();
        assert_eq!(
            index.releases.len(),
            2,
            "--force replaces, it does not duplicate"
        );
    }

    #[tokio::test]
    async fn win32_below_100_percent_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        assert!(release(&store, &request(&dir, "1.0.0", 10)).await.is_err());
        assert!(!store.contains("myapp/config.json"));
        assert!(!store.contains(&mac().index_key()));
    }

    #[tokio::test]
    async fn a_darwin_only_staged_first_release_is_refused() {
        // latest.json can only describe a 100% release; with nothing at 100%
        // there is nothing to write, so this fails before upload.
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        let mut staged = request(&dir, "1.0.0", 10);
        staged.targets.truncate(1);
        assert!(release(&store, &staged).await.is_err());
        assert!(!store.contains(&mac().asset_key(&Version::new(1, 0, 0), "MyApp-1.0.0-mac.zip")));
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        let mut dry = request(&dir, "1.0.0", 100);
        dry.dry_run = true;
        assert!(release(&store, &dry).await.unwrap().dry_run);
        assert!(!store.contains("myapp/config.json"));
        assert!(!store.contains(&mac().index_key()));
    }

    #[tokio::test]
    async fn concurrent_publishers_both_land_via_conditional_retry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        let a = request(&dir, "1.0.0", 100);
        let b = request(&dir, "1.1.0", 100);
        let (ra, rb) = tokio::join!(release(&store, &a), release(&store, &b));
        ra.unwrap();
        rb.unwrap();
        let index: Index =
            serde_json::from_slice(&store.get(&win().index_key()).await.unwrap().bytes).unwrap();
        assert_eq!(
            index.releases.len(),
            2,
            "neither write may clobber the other"
        );
    }

    #[test]
    fn targets_parse_and_reject_wrong_asset_types() {
        let t: Target = "win32/x64=dist/App-1.0.0-full.nupkg".parse().unwrap();
        assert_eq!((t.platform, t.arch), (Platform::Win32, Arch::X64));
        assert!("darwin=x.zip".parse::<Target>().is_err());
        assert!(asset_kind(Platform::Darwin, "App.dmg").is_err());
        assert!(asset_kind(Platform::Win32, "App.exe").is_err());
    }
}
