# Spécifications UX/UI : Systray & Notifications (SyncGDrive)

Ce document définit la machine d'état visuelle de l'application SyncGDrive. L'objectif est de fournir un retour visuel clair, précis et non intrusif via la zone de notification (systray) et les notifications système.

## 1. États et Icônes de la Systray

L'icône de la systray doit refléter l'étape exacte du cycle de vie du moteur de synchronisation.

| État du Moteur | Action en cours | Icône Suggérée (Nom ou Type) | Animation |
| :--- | :--- | :--- | :--- |
| **Démarrage** | Initialisation, chargement config, test connexion | `system-run-symbolic` ou icône d'engrenage statique | Non |
| **Scan Distant** | Interrogation de l'API/KIO (Google Drive) | `network-server-symbolic` (avec badge loupe) | Non |
| **Scan Local** | Lecture de l'arborescence du dossier surveillé | `folder-saved-search-symbolic` | Non |
| **Comparaison** | Vérification avec la base SQLite (Index/Hashes) | `edit-find-replace-symbolic` ou balance | Non |
| **Création Dossiers** | Phase 1 : Reproduction de l'arborescence (mkdir) | `folder-new-symbolic` (Personnalisé) | **Oui** (SVG animé) |
| **Transfert** | Phase 2 : Upload/Download des fichiers | `emblem-synchronizing` ou flèches circulaires | **Oui** (SVG animé) |
| **Attente (Idle)** | Scan initial terminé, surveillance inotify active | `emblem-ok-symbolic` (Bouclier ou Check vert) | Non |
| **Settings** | Fenêtre de configuration ouverte, moteur en pause | `preferences-system-symbolic` | Non |
| **Arrêt** | Séquence de fermeture (Shutdown token actif) | `system-shutdown-symbolic` | Non |

## 2. Note Technique : Animations SVG via KSNI (D-Bus)

*Spécification d'implémentation pour le thread UI :*
Le protocole StatusNotifierItem (SNI) géré par `ksni` et KDE Plasma ne supporte pas toujours la lecture native et fluide d'un fichier `.svg` contenant des balises `<animate>`.
Pour garantir l'animation lors du **Transfert** et de la **Création de dossiers**, le backend `ksni` devra :
1. Soit utiliser les indicateurs de progression natifs de Plasma si l'API le permet.
2. Soit (méthode recommandée) faire cycler l'interface (Timer asynchrone) entre plusieurs icônes statiques (ex: `sync-frame1.svg`, `sync-frame2.svg`) à intervalles réguliers (ex: 500ms) tant que l'état est actif.

## 3. Stratégie des Notifications Système

Les notifications (`notify-send` / Libadwaita) doivent être utilisées avec parcimonie pour ne pas saturer l'utilisateur.

**À notifier (Alertes Actives) :**
* **Erreur Fatale :** "Jeton Google expiré. Veuillez vous reconnecter." (Clic = Ouvre Settings/Comptes).
* **Erreur de Chemin :** "Dossier local introuvable."
* **Conflit :** "Fichier modifié des deux côtés. Copie de sauvegarde créée."

**À NE PAS notifier (Événements Silencieux) :**
* Démarrage normal de l'application.
* Fin du scan initial.
* Fichier individuel synchronisé avec succès (utiliser le Tooltip de la systray pour afficher le dernier fichier traité).

## 4. Règles des Notifications Système (Desktop Pop-ups)

Pour éviter la "fatigue des notifications", le système de pop-ups (via `notify-rust` ou Libadwaita) est réservé aux événements majeurs et aux erreurs nécessitant une intervention humaine. Les changements d'état courants (scan, transfert, création de dossiers) sont affichés **uniquement** via le texte d'information de l'icône (Tooltip).

### A. Notifications d'Information (Succès)
* **Fin de Synchronisation Initiale**
    * *Titre :* SyncGDrive — Synchronisation terminée
    * *Message :* "Le dossier est à jour. Surveillance active, vous pouvez travailler en toute sécurité."
    * *Comportement :* S'efface automatiquement après quelques secondes.

### B. Notifications d'Action (Erreurs)
* **Jeton Expire / Auth KIO**
    * *Titre :* SyncGDrive — Action requise
    * *Message :* "Accès Google Drive expiré. Veuillez vous reconnecter dans les paramètres système KDE."
    * *Comportement :* Reste à l'écran jusqu'à ce que l'utilisateur la ferme (Sticky).
* **Dossier Local Perdu**
    * *Titre :* SyncGDrive — Dossier introuvable
    * *Message :* "Le dossier surveillé a été renommé ou supprimé. Moteur en pause."
    * *Comportement :* Sticky. Un clic dessus ouvre la fenêtre des Settings.
* **Quota Dépassé**
    * *Titre :* SyncGDrive — Espace insuffisant
    * *Message :* "Quota Google Drive ou disque local plein. Transferts suspendus."

### C. (Optionnel V2) Notifications de Conflit
* **Conflit de Fichier**
    * *Titre :* SyncGDrive — Conflit détecté
    * *Message :* "Le fichier 'document.txt' a été modifié des deux côtés. Une copie de sauvegarde a été créée."

## 5. Menu Contextuel de la Systray (ksni)

Le menu contextuel (clic droit sur l'icône) est **dynamique**. Son contenu change en fonction de l'état actuel du `SyncEngine`.

### A. Structure du Menu

* **[État Actuel]** *(Texte grisé, non cliquable. Ex: "Surveillance active" ou "Transfert en cours...")*
* `---` *(Séparateur)*
* **[Action Dynamique de Synchronisation]**
    * *Si état = Idle (Attente) :* **"Synchroniser maintenant"** (Force un scan complet).
    * *Si état = ScanInitial / Transfert :* **"Mettre en pause"** (Stoppe les workers sans tuer l'app).
    * *Si état = Paused :* **"Reprendre la synchronisation"**.
* `---` *(Séparateur)*
* **📂 Ouvrir le dossier local** *(Raccourci ouvre Dolphin sur le dossier surveillé)*
* **☁️ Ouvrir Google Drive** *(Raccourci ouvre Dolphin sur `gdrive:/...` ou le navigateur)*
* `---` *(Séparateur)*
* **🚀 Lancer au démarrage** *(Case à cocher / Toggle. Crée ou supprime le fichier `.desktop` dans autostart)*
* **⚙️ Réglages** *(Ouvre la fenêtre Libadwaita)*
* **📄 Voir les logs** *(Ouvre le fichier de log dans l'éditeur par défaut, ex: Kate)*
* **ℹ️ À propos** *(Ouvre une modale Libadwaita avec la version et les crédits)*
* `---` *(Séparateur)*
* **🛑 Quitter SyncGDrive** *(Déclenche le CancellationToken, ferme proprement)*

## 6. Spécifications Techniques : Système & Résilience (Architecture Linux)

Pour garantir la robustesse du démon sur Arch Linux, l'application s'appuie sur les standards XDG et l'écosystème systemd.

* **Point d'Entrée Manuel (.desktop) :**
  À l'installation ou au premier lancement, l'application génère un fichier `~/.local/share/applications/syncgdrive.desktop`. Cela permet à l'utilisateur de lancer l'interface manuellement depuis le menu des applications (KDE Kickoff, Rofi, etc.) avec son icône dédiée.

* **Lancement au Démarrage (Service Systemd Utilisateur) :**
  Plutôt que d'utiliser un fichier `.desktop` dans le dossier classique `~/.config/autostart/`, le toggle "Lancer au démarrage" dans le menu systray interagit directement avec systemd pour une résilience maximale.
    * *Activation :* Exécute `systemctl --user enable syncgdrive.service` (Crée le lien symbolique).
    * *Désactivation :* Exécute `systemctl --user disable syncgdrive.service`.
    * *Avantages :* Permet de définir des dépendances réseau (`After=network-online.target`) et d'appliquer des politiques de redémarrage automatique (`Restart=on-failure`) en cas de crash isolé.

* **Contrôle d'Instance Unique (File Lock / PID File) :**
  Même si systemd gère le démon, pour éviter les lancements multiples (ex: clic répété dans le menu des applications), le code Rust utilise un verrou de fichier exclusif (ex: via `flock`) sur `$XDG_RUNTIME_DIR/syncgdrive.lock` (généralement situé dans `/run/user/1000/`). Si le fichier est verrouillé par un processus actif, la nouvelle instance affiche la notification "SyncGDrive est déjà en cours d'exécution" et se ferme instantanément.

* **Rotation des Logs Interne (Logrotate intégré) :**
  Plutôt que de dépendre du démon `logrotate` système (qui nécessite des droits root) ou d'être limité à `journald`, le moteur de logs de l'application utilise `tracing_appender::rolling::daily` pour gérer sa rotation de manière autonome :
    * *Règle :* Génération d'un fichier de log par jour (ex: `syncgdrive.log.2026-03-13`).
    * *Rétention :* Conservation stricte des 7 derniers jours maximum pour ne jamais saturer le disque local.
    * *Accessibilité :* Les logs restent facilement consultables en clair via l'interface UI (bouton "Voir les logs").

## 7. Comportement au Survol (Tooltip Dynamique)

Le Tooltip de la systray est la source de vérité absolue de l'application. Son contenu texte est mis à jour en temps réel par le `SyncEngine`.

Étant donné que le protocole système (SNI) n'accepte que du texte pour les tooltips, les barres de progression seront dessinées dynamiquement via des blocs de caractères Unicode (ex: `█` et `░`) pour offrir un rendu visuel natif et précis.

### A. Format du Tooltip en phase active (Création / Modification / Transfert)
Le tooltip doit toujours afficher 3 niveaux d'information pour garantir une précision totale à l'utilisateur :
1. **L'opération en cours** + La barre de progression visuelle globale.
2. **La cible précise** (nom du fichier ou du dossier en cours de traitement).
3. **Les métriques** (Poids, position dans la file d'attente).

*Exemples de rendu texte attendu au survol :*

**➔ État : Création de dossiers**
Création de l'arborescence : 45% [████░░░░░░]
Dossier : /Projets/2026/Dossier_Client
(12 sur 28 dossiers créés)

**➔ État : Transfert de fichiers (Upload/Download)**
Envoi en cours : 80% [████████░░]
Fichier : rapport_veterinaire_mars.pdf
Poids : 4.2 Mo (8 / 10 fichiers)

**➔ État : Suppression**
Nettoyage distant : 50% [█████░░░░░]
Suppression : brouillon_v1.docx
(1 sur 2 fichiers)

### B. Format du Tooltip dans les états de transition
Pour les phases qui ne peuvent pas être quantifiées par un pourcentage exact, le tooltip doit indiquer précisément l'action en cours :

* **Scan Distant :** "Analyse Google Drive en cours... (Lecture de : /Archives)"
* **Scan Local :** "Analyse du disque local... (1452 fichiers indexés)"
* **Comparaison :** "Comparaison avec la base de données... (Calcul des différences)"
* **Attente (Idle) :** "Surveillance active — Dossier à jour.\n✅ Dernier transfert : ordonnance_chien.pdf"
* **Pause :** "Moteur suspendu. (Ouvrez le menu contextuel pour reprendre)"

## 8. Annexe Technique : Générateur de Barre de Progression (Rust)

Pour respecter les contraintes du tooltip D-Bus (texte uniquement) tout en offrant un rendu visuel de haute qualité, le moteur UI utilisera la fonction utilitaire suivante.

Elle convertit un pourcentage d'avancement (0-100) en une chaîne de caractères Unicode (blocs pleins `█` et ombrés `░`).

```rust
/// Génère une barre de progression visuelle en texte pour le Tooltip KSNI.
///
/// # Arguments
/// * `percent` - Le pourcentage d'accomplissement (0 à 100).
/// * `length` - La longueur totale de la barre en nombre de caractères (ex: 10).
///
/// # Exemple
/// `generate_progress_bar(45, 10)` retournera `"[█████░░░░░]"`
pub fn generate_progress_bar(percent: f64, length: usize) -> String {
    // Sécurité : forcer la valeur entre 0.0 et 100.0
    let clamped_percent = percent.clamp(0.0, 100.0);
    
    // Calculer le nombre de blocs pleins
    let filled_chars = ((clamped_percent / 100.0) * length as f64).round() as usize;
    // Déduire les blocs vides
    let empty_chars = length.saturating_sub(filled_chars);

    // Caractères Unicode : Plein (U+2588) et Ombré léger (U+2591)
    let filled_str = "█".repeat(filled_chars);
    let empty_str = "░".repeat(empty_chars);

    format!("[{}{}]", filled_str, empty_str)
}
```
