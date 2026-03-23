# 🚀 Plan d'Excellence — SyncGDrive V2

**Objectif :** Valider l'intégration système, la résilience du moteur asynchrone et l'ergonomie de l'interface avant le déploiement en production sur Arch Linux. Ce document sert de "Pre-flight Check" pour la Phase 8 (Dry-Run).

---

## 🛡️ Phase A : Sécurité et Intégration Système (Fondations)
*Objectif : Garantir que le daemon s'intègre nativement à Arch Linux / KDE Plasma sans compromettre la sécurité.*

- [x] **Service Systemd Utilisateur :** Fichier `syncgdrive.service` configuré avec `Restart=on-failure`, timeout de 5s pour arrêt gracieux (SIGTERM).
- [x] **Héritage Graphique :** Suppression de `Environment=DISPLAY=:0` pour laisser KDE injecter dynamiquement `WAYLAND_DISPLAY` au démarrage.
- [x] **Structure de Déploiement :** Création du dossier `dist/` contenant le modèle `.env.example`.
- [x] **Sécurisation des Secrets :** Génération automatique du fichier de configuration s'il est manquant, avec consigne stricte de verrouillage (`chmod 600 .env`) pour protéger le `CLIENT_ID` et le `CLIENT_SECRET`.
- [x] **Protection du Token :** Le fichier `token.enc` est chiffré au repos.

---

## ⚙️ Phase B : Résilience et Observabilité (Le Moteur)
*Objectif : S'assurer que le moteur Tokio est indestructible et ne génère du bruit que lorsque c'est strictement nécessaire.*

- [ ] **Étanchéité du Dry-Run :** Vérification absolue que le flag `--dry-run` bloque **toute** requête mutative (`POST`, `PUT`, `DELETE`) vers l'API Google Drive et toute modification locale, tout en simulant l'algorithme de résolution.
- [ ] **Bouclier Anti-429 (Google Drive) :** Validation du backoff exponentiel lors de requêtes massives.
- [ ] **ERRor Radar :** La règle métier est stricte. Une ligne de log structurée (pour l'ERRor Radar) ne doit être émise qu'après **3 tentatives infructueuses** consécutives sur un même fichier.
- [ ] **Audit des Deadlocks :** Vérification que la file d'attente hors-ligne (Transaction Queue) se remplit et se vide correctement lors des transitions de statut du réseau, sans bloquer le thread principal.

---

## 🎨 Phase C : Ergonomie et Validation UX (L'Interface)
*Objectif : Une expérience utilisateur fluide, claire et sans blocage (freeze).*

- [x] **Serveur UI Persistant :** Le thread GTK4 maintient l'application en vie en arrière-plan (via `ApplicationHoldGuard`) et communique de manière asynchrone avec le moteur.
- [x] **Animations Systray :** Les 28 frames SVG s'animent de manière fluide et s'arrêtent instantanément lors des changements d'état (Idle, Pause, Help).
- [x] **Clarté de l'Aide :** La fenêtre Libadwaita affiche des instructions typographiées claires pour la configuration initiale, assurant une lecture facile.
- [x] **Stress Test UI :** Les interactions frénétiques avec le menu mettent le moteur en pause de manière sécurisée sans le faire crasher.

---

## 🧪 Phase D : Protocole d'Exécution (Phase 8 - Dry-Run)
*Objectif : Exécution du test grandeur nature en toute sécurité.*

1. **Lancement Monitoré :** Démarrage avec `RUST_LOG=debug` et le flag `--dry-run`.
2. **Test de Conflit :** Simulation de modifications simultanées en local et sur le Drive pour observer la logique de décision du moteur dans les logs.
3. **Test de Coupure :** Désactivation de la carte réseau locale pour vérifier le passage en mode hors-ligne et la mise en file d'attente des événements système.
4. **Test de Reprise :** Rétablissement du réseau pour valider le dépilage de la file d'attente et l'absence d'erreurs 429 massives.

---

## 🔭 Carnet de Route (Préparation V3)
*Ces points sont identifiés mais volontairement repoussés à la V3 pour privilégier la stabilité de la V2.*

- **Pont I/O Asynchrone :** Déplacer les calculs intensifs (hachage MD5, lecture de gros fichiers locaux) dans des `tokio::task::spawn_blocking` pour éviter d'asphyxier la boucle asynchrone principale.
- **Amélioration du Monitoring :** Ajustements fins des logs structurés (ERRor Radar) selon les retours d'expérience en conditions réelles.