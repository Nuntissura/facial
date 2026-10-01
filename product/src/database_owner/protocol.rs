//! Bounded, typed private-pipe protocol. No endpoint or operator data is logged.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::PathBuf,
    time::Instant,
};
use surrealdb::types::Value;

pub(super) const MAX_FRAME: usize = 128 * 1024 * 1024;
pub(super) const MAX_ACKNOWLEDGED: usize = 32;
pub(super) const VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Startup {
    pub version: u32,
    pub database_root: PathBuf,
    pub database: String,
    pub schema_version: u64,
    pub owner_id: String,
    pub epoch: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub owner_id: String,
    pub epoch: u64,
    pub operation_id: String,
    pub sql: String,
    pub bindings: BTreeMap<String, Value>,
    pub digest: String,
    pub acknowledged: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_delay_after_commit_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub owner_id: String,
    pub epoch: u64,
    pub operation_id: String,
    pub digest: Option<String>,
    pub results: Vec<Result<Value, String>>,
    pub error: Option<String>,
}

pub(super) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    encode_until(value, None)
}

// CBOR forwards strings/bytes directly to Write; unlike JSON it does not scan
// an entire string for escapes before the first cooperative deadline check.
struct Encoder {
    bytes: Option<Vec<u8>>,
    hash: Sha256,
    length: usize,
    deadline: Option<Instant>,
}
impl Encoder {
    fn check(&self) -> std::io::Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(std::io::Error::other(
                "safe_unit_timeout: database deadline before dispatch",
            ));
        }
        Ok(())
    }
}
impl Write for Encoder {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.check()?;
        if bytes.len() > MAX_FRAME.saturating_sub(self.length) {
            return Err(std::io::Error::other("database_owner_frame_limit"));
        }
        for chunk in bytes.chunks(64 * 1024) {
            self.check()?;
            #[cfg(test)]
            if chunk.len() >= 64 * 1024 {
                let duration = ENCODE_DELAY.with(|delay| delay.get());
                if self.deadline.is_some_and(|deadline| {
                    duration >= deadline.saturating_duration_since(Instant::now())
                }) {
                    return Err(std::io::Error::other(
                        "safe_unit_timeout: database deadline during encoding",
                    ));
                }
                std::thread::sleep(duration);
                self.check()?;
            }
            if let Some(output) = self.bytes.as_mut() {
                output.extend_from_slice(chunk);
            } else {
                self.hash.update(chunk);
            }
            self.length += chunk.len();
            self.check()?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.check()
    }
}

#[cfg(test)]
thread_local! { static ENCODE_DELAY: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(std::time::Duration::ZERO) }; }
#[cfg(test)]
pub(super) struct EncodeDelay(std::time::Duration);
#[cfg(test)]
pub(super) fn delay_encode(duration: std::time::Duration) -> EncodeDelay {
    EncodeDelay(ENCODE_DELAY.with(|delay| delay.replace(duration)))
}
#[cfg(test)]
impl Drop for EncodeDelay {
    fn drop(&mut self) {
        ENCODE_DELAY.with(|delay| delay.set(self.0));
    }
}

pub(super) fn encode_until<T: Serialize>(
    value: &T,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, String> {
    let mut encoder = Encoder {
        bytes: Some(Vec::with_capacity(MAX_FRAME)),
        hash: Sha256::new(),
        length: 0,
        deadline,
    };
    encoder.check().map_err(|error| error.to_string())?;
    ciborium::ser::into_writer(value, &mut encoder).map_err(|error| error.to_string())?;
    encoder.check().map_err(|error| error.to_string())?;
    Ok(encoder.bytes.take().expect("frame encoder"))
}

pub(super) fn digest_until<T: Serialize>(
    value: &T,
    deadline: Option<Instant>,
) -> Result<String, String> {
    let mut encoder = Encoder {
        bytes: None,
        hash: Sha256::new(),
        length: 0,
        deadline,
    };
    encoder.check().map_err(|error| error.to_string())?;
    ciborium::ser::into_writer(value, &mut encoder).map_err(|error| error.to_string())?;
    encoder.check().map_err(|error| error.to_string())?;
    Ok(format!("{:x}", encoder.hash.finalize()))
}

pub(super) fn write_frame(output: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_FRAME {
        return Err("database_owner_frame_limit".into());
    }
    output
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(|e| e.to_string())?;
    output.write_all(bytes).map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())
}

pub(super) fn read_frame<T: serde::de::DeserializeOwned>(
    input: &mut impl Read,
) -> Result<T, String> {
    let mut header = [0; 4];
    input.read_exact(&mut header).map_err(|e| e.to_string())?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("database_owner_frame_limit".into());
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    let mut input = std::io::Cursor::new(bytes);
    let result = ciborium::de::from_reader(&mut input)
        .map_err(|e| format!("database owner protocol: {e}"))?;
    if input.position() != length as u64 {
        return Err("database owner trailing frame data".into());
    }
    Ok(result)
}
