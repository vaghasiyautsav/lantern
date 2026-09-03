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
    /// The message this one answers, if any.
    pub reply_to: Option<Uuid>,
}

fn row_to_message(row: &rusqlite::Row) -> rusqlite::Result<StoredMessage> {
    let mid_bytes: Vec<u8> = row.get(0)?;
    let pid: Vec<u8> = row.get(1)?;
    let reply_bytes: Option<Vec<u8>> = row.get(6)?;
    Ok(StoredMessage {
        mid: Uuid::from_slice(&mid_bytes).unwrap_or_default(),
        peer_id: pid.try_into().unwrap_or([0; 32]),
        outgoing: row.get::<_, i64>(2)? != 0,
        ts: row.get::<_, i64>(3)? as u64,
        text: row.get(4)?,
        state: row.get::<_, i64>(5)? as u8,
        reply_to: reply_bytes.and_then(|b| Uuid::from_slice(&b).ok()),
    })
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
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Additive migrations for databases created by earlier builds. Each
    /// step must be safe to run on a fresh database too, so `init` stays a
    /// single path rather than branching on version.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        let has_reply_to = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'reply_to'")?
            .exists([])?;
        if !has_reply_to {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN reply_to BLOB")?;
        }
        let has_read = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'read'")?
            .exists([])?;
        if !has_read {
            // Everything already on disk was seen in whatever UI stored it;
            // only messages that arrive after this build count as unread.
            conn.execute_batch("ALTER TABLE messages ADD COLUMN read INTEGER NOT NULL DEFAULT 1")?;
        }
        // Full-text index over message bodies, kept in step by triggers so
        // every write path — including deletes, which matter because FTS
        // keeps its own copy of the text and secure_delete on the base table
        // alone would leave deleted messages greppable in the index.
        let has_fts = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'")?
            .exists([])?;
        conn.execute_batch(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
                USING fts5(body, content='messages', content_rowid='msg_rowid');
            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, body) VALUES (new.msg_rowid, new.body);
            END;
            CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, body)
                    VALUES ('delete', old.msg_rowid, old.body);
            END;
            "#,
        )?;
        if !has_fts {
            // First time this database has the index: history predates the
            // triggers, so index it now. This has to be FTS5's own rebuild
            // command — an external-content table answers a plain SELECT
            // from the content table, not from its index, so "insert the
            // rows not already in messages_fts" sees every row as present
            // and indexes nothing.
            conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES ('rebuild')")?;
        }
        Ok(())
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

    /// Everything from this peer is now seen. Returns how many flipped.
    pub fn mark_read(&self, peer: &[u8; 32]) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE messages SET read = 1 WHERE peer_id = ?1 AND read = 0",
            params![peer.as_slice()],
        )?)
    }

    /// Unread incoming messages per peer — what a roster shows as badges,
    /// surviving a restart because it is derived from the store, not RAM.
    pub fn unread_counts(&self) -> Result<Vec<([u8; 32], u32)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT peer_id, COUNT(*) FROM messages
             WHERE read = 0 AND outgoing = 0 GROUP BY peer_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(id, n)| id.try_into().ok().map(|id| (id, n)))
            .collect())
    }

    /// Full-text search over every conversation, newest first. The input is
    /// wrapped as one quoted FTS5 phrase with a prefix star, so typing is
    /// matched literally as it grows — and hostile input cannot reach FTS5's
    /// query syntax, whose errors would otherwise surface for a stray quote.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<StoredMessage>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT m.mid, m.peer_id, m.outgoing, m.ts, m.body, m.state, m.reply_to
            FROM messages_fts f JOIN messages m ON m.msg_rowid = f.rowid
            WHERE messages_fts MATCH '"' || replace(?1, '"', '""') || '"*'
            ORDER BY m.ts DESC LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![query, limit as i64], row_to_message)?;
        Ok(rows.filter_map(Result::ok).collect())
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
            INSERT OR IGNORE INTO messages
                (mid, peer_id, outgoing, ts, body, state, reply_to, read)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?3)
            "#,
            params![
                m.mid.as_bytes().as_slice(),
                m.peer_id.as_slice(),
                m.outgoing as i64,
                m.ts,
                m.text,
                m.state as i64,
                m.reply_to.map(|r| r.as_bytes().to_vec())
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
            SELECT mid, peer_id, outgoing, ts, body, state, reply_to
            FROM messages WHERE peer_id = ?1
            ORDER BY ts DESC, msg_rowid DESC LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![peer_id.as_slice(), limit as i64], row_to_message)?;
        let mut out: Vec<StoredMessage> = rows.filter_map(Result::ok).collect();
        out.reverse(); // oldest first
        Ok(out)
    }

    /// Delete one message from the local log. Returns true if a row went.
    ///
    /// Local only, and deliberately so: Wisp has no "delete for everyone"
    /// frame, and the copy on the other machine is theirs to keep. Callers
    /// must say so in their wording rather than implying a remote wipe.
    ///
    /// `secure_delete` is ON (see `init`), so the freed pages are zeroed
    /// rather than merely unlinked, and `checkpoint` clears the log copy —
    /// between them the words are gone from disk, not just from the index.
    pub fn delete_message(&self, mid: &Uuid) -> Result<bool, StoreError> {
        let gone = self.conn.execute(
            "DELETE FROM messages WHERE mid = ?1",
            params![mid.as_bytes().as_slice()],
        )?;
        // A reply whose original is gone should read as an ordinary message,
        // not quote a hole.
        self.conn.execute(
            "UPDATE messages SET reply_to = NULL WHERE reply_to = ?1",
            params![mid.as_bytes().as_slice()],
        )?;
        self.checkpoint();
        Ok(gone > 0)
    }

    /// Fold the write-ahead log back into the database and truncate it.
    ///
    /// Deleted text otherwise outlives the delete: `secure_delete` zeroes the
    /// freed *page*, but the WAL still holds the frame containing that page
    /// as it was before, and `-wal` files live for as long as the process
    /// does. Verified by grepping the files after a delete — without this the
    /// message bodies are still readable in `lantern.db-wal`.
    ///
    /// Best-effort: a checkpoint that can't run (another reader mid-query)
    /// only means the words linger a little longer, which must not turn into
    /// a failed delete.
    fn checkpoint(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    /// Delete the whole conversation with one peer. Returns rows removed.
    ///
    /// No `reply_to` fixup is needed: replies only ever reference messages
    /// in the same conversation, so every reference dies with its target.
    ///
    /// The peer row itself stays: it carries the pinned key and the verified
    /// flag, and dropping those would silently downgrade a verified contact
    /// back to first-contact trust — the next connection would be accepted as
    /// new rather than checked against what we already pinned. You are
    /// clearing what was said, not un-knowing the person.
    pub fn clear_history(&self, peer_id: &[u8; 32]) -> Result<usize, StoreError> {
        let gone = self.conn.execute(
            "DELETE FROM messages WHERE peer_id = ?1",
            params![peer_id.as_slice()],
        )?;
        self.checkpoint();
        Ok(gone)
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
    fn clearing_a_conversation_keeps_the_peer_and_its_trust() {
        let store = Store::open_in_memory().unwrap();
        let peer = [7u8; 32];
        let other = [8u8; 32];
        store.record_peer(&peer, "Mira", "mira-mbp", 1000).unwrap();
        store.record_peer(&other, "Ravi", "ravi-box", 1000).unwrap();
        store.set_verified(&peer, true).unwrap();

        for (who, text) in [(peer, "one"), (peer, "two"), (other, "theirs")] {
            store
                .insert_message(&StoredMessage {
                    mid: Uuid::new_v4(),
                    peer_id: who,
                    outgoing: false,
                    ts: 2000,
                    text: text.into(),
                    state: 0,
                    reply_to: None,
                })
                .unwrap();
        }

        assert_eq!(store.clear_history(&peer).unwrap(), 2);
        assert!(store.history(&peer, 10).unwrap().is_empty());
        // Only that conversation.
        assert_eq!(store.history(&other, 10).unwrap().len(), 1);
        // And the peer survives with its trust intact — dropping the row
        // would downgrade a verified contact back to first-contact trust.
        assert!(store.is_verified(&peer).unwrap());
        // Clearing an already-empty conversation is a no-op, not a failure.
        assert_eq!(store.clear_history(&peer).unwrap(), 0);
    }

        /// Their `msg` is outgoing; unread only counts what arrives.
    fn incoming(peer: [u8; 32], ts: u64, text: &str) -> StoredMessage {
        StoredMessage { outgoing: false, ..msg(peer, ts, text) }
    }

    #[test]
    fn unread_counts_survive_via_store_and_clear_on_mark_read() {
        let store = Store::open_in_memory().unwrap();
        let a = [1u8; 32];
        let b = [2u8; 32];
        store.record_peer(&a, "A", "h", 1).unwrap();
        store.record_peer(&b, "B", "h", 1).unwrap();
        store.insert_message(&incoming(a, 2000, "one")).unwrap();
        store.insert_message(&incoming(a, 2001, "two")).unwrap();
        store.insert_message(&msg(a, 2002, "mine — never unread")).unwrap();
        store.insert_message(&incoming(b, 2003, "theirs")).unwrap();

        let mut counts = store.unread_counts().unwrap();
        counts.sort();
        assert_eq!(counts, vec![(a, 2), (b, 1)]);

        assert_eq!(store.mark_read(&a).unwrap(), 2);
        assert_eq!(store.unread_counts().unwrap(), vec![(b, 1)]);
    }

    #[test]
    fn search_finds_prefixes_and_forgets_deleted_text() {
        let store = Store::open_in_memory().unwrap();
        let peer = [3u8; 32];
        store.record_peer(&peer, "P", "h", 1).unwrap();
        store.insert_message(&incoming(peer, 2000, "the quarterly report is late")).unwrap();
        let doomed = msg(peer, 2001, "secret rendezvous location");
        store.insert_message(&doomed).unwrap();

        // Prefix while typing, and hostile input is a term, not syntax.
        assert_eq!(store.search("quart", 10).unwrap().len(), 1);
        assert_eq!(store.search("report\" OR \"", 10).unwrap().len(), 0);

        // A deleted message must leave the index too — FTS keeps its own
        // copy of the text, so without the delete trigger it would still be
        // findable after secure_delete wiped the base row.
        store.delete_message(&doomed.mid).unwrap();
        assert!(store.search("rendezvous", 10).unwrap().is_empty());
    }

    /// A database that predates the search index: everything already in it
    /// must be searchable after the upgrade, not just what arrives later.
    #[test]
    fn upgrade_indexes_existing_history() {
        let dir = std::env::temp_dir().join(format!("lantern-fts-upgrade-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        let peer = [4u8; 32];

        // Old build: rows written before the index and its triggers existed.
        {
            let store = Store::open(&path).unwrap();
            store.record_peer(&peer, "P", "h", 1).unwrap();
            store.insert_message(&incoming(peer, 2000, "the quarterly report is late")).unwrap();
            store.insert_message(&msg(peer, 2001, "invoice attached")).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TRIGGER messages_ai; DROP TRIGGER messages_ad; DROP TABLE messages_fts;",
                )
                .unwrap();
        }

        // New build opens it and migrates.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.search("quart", 10).unwrap().len(), 1);
        assert_eq!(store.search("invoice", 10).unwrap().len(), 1);
        // And the triggers are live again for anything new.
        store.insert_message(&incoming(peer, 2002, "fresh after upgrade")).unwrap();
        assert_eq!(store.search("fresh", 10).unwrap().len(), 1);

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

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
            reply_to: None,
        };
        store.insert_message(&m).unwrap();
        store.set_message_state(&m.mid, 1).unwrap();

        let hist = store.history(&peer, 10).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].text, "hello");
        assert_eq!(hist[0].state, 1);
        assert_eq!(hist[0].reply_to, None);

        // State never regresses (delivered after read stays read).
        store.set_message_state(&m.mid, 2).unwrap();
        store.set_message_state(&m.mid, 1).unwrap();
        assert_eq!(store.history(&peer, 10).unwrap()[0].state, 2);
    }

    #[test]
    fn reply_reference_survives_the_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let peer = [7u8; 32];
        store.record_peer(&peer, "Mira", "mira-mbp", 1000).unwrap();

        let first = StoredMessage {
            mid: Uuid::new_v4(),
            peer_id: peer,
            outgoing: false,
            ts: 2000,
            text: "did the build finish?".into(),
            state: 1,
            reply_to: None,
        };
        let answer = StoredMessage {
            mid: Uuid::new_v4(),
            peer_id: peer,
            outgoing: true,
            ts: 2001,
            text: "yes — clean".into(),
            state: 0,
            reply_to: Some(first.mid),
        };
        store.insert_message(&first).unwrap();
        store.insert_message(&answer).unwrap();

        let hist = store.history(&peer, 10).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].reply_to, None);
        assert_eq!(hist[1].reply_to, Some(first.mid));
    }

    /// A helper so the deletion tests read as what they're testing.
    fn msg(peer: [u8; 32], ts: u64, text: &str) -> StoredMessage {
        StoredMessage {
            mid: Uuid::new_v4(),
            peer_id: peer,
            outgoing: true,
            ts,
            text: text.into(),
            state: 1,
            reply_to: None,
        }
    }

    #[test]
    fn deleting_one_message_leaves_the_rest_and_unquotes_its_replies() {
        let store = Store::open_in_memory().unwrap();
        let peer = [3u8; 32];
        let first = msg(peer, 1000, "ignore this");
        let keep = msg(peer, 1001, "keep me");
        let mut answer = msg(peer, 1002, "answering the first");
        answer.reply_to = Some(first.mid);
        for m in [&first, &keep, &answer] {
            store.insert_message(m).unwrap();
        }

        assert!(store.delete_message(&first.mid).unwrap());
        // Deleting twice is not an error — it just reports nothing went.
        assert!(!store.delete_message(&first.mid).unwrap());

        let hist = store.history(&peer, 10).unwrap();
        assert_eq!(hist.len(), 2);
        assert!(hist.iter().all(|m| m.text != "ignore this"));
        // The reply survives; it simply no longer points at a missing message.
        let surviving_answer = hist.iter().find(|m| m.mid == answer.mid).unwrap();
        assert_eq!(surviving_answer.reply_to, None);
    }

}
