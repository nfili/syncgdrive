use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::helpers::TestEnv;
use sync_g_drive::engine::{EngineCommand, EngineStatus, SyncEngine};

// ── 1. TEST DE PAUSE ET REPRISE ──────────────────────────────────────────────

#[tokio::test]
async fn test_command_pause_resume() {
    let env = TestEnv::setup();
    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    // On garde le récepteur de statuts pour écouter le moteur
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    // 1. On attend que le moteur soit prêt (Idle).
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    // 2. Action : On envoie la commande Pause
    cmd_tx.send(EngineCommand::Pause).await.unwrap();

    // 3. Vérification : Le moteur doit confirmer qu'il est en pause
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Paused)).await;

    // 4. Action : On reprend
    cmd_tx.send(EngineCommand::Resume).await.unwrap();

    // 5. Vérification : Il doit retourner en Idle (ou Syncing s'il y avait du travail)
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    // Nettoyage
    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 2. TEST D'ARRÊT GRACIEUX (SHUTDOWN) ──────────────────────────────────────

#[tokio::test]
async fn test_command_shutdown() {
    let env = TestEnv::setup();
    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    // Action : Ordre de fermeture
    cmd_tx.send(EngineCommand::Shutdown).await.unwrap();

    // Vérification 1 : Le moteur annonce son arrêt
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Stopped)).await;

    // Vérification 2 : Le thread (la tâche tokio) doit se terminer proprement sans timeout
    let result = tokio::time::timeout(Duration::from_secs(2), engine_handle).await;
    assert!(result.is_ok(), "Le moteur a refusé de s'arrêter dans le temps imparti !");
}

// ── 3. TEST DE MISE À JOUR DE LA CONFIGURATION À CHAUD ──────────────────────

#[tokio::test]
async fn test_command_apply_config() {
    let mut env = TestEnv::setup();
    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    // 1. On prépare une nouvelle configuration mutée
    env.config.max_workers = 42;
    let new_config = Arc::new(env.config.clone());

    // 2. Action : On l'applique à chaud
    cmd_tx.send(EngineCommand::ApplyConfig(new_config)).await.unwrap();

    // 3. Vérification : Le moteur doit digérer l'info et retourner à son cycle normal
    // Sans crasher, et se remettre en attente de travail.
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── 4. TEST DES ÉTATS D'INTERFACE (PARAMÈTRES ET AIDE) ──────────────────────

#[tokio::test]
async fn test_command_ui_states() {
    let env = TestEnv::setup();
    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine.run(env.db, shutdown.clone(), cmd_rx, status_tx).await
    });

    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    // Test Ouverture des paramètres
    cmd_tx.send(EngineCommand::OpenSettings).await.unwrap();
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Settings)).await;

    // Test Reprise depuis les paramètres
    cmd_tx.send(EngineCommand::Resume).await.unwrap();
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Idle)).await;

    // Test Ouverture de l'aide
    cmd_tx.send(EngineCommand::OpenHelp).await.unwrap();
    wait_for_status(&mut status_rx, |s| matches!(s, EngineStatus::Help)).await;

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

// ── FONCTION UTILITAIRE DE SCRUTATION DES STATUTS ────────────────────────────

/// Lit le canal des statuts jusqu'à trouver celui qui correspond au prédicat,
/// ou échoue au bout de 2 secondes.
async fn wait_for_status<F>(status_rx: &mut mpsc::UnboundedReceiver<EngineStatus>, predicate: F)
where
    F: Fn(&EngineStatus) -> bool
{
    let timeout_res = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(status) = status_rx.recv().await {
            if predicate(&status) {
                return;
            }
        }
    }).await;

    assert!(timeout_res.is_ok(), "Le moteur n'a pas émis le statut attendu dans les temps");
}