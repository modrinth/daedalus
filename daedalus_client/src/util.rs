use crate::Error;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use s3::creds::Credentials;
use s3::{Bucket, Region};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tokio::sync::Semaphore;

lazy_static::lazy_static! {
    static ref BUCKET : Bucket = {
        let region = dotenvy::var("S3_REGION").unwrap();
        let b = Bucket::new(
            &dotenvy::var("S3_BUCKET_NAME").unwrap(),
            if &*region == "r2" {
                Region::R2 {
                    account_id: dotenvy::var("S3_URL").unwrap(),
                }
            } else {
                Region::Custom {
                    region: region.clone(),
                    endpoint: dotenvy::var("S3_URL").unwrap(),
                }
            },
            Credentials::new(
                Some(&*dotenvy::var("S3_ACCESS_TOKEN").unwrap()),
                Some(&*dotenvy::var("S3_SECRET").unwrap()),
                None,
                None,
                None,
            ).unwrap(),
        ).unwrap();

        if region == "path-style" {
            b.with_path_style()
        } else {
            b
        }
    };
}

lazy_static::lazy_static! {
    static ref REQWEST_CLIENT: reqwest::Client = {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(header) = reqwest::header::HeaderValue::from_str(&format!(
            "modrinth/daedalus/{} (support@modrinth.com)",
            env!("CARGO_PKG_VERSION")
        )) {
            headers.insert(reqwest::header::USER_AGENT, header);
        }

        let client = reqwest::Client::builder()
            .tcp_keepalive(Some(std::time::Duration::from_secs(10)))
            .timeout(std::time::Duration::from_secs(15))
            .default_headers(headers)
            .build()
            .unwrap();

        client
    };
}

#[tracing::instrument(skip(bytes, semaphore))]
pub async fn upload_file_to_bucket(
    path: String,
    bytes: Bytes,
    content_type: Option<String>,
    semaphore: &Arc<Semaphore>,
) -> Result<(), Error> {
    let _permit = semaphore.acquire().await?;
    let key = path.clone();

    for attempt in 1..=4 {
        tracing::trace!("Attempting file upload, attempt {attempt}");
        let result = if let Some(ref content_type) = content_type {
            BUCKET
                .put_object_with_content_type(key.clone(), &bytes, content_type)
                .await
        } else {
            BUCKET.put_object(key.clone(), &bytes).await
        }
        .map_err(|err| Error::S3 {
            inner: err,
            file: path.clone(),
        });

        match result {
            Ok(_) => return Ok(()),
            Err(_) if attempt <= 3 => continue,
            Err(_) => {
                result?;
            }
        }
    }
    unreachable!()
}

pub async fn upload_url_to_bucket_mirrors(
    base: String,
    mirrors: Vec<String>,
    semaphore: &Arc<Semaphore>,
) -> Result<(), Error> {
    if mirrors.is_empty() {
        return Err(Error::Daedalus(daedalus::Error::ParseError(
            "No mirrors provided!".to_string(),
        )));
    }

    for (index, mirror) in mirrors.iter().enumerate() {
        let result = upload_url_to_bucket(
            &base,
            &format!("{}{}", mirror, base),
            semaphore,
        )
        .await;

        if result.is_ok() || (result.is_err() && index == (mirrors.len() - 1)) {
            return result;
        }
    }

    unreachable!()
}

#[tracing::instrument(skip(semaphore))]
pub async fn upload_url_to_bucket(
    path: &str,
    url: &str,
    semaphore: &Arc<Semaphore>,
) -> Result<(), Error> {
    let _permit = semaphore.acquire().await?;

    for attempt in 1..=4 {
        tracing::trace!("Attempting streaming file upload, attempt {attempt}");

        let result: Result<(), Error> = {
            let response =
                REQWEST_CLIENT.get(url).send().await.map_err(|err| {
                    Error::Fetch {
                        inner: err,
                        item: url.to_string(),
                    }
                })?;

            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .map(|ct| ct.to_str().ok())
                .flatten()
                .unwrap_or("application/octet-stream")
                .to_string();

            let total_size = response.content_length().unwrap_or(0);

            const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

            if total_size < MIN_PART_SIZE as u64 {
                let data =
                    response.bytes().await.map_err(|err| Error::Fetch {
                        inner: err,
                        item: url.to_string(),
                    })?;
                BUCKET.put_object(&path, &data).await.map_err(|err| {
                    Error::S3 {
                        inner: err,
                        file: path.to_string(),
                    }
                })?;
            } else {
                let mut stream = response.bytes_stream();

                let multipart = BUCKET
                    .initiate_multipart_upload(&path, &content_type)
                    .await
                    .map_err(|err| Error::S3 {
                        inner: err,
                        file: path.to_string(),
                    })?;

                let mut parts = Vec::new();
                let mut buffer = BytesMut::new();

                async fn upload_part(
                    parts: &mut Vec<s3::serde_types::Part>,
                    buffer: Vec<u8>,
                    path: &str,
                    upload_id: &str,
                    content_type: &str,
                ) -> Result<(), Error> {
                    let part = BUCKET
                        .put_multipart_chunk(
                            buffer,
                            path,
                            (parts.len() + 1) as u32,
                            upload_id,
                            content_type,
                        )
                        .await
                        .map_err(|err| Error::S3 {
                            inner: err,
                            file: path.to_string(),
                        })?;

                    parts.push(part);

                    Ok(())
                }

                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|err| Error::Fetch {
                        inner: err,
                        item: url.to_string(),
                    })?;

                    buffer.extend_from_slice(&chunk);

                    if buffer.len() >= MIN_PART_SIZE {
                        upload_part(
                            &mut parts,
                            buffer.to_vec(),
                            &path,
                            &multipart.upload_id,
                            &content_type,
                        )
                        .await?;
                        buffer.clear();
                    }
                }

                if !buffer.is_empty() {
                    let part = BUCKET
                        .put_multipart_chunk(
                            buffer.to_vec(),
                            &path,
                            (parts.len() + 1) as u32,
                            &multipart.upload_id,
                            &content_type,
                        )
                        .await
                        .map_err(|err| Error::S3 {
                            inner: err,
                            file: path.to_string(),
                        })?;

                    parts.push(part);
                }

                BUCKET
                    .complete_multipart_upload(
                        &path,
                        &multipart.upload_id,
                        parts,
                    )
                    .await
                    .map_err(|err| Error::S3 {
                        inner: err,
                        file: path.to_string(),
                    })?;
            }

            Ok(())
        };

        match result {
            Ok(_) => return Ok(()),
            Err(_) if attempt <= 3 => continue,
            Err(_) => {
                result?;
            }
        }
    }
    unreachable!()
}

#[tracing::instrument(skip(bytes))]
pub async fn sha1_async(bytes: Bytes) -> Result<String, Error> {
    let hash = tokio::task::spawn_blocking(move || {
        sha1_smol::Sha1::from(bytes).hexdigest()
    })
    .await?;

    Ok(hash)
}

#[tracing::instrument(skip(semaphore))]
pub async fn download_file(
    url: &str,
    sha1: Option<&str>,
    semaphore: &Arc<Semaphore>,
) -> Result<bytes::Bytes, crate::Error> {
    let _permit = semaphore.acquire().await?;
    tracing::trace!("Starting file download");

    for attempt in 1..=4 {
        let result = REQWEST_CLIENT.get(url).send().await;

        match result {
            Ok(x) => {
                let bytes = x.bytes().await;

                if let Ok(bytes) = bytes {
                    if let Some(sha1) = sha1 {
                        if &*sha1_async(bytes.clone()).await? != sha1 {
                            if attempt <= 3 {
                                continue;
                            } else {
                                return Err(crate::Error::ChecksumFailure {
                                    hash: sha1.to_string(),
                                    url: url.to_string(),
                                    tries: attempt,
                                });
                            }
                        }
                    }

                    return Ok(bytes);
                } else if attempt <= 3 {
                    continue;
                } else if let Err(err) = bytes {
                    return Err(crate::Error::Fetch {
                        inner: err,
                        item: url.to_string(),
                    });
                }
            }
            Err(_) if attempt <= 3 => continue,
            Err(err) => {
                return Err(crate::Error::Fetch {
                    inner: err,
                    item: url.to_string(),
                })
            }
        }
    }

    unreachable!()
}

pub async fn fetch_json<T: DeserializeOwned>(
    url: &str,
    semaphore: &Arc<Semaphore>,
) -> Result<T, Error> {
    Ok(serde_json::from_slice(
        &download_file(url, None, semaphore).await?,
    )?)
}

pub fn format_url(path: &str) -> String {
    format!("{}/{}", &*dotenvy::var("BASE_URL").unwrap(), path)
}
