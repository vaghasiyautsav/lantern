//! Wisp/1 wire types: discovery beacons and control frames.
//!
//! Design references: DESIGN.md §2.2 (beacons), §2.4 (control frames).

pub mod beacon;
pub mod frames;

pub use beacon::{Beacon, BeaconError, BeaconType, DeviceClass, PresenceState};
pub use frames::{read_frame, write_frame, ControlFrame, FrameError};

/// First byte 0x2A keeps the QUIC Fixed Bit clear (RFC 9000 §17.2 reserves
/// `0x00–0x3F` for sharing a port with non-QUIC protocols).
pub const MAGIC: [u8; 4] = [0x2A, b'W', b'S', b'P'];
pub const VERSION: u8 = 0x01;

/// Default UDP port for discovery (and, later, shared QUIC).
pub const DISCOVERY_PORT: u16 = 3939;

/// Maximum beacon datagram size: never fragments on a 1280-MTU path
/// (1280 − 40 IPv6 − 8 UDP = 1232; 1200 leaves headroom).
pub const MAX_BEACON_BYTES: usize = 1200;

/// Domain-separation prefix for beacon signatures (§2.2).
pub const BEACON_SIG_CONTEXT: &[u8] = b"wisp-beacon-v1\x00";
