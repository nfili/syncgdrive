use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::helpers::{MockCall, TestEnv};
use sync_g_drive::engine::{EngineCommand, SyncEngine};

// ── 1. NOUVEAU FICHIER ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_watcher_detects_new_file() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // On laisse inotify s'attacher au dossier
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Action : Création
    let file_path = sync_dir.join("fichier_spontane.txt");
    std::fs::write(&file_path, "Création détectée").unwrap();

    // Vérification
    let wait_res = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_res.is_ok(), "Le watcher n'a pas détecté le nouveau fichier");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 2. SUPPRESSION ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_watcher_detects_delete() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    // On crée le fichier avant le lancement
    let file_path = sync_dir.join("a_supprimer.txt");
    std::fs::write(&file_path, "A jeter").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // On fait un ForceScan initial pour que le moteur "connaisse" le fichier
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    env.mock_provider.clear_calls();
    tokio::time::sleep(Duration::from_millis(300)).await; // Laisse inotify souffler

    // Action : Suppression
    std::fs::remove_file(&file_path).unwrap();

    // Vérification
    let wait_del = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Delete { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_del.is_ok(), "Le watcher n'a pas détecté la suppression");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 3. DEBOUNCE (ANTI-REBOND) ─────────────────────────────────────────────────



#[tokio::test]
async fn test_watcher_debounce() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Action : 5 modifications ultra rapides (rafale)
    let file_path = sync_dir.join("fichier_rafale.txt");
    for i in 0..5 {
        std::fs::write(&file_path, format!("Écriture {}", i)).unwrap();
        // Une sauvegarde classique de logiciel génère de multiples micro-écritures
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // On attend un peu pour laisser passer la fenêtre de debounce
    // (Généralement configurée entre 200ms et 1s dans l'engine).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Vérification : L'anti-rebond a dû fusionner les 5 événements en 1 seul Upload
    let calls = env.mock_provider.get_calls();
    let upload_count = calls.iter().filter(|c| matches!(c, MockCall::Upload { .. })).count();

    assert_eq!(upload_count, 1, "Le debounce a échoué : il devrait y avoir exactement 1 upload pour la rafale");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 4. RENAME EN INTERNE ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_watcher_rename_within() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    let file_path_old = sync_dir.join("nom_original.txt");
    std::fs::write(&file_path_old, "Contenu").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // ForceScan pour l'enregistrer dans la DB locale et sur le Mock
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    env.mock_provider.clear_calls();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Action : Renommage
    let file_path_new = sync_dir.join("nouveau_nom.txt");
    std::fs::rename(&file_path_old, &file_path_new).unwrap();

    let wait_rename = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = env.mock_provider.get_calls();

            let has_rename = calls.iter().any(|c| matches!(c, MockCall::Rename { .. }));
            let has_delete = calls.iter().any(|c| matches!(c, MockCall::Delete { .. }));
            let has_upload = calls.iter().any(|c| matches!(c, MockCall::Upload { .. }));

            // On sort de la boucle si on a trouvé l'une ou l'autre des stratégies de renommage
            if has_rename || (has_delete && has_upload) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_rename.is_ok(), "Le watcher n'a pas déclenché d'appel Rename sur le cloud");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 5. DÉPLACEMENT DEPUIS L'EXTÉRIEUR ─────────────────────────────────────────

#[tokio::test]
async fn test_watcher_rename_from_outside() {
    let env = TestEnv::setup();

    // Le dossier surveillé
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    // Le dossier EXTÉRIEUR non surveillé
    let outside_dir = env.local_dir.path().join("dossier_externe");
    std::fs::create_dir_all(&outside_dir).unwrap();

    // On crée le fichier DEHORS
    let file_outside = outside_dir.join("fichier_import.txt");
    std::fs::write(&file_outside, "Données importées").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Action : Déplacement depuis l'extérieur VERS l'intérieur (inotify verra ça comme une création)
    let file_inside = sync_dir.join("fichier_import.txt");
    std::fs::rename(&file_outside, &file_inside).unwrap();

    // Vérification : Ça doit déclencher un Upload (et non un Rename !)
    let wait_upload = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_upload.is_ok(), "Le fichier entrant n'a pas été uploadé par le watcher");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}