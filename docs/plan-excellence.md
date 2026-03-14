# SyncGDrive — Plan d'Excellence 10/10

> Basé sur la revue UI & Intégration.
> Chaque point est confronté au code actuel : déjà fait (✅), à faire (🔧), ou partiellement couvert (⚠️).

---

## 1. Triangle de Fer de la Communication

> Verdict de la revue : **Validé**.

### État actuel

| Canal | Implémentation | Statut |
|---|---|---|
| **Moteur → UI** | `mpsc::unbounded_channel<EngineStatus>` → task Tokio qui écrit dans `Arc<Mutex<EngineStatus>>` → ksni poll en thread D-Bus | ✅ |
| **Tray → Moteur** | `cmd_tx.try_send(EngineCommand)` depuis les callbacks ksni | ✅ |
| **UI Settings → Moteur** | `cmd_tx.try_send(ApplyConfig / Pause / Resume)` depuis thread GTK | ✅ |

### Nuance vs la revue

La revue mentionne `glib::MainContext::channel` pour Moteur → UI. En réalité, l'architecture actuelle n'utilise **pas** glib pour le transport : c'est un `Arc<Mutex<EngineStatus>>` partagé entre le thread Tokio et ksni. C'est **plus simple et tout aussi correct** car ksni n'est pas GTK — c'est du D-Bus pur. Le thread GTK (Settings) n'a pas besoin de recevoir le statut en temps réel, il ne fait que de la config.

### Action

✅ **Rien à changer.** L'architecture est solide et chaque thread communique de façon thread-safe sans dépendances croisées.

---

## 2. Excellence de la Fenêtre Settings

### 2a. Validation « Live » des champs

> La revue suggère : signaux `changed` + icônes ✅/❌ en temps réel.

**État actuel** : ✅ **Implémenté dans `ui/settings.rs`.**

Chaque champ affiche une icône suffix dynamique :
- **Dossier local** : `emblem-ok-symbolic` si le chemin est un dossier existant, `dialog-error-symbolic` sinon (expand tilde supporté)
- **URL distante** : `emblem-ok-symbolic` si le protocole est reconnu (`gdrive:/`, `smb://`, `sftp://`, `webdav://`, `ftp://`), `dialog-error-symbolic` sinon
- **Bouton Enregistrer** grisé tant que les deux champs ne sont pas valides
- Validation déclenchée à chaque frappe (`connect_changed`) + au chargement initial

Fonctions helper : `is_local_valid()`, `is_remote_valid()`, `update_local_status()`, `update_remote_status()`, `update_save_sensitivity()`.

**Priorité** : ✅ Fait.

---

### 2b. Sélecteur de fichiers

> La revue suggère : `gtk::FileChooserNative`.

**État actuel** : le code utilise `gtk4::FileDialog` (API moderne GTK 4.10+).

**Verdict** : ✅ **Déjà optimal.**

`FileDialog` est le **remplacement officiel** de `FileChooserNative` en GTK4. Il :
- Passe par le portail XDG Desktop (compatible Flatpak, Snap)
- Respecte les favoris et le thème Dolphin/KDE
- Est l'API recommandée pour GTK ≥ 4.10

La suggestion `FileChooserNative` de la revue s'appliquait à **GTK3**, pas à notre stack GTK4/libadwaita. Aucun changement nécessaire.

---

## 3. Gestion du Statut Systray

### 3a. Icônes symboliques

> La revue suggère : icônes `-symbolic` pour adaptation clair/sombre.

**État actuel** : noms d'icônes FreeDesktop standard (`emblem-default`, `emblem-synchronizing`, `dialog-warning`, `dialog-error`, `media-playback-pause`).

**Verdict** : ✅ **Implémenté dans `ui/tray.rs`.**

Les icônes symboliques sont maintenant utilisées dans `icon_name()` :

| État | Icône symbolique |
|---|---|
| Starting | `system-run-symbolic` |
| Idle | `emblem-ok-symbolic` |
| ScanProgress (Remote) | `network-server-symbolic` |
| ScanProgress (Local) | `folder-saved-search-symbolic` |
| ScanProgress (Dirs) | `folder-new-symbolic` |
| ScanProgress (Compare) | `edit-find-replace-symbolic` |
| SyncProgress/Syncing | `emblem-synchronizing-symbolic` |
| Paused | `preferences-system-symbolic` |
| Error | `dialog-error` |
| Stopped | `system-shutdown-symbolic` |

**Priorité** : ✅ Fait.

---

### 3b. Tooltip dynamique

> La revue suggère : afficher le dernier fichier synchronisé au survol.

**État actuel** : le tooltip est **déjà dynamique** et très détaillé :

| État | Tooltip affiché |
|---|---|
| `Unconfigured` | « Ouvrez les Réglages pour configurer. *raison* » |
| `Idle` | « Surveillance active. /home/… → gdrive:/… » |
| `ScanProgress` | « Scan (Inventaire/Dossiers/Comparaison) — 42/256 — main.rs » |
| `SyncProgress` | « Transfert 12/156 — rapport.pdf (4 Ko) » |
| `Syncing` | « *N* transfert(s) en cours » |
| `Paused` | « Réglages ouverts. Reprendra à la fermeture. » |
| `Error` | « *message* — Vérifiez les logs ou les tokens KIO. » |

**Verdict** : ✅ **Implémenté dans `ui/tray.rs`.**

Le champ `last_synced: String` est stocké dans `SyncTray`. Il est mis à jour à chaque `SyncProgress` via `handle.update()`. Le tooltip Idle affiche :
```
Surveillance active — Dossier à jour.
/home/user/Projets → gdrive:/MonDrive/Backup
✅ Dernier transfert : rapport.pdf
```

**Priorité** : ✅ Fait.

---

## 4. Robustesse du Shutdown

### 4a. Séquence de fermeture GTK + ksni

> La revue demande : quand GTK reçoit Quit, libérer le thread ksni.

**État actuel** :
- Le thread **ksni** (`ksni-tray`) tourne dans une boucle `sleep(3600)` — il ne surveille pas le shutdown token
- Le thread **GTK Settings** (`gtk-settings`) est fire-and-forget : `app.run_with_args()` bloque puis `Resume` est envoyé
- `main.rs` attend le moteur avec un **timeout 3s**, puis sort — les threads OS sont tués par `exit()`

**Verdict** : ✅ **Implémenté dans `ui/tray.rs`.**

Le systray est maintenant une tâche Tokio propre qui écoute le `CancellationToken` dans un `tokio::select!` avec `biased;`. Quand le shutdown est déclenché, la boucle sort et appelle `handle.shutdown().await` qui :
1. Drop le handle ksni proprement
2. Notifie le host D-Bus → icône retirée du systray avant la mort du processus

**Priorité** : ✅ Fait.

---

### 4b. Timeout sur le join UI

> La revue suggère : timeout 2s sur le join du thread UI.

**État actuel** : `main.rs` a un timeout de **3 secondes** sur le moteur mais pas sur les threads UI. Cependant, les threads UI ne sont **pas joinés** — ils sont détachés (`spawn` sans `join`). Quand le processus sort, ils sont tués automatiquement.

**Verdict** : ✅ **Le problème ne se pose pas** dans l'architecture actuelle.

Les threads `ksni-tray` et `gtk-settings` sont fire-and-forget. Aucun `join()` n'est appelé, donc aucun risque de blocage. La séquence de shutdown est :

```
signal → shutdown.cancel()
       → cmd_tx.send(Shutdown)
       → engine finit (max 3s)
       → main() retourne → process exit
       → tous les threads OS tués
```

Le timeout de 3s dans `main.rs` (lignes 103-108) couvre déjà le cas « le moteur ne finit pas ». Les threads UI meurent avec le processus.

---

## Résumé des actions

| # | Action | Fichier | Priorité | Effort |
|---|---|---|---|---|
| 1 | ~~Triangle de Fer~~ | — | ✅ Validé | — |
| 2a | ~~Validation live Settings~~ | `ui/settings.rs` | ✅ Implémenté | — |
| 2b | ~~FileChooserNative~~ | — | ✅ Déjà optimal (FileDialog GTK4) | — |
| 3a | ~~Icônes `-symbolic`~~ | `ui/tray.rs` | ✅ Implémenté | — |
| 3b | ~~Tooltip dynamique~~ | — | ✅ Implémenté | — |
| 3b+ | ~~Dernier fichier dans tooltip Idle~~ | `ui/tray.rs` | ✅ Implémenté | — |
| 4a | ~~Arrêt propre du thread ksni~~ | `ui/tray.rs` | ✅ Implémenté | — |
| 4b | ~~Timeout join UI~~ | — | ✅ Non nécessaire (threads détachés) | — |

### Reste à faire

✅ **Tous les points du plan d'excellence sont implémentés.**

