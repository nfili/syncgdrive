use gtk4::prelude::*;
use libadwaita::prelude::*;
use crate::engine::EngineCommand;

// NOUVEAU : On ne crée plus l'application ici, on la reçoit du Serveur UI !
pub fn show_help_in_app(app: &libadwaita::Application, cmd_tx: tokio::sync::mpsc::Sender<crate::engine::EngineCommand>) {

    let _ = cmd_tx.try_send(EngineCommand::OpenHelp);

    let window = libadwaita::Window::builder()
        .application(app) // On s'accroche proprement au serveur
        .title("Aide & Configuration")
        .default_width(600)
        .default_height(500)
        .modal(true)
        .build();

    let header_bar = libadwaita::HeaderBar::builder().build();

    let main_vbox = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    main_vbox.append(&header_bar);

    let clamp = libadwaita::Clamp::builder()
        .maximum_size(540)
        .margin_top(32)
        .margin_bottom(32)
        .margin_start(24)
        .margin_end(24)
        .build();

    let help_text = r#"
<span size="x-large" weight="bold">Bienvenue dans SyncGDrive</span>

Ce logiciel synchronise vos fichiers locaux avec Google Drive de manière transparente et sécurisée.

<span size="large" weight="bold">1. Configuration du compte Google</span>
Pour que l'application puisse accéder à votre Drive, vous devez lui fournir des identifiants (Client ID et Secret).

• Allez sur la <a href="https://console.cloud.google.com">Console Google Cloud</a>.
• Créez un nouveau projet.
• Dans "API et services", activez l'API <b>Google Drive API</b>.
• Allez dans "Écran de consentement OAuth" et configurez-le en mode "Externe" (ajoutez votre adresse email dans les utilisateurs tests).
• Allez dans "Identifiants", cliquez sur "Créer des identifiants" > "ID client OAuth".
• Choisissez le type <b>Application de bureau</b>.

<span size="large" weight="bold">2. Enregistrement des identifiants</span>
Copiez le <b>Client ID</b> et le <b>Client Secret</b> fournis par Google, puis insérez-les dans le fichier <tt>.env</tt> situé à la racine du projet ou dans votre dossier de configuration :

<tt>SYNCGDRIVE_CLIENT_ID="votre_client_id"
SYNCGDRIVE_CLIENT_SECRET="votre_client_secret"</tt>

<span size="large" weight="bold">3. Première synchronisation</span>
Au démarrage, l'application ouvrira votre navigateur Web. Connectez-vous avec votre compte Google et autorisez l'accès. Un jeton sécurisé sera alors chiffré et sauvegardé localement sur votre machine.

<span size="large" weight="bold">Dépannage</span>
Si votre jeton expire ou est révoqué, l'application passera en mode <i>Hors-Ligne</i>. Elle vous demandera automatiquement de vous reconnecter lors de la prochaine tentative de synchronisation.
"#;

    let label = gtk4::Label::builder()
        .use_markup(true)
        .wrap(true)
        .xalign(0.0) // Aligner le texte à gauche
        .label(help_text)
        .build();

    // On rend les liens cliquables pour ouvrir le navigateur par défaut
    label.connect_activate_link(|_, uri| {
        let _ = std::process::Command::new("xdg-open").arg(uri).spawn();
        gtk4::glib::Propagation::Stop
    });

    clamp.set_child(Some(&label));

    // CORRECTION ICI : On utilise gtk4 et on retire l'attribut .application()
    let scrolled_window = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .vexpand(true)
        .build();

    main_vbox.append(&scrolled_window);
    window.set_content(Some(&main_vbox));

    window.connect_close_request(move |_| {
        let _ = cmd_tx.try_send(crate::engine::EngineCommand::Resume);
        gtk4::glib::Propagation::Proceed
    });

    // On affiche directement la fenêtre !
    window.present();
}