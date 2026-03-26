//! Régulateur d'appels API (Rate Limiter).
//!
//! Contrairement au limiteur de bande passante qui gère le volume de données (Octets/s),
//! ce module gère la fréquence des requêtes (Requêtes/s) pour respecter les quotas
//! stricts de l'API Google Drive et gérer élégamment les punitions (HTTP 429 Too Many Requests).

use std::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};

/// État interne du régulateur d'appels API.
struct RateState {
    /// Nombre de jetons (requêtes) actuellement disponibles.
    tokens: f64,
    /// Moment de la dernière recharge du seau.
    last_refill: Instant,
    /// Si défini, bloque absolument toute requête jusqu'à cet instant (Backoff).
    locked_until: Option<Instant>,
}

/// Bouclier anti-bannissement pour les requêtes sortantes.
///
/// Implémente l'algorithme "Token Bucket" pour lisser les envois, 
/// et un mécanisme de "Verrouillage Temporel" en cas de punition par le serveur.
pub struct ApiRateLimiter {
    max_rps: u32,
    state: Mutex<RateState>,
}

impl ApiRateLimiter {
    /// Crée un nouveau régulateur.
    ///
    /// `max_rps` définit le nombre maximum de requêtes par seconde autorisées.
    pub fn new(max_rps: u32) -> Self {
        Self {
            max_rps,
            state: Mutex::new(RateState {
                tokens: max_rps as f64,
                last_refill: Instant::now(),
                locked_until: None,
            }),
        }
    }

    /// Demande l'autorisation d'envoyer une requête API.
    ///
    /// Cette méthode est asynchrone : si le quota est dépassé, elle mettra 
    /// gracieusement la coroutine en pause (`sleep`) jusqu'à ce qu'un créneau se libère.
    pub async fn acquire(&self) {
        if self.max_rps == 0 {
            return;
        }

        loop {
            let delay = {
                let mut state = self.state.lock().unwrap();
                let now = Instant::now();

                // 1. Sommes-nous sous le coup d'un HTTP 429 (Too Many Requests) ?
                if let Some(lock_end) = state.locked_until {
                    if now < lock_end {
                        // Punition en cours : on calcule le temps d'attente restant
                        Some(lock_end.duration_since(now))
                    } else {
                        // Punition terminée : on lève le verrou
                        state.locked_until = None;
                        None
                    }
                } else {
                    // 2. Token Bucket classique pour le lissage normal (RPS)
                    let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                    state.tokens += elapsed * (self.max_rps as f64);

                    // On plafonne le nombre de jetons au maximum autorisé
                    if state.tokens > self.max_rps as f64 {
                        state.tokens = self.max_rps as f64;
                    }
                    state.last_refill = now;

                    // Consommation d'un jeton (1 requête)
                    if state.tokens >= 1.0 {
                        state.tokens -= 1.0;
                        None
                    } else {
                        // Pas assez de jetons : on calcule le temps d'attente exact
                        let wait_secs = (1.0 - state.tokens) / (self.max_rps as f64);
                        Some(Duration::from_secs_f64(wait_secs))
                    }
                }
            }; // 🛡️ Le verrou du Mutex est relâché ici, AVANT le `.await` !

            if let Some(d) = delay {
                sleep(d).await;
            } else {
                break; // Autorisation accordée, on sort de la boucle
            }
        }
    }

    /// Réagit à une erreur HTTP 429 (Too Many Requests).
    ///
    /// Verrouille globalement le moteur pour la durée exigée par le serveur Google
    /// via l'en-tête `Retry-After`.
    pub async fn handle_rate_limit(&self, retry_after_secs: u64) {
        let mut state = self.state.lock().unwrap();
        let lock_end = Instant::now() + Duration::from_secs(retry_after_secs);

        // On ne met à jour que si la nouvelle punition est plus longue que l'actuelle
        // (utile si plusieurs workers reçoivent un 429 en même temps).
        if let Some(current_lock) = state.locked_until {
            if lock_end > current_lock {
                state.locked_until = Some(lock_end);
            }
        } else {
            state.locked_until = Some(lock_end);
        }
    }
}