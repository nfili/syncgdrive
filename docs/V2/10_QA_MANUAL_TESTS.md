# 🧪 SyncGDrive V2 — Procédure de Test Humain (QA Manuelle)

> Ce document décrit les tests manuels à réaliser par un humain pour valider
> que SyncGDrive V2 fonctionne correctement de bout en bout.
>
> **Pré-requis** : Toutes les phases (1–9) implémentées, `cargo test` et `cargo clippy` clean.
> **Durée estimée** : ~2 heures pour la procédure complète.

---

## Légende

- ✅ = OK
- ❌ = Échec (noter le détail)
- ⏭ = Non applicable / skippé

---

## T1. Installation et Premier Lancement

### T1.1 Installation propre

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `make install DESTDIR=/tmp/test-install` | Pas d'erreur, fichiers installés | |
| 2 | Vérifier `/tmp/test-install/usr/bin/syncgdrive` existe | Binaire présent, exécutable | |
| 3 | Vérifier `/tmp/test-install/usr/share/applications/syncgdrive.desktop` | Fichier présent | |
| 4 | `desktop-file-validate /tmp/test-install/usr/share/applications/syncgdrive.desktop` | Pas d'erreur | |
| 5 | Vérifier l'icône SVG dans `hicolor/scalable/apps/` | Fichier présent, taille > 0 | |
| 6 | Vérifier le service systemd | Fichier présent | |

### T1.2 Premier lancement (aucune config existante)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Supprimer `~/.config/syncgdrive/` si existant | Nettoyage ok | |
| 2 | Supprimer `~/.local/share/syncgdrive/` si existant | Nettoyage ok | |
| 3 | Lancer `syncgdrive` | Le programme démarre sans crash | |
| 4 | Vérifier le systray | Icône visible (état « Configuration requise ») | |
| 5 | Vérifier `~/.config/syncgdrive/config.toml` | Fichier créé avec valeurs par défaut | |
| 6 | La fenêtre Settings s'ouvre automatiquement | Oui — car config invalide (aucune sync_pair) | |
| 7 | Fermer la fenêtre Settings sans configurer | L'application reste dans le systray, état `Unconfigured` | |

---

## T2. Configuration et OAuth2

### T2.1 Wizard OAuth2

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Ouvrir les Réglages depuis le menu systray | Fenêtre Settings s'affiche, moteur en pause | |
| 2 | Cliquer « Lier un compte Google » | Le navigateur s'ouvre sur la page de consentement Google | |
| 3 | S'authentifier avec un compte Google | Page de callback affiche « ✅ Autorisation réussie » | |
| 4 | Revenir dans SyncGDrive | Le wizard affiche « Compte lié : user@gmail.com » | |
| 5 | Vérifier le stockage du token | `secret-tool lookup service syncgdrive` retourne quelque chose (ou fichier `.enc` présent) | |

### T2.2 Configuration d'une paire de sync

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Créer un dossier test : `mkdir -p ~/SyncTest/docs` | Dossier créé | |
| 2 | Renseigner le chemin local : `~/SyncTest` | Icône ✅ apparaît | |
| 3 | Renseigner un chemin invalide : `/inexistant` | Icône ❌ apparaît, bouton Enregistrer grisé | |
| 4 | Corriger le chemin : `~/SyncTest` | Icône ✅, bouton Enregistrer actif | |
| 5 | Nommer la paire : « Test QA » | Nom affiché dans la liste | |
| 6 | Cliquer Enregistrer | Toast « Configuration sauvegardée », fenêtre se ferme | |
| 7 | Le moteur reprend | Systray passe à « Scan… » puis « Surveillance active » | |

---

## T3. Scan Initial

### T3.1 Premier scan (dossier quasi vide)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Créer quelques fichiers : `echo "test" > ~/SyncTest/file1.txt` etc. (5 fichiers) | Fichiers créés | |
| 2 | Forcer un scan : menu systray « Synchroniser maintenant » | Scan démarre | |
| 3 | La fenêtre de scan initial s'affiche (DB vide, premier scan) | Fenêtre visible avec barres de progression | |
| 4 | Les barres montrent la phase courante | Phase affichée (Inventaire local, Création dossiers, etc.) | |
| 5 | Les chemins affichent `📂 parent/` + `📄 fichier` | Pas juste un nom de fichier isolé | |
| 6 | Le temps écoulé s'incrémente | Compteur visible | |
| 7 | Scan terminé → fenêtre se ferme automatiquement | Fermeture propre | |
| 8 | Notification « Surveillance des dossiers prête ! » | Notification visible avec nb fichiers/dossiers | |
| 9 | Vérifier sur Google Drive | Les 5 fichiers sont présents dans le bon dossier | |

### T3.2 Fenêtre de scan — bouton Réduire

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Relancer un scan (changer `local_root` pour forcer la fenêtre) | Fenêtre s'affiche | |
| 2 | Cliquer « Réduire dans le systray » | Fenêtre disparaît, scan continue (icône animée) | |
| 3 | Attendre la fin du scan | Notification envoyée (pas de fenêtre) | |

---

## T4. Synchronisation en Temps Réel (Watcher)

### T4.1 Création de fichier

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `echo "nouveau" > ~/SyncTest/new_file.txt` | Fichier créé | |
| 2 | Attendre 2–3 secondes | Icône systray passe en animation (flèches), puis revient à idle | |
| 3 | Vérifier le tooltip | Affiche « ✅ Dernier transfert : 📂 / 📄 new_file.txt » | |
| 4 | Vérifier sur Google Drive | `new_file.txt` présent avec le bon contenu | |

### T4.2 Modification de fichier

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `echo "modifié" >> ~/SyncTest/file1.txt` | Fichier modifié | |
| 2 | Attendre 2–3 secondes | Upload déclenché | |
| 3 | Vérifier sur Google Drive | `file1.txt` contient « test\nmodifié » | |

### T4.3 Suppression de fichier

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `rm ~/SyncTest/file1.txt` | Fichier supprimé localement | |
| 2 | Attendre 2–3 secondes | Action sur Google Drive | |
| 3 | Vérifier sur Google Drive | `file1.txt` dans la corbeille (si `delete_mode = "trash"`) | |

### T4.4 Renommage de fichier

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `mv ~/SyncTest/new_file.txt ~/SyncTest/renamed.txt` | Fichier renommé | |
| 2 | Attendre 2–3 secondes | Rename détecté | |
| 3 | Vérifier sur Google Drive | Fichier renommé (pas un doublon) | |

### T4.5 Création de sous-dossier + fichier

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `mkdir -p ~/SyncTest/sub/deep && echo "deep" > ~/SyncTest/sub/deep/file.txt` | Arborescence créée | |
| 2 | Attendre 5 secondes | Dossiers créés puis fichier uploadé | |
| 3 | Vérifier sur Google Drive | `sub/deep/file.txt` présent dans la bonne hiérarchie | |

### T4.6 Debounce (modifications rapides)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `for i in $(seq 1 10); do echo "$i" > ~/SyncTest/rapid.txt; done` | 10 écritures rapides | |
| 2 | Attendre 3 secondes | **Un seul upload** (pas 10) grâce au debounce | |
| 3 | Vérifier le contenu sur Drive | `rapid.txt` contient « 10 » | |

---

## T5. Progression et Bande Passante

### T5.1 Gros fichier — progression visible

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `dd if=/dev/urandom of=~/SyncTest/big.bin bs=1M count=50` | Fichier 50 Mo créé | |
| 2 | Observer le tooltip pendant l'upload | Vitesse (Mo/s), pourcentage, ETA affichés | |
| 3 | Observer l'icône systray | Animation active pendant le transfert | |
| 4 | Upload terminé | Icône revient à idle, tooltip « Dernier transfert : big.bin » | |
| 5 | Vérifier sur Google Drive | `big.bin` présent, taille = 50 Mo | |

### T5.2 Limitation de bande passante

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Éditer `config.toml` : `upload_limit_kbps = 500` | Config modifiée | |
| 2 | Forcer un scan ou créer un gros fichier | Upload à ~500 Ko/s visible dans le tooltip | |
| 3 | Remettre `upload_limit_kbps = 0` | Vitesse redevient normale | |

---

## T6. Résilience

### T6.1 Coupure réseau (mode offline)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Couper le WiFi / débrancher Ethernet | Réseau coupé | |
| 2 | Observer le systray | Icône passe à « Offline » (nuage barré) | |
| 3 | `echo "offline" > ~/SyncTest/offline_test.txt` | Fichier créé localement | |
| 4 | Attendre 10 secondes | Pas de tentative d'upload (pas d'erreur en boucle) | |
| 5 | Rebrancher le réseau | Réseau rétabli | |
| 6 | Observer le systray | Icône revient, animation, puis idle | |
| 7 | Vérifier sur Google Drive | `offline_test.txt` a été uploadé automatiquement | |
| 8 | Notification « Connexion rétablie » | Affichée | |

### T6.2 Vérification intégrité (pas de corruption)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Après un upload, vérifier les logs | `integrity: OK` ou pas de warning intégrité | |
| 2 | Le MD5 local correspond au MD5 retourné par Google | Pas de mismatch dans les logs | |

### T6.3 Corbeille

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Vérifier `config.toml` : `delete_mode = "trash"` | Config en mode corbeille | |
| 2 | Supprimer un fichier localement | Fichier mis à la corbeille Drive (pas supprimé définitivement) | |
| 3 | Vérifier sur Google Drive → Corbeille | Le fichier y est | |

---

## T7. Systray et Menu Contextuel

### T7.1 États du systray

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Au repos | Icône idle (coche verte / nuage calme) | |
| 2 | Pendant un scan | Icône scan animée (frames qui tournent) | |
| 3 | Pendant un transfert | Icône sync animée | |
| 4 | En pause (via menu) | Icône pause | |
| 5 | Après une erreur config | Icône erreur (exclamation rouge) | |
| 6 | Hors ligne | Icône offline (nuage barré) | |

### T7.2 Menu contextuel

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Clic droit sur le systray | Menu affiché avec tous les éléments | |
| 2 | Première ligne = état actuel (grisé) | Statut non cliquable | |
| 3 | « Synchroniser maintenant » | Scan forcé déclenché | |
| 4 | « ⏸ Mettre en pause » | Moteur en pause, icône change | |
| 5 | « ▶ Reprendre » | Moteur reprend | |
| 6 | « 📂 Ouvrir le dossier local » | Gestionnaire de fichiers s'ouvre | |
| 7 | « ⚙ Réglages… » | Fenêtre Settings s'ouvre, moteur en pause | |
| 8 | Fermer Settings | Moteur reprend automatiquement | |
| 9 | « 📄 Voir les logs » | Dossier de logs s'ouvre | |
| 10 | « ℹ À propos » | Fenêtre À propos s'affiche | |
| 11 | « 🚀 Lancer au démarrage » | Toggle visible (✓ si activé) | |
| 12 | `systemctl --user is-enabled syncgdrive.service` | Résultat cohérent avec le toggle | |
| 13 | « 🛑 Quitter SyncGDrive » | Application s'arrête proprement | |

### T7.3 Tooltip dynamique

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Survol systray au repos | « Surveillance active — Dossier à jour » + source → destination | |
| 2 | Survol pendant un transfert | Barre de progression + vitesse + fichier courant (📂+📄) | |
| 3 | Survol pendant un scan | Phase + compteur + élément courant (📂+📄) | |
| 4 | Survol en pause | « Moteur suspendu » | |
| 5 | Survol hors ligne | « Hors ligne — en attente du réseau » | |

---

## T8. Fenêtre Settings — Validation Live

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Ouvrir les Réglages | Champs pré-remplis avec la config actuelle | |
| 2 | Vider le chemin local | Icône disparaît, bouton Enregistrer grisé | |
| 3 | Saisir un chemin invalide (`/inexistant`) | Icône ❌, tooltip « Ce dossier n'existe pas » | |
| 4 | Saisir un chemin valide (`~/SyncTest`) | Icône ✅, tooltip « Dossier valide » | |
| 5 | Modifier les exclusions (ajouter `*.log`) | Pattern ajouté dans la liste | |
| 6 | Supprimer une exclusion | Pattern retiré | |
| 7 | Modifier le nombre de workers (SpinRow) | Valeur change entre 1 et 16 | |
| 8 | Toggle notifications | Switch fonctionne | |
| 9 | Enregistrer | Toast de confirmation, config sauvegardée | |
| 10 | Vérifier `config.toml` | Modifications bien écrites | |

### T8.1 Section Avancé

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Ouvrir la section Avancé (groupe repliable) | Les champs `[advanced]` sont visibles | |
| 2 | Modifier `debounce_ms` → 1000 | Champ accepte la valeur | |
| 3 | Enregistrer | Valeur écrite dans `config.toml` sous `[advanced]` | |
| 4 | Remettre les défauts | Tout revient aux valeurs d'origine | |

---

## T9. Exclusions (Ignore Patterns)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Ajouter `*.tmp` dans les exclusions | Pattern ajouté | |
| 2 | `echo "temp" > ~/SyncTest/test.tmp` | Fichier créé localement | |
| 3 | Attendre 5 secondes | **Aucun upload** — fichier ignoré | |
| 4 | Vérifier sur Google Drive | `test.tmp` absent | |
| 5 | `echo "ok" > ~/SyncTest/test.txt` | Fichier non exclu | |
| 6 | Attendre 3 secondes | Upload déclenché | |
| 7 | Vérifier sur Google Drive | `test.txt` présent | |

---

## T10. Mode Dry-Run

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `SYNCGDRIVE_DRY_RUN=1 syncgdrive` | Application démarre en mode dry-run | |
| 2 | Observer les logs | `[DRY-RUN] upload: …`, `[DRY-RUN] mkdir: …` | |
| 3 | Vérifier sur Google Drive | **Aucun fichier créé/modifié** | |
| 4 | Résumé affiché en fin de scan | Nombre de fichiers/dossiers/taille listés | |
| 5 | Arrêter l'application | Arrêt propre | |

---

## T11. Migration V1 → V2

> Ce test est uniquement pertinent si une installation V1 existait.

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Avoir un `config.toml` V1 (avec `local_root`/`remote_root`) | Fichier V1 en place | |
| 2 | Lancer SyncGDrive V2 | Pas de crash, migration silencieuse | |
| 3 | Vérifier `config.toml` | Converti en V2 (avec `[[sync_pairs]]`) | |
| 4 | Vérifier `config.toml.v1.bak` | Backup de l'ancienne config | |
| 5 | Vérifier la DB | Tables V2 créées (`path_cache`, `offline_queue`, `schema_version`) | |
| 6 | Les fichiers déjà synchronisés en V1 ne sont **pas** re-uploadés | DB `file_index` conservée | |

---

## T12. Multi-Sync (Plusieurs Paires)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Ajouter une 2ème paire dans Settings | Liste affiche 2 paires | |
| 2 | Chaque paire a son propre dossier local et remote | Config correcte | |
| 3 | Modifier un fichier dans la paire 1 | Seule la paire 1 upload | |
| 4 | Modifier un fichier dans la paire 2 | Seule la paire 2 upload | |
| 5 | Désactiver la paire 2 | Plus d'upload pour cette paire | |
| 6 | Le systray affiche le statut agrégé | Statut global correct | |

---

## T13. Arrêt et Redémarrage

### T13.1 Arrêt gracieux

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Pendant un transfert, cliquer « Quitter SyncGDrive » | L'application s'arrête proprement en ≤ `shutdown_timeout_secs` | |
| 2 | Vérifier les logs | « shutdown graceful » ou similaire, pas de panic | |
| 3 | Vérifier le lock file | `$XDG_RUNTIME_DIR/syncgdrive.lock` libéré | |

### T13.2 Kill -9 et redémarrage

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `kill -9 $(pgrep syncgdrive)` | Processus tué | |
| 2 | Relancer `syncgdrive` | Démarre correctement (le lock flock est libéré par le kernel) | |
| 3 | Pas de PID stale | L'instance unique fonctionne | |

### T13.3 Double instance

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Lancer `syncgdrive` (déjà en cours) | Message « Une instance est déjà en cours » + notification | |
| 2 | L'application se ferme immédiatement | Pas de deuxième instance | |

---

## T14. Logs

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | Vérifier `~/.local/state/syncgdrive/logs/` | Fichiers de log présents | |
| 2 | Les logs sont datés (rotation quotidienne) | `syncgdrive.log.2026-03-14` | |
| 3 | Les anciens logs (> `log_retention_days`) sont supprimés | Pas de fichiers > 7 jours | |
| 4 | `RUST_LOG=debug syncgdrive` | Logs verbeux affichés | |

---

## T15. Packaging Final

### T15.1 Arch Linux (PKGBUILD)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `makepkg -si` dans le dossier `dist/` | Build + installation sans erreur | |
| 2 | `namcap syncgdrive-*.pkg.tar.zst` | Pas d'erreur critique | |
| 3 | `pacman -Ql syncgdrive` | Tous les fichiers listés dans §4.1 | |
| 4 | `syncgdrive` depuis le terminal | L'application se lance | |
| 5 | Icône visible dans le lanceur d'applications | Oui | |
| 6 | `pacman -R syncgdrive` | Désinstallation propre | |
| 7 | Vérifier que `~/.config/syncgdrive/` est **préservée** | Config non supprimée | |

### T15.2 Debian/Ubuntu (.deb)

| # | Action | Résultat attendu | ✅/❌ |
|---|--------|-------------------|-------|
| 1 | `dpkg -i syncgdrive_2.0.0_amd64.deb` | Installation sans erreur | |
| 2 | `lintian syncgdrive_2.0.0_amd64.deb` | Pas d'erreur critique | |
| 3 | `which syncgdrive` | `/usr/bin/syncgdrive` | |
| 4 | `dpkg -r syncgdrive` | Désinstallation propre | |

---

## Résumé Final

| Section | Tests | Passés | Échecs | Notes |
|---------|-------|--------|--------|-------|
| T1. Installation | 13 | | | |
| T2. Config + OAuth2 | 12 | | | |
| T3. Scan initial | 12 | | | |
| T4. Watcher temps réel | 16 | | | |
| T5. Progression/Bandwidth | 8 | | | |
| T6. Résilience | 10 | | | |
| T7. Systray + Menu | 18 | | | |
| T8. Settings | 14 | | | |
| T9. Exclusions | 7 | | | |
| T10. Dry-Run | 5 | | | |
| T11. Migration V1→V2 | 6 | | | |
| T12. Multi-Sync | 6 | | | |
| T13. Arrêt/Redémarrage | 7 | | | |
| T14. Logs | 4 | | | |
| T15. Packaging | 11 | | | |
| **TOTAL** | **149** | | | |

---

**Date du test** : ____________________

**Testeur** : ____________________

**Version** : ____________________

**Verdict global** : ✅ / ❌

**Remarques** :

