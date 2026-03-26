//! Suivi télémétrique et contrôle du débit réseau.
//!
//! Ce module fournit les outils nécessaires pour surveiller en temps réel la vitesse
//! de transfert (algorithme de fenêtre glissante) et limiter la consommation
//! de bande passante (algorithme du Token Bucket).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Instantané des compteurs, prêt à être consommé par l'interface utilisateur.
///
/// L'utilisation de ce clone permet à l'UI de s'actualiser sans jamais bloquer
/// les threads de synchronisation (zéro contention).
#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    /// Nombre total de fichiers à traiter dans le batch actuel.
    pub total_files: usize,
    /// Nombre de fichiers déjà traités (uploadés, supprimés ou renommés).
    pub done_files: usize,
    /// Poids total (en octets) des fichiers à uploader.
    pub total_bytes: u64,
    /// Volume de données (en octets) déjà envoyé sur le réseau.
    pub sent_bytes: u64,
    /// Poids du fichier actuellement en cours de transfert.
    pub current_file_size: u64,
    /// Nombre d'octets transférés pour le fichier actuel.
    pub current_bytes_sent: u64,
    /// Dossier parent du fichier en cours.
    pub current_dir: String,
    /// Nom du fichier en cours de traitement.
    pub current_name: String,
    /// Vitesse instantanée estimée en octets par seconde (Bps).
    pub speed_bps: u64,
    /// Temps restant estimé formaté pour l'affichage (ex: "~2 min").
    pub eta_string: String,
}

/// État interne soumis à des mutations complexes (protégé par Mutex).
struct TrackerState {
    pub current_dir: String,
    pub current_name: String,
    pub current_file_size: u64,
    pub current_bytes_sent: u64,
    /// Historique des transferts récents pour calculer la vitesse glissante.
    pub speed_samples: VecDeque<(Instant, u64)>,
}

/// Sonde télémétrique centralisée pour les opérations de transfert.
///
/// Combine des compteurs atomiques pour les métriques simples (accès ultra-rapide
/// depuis de multiples workers) et un Mutex pour les calculs de vitesse complexes.
pub struct ProgressTracker {
    pub total_files: AtomicUsize,
    pub done_files: AtomicUsize,
    pub total_bytes: AtomicU64,
    pub sent_bytes: AtomicU64,
    state: Mutex<TrackerState>,
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressTracker {
    /// Crée un nouveau tracker vierge.
    pub fn new() -> Self {
        Self {
            total_files: AtomicUsize::new(0),
            done_files: AtomicUsize::new(0),
            total_bytes: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            state: Mutex::new(TrackerState {
                current_dir: String::new(),
                current_name: String::new(),
                current_file_size: 0,
                current_bytes_sent: 0,
                speed_samples: VecDeque::with_capacity(100),
            }),
        }
    }

    /// Définit le fichier actuellement en cours de transfert pour l'affichage UI.
    pub fn set_current_file(&self, dir: String, name: String, size: u64) {
        let mut state = self.state.lock().unwrap();
        state.current_dir = dir;
        state.current_name = name;
        state.current_file_size = size;
        state.current_bytes_sent = 0;
    }

    /// Enregistre une portion d'octets envoyée sur le réseau.
    ///
    /// Doit être appelé très fréquemment (ex: par les callbacks de `reqwest`)
    /// pour maintenir une courbe de vitesse précise.
    pub fn record_bytes(&self, n: u64) {
        let current_total = self.sent_bytes.fetch_add(n, Ordering::Relaxed) + n;

        let mut state = self.state.lock().unwrap();
        state.current_bytes_sent += n;

        let now = Instant::now();
        state.speed_samples.push_back((now, current_total));

        // Nettoyage de la fenêtre glissante : on ne garde que les 5 dernières secondes
        while let Some(&(ts, _)) = state.speed_samples.front() {
            if now.duration_since(ts).as_secs_f64() > 5.0 {
                state.speed_samples.pop_front();
            } else {
                break;
            }
        }
    }

    // ─── LOGIQUE INTERNE (SANS VERROU) ────────────────────────────────────────

    /// Calcule la vitesse instantanée en fonction des échantillons conservés.
    fn calculate_speed(state: &TrackerState) -> u64 {
        if state.speed_samples.len() < 2 {
            return 0;
        }

        let (first_ts, first_bytes) = state.speed_samples.front().unwrap();
        let (last_ts, last_bytes) = state.speed_samples.back().unwrap();

        let elapsed = last_ts.duration_since(*first_ts).as_secs_f64();
        if elapsed > 0.0 {
            (last_bytes.saturating_sub(*first_bytes) as f64 / elapsed) as u64
        } else {
            0
        }
    }

    /// Estime le temps restant en secondes (ETA).
    fn calculate_eta_secs(speed: u64, total: u64, sent: u64) -> Option<u64> {
        if speed == 0 || sent >= total {
            return None;
        }
        Some((total - sent) / speed)
    }

    /// Formate un délai en secondes vers une chaîne lisible par l'humain.
    fn format_eta(eta: Option<u64>) -> String {
        match eta {
            None => "⏳ En attente…".into(),
            Some(secs) if secs < 60 => format!("~{} s", secs),
            Some(secs) if secs < 3600 => format!("~{} min", secs / 60),
            Some(secs) => format!("~{} h", secs / 3600),
        }
    }

    // ─── API PUBLIQUE (AVEC VERROU) ───────────────────────────────────────────

    /// Retourne la vitesse de transfert actuelle en octets par seconde.
    pub fn speed_bps(&self) -> u64 {
        let state = self.state.lock().unwrap();
        Self::calculate_speed(&state)
    }

    /// Retourne l'estimation du temps restant en secondes, si calculable.
    pub fn eta_secs(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        let speed = Self::calculate_speed(&state);
        let total = self.total_bytes.load(Ordering::Relaxed);
        let sent = self.sent_bytes.load(Ordering::Relaxed);
        Self::calculate_eta_secs(speed, total, sent)
    }

    /// Retourne l'estimation du temps restant formatée en texte.
    pub fn human_eta(&self) -> String {
        let eta = self.eta_secs();
        Self::format_eta(eta)
    }

    /// Capture un instantané complet et cohérent de l'état du tracker.
    pub fn snapshot(&self) -> ProgressSnapshot {
        // Lecture des compteurs atomiques (lock-free)
        let total_files = self.total_files.load(Ordering::Relaxed);
        let done_files = self.done_files.load(Ordering::Relaxed);
        let total_bytes = self.total_bytes.load(Ordering::Relaxed);
        let sent_bytes = self.sent_bytes.load(Ordering::Relaxed);

        // Verrouillage unique pour les variables corrélées
        let state = self.state.lock().unwrap();
        let speed = Self::calculate_speed(&state);
        let eta = Self::calculate_eta_secs(speed, total_bytes, sent_bytes);

        ProgressSnapshot {
            total_files,
            done_files,
            total_bytes,
            sent_bytes,
            current_file_size: state.current_file_size,
            current_bytes_sent: state.current_bytes_sent,
            current_dir: state.current_dir.clone(),
            current_name: state.current_name.clone(),
            speed_bps: speed,
            eta_string: Self::format_eta(eta),
        }
    }
}

// ─── LIMITATION DE BANDE PASSANTE ─────────────────────────────────────────────

/// État du seau à jetons pour le limiteur de débit.
struct BucketState {
    /// Nombre de jetons (octets) actuellement disponibles.
    tokens: f64,
    /// Moment de la dernière recharge du seau.
    last_refill: Instant,
}

/// Régulateur de débit asynchrone basé sur l'algorithme "Token Bucket".
///
/// Permet de brider la vitesse d'upload pour ne pas saturer la connexion de l'utilisateur.
pub struct BandwidthLimiter {
    limit_bps: u64,
    state: Mutex<BucketState>,
}

impl BandwidthLimiter {
    /// Instancie un nouveau limiteur. Une limite de `0` désactive la restriction.
    pub fn new(limit_kbps: u64) -> Self {
        let limit_bps = limit_kbps * 1024;
        Self {
            limit_bps,
            state: Mutex::new(BucketState {
                tokens: limit_bps as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Demande la permission d'envoyer `n` octets.
    ///
    /// Met la tâche asynchrone en sommeil si le débit maximum est atteint,
    /// jusqu'à ce que suffisamment de jetons soient générés.
    pub async fn acquire(&self, n: u64) {
        if self.limit_bps == 0 {
            return;
        }

        loop {
            let delay = {
                let mut state = self.state.lock().unwrap();
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();

                state.tokens += elapsed * (self.limit_bps as f64);
                if state.tokens > self.limit_bps as f64 {
                    state.tokens = self.limit_bps as f64;
                }
                state.last_refill = now;

                if state.tokens >= n as f64 {
                    state.tokens -= n as f64;
                    None
                } else {
                    let needed = n as f64 - state.tokens;
                    let wait_secs = needed / (self.limit_bps as f64);
                    Some(Duration::from_secs_f64(wait_secs))
                }
            }; // 🛡️ Le verrou du Mutex est relâché ici, AVANT le `.await` !

            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_tracker_record_bytes() {
        let tracker = ProgressTracker::new();
        // S'il n'y a pas de vitesse, le tracker doit renvoyer notre marqueur vide
        assert_eq!(tracker.snapshot().eta_string, "⏳ En attente…");
    }

    #[test]
    fn test_progress_tracker_eta_zero_speed() {
        let tracker = ProgressTracker::new();
        // Le tracker doit renvoyer ta chaîne par défaut quand la vitesse est de 0.
        assert_eq!(tracker.snapshot().eta_string, "⏳ En attente…");
    }

    #[test]
    fn test_human_eta_format() {
        assert_eq!(ProgressTracker::format_eta(Some(45)), "~45 s");
        assert_eq!(ProgressTracker::format_eta(Some(125)), "~2 min");
        assert_eq!(ProgressTracker::format_eta(Some(3650)), "~1 h");
        assert_eq!(ProgressTracker::format_eta(None), "⏳ En attente…");
    }

    #[tokio::test]
    async fn test_bandwidth_limiter_unlimited() {
        let limiter = BandwidthLimiter::new(0);
        let start = Instant::now();
        limiter.acquire(10_000_000).await; // 10 Mo
        assert!(
            start.elapsed() < Duration::from_millis(5),
            "Illimité doit retourner immédiatement"
        );
    }

    #[tokio::test]
    async fn test_bandwidth_limiter_throttle() {
        let limiter = BandwidthLimiter::new(10); // 10 Ko/s

        // On vide le bucket manuellement pour le test
        limiter.state.lock().unwrap().tokens = 0.0;

        let start = Instant::now();
        // Pour avoir 10 Ko (10240 octets) à 10 Ko/s, il faut attendre ~1 seconde.
        // On demande 5 Ko (5120), on devrait attendre ~0.5s.
        limiter.acquire(5120).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(450),
            "Le limiteur n'a pas ralenti le flux. Durée: {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "Le limiteur est trop lent. Durée: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_progress_tracker_speed() {
        let tracker = ProgressTracker::new();

        // On simule manuellement l'historique pour valider la formule mathématique
        let mut state = tracker.state.lock().unwrap();
        let now = Instant::now();

        // On a envoyé 0 octets il y a 2 secondes
        state
            .speed_samples
            .push_back((now - Duration::from_secs(2), 0));
        // On a envoyé 5000 octets maintenant
        state.speed_samples.push_back((now, 5000));
        drop(state);

        // La vitesse doit être exactement de 2500 octets/seconde
        assert_eq!(tracker.speed_bps(), 2500);
    }

    #[test]
    fn test_progress_snapshot_accuracy() {
        let tracker = ProgressTracker::new();

        // On bombarde le tracker avec 100 appels très rapides de 10 octets
        for _ in 0..100 {
            tracker.record_bytes(10);
        }

        let snap = tracker.snapshot();

        // Le snapshot DOIT refléter la somme exacte, sans aucune perte
        assert_eq!(
            snap.current_bytes_sent, 1000,
            "Le snapshot a perdu des octets locaux"
        );
        assert_eq!(
            tracker.sent_bytes.load(Ordering::Relaxed),
            1000,
            "Les compteurs atomiques ont divergé"
        );
    }
}
