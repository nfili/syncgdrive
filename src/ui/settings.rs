use std::sync::{Arc, Mutex};

use anyhow::Result;
use gtk4::prelude::*;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::engine::EngineCommand;

// ── Standalone runner ─────────────────────────────────────────────────────────

/// Lance la fenêtre Settings de façon autonome.
/// Bloque jusqu'à fermeture de la fenêtre, puis GTK se termine proprement.
///
/// **Pré-requis** : `libadwaita::init()` doit avoir été appelé sur ce thread
/// (géré par le thread `gtk-ui` dans `tray.rs`).
///
/// **Pause/Resume** : envoie `Pause` à l'ouverture et `Resume` à la fermeture
/// (que ce soit via Enregistrer ou la croix ✕).
pub fn run_standalone(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) -> Result<()> {

    // Pause immédiate : le moteur arrête de traiter les tasks.
    let _ = cmd_tx.try_send(EngineCommand::Pause);

    let app = gtk4::Application::builder()
        .application_id("fr.clyds.syncgdrive.settings")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let resume_tx = cmd_tx.clone();
    app.connect_activate(move |app| {
        let (cfg, _) = match AppConfig::load_or_create() {
            Ok(c) => c,
            Err(_) => (AppConfig::default(), true),
        };
        let config = Arc::new(Mutex::new(cfg));
        let app2 = app.downgrade();
        let tx = cmd_tx.clone();
        let _ = open(Some(app), config, move |new_cfg| {
            let _ = tx.try_send(EngineCommand::ApplyConfig(new_cfg));
            // Resume envoyé par le shutdown hook ci-dessous, pas ici.
            if let Some(a) = app2.upgrade() {
                a.quit();
            }
        });
    });

    app.run_with_args::<String>(&[]);

    // La fenêtre est fermée (croix OU enregistrer) : reprendre la synchro.
    let _ = resume_tx.try_send(EngineCommand::Resume);
    Ok(())
}

// ── Fenêtre Settings ──────────────────────────────────────────────────────────

/// Ouvre la fenêtre Settings (libadwaita).
/// `app` : application GTK parente (indispensable pour le main-loop).
pub fn open<F>(
    app: Option<&gtk4::Application>,
    config: Arc<Mutex<AppConfig>>,
    on_save: F,
) -> Result<()>
where
    F: Fn(AppConfig) + 'static,
{
    let cfg = config
        .lock()
        .map_err(|_| anyhow::anyhow!("config mutex poisoned"))?
        .clone();

    // ── Fenêtre ───────────────────────────────────────────────────────────────
    let mut win_builder = libadwaita::Window::builder()
        .title("SyncGDrive — Réglages")
        .default_width(580)
        .default_height(640);
    if let Some(a) = app {
        win_builder = win_builder.application(a);
    }
    let win = win_builder.build();

    let toast_overlay = libadwaita::ToastOverlay::new();

    // Contenu scrollable pour que tout tienne même sur un petit écran.
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();

    let page = libadwaita::PreferencesPage::new();

    // ══════════════════════════════════════════════════════════════════════════
    //  Groupe : Chemins
    // ══════════════════════════════════════════════════════════════════════════
    let grp_paths = libadwaita::PreferencesGroup::builder()
        .title("Chemins")
        .build();

    let local_row = libadwaita::EntryRow::builder()
        .title("Dossier local")
        .text(
            cfg.sync_pairs.first()
                .map(|p| p.local_path.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_str() // <--- On force explicitement en &str
        ).build();

    // Icône de validation live ✅/❌
    let local_status = gtk4::Image::builder()
        .valign(gtk4::Align::Center)
        .build();
    local_row.add_suffix(&local_status);

    // Bouton parcourir pour le dossier local
    let local_browse_btn = gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Parcourir…")
        .css_classes(["flat"])
        .build();
    local_row.add_suffix(&local_browse_btn);
    {
        let lr = local_row.clone();
        let win_weak = win.downgrade();
        local_browse_btn.connect_clicked(move |_| {
            let Some(w) = win_weak.upgrade() else { return };
            let dlg = gtk4::FileDialog::builder()
                .title("Choisir le dossier local à synchroniser")
                .modal(true)
                .build();
            let lr2 = lr.clone();
            dlg.select_folder(Some(&w), gtk4::gio::Cancellable::NONE, move |res| {
                if let Ok(folder) = res {
                    if let Some(path) = folder.path() {
                        lr2.set_text(&path.to_string_lossy());
                    }
                }
            });
        });
    }

    let remote_row = libadwaita::EntryRow::builder()
        .title("URL distante (ex: gdrive:/MonDrive/Backup)")
        .text(
            cfg.sync_pairs.first()
                .map(|p| p.remote_folder_id.clone()) // <--- Correction : on pointe bien sur le remote_folder_id !
                .unwrap_or_default()
                .as_str() // <--- On force explicitement en &str
        ).build();

    // Icône de validation live ✅/❌
    let remote_status = gtk4::Image::builder()
        .valign(gtk4::Align::Center)
        .build();
    remote_row.add_suffix(&remote_status);

    grp_paths.add(&local_row);
    grp_paths.add(&remote_row);
    page.add(&grp_paths);

    // ══════════════════════════════════════════════════════════════════════════
    //  Groupe : Exclusions
    // ══════════════════════════════════════════════════════════════════════════
    let grp_ignore = libadwaita::PreferencesGroup::builder()
        .title("Exclusions")
        .description("Dossiers et fichiers à ne pas synchroniser")
        .build();

    // La ListBox stocke les patterns — chaque ligne = un pattern + bouton ❌
    let ignore_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Remplir avec les patterns existants
    for pat in &cfg.ignore_patterns {
        append_ignore_row(&ignore_list, pat);
    }

    grp_ignore.add(&ignore_list);

    // ── Barre de boutons sous la liste ────────────────────────────────────────
    let btn_bar = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .margin_top(6)
        .halign(gtk4::Align::Start)
        .build();

    // Bouton : ajouter un glob manuellement
    let btn_add_glob = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Ajouter un pattern glob (ex: **/build/**)")
        .build();
    {
        let list = ignore_list.clone();
        let overlay = toast_overlay.clone();
        let win_weak = win.downgrade();
        btn_add_glob.connect_clicked(move |_| {
            let Some(w) = win_weak.upgrade() else { return };
            show_add_glob_dialog(&w, &list, &overlay);
        });
    }

    // Bouton : parcourir pour exclure (sélection multiple fichiers + dossiers)
    let btn_browse = gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .label("Parcourir…")
        .tooltip_text("Choisir des fichiers ou dossiers à exclure (sélection multiple)")
        .build();
    {
        let list = ignore_list.clone();
        let lr = local_row.clone();
        let win_weak = win.downgrade();
        btn_browse.connect_clicked(move |_| {
            let Some(w) = win_weak.upgrade() else { return };
            browse_exclude(&w, &list, &lr);
        });
    }

    btn_bar.append(&btn_add_glob);
    btn_bar.append(&btn_browse);
    grp_ignore.add(&btn_bar);

    page.add(&grp_ignore);

    // ══════════════════════════════════════════════════════════════════════════
    //  Groupe : Options
    // ══════════════════════════════════════════════════════════════════════════
    let grp_opts = libadwaita::PreferencesGroup::builder()
        .title("Options")
        .build();

    let workers_row = libadwaita::SpinRow::new(
        Some(&gtk4::Adjustment::new(
            cfg.max_workers as f64, 1.0, 16.0, 1.0, 1.0, 0.0,
        )),
        1.0,
        0,
    );
    workers_row.set_title("Workers parallèles");

    let notif_row = libadwaita::SwitchRow::builder()
        .title("Notifications bureau")
        .active(cfg.notifications)
        .build();

    grp_opts.add(&workers_row);
    grp_opts.add(&notif_row);
    page.add(&grp_opts);

    // ══════════════════════════════════════════════════════════════════════════
    //  Bouton Enregistrer
    // ══════════════════════════════════════════════════════════════════════════
    let btn_save = gtk4::Button::builder()
        .label("Enregistrer")
        .css_classes(["suggested-action", "pill"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(24)
        .margin_end(24)
        .build();

    // ══════════════════════════════════════════════════════════════════════════
    //  Validation live des champs (icônes ✅/❌ + grisage bouton)
    // ══════════════════════════════════════════════════════════════════════════

    // Validation initiale au chargement
    update_local_status(&local_row, &local_status);
    update_remote_status(&remote_row, &remote_status);
    update_save_sensitivity(&local_row, &remote_row, &btn_save);

    // Callback : chaque frappe dans le champ local
    {
        let ls = local_status.clone();
        let lr = local_row.clone();
        let rr = remote_row.clone();
        let bs = btn_save.clone();
        local_row.connect_changed(move |row| {
            update_local_status(row, &ls);
            update_save_sensitivity(&lr, &rr, &bs);
        });
    }

    // Callback : chaque frappe dans le champ remote
    {
        let rs = remote_status.clone();
        let lr = local_row.clone();
        let rr = remote_row.clone();
        let bs = btn_save.clone();
        remote_row.connect_changed(move |row| {
            update_remote_status(row, &rs);
            update_save_sensitivity(&lr, &rr, &bs);
        });
    }

    // ── Assemblage ────────────────────────────────────────────────────────────
    scroll.set_child(Some(&page));

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let header = libadwaita::HeaderBar::new();
    vbox.append(&header);
    vbox.append(&scroll);
    vbox.append(&btn_save);
    toast_overlay.set_child(Some(&vbox));
    win.set_content(Some(&toast_overlay));

    // ══════════════════════════════════════════════════════════════════════════
    //  Handler : Enregistrer
    // ══════════════════════════════════════════════════════════════════════════
    let local_row2 = local_row.clone();
    let remote_row2 = remote_row.clone();
    let ignore_list2 = ignore_list.clone();
    let config2 = config.clone();
    let win2 = win.clone();
    let overlay2 = toast_overlay.clone();

    btn_save.connect_clicked(move |_| {
        let local = local_row2.text().to_string();
        let remote = remote_row2.text().to_string();
        let patterns = collect_patterns(&ignore_list2);

        let mut new_cfg = config2.lock().unwrap().clone();
        if new_cfg.sync_pairs.is_empty() {
            new_cfg.sync_pairs.push(crate::config::SyncPair {
                name: "Sync principal".into(),
                local_path: std::path::PathBuf::from(&local),
                remote_folder_id: remote,
                provider: "GoogleDrive".into(),
                active: true,
                ignore_patterns: vec![],
            });
        } else {
            new_cfg.sync_pairs[0].local_path = std::path::PathBuf::from(&local);
            new_cfg.sync_pairs[0].remote_folder_id = remote;
        }
        new_cfg.ignore_patterns = patterns;
        new_cfg.max_workers = workers_row.value() as usize;
        new_cfg.notifications = notif_row.is_active();

        if let Err(e) = new_cfg.validate() {
            let toast = libadwaita::Toast::builder()
                .title(e.to_string())
                .timeout(4)
                .build();
            overlay2.add_toast(toast);
            return;
        }

        if let Err(e) = new_cfg.save() {
            let toast = libadwaita::Toast::builder()
                .title(format!("Erreur sauvegarde : {e}"))
                .timeout(4)
                .build();
            overlay2.add_toast(toast);
            return;
        }

        *config2.lock().unwrap() = new_cfg.clone();
        on_save(new_cfg);
        win2.close();
    });

    win.present();
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
//  Helpers Validation Live
// ══════════════════════════════════════════════════════════════════════════════

/// Expand `~/…` vers le home réel (même logique que `config.rs`).
fn settings_expand_tilde(text: &str) -> std::path::PathBuf {
    if text.starts_with("~/") || text == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(&text[2..])
    } else {
        std::path::PathBuf::from(text)
    }
}

/// Vérifie si le champ local est valide (non vide + dossier existant).
fn is_local_valid(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let path = settings_expand_tilde(text.trim());
    path.is_dir()
}

/// Vérifie si le champ remote est valide (protocole reconnu).
fn is_remote_valid(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    const SUPPORTED: &[&str] = &["gdrive://", "gdrive:/", "smb://", "sftp://", "webdav://", "ftp://"];
    SUPPORTED.iter().any(|p| text.starts_with(p))
}

/// Met à jour l'icône ✅/❌ du champ local.
fn update_local_status(row: &libadwaita::EntryRow, icon: &gtk4::Image) {
    let text = row.text().to_string();
    if text.trim().is_empty() {
        icon.set_icon_name(None);
    } else if is_local_valid(&text) {
        icon.set_icon_name(Some("emblem-ok-symbolic"));
        icon.set_tooltip_text(Some("Dossier valide"));
    } else {
        icon.set_icon_name(Some("dialog-error-symbolic"));
        icon.set_tooltip_text(Some("Ce dossier n'existe pas"));
    }
}

/// Met à jour l'icône ✅/❌ du champ remote.
fn update_remote_status(row: &libadwaita::EntryRow, icon: &gtk4::Image) {
    let text = row.text().to_string();
    if text.trim().is_empty() {
        icon.set_icon_name(None);
    } else if is_remote_valid(&text) {
        icon.set_icon_name(Some("emblem-ok-symbolic"));
        icon.set_tooltip_text(Some("Protocole reconnu"));
    } else {
        icon.set_icon_name(Some("dialog-error-symbolic"));
        icon.set_tooltip_text(Some("Protocole invalide (gdrive:/, smb://, sftp://, webdav://, ftp://)"));
    }
}

/// Grise ou active le bouton Enregistrer selon la validité des champs.
fn update_save_sensitivity(
    local_row: &libadwaita::EntryRow,
    remote_row: &libadwaita::EntryRow,
    btn_save: &gtk4::Button,
) {
    let ok = is_local_valid(&local_row.text()) && is_remote_valid(&remote_row.text());
    btn_save.set_sensitive(ok);
}

// ══════════════════════════════════════════════════════════════════════════════
//  Helpers Exclusions
// ══════════════════════════════════════════════════════════════════════════════

/// Ajoute une ligne dans la ListBox avec le pattern et un bouton supprimer.
fn append_ignore_row(list: &gtk4::ListBox, pattern: &str) {
    let row = libadwaita::ActionRow::builder()
        .title(pattern)
        .build();

    let btn_del = gtk4::Button::builder()
        .icon_name("edit-delete-symbolic")
        .css_classes(["flat", "circular", "error"])
        .valign(gtk4::Align::Center)
        .tooltip_text("Retirer cette exclusion")
        .build();

    row.add_suffix(&btn_del);

    let list_ref = list.clone();
    let row_ref = row.clone();
    btn_del.connect_clicked(move |_| {
        list_ref.remove(&row_ref);
    });

    list.append(&row);
}

/// Parcourt la ListBox et retourne tous les patterns non-vides.
fn collect_patterns(list: &gtk4::ListBox) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut idx = 0;
    loop {
        let Some(row) = list.row_at_index(idx) else { break };
        if let Some(action_row) = row.downcast_ref::<libadwaita::ActionRow>() {
            let t = action_row.title().to_string();
            if !t.is_empty() {
                patterns.push(t);
            }
        }
        idx += 1;
    }
    patterns
}

/// Ouvre un FileDialog en sélection **multiple** pour choisir
/// des fichiers ou dossiers à exclure d'un coup.
/// On utilise `select_multiple_folders` car il permet aussi de sélectionner
/// des éléments mixtes sur la plupart des implémentations GTK4 portal.
fn browse_exclude(
    win: &libadwaita::Window,
    list: &gtk4::ListBox,
    local_row: &libadwaita::EntryRow,
) {
    let local_text = local_row.text().to_string();
    let dlg = gtk4::FileDialog::builder()
        .title("Éléments à exclure (sélection multiple)")
        .modal(true)
        .build();

    // Positionne le dialogue dans le local_root si possible
    if !local_text.is_empty() {
        let path = std::path::Path::new(&local_text);
        if path.is_dir() {
            dlg.set_initial_folder(Some(&gtk4::gio::File::for_path(path)));
        }
    }

    let list2 = list.clone();
    let lr2 = local_row.clone();

    dlg.select_multiple_folders(Some(win), gtk4::gio::Cancellable::NONE, move |res| {
        if let Ok(items) = res {
            let root = lr2.text().to_string();
            for i in 0..items.n_items() {
                let Some(obj) = items.item(i) else { continue };
                let Ok(file) = obj.downcast::<gtk4::gio::File>() else { continue };
                if let Some(path) = file.path() {
                    let pattern = path_to_glob(&root, &path);
                    if !pattern.is_empty() {
                        append_ignore_row(&list2, &pattern);
                    }
                }
            }
        }
    });
}

/// Convertit un chemin absolu en glob relatif.
///
/// Sous `local_root` :
///   /home/user/Projets/UltraFs/target  →  **/target/**
///   /home/user/Projets/UltraFs/foo.log →  **/foo.log
///
/// Hors `local_root` : utilise le nom seul.
fn path_to_glob(local_root_text: &str, selected: &std::path::Path) -> String {
    let local_root = std::path::Path::new(local_root_text);
    let name = selected
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return String::new();
    }

    if let Ok(rel) = selected.strip_prefix(local_root) {
        let rel_str = rel.to_string_lossy();
        return if selected.is_dir() {
            format!("**/{rel_str}/**")
        } else {
            format!("**/{rel_str}")
        };
    }

    // Hors du local_root : pattern par nom uniquement
    if selected.is_dir() {
        format!("**/{name}/**")
    } else {
        format!("**/{name}")
    }
}

/// Dialogue pour saisir un glob manuellement.
fn show_add_glob_dialog(
    win: &libadwaita::Window,
    list: &gtk4::ListBox,
    overlay: &libadwaita::ToastOverlay,
) {
    let dlg = libadwaita::Window::builder()
        .title("Ajouter un pattern d'exclusion")
        .default_width(420)
        .default_height(160)
        .modal(true)
        .transient_for(win)
        .build();

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.append(&libadwaita::HeaderBar::new());

    let entry = libadwaita::EntryRow::builder()
        .title("Pattern glob (ex: **/build/**, **/*.log)")
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .build();
    vbox.append(&entry);

    let btn_ok = gtk4::Button::builder()
        .label("Ajouter")
        .css_classes(["suggested-action", "pill"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(24)
        .margin_end(24)
        .build();
    vbox.append(&btn_ok);

    dlg.set_content(Some(&vbox));

    let list2 = list.clone();
    let overlay2 = overlay.clone();
    let dlg2 = dlg.clone();
    let entry2 = entry.clone();
    btn_ok.connect_clicked(move |_| {
        let text = entry2.text().trim().to_string();
        if text.is_empty() {
            let toast = libadwaita::Toast::builder()
                .title("Pattern vide")
                .timeout(2)
                .build();
            overlay2.add_toast(toast);
            return;
        }
        append_ignore_row(&list2, &text);
        dlg2.close();
    });

    // Validation par Entrée
    let btn_ok2 = btn_ok.clone();
    entry.connect_activate(move |_| {
        btn_ok2.emit_clicked();
    });

    dlg.present();
}
