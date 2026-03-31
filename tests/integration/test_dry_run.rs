use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::helpers::{MockCall, TestEnv};
use sync_g_drive::engine::{EngineCommand, SyncEngine};

#[tokio::test]
async fn test_dry_run_prevents_all_mutations() {
    let env = TestEnv::setup();

    // 1. On récupère le dossier surveillé dynamiquement
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    std::fs::create_dir_all(sync_dir).unwrap();

    // 2. On crée un fichier qui nécessiterait normalement un Upload
    let file_path = sync_dir.join("fichier_test_dry_run.txt");
    std::fs::write(&file_path, "Contenu à ne surtout pas envoyer").unwrap();

    let mock = Arc::new(env.mock_provider.clone());

    // ── LE COEUR DU TEST : dry_run est à TRUE ──
    let engine = SyncEngine::new(Arc::new(env.config.clone()), true, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, mut _status_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 3. On force le scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 4. On attend 1 seconde pour être sûr que le moteur a eu le temps de faire son cycle complet.
    // (Ici le timeout est acceptable car on veut vérifier que RIEN ne s'est passé).
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // 5. VÉRIFICATION STRICTE
    let calls = env.mock_provider.get_calls();

    // Le moteur a le droit de lire le cloud (ListRemote) ou de vérifier la santé (CheckHealth)
    let has_read = calls
        .iter()
        .any(|c| matches!(c, MockCall::ListRemote { .. }));
    assert!(
        has_read,
        "Le moteur aurait dû au moins lister le contenu distant pour calculer le diff"
    );

    // Mais il a interdiction formelle d'écrire !
    let has_writes = calls.iter().any(|c| {
        matches!(
            c,
            MockCall::Upload { .. }
                | MockCall::Mkdir { .. }
                | MockCall::Delete { .. }
                | MockCall::Rename { .. }
        )
    });

    assert!(
        !has_writes,
        "DANGER : Le moteur a tenté une opération d'écriture réseau en mode DRY RUN !"
    );

    // Nettoyage final
    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}
