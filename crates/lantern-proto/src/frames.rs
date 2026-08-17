//! Control-channel frames: length-prefixed CBOR on QUIC bidi stream 0.
//!
//! DESIGN.md §2.4. u32 BE length prefix, ≤1 MiB per frame.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("cbor: {0}")]
    Cbor(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub enum ControlFrame {
    /// First frame in each direction after the QUIC handshake.
    #[serde(rename = "hello")]
    Hello {
        proto: u32,
        #[serde(with = "serde_bytes")]
        id: Vec<u8>,
        name: String,
        host: String,
        caps: u32,
    },
    #[serde(rename = "hack")]
    HelloAck { accepted: bool, reason: Option<String> },

    #[serde(rename = "msg")]
    Msg {
        mid: Uuid,
        ts: u64,
        text: String,
        /// "md" or "plain"
        fmt: String,
        reply_to: Option<Uuid>,
        sealed: bool,
        receipt: bool,
    },
    #[serde(rename = "ack")]
    Ack {
        mid: Uuid,
        /// "delivered" | "read" | "opened"
        kind: String,
        ts: u64,
    },

    /// Offer one file as a chunk manifest. `chunks` is the concatenation of
    /// per-chunk BLAKE3 hashes (32 bytes each); `root` is the whole-file
    /// BLAKE3 and doubles as the resume key: a re-offer of the same content
    /// finds the receiver's partial state regardless of xid.
    ///
    /// (Flat chunk-hash list rather than bao-tree for now: 32 B per MiB is
    /// 0.003% overhead, and per-chunk verification + resume need nothing
    /// more until folder streaming lands. Deviation noted in build status.)
    #[serde(rename = "offer")]
    OfferFile {
        xid: Uuid,
        name: String,
        size: u64,
        chunk_size: u32,
        #[serde(with = "serde_bytes")]
        root: Vec<u8>,
        #[serde(with = "serde_bytes")]
        chunks: Vec<u8>,
    },
    /// Receiver's answer: the chunk indices it still needs. Empty = it
    /// already holds every chunk (dedup / completed earlier) — the sender
    /// sends nothing and the receiver finalizes immediately.
    #[serde(rename = "accept")]
    AcceptFile { xid: Uuid, need: Vec<u32> },
    #[serde(rename = "decline")]
    DeclineFile { xid: Uuid, reason: Option<String> },
    /// Receiver's verdict after the transfer completes (or fails).
    #[serde(rename = "xdone")]
    XferDone {
        xid: Uuid,
        ok: bool,
        err: Option<String>,
    },

    #[serde(rename = "err")]
    Error { code: u32, msg: String },
}

impl ControlFrame {
    pub fn to_bytes(&self) -> Result<Vec<u8>, FrameError> {
        let mut body = Vec::new();
        ciborium::ser::into_writer(self, &mut body).map_err(|e| FrameError::Cbor(e.to_string()))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameError> {
        ciborium::de::from_reader(body).map_err(|e| FrameError::Cbor(e.to_string()))
    }
}

/// Read one frame from an async stream.
pub async fn read_frame<R>(reader: &mut R) -> Result<ControlFrame, FrameError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    ControlFrame::from_bytes(&body)
}

/// Write one frame to an async stream.
pub async fn write_frame<W>(writer: &mut W, frame: &ControlFrame) -> Result<(), FrameError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let bytes = frame.to_bytes()?;
    writer.write_all(&bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let f = ControlFrame::Msg {
            mid: Uuid::new_v4(),
            ts: 123,
            text: "hello over the wire".into(),
            fmt: "plain".into(),
            reply_to: None,
            sealed: false,
            receipt: true,
        };
        let bytes = f.to_bytes().unwrap();
        let body = &bytes[4..];
        assert_eq!(ControlFrame::from_bytes(body).unwrap(), f);
    }

    #[test]
    fn garbage_frame_rejected() {
        assert!(ControlFrame::from_bytes(&[0xFF, 0x00, 0xAB]).is_err());
    }
}
