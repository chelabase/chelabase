//! A socket's outbound queue (spec §2; RT1 final fix round): bounded by frames and by bytes.
//!
//! Every frame takes `len` byte permits (at most the whole budget, so one frame larger than the
//! budget still fits alone) for as long as it waits in the queue; the reader gives them back as
//! it takes the frame. [`OutTx::try_send`] refuses a frame that finds the queue full either way
//! (the session then closes a client that stopped reading); [`OutTx::send`] waits for room (a
//! channel's task).

use std::sync::Arc;

use tokio::sync::{Semaphore, mpsc};

/// Frames queued for one socket.
pub const OUT_FRAMES: usize = 1024;
/// Bytes queued for one socket.
pub const OUT_BYTES: usize = 4 * 1024 * 1024;

/// The queue is full (frames or bytes), or its reader is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// The writing side; cloned by every task that writes to the socket.
#[derive(Clone)]
pub struct OutTx {
    tx: mpsc::Sender<String>,
    bytes: Arc<Semaphore>,
    max_bytes: usize,
}

/// The reading side (the session's socket loop).
pub struct OutRx {
    rx: mpsc::Receiver<String>,
    bytes: Arc<Semaphore>,
    max_bytes: usize,
}

/// A queue of at most `frames` frames and `max_bytes` bytes.
pub fn out_queue(frames: usize, max_bytes: usize) -> (OutTx, OutRx) {
    let (tx, rx) = mpsc::channel(frames.max(1));
    let max_bytes = max_bytes.clamp(1, Semaphore::MAX_PERMITS);
    let bytes = Arc::new(Semaphore::new(max_bytes));
    (
        OutTx {
            tx,
            bytes: bytes.clone(),
            max_bytes,
        },
        OutRx {
            rx,
            bytes,
            max_bytes,
        },
    )
}

/// The byte permits a frame takes.
fn cost(text: &str, max_bytes: usize) -> u32 {
    u32::try_from(text.len().clamp(1, max_bytes)).unwrap_or(u32::MAX)
}

impl OutTx {
    /// Queues `text` now, or [`Full`].
    pub fn try_send(&self, text: String) -> Result<(), Full> {
        let n = cost(&text, self.max_bytes);
        let permit = self.bytes.try_acquire_many(n).map_err(|_| Full)?;
        match self.tx.try_send(text) {
            Ok(()) => {
                permit.forget();
                Ok(())
            }
            Err(_) => Err(Full),
        }
    }

    /// Queues `text` once there is room; [`Full`] only when the reader is gone.
    pub async fn send(&self, text: String) -> Result<(), Full> {
        let n = cost(&text, self.max_bytes);
        let permit = self.bytes.acquire_many(n).await.map_err(|_| Full)?;
        self.tx.send(text).await.map_err(|_| Full)?;
        permit.forget();
        Ok(())
    }
}

impl OutRx {
    /// The next frame (its bytes given back); `None` once every sender is gone. Cancel-safe.
    pub async fn recv(&mut self) -> Option<String> {
        let text = self.rx.recv().await?;
        self.bytes.add_permits(cost(&text, self.max_bytes) as usize);
        Some(text)
    }
}

impl Drop for OutRx {
    fn drop(&mut self) {
        // Writers waiting for room learn the reader is gone.
        self.bytes.close();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn frames_bound_the_queue() {
        let (tx, _rx) = out_queue(2, 1024);
        assert_eq!(tx.try_send("a".into()), Ok(()));
        assert_eq!(tx.try_send("b".into()), Ok(()));
        assert_eq!(tx.try_send("c".into()), Err(Full), "the third frame");
    }

    #[test]
    fn bytes_bound_the_queue() {
        let (tx, _rx) = out_queue(100, 10);
        assert_eq!(tx.try_send("12345".into()), Ok(()));
        assert_eq!(tx.try_send("1234".into()), Ok(()));
        assert_eq!(tx.try_send("12".into()), Err(Full), "11 bytes past 10");
        assert_eq!(tx.try_send("1".into()), Ok(()), "exactly 10");
        assert_eq!(tx.try_send("1".into()), Err(Full));
    }

    #[tokio::test]
    async fn reading_gives_the_bytes_back() {
        let (tx, mut rx) = out_queue(100, 10);
        tx.try_send("1234567890".into()).unwrap();
        assert_eq!(tx.try_send("x".into()), Err(Full));
        assert_eq!(rx.recv().await.as_deref(), Some("1234567890"));
        assert_eq!(tx.try_send("x".into()), Ok(()));
    }

    #[tokio::test]
    async fn a_frame_over_the_budget_fits_alone() {
        let (tx, mut rx) = out_queue(100, 4);
        tx.try_send("too big for the budget".into()).unwrap();
        assert_eq!(
            tx.try_send("x".into()),
            Err(Full),
            "it takes the whole budget"
        );
        assert_eq!(rx.recv().await.as_deref(), Some("too big for the budget"));
        tx.try_send("x".into()).unwrap();
    }

    #[test]
    fn a_frame_refused_by_the_channel_keeps_no_bytes() {
        let (tx, _rx) = out_queue(1, 100);
        tx.try_send("a".into()).unwrap();
        // The channel is full: the bytes this frame took are returned.
        assert_eq!(tx.try_send("bbbbbbbbbb".into()), Err(Full));
        assert_eq!(tx.bytes.available_permits(), 99);
    }

    #[tokio::test]
    async fn send_waits_for_room_and_try_send_doesnt() {
        let (tx, mut rx) = out_queue(100, 4);
        tx.try_send("abcd".into()).unwrap();
        let waiting = {
            let tx = tx.clone();
            tokio::spawn(async move { tx.send("efgh".into()).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "no room: the writer waits");
        assert_eq!(rx.recv().await.as_deref(), Some("abcd"));
        assert_eq!(waiting.await.unwrap(), Ok(()));
        assert_eq!(rx.recv().await.as_deref(), Some("efgh"));
    }

    #[tokio::test]
    async fn a_waiting_writer_learns_the_reader_is_gone() {
        let (tx, rx) = out_queue(100, 4);
        tx.try_send("abcd".into()).unwrap();
        let waiting = {
            let tx = tx.clone();
            tokio::spawn(async move { tx.send("efgh".into()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(rx);
        assert_eq!(waiting.await.unwrap(), Err(Full));
        assert_eq!(tx.try_send("x".into()), Err(Full));
    }

    #[tokio::test]
    async fn order_is_kept() {
        let (tx, mut rx) = out_queue(100, 1000);
        for i in 0..50 {
            tx.try_send(i.to_string()).unwrap();
        }
        for i in 0..50 {
            assert_eq!(rx.recv().await, Some(i.to_string()));
        }
    }
}
