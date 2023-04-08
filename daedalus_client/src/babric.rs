use std::sync::Arc;
use tokio::sync::Semaphore;
use daedalus::modded::PartialVersionInfo;
use crate::fabric::{FabricVersions, fetch_fabric_like_version, fetch_fabric_like_versions};
use crate::{download_file, format_url, upload_file_to_bucket, Error};
use daedalus::minecraft::{Library, VersionManifest};
use daedalus::modded::{
    LoaderVersion, Manifest, Version, DUMMY_REPLACE_STRING,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

const BABRIC_META_URL: &str = "https://meta.babric.glass-launcher.net/v2";

pub async fn retrieve_data(
    minecraft_versions: &VersionManifest,
    uploaded_files: &mut Vec<String>,
    semaphore: Arc<Semaphore>,
) -> Result<(), Error> {
    let mut list = fetch_babric_versions(None, semaphore.clone()).await?;
    let old_manifest = daedalus::modded::fetch_manifest(&format_url(&format!(
        "babric/v{}/manifest.json",
        daedalus::modded::CURRENT_BABRIC_FORMAT_VERSION,
    )))
        .await
        .ok();

    let mut versions = if let Some(old_manifest) = old_manifest {
        old_manifest.game_versions
    } else {
        Vec::new()
    };

    let loaders_mutex = RwLock::new(Vec::new());

    {
        let mut loaders = loaders_mutex.write().await;

        for loader in &list.loader {
            loaders.push((Box::new(loader.stable), loader.version.clone()))
        }

        list.loader
            .retain(|x| loaders.iter().any(|val| val.1 == x.version))
    }

    const DUMMY_GAME_VERSION: &str = "b1.7.3";

    let loader_version_mutex = Mutex::new(Vec::new());
    let uploaded_files_mutex = Arc::new(Mutex::new(Vec::new()));

    let loader_versions = futures::future::try_join_all(
        loaders_mutex.read().await.clone().into_iter().map(
            |(stable, loader)| async {
                {
                    if versions.iter().any(|x| {
                        x.id == DUMMY_REPLACE_STRING
                            && x.loaders.iter().any(|x| x.id == loader)
                    }) {
                        return Ok(None);
                    }
                }

                let version = fetch_babric_version(
                    DUMMY_GAME_VERSION,
                    &loader,
                    semaphore.clone(),
                )
                    .await?;

                Ok::<Option<(Box<bool>, String, PartialVersionInfo)>, Error>(
                    Some((stable, loader, version)),
                )
            },
        ),
    )
        .await?;

    let visited_artifacts_mutex = Arc::new(Mutex::new(Vec::new()));
    futures::future::try_join_all(loader_versions.into_iter()
        .flatten().map(
        |(stable, loader, version)| async {
            let libs = futures::future::try_join_all(
                version.libraries.into_iter().map(|mut lib| async {
                    {
                        let mut visited_assets =
                            visited_artifacts_mutex.lock().await;

                        if visited_assets.contains(&lib.name) {
                            lib.name = lib.name.replace(DUMMY_GAME_VERSION, DUMMY_REPLACE_STRING);
                            lib.url = Some(format_url("maven/"));

                            return Ok(lib);
                        } else {
                            visited_assets.push(lib.name.clone())
                        }
                    }

                    if lib.name.contains(DUMMY_GAME_VERSION) {
                        lib.name = lib.name.replace(DUMMY_GAME_VERSION, DUMMY_REPLACE_STRING);
                        futures::future::try_join_all(list.game.clone().into_iter().map(|game_version| async {
                            let semaphore = semaphore.clone();
                            let uploaded_files_mutex = uploaded_files_mutex.clone();
                            let lib_name = lib.name.clone();
                            let lib_url = lib.url.clone();

                            async move {
                                let artifact_path =
                                    daedalus::get_path_from_artifact(&lib_name.replace(DUMMY_REPLACE_STRING, &game_version.version))?;

                                let artifact = download_file(
                                    &format!(
                                        "{}{}",
                                        lib_url.unwrap_or_else(|| {
                                            "https://maven.glass-launcher.net/babric/".to_string()
                                        }),
                                        artifact_path
                                    ),
                                    None,
                                    semaphore.clone(),
                                )
                                    .await?;

                                upload_file_to_bucket(
                                    format!("{}/{}", "maven", artifact_path),
                                    artifact.to_vec(),
                                    Some("application/java-archive".to_string()),
                                    &uploaded_files_mutex,
                                    semaphore.clone(),
                                )
                                    .await?;

                                Ok::<(), Error>(())
                            }.await?;

                            Ok::<(), Error>(())
                        })).await?;
                        lib.url = Some(format_url("maven/"));

                        return Ok(lib);
                    }

                    let artifact_path =
                        daedalus::get_path_from_artifact(&lib.name)?;

                    let artifact = download_file(
                        &format!(
                            "{}{}",
                            lib.url.unwrap_or_else(|| {
                                "https://maven.glass-launcher.net/babric/".to_string()
                            }),
                            artifact_path
                        ),
                        None,
                        semaphore.clone(),
                    )
                        .await?;

                    lib.url = Some(format_url("maven/"));

                    upload_file_to_bucket(
                        format!("{}/{}", "maven", artifact_path),
                        artifact.to_vec(),
                        Some("application/java-archive".to_string()),
                        &uploaded_files_mutex,
                        semaphore.clone(),
                    )
                        .await?;

                    Ok::<Library, Error>(lib)
                }),
            )
                .await?;

            let version_path = format!(
                "babric/v{}/versions/{}.json",
                daedalus::modded::CURRENT_BABRIC_FORMAT_VERSION,
                &loader
            );

            upload_file_to_bucket(
                version_path.clone(),
                serde_json::to_vec(&PartialVersionInfo {
                    arguments: version.arguments,
                    id: version
                        .id
                        .replace(DUMMY_GAME_VERSION, DUMMY_REPLACE_STRING),
                    main_class: version.main_class,
                    release_time: version.release_time,
                    time: version.time,
                    type_: version.type_,
                    inherits_from: version
                        .inherits_from
                        .replace(DUMMY_GAME_VERSION, DUMMY_REPLACE_STRING),
                    libraries: libs,
                    minecraft_arguments: version.minecraft_arguments,
                    processors: None,
                    data: None,
                })?,
                Some("application/json".to_string()),
                &uploaded_files_mutex,
                semaphore.clone(),
            )
                .await?;

            {
                let mut loader_version_map = loader_version_mutex.lock().await;
                async move {
                    loader_version_map.push(LoaderVersion {
                        id: loader.to_string(),
                        url: format_url(&version_path),
                        stable: *stable,
                    });
                }
                    .await;
            }

            Ok::<(), Error>(())
        },
    ))
        .await?;

    versions.push(Version {
        id: DUMMY_REPLACE_STRING.to_string(),
        stable: true,
        loaders: loader_version_mutex.into_inner(),
    });

    for version in &list.game {
        versions.push(Version {
            id: version.version.clone(),
            stable: version.stable,
            loaders: vec![],
        });
    }

    versions.sort_by(|x, y| {
        minecraft_versions
            .versions
            .iter()
            .position(|z| x.id == z.id)
            .unwrap_or_default()
            .cmp(
                &minecraft_versions
                    .versions
                    .iter()
                    .position(|z| y.id == z.id)
                    .unwrap_or_default(),
            )
    });

    for version in &mut versions {
        version.loaders.sort_by(|x, y| {
            list.loader
                .iter()
                .position(|z| {
                    x.id.split('-').next().unwrap_or_default() == &*z.version
                })
                .unwrap_or_default()
                .cmp(
                    &list
                        .loader
                        .iter()
                        .position(|z| {
                            y.id.split('-').next().unwrap_or_default()
                                == z.version
                        })
                        .unwrap_or_default(),
                )
        })
    }

    upload_file_to_bucket(
        format!(
            "babric/v{}/manifest.json",
            daedalus::modded::CURRENT_BABRIC_FORMAT_VERSION,
        ),
        serde_json::to_vec(&Manifest {
            game_versions: versions,
        })?,
        Some("application/json".to_string()),
        &uploaded_files_mutex,
        semaphore,
    )
        .await?;

    if let Ok(uploaded_files_mutex) = Arc::try_unwrap(uploaded_files_mutex) {
        uploaded_files.extend(uploaded_files_mutex.into_inner());
    }

    Ok(())
}

async fn fetch_babric_version(
    version_number: &str,
    loader_version: &str,
    semaphore: Arc<Semaphore>,
) -> Result<PartialVersionInfo, Error> {
    fetch_fabric_like_version(version_number, loader_version, semaphore, BABRIC_META_URL).await
}

async fn fetch_babric_versions(
    url: Option<&str>,
    semaphore: Arc<Semaphore>,
) -> Result<FabricVersions, Error> {
    fetch_fabric_like_versions(url, semaphore, BABRIC_META_URL).await
}