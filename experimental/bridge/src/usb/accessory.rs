//! AA bulk endpoint I/O over FunctionFS ep1 (OUT) and ep2 (IN).

use anyhow::Result;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

pub struct AccessoryConnection {
    reader: Mutex<File>, // ep1 — bulk OUT (car → device)
    writer: Mutex<File>, // ep2 — bulk IN  (device → car)
}

impl AccessoryConnection {
    /// Construct from FunctionFS bulk endpoint file handles.
    /// ep_out: ep1 opened for reading (car→device)
    /// ep_in:  ep2 opened for writing (device→car)
    pub fn from_ffs(ep_out: File, ep_in: File) -> Self {
        Self {
            reader: Mutex::new(ep_out),
            writer: Mutex::new(ep_in),
        }
    }

    /// Read up to `buf.len()` bytes from the bulk OUT endpoint (car → device).
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let n = self.reader.lock().await.read(buf).await?;
        Ok(n)
    }

    /// Write every frame of one AA message to the bulk IN endpoint (device → car).
    ///
    /// The endpoint lock is held for the WHOLE message, not per frame. The peer
    /// reassembles multi-frame messages in a map keyed only by channel id, so two
    /// senders interleaving their FIRST/MIDDLE/LAST frames on one channel would be
    /// spliced into a single garbage message. Callers must therefore always hand a
    /// complete message to this method instead of writing frames one at a time.
    pub async fn write_all_frames(&self, frames: &[Vec<u8>]) -> Result<()> {
        let mut writer = self.writer.lock().await;
        for frame in frames {
            writer.write_all(frame).await?;
        }
        Ok(())
    }
}
