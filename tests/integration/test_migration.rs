use std::fs;
use std::path::PathBuf;
use rusqlite::Connection;
use tempfile::tempdir;

use sync_g_drive::config::AppConfig;
use sync_g_drive::db::Database;

// ── 1. MIGRATION DE LA CONFIGURATION (V1 -> V2) ─────────────────────────────

#[tokio::test]
async fn test_config_v1_migrated() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let v1_content = r#"
local_root = "/tmp/test"
remote_root = "DRIVE_123"
"#;
    fs::write(&config_path, v1_content).unwrap();

    let (config, _) = AppConfig::load_from_path(&config_path).expect("Erreur de chargement");

    assert_eq!(config.sync_pairs.len(), 1, "La migration n'a pas créé la sync_pair");
    assert_eq!(config.sync_pairs.first().unwrap().local_path, PathBuf::from("/tmp/test"));
}

// ── 2. CRÉATION DU BACKUP DE CONFIGURATION ──────────────────────────────────

#[tokio::test]
async fn test_config_v1_backup_created() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let v1_content = "local_root = \"/tmp/test\"\n";
    fs::write(&config_path, v1_content).unwrap();

    let _ = AppConfig::load_from_path(&config_path).unwrap();

    // CORRECTION : L'extension générée par config.rs est .toml.v1.bak
    let backup_path = dir.path().join("config.v1.bak");
    assert!(
        backup_path.exists(),
        "Le fichier de sauvegarde .toml.v1.bak n'a pas été créé"
    );
}

// ── 3. MIGRATION DE LA BASE DE DONNÉES (V1 -> V2) ───────────────────────────

#[tokio::test]
async fn test_db_v1_migrated() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sync.db");

    // 1. On crée le vieux schéma
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE files (id TEXT PRIMARY KEY, local_path TEXT);", []).unwrap();
    } // ← On libère le lock du fichier ici

    // 2. On lance la migration via ta structure Database
    let db = Database::open(&db_path).unwrap(); // Ajout du `mut` au cas où ta fonction le demande
    let db_result = db.init_and_migrate();
    assert!(db_result.is_ok(), "L'initialisation/migration de la DB a échoué");
}

// ── 4. IDEMPOTENCE DE LA MIGRATION DB ───────────────────────────────────────

#[tokio::test]
async fn test_db_migration_idempotent() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sync.db");

    let db = Database::open(&db_path).unwrap();

    // Première passe
    let db1 = db.init_and_migrate();
    assert!(db1.is_ok(), "La première migration a échoué");

    let db2 = db.init_and_migrate();
    assert!(
        db2.is_ok(),
        "La migration n'est pas idempotente !"
    );
}