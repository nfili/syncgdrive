use std::collections::VecDeque;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{Client, header};
use std::sync::Arc;
use std::path::Path;
use chrono::DateTime;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::{
    RemoteProvider, RemoteIndex, UploadResult, ChangesPage, HealthStatus,
    path_cache::PathCache,
};
use crate::auth::GoogleAuth;
use crate::config::AdvancedConfig;
use crate::engine::bandwidth::{ProgressTracker, BandwidthLimiter}; // NOUVEAU

/// Le fournisseur Google Drive
pub struct GDriveProvider {
    client: Client,
    auth: Arc<GoogleAuth>,
    path_cache: Arc<PathCache>,
    config: Arc<AdvancedConfig>,
    shutdown: CancellationToken,
    limiter: Arc<BandwidthLimiter>,      // NOUVEAU : Le limiteur global !
}

impl GDriveProvider {
    pub fn new(
        auth: Arc<GoogleAuth>,
        path_cache: Arc<PathCache>,
        config: Arc<AdvancedConfig>,
        shutdown: CancellationToken,
    ) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("SyncGDrive/2.0 (Arch Linux)"),
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(config.max_concurrent_ls)
            .build()
            .context("Erreur d'initialisation du client HTTP reqwest")?;

        // On initialise le limiteur avec la configuration
        let limiter = Arc::new(BandwidthLimiter::new(config.upload_limit_kbps));

        Ok(Self {
            client,
            auth,
            path_cache,
            config,
            shutdown,
            limiter,
        })
    }

    async fn upload_simple(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,
        tracker: Arc<ProgressTracker>, // NOUVEAU
    ) -> Result<UploadResult> {
        let token = self.get_token().await?;

        let mut meta_json = serde_json::json!({
            "name": file_name,
        });

        if existing_id.is_none() {
            meta_json.as_object_mut().unwrap().insert(
                "parents".to_string(),
                serde_json::json!([parent_id]),
            );
        }

        let metadata_part = reqwest::multipart::Part::text(meta_json.to_string())
            .mime_str("application/json")?;

        let file_bytes = tokio::fs::read(local_path)
            .await
            .context("Erreur de lecture du fichier local")?;

        let file_size = file_bytes.len() as u64;

        // On applique le limiteur UNIQUEMENT si la limite est supérieure à 0
        if self.config.upload_limit_kbps > 0 {
            tokio::select! {
                _ = self.limiter.acquire(file_size) => {},
                _ = self.shutdown.cancelled() => {
                    anyhow::bail!("Attente de bande passante annulée proprement par le signal d'arrêt.");
                }
            }
        } else if self.shutdown.is_cancelled() {
            anyhow::bail!("Upload annulé.");
        }

        let file_part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")?;

        let form = reqwest::multipart::Form::new()
            .part("metadata", metadata_part)
            .part("file", file_part);

        let url = if let Some(id) = existing_id {
            format!("{}/files/{}?uploadType=multipart&fields=id,md5Checksum,size", self.config.upload_base, id)
        } else {
            format!("{}/files?uploadType=multipart&fields=id,md5Checksum,size", self.config.upload_base)
        };

        let request_builder = if existing_id.is_some() {
            self.client.patch(&url)
        } else {
            self.client.post(&url)
        };

        let request_future = request_builder
            .bearer_auth(token)
            .multipart(form)
            .send();

        let res = tokio::select! {
            result = request_future => result.context("Erreur réseau lors de l'upload simple")?,
            _ = self.shutdown.cancelled() => {
                anyhow::bail!("Upload annulé proprement par le signal d'arrêt.");
            }
        };

        if !res.status().is_success() {
            anyhow::bail!("Erreur API lors de l'upload simple : {}", res.status());
        }

        tracker.record_bytes(file_size);
        let data: serde_json::Value = res.json().await?;

        Ok(UploadResult {
            drive_id: data["id"].as_str().unwrap_or_default().to_string(),
            md5_checksum: data["md5Checksum"].as_str().unwrap_or_default().to_string(),
            size_bytes: data["size"].as_str().unwrap_or("0").parse().unwrap_or(0),
        })
    }

    async fn upload_resumable(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,
        file_size: u64,
        tracker: Arc<ProgressTracker>, // NOUVEAU
    ) -> Result<UploadResult> {
        let token = self.get_token().await?;

        // ─── ÉTAPE 1 : INITIATION ───
        let mut meta_json = serde_json::json!({ "name": file_name });

        if existing_id.is_none() {
            meta_json.as_object_mut().unwrap().insert(
                "parents".to_string(),
                serde_json::json!([parent_id]),
            );
        }

        let init_url = if let Some(id) = existing_id {
            format!("{}/files/{}?uploadType=resumable", self.config.upload_base, id)
        } else {
            format!("{}/files?uploadType=resumable", self.config.upload_base)
        };

        let init_request = if existing_id.is_some() {
            self.client.patch(&init_url)
        } else {
            self.client.post(&init_url)
        };

        let init_res = init_request
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .query(&[("fields", "id,md5Checksum,size")])
            .json(&meta_json)
            .send()
            .await
            .context("Erreur réseau lors de l'initiation de l'upload resumable")?;

        if !init_res.status().is_success() {
            anyhow::bail!("Erreur API (init resumable) : {}", init_res.status());
        }

        let session_uri = init_res
            .headers()
            .get(reqwest::header::LOCATION)
            .context("L'API Google n'a pas renvoyé l'en-tête Location")?
            .to_str()?
            .to_string();


        // ─── ÉTAPE 2 : LE STREAMING MAGIQUE ! ───
        let file = tokio::fs::File::open(local_path)
            .await
            .context("Impossible d'ouvrir le fichier local pour le streaming")?;

        let stream_tracker = tracker.clone();
        let stream_limiter = self.limiter.clone();
        let limit_kbps = self.config.upload_limit_kbps;

        // Création du flux asynchrone qui découpe le fichier par blocs de 64 Ko
        let stream = async_stream::stream! {
            let mut file = file;
            let mut buf = vec![0u8; 64 * 1024]; // 64 Ko
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break, // Fin du fichier
                    Ok(n) => {
                        // NOUVEAU : Pareil, on ne limite que si > 0
                        if limit_kbps > 0 {
                            stream_limiter.acquire(n as u64).await;
                        }
                        stream_tracker.record_bytes(n as u64);
                        yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
                    }
                    Err(e) => yield Err(e),
                }
            }
        };

        let body = reqwest::Body::wrap_stream(stream);

        let put_future = self.client.put(&session_uri)
            .header(reqwest::header::CONTENT_LENGTH, file_size)
            .body(body)
            .send();

        // ─── ÉTAPE 3 : ENVOI ET SURVEILLANCE ───
        let res = tokio::select! {
            result = put_future => result.context("Erreur réseau lors du streaming")?,
            _ = self.shutdown.cancelled() => {
                anyhow::bail!("Upload lourd annulé proprement.");
            }
        };

        if !res.status().is_success() {
            anyhow::bail!("Erreur API pendant l'envoi du fichier : {}", res.status());
        }

        let data: serde_json::Value = res.json().await?;

        Ok(UploadResult {
            drive_id: data["id"].as_str().unwrap_or_default().to_string(),
            md5_checksum: data["md5Checksum"].as_str().unwrap_or_default().to_string(),
            size_bytes: data["size"].as_str().unwrap_or("0").parse().unwrap_or(0),
        })
    }

    async fn get_token(&self) -> Result<String> {
        self.auth.get_valid_token().await
    }

    pub fn cache(&self) -> Arc<PathCache> {
        Arc::clone(&self.path_cache)
    }
}

#[async_trait]
impl RemoteProvider for GDriveProvider {
    async fn check_health(&self) -> Result<HealthStatus> {
        let token = self.get_token().await?;

        let res = self.client
            .get(format!("{}/about", self.config.api_base))
            .query(&[("fields", "user,storageQuota")])
            .bearer_auth(&token)
            .send()
            .await?;

        if res.status().is_client_error() {
            if res.status().as_u16() == 401 {
                return Ok(HealthStatus::AuthExpired);
            }
            return Ok(HealthStatus::Unreachable);
        }

        let data: serde_json::Value = res.json().await?;

        let email = data["user"]["emailAddress"].as_str().unwrap_or("Inconnu").to_string();
        let quota_used = data["storageQuota"]["usage"].as_str().unwrap_or("0").parse().unwrap_or(0);
        let quota_total = data["storageQuota"]["limit"].as_str().unwrap_or("0").parse().unwrap_or(0);

        Ok(HealthStatus::Ok { email, quota_used, quota_total })
    }

    async fn list_remote(&self, root_id: &str) -> Result<RemoteIndex> {
        let token = self.get_token().await?;

        let mut files = Vec::new();
        let mut dirs = Vec::new();

        let mut queue: VecDeque<(String, String)> = VecDeque::new();
        queue.push_back((root_id.to_string(), String::new()));

        while let Some((current_folder_id, current_path)) = queue.pop_front() {
            let mut page_token: Option<String> = None;

            loop {
                let safe_folder_id = current_folder_id.replace('\'', "\\'");
                let query = format!("'{}' in parents and trashed = false", safe_folder_id);

                let mut request = self.client
                    .get(format!("{}/files", self.config.api_base))
                    .bearer_auth(&token)
                    .query(&[
                        ("q", query.as_str()),
                        ("fields", "nextPageToken, files(id, name, mimeType, parents, md5Checksum, size, modifiedTime)"),
                        ("pageSize", "1000"),
                    ]);

                if let Some(ref pt) = page_token {
                    request = request.query(&[("pageToken", pt)]);
                }

                let res = request.send().await.context("Erreur réseau lors du listing BFS")?;

                if !res.status().is_success() {
                    anyhow::bail!("Erreur API lors du listing du dossier {} : {}", current_path, res.status());
                }

                let data: serde_json::Value = res.json().await?;

                if let Some(items) = data["files"].as_array() {
                    for item in items {
                        let id = item["id"].as_str().unwrap_or_default().to_string();
                        let name = item["name"].as_str().unwrap_or_default().to_string();
                        let mime_type = item["mimeType"].as_str().unwrap_or_default();

                        let rel_path = if current_path.is_empty() {
                            name.clone()
                        } else {
                            format!("{}/{}", current_path, name)
                        };

                        if mime_type == "application/vnd.google-apps.folder" {
                            dirs.push(crate::remote::RemoteDir {
                                relative_path: rel_path.clone(),
                                drive_id: id.clone(),
                                parent_id: current_folder_id.clone(),
                            });
                            queue.push_back((id, rel_path));
                        } else {
                            let md5 = item["md5Checksum"].as_str().unwrap_or_default().to_string();
                            let size = item["size"].as_str().unwrap_or("0").parse::<u64>().unwrap_or(0);

                            let time_str = item["modifiedTime"].as_str().unwrap_or_default();
                            let modified_time = DateTime::parse_from_rfc3339(time_str)
                                .map(|dt| dt.timestamp())
                                .unwrap_or(0);

                            files.push(crate::remote::RemoteFile {
                                relative_path: rel_path,
                                drive_id: id,
                                parent_id: current_folder_id.clone(),
                                md5,
                                size,
                                modified_time,
                            });
                        }
                    }
                }

                if let Some(next_token) = data["nextPageToken"].as_str() {
                    page_token = Some(next_token.to_string());
                } else {
                    break;
                }
            }
        }

        Ok(RemoteIndex { files, dirs })
    }

    async fn mkdir(&self, parent_id: &str, name: &str) -> Result<String> {
        let token = self.get_token().await?;

        let safe_name = name.replace('\'', "\\'");
        let query = format!(
            "name = '{}' and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
            safe_name, parent_id
        );

        let search_res = self.client
            .get(format!("{}/files", self.config.api_base))
            .query(&[("q", query.as_str()), ("fields", "files(id)")])
            .bearer_auth(&token)
            .send()
            .await?;

        if !search_res.status().is_success() {
            anyhow::bail!("Erreur API lors de la vérification du dossier : {}", search_res.status());
        }

        let search_data: serde_json::Value = search_res.json().await?;

        if let Some(files) = search_data["files"].as_array() {
            if let Some(first_file) = files.first() {
                if let Some(id) = first_file["id"].as_str() {
                    return Ok(id.to_string());
                }
            }
        }

        let body = serde_json::json!({
            "name": name,
            "mimeType": "application/vnd.google-apps.folder",
            "parents": [parent_id]
        });

        let create_res = self.client
            .post(format!("{}/files", self.config.api_base))
            .json(&body)
            .bearer_auth(&token)
            .send()
            .await?;

        if !create_res.status().is_success() {
            anyhow::bail!("Erreur API lors de la création du dossier : {}", create_res.status());
        }

        let create_data: serde_json::Value = create_res.json().await?;
        let new_id = create_data["id"]
            .as_str()
            .context("L'API Google n'a pas retourné d'ID pour le nouveau dossier")?;

        Ok(new_id.to_string())
    }

    async fn upload(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,
        tracker: Arc<ProgressTracker>, // NOUVEAU
    ) -> Result<UploadResult> {
        let metadata = tokio::fs::metadata(local_path)
            .await
            .context("Impossible de lire les métadonnées du fichier local")?;

        let file_size = metadata.len();
        let chunk_threshold = self.config.chunk_threshold;

        if file_size <= chunk_threshold {
            self.upload_simple(local_path, parent_id, file_name, existing_id, tracker).await
        } else {
            self.upload_resumable(local_path, parent_id, file_name, existing_id, file_size, tracker).await
        }
    }

    async fn delete(&self, file_id: &str) -> Result<()> {
        let token = self.get_token().await?;
        let is_permanent = self.config.delete_mode == "permanent";

        if is_permanent {
            let res = self.client
                .delete(format!("{}/files/{}", self.config.api_base, file_id))
                .bearer_auth(&token)
                .send()
                .await?;

            if !res.status().is_success() {
                anyhow::bail!("Erreur API lors de la suppression définitive : {}", res.status());
            }
        } else {
            let body = serde_json::json!({ "trashed": true });
            let res = self.client
                .patch(format!("{}/files/{}", self.config.api_base, file_id))
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?;

            if !res.status().is_success() {
                anyhow::bail!("Erreur API lors de la mise à la corbeille : {}", res.status());
            }
        }

        Ok(())
    }

    async fn rename(
        &self,
        file_id: &str,
        new_name: Option<&str>,
        new_parent_id: Option<&str>,
    ) -> Result<()> {
        let token = self.get_token().await?;

        let mut request = self.client
            .patch(format!("{}/files/{}", self.config.api_base, file_id))
            .bearer_auth(&token);

        let mut body = serde_json::Map::new();
        if let Some(name) = new_name {
            body.insert("name".to_string(), serde_json::json!(name));
        }

        if let Some(new_parent) = new_parent_id {
            let get_res = self.client
                .get(format!("{}/files/{}?fields=parents", self.config.api_base, file_id))
                .bearer_auth(&token)
                .send()
                .await?;

            if get_res.status().is_success() {
                let get_data: serde_json::Value = get_res.json().await?;
                if let Some(parents) = get_data["parents"].as_array() {
                    let old_parents: Vec<&str> = parents.iter().filter_map(|p| p.as_str()).collect();
                    let remove_parents_str = old_parents.join(",");

                    request = request.query(&[
                        ("addParents", new_parent),
                        ("removeParents", &remove_parents_str)
                    ]);
                }
            }
        }

        let res = request.json(&body).send().await?;

        if !res.status().is_success() {
            anyhow::bail!("Erreur API lors du renommage/déplacement : {}", res.status());
        }

        Ok(())
    }

    async fn get_changes(&self, cursor: Option<&str>) -> Result<ChangesPage> {
        let token = self.get_token().await?;

        let current_cursor = match cursor {
            Some(c) => c.to_string(),
            None => {
                let res = self.client
                    .get(format!("{}/changes/startPageToken", self.config.api_base))
                    .bearer_auth(&token)
                    .send()
                    .await?;

                let data: serde_json::Value = res.json().await?;
                data["startPageToken"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }
        };

        let res = self.client
            .get(format!("{}/changes", self.config.api_base))
            .bearer_auth(&token)
            .query(&[
                ("pageToken", current_cursor.as_str()),
                ("fields", "nextPageToken, newStartPageToken, changes(fileId, file(name, parents), removed)")
            ])
            .send()
            .await?;

        if !res.status().is_success() {
            anyhow::bail!("Erreur API lors de la récupération des changements : {}", res.status());
        }

        let data: serde_json::Value = res.json().await?;
        let mut changes = Vec::new();

        if let Some(items) = data["changes"].as_array() {
            for item in items {
                let file_id = item["fileId"].as_str().unwrap_or_default().to_string();
                let removed = item["removed"].as_bool().unwrap_or(false);

                if removed {
                    changes.push(crate::remote::Change::Deleted { drive_id: file_id });
                } else if let Some(file) = item.get("file") {
                    let name = file["name"].as_str().unwrap_or_default().to_string();
                    let parent_id = file["parents"]
                        .as_array()
                        .and_then(|p| p.first())
                        .and_then(|id| id.as_str())
                        .unwrap_or_default()
                        .to_string();

                    changes.push(crate::remote::Change::Modified {
                        drive_id: file_id,
                        name,
                        parent_id
                    });
                }
            }
        }

        let new_cursor = data["nextPageToken"]
            .as_str()
            .or_else(|| data["newStartPageToken"].as_str())
            .unwrap_or(&current_cursor)
            .to_string();

        let has_more = data["nextPageToken"].as_str().is_some();

        Ok(ChangesPage { changes, new_cursor, has_more })
    }

    async fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdvancedConfig;
    use crate::auth::oauth2::GoogleTokens;
    use mockito::Server;
    use tokio_util::sync::CancellationToken;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    static TEST_MUTEX: AsyncMutex<()> = AsyncMutex::const_new(());

    async fn setup_mock_provider(server_url: String) -> GDriveProvider {
        let test_uuid = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let temp_dir = std::env::temp_dir().join(format!("sync_test_{}", test_uuid));

        let config_dir = temp_dir.join("syncgdrive");
        std::fs::create_dir_all(&config_dir).ok();

        std::env::set_var("XDG_CONFIG_HOME", temp_dir.to_str().unwrap());
        std::env::set_var("SYNCGDRIVE_CLIENT_SECRET", "secret_de_test_permanent_123");

        let mut config = AdvancedConfig::default();
        config.api_base = server_url.clone();
        config.upload_base = server_url;
        config.chunk_threshold = 5 * 1024 * 1024;

        let auth = GoogleAuth::new();
        let dummy_tokens = GoogleTokens {
            access_token: "fake_access_token".into(),
            refresh_token: "fake_refresh_token".into(),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            scope: "test".into(),
        };
        auth.save_tokens(&dummy_tokens).unwrap();

        GDriveProvider::new(
            Arc::new(auth),
            Arc::new(PathCache::new()),
            Arc::new(config),
            CancellationToken::new(),
        ).unwrap()
    }

    #[tokio::test]
    async fn test_check_health_ok() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server.mock("GET", "/about")
            .match_query(mockito::Matcher::Any)
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{
                "user": { "emailAddress": "bella@filippozzi.fr" },
                "storageQuota": { "usage": "1500", "limit": "15000" }
            }"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let status = provider.check_health().await.expect("check_health a échoué");

        mock.assert_async().await;

        match status {
            HealthStatus::Ok { email, quota_used, quota_total } => {
                assert_eq!(email, "bella@filippozzi.fr");
                assert_eq!(quota_used, 1500);
                assert_eq!(quota_total, 15000);
            },
            _ => panic!("Le statut de santé devrait être Ok"),
        }
    }

    #[tokio::test]
    async fn test_upload_simple_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=multipart.*".into()))
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id": "file_simple_123", "md5Checksum": "abcde", "size": "15"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let tracker = Arc::new(ProgressTracker::new()); // Tracker vide pour le test

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_simple.txt");
        tokio::fs::write(&file_path, "Hello Arch Linux!").await.unwrap();

        let res = provider.upload(&file_path, "parent_id", "test_simple.txt", None, tracker).await.unwrap();

        mock.assert_async().await;
        assert_eq!(res.drive_id, "file_simple_123");
        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_upload_resumable_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock_init = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=resumable.*".into()))
            .with_status(200)
            .with_header("Location", &format!("{}/upload_session_123", server.url()))
            .create_async().await;

        let mock_chunk = server.mock("PUT", "/upload_session_123")
            .with_status(200)
            .with_body(r#"{"id": "resumable_123", "md5Checksum": "chk", "size": "6000000"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let tracker = Arc::new(ProgressTracker::new());

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_lourd.bin");
        let file = std::fs::File::create(&file_path).unwrap();
        file.set_len(6_000_000).unwrap();

        let res = provider.upload_resumable(&file_path, "parent", "test_lourd.bin", None, 6_000_000, tracker).await.unwrap();

        mock_init.assert_async().await;
        mock_chunk.assert_async().await;
        assert_eq!(res.drive_id, "resumable_123");

        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_multipart_body_format() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=multipart.*".into()))
            .match_header("content-type", mockito::Matcher::Regex(r"multipart/form-data;.*".into()))
            .with_status(200)
            .with_body(r#"{"id": "multipart_123", "md5Checksum": "ok", "size": "10"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let tracker = Arc::new(ProgressTracker::new());

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_multipart.txt");
        tokio::fs::write(&file_path, "1234567890").await.unwrap();

        let _ = provider.upload(&file_path, "parent_id", "test_multipart.txt", None, tracker).await;

        mock.assert_async().await;
        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_resumable_initiation_headers() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock_init = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=resumable.*".into()))
            .match_header("content-type", "application/json")
            .with_status(200)
            .with_header("Location", &format!("{}/session_uri", server.url()))
            .create_async().await;

        let _mock_chunk = server.mock("PUT", "/session_uri")
            .with_status(200)
            .with_body(r#"{"id": "ok", "md5Checksum": "ok", "size": "6000000"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let tracker = Arc::new(ProgressTracker::new());

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_headers.bin");
        let file = std::fs::File::create(&file_path).unwrap();
        file.set_len(6_000_000).unwrap();

        let _ = provider.upload_resumable(&file_path, "parent", "test_headers.bin", None, 6_000_000, tracker).await;

        mock_init.assert_async().await;
        let _ = tokio::fs::remove_file(file_path).await;
    }
}