use std::path::PathBuf;
use sync_g_drive::config::AppConfig;

// ── 1. SÉRIALISATION / DÉSÉRIALISATION (ROUNDTRIP) ──────────────────────────

#[test]
fn test_full_config_roundtrip() {
    // 1. On crée une configuration avec des valeurs modifiées
    let toml_base = r#"
        [[sync_pairs]]
        name = "Dossier Principal"
        local_path = "/tmp/base"
        remote_folder_id = "BASE_ID"
    "#;
    let (mut original, _) = AppConfig::parse_and_migrate(toml_base).expect("Base TOML invalide");

    original.max_workers = 12;
    original.advanced.debounce_ms = 777;

    if let Some(pair) = original.sync_pairs.first_mut() {
        pair.local_path = PathBuf::from("/chemin/custom/test");
        pair.remote_folder_id = "CUSTOM_ID_999".to_string();
    }

    // 2. Action : On sérialise en chaîne de caractères TOML
    let toml_str = toml::to_string(&original).expect("Échec de la sérialisation TOML");

    // 3. Action : On recrée un objet à partir de cette chaîne
    let (deserialized, _) = AppConfig::parse_and_migrate(&toml_str)
        .expect("Échec de la désérialisation TOML");

    // 4. Vérification : Les données doivent être strictement identiques
    assert_eq!(
        original.max_workers, deserialized.max_workers,
        "La valeur max_workers a été altérée"
    );
    assert_eq!(
        original.advanced.debounce_ms, deserialized.advanced.debounce_ms,
        "La valeur debounce_ms a été altérée"
    );

    let orig_pair = original.sync_pairs.first().unwrap();
    let des_pair = deserialized.sync_pairs.first().unwrap();

    assert_eq!(orig_pair.local_path, des_pair.local_path);
    assert_eq!(orig_pair.remote_folder_id, des_pair.remote_folder_id);
}

// ── 2. APPLICATION DES VALEURS PAR DÉFAUT (PARTIAL ADVANCED) ────────────────

#[test]
fn test_partial_advanced_defaults() {
    // 1. Un TOML où l'utilisateur n'a défini QU'UNE SEULE variable avancée
    let toml = r#"
        [[sync_pairs]]
        name = "Pair 1"
        local_path = "/tmp/test"
        remote_folder_id = "DRIVE_123"

        [advanced]
        debounce_ms = 999
    "#;

    // 2. Action
    let (config, _) = AppConfig::parse_and_migrate(toml).expect("Le TOML doit être valide");

    // 3. Vérification
    assert_eq!(
        config.advanced.debounce_ms, 999,
        "La valeur spécifiée par l'utilisateur a été ignorée"
    );
    assert_eq!(
        config.max_workers, 4, // 4 est ta valeur par défaut dans config.rs
        "Les valeurs non spécifiées doivent prendre leur valeur par défaut"
    );
}

// ── 3. CHARGEMENT DE MULTIPLES DOSSIERS (MULTI SYNC PAIRS) ──────────────────

#[test]
fn test_multi_sync_pairs() {
    let toml = r#"
        [[sync_pairs]]
        name = "Documents"
        local_path = "/tmp/docs"
        remote_folder_id = "ID_1"

        [[sync_pairs]]
        name = "Photos"
        local_path = "/tmp/photos"
        remote_folder_id = "ID_2"

        [[sync_pairs]]
        name = "Projets"
        local_path = "/tmp/projets"
        remote_folder_id = "ID_3"
    "#;

    let (config, _) = AppConfig::parse_and_migrate(toml).unwrap();

    assert_eq!(config.sync_pairs.len(), 3, "Le parseur n'a pas détecté les 3 dossiers");
    assert_eq!(config.sync_pairs[0].remote_folder_id, "ID_1");
    assert_eq!(config.sync_pairs[1].remote_folder_id, "ID_2");
    assert_eq!(config.sync_pairs[2].remote_folder_id, "ID_3");
}

// ── 4. FUSION DES RÈGLES D'EXCLUSION (IGNORE PATTERNS MERGE) ────────────────



#[test]
fn test_ignore_patterns_parsed() {
    let toml = r#"
        [[sync_pairs]]
        name = "Dev"
        local_path = "/tmp/test"
        remote_folder_id = "DRIVE_123"
        ignore_patterns = ["*.tmp", ".git/"]
    "#;

    let (config, _) = AppConfig::parse_and_migrate(toml).unwrap();
    let pair = config.sync_pairs.first().unwrap();

    assert_eq!(pair.ignore_patterns.len(), 2, "Les patterns n'ont pas été lus correctement");
    assert!(pair.ignore_patterns.contains(&"*.tmp".to_string()));
    assert!(pair.ignore_patterns.contains(&".git/".to_string()));
}