//! Channel-based embedding service.
//!
//! [`EmbedService`] wraps a single [`crate::Embedder`] behind a
//! `crossbeam_channel` so multiple callers — the watcher (batch) and the
//! query path (`code_search`) — can submit work without contending on a
//! mutex.  The worker processes requests serially on its own OS thread;
//! ONNX inference is CPU-bound and benefits from sequential batching anyway.
//!
//! `EmbedService` is `Send + Sync` (it only holds a `Sender`) so it can be
//! shared via `Arc` with zero locking on the hot path.

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};

use crate::embedder::Embedder;
use crate::error::EmbedError;

struct Request {
    texts: Vec<String>,
    reply: Sender<Result<Vec<Vec<f32>>, EmbedError>>,
}

/// Lock-free embedding service backed by a dedicated worker thread.
///
/// Clone the `Arc<EmbedService>` and call [`embed_batch`] / [`embed_one`]
/// from any thread — channel send is the only synchronisation point.
///
/// [`embed_batch`]: EmbedService::embed_batch
/// [`embed_one`]: EmbedService::embed_one
pub struct EmbedService {
    tx: Sender<Request>,
}

impl EmbedService {
    /// Spawn the worker thread and return a handle.
    ///
    /// The worker exits when all `EmbedService` handles are dropped (the
    /// channel closes automatically).
    ///
    /// # Errors
    /// Returns `Err` if the OS refuses to spawn the thread (e.g., resource limits).
    pub fn spawn(mut embedder: Embedder) -> std::io::Result<Self> {
        let (tx, rx): (Sender<Request>, Receiver<Request>) = unbounded();
        std::thread::Builder::new()
            .name("recon-embed-worker".into())
            .spawn(move || {
                while let Ok(req) = rx.recv() {
                    let result = embedder.embed_batch(&req.texts);
                    let _ = req.reply.send(result);
                }
            })?;
        Ok(Self { tx })
    }

    /// Embed a batch of texts. Blocks the calling thread until the worker
    /// finishes — use from `spawn_blocking` or a dedicated thread.
    pub fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
        let (reply_tx, reply_rx) = bounded(1);
        self.tx
            .send(Request {
                texts,
                reply: reply_tx,
            })
            .map_err(|_| EmbedError::Model("embed service closed".into()))?;
        reply_rx
            .recv()
            .map_err(|_| EmbedError::Model("embed worker died".into()))?
    }

    /// Embed a single text passage.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vecs = self.embed_batch(vec![text.to_string()])?;
        vecs.pop()
            .ok_or_else(|| EmbedError::Model("empty embed result".into()))
    }
}
