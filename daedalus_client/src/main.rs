use crate::util::{
    upload_file_to_bucket, upload_url_to_bucket, upload_url_to_bucket_mirrors,
};
use daedalus::get_path_from_artifact;
use dashmap::{DashMap, DashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod fabric;
mod forge;
mod minecraft;
pub mod util;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Daedalus(#[from] daedalus::Error),
    #[error("Error while managing asynchronous tasks")]
    TaskError(#[from] tokio::task::JoinError),
    #[error("Error while deserializing JSON")]
    Serde(#[from] serde_json::Error),
    #[error("Failed to validate file checksum at url {url} with hash {hash} after {tries} tries")]
    ChecksumFailure {
        hash: String,
        url: String,
        tries: u32,
    },
    #[error("Unable to fetch {item}")]
    Fetch { inner: reqwest::Error, item: String },
    #[error("Error while uploading file to S3: {file}")]
    S3 {
        inner: s3::error::S3Error,
        file: String,
    },
    #[error("Error acquiring semaphore: {0}")]
    Acquire(#[from] tokio::sync::AcquireError),
    #[error("Tracing error: {0}")]
    Tracing(#[from] tracing::subscriber::SetGlobalDefaultError),
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    dotenvy::dotenv().ok();

    let subscriber = tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env());

    tracing::subscriber::set_global_default(subscriber)?;

    tracing::info!("Initialized tracing. Starting Daedalus!");

    if check_env_vars() {
        tracing::error!("Some environment variables are missing!");

        return Ok(());
    }

    // let mut timer = tokio::time::interval(Duration::from_secs(60 * 60));
    let semaphore = Arc::new(Semaphore::new(10));

    // loop {
        // path, upload file
        let upload_files: DashMap<String, UploadFile> = DashMap::new();
        // path, mirror artifact
        let mirror_artifacts: DashMap<String, MirrorArtifact> = DashMap::new();

        minecraft::fetch(semaphore.clone(), &upload_files, &mirror_artifacts)
            .await?;
        fabric::fetch_fabric(
            semaphore.clone(),
            &upload_files,
            &mirror_artifacts,
        )
        .await?;
        fabric::fetch_quilt(
            semaphore.clone(),
            &upload_files,
            &mirror_artifacts,
        )
        .await?;

        futures::future::try_join_all(upload_files.iter().map(|x| {
            upload_file_to_bucket(
                x.key().clone(),
                x.value().file.clone(),
                x.value().content_type.clone(),
                &semaphore,
            )
        }))
        .await?;

        futures::future::try_join_all(mirror_artifacts.iter().map(|x| {
            upload_url_to_bucket_mirrors(
                format!("maven/{}", x.key()),
                x.value().mirrors.iter().map(|x| x.key().clone()).collect(),
                &semaphore,
            )
        }))
        .await?;

    Ok(())

        // TODO: clear cloudflare cache here: https://developers.cloudflare.com/api/operations/zone-purge#purge-cached-content-by-url

        // timer.tick().await;
    // }
}

pub struct UploadFile {
    file: bytes::Bytes,
    content_type: Option<String>,
}

pub struct MirrorArtifact {
    pub mirrors: DashSet<String>,
}

pub fn insert_mirrored_artifact(
    artifact: &str,
    mirror: String,
    mirror_artifacts: &DashMap<String, MirrorArtifact>,
) -> Result<(), Error> {
    mirror_artifacts
        .entry(get_path_from_artifact(&artifact)?)
        .or_insert(MirrorArtifact {
            mirrors: DashSet::new(),
        })
        .mirrors
        .insert(mirror);

    Ok(())
}

fn check_env_vars() -> bool {
    let mut failed = false;

    fn check_var<T: std::str::FromStr>(var: &str) -> bool {
        if dotenvy::var(var)
            .ok()
            .and_then(|s| s.parse::<T>().ok())
            .is_none()
        {
            tracing::warn!(
                "Variable `{}` missing in dotenvy or not of type `{}`",
                var,
                std::any::type_name::<T>()
            );
            true
        } else {
            false
        }
    }

    failed |= check_var::<String>("BASE_URL");

    failed |= check_var::<String>("S3_ACCESS_TOKEN");
    failed |= check_var::<String>("S3_SECRET");
    failed |= check_var::<String>("S3_URL");
    failed |= check_var::<String>("S3_REGION");
    failed |= check_var::<String>("S3_BUCKET_NAME");

    failed
}
