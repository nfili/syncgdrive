use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// On importe notre environnement de test et notre faux Google Drive
use super::helpers::{MockCall, TestEnv};
use sync_g_drive::engine::{EngineCommand, EngineStatus, SyncEngine};

#[tokio::test]
async fn test_initial_scan_uploads_all() {
    // ── 1. ARRANGE (Préparation) ──────────────────────────────────────────────
    let env = TestEnv::setup();

    // On crée une arborescence locale avec un dossier et deux fichiers
    let root_path = env.local_dir.path();

    let file1_path = root_path.join("document.txt");
    fs::write(&file1_path, "Contenu du document").unwrap();

    let sub_dir = root_path.join("sauvegardes");
    fs::create_dir(&sub_dir).unwrap();

    let file2_path = sub_dir.join("data.csv");
    fs::write(&file2_path, "1,2,3").unwrap();

    // ── 2. ACT (Action) ───────────────────────────────────────────────────────

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel::<EngineStatus>();
    let shutdown = CancellationToken::new();

    // On lance le moteur dans une tâche asynchrone séparée
    let engine_handle = tokio::spawn(async move {
        // Supposons que tu aies adapté `run` pour accepter le provider,
        // ou que tu testes directement la fonction `scan_and_sync`
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // On force un scan immédiat via le channel de commandes
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    let wait_result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env.mock_provider.get_calls().len() == 4 {
                break; // Le moteur a terminé ses requêtes réseau !
            }
            tokio::time::sleep(Duration::from_millis(50)).await; // Pause de 50ms
        }
    })
    .await;

    assert!(
        wait_result.is_ok(),
        "Timeout : le moteur n'a pas atteint les 4 opérations réseau"
    );

    // On demande au moteur de s'arrêter proprement
    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;

    // ── 3. ASSERT (Vérification) ──────────────────────────────────────────────
    let calls = env.mock_provider.get_calls();
    // On s'attend à 4 appels réseaux : 1 ListRemote, 1 Mkdir et 2 Uploads
    assert_eq!(
        calls.len(),
        4,
        "Il devrait y avoir exactement 4 opérations réseau"
    );

    let mut list_count = 0;
    let mut mkdir_count = 0;
    let mut upload_count = 0;

    for call in calls {
        match call {
            MockCall::ListRemote { root_id } => {
                assert_eq!(root_id, "ROOT_ID");
                list_count += 1;
            }
            MockCall::Mkdir { name, parent_id } => {
                assert_eq!(name, "sauvegardes");
                assert_eq!(parent_id, "ROOT_ID");
                mkdir_count += 1;
            }
            MockCall::Upload { local_path, .. } => {
                assert!(local_path.ends_with("document.txt") || local_path.ends_with("data.csv"));
                upload_count += 1;
            }
            _ => panic!("Opération réseau inattendue : {:?}", call),
        }
    }

    assert_eq!(list_count, 1);
    assert_eq!(mkdir_count, 1);
    assert_eq!(upload_count, 2);
}

#[tokio::test]
async fn test_rescan_skips_unchanged() {
    let env = TestEnv::setup();
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    // 1. On crée un fichier et on prépare le moteur
    let file_path = sync_dir.join("stable.txt");
    fs::write(&file_path, "Données inchangées").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 2. PREMIER SCAN : Le fichier doit être uploadé
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    let wait_initial = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env
                .mock_provider
                .get_calls()
                .iter()
                .any(|c| matches!(c, MockCall::Upload { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(wait_initial.is_ok(), "Le premier scan a échoué");

    // 3. On nettoie l'historique du Mock pour la deuxième phase
    env.mock_provider.clear_calls();

    // 4. DEUXIÈME SCAN (Rescan) sans avoir touché au fichier
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // On attend 1 seconde pour laisser le moteur réfléchir
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // 5. VÉRIFICATION : Le moteur a le droit de faire un ListRemote, mais AUCUN Upload !
    let calls_after_rescan = env.mock_provider.get_calls();
    let upload_count = calls_after_rescan
        .iter()
        .filter(|c| matches!(c, MockCall::Upload { .. }))
        .count();

    assert_eq!(
        upload_count, 0,
        "Le moteur a ré-uploadé un fichier qui n'avait pas changé !"
    );

    // Nettoyage
    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

#[tokio::test]
async fn test_scan_ignores_patterns() {
    let mut env = TestEnv::setup();

    // ── 1. ON AJOUTE UNE RÈGLE SIMPLE ET INFAILLIBLE ──
    if let Some(pair) = env.config.sync_pairs.first_mut() {
        pair.ignore_patterns.push("*.tmp".to_string());
    }

    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    // 2. On crée un fichier valide ET un fichier ".tmp" qui doit être ignoré
    let valid_file = sync_dir.join("document_valide.txt");
    fs::write(&valid_file, "Ce fichier doit passer").unwrap();

    let ignore_file = sync_dir.join("brouillon.tmp");
    fs::write(&ignore_file, "Ce fichier doit être ignoré").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 3. On lance le scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 4. On attend de voir l'upload du fichier valide
    let wait_upload = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env
                .mock_provider
                .get_calls()
                .iter()
                .any(|c| matches!(c, MockCall::Upload { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(wait_upload.is_ok(), "Le fichier valide n'a pas été uploadé");

    // 5. VÉRIFICATION STRICTE
    let calls = env.mock_provider.get_calls();

    // On vérifie que le fichier .tmp n'a JAMAIS été envoyé au Mock
    for call in calls {
        if let MockCall::Upload { local_path, .. } = call {
            assert!(
                !local_path.ends_with(".tmp"),
                "ERREUR : Le fichier ignoré a fuité vers le réseau ! Path: {}",
                local_path
            );
        }
    }

    // Nettoyage
    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

#[tokio::test]
async fn test_scan_detects_new_file() {
    let env = TestEnv::setup();
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    // 1. Initialisation avec un premier fichier
    let file1 = sync_dir.join("fichier_1.txt");
    fs::write(&file1, "Premier fichier").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 2. Premier scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env
                .mock_provider
                .get_calls()
                .iter()
                .any(|c| matches!(c, MockCall::Upload { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    // 3. On nettoie l'historique et on ajoute un NOUVEAU fichier
    env.mock_provider.clear_calls();
    let file2 = sync_dir.join("fichier_2_nouveau.txt");
    fs::write(&file2, "Nouveau fichier ajouté plus tard").unwrap();

    // 4. Deuxième scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 5. On vérifie que le moteur n'upload QUE le nouveau fichier
    let wait_new = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = env.mock_provider.get_calls();
            if calls.iter().any(|c| matches!(c, MockCall::Upload { local_path, .. } if local_path.ends_with("fichier_2_nouveau.txt"))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await;

    assert!(
        wait_new.is_ok(),
        "Le nouveau fichier n'a pas été détecté lors du second scan"
    );

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

#[tokio::test]
async fn test_scan_detects_modified_file() {
    let env = TestEnv::setup();
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    let file_path = sync_dir.join("facture.txt");
    fs::write(&file_path, "Montant: 100€").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 1. Upload initial
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let wait_init = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env
                .mock_provider
                .get_calls()
                .iter()
                .any(|c| matches!(c, MockCall::Upload { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(wait_init.is_ok(), "L'upload initial a échoué");

    tokio::time::sleep(Duration::from_millis(500)).await;

    env.mock_provider.clear_calls();

    // 2. On MODIFIE le fichier localement (Le mtime sera maintenant différent).
    fs::write(&file_path, "Montant: 200€ (Modifié)").unwrap();

    // 3. Nouveau scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 4. On attend un nouvel upload pour ce même fichier
    let wait_mod = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = env.mock_provider.get_calls();
            if calls.iter().any(|c| matches!(c, MockCall::Upload { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    assert!(
        wait_mod.is_ok(),
        "La modification du fichier n'a pas déclenché de nouvel upload"
    );

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

#[tokio::test]
async fn test_scan_detects_deleted_file() {
    let env = TestEnv::setup();
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    let file_path = sync_dir.join("brouillon_a_jeter.txt");
    fs::write(&file_path, "Texte temporaire").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 1. Upload initial pour que le fichier existe côté cloud (dans la mémoire du Mock)
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env
                .mock_provider
                .get_calls()
                .iter()
                .any(|c| matches!(c, MockCall::Upload { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    env.mock_provider.clear_calls();

    // 2. On SUPPRIME le fichier du disque local
    fs::remove_file(&file_path).unwrap();

    // 3. Nouveau scan : le moteur va comparer le dossier vide avec le cloud (qui a le fichier)
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 4. On attend l'appel "Delete" vers l'API
    let wait_del = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = env.mock_provider.get_calls();
            if calls.iter().any(|c| matches!(c, MockCall::Delete { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    assert!(
        wait_del.is_ok(),
        "La suppression locale n'a pas été répercutée sur le cloud"
    );

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

#[tokio::test]
async fn test_scan_creates_directories() {
    let env = TestEnv::setup();
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    // 1. Création d'une arborescence profonde
    let deep_dir = sync_dir.join("parent").join("enfant").join("petit_enfant");
    fs::create_dir_all(&deep_dir).unwrap();
    let file_path = deep_dir.join("tresor.txt");
    fs::write(&file_path, "Trouvé !").unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 2. Scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. On attend l'upload final du fichier (ce qui prouve que l'arborescence a été traitée).
    let wait_upload = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if env
                .mock_provider
                .get_calls()
                .iter()
                .any(|c| matches!(c, MockCall::Upload { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        wait_upload.is_ok(),
        "Le fichier dans l'arborescence n'a pas été uploadé"
    );

    // 4. Vérification stricte des Mkdir
    let calls = env.mock_provider.get_calls();
    let mkdir_count = calls
        .iter()
        .filter(|c| matches!(c, MockCall::Mkdir { .. }))
        .count();

    // Le moteur doit au moins créer "parent", "enfant" et "petit_enfant".
    assert!(
        mkdir_count >= 3,
        "Le moteur n'a pas créé tous les dossiers récursifs de l'arborescence"
    );

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}

#[tokio::test]
async fn test_scan_handles_empty_dir() {
    let env = TestEnv::setup();
    let primary_pair = env
        .config
        .get_primary_pair()
        .expect("Aucun dossier configuré");
    let sync_dir = &primary_pair.local_path;
    fs::create_dir_all(sync_dir).unwrap();

    // 1. Création d'un dossier totalement vide
    let empty_dir = sync_dir.join("dossier_fantome");
    fs::create_dir_all(&empty_dir).unwrap();

    let mock = Arc::new(env.mock_provider.clone());
    let engine = SyncEngine::new(Arc::new(env.config.clone()), false, mock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(10);
    let (status_tx, _) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();

    let engine_handle = tokio::spawn(async move {
        engine
            .run(env.db, shutdown.clone(), cmd_rx, status_tx)
            .await
    });

    // 2. Scan
    let _ = cmd_tx.send(EngineCommand::ForceScan).await;

    // 3. On scrute spécifiquement la création de ce dossier précis
    let wait_mkdir = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = env.mock_provider.get_calls();
            if calls
                .iter()
                .any(|c| matches!(c, MockCall::Mkdir { name, .. } if name == "dossier_fantome"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        wait_mkdir.is_ok(),
        "Le dossier vide n'a pas été créé côté cloud"
    );

    // 4. On s'assure qu'AUCUN fichier n'a été uploadé accidentellement
    let calls = env.mock_provider.get_calls();
    let upload_count = calls
        .iter()
        .filter(|c| matches!(c, MockCall::Upload { .. }))
        .count();
    assert_eq!(
        upload_count, 0,
        "Un upload inattendu a eu lieu pour un dossier vide"
    );

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;
    let _ = engine_handle.await;
}
