//! Point d'entrée des tests d'intégration.
//! Cargo va compiler ce fichier comme un exécutable de test séparé,
//! et va chercher automatiquement le contenu des modules dans le dossier `integration/`.

#[path = "integration/helpers.rs"]
pub mod helpers;

#[path = "integration/test_config.rs"]
pub mod test_config;

#[path = "integration/test_migration.rs"]
pub mod test_migration;
#[path = "integration/test_offline.rs"]
pub mod test_offline;

#[path = "integration/test_scan.rs"]
pub mod test_scan;

#[path = "integration/test_watcher.rs"]
pub mod test_watcher;

#[path = "integration/test_dry_run.rs"]
pub mod test_dry_run;

#[path = "integration/test_commands.rs"]
pub mod test_commands;

#[path = "integration/test_conflicts.rs"]
pub mod test_conflicts;
