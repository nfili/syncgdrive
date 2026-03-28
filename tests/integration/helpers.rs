use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use anyhow::Result;
use async_trait::async_trait;
use tempfile::TempDir;

use sync_g_drive::config::{AppConfig, SyncPair};
use sync_g_drive::db::Database;
use sync_g_drive::engine::bandwidth::ProgressTracker;
// ── Modèles de données pour le Mock ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MockFile {
    pub id: String,
    pub name: String,
    pub parent_id: String,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub enum MockCall {
    ListRemote { root_id: String },
    Mkdir { parent_id: String, name: String },
    Upload {
        local_path: String,
        parent_id: String,
        file_name: String,
        existing_id: Option<String>
    },
    Delete { file_id: String },
    Rename {
        file_id: String,
        new_name: Option<String>,
        new_parent_id: Option<String>
    },
    GetChanges { cursor: Option<String> },
    CheckHealth,
    Shutdown,
}


// ── Le faux Google Drive (MockProvider) ───────────────────────────────────────

/// Simule l'API Google Drive en mémoire vive pour les tests.
/// Enregistre chaque appel dans `calls` pour qu'on puisse vérifier
/// le comportement du moteur de synchronisation (Assertions).
#[derive(Clone)]
pub struct MockProvider {
    pub files: Arc<Mutex<HashMap<String, MockFile>>>,
    pub dirs: Arc<Mutex<HashMap<String, String>>>, // path -> id
    pub next_id: Arc<AtomicU64>,
    pub calls: Arc<Mutex<Vec<MockCall>>>,
    pub is_offline: Arc<AtomicBool>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockProvider {
    pub fn new() -> Self {
        let mut dirs = HashMap::new();
        // On initialise toujours un dossier racine fictif
        dirs.insert("root".to_string(), "ROOT_ID".to_string());

        Self {
            files: Arc::new(Mutex::new(HashMap::new())),
            dirs: Arc::new(Mutex::new(dirs)),
            next_id: Arc::new(AtomicU64::new(1000)),
            calls: Arc::new(Mutex::new(Vec::new())),
            is_offline: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Génère un identifiant unique (ex: "MOCK_ID_1001")
    fn generate_id(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("MOCK_ID_{}", id)
    }

    /// Récupère tout l'historique des appels (pour les `assert_eq!`)
    pub fn get_calls(&self) -> Vec<MockCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Efface l'historique des appels
    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}

use sync_g_drive::remote::{ChangesPage, HealthStatus, RemoteIndex, RemoteProvider, UploadResult};

// ── Implémentation du Trait pour le Mock ───────────────────────────────────

#[async_trait]
impl RemoteProvider for MockProvider {

    async fn list_remote(&self, root_id: &str) -> Result<RemoteIndex> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::ListRemote {
                root_id: root_id.to_string(),
            });

        // Retourne un index vide basé sur ta structure exacte
        Ok(RemoteIndex {
            files: vec![],
            dirs: vec![],
        })
    }

    async fn mkdir(&self, parent_id: &str, name: &str) -> Result<String> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::Mkdir {
            parent_id: parent_id.to_string(),
            name: name.to_string(),
        });

        let new_id = self.generate_id();
        self.dirs
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .insert(name.to_string(), new_id.clone());
        Ok(new_id)
    }

    async fn upload(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,
        _tracker: Arc<ProgressTracker>,
    ) -> Result<UploadResult> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::Upload {
            local_path: local_path.to_string_lossy().to_string(),
            parent_id: parent_id.to_string(),
            file_name: file_name.to_string(),
            existing_id: existing_id.map(|s| s.to_string()),
        });

        if self.is_offline.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("Connexion perdue pendant l'upload"));
        }
        let fake_id = self.generate_id();
        let size_bytes = tokio::fs::metadata(local_path).await.map(|m| m.len()).unwrap_or(0);

        // 2. On calcule son vrai MD5 pour satisfaire le moteur
        let real_md5 = sync_g_drive::engine::integrity::compute_hash(local_path).await?;

        Ok(UploadResult {
            drive_id: fake_id,
            md5_checksum: real_md5, // <-- Fini le "mock_md5_hash" !
            size_bytes,
        })
    }

    async fn delete(&self, file_id: &str) -> Result<()> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::Delete {
            file_id: file_id.to_string(),
        });

        self.files
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .retain(|_, f| f.id != file_id);
        Ok(())
    }

    async fn rename(
        &self,
        file_id: &str,
        new_name: Option<&str>,
        new_parent_id: Option<&str>,
    ) -> Result<()> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::Rename {
            file_id: file_id.to_string(),
            new_name: new_name.map(|s| s.to_string()),
            new_parent_id: new_parent_id.map(|s| s.to_string()),
        });
        Ok(())
    }

    async fn get_changes(&self, cursor: Option<&str>) -> Result<ChangesPage> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::GetChanges {
            cursor: cursor.map(|s| s.to_string()),
        });

        // Retourne une page de deltas vide
        Ok(ChangesPage {
            changes: vec![],
            new_cursor: "mock_next_cursor".to_string(),
            has_more: false,
        })
    }

    async fn check_health(&self) -> Result<HealthStatus> {
        self.calls
            .lock()
            .map_err(|e| anyhow::anyhow!("Mutex empoisonné: {}", e))?
            .push(MockCall::CheckHealth);

        if self.is_offline.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("Erreur réseau simulée (Offline)"));
        }

        // Simule une connexion Google Drive saine avec 15 Go de quota
        Ok(HealthStatus::Ok {
            email: "test_dry_run@gmail.com".to_string(),
            quota_used: 1024 * 1024, // 1 Mo utilisé
            quota_total: 15 * 1024 * 1024 * 1024, // 15 Go au total
        })
    }

    async fn shutdown(&self) {
        self.calls.lock().unwrap().push(MockCall::Shutdown);
    }
}

// ── Environnement de Test Isolé ───────────────────────────────────────────────

/// Regroupe tout le nécessaire pour lancer un test sans polluer le vrai système.
pub struct TestEnv {
    pub local_dir: TempDir,
    pub db_file: TempDir,
    pub db: Database,
    pub config: AppConfig,
    pub mock_provider: MockProvider,
}

impl TestEnv {
    /// Crée un environnement flambant neuf pour un test.
    pub fn setup() -> Self {
        let local_dir = tempfile::tempdir().expect("Impossible de créer le dossier local de test");
        let db_dir = tempfile::tempdir().expect("Impossible de créer le dossier de DB de test");
        let db_path = db_dir.path().join("index.db");

        let db = Database::open(&db_path).expect("Impossible d'ouvrir la DB de test");
        db.init_and_migrate().expect("Impossible de migrer la DB de test");

        let mut config = AppConfig::default();
        config.sync_pairs.push(SyncPair {
            name: "TestSync".into(),
            local_path: local_dir.path().to_path_buf(),
            remote_folder_id: "ROOT_ID".into(),
            provider: "MockProvider".into(),
            active: true,
            ignore_patterns: vec![],
        });

        Self {
            local_dir,
            db_file: db_dir,
            db,
            config,
            mock_provider: MockProvider::new(),
        }
    }
}