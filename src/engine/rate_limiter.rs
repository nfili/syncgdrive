use std::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};

struct RateState {
    tokens: f64,
    last_refill: Instant,
    locked_until: Option<Instant>,
}

pub struct ApiRateLimiter {
    max_rps: u32,
    state: Mutex<RateState>,
}

impl ApiRateLimiter {
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

    /// Attend qu'un slot soit disponible avant d'envoyer une requête API.
    pub async fn acquire(&self) {
        if self.max_rps == 0 { return; }

        loop {
            let delay = {
                let mut state = self.state.lock().unwrap();
                let now = Instant::now();

                // 1. Sommes-nous sous le coup d'un HTTP 429 ?
                if let Some(lock_end) = state.locked_until {
                    if now < lock_end {
                        Some(lock_end.duration_since(now))
                    } else {
                        state.locked_until = None;
                        None
                    }
                } else {
                    // 2. Token Bucket classique pour le Rate Limiting (RPS)
                    let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                    state.tokens += elapsed * (self.max_rps as f64);
                    if state.tokens > self.max_rps as f64 {
                        state.tokens = self.max_rps as f64;
                    }
                    state.last_refill = now;

                    if state.tokens >= 1.0 {
                        state.tokens -= 1.0;
                        None
                    } else {
                        let wait_secs = (1.0 - state.tokens) / (self.max_rps as f64);
                        Some(Duration::from_secs_f64(wait_secs))
                    }
                }
            };

            if let Some(d) = delay {
                sleep(d).await;
            } else {
                break;
            }
        }
    }

    /// Gère un 429 Too Many Requests — met en pause globale.
    pub async fn handle_rate_limit(&self, retry_after_secs: u64) {
        let mut state = self.state.lock().unwrap();
        let lock_end = Instant::now() + Duration::from_secs(retry_after_secs);

        // On ne met à jour que si la nouvelle punition est plus longue que l'actuelle
        if let Some(current_lock) = state.locked_until {
            if lock_end > current_lock {
                state.locked_until = Some(lock_end);
            }
        } else {
            state.locked_until = Some(lock_end);
        }
    }
}