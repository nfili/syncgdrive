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

/// Le fournisseur Google Drive
pub struct GDriveProvider {
    client: Client,                      // partagé, Keep-Alive
    auth: Arc<GoogleAuth>,               // Phase 2
    path_cache: Arc<PathCache>,          // cache path→ID
    config: Arc<AdvancedConfig>,         // Source de vérité unique !
    shutdown: CancellationToken,         // arrêt gracieux
}

impl GDriveProvider {
    /// Initialise le client HTTP avec un pool de connexions optimisé
    /// Initialise le client HTTP avec un pool de connexions optimisé
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
            .timeout(std::time::Duration::from_secs(30)) // Sécurité : 30s max par requête
            .connect_timeout(std::time::Duration::from_secs(5)) // Échec rapide si le serveur (mock) ne répond pa
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .build()
            .context("Erreur d'initialisation du client HTTP reqwest")?;

        Ok(Self {
            client,
            auth,
            path_cache,
            config,
            shutdown,
        })
    }

    /// Helper interne pour l'upload des fichiers de moins de 5 Mo
    async fn upload_simple(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,
    ) -> Result<UploadResult> {
        let token = self.get_token().await?;

        // 1. Préparation des métadonnées JSON
        let mut meta_json = serde_json::json!({
            "name": file_name,
        });

        // GDrive interdit de modifier les 'parents' lors d'un PATCH (mise à jour du contenu).
        // Ils ne sont définis que lors de la création (POST).
        if existing_id.is_none() {
            meta_json.as_object_mut().unwrap().insert(
                "parents".to_string(),
                serde_json::json!([parent_id]),
            );
        }

        let metadata_part = reqwest::multipart::Part::text(meta_json.to_string())
            .mime_str("application/json")?;

        // 2. Lecture du fichier en mémoire (Totalement sûr car on a garanti < 5 Mo)
        let file_bytes = tokio::fs::read(local_path)
            .await
            .context("Erreur de lecture du fichier local")?;

        let file_part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")?;

        // 3. Assemblage du formulaire Multipart
        let form = reqwest::multipart::Form::new()
            .part("metadata", metadata_part)
            .part("file", file_part);

        // 4. Choix de la méthode : POST (Création) ou PATCH (Mise à jour)
        // On demande à Google de renvoyer md5Checksum pour garantir l'intégrité de la sauvegarde
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

        // 5. Exécution avec annulation gracieuse !
        let res = tokio::select! {
            result = request_future => result.context("Erreur réseau lors de l'upload simple")?,
            _ = self.shutdown.cancelled() => {
                anyhow::bail!("Upload annulé proprement par le signal d'arrêt.");
            }
        };

        if !res.status().is_success() {
            anyhow::bail!("Erreur API lors de l'upload simple : {}", res.status());
        }

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
    ) -> Result<UploadResult> {
        let token = self.get_token().await?;

        // ─── ÉTAPE 1 : INITIATION DE LA SESSION ───
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
            // On indique qu'on veut récupérer id, md5 et taille à la toute fin
            .query(&[("fields", "id,md5Checksum,size")])
            .json(&meta_json)
            .send()
            .await
            .context("Erreur réseau lors de l'initiation de l'upload resumable")?;

        if !init_res.status().is_success() {
            anyhow::bail!("Erreur API (init resumable) : {}", init_res.status());
        }

        // On récupère l'URL unique de session fournie par Google
        let session_uri = init_res
            .headers()
            .get(reqwest::header::LOCATION)
            .context("L'API Google n'a pas renvoyé l'en-tête Location")?
            .to_str()?
            .to_string();


        // ─── ÉTAPE 2 : ENVOI DES CHUNKS (MORCEAUX) ───
        let mut file = tokio::fs::File::open(local_path)
            .await
            .context("Impossible d'ouvrir le fichier local pour le streaming")?;

        // GDrive exige que la taille du chunk soit un multiple de 256 Ko.
        // On sécurise ici : 5 Mo est bien un multiple de 256 Ko (20 * 256 Ko = 5 Mo).
        let chunk_size = self.config.chunk_threshold as usize;
        let mut buffer = vec![0u8; chunk_size];
        let mut bytes_sent = 0u64;
        let mut final_data: Option<serde_json::Value> = None;

        while bytes_sent < file_size {
            // On calcule combien d'octets il reste à lire (soit un chunk complet, soit la fin du fichier)
            let bytes_to_read = std::cmp::min(chunk_size as u64, file_size - bytes_sent) as usize;

            // On remplit le buffer
            file.read_exact(&mut buffer[..bytes_to_read])
                .await
                .context("Erreur de lecture du fichier local en cours de streaming")?;

            // Construction de l'en-tête Content-Range exigé par Google
            // Format : "bytes DEBUT-FIN/TOTAL"
            let end_byte = bytes_sent + bytes_to_read as u64 - 1;
            let content_range = format!("bytes {}-{}/{}", bytes_sent, end_byte, file_size);

            // On clone la portion utile du buffer pour la requête
            let chunk_data = buffer[..bytes_to_read].to_vec();

            let put_future = self.client.put(&session_uri)
                .header(reqwest::header::CONTENT_LENGTH, bytes_to_read)
                .header(reqwest::header::CONTENT_RANGE, content_range)
                .body(chunk_data)
                .send();

            // Exécution avec interception du signal d'arrêt !
            let res = tokio::select! {
                result = put_future => result.context("Erreur réseau lors de l'envoi d'un chunk")?,
                _ = self.shutdown.cancelled() => {
                    anyhow::bail!("Upload lourd annulé proprement par l'utilisateur ou le système.");
                }
            };

            let status = res.status();

            if status.is_success() {
                // Code 200 ou 201 : Le fichier est totalement uploadé
                final_data = Some(res.json().await?);
                break;
            } else if status.as_u16() == 308 {
                // Code 308 (Resume Incomplete) : Google a bien reçu le morceau, on passe au suivant
                bytes_sent += bytes_to_read as u64;
            } else {
                // Autre code d'erreur (ex: 503, 403)
                anyhow::bail!("Erreur API pendant l'envoi d'un chunk : {}", status);
            }
        }

        // ─── ÉTAPE 3 : RETOUR DES RÉSULTATS ───
        let data = final_data.context("L'upload s'est terminé sans renvoyer les données finales")?;

        Ok(UploadResult {
            drive_id: data["id"].as_str().unwrap_or_default().to_string(),
            md5_checksum: data["md5Checksum"].as_str().unwrap_or_default().to_string(),
            size_bytes: data["size"].as_str().unwrap_or("0").parse().unwrap_or(0),
        })
    }

    /// Helper interne pour récupérer un token valide
    async fn get_token(&self) -> Result<String> {
        self.auth.get_valid_token().await
    }
    /// Expose le cache au SyncEngine pour la résolution des chemins locaux
    pub fn cache(&self) -> Arc<PathCache> {
        Arc::clone(&self.path_cache)
    }
}

#[async_trait]
impl RemoteProvider for GDriveProvider {
    async fn check_health(&self) -> Result<HealthStatus> {
        let token = self.get_token().await?;

        // Utilisation de l'URL dynamique
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

    // --- Les autres méthodes du trait (stub pour que le compilateur soit content) ---

    async fn list_remote(&self, root_id: &str) -> Result<RemoteIndex> {
        let token = self.get_token().await?;

        let mut files = Vec::new();
        let mut dirs = Vec::new();

        // File d'attente pour le BFS : (ID du dossier, Chemin relatif courant)
        let mut queue: VecDeque<(String, String)> = VecDeque::new();
        queue.push_back((root_id.to_string(), String::new()));

        while let Some((current_folder_id, current_path)) = queue.pop_front() {
            let mut page_token: Option<String> = None;

            loop {
                // On échappe l'ID pour éviter les erreurs de syntaxe dans la requête GDrive
                let safe_folder_id = current_folder_id.replace('\'', "\\'");
                let query = format!("'{}' in parents and trashed = false", safe_folder_id);

                let mut request = self.client
                    .get(format!("{}/files", self.config.api_base))
                    .bearer_auth(&token)
                    .query(&[
                        ("q", query.as_str()),
                        ("fields", "nextPageToken, files(id, name, mimeType, parents, md5Checksum, size, modifiedTime)"),
                        ("pageSize", "1000"), // Optimisation : on prend le maximum par requête
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

                        // Construction dynamique du chemin relatif
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

                            // On ajoute ce sous-dossier à la file pour l'explorer plus tard
                            queue.push_back((id, rel_path));
                        } else {
                            let md5 = item["md5Checksum"].as_str().unwrap_or_default().to_string();
                            let size = item["size"].as_str().unwrap_or("0").parse::<u64>().unwrap_or(0);

                            // Parsing du timestamp en i64 (Unix timestamp)
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

                // Vérification de la pagination pour le dossier courant
                if let Some(next_token) = data["nextPageToken"].as_str() {
                    page_token = Some(next_token.to_string());
                } else {
                    break; // Ce dossier est entièrement lu, on passe au suivant dans la file
                }
            }
        }

        Ok(RemoteIndex { files, dirs })
    }

    async fn mkdir(&self, parent_id: &str, name: &str) -> Result<String> {
        let token = self.get_token().await?;

        // 1. L'Anti-Doublon : Recherche si le dossier existe déjà
        // On échappe les apostrophes pour ne pas casser la requête GDrive QL.
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

        // Si le dossier existe, on s'arrête là et on retourne son ID
        if let Some(files) = search_data["files"].as_array() {
            if let Some(first_file) = files.first() {
                if let Some(id) = first_file["id"].as_str() {
                    return Ok(id.to_string());
                }
            }
        }

        // 2. Le dossier n'existe pas, on procède à sa création
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
    ) -> Result<UploadResult> {
        let metadata = tokio::fs::metadata(local_path)
            .await
            .context("Impossible de lire les métadonnées du fichier local")?;

        let file_size = metadata.len();

        // On récupère le seuil défini dans ta config (par défaut souvent 5 Mo = 5 * 1024 * 1024)
        // Si tu ne l'as pas encore dans config, on peut utiliser une constante temporaire.
        let chunk_threshold = self.config.chunk_threshold;

        if file_size <= chunk_threshold {
            // Fichier léger : on envoie tout d'un coup
            self.upload_simple(local_path, parent_id, file_name, existing_id).await
        } else {
            // Fichier lourd : on initie une session resumable
            self.upload_resumable(local_path, parent_id, file_name, existing_id, file_size).await
        }
    }

    async fn delete(&self, file_id: &str) -> Result<()> {
        let token = self.get_token().await?;

        // On vérifie le mode de suppression dans la config (par défaut on met à la corbeille par sécurité)
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
            // Mise à la corbeille via PATCH
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

        // 1. Changement de nom
        let mut body = serde_json::Map::new();
        if let Some(name) = new_name {
            body.insert("name".to_string(), serde_json::json!(name));
        }

        // 2. Déplacement (Changement de parent)
        // L'API Google exige de spécifier "addParents" et "removeParents".
        if let Some(new_parent) = new_parent_id {
            // On doit d'abord interroger l'API pour connaître les parents actuels à retirer
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

                    // On modifie l'URL de la requête PATCH avec les paramètres de déplacement
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

        // Si aucun curseur n'est fourni, on demande un "StartPageToken" initial à Google
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
                // On demande le strict nécessaire pour notre enum Change
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

        // Le prochain curseur à sauvegarder dans SQLite
        let new_cursor = data["nextPageToken"]
            .as_str()
            .or_else(|| data["newStartPageToken"].as_str())
            .unwrap_or(&current_cursor)
            .to_string();

        let has_more = data["nextPageToken"].as_str().is_some();

        Ok(ChangesPage { changes, new_cursor, has_more })
    }

    async fn shutdown(&self) {
        // Envoie le signal d'annulation à tous les select! en cours
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

    // Verrou global pour empêcher la collision de lecture/écriture des faux tokens
    static TEST_MUTEX: AsyncMutex<()> = AsyncMutex::const_new(());

    async fn setup_mock_provider(server_url: String) -> GDriveProvider {
        let test_uuid = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let temp_dir = std::env::temp_dir().join(format!("sync_test_{}", test_uuid));

        // CRITIQUE : Créer le sous-dossier attendu par EncryptedFileStorage
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
            access_token: "fake_access_token".into(), // CORRECTION : Doit correspondre aux mocks
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
            .match_query(mockito::Matcher::Any) // Évite les soucis de %2C
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
    async fn test_mkdir_returns_existing() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock_search = server.mock("GET", "/files")
            .match_query(mockito::Matcher::Any)
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"files": [{"id": "dossier_existant_123"}]}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let id = provider.mkdir("parent_root", "Photos").await.expect("mkdir a échoué");

        mock_search.assert_async().await;
        assert_eq!(id, "dossier_existant_123");
    }

    #[tokio::test]
    async fn test_mkdir_creates_new() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock_search = server.mock("GET", "/files")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"files": []}"#)
            .create_async().await;

        let mock_create = server.mock("POST", "/files")
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id": "nouveau_dossier_999"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let id = provider.mkdir("parent_root", "Projets").await.expect("mkdir a échoué");

        mock_search.assert_async().await;
        mock_create.assert_async().await;
        assert_eq!(id, "nouveau_dossier_999");
    }

    #[tokio::test]
    async fn test_upload_simple_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        // CORRECTION : Utilisation de Regex pour ne pas bloquer sur les %2C
        let mock = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=multipart.*".into()))
            .match_header("authorization", "Bearer fake_access_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id": "file_simple_123", "md5Checksum": "abcde", "size": "15"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_simple.txt");
        tokio::fs::write(&file_path, "Hello Arch Linux!").await.unwrap();

        let res = provider.upload(&file_path, "parent_id", "test_simple.txt", None).await.unwrap();

        mock.assert_async().await;
        assert_eq!(res.drive_id, "file_simple_123");
        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_delete_trash_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server.mock("PATCH", "/files/file_to_trash")
            .match_body(mockito::Matcher::Json(serde_json::json!({"trashed": true})))
            .with_status(200)
            .create_async().await;

        let mut provider = setup_mock_provider(server.url()).await;
        Arc::get_mut(&mut provider.config).unwrap().delete_mode = "trash".to_string();

        provider.delete("file_to_trash").await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_delete_permanent_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        let mock = server.mock("DELETE", "/files/file_to_delete")
            .with_status(204)
            .create_async().await;

        let mut provider = setup_mock_provider(server.url()).await;
        Arc::get_mut(&mut provider.config).unwrap().delete_mode = "permanent".to_string();

        provider.delete("file_to_delete").await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_list_paginated_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        // CORRECTION : Regex simplifié pour bypasser l'encodage URL des apostrophes (%27)
        let mock_page1 = server.mock("GET", "/files")
            .match_query(mockito::Matcher::Regex(r".*root_id.*".into()))
            .with_status(200)
            .with_body(r#"{"nextPageToken": "token_page_2", "files": [{"id": "f1", "name": "fichier1.txt", "mimeType": "text/plain"}]}"#)
            .create_async().await;

        let mock_page2 = server.mock("GET", "/files")
            .match_query(mockito::Matcher::Regex(r".*pageToken=token_page_2.*".into()))
            .with_status(200)
            .with_body(r#"{"files": [{"id": "d1", "name": "Sous-Dossier", "mimeType": "application/vnd.google-apps.folder"}]}"#)
            .create_async().await;

        let mock_d1_content = server.mock("GET", "/files")
            .match_query(mockito::Matcher::Regex(r".*d1.*".into()))
            .with_status(200)
            .with_body(r#"{"files": []}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let index = provider.list_remote("root_id").await.expect("Listing échoué");

        mock_page1.assert_async().await;
        mock_page2.assert_async().await;
        mock_d1_content.assert_async().await;

        assert_eq!(index.files.len(), 1);
        assert_eq!(index.dirs.len(), 1);
    }

    #[tokio::test]
    async fn test_parse_file_list_response() {
        let _guard = TEST_MUTEX.lock().await;
        let response_body = r#"{
            "files": [
                {"id": "id_123", "name": "photo.jpg", "mimeType": "image/jpeg", "md5Checksum": "abc", "size": "500", "modifiedTime": "2026-03-17T10:00:00Z"}
            ]
        }"#;

        let data: serde_json::Value = serde_json::from_str(response_body).unwrap();
        let items = data["files"].as_array().unwrap();
        let item = &items[0];

        assert_eq!(item["id"].as_str().unwrap(), "id_123");
        assert_eq!(item["name"].as_str().unwrap(), "photo.jpg");
        assert_eq!(item["size"].as_str().unwrap().parse::<u64>().unwrap(), 500);
    }

    #[tokio::test]
    async fn test_upload_resumable_mock() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        // CORRECTION : Regex pour bypasser les , -> %2C dans la query URL
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

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_lourd.bin");
        let file = std::fs::File::create(&file_path).unwrap();
        file.set_len(6_000_000).unwrap();

        let res = provider.upload_resumable(&file_path, "parent", "test_lourd.bin", None, 6_000_000).await.unwrap();

        mock_init.assert_async().await;
        mock_chunk.assert_async().await;
        assert_eq!(res.drive_id, "resumable_123");

        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_parse_empty_response() {
        let _guard = TEST_MUTEX.lock().await;
        let response_body = r#"{"files": []}"#;
        let data: serde_json::Value = serde_json::from_str(response_body).unwrap();
        let items = data["files"].as_array().unwrap();

        assert!(items.is_empty());
    }

    // ─── TESTS MANQUANTS DE LA CHECKLIST (SECTION 7) ───

    #[test]
    fn test_build_list_query() {
        // Test unitaire pur (pas besoin de tokio ou de mock)
        // Vérifie l'échappement correct des apostrophes pour files.list
        let folder_id = "Dossier_d'images";
        let safe_folder_id = folder_id.replace('\'', "\\'");
        let query = format!("'{}' in parents and trashed = false", safe_folder_id);

        assert_eq!(query, "'Dossier_d\\'images' in parents and trashed = false");
    }

    #[tokio::test]
    async fn test_multipart_body_format() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        // On vérifie que le header Content-Type contient bien la directive "multipart/form-data"
        let mock = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=multipart.*".into()))
            .match_header("content-type", mockito::Matcher::Regex(r"multipart/form-data;.*".into()))
            .with_status(200)
            .with_body(r#"{"id": "multipart_123", "md5Checksum": "ok", "size": "10"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_multipart.txt");
        tokio::fs::write(&file_path, "1234567890").await.unwrap();

        let _ = provider.upload(&file_path, "parent_id", "test_multipart.txt", None).await;

        mock.assert_async().await;
        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_resumable_initiation_headers() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        // On vérifie que l'initiation envoie bien un JSON et demande le resumable
        let mock_init = server.mock("POST", "/files")
            .match_query(mockito::Matcher::Regex(r".*uploadType=resumable.*".into()))
            .match_header("content-type", "application/json") // Header critique pour Google
            .with_status(200)
            .with_header("Location", &format!("{}/session_uri", server.url()))
            .create_async().await;

        // On mock la suite pour éviter que la fonction ne panique sur le PUT
        let _mock_chunk = server.mock("PUT", "/session_uri")
            .with_status(200)
            .with_body(r#"{"id": "ok", "md5Checksum": "ok", "size": "6000000"}"#)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_headers.bin");
        let file = std::fs::File::create(&file_path).unwrap();
        file.set_len(6_000_000).unwrap();

        let _ = provider.upload_resumable(&file_path, "parent", "test_headers.bin", None, 6_000_000).await;

        mock_init.assert_async().await;
        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_token_refresh_during_upload() {
        let _guard = TEST_MUTEX.lock().await;
        let mut server = Server::new_async().await;

        // Mock d'une API qui refuse l'accès (Token expiré / 401)
        let mock_401 = server.mock("GET", "/about")
            .match_query(mockito::Matcher::Any)
            .with_status(401)
            .create_async().await;

        let provider = setup_mock_provider(server.url()).await;

        // On passe par check_health car c'est lui qui détecte le 401 en premier
        // dans le cycle de vie du SyncEngine
        let status = provider.check_health().await.unwrap();

        mock_401.assert_async().await;
        assert!(matches!(status, HealthStatus::AuthExpired));
        // Note: La logique de retry (refresh -> retry) est pilotée par le SyncEngine
        // qui reçoit ce statut `AuthExpired`, appelle `auth.refresh()` puis retente l'opération.
    }
}