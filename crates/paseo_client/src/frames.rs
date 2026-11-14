//! Binary WebSocket frames: the chunked-transport layer and terminal stream frames.
//!
//! Opcode space of the first byte (from `@getpaseo/protocol`): 0x01-0x08 terminal stream,
//! 0x10-0x12 file transfer, 0x30 host routing, 0x31 chunked transport.

use anyhow::{Context as _, Result, anyhow, bail};

/// Largest logical message the daemon accepts or sends (`MAX_WEBSOCKET_MESSAGE_BYTES`).
pub const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;

const CHUNK_OPCODE: u8 = 0x31;
const CHUNK_VERSION: u8 = 1;
const CHUNK_HEADER_BYTES: usize = 4;
const CHUNK_KIND_TEXT: u8 = 0;
const CHUNK_KIND_BINARY: u8 = 1;
const CHUNK_FLAG_START: u8 = 1;
const CHUNK_FLAG_END: u8 = 2;

/// A logical message after chunk reassembly.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

/// Reassembles messages the daemon splits into `[0x31, version, kind, flags, payload...]`
/// frames. The daemon only chunks after the client advertised `binary_chunking_v1` and its
/// `server_info` has the `binaryChunkingV1` feature, and the client never needs to chunk the small
/// messages it sends.
#[derive(Default)]
pub struct ChunkAssembler {
    enabled: bool,
    assembly: Option<(u8, Vec<u8>)>,
}

impl ChunkAssembler {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.assembly = None;
        }
    }

    /// Returns the logical message once complete. An error means the stream is corrupt and the
    /// connection must be closed, as the daemon's own client does.
    pub fn receive(&mut self, frame: Frame) -> Result<Option<Frame>> {
        let bytes = match frame {
            Frame::Binary(bytes) if bytes.first() == Some(&CHUNK_OPCODE) => bytes,
            other => {
                if self.assembly.is_some() {
                    bail!("chunked message interrupted by another frame");
                }
                return Ok(Some(other));
            }
        };

        if !self.enabled {
            bail!("received a chunk frame without negotiated chunking");
        }
        let Some(&[_, version, kind, flags]) = bytes.get(..CHUNK_HEADER_BYTES) else {
            bail!("chunk frame shorter than its header");
        };
        if bytes.len() == CHUNK_HEADER_BYTES
            || version != CHUNK_VERSION
            || kind > CHUNK_KIND_BINARY
            || flags & !(CHUNK_FLAG_START | CHUNK_FLAG_END) != 0
        {
            bail!("invalid chunk header");
        }
        let is_start = flags & CHUNK_FLAG_START != 0;
        let is_end = flags & CHUNK_FLAG_END != 0;
        if is_start == self.assembly.is_some() {
            bail!("invalid chunk sequence");
        }

        let (assembly_kind, buffer) = self.assembly.get_or_insert_with(|| (kind, Vec::new()));
        if *assembly_kind != kind {
            bail!("chunk kind changed mid-message");
        }
        buffer.extend_from_slice(&bytes[CHUNK_HEADER_BYTES..]);
        if buffer.len() > MAX_MESSAGE_BYTES {
            bail!("chunked message exceeds {MAX_MESSAGE_BYTES} bytes");
        }
        if !is_end {
            return Ok(None);
        }

        let (kind, buffer) = self
            .assembly
            .take()
            .ok_or_else(|| anyhow!("chunk assembly missing"))?;
        Ok(Some(if kind == CHUNK_KIND_TEXT {
            Frame::Text(String::from_utf8(buffer).context("chunked text message is not UTF-8")?)
        } else {
            Frame::Binary(buffer)
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminalOpcode {
    /// Daemon to client: raw terminal output bytes.
    Output = 0x01,
    /// Client to daemon: raw input bytes.
    Input = 0x02,
    /// Client to daemon: JSON `{rows, cols, intent?}`.
    Resize = 0x03,
    /// Daemon to client: JSON terminal state (grid, scrollback, cursor).
    Snapshot = 0x04,
    /// Daemon to client: bytes that restore the screen when attaching.
    Restore = 0x05,
    ReplayBegin = 0x06,
    ReplayData = 0x07,
    ReplayEnd = 0x08,
}

impl TerminalOpcode {
    fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x01 => Self::Output,
            0x02 => Self::Input,
            0x03 => Self::Resize,
            0x04 => Self::Snapshot,
            0x05 => Self::Restore,
            0x06 => Self::ReplayBegin,
            0x07 => Self::ReplayData,
            0x08 => Self::ReplayEnd,
            _ => return None,
        })
    }
}

/// `[opcode, slot, payload...]`, where `slot` is assigned by `subscribe_terminal_response`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFrame {
    pub opcode: TerminalOpcode,
    pub slot: u8,
    pub payload: Vec<u8>,
}

impl TerminalFrame {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&opcode, rest) = bytes.split_first()?;
        let (&slot, payload) = rest.split_first()?;
        Some(Self {
            opcode: TerminalOpcode::from_byte(opcode)?,
            slot,
            payload: payload.to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(2 + self.payload.len());
        bytes.push(self.opcode as u8);
        bytes.push(self.slot);
        bytes.extend_from_slice(&self.payload);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: u8, flags: u8, payload: &[u8]) -> Frame {
        let mut bytes = vec![CHUNK_OPCODE, CHUNK_VERSION, kind, flags];
        bytes.extend_from_slice(payload);
        Frame::Binary(bytes)
    }

    #[test]
    fn reassembles_chunked_text() {
        let mut assembler = ChunkAssembler::default();
        assembler.set_enabled(true);
        assert_eq!(
            assembler
                .receive(chunk(CHUNK_KIND_TEXT, CHUNK_FLAG_START, b"{\"a\":"))
                .unwrap(),
            None
        );
        assert_eq!(
            assembler
                .receive(chunk(CHUNK_KIND_TEXT, CHUNK_FLAG_END, b"1}"))
                .unwrap(),
            Some(Frame::Text("{\"a\":1}".into()))
        );
        assert_eq!(
            assembler
                .receive(chunk(
                    CHUNK_KIND_BINARY,
                    CHUNK_FLAG_START | CHUNK_FLAG_END,
                    &[1, 2]
                ))
                .unwrap(),
            Some(Frame::Binary(vec![1, 2]))
        );
    }

    #[test]
    fn passes_through_unchunked_frames_and_rejects_interleaving() {
        let mut assembler = ChunkAssembler::default();
        assert_eq!(
            assembler.receive(Frame::Text("x".into())).unwrap(),
            Some(Frame::Text("x".into()))
        );
        assert!(
            assembler
                .receive(chunk(CHUNK_KIND_TEXT, CHUNK_FLAG_START, b"x"))
                .is_err(),
            "chunks must be rejected unless negotiated"
        );

        assembler.set_enabled(true);
        assembler
            .receive(chunk(CHUNK_KIND_TEXT, CHUNK_FLAG_START, b"x"))
            .unwrap();
        assert!(assembler.receive(Frame::Text("y".into())).is_err());
    }

    #[test]
    fn terminal_frames_round_trip() {
        let frame = TerminalFrame {
            opcode: TerminalOpcode::Input,
            slot: 7,
            payload: b"ls\r".to_vec(),
        };
        assert_eq!(TerminalFrame::decode(&frame.encode()), Some(frame));
        assert_eq!(TerminalFrame::decode(&[0x10, 0]), None);
        assert_eq!(TerminalFrame::decode(&[0x01]), None);
    }
}
