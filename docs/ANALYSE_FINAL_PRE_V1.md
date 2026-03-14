# Audit Final — tray.rs (2026-03-14)

> Audit externe en 3 itérations. Conclusions finales après rectifications.

## Verdict : ✅ Prêt pour la production

Le code de `tray.rs` est validé sans modification nécessaire.

---

## Points audités

### 1. Concurrence et Verrous — ✅ Aucun deadlock

**Lignes 66-70** : `tray.config.lock()` est acquis et **droppé** à la fin du bloc `if`
(ligne 69) **avant** l'acquisition de `tray.status.lock()` (ligne 70).
Les locks sont séquentiels, jamais imbriqués. Aucun chemin de code ne verrouille
`status` puis `config` dans l'ordre inverse.

**Verdict** : Faux positif de l'audit initial.

### 2. Ordre d'Exécution GTK — ✅ Aucune race condition

**Lignes 87 + 501-507** : Si `open_settings` est `true`, le message `OpenSettings`
est envoyé dans le canal `mpsc` (ligne 87). Le thread `gtk-ui` appelle
`libadwaita::init()` (ligne 501) **puis** entre dans `while let Ok(action) = rx.recv()`
(ligne 507). Le message attend dans le buffer du canal jusqu'à ce que GTK soit prêt.

**Verdict** : Faux positif — la synchronisation est garantie par le canal `mpsc`.

### 3. Architecture Single Thread GTK — ✅ Pattern optimal

**Lignes 484-534** : `OnceLock<Sender<GtkAction>>` + thread `gtk-ui` permanent.
`libadwaita::init()` appelé **une seule fois** (ligne 501). Communication par
`enum GtkAction { OpenSettings, ShowAbout }`. Settings et À propos s'exécutent
séquentiellement sur le même thread OS.

**Verdict** : Implémentation de référence pour intégrer GTK4 dans un runtime Tokio.

### 4. Notification de Sync Initiale — ✅ Choix d'UX valide

**Lignes 63-68** : La notification n'est envoyée que si `last_synced` est non vide
(au moins un fichier transféré). Si le dossier est déjà à jour → pas de notification
inutile. Le tooltip dynamique (lignes 176-189) informe l'utilisateur via
« Surveillance active — Dossier à jour ».

**Verdict** : Design intentionnel conforme à la politique de silence (UX_SYSTRAY.md §3-§4).

### 5. Points confirmés comme excellents

| Aspect | Lignes | Évaluation |
|---|---|---|
| Arrêt gracieux (`select! biased` + `CancellationToken`) | 50-53 | ✅ Exemplaire |
| Machine d'état complète (10 états, icônes symboliques) | 115-131 | ✅ Robuste |
| Tooltip dynamique avec barres Unicode | 166-269 | ✅ UX riche |
| Menu contextuel dynamique (11 entrées) | 273-421 | ✅ Complet |
| Isolation D-Bus (ksni) / GTK (thread unique) | 484-534 | ✅ Architecture propre |

---

## Note sur le processus d'audit

L'audit externe (LLM) a nécessité **3 itérations** pour arriver à un diagnostic correct :

1. **Itération 1** : Auditeur a halluciné du code inexistant (threads ponctuels
   `gtk-about` / `gtk-settings` avec `libadwaita::init()` répété).
2. **Itération 2** : Auditeur s'est appuyé sur une doc obsolète dans `ui/mod.rs`
   (corrigée depuis) pour diagnostiquer le même faux bug.
3. **Itération 3** : Auditeur a identifié les bons numéros de ligne mais émis
   3 faux positifs (deadlock, race condition, architecture manquante).
4. **Rectification finale** : Tous les faux positifs reconnus et corrigés.

**Leçon** : Un audit LLM doit être confronté au code réel ligne par ligne.
Les diagnostics plausibles ne sont pas des diagnostics corrects.
