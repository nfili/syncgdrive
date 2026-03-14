# Phase 9 — Packaging (PKGBUILD, .deb, PPA, .desktop)

---

## 1. Objectif

Produire des paquets installables pour les distributions Linux majeures, avec intégration système complète (fichier .desktop, icône, service systemd, sysctl inotify).

---

## 2. Pré-requis

- **Phases 1–8** : Application complète et testée.

---

## 3. Fichiers à Créer

| Fichier | Description |
|---------|-------------|
| `dist/PKGBUILD` | Paquet Arch Linux (AUR) |
| `dist/debian/control` | Métadonnées paquet Debian |
| `dist/debian/rules` | Script de build Debian |
| `dist/debian/changelog` | Changelog Debian |
| `dist/debian/copyright` | Licence MIT |
| `dist/debian/syncgdrive.install` | Fichiers à installer |
| `dist/syncgdrive.desktop` | Entrée XDG (application menu) |
| `dist/syncgdrive.svg` | Icône application (hicolor) |
| `dist/99-syncgdrive-inotify.conf` | sysctl inotify pour gros dépôts |
| `dist/syncgdrive.service` | Service systemd --user (déjà existant, à mettre à jour) |
| `Makefile` | Cible `install`, `uninstall`, `dist-arch`, `dist-deb` |

---

## 4. Spécification Détaillée

### 4.1 Arborescence d'Installation

```
/usr/bin/syncgdrive                              # Binaire
/usr/lib/systemd/user/syncgdrive.service         # Service systemd
/usr/share/applications/syncgdrive.desktop       # Entrée menu
/usr/share/icons/hicolor/scalable/apps/syncgdrive.svg  # Icône
/usr/share/doc/syncgdrive/README.md              # Documentation
/usr/share/licenses/syncgdrive/LICENSE           # Licence MIT
```

### 4.2 PKGBUILD (Arch Linux / AUR)

```bash
# Maintainer: clyds <clyds@users.noreply.github.com>
pkgname=syncgdrive
pkgver=2.0.0
pkgrel=1
pkgdesc="Synchronisation unidirectionnelle locale → Google Drive"
arch=('x86_64')
url="https://github.com/clyds/SyncGDrive"
license=('MIT')
depends=('gtk4' 'libadwaita' 'sqlite')
makedepends=('rust' 'cargo' 'pkg-config')
optdepends=(
    'gnome-keyring: stockage sécurisé des tokens OAuth2'
    'kwallet: stockage sécurisé des tokens OAuth2 (KDE)'
)
source=("$pkgname-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
    cd "$pkgname-$pkgver"
    cargo build --release --features ui
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 "target/release/syncgdrive" "$pkgdir/usr/bin/syncgdrive"
    install -Dm644 "dist/syncgdrive.service" "$pkgdir/usr/lib/systemd/user/syncgdrive.service"
    install -Dm644 "dist/syncgdrive.desktop" "$pkgdir/usr/share/applications/syncgdrive.desktop"
    install -Dm644 "dist/syncgdrive.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/syncgdrive.svg"
    install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
    install -Dm644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"

    # Optionnel : sysctl inotify
    install -Dm644 "dist/99-syncgdrive-inotify.conf" "$pkgdir/etc/sysctl.d/99-syncgdrive-inotify.conf"
}

post_install() {
    echo ">> Pour activer SyncGDrive au démarrage :"
    echo "   systemctl --user enable --now syncgdrive.service"
    echo ""
    echo ">> Si vous surveillez de gros dossiers, rechargez les limites inotify :"
    echo "   sudo sysctl --system"
}
```

### 4.3 Fichier .desktop

```ini
[Desktop Entry]
Type=Application
Name=SyncGDrive
GenericName=Synchronisation Google Drive
Comment=Synchronisation unidirectionnelle locale → Google Drive
Exec=syncgdrive
Icon=syncgdrive
Categories=Utility;FileTools;Network;
Keywords=sync;drive;google;backup;
StartupNotify=false
Terminal=false
X-GNOME-Autostart-enabled=false
```

### 4.4 Service Systemd V2

```ini
[Unit]
Description=SyncGDrive — Synchronisation locale → Google Drive
Documentation=https://github.com/clyds/SyncGDrive
After=network-online.target graphical-session.target
Wants=network-online.target
ConditionEnvironment=DISPLAY

[Service]
Type=simple
ExecStart=/usr/bin/syncgdrive
Restart=on-failure
RestartSec=10
# Augmentation progressive du délai de restart
StartLimitIntervalSec=300
StartLimitBurst=5
# Environnement pour l'accès D-Bus (systray + keyring)
Environment=DISPLAY=:0
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/%U/bus

[Install]
WantedBy=graphical-session.target
```

### 4.5 Sysctl Inotify

```ini
# /etc/sysctl.d/99-syncgdrive-inotify.conf
# Augmente les limites inotify pour la surveillance de gros dossiers.
# Valeur par défaut : 8192. Recommandé : 524288.
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
```

### 4.6 Paquet Debian (.deb)

**`debian/control`** :
```
Source: syncgdrive
Section: utils
Priority: optional
Maintainer: clyds <clyds@users.noreply.github.com>
Build-Depends: debhelper-compat (= 13), cargo, pkg-config,
 libgtk-4-dev, libadwaita-1-dev, libsqlite3-dev
Standards-Version: 4.6.2
Homepage: https://github.com/clyds/SyncGDrive

Package: syncgdrive
Architecture: amd64
Depends: ${shlibs:Depends}, ${misc:Depends},
 libgtk-4-1, libadwaita-1-0, libsqlite3-0
Recommends: gnome-keyring | kwalletmanager
Description: Synchronisation unidirectionnelle locale → Google Drive
 SyncGDrive surveille un ou plusieurs dossiers locaux et les
 synchronise automatiquement vers Google Drive via l'API REST v3.
 L'ordinateur local est la source de vérité.
```

### 4.7 Makefile

```makefile
PREFIX ?= /usr
DESTDIR ?=
CARGO_FLAGS ?= --release --features ui

.PHONY: build install uninstall clean

build:
	cargo build $(CARGO_FLAGS)

install: build
	install -Dm755 target/release/syncgdrive $(DESTDIR)$(PREFIX)/bin/syncgdrive
	install -Dm644 dist/syncgdrive.service $(DESTDIR)$(PREFIX)/lib/systemd/user/syncgdrive.service
	install -Dm644 dist/syncgdrive.desktop $(DESTDIR)$(PREFIX)/share/applications/syncgdrive.desktop
	install -Dm644 dist/syncgdrive.svg $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/syncgdrive.svg
	install -Dm644 LICENSE $(DESTDIR)$(PREFIX)/share/licenses/syncgdrive/LICENSE

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/syncgdrive
	rm -f $(DESTDIR)$(PREFIX)/lib/systemd/user/syncgdrive.service
	rm -f $(DESTDIR)$(PREFIX)/share/applications/syncgdrive.desktop
	rm -f $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/syncgdrive.svg
	rm -rf $(DESTDIR)$(PREFIX)/share/licenses/syncgdrive/

clean:
	cargo clean
```

---

## 5. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Build sans feature `ui` | Binaire headless — pas de .desktop ni d'icône |
| Pas de DISPLAY (serveur SSH) | Service démarre, fonctionne en headless |
| Mise à jour paquet | Config préservée (dans `$XDG_CONFIG_HOME`, pas dans `/usr`) |
| Désinstallation | Binaire + service supprimés, config utilisateur préservée |
| Arch + KDE (KWallet au lieu de gnome-keyring) | `optdepends` — les deux fonctionnent |
| Cross-compilation ARM | PKGBUILD `arch=('x86_64' 'aarch64')` |

---

## 6. Tests à Écrire

### Validation packaging

- `test_pkgbuild_lint` : `namcap PKGBUILD` sans erreur
- `test_desktop_validate` : `desktop-file-validate syncgdrive.desktop` OK
- `test_systemd_verify` : `systemd-analyze verify syncgdrive.service` OK
- `test_install_uninstall` : Makefile install/uninstall dans un DESTDIR temp

---

## 7. Critères d'Acceptation

- [ ] `makepkg -si` installe correctement sur Arch Linux
- [ ] `dpkg -i syncgdrive_2.0.0_amd64.deb` installe sur Debian/Ubuntu
- [ ] `systemctl --user enable syncgdrive.service` fonctionne post-install
- [ ] L'icône apparaît dans le menu d'applications
- [ ] Le service se relance automatiquement après un crash
- [ ] Les limites inotify sont appliquées après `sysctl --system`
- [ ] La désinstallation est propre (pas de fichiers résiduels dans `/usr`)
- [ ] La config utilisateur (`~/.config/syncgdrive/`) est préservée
- [ ] `desktop-file-validate` : OK
- [ ] `namcap` : OK (Arch)
- [ ] `lintian` : OK (Debian)

