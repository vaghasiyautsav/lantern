//! SQLite persistence: peers and the message log.
//!
//! DESIGN.md §7. Prototype carries the tables Phase 1 needs — peers,
//! messages (with the FTS5-correct msg_rowid INTEGER PRIMARY KEY), kv.
//! Transfers/chunks/queue arrive with Phase 3.

use std::path::Path;

use rusqlite::{params, Connection};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub mid: Uuid,
    pub peer_id: [u8; 32],
    pub outgoing: bool,
    pub ts: u64,
    pub text: String,
    /// 0 sending · 1 delivered · 2 read
    pub state: u8,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS peers (
                id          BLOB PRIMARY KEY,
                name        TEXT NOT NULL,
                host        TEXT NOT NULL DEFAULT '',
                group_tag   TEXT NOT NULL DEFAULT '',
                first_seen  INTEGER NOT NULL,
                last_seen   INTEGER NOT NULL,
                verified    INTEGER NOT NULL DEFAULT 0,
                pinned_key  BLOB
            );
            CREATE TABLE IF NOT EXISTS messages (
                msg_rowid   INTEGER PRIMARY KEY AUTOINCREMENT,
                mid         BLOB UNIQUE NOT NULL,
                peer_id     BLOB NOT NULL,
                outgoing    INTEGER NOT NULL,
                ts          INTEGER NOT NULL,
                body        TEXT NOT NULL,
                state       INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_messages_peer ON messages(peer_id, ts);
            CREATE TABLE IF NOT EXISTS kv (
                k TEXT PRIMARY KEY,
                v BLOB
            );
            "#,
        )?;
        Ok(Self { conn })
    }

    // -- peers ------------------------------------------------------------

    /// Upsert a peer sighting. Returns the previously pinned key if it
    /// differs from `id`'s current key material context (TOFU hook).
    pub fn record_peer(
        &self,
        id: &[u8; 32],
        name: &str,
        host: &str,
        now: u64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO peers (id, name, host, first_seen, last_seen, pinned_key)
            VALUES (?1, ?2, ?3, ?4, ?4, ?1)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                host = excluded.host,
                last_seen = excluded.last_seen
            "#,
            params![id.as_slice(), name, host, now],
        )?;
        Ok(())
    }

    /// Identities previously seen using this name+host, other than `id`.
    /// A non-empty result is the TOFU red flag: someone (or a reinstall)
    /// is presenting a known face with a new key.
    pub fn conflicting_identities(
        &self,
        name: &str,
        host: &str,
        id: &[u8; 32],
    ) -> Result<Vec<[u8; 32]>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM peers WHERE name = ?1 AND host = ?2 AND id <> ?3",
        )?;
        let rows = stmt.query_map(params![name, host, id.as_slice()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|v| v.try_into().ok())
            .collect())
    }

    pub fn set_verified(&self, id: &[u8; 32], verified: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE peers SET verified = ?2 WHERE id = ?1",
            params![id.as_slice(), verified as i64],
        )?;
        Ok(())
    }

    pub fn is_verified(&self, id: &[u8; 32]) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT verified FROM peers WHERE id = ?1",
                params![id.as_slice()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            != 0)
    }

    // -- messages ---------------------------------------------------------

    pub fn insert_message(&self, m: &StoredMessage) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT OR IGNORE INTO messages (mid, peer_id, outgoing, ts, body, state)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                m.mid.as_bytes().as_slice(),
                m.peer_id.as_slice(),
                m.outgoing as i64,
                m.ts,
                m.text,
                m.state as i64
            ],
        )?;
        Ok(())
    }

    pub fn set_message_state(&self, mid: &Uuid, state: u8) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE messages SET state = MAX(state, ?2) WHERE mid = ?1",
            params![mid.as_bytes().as_slice(), state as i64],
        )?;
        Ok(())
    }

    pub fn history(
        &self,
        peer_id: &[u8; 32],
        limit: usize,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT mid, peer_id, outgoing, ts, body, state
            FROM messages WHERE peer_id = ?1
            ORDER BY ts DESC, msg_rowid DESC LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![peer_id.as_slice(), limit as i64], |row| {
            let mid_bytes: Vec<u8> = row.get(0)?;
            let pid: Vec<u8> = row.get(1)?;
            Ok(StoredMessage {
                mid: Uuid::from_slice(&mid_bytes).unwrap_or_default(),
                peer_id: pid.try_into().unwrap_or([0; 32]),
                outgoing: row.get::<_, i64>(2)? != 0,
                ts: row.get::<_, i64>(3)? as u64,
                text: row.get(4)?,
                state: row.get::<_, i64>(5)? as u8,
            })
        })?;
        let mut out: Vec<StoredMessage> = rows.filter_map(Result::ok).collect();
        out.reverse(); // oldest first
        Ok(out)
    }

    pub fn message_count(&self) -> Result<u64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?
            as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_log_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let peer = [9u8; 32];
        store.record_peer(&peer, "Mira", "mira-mbp", 1000).unwrap();

        let m = StoredMessage {
            mid: Uuid::new_v4(),
            peer_id: peer,
            outgoing: true,
            ts: 2000,
            text: "hello".into(),
            state: 0,
        };
        store.insert_message(&m).unwrap();
        store.set_message_state(&m.mid, 1).unwrap();

        let hist = store.history(&peer, 10).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].text, "hello");
        assert_eq!(hist[0].state, 1);

        // State never regresses (delivered after read stays read).
        store.set_message_state(&m.mid, 2).unwrap();
        store.set_message_state(&m.mid, 1).unwrap();
        assert_eq!(store.history(&peer, 10).unwrap()[0].state, 2);
    }
}
