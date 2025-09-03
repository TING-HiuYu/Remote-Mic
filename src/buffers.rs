use crossbeam_channel::{Receiver, Sender};
use crossbeam_channel as channel;
use parking_lot::Mutex;
use std::sync::Arc;

/// Fixed-size reusable audio buffer pool (lock-per-buffer + free index queue).
/// First 4 bytes in each buffer are reserved for payload length (little endian).
const DEFAULT_BUFFER_SIZE: usize = 4096 * 4; // matches usage in GUI; includes header room
pub struct AudioBufferPool {
    free_tx: Sender<usize>,
    free_rx: Receiver<usize>,
    /// Underlying raw byte storage guarded by lightweight mutexes.
    pub data: Vec<Mutex<Vec<u8>>>,
}

impl AudioBufferPool {
    /// Create a new pool with `count` buffers using the default size.
    pub fn new(count: usize) -> Arc<Self> {
        let size = DEFAULT_BUFFER_SIZE;
        let (tx, rx) = channel::bounded(count);
        let mut data = Vec::with_capacity(count);
        for i in 0..count {
            data.push(Mutex::new(vec![0u8; size]));
            tx.send(i).unwrap();
        }
        Arc::new(Self { free_tx: tx, free_rx: rx, data })
    }

    /// Try acquire a free buffer index (non-blocking).
    pub fn pop(&self) -> Option<usize> {
        self.free_rx.try_recv().ok()
    }

    /// Return a buffer index to the free queue.
    pub fn push(&self, idx: usize) {
        let _ = self.free_tx.send(idx);
    }

}
