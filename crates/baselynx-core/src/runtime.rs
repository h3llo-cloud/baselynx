//! The core owns one tokio runtime. GPUI's executor is not tokio, so every public async
//! entry point spawns its work here and returns a future that any executor can await.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;

use crate::error::{CoreError, Result};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("baselynx-core")
            .enable_all()
            .build()
            .expect("failed to start the tokio runtime")
    })
}

/// Runs `fut` on the core runtime. Dropping the returned future does not cancel the
/// work; use the cancellation each API provides.
pub fn spawn<F, T>(fut: F) -> impl Future<Output = Result<T>> + Send + 'static
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let handle = runtime().spawn(fut);
    async move {
        match handle.await {
            Ok(result) => result,
            Err(err) if err.is_cancelled() => Err(CoreError::Cancelled),
            Err(err) => Err(CoreError::Internal(format!("task failed: {err}"))),
        }
    }
}

/// Runs blocking work (file I/O, keyring) on the runtime's blocking pool.
pub fn spawn_blocking<F, T>(f: F) -> impl Future<Output = Result<T>> + Send + 'static
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let handle = runtime().spawn_blocking(f);
    async move {
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(CoreError::Internal(format!("task failed: {err}"))),
        }
    }
}
