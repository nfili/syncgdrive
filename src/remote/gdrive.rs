use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::DateTime;
use reqwest::{header, Client};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::{
    path_cache::PathCache, ChangesPage, HealthStatus, RemoteIndex, RemoteProvider, UploadResult,
};
use crate::auth::GoogleAuth;
use crate::config::AdvancedConfig;
use crate::engine::bandwidth::{BandwidthLimiter, ProgressTracker};
use crate::engine::rate_limiter::ApiRateLimiter; // NOUVEAU PHASE 6

/// Le fournisseur Google Drive
pub struct GDriveProvider {
    client: Client,
    auth: Arc<GoogleAuth>,
    path_cache: Arc<PathCache>,
    config: Arc<AdvancedConfig>,
    shutdown: CancellationToken,
    limiter: Arc<BandwidthLimiter>,
    api_limiter: Arc<ApiRateLimiter>, // NOUVEAU : Le péage anti-bannissement
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

        let limiter = Arc::new(BandwidthLimiter::new(config.upload_limit_kbps));
        // On récupère la limite RPS depuis la configuration (par défaut 10 requêtes / seconde).
        let api_limiter = Arc::new(ApiRateLimiter::new(config.api_rate_limit_rps));

        Ok(Self {
            client,
            auth,
            path_cache,
            config,
            shutdown,
            limiter,
            api_limiter,
        })
    }

    /// Helper interne pour lire un en-tête Retry-After s'il existe
    fn parse_retry_after(headers: &header::HeaderMap) -> u64 {
        headers
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60) // 60 secondes de pause par défaut si non spécifié
    }

    async fn upload_simple(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,
        tracker: Arc<ProgressTracker>,
    ) -> Result<UploadResult> {
        self.api_limiter.acquire().await; // Péage API
        let token = self.get_token().await?;

        let mut meta_json = serde_json::json!({
            "name": file_name,
        });

        if existing_id.is_none() {
            meta_json
                .as_object_mut()
                .unwrap()
                .insert("parents".to_string(), serde_json::json!([parent_id]));
        }

        let metadata_part =
            reqwest::multipart::Part::text(meta_json.to_string()).mime_str("application/json")?;

        let file_bytes = tokio::fs::read(local_path)
            .await
            .context("Erreur de lecture du fichier local")?;

        let file_size = file_bytes.len() as u64;

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
            format!(
                "{}/files/{}?uploadType=multipart&fields=id,md5Checksum,size",
                self.config.upload_base, id
            )
        } else {
            format!(
                "{}/files?uploadType=multipart&fields=id,md5Checksum,size",
                self.config.upload_base
            )
        };

        let request_builder = if existing_id.is_some() {
            self.client.patch(&url)
        } else {
            self.client.post(&url)
        };

        let request_future = request_builder.bearer_auth(token).multipart(form).send();

        let res = tokio::select! {
            result = request_future => result.context("Erreur réseau lors de l'upload simple")?,
            _ = self.shutdown.cancelled() => {
                anyhow::bail!("Upload annulé proprement par le signal d'arrêt.");
            }
        };

        // GESTION DU 429 TOO MANY REQUESTS
        if res.status() == 429 {
            let wait = Self::parse_retry_after(res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!(
                "Erreur API 429 : Google Drive demande une pause de {}s",
                wait
            );
            // La fonction `retry` du worker se chargera de recommencer plus tard
        }

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
        tracker: Arc<ProgressTracker>,
    ) -> Result<UploadResult> {
        self.api_limiter.acquire().await; // Péage API
        let token = self.get_token().await?;

        // ─── ÉTAPE 1 : INITIATION ───
        let mut meta_json = serde_json::json!({ "name": file_name });

        if existing_id.is_none() {
            meta_json
                .as_object_mut()
                .unwrap()
                .insert("parents".to_string(), serde_json::json!([parent_id]));
        }

        let init_url = if let Some(id) = existing_id {
            format!(
                "{}/files/{}?uploadType=resumable",
                self.config.upload_base, id
            )
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
            .header(header::CONTENT_TYPE, "application/json")
            .query(&[("fields", "id,md5Checksum,size")])
            .json(&meta_json)
            .send()
            .await
            .context("Erreur réseau lors de l'initiation de l'upload resumable")?;

        if init_res.status() == 429 {
            let wait = Self::parse_retry_after(init_res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (init resumable) : Pause de {}s", wait);
        }

        if !init_res.status().is_success() {
            anyhow::bail!("Erreur API (init resumable) : {}", init_res.status());
        }

        let session_uri = init_res
            .headers()
            .get(header::LOCATION)
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

        let stream = async_stream::stream! {
            let mut file = file;
            let mut buf = vec![0u8; 64 * 1024]; // 64 Ko
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break, // Fin du fichier
                    Ok(n) => {
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

        self.api_limiter.acquire().await; // Péage API (le PUT de streaming compte comme une requête)

        let put_future = self
            .client
            .put(&session_uri)
            .header(header::CONTENT_LENGTH, file_size)
            .body(body)
            .send();

        // ─── ÉTAPE 3 : ENVOI ET SURVEILLANCE ───
        let res = tokio::select! {
            result = put_future => result.context("Erreur réseau lors du streaming")?,
            _ = self.shutdown.cancelled() => {
                anyhow::bail!("Upload lourd annulé proprement.");
            }
        };

        if res.status() == 429 {
            let wait = Self::parse_retry_after(res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (streaming resumable) : Pause de {}s", wait);
        }

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
    async fn list_remote(&self, root_id: &str) -> Result<RemoteIndex> {
        let mut files = Vec::new();
        let mut dirs = Vec::new();

        let mut queue: VecDeque<(String, String)> = VecDeque::new();
        queue.push_back((root_id.to_string(), String::new()));

        while let Some((current_folder_id, current_path)) = queue.pop_front() {
            tokio::task::yield_now().await;
            let mut page_token: Option<String> = None;

            loop {
                self.api_limiter.acquire().await; // Péage API dans la boucle
                let token = self.get_token().await?;

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

                let res = request
                    .send()
                    .await
                    .context("Erreur réseau lors du listing BFS")?;

                if res.status() == 429 {
                    let wait = Self::parse_retry_after(res.headers());
                    self.api_limiter.handle_rate_limit(wait).await;
                    // On ne casse pas la boucle, on refait un tour au prochain passage
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    continue;
                }

                if !res.status().is_success() {
                    anyhow::bail!(
                        "Erreur API lors du listing du dossier {} : {}",
                        current_path,
                        res.status()
                    );
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
                            let size = item["size"]
                                .as_str()
                                .unwrap_or("0")
                                .parse::<u64>()
                                .unwrap_or(0);

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
        self.api_limiter.acquire().await; // Péage API (Search)
        let token = self.get_token().await?;

        let safe_name = name.replace('\'', "\\'");
        let query = format!(
            "name = '{}' and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
            safe_name, parent_id
        );

        let search_res = self
            .client
            .get(format!("{}/files", self.config.api_base))
            .query(&[("q", query.as_str()), ("fields", "files(id)")])
            .bearer_auth(&token)
            .send()
            .await?;

        if search_res.status() == 429 {
            let wait = Self::parse_retry_after(search_res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (mkdir search) : Pause de {}s", wait);
        }

        if !search_res.status().is_success() {
            anyhow::bail!(
                "Erreur API lors de la vérification du dossier : {}",
                search_res.status()
            );
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

        self.api_limiter.acquire().await; // Péage API (Create)
        let create_res = self
            .client
            .post(format!("{}/files", self.config.api_base))
            .json(&body)
            .bearer_auth(&token)
            .send()
            .await?;

        if create_res.status() == 429 {
            let wait = Self::parse_retry_after(create_res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (mkdir create) : Pause de {}s", wait);
        }

        if !create_res.status().is_success() {
            anyhow::bail!(
                "Erreur API lors de la création du dossier : {}",
                create_res.status()
            );
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
        tracker: Arc<ProgressTracker>,
    ) -> Result<UploadResult> {
        let metadata = tokio::fs::metadata(local_path)
            .await
            .context("Impossible de lire les métadonnées du fichier local")?;

        let file_size = metadata.len();
        let chunk_threshold = self.config.chunk_threshold;

        if file_size <= chunk_threshold {
            self.upload_simple(local_path, parent_id, file_name, existing_id, tracker)
                .await
        } else {
            self.upload_resumable(
                local_path,
                parent_id,
                file_name,
                existing_id,
                file_size,
                tracker,
            )
            .await
        }
    }

    async fn delete(&self, file_id: &str) -> Result<()> {
        self.api_limiter.acquire().await; // Péage API
        let token = self.get_token().await?;
        let is_permanent = self.config.delete_mode == "permanent";

        let res = if is_permanent {
            self.client
                .delete(format!("{}/files/{}", self.config.api_base, file_id))
                .bearer_auth(&token)
                .send()
                .await?
        } else {
            let body = serde_json::json!({ "trashed": true });
            self.client
                .patch(format!("{}/files/{}", self.config.api_base, file_id))
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?
        };

        if res.status() == 429 {
            let wait = Self::parse_retry_after(res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (delete) : Pause de {}s", wait);
        }

        if !res.status().is_success() {
            anyhow::bail!(
                "Erreur API lors de la suppression/mise à la corbeille : {}",
                res.status()
            );
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

        let mut request = self
            .client
            .patch(format!("{}/files/{}", self.config.api_base, file_id))
            .bearer_auth(&token);

        let mut body = serde_json::Map::new();
        if let Some(name) = new_name {
            body.insert("name".to_string(), serde_json::json!(name));
        }

        if let Some(new_parent) = new_parent_id {
            self.api_limiter.acquire().await; // Péage API (Get parents)
            let get_res = self
                .client
                .get(format!(
                    "{}/files/{}?fields=parents",
                    self.config.api_base, file_id
                ))
                .bearer_auth(&token)
                .send()
                .await?;

            if get_res.status() == 429 {
                let wait = Self::parse_retry_after(get_res.headers());
                self.api_limiter.handle_rate_limit(wait).await;
                anyhow::bail!("Erreur API 429 (rename get_parents) : Pause de {}s", wait);
            }

            if get_res.status().is_success() {
                let get_data: serde_json::Value = get_res.json().await?;
                if let Some(parents) = get_data["parents"].as_array() {
                    let old_parents: Vec<&str> =
                        parents.iter().filter_map(|p| p.as_str()).collect();
                    let remove_parents_str = old_parents.join(",");

                    request = request.query(&[
                        ("addParents", new_parent),
                        ("removeParents", &remove_parents_str),
                    ]);
                }
            }
        }

        self.api_limiter.acquire().await; // Péage API (Rename/Move)
        let res = request.json(&body).send().await?;

        if res.status() == 429 {
            let wait = Self::parse_retry_after(res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (rename patch) : Pause de {}s", wait);
        }

        if !res.status().is_success() {
            anyhow::bail!(
                "Erreur API lors du renommage/déplacement : {}",
                res.status()
            );
        }

        Ok(())
    }

    async fn get_changes(&self, cursor: Option<&str>) -> Result<ChangesPage> {
        self.api_limiter.acquire().await; // Péage API
        let token = self.get_token().await?;

        let current_cursor = match cursor {
            Some(c) => c.to_string(),
            None => {
                let res = self
                    .client
                    .get(format!("{}/changes/startPageToken", self.config.api_base))
                    .bearer_auth(&token)
                    .send()
                    .await?;

                if res.status() == 429 {
                    let wait = Self::parse_retry_after(res.headers());
                    self.api_limiter.handle_rate_limit(wait).await;
                    anyhow::bail!(
                        "Erreur API 429 (get_changes startToken) : Pause de {}s",
                        wait
                    );
                }

                let data: serde_json::Value = res.json().await?;
                data["startPageToken"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }
        };

        self.api_limiter.acquire().await; // Péage API (Changes)
        let res = self.client
            .get(format!("{}/changes", self.config.api_base))
            .bearer_auth(&token)
            .query(&[
                ("pageToken", current_cursor.as_str()),
                ("fields", "nextPageToken, newStartPageToken, changes(fileId, file(name, parents), removed)")
            ])
            .send()
            .await?;

        if res.status() == 429 {
            let wait = Self::parse_retry_after(res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            anyhow::bail!("Erreur API 429 (get_changes) : Pause de {}s", wait);
        }

        if !res.status().is_success() {
            anyhow::bail!(
                "Erreur API lors de la récupération des changements : {}",
                res.status()
            );
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
                        parent_id,
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

        Ok(ChangesPage {
            changes,
            new_cursor,
            has_more,
        })
    }

    async fn check_health(&self) -> Result<HealthStatus> {
        self.api_limiter.acquire().await; // Péage API
        let token = self.get_token().await?;

        let res = self
            .client
            .get(format!("{}/about", self.config.api_base))
            .query(&[("fields", "user,storageQuota")])
            .bearer_auth(&token)
            .send()
            .await?;

        if res.status() == 429 {
            let wait = Self::parse_retry_after(res.headers());
            self.api_limiter.handle_rate_limit(wait).await;
            return Ok(HealthStatus::Unreachable);
        }

        if res.status().is_client_error() {
            if res.status().as_u16() == 401 {
                return Ok(HealthStatus::AuthExpired);
            }
            return Ok(HealthStatus::Unreachable);
        }

        let data: serde_json::Value = res.json().await?;

        let email = data["user"]["emailAddress"]
            .as_str()
            .unwrap_or("Inconnu")
            .to_string();
        let quota_used = data["storageQuota"]["usage"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        let quota_total = data["storageQuota"]["limit"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);

        Ok(HealthStatus::Ok {
            email,
            quota_used,
            quota_total,
        })
    }

    async fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

// Les tests actuels ne sont pas modifiés
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth2::GoogleTokens;
    use crate::config::AdvancedConfig;
    use mockito::Server;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_util::sync::CancellationToken;

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
        config.api_rate_limit_rps = 1000; // Très haut pour ne pas ralentir les tests

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
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_check_health_ok() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server
            .mock("GET", "/about")
            .match_query(mockito::Matcher::Any)
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "user": { "emailAddress": "bella@filippozzi.fr" },
                "storageQuota": { "usage": "1500", "limit": "15000" }
            }"#,
            )
            .create_async()
            .await;

        let provider = setup_mock_provider(server.url()).await;
        let status = provider
            .check_health()
            .await
            .expect("check_health a échoué");

        mock.assert_async().await;

        match status {
            HealthStatus::Ok {
                email,
                quota_used,
                quota_total,
            } => {
                assert_eq!(email, "bella@filippozzi.fr");
                assert_eq!(quota_used, 1500);
                assert_eq!(quota_total, 15000);
            }
            _ => panic!("Le statut de santé devrait être Ok"),
        }
    }

    #[tokio::test]
    async fn test_upload_simple_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server
            .mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=multipart.*".into()))
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id": "file_simple_123", "md5Checksum": "abcde", "size": "15"}"#)
            .create_async()
            .await;

        let provider = setup_mock_provider(server.url()).await;
        let tracker = Arc::new(ProgressTracker::new());

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_simple.txt");
        tokio::fs::write(&file_path, "Hello Arch Linux!")
            .await
            .unwrap();

        let res = provider
            .upload(&file_path, "parent_id", "test_simple.txt", None, tracker)
            .await
            .unwrap();

        mock.assert_async().await;
        assert_eq!(res.drive_id, "file_simple_123");
        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_upload_resumable_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock_init = server
            .mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=resumable.*".into()))
            .with_status(200)
            .with_header("Location", &format!("{}/upload_session_123", server.url()))
            .create_async()
            .await;

        let mock_chunk = server
            .mock("PUT", "/upload_session_123")
            .with_status(200)
            .with_body(r#"{"id": "resumable_123", "md5Checksum": "chk", "size": "6000000"}"#)
            .create_async()
            .await;

        let provider = setup_mock_provider(server.url()).await;
        let tracker = Arc::new(ProgressTracker::new());

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_lourd.bin");
        let file = std::fs::File::create(&file_path).unwrap();
        file.set_len(6_000_000).unwrap();

        let res = provider
            .upload_resumable(
                &file_path,
                "parent",
                "test_lourd.bin",
                None,
                6_000_000,
                tracker,
            )
            .await
            .unwrap();

        mock_init.assert_async().await;
        mock_chunk.assert_async().await;
        assert_eq!(res.drive_id, "resumable_123");

        let _ = tokio::fs::remove_file(file_path).await;
    }
}
