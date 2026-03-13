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

**État actuel** : la validation se fait uniquement au clic sur **Enregistrer** (`btn_save.connect_clicked`). Si le chemin est invalide, un toast apparaît.

**Verdict** : 🔧 **À implémenter.**

**Plan d'implémentation** (`src/ui/settings.rs`) :

1. Ajouter une icône suffix ✅/❌ sur `local_row` (à droite, avant le bouton Parcourir)
2. Connecter `local_row.connect_changed(…)` :
   - Expand tilde (`~/` → `/home/user/…`)
   - Tester `Path::is_dir()`
   - Mettre à jour l'icône suffix : `emblem-ok-symbolic` ou `dialog-error-symbolic`
3. Ajouter une icône suffix ✅/❌ sur `remote_row`
4. Connecter `remote_row.connect_changed(…)` :
   - Tester que le texte commence par un protocole reconnu (`gdrive:/`, `smb://`, etc.)
   - Mettre à jour l'icône suffix
5. Griser le bouton **Enregistrer** (`btn_save.set_sensitive(false)`) tant que les deux champs ne sont pas valides

**Priorité** : 🟡 Moyenne — améliore l'UX mais le toast au save fonctionne déjà.

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

**Verdict** : ⚠️ **Amélioration possible.**

Les icônes actuelles fonctionnent mais ne sont pas des variantes `-symbolic`. Sur KDE Plasma, les icônes symboliques s'adaptent mieux au mode clair/sombre et sont plus cohérentes visuellement dans le systray.

**Plan d'implémentation** (`src/ui/mod.rs`, `fn icon_name`) :

| Actuel | Symbolique |
|---|---|
| `emblem-default` | `emblem-ok-symbolic` |
| `emblem-synchronizing` | `emblem-synchronizing-symbolic` |
| `dialog-warning` | `dialog-warning-symbolic` |
| `dialog-error` | `dialog-error-symbolic` |
| `media-playback-pause` | `media-playback-pause-symbolic` |

> **Attention** : ksni utilise le protocole StatusNotifierItem D-Bus qui transmet un `icon_name` au host Plasma. Les icônes `-symbolic` ne sont pas toujours disponibles dans ce contexte — tester sur le système cible avant de changer. Les noms actuels sont plus universels.

**Priorité** : 🟢 Basse — cosmétique, à tester sur le Plasma du labo avant d'appliquer.

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

**Verdict** : ✅ **Déjà implémenté**, et plus riche que ce que la revue demandait.

Le seul ajout possible serait de mémoriser le **dernier fichier synchronisé** pour l'afficher en état `Idle`. C'est un plus cosmétique :

**Plan optionnel** :
1. Ajouter un champ `last_synced: Option<String>` dans `EngineStatus::Idle` (ou un champ séparé dans `SyncTray`)
2. Le worker pose le nom du fichier après un upload réussi
3. Le tooltip Idle affiche « Surveillance active — dernier : rapport.pdf »

**Priorité** : 🟢 Basse — le tooltip fonctionne déjà très bien.

---

## 4. Robustesse du Shutdown

### 4a. Séquence de fermeture GTK + ksni

> La revue demande : quand GTK reçoit Quit, libérer le thread ksni.

**État actuel** :
- Le thread **ksni** (`ksni-tray`) tourne dans une boucle `sleep(3600)` — il ne surveille pas le shutdown token
- Le thread **GTK Settings** (`gtk-settings`) est fire-and-forget : `app.run_with_args()` bloque puis `Resume` est envoyé
- `main.rs` attend le moteur avec un **timeout 3s**, puis sort — les threads OS sont tués par `exit()`

**Verdict** : ⚠️ **Fonctionnel mais pas chirurgical.**

Le thread ksni ne s'arrête jamais proprement — il est tué par la fin du processus. C'est acceptable car :
- ksni n'a pas de ressources persistantes à libérer
- Le host Plasma détecte la disparition du PID et retire l'icône

**Plan d'amélioration** (`src/ui/mod.rs`) :

1. Remplacer la boucle `sleep(3600)` du thread ksni par une attente sur le shutdown token :
   ```
   // Au lieu de : loop { sleep(3600) }
   // Faire : while !shutdown.is_cancelled() { sleep(1) }
   ```
   Le handle ksni sera droppé proprement → D-Bus notifié → icône retirée du systray **avant** la mort du processus.

2. Le thread GTK Settings est déjà correct : il bloque sur `app.run_with_args()`, puis envoie `Resume` et retourne. Pas de changement nécessaire.

**Priorité** : 🟡 Moyenne — améliore la propreté de sortie sur KDE Plasma.

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
| 2a | Validation live des champs Settings | `ui/settings.rs` | 🟡 Moyenne | ~60 lignes |
| 2b | ~~FileChooserNative~~ | — | ✅ Déjà optimal (FileDialog GTK4) | — |
| 3a | Icônes `-symbolic` (tester sur Plasma) | `ui/mod.rs` | 🟢 Basse | ~10 lignes |
| 3b | ~~Tooltip dynamique~~ | — | ✅ Déjà implémenté | — |
| 3b+ | Dernier fichier synchronisé dans tooltip Idle | `ui/mod.rs` + `engine/mod.rs` | 🟢 Basse | ~20 lignes |
| 4a | Arrêt propre du thread ksni | `ui/mod.rs` | 🟡 Moyenne | ~10 lignes |
| 4b | ~~Timeout join UI~~ | — | ✅ Non nécessaire (threads détachés) | — |

### Ordre d'implémentation recommandé

1. **4a** — Arrêt propre ksni (rapide, améliore la propreté)
2. **2a** — Validation live Settings (meilleure UX pour le premier lancement)
3. **3a** — Icônes symboliques (cosmétique, à tester d'abord)
4. **3b+** — Dernier fichier dans tooltip Idle (cosmétique)

