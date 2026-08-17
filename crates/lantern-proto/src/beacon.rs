//! Discovery beacons: signed, self-describing presence datagrams.
//!
//! Wire layout (DESIGN.md §2.2):
//! ```text
//! 0   4  magic    0x2A 'W' 'S' 'P'
//! 4   1  version  0x01
//! 5   1  type
//! 6   2  flags    (BE)
//! 8   2  length   payload length (BE)
//! 10  N  payload  CBOR map, integer keys, sig at key 99
//! ```
//! The Ed25519 signature covers `"wisp-beacon-v1\0" || version || type ||
//! flags || canonical-CBOR(payload minus sig)` — header included, so `type`
//! and `flags` are not malleable.

use ciborium::value::{Integer, Value};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::{BEACON_SIG_CONTEXT, MAGIC, MAX_BEACON_BYTES, VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BeaconType {
    Hello = 0x01,
    HelloAck = 0x02,
    Bye = 0x03,
    Ping = 0x04,
    Pong = 0x05,
}

impl BeaconType {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Self::Hello,
            0x02 => Self::HelloAck,
            0x03 => Self::Bye,
            0x04 => Self::Ping,
            0x05 => Self::Pong,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum DeviceClass {
    #[default]
    Desktop = 0,
    Laptop = 1,
    Server = 2,
    Handheld = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PresenceState {
    #[default]
    Active = 0,
    Idle = 1,
    Away = 2,
    Dnd = 3,
    Invisible = 4,
}

impl PresenceState {
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Active,
            1 => Self::Idle,
            2 => Self::Away,
            3 => Self::Dnd,
            4 => Self::Invisible,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BeaconError {
    #[error("datagram too short")]
    TooShort,
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported version {0}")]
    BadVersion(u8),
    #[error("unknown beacon type {0:#x}")]
    BadType(u8),
    #[error("length field disagrees with datagram")]
    BadLength,
    #[error("payload is not a CBOR map of integer keys")]
    BadPayload,
    #[error("missing required field {0}")]
    MissingField(u32),
    #[error("field {0} has the wrong type")]
    BadField(u32),
    #[error("bad identity key")]
    BadKey,
    #[error("signature verification failed")]
    BadSignature,
    #[error("beacon exceeds {MAX_BEACON_BYTES} bytes")]
    TooLarge,
    #[error("cbor: {0}")]
    Cbor(String),
}

/// A fully-parsed, signature-verified (on decode) discovery beacon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub beacon_type: BeaconType,
    pub flags: u16,
    /// Ed25519 public key — the device identity.
    pub id: [u8; 32],
    pub name: String,
    pub host: String,
    pub group: String,
    pub device: DeviceClass,
    /// QUIC port the peer listens on.
    pub port: u16,
    pub state: PresenceState,
    pub status: String,
    pub avatar: Option<[u8; 32]>,
    pub caps: u32,
    pub seq: u64,
    pub boot: [u8; 8],
    /// Unix milliseconds; staleness hint only.
    pub ts: u64,
}

// Payload map keys (§2.2).
const K_ID: u32 = 1;
const K_NAME: u32 = 2;
const K_HOST: u32 = 3;
const K_GROUP: u32 = 4;
const K_DEVICE: u32 = 5;
const K_PORT: u32 = 6;
const K_STATE: u32 = 7;
const K_STATUS: u32 = 8;
const K_AVATAR: u32 = 9;
const K_CAPS: u32 = 10;
const K_SEQ: u32 = 11;
const K_BOOT: u32 = 12;
const K_TS: u32 = 13;
const K_SIG: u32 = 99;

impl Beacon {
    /// Encode and sign. Fails only if the result would exceed the datagram cap.
    pub fn encode(&self, signing_key: &SigningKey) -> Result<Vec<u8>, BeaconError> {
        let unsigned = self.payload_map_without_sig();
        let payload_bytes = to_canonical_cbor(&unsigned)?;

        let mut sig_input =
            Vec::with_capacity(BEACON_SIG_CONTEXT.len() + 4 + payload_bytes.len());
        sig_input.extend_from_slice(BEACON_SIG_CONTEXT);
        sig_input.push(VERSION);
        sig_input.push(self.beacon_type as u8);
        sig_input.extend_from_slice(&self.flags.to_be_bytes());
        sig_input.extend_from_slice(&payload_bytes);
        let sig: Signature = signing_key.sign(&sig_input);

        // Final payload = unsigned map + sig at key 99.
        let mut entries = match unsigned {
            Value::Map(m) => m,
            _ => unreachable!(),
        };
        entries.push((
            Value::Integer(Integer::from(K_SIG)),
            Value::Bytes(sig.to_bytes().to_vec()),
        ));
        let full_payload = to_canonical_cbor(&Value::Map(entries))?;

        let total = 10 + full_payload.len();
        if total > MAX_BEACON_BYTES {
            return Err(BeaconError::TooLarge);
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(self.beacon_type as u8);
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&(full_payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&full_payload);
        Ok(out)
    }

    /// Decode and verify the signature. A beacon that fails verification is
    /// indistinguishable from garbage and is rejected outright.
    pub fn decode(datagram: &[u8]) -> Result<Self, BeaconError> {
        if datagram.len() < 10 {
            return Err(BeaconError::TooShort);
        }
        if datagram[..4] != MAGIC {
            return Err(BeaconError::BadMagic);
        }
        let version = datagram[4];
        if version != VERSION {
            return Err(BeaconError::BadVersion(version));
        }
        let beacon_type =
            BeaconType::from_u8(datagram[5]).ok_or(BeaconError::BadType(datagram[5]))?;
        let flags = u16::from_be_bytes([datagram[6], datagram[7]]);
        let len = u16::from_be_bytes([datagram[8], datagram[9]]) as usize;
        if datagram.len() != 10 + len {
            return Err(BeaconError::BadLength);
        }

        let value: Value = ciborium::de::from_reader(&datagram[10..])
            .map_err(|e| BeaconError::Cbor(e.to_string()))?;
        let entries = match value {
            Value::Map(m) => m,
            _ => return Err(BeaconError::BadPayload),
        };

        let mut fields: Vec<(u32, Value)> = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            let key: u32 = match k {
                Value::Integer(i) => u32::try_from(i128::from(i))
                    .map_err(|_| BeaconError::BadPayload)?,
                _ => return Err(BeaconError::BadPayload),
            };
            fields.push((key, v));
        }

        let sig_bytes = take_bytes(&mut fields, K_SIG)?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| BeaconError::BadField(K_SIG))?;
        let sig = Signature::from_bytes(&sig_arr);

        // Re-encode the remaining fields canonically and verify.
        let mut sorted = fields.clone();
        sorted.sort_by_key(|(k, _)| *k);
        let unsigned_map = Value::Map(
            sorted
                .iter()
                .map(|(k, v)| (Value::Integer(Integer::from(*k)), v.clone()))
                .collect(),
        );
        let payload_bytes = to_canonical_cbor(&unsigned_map)?;

        let id_bytes = take_bytes(&mut fields, K_ID)?;
        let id: [u8; 32] = id_bytes
            .try_into()
            .map_err(|_| BeaconError::BadField(K_ID))?;
        let vk = VerifyingKey::from_bytes(&id).map_err(|_| BeaconError::BadKey)?;

        let mut sig_input =
            Vec::with_capacity(BEACON_SIG_CONTEXT.len() + 4 + payload_bytes.len());
        sig_input.extend_from_slice(BEACON_SIG_CONTEXT);
        sig_input.push(version);
        sig_input.push(beacon_type as u8);
        sig_input.extend_from_slice(&flags.to_be_bytes());
        sig_input.extend_from_slice(&payload_bytes);
        vk.verify(&sig_input, &sig)
            .map_err(|_| BeaconError::BadSignature)?;

        Ok(Beacon {
            beacon_type,
            flags,
            id,
            name: take_text(&mut fields, K_NAME)?,
            host: take_text(&mut fields, K_HOST)?,
            group: take_text_opt(&mut fields, K_GROUP)?.unwrap_or_default(),
            device: match take_uint_opt(&mut fields, K_DEVICE)?.unwrap_or(0) {
                0 => DeviceClass::Desktop,
                1 => DeviceClass::Laptop,
                2 => DeviceClass::Server,
                3 => DeviceClass::Handheld,
                _ => DeviceClass::Desktop,
            },
            port: take_uint(&mut fields, K_PORT)? as u16,
            state: PresenceState::from_u8(take_uint_opt(&mut fields, K_STATE)?.unwrap_or(0) as u8)
                .unwrap_or_default(),
            status: take_text_opt(&mut fields, K_STATUS)?.unwrap_or_default(),
            avatar: match take_bytes_opt(&mut fields, K_AVATAR)? {
                Some(b) => Some(b.try_into().map_err(|_| BeaconError::BadField(K_AVATAR))?),
                None => None,
            },
            caps: take_uint_opt(&mut fields, K_CAPS)?.unwrap_or(0) as u32,
            seq: take_uint(&mut fields, K_SEQ)?,
            boot: take_bytes(&mut fields, K_BOOT)?
                .try_into()
                .map_err(|_| BeaconError::BadField(K_BOOT))?,
            ts: take_uint(&mut fields, K_TS)?,
        })
    }

    fn payload_map_without_sig(&self) -> Value {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        let mut put = |k: u32, v: Value| {
            entries.push((Value::Integer(Integer::from(k)), v));
        };
        put(K_ID, Value::Bytes(self.id.to_vec()));
        put(K_NAME, Value::Text(self.name.clone()));
        put(K_HOST, Value::Text(self.host.clone()));
        if !self.group.is_empty() {
            put(K_GROUP, Value::Text(self.group.clone()));
        }
        put(K_DEVICE, Value::Integer(Integer::from(self.device as u8)));
        put(K_PORT, Value::Integer(Integer::from(self.port)));
        put(K_STATE, Value::Integer(Integer::from(self.state as u8)));
        if !self.status.is_empty() {
            put(K_STATUS, Value::Text(self.status.clone()));
        }
        if let Some(av) = self.avatar {
            put(K_AVATAR, Value::Bytes(av.to_vec()));
        }
        put(K_CAPS, Value::Integer(Integer::from(self.caps)));
        put(K_SEQ, Value::Integer(Integer::from(self.seq)));
        put(K_BOOT, Value::Bytes(self.boot.to_vec()));
        put(K_TS, Value::Integer(Integer::from(self.ts)));
        // Keys are already ascending by construction (1..13, sig added later).
        Value::Map(entries)
    }
}

fn to_canonical_cbor(v: &Value) -> Result<Vec<u8>, BeaconError> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).map_err(|e| BeaconError::Cbor(e.to_string()))?;
    Ok(out)
}

fn take(fields: &mut Vec<(u32, Value)>, key: u32) -> Option<Value> {
    let idx = fields.iter().position(|(k, _)| *k == key)?;
    Some(fields.remove(idx).1)
}

fn take_bytes(fields: &mut Vec<(u32, Value)>, key: u32) -> Result<Vec<u8>, BeaconError> {
    take_bytes_opt(fields, key)?.ok_or(BeaconError::MissingField(key))
}

fn take_bytes_opt(
    fields: &mut Vec<(u32, Value)>,
    key: u32,
) -> Result<Option<Vec<u8>>, BeaconError> {
    match take(fields, key) {
        None => Ok(None),
        Some(Value::Bytes(b)) => Ok(Some(b)),
        Some(_) => Err(BeaconError::BadField(key)),
    }
}

fn take_text(fields: &mut Vec<(u32, Value)>, key: u32) -> Result<String, BeaconError> {
    take_text_opt(fields, key)?.ok_or(BeaconError::MissingField(key))
}

fn take_text_opt(
    fields: &mut Vec<(u32, Value)>,
    key: u32,
) -> Result<Option<String>, BeaconError> {
    match take(fields, key) {
        None => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t)),
        Some(_) => Err(BeaconError::BadField(key)),
    }
}

fn take_uint(fields: &mut Vec<(u32, Value)>, key: u32) -> Result<u64, BeaconError> {
    take_uint_opt(fields, key)?.ok_or(BeaconError::MissingField(key))
}

fn take_uint_opt(fields: &mut Vec<(u32, Value)>, key: u32) -> Result<Option<u64>, BeaconError> {
    match take(fields, key) {
        None => Ok(None),
        Some(Value::Integer(i)) => Ok(Some(
            u64::try_from(i128::from(i)).map_err(|_| BeaconError::BadField(key))?,
        )),
        Some(_) => Err(BeaconError::BadField(key)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn test_beacon(id: [u8; 32]) -> Beacon {
        Beacon {
            beacon_type: BeaconType::Hello,
            flags: 0,
            id,
            name: "Mira".into(),
            host: "mira-mbp".into(),
            group: "Design".into(),
            device: DeviceClass::Laptop,
            port: 3940,
            state: PresenceState::Active,
            status: "shipping v2".into(),
            avatar: None,
            caps: 0b1111,
            seq: 7,
            boot: [1, 2, 3, 4, 5, 6, 7, 8],
            ts: 1_755_000_000_000,
        }
    }

    #[test]
    fn round_trip() {
        let sk = SigningKey::generate(&mut OsRng);
        let beacon = test_beacon(sk.verifying_key().to_bytes());
        let bytes = beacon.encode(&sk).unwrap();
        assert!(bytes.len() <= MAX_BEACON_BYTES);
        // Fixed Bit clear: never mistakable for QUIC.
        assert_eq!(bytes[0] & 0xC0, 0x00);
        let decoded = Beacon::decode(&bytes).unwrap();
        assert_eq!(decoded, beacon);
    }

    #[test]
    fn tampered_header_rejected() {
        let sk = SigningKey::generate(&mut OsRng);
        let beacon = test_beacon(sk.verifying_key().to_bytes());
        let mut bytes = beacon.encode(&sk).unwrap();
        // Flip HELLO into BYE — the classic roster-eviction replay.
        bytes[5] = BeaconType::Bye as u8;
        assert!(matches!(
            Beacon::decode(&bytes),
            Err(BeaconError::BadSignature)
        ));
    }

    #[test]
    fn tampered_payload_rejected() {
        let sk = SigningKey::generate(&mut OsRng);
        let beacon = test_beacon(sk.verifying_key().to_bytes());
        let mut bytes = beacon.encode(&sk).unwrap();
        let n = bytes.len();
        bytes[n / 2] ^= 0xFF;
        assert!(Beacon::decode(&bytes).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let sk = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng);
        // Beacon claims `other`'s identity but is signed by `sk`.
        let beacon = test_beacon(other.verifying_key().to_bytes());
        let bytes = beacon.encode(&sk).unwrap();
        assert!(matches!(
            Beacon::decode(&bytes),
            Err(BeaconError::BadSignature)
        ));
    }

    #[test]
    fn garbage_rejected_not_panicking() {
        for len in 0..64 {
            let garbage = vec![0xAB; len];
            let _ = Beacon::decode(&garbage);
        }
        let mut near = vec![0x2A, b'W', b'S', b'P', 1, 1, 0, 0, 0, 5];
        near.extend_from_slice(&[0xFF; 5]);
        let _ = Beacon::decode(&near);
    }
}
