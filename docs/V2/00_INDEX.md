# 📋 SyncGDrive V2 — Index des Documents de Conception

> Chaque phase a son propre document de spécification détaillée.
> **Aucun code ne sera écrit avant que tous les documents soient validés.**

---

## Document Maître

| Document | Description |
|----------|-------------|
| [`pre_developpement_V2.md`](pre_developpement_V2.md) | Vision globale, objectifs, inventaire V1, résumé de toutes les améliorations |

---

## Documents par Phase

| Phase | Document | Titre | Dépend de | Statut |
|-------|----------|-------|-----------|--------|
| 1 | [`01_CONFIG_V2.md`](01_CONFIG_V2.md) | Migration Config TOML V2 + Schéma DB | — | 📝 Rédigé |
| 2 | [`02_AUTH_OAUTH2.md`](02_AUTH_OAUTH2.md) | Module OAuth2 Google Drive | Phase 1 | 📝 Rédigé |
| 3 | [`03_REMOTE_PROVIDER.md`](03_REMOTE_PROVIDER.md) | Trait RemoteProvider + Backend Google Drive | Phases 1, 2 | 📝 Rédigé |
| 4 | [`04_HARDCODE_CLEANUP.md`](04_HARDCODE_CLEANUP.md) | Élimination des Constantes Hardcodées | Phase 1 | 📝 Rédigé |
| 5 | [`05_PROGRESS_BANDWIDTH.md`](05_PROGRESS_BANDWIDTH.md) | Progression Octets/Vitesse + Limite Bande Passante | Phase 3 | 📝 Rédigé |
| 6 | [`06_RESILIENCE.md`](06_RESILIENCE.md) | Offline, Intégrité, Corbeille, Rate Limiter | Phases 3, 4 | 📝 Rédigé |
| 7 | [`07_UX_PREMIUM.md`](07_UX_PREMIUM.md) | Icônes SVG, Animation Systray, Fenêtre Scan, Chemins Lisibles | Phases 4, 5 | 📝 Rédigé |
| 8 | [`08_DRY_RUN_TESTS.md`](08_DRY_RUN_TESTS.md) | Mode Dry-Run + Tests d'Intégration | Phases 1–7 | 📝 Rédigé |
| 9 | [`09_PACKAGING.md`](09_PACKAGING.md) | Packaging PKGBUILD, .deb, PPA, .desktop | Phases 1–8 | 📝 Rédigé |
| 10 | [`10_QA_MANUAL_TESTS.md`](10_QA_MANUAL_TESTS.md) | Procédure de Test Humain (149 tests QA) | Phases 1–9 | 📝 Rédigé |

---

## Graphe de Dépendances

```
Phase 1 (Config V2)
├── Phase 2 (OAuth2)
│   └── Phase 3 (RemoteProvider)
│       ├── Phase 5 (Progression/Bandwidth)
│       │   └── Phase 7 (UX Premium)
│       └── Phase 6 (Résilience)
├── Phase 4 (Hardcode Cleanup)
│   ├── Phase 6 (Résilience)
│   └── Phase 7 (UX Premium)
└────────────────────────────────┐
                                 ▼
                          Phase 8 (Dry-Run + Tests)
                                 │
                                 ▼
                          Phase 9 (Packaging)
                                 │
                                 ▼
                          Phase 10 (QA Manuelle — 149 tests)
```

---

## Convention des Documents

Chaque document de phase contient :

1. **Objectif** — Ce que cette phase accomplit
2. **Pré-requis** — Phases dont elle dépend
3. **Fichiers impactés** — Créations et modifications
4. **Structures de données** — Structs, enums, tables SQL
5. **Spécification détaillée** — Comportement attendu, algorithmes
6. **Cas limites** — Edge cases à gérer
7. **Tests à écrire** — Unitaires et d'intégration
8. **Critères d'acceptation** — Checklist pour valider la phase

