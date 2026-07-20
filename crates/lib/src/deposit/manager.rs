//! Background driver: one serialized tick loop over the deposit engine.
//!
//! Serialization is deliberate — a tick is the unit of consistency (nonce
//! read -> schedule -> send), so ticks never overlap; `retry_parked` and
//! shutdown share the same exclusion via the engine mutex.

use std::sync::Arc;

use platform_utils::tokio;

use crate::deposit::engine::DepositEngine;

pub(crate) struct DepositManager {
    engine: Arc<DepositEngine>,
    /// Serializes ticks against integrator-triggered engine entry points.
    tick_lock: Arc<tokio::sync::Mutex<()>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl DepositManager {
    pub(crate) fn new(engine: Arc<DepositEngine>) -> Self {
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        Self {
            engine,
            tick_lock: Arc::new(tokio::sync::Mutex::new(())),
            shutdown_tx,
        }
    }

    pub(crate) fn engine(&self) -> &Arc<DepositEngine> {
        &self.engine
    }

    /// Run `f` with the tick exclusion held (integrator entry points).
    pub(crate) async fn with_engine_exclusive<F, T>(&self, f: F) -> T
    where
        F: AsyncFnOnce(&DepositEngine) -> T,
    {
        let _guard = self.tick_lock.lock().await;
        f(&self.engine).await
    }

    /// Spawn the background tick loop. A no-op loop is never spawned: the
    /// caller only constructs a manager when deposits are configured.
    pub(crate) fn start(&self, interval_secs: u64) {
        let engine = Arc::clone(&self.engine);
        let tick_lock = Arc::clone(&self.tick_lock);
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
            loop {
                tokio::select! {
                    // Named binding: the tick value is `Instant` on native
                    // but `()` on WASM, and a bare `_` trips
                    // `ignored_unit_patterns` there.
                    _tick = ticker.tick() => {
                        let _guard = tick_lock.lock().await;
                        engine.tick().await;
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            tracing::debug!("deposit manager shutting down");
                            return;
                        }
                    }
                }
            }
        });
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}
