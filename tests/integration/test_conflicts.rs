use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::helpers::{MockCall, TestEnv};
use sync_g_drive::engine::{EngineCommand, SyncEngine};

// ── 1. LE LOCAL ÉCRASE LE CLOUD (MODIFICATION) ─────────────────────────────

#[tokio::test]
async fn test_conflict_local_wins_on_modify() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    let file_path = sync_dir.join("document_important.txt");
    std::fs::write(&file_path, "Version 1").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // 1. Upload initial pour établir la base
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    // Respiration temporelle pour l'horodatage système et SQLite
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.mock_provider.clear_calls();

    // 2. Action : On modifie violemment le fichier en local
    // Dans notre architecture "Local Wins", peu importe si le cloud a changé entre temps,
    // cette modification DOIT déclencher un Upload pour imposer la loi du disque dur.
    std::fs::write(&file_path, "Version 2 (Vérité Locale)").unwrap();

    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. Vérification : Le moteur force l'Upload
    let wait_conflict = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_conflict.is_ok(), "Le moteur n'a pas imposé la version locale (Upload manquant)");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 2. LE LOCAL FORCE LA SUPPRESSION ───────────────────────────────────────

#[tokio::test]
async fn test_conflict_local_wins_on_delete() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    let file_path = sync_dir.join("fichier_a_purger.txt");
    std::fs::write(&file_path, "Données obsolètes").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // 1. Upload initial
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    env.mock_provider.clear_calls();

    // 2. Action : Suppression locale pure et simple
    // Même si le fichier existait toujours sur le cloud, le local dicte sa loi.
    std::fs::remove_file(&file_path).unwrap();

    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. Vérification : Ordre de suppression envoyé
    let wait_delete = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Delete { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_delete.is_ok(), "Le moteur n'a pas propagé la suppression locale sur le cloud");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 3. LE LOCAL FORCE LE RENOMMAGE ─────────────────────────────────────────

#[tokio::test]
async fn test_conflict_local_wins_on_rename() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    let old_path = sync_dir.join("nom_obsolete.txt");
    std::fs::write(&old_path, "Données à conserver").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // 1. Upload initial
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    env.mock_provider.clear_calls();

    // 2. Action : On renomme localement
    let new_path = sync_dir.join("nom_definitif.txt");
    std::fs::rename(&old_path, &new_path).unwrap();

    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. Vérification : Le cloud doit refléter le nouveau nom
    let wait_rename = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = env.mock_provider.get_calls();

            let has_rename = calls.iter().any(|c| matches!(c, MockCall::Rename { .. }));
            let has_delete = calls.iter().any(|c| matches!(c, MockCall::Delete { .. }));
            let has_upload = calls.iter().any(|c| matches!(c, MockCall::Upload { .. }));

            if has_rename || (has_delete && has_upload) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_rename.is_ok(), "Le moteur n'a pas imposé le renommage local sur le cloud");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}