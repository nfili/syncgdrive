# Variables globales d'installation
APP_NAME = syncgdrive
PREFIX ?= /usr
DESTDIR ?=
BINDIR = $(PREFIX)/bin
DATADIR = $(PREFIX)/share
SYSTEMD_USER_DIR = $(PREFIX)/lib/systemd/user
SYSCTL_DIR = $(PREFIX)/lib/sysctl.d
LICENSE_DIR = $(DATADIR)/licenses/$(APP_NAME)
CARGO_FLAGS ?= --release --features ui

.PHONY: all build install uninstall clean

all: build

build:
	@echo "==> Compilation de $(APP_NAME) en mode release..."
	cargo build $(CARGO_FLAGS)
	@echo "==> Compilation terminée. Binaire disponible dans target/release/$(APP_NAME)."

install:
	@echo "==> Installation de $(APP_NAME) dans $(DESTDIR)$(PREFIX)..."
	# 1. Binaire
	install -Dm755 target/release/$(APP_NAME) "$(DESTDIR)$(BINDIR)/$(APP_NAME)"
	# 2. Icône SVG (Ton logo hybride SGD)
	install -Dm644 dist/$(APP_NAME).svg "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_NAME).svg"
	# 3. Fichier Desktop (Menu des applications)
	install -Dm644 dist/$(APP_NAME).desktop "$(DESTDIR)$(DATADIR)/applications/$(APP_NAME).desktop"
	# 4. Service Systemd (User)
	install -Dm644 dist/$(APP_NAME).service "$(DESTDIR)$(SYSTEMD_USER_DIR)/$(APP_NAME).service"
	# 5. Configuration sysctl (Limites Inotify)
	install -Dm644 dist/99-$(APP_NAME)-inotify.conf "$(DESTDIR)$(SYSCTL_DIR)/99-$(APP_NAME)-inotify.conf"
	install -Dm644 dist/.env.example "$(DESTDIR)$(DATADIR)/doc/$(APP_NAME)/.env.example"
	install -Dm644 LICENSE "$(DESTDIR)$(LICENSE_DIR)/LICENSE"
	@echo "==> Fichiers installés."

uninstall:
	@echo "==> Désinstallation de $(APP_NAME)..."
	rm -f "$(DESTDIR)$(BINDIR)/$(APP_NAME)"
	rm -f "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_NAME).svg"
	rm -f "$(DESTDIR)$(DATADIR)/applications/$(APP_NAME).desktop"
	rm -f "$(DESTDIR)$(SYSTEMD_USER_DIR)/$(APP_NAME).service"
	rm -f "$(DESTDIR)$(SYSCTL_DIR)/99-$(APP_NAME)-inotify.conf"
	rm -f "$(DESTDIR)$(DATADIR)/doc/$(APP_NAME)/.env.example"
	rm -rf "$(DESTDIR)$(LICENSE_DIR)"
	@echo "==> Note: Le dossier personnel ~/.config/$(APP_NAME) n'est pas touché."

clean:
	@echo "==> Nettoyage du dossier target..."
	cargo clean