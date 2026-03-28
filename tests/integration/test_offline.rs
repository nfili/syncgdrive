use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::helpers::{MockCall, TestEnv};
use sync_g_drive::engine::{EngineCommand, SyncEngine};

// ── 1. MISE EN FILE D'ATTENTE (OFFLINE) ──────────────────────────────────────

#[tokio::test]
async fn test_offline_queues_events() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    // 1. On coupe le réseau AVANT de lancer le moteur
    env.mock_provider.is_offline.store(true, Ordering::SeqCst);

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx,_) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // 2. Action : Création d'un fichier pendant la coupure
    let file_path = sync_dir.join("rapport_sans_internet.txt");
    std::fs::write(&file_path, "Données en attente").unwrap();

    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. Vérification : Le moteur doit tenter l'upload, échouer, et le mettre en queue
    let wait_offline = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_offline.is_ok(), "Le moteur n'a pas tenté de traiter l'événement hors-ligne");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 2. VIDAGE DE LA FILE D'ATTENTE (RETOUR ONLINE) ───────────────────────────

#[tokio::test]
async fn test_online_flushes_queue() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    // 1. Démarrage en mode hors-ligne
    env.mock_provider.is_offline.store(true, Ordering::SeqCst);

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    let file_path = sync_dir.join("fichier_a_flusher.txt");
    std::fs::write(&file_path, "Contenu à synchroniser plus tard").unwrap();
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // On attend que la tentative hors-ligne échoue et soit mise en queue
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    // 2. Action : On rétablit le réseau !
    env.mock_provider.clear_calls(); // On nettoie l'historique pour la vérification
    env.mock_provider.is_offline.store(false, Ordering::SeqCst);

    // On force un cycle pour déclencher le flush (ou on attend la boucle de retry de ton moteur).
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. Vérification : Le fichier est bien uploadé
    let wait_online = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(wait_online.is_ok(), "Le fichier n'a pas été uploadé après le retour du réseau");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 3. DÉDUPLICATION OFFLINE ──────────────────────────────────────────────────

#[tokio::test]
async fn test_offline_dedup() {
    let env = TestEnv::setup();
    let primary_pair = env.config.get_primary_pair().expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    // 1. Démarrage en mode hors-ligne
    env.mock_provider.is_offline.store(true, Ordering::SeqCst);

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    let file_path = sync_dir.join("fichier_modifie_3_fois.txt");

    // 2. Action : 3 modifications espacées pendant la coupure
    for i in 1..=3 {
        std::fs::write(&file_path, format!("Version {}", i)).unwrap();
        let _ = cmd_tx.send(EngineCommand::ForceScan).await;
        // Petite pause pour s'assurer que le moteur a le temps d'enregistrer la tâche
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 3. On rétablit le réseau
    env.mock_provider.clear_calls();
    env.mock_provider.is_offline.store(false, Ordering::SeqCst);

    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 4. On attend que l'upload se fasse
    let wait_flush = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;
    assert!(wait_flush.is_ok(), "Le flush n'a pas eu lieu au retour du réseau");

    // Laisse un instant au moteur pour éventuellement (à tort) envoyer des doublons
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 5. Vérification stricte : Exactement 1 seul upload pour les 3 modifications
    let calls = env.mock_provider.get_calls();
    let upload_count = calls.iter().filter(|c| matches!(c, MockCall::Upload { .. })).count();

    assert_eq!(upload_count, 1, "La déduplication a échoué : le moteur a envoyé plusieurs uploads pour le même fichier !");

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}