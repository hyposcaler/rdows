use rdows_client::rdows_core::error::RdowsError;
use rdows_client::rdows_core::memory::{LKey, RKey};
use rdows_client::rdows_core::queue::ScatterGatherEntry;
use rdows_client::RdowsConnection;
use serde::Serialize;

pub const SLOT_COUNT: usize = 64;
pub const SLOT_SIZE: usize = 1024;
pub const MR_SIZE: u64 = (SLOT_COUNT * SLOT_SIZE) as u64;
pub const MAX_KEY_LEN: usize = 255;
pub const MAX_VALUE_LEN: usize = 764;

const OFF_STATUS: usize = 0;
const OFF_KEY_LEN: usize = 1;
const OFF_VAL_LEN: usize = 3;
const OFF_KEY: usize = 5;
const OFF_VALUE: usize = 260;

#[derive(Debug)]
pub enum KvError {
    Rdma(RdowsError),
    TableFull,
    KeyTooLong,
    ValueTooLong,
}

impl From<RdowsError> for KvError {
    fn from(e: RdowsError) -> Self {
        KvError::Rdma(e)
    }
}

impl std::fmt::Display for KvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvError::Rdma(e) => write!(f, "RDMA error: {e}"),
            KvError::TableFull => write!(f, "hash table full"),
            KvError::KeyTooLong => write!(f, "key exceeds {MAX_KEY_LEN} bytes"),
            KvError::ValueTooLong => write!(f, "value exceeds {MAX_VALUE_LEN} bytes"),
        }
    }
}

#[derive(Serialize, Clone)]
pub struct PutResult {
    pub operation: &'static str,
    pub slot: usize,
    pub offset: String,
    pub bytes: usize,
    pub key: String,
    pub value: String,
    pub probes: usize,
    pub update: bool,
}

#[derive(Serialize, Clone)]
pub struct GetResult {
    pub operation: &'static str,
    pub slot: usize,
    pub offset: String,
    pub bytes: usize,
    pub key: String,
    pub value: String,
    pub probes: usize,
    pub raw_hex: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct DeleteResult {
    pub operation: &'static str,
    pub slot: usize,
    pub offset: String,
    pub bytes: usize,
    pub key: String,
    pub probes: usize,
    pub found: bool,
}

#[derive(Serialize, Clone)]
pub struct SlotInfo {
    pub index: usize,
    pub offset: String,
    pub occupied: bool,
    pub key: Option<String>,
    pub value: Option<String>,
}

pub struct KvStore {
    conn: RdowsConnection,
    remote_rkey: RKey,
    local_lkey: LKey,
    pub session_id: u32,
    pub remote_rkey_value: u32,
    next_wrid: u64,
}

fn hash_key(key: &[u8]) -> usize {
    let mut hash: u32 = 2_166_136_261;
    for &b in key {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    (hash as usize) % SLOT_COUNT
}

fn encode_slot(key: &str, value: &str) -> [u8; SLOT_SIZE] {
    let mut slot = [0u8; SLOT_SIZE];
    slot[OFF_STATUS] = 0x01;
    slot[OFF_KEY_LEN..OFF_KEY_LEN + 2].copy_from_slice(&(key.len() as u16).to_be_bytes());
    slot[OFF_VAL_LEN..OFF_VAL_LEN + 2].copy_from_slice(&(value.len() as u16).to_be_bytes());
    slot[OFF_KEY..OFF_KEY + key.len()].copy_from_slice(key.as_bytes());
    slot[OFF_VALUE..OFF_VALUE + value.len()].copy_from_slice(value.as_bytes());
    slot
}

fn decode_slot(data: &[u8; SLOT_SIZE]) -> Option<(String, String)> {
    if data[OFF_STATUS] == 0x00 {
        return None;
    }
    let key_len = u16::from_be_bytes([data[OFF_KEY_LEN], data[OFF_KEY_LEN + 1]]) as usize;
    let val_len = u16::from_be_bytes([data[OFF_VAL_LEN], data[OFF_VAL_LEN + 1]]) as usize;
    let key = String::from_utf8_lossy(&data[OFF_KEY..OFF_KEY + key_len]).to_string();
    let value = String::from_utf8_lossy(&data[OFF_VALUE..OFF_VALUE + val_len]).to_string();
    Some((key, value))
}

fn hex_dump_region(data: &[u8], base_offset: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, chunk) in data.chunks(16).enumerate() {
        if i >= max_lines {
            break;
        }
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| if (0x20..=0x7E).contains(&b) { b as char } else { '.' })
            .collect();
        lines.push(format!(
            "{:04X}  {:<48}  {}",
            base_offset + i * 16,
            hex.join(" "),
            ascii
        ));
    }
    lines
}

fn slot_hex_lines(data: &[u8; SLOT_SIZE]) -> Vec<String> {
    let mut lines = Vec::new();

    // Header: status + key_len + val_len (bytes 0-4)
    lines.push("--- header (status, key_len, val_len) ---".to_string());
    lines.extend(hex_dump_region(&data[0..OFF_KEY], 0, 1));

    // Key region: show only the occupied portion + 1 line of padding
    let key_len = u16::from_be_bytes([data[OFF_KEY_LEN], data[OFF_KEY_LEN + 1]]) as usize;
    let key_show = key_len.div_ceil(16) + 1;
    lines.push(format!("--- key data ({key_len} bytes) ---"));
    lines.extend(hex_dump_region(
        &data[OFF_KEY..OFF_KEY + key_show.min(16) * 16],
        OFF_KEY,
        key_show.min(16),
    ));

    // Value region: show only the occupied portion + 1 line of padding
    let val_len = u16::from_be_bytes([data[OFF_VAL_LEN], data[OFF_VAL_LEN + 1]]) as usize;
    let val_show = val_len.div_ceil(16) + 1;
    let val_end = (OFF_VALUE + val_show * 16).min(SLOT_SIZE);
    lines.push(format!("--- value data ({val_len} bytes) ---"));
    lines.extend(hex_dump_region(
        &data[OFF_VALUE..val_end],
        OFF_VALUE,
        val_show.min(48),
    ));

    lines
}

impl KvStore {
    pub fn new(
        conn: RdowsConnection,
        session_id: u32,
        remote_rkey: RKey,
        local_lkey: LKey,
    ) -> Self {
        let remote_rkey_value = remote_rkey.0;
        Self {
            conn,
            remote_rkey,
            local_lkey,
            session_id,
            remote_rkey_value,
            next_wrid: 1,
        }
    }

    fn next_wrid(&mut self) -> u64 {
        let id = self.next_wrid;
        self.next_wrid += 1;
        id
    }

    async fn read_slot(&mut self, slot_index: usize) -> Result<[u8; SLOT_SIZE], KvError> {
        let remote_va = (slot_index * SLOT_SIZE) as u64;
        let wrid = self.next_wrid();
        self.conn
            .rdma_read(
                wrid,
                self.remote_rkey,
                remote_va,
                SLOT_SIZE as u64,
                self.local_lkey,
                0,
            )
            .await?;
        self.conn.poll_cq(1);
        let data = self.conn.read_local_mr(self.local_lkey, 0, SLOT_SIZE)?;
        let mut buf = [0u8; SLOT_SIZE];
        buf.copy_from_slice(&data);
        Ok(buf)
    }

    async fn write_slot(
        &mut self,
        slot_index: usize,
        slot_data: &[u8; SLOT_SIZE],
    ) -> Result<(), KvError> {
        let remote_va = (slot_index * SLOT_SIZE) as u64;
        self.conn.write_local_mr(self.local_lkey, 0, slot_data)?;
        let wrid = self.next_wrid();
        self.conn
            .rdma_write(
                wrid,
                self.remote_rkey,
                remote_va,
                &[ScatterGatherEntry {
                    lkey: self.local_lkey,
                    offset: 0,
                    length: SLOT_SIZE as u32,
                }],
            )
            .await?;
        self.conn.poll_cq(1);
        Ok(())
    }

    pub async fn put(&mut self, key: &str, value: &str) -> Result<PutResult, KvError> {
        if key.len() > MAX_KEY_LEN {
            return Err(KvError::KeyTooLong);
        }
        if value.len() > MAX_VALUE_LEN {
            return Err(KvError::ValueTooLong);
        }

        let start = hash_key(key.as_bytes());
        for i in 0..SLOT_COUNT {
            let idx = (start + i) % SLOT_COUNT;
            let slot = self.read_slot(idx).await?;
            match decode_slot(&slot) {
                None => {
                    let new_slot = encode_slot(key, value);
                    self.write_slot(idx, &new_slot).await?;
                    return Ok(PutResult {
                        operation: "RDMA_WRITE",
                        slot: idx,
                        offset: format!("0x{:04X}", idx * SLOT_SIZE),
                        bytes: SLOT_SIZE,
                        key: key.to_string(),
                        value: value.to_string(),
                        probes: i + 1,
                        update: false,
                    });
                }
                Some((existing_key, _)) if existing_key == key => {
                    let new_slot = encode_slot(key, value);
                    self.write_slot(idx, &new_slot).await?;
                    return Ok(PutResult {
                        operation: "RDMA_WRITE",
                        slot: idx,
                        offset: format!("0x{:04X}", idx * SLOT_SIZE),
                        bytes: SLOT_SIZE,
                        key: key.to_string(),
                        value: value.to_string(),
                        probes: i + 1,
                        update: true,
                    });
                }
                Some(_) => continue,
            }
        }
        Err(KvError::TableFull)
    }

    pub async fn get(&mut self, key: &str) -> Result<Option<GetResult>, KvError> {
        let start = hash_key(key.as_bytes());
        for i in 0..SLOT_COUNT {
            let idx = (start + i) % SLOT_COUNT;
            let slot = self.read_slot(idx).await?;
            match decode_slot(&slot) {
                None => return Ok(None),
                Some((existing_key, value)) if existing_key == key => {
                    return Ok(Some(GetResult {
                        operation: "RDMA_READ",
                        slot: idx,
                        offset: format!("0x{:04X}", idx * SLOT_SIZE),
                        bytes: SLOT_SIZE,
                        key: key.to_string(),
                        value,
                        probes: i + 1,
                        raw_hex: slot_hex_lines(&slot),
                    }));
                }
                Some(_) => continue,
            }
        }
        Ok(None)
    }

    pub async fn delete(&mut self, key: &str) -> Result<DeleteResult, KvError> {
        let start = hash_key(key.as_bytes());
        for i in 0..SLOT_COUNT {
            let idx = (start + i) % SLOT_COUNT;
            let slot = self.read_slot(idx).await?;
            match decode_slot(&slot) {
                None => {
                    return Ok(DeleteResult {
                        operation: "RDMA_WRITE",
                        slot: idx,
                        offset: format!("0x{:04X}", idx * SLOT_SIZE),
                        bytes: SLOT_SIZE,
                        key: key.to_string(),
                        probes: i + 1,
                        found: false,
                    });
                }
                Some((existing_key, _)) if existing_key == key => {
                    let zeroed = [0u8; SLOT_SIZE];
                    self.write_slot(idx, &zeroed).await?;
                    return Ok(DeleteResult {
                        operation: "RDMA_WRITE",
                        slot: idx,
                        offset: format!("0x{:04X}", idx * SLOT_SIZE),
                        bytes: SLOT_SIZE,
                        key: key.to_string(),
                        probes: i + 1,
                        found: true,
                    });
                }
                Some(_) => continue,
            }
        }
        Ok(DeleteResult {
            operation: "RDMA_READ",
            slot: start,
            offset: format!("0x{:04X}", start * SLOT_SIZE),
            bytes: SLOT_SIZE,
            key: key.to_string(),
            probes: SLOT_COUNT,
            found: false,
        })
    }

    pub async fn dump_slots(&mut self) -> Result<Vec<SlotInfo>, KvError> {
        let mut slots = Vec::with_capacity(SLOT_COUNT);
        for idx in 0..SLOT_COUNT {
            let slot = self.read_slot(idx).await?;
            let (occupied, key, value) = match decode_slot(&slot) {
                Some((k, v)) => (true, Some(k), Some(v)),
                None => (false, None, None),
            };
            slots.push(SlotInfo {
                index: idx,
                offset: format!("0x{:04X}", idx * SLOT_SIZE),
                occupied,
                key,
                value,
            });
        }
        Ok(slots)
    }
}
