use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use semver::Version;
use tanukistore_core::model::RolloutPct;
use tanukistore_core::store::{S3Config, S3Store};
use tanukistore_publish::{ReleaseRequest, Target, release};

/// Publishes Electron releases into tanukistore's bucket (spec 8.1).
///
/// Holds WRITE credentials. The server holds read-only ones (spec 7), so this
/// is the only binary that can change what clients are offered.
#[derive(Parser)]
#[command(name = "tanukistore-publish", version)]
struct Cli {
    #[command(flatten)]
    s3: S3Args,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct S3Args {
    #[arg(long, env = "S3_ENDPOINT")]
    s3_endpoint: String,
    #[arg(long, env = "S3_BUCKET")]
    s3_bucket: String,
    #[arg(long, env = "S3_REGION", default_value = "us-east-1")]
    s3_region: String,
    #[arg(long, env = "S3_ACCESS_KEY", hide_env_values = true)]
    s3_access_key: String,
    #[arg(long, env = "S3_SECRET_KEY", hide_env_values = true)]
    s3_secret_key: String,
}

#[derive(Subcommand)]
enum Command {
    /// Upload assets and commit a new release for one or more targets.
    Release {
        #[arg(long)]
        app: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        version: Version,
        #[arg(long, default_value_t = 100)]
        rollout_pct: u8,
        #[arg(long)]
        notes_file: Option<PathBuf>,
        /// `platform/arch=path`, repeatable.
        #[arg(long = "target", required = true)]
        targets: Vec<Target>,
        /// Public base URL embedded in latest.json (spec 4.7).
        #[arg(long, env = "TANUKI_BASE_URL")]
        base_url: String,
        /// Replace an existing version. For repairing a broken publish only.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let store = S3Store::new(S3Config {
        presign_endpoint: cli.s3.s3_endpoint.clone(),
        endpoint: cli.s3.s3_endpoint,
        bucket: cli.s3.s3_bucket,
        region: cli.s3.s3_region,
        access_key: cli.s3.s3_access_key,
        secret_key: cli.s3.s3_secret_key,
    });

    match cli.command {
        Command::Release {
            app,
            channel,
            version,
            rollout_pct,
            notes_file,
            targets,
            base_url,
            force,
            dry_run,
        } => {
            let notes = match notes_file {
                Some(path) => std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
                None => String::new(),
            };
            let request = ReleaseRequest {
                app,
                channel,
                version,
                rollout_pct: RolloutPct::new(rollout_pct)?,
                notes,
                pub_date: chrono::Utc::now(),
                targets,
                base_url,
                force,
                dry_run,
            };
            let report = release(&store, &request).await?;
            if !report.dry_run {
                println!(
                    "published {} to: {}",
                    request.version,
                    report.committed.join(", ")
                );
            }
        }
    }
    Ok(())
}
