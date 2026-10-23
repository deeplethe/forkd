//! Desired state, on disk, in sqlite.
//!
//! Two rules this module exists to enforce, both learned the hard way in
//! the v0.5 controller:
//!
//! 1. **The DB holds what we WANT, never what we observed.** Actual state
//!    is read back from the node every time. A controller that trusts its
//!    own cache kills sandboxes it merely forgot about.
//! 2. **A restart adopts, it never reconciles by killing.** Measured on bm:
//!    `kill -9` on the controller cost 0 of 20 sandboxes, and all 20 were
//!    re-adopted in 134 ms, because each sandbox lives in its own systemd
//!    scope rather than as our child.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tier::Tier;

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A sandbox as the master wants it to be.
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub id: String,
    pub node_id: String,
    pub pool: String,
    pub template: String,
    pub tier: Tier,
    /// 128-bit hex. Presented by callers to reach this sandbox.
    pub token: String,
    /// Volume path, the only thing that survives Detached.
    pub volume: Option<String>,
    pub port: Option<u16>,
    pub created_at: i64,
    pub last_active_at: i64,
}

/// A node that has registered and may hold sandboxes.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub pool: String,
    /// Compatibility fingerprint. Snapshots only move within one value.
    /// Cross-node restore failed on bm/bj/hk even with matching
    /// fingerprints, so this gates placement, not migration.
    pub fingerprint: String,
    pub max_sandboxes: u32,
    pub last_seen_at: i64,
    pub connected: bool,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating state dir {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening state db {}", path.display()))?;
        Self::from_conn(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // WAL so a reader never blocks the control loop; NORMAL because
        // losing the last few ms of desired state is survivable (we re-adopt
        // from the node anyway) and FULL costs an fsync per tier change.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS nodes (
                id             TEXT PRIMARY KEY,
                pool           TEXT NOT NULL DEFAULT 'default',
                fingerprint    TEXT NOT NULL,
                max_sandboxes  INTEGER NOT NULL DEFAULT 0,
                last_seen_at   INTEGER NOT NULL,
                connected      INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS sandboxes (
                id             TEXT PRIMARY KEY,
                node_id        TEXT NOT NULL,
                pool           TEXT NOT NULL DEFAULT 'default',
                template       TEXT NOT NULL,
                tier           TEXT NOT NULL,
                token          TEXT NOT NULL,
                volume         TEXT,
                port           INTEGER,
                created_at     INTEGER NOT NULL,
                last_active_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sandboxes_node ON sandboxes(node_id);
            CREATE INDEX IF NOT EXISTS idx_sandboxes_tier ON sandboxes(tier);
            CREATE INDEX IF NOT EXISTS idx_sandboxes_idle ON sandboxes(last_active_at);

            -- Short-lived bearer tokens a new node exchanges for a client
            -- certificate. Single use: consumed_at is set on redemption.
            CREATE TABLE IF NOT EXISTS node_tokens (
                token       TEXT PRIMARY KEY,
                pool        TEXT NOT NULL DEFAULT 'default',
                expires_at  INTEGER NOT NULL,
                consumed_at INTEGER,
                node_id     TEXT
            );

            CREATE TABLE IF NOT EXISTS templates (
                name       TEXT PRIMARY KEY,
                rootfs     TEXT NOT NULL,
                spec       TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    // ---- nodes ----------------------------------------------------------

    pub fn upsert_node(&self, n: &Node) -> Result<()> {
        self.conn.execute(
            "INSERT INTO nodes (id, pool, fingerprint, max_sandboxes, last_seen_at, connected)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                pool=excluded.pool,
                fingerprint=excluded.fingerprint,
                max_sandboxes=excluded.max_sandboxes,
                last_seen_at=excluded.last_seen_at,
                connected=excluded.connected",
            params![
                n.id,
                n.pool,
                n.fingerprint,
                n.max_sandboxes,
                n.last_seen_at,
                n.connected as i32
            ],
        )?;
        Ok(())
    }

    pub fn set_node_connected(&self, id: &str, connected: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET connected=?2, last_seen_at=?3 WHERE id=?1",
            params![id, connected as i32, now_secs()],
        )?;
        Ok(())
    }

    pub fn list_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pool, fingerprint, max_sandboxes, last_seen_at, connected
             FROM nodes ORDER BY pool, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Node {
                    id: r.get(0)?,
                    pool: r.get(1)?,
                    fingerprint: r.get(2)?,
                    max_sandboxes: r.get(3)?,
                    last_seen_at: r.get(4)?,
                    connected: r.get::<_, i32>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- sandboxes ------------------------------------------------------

    pub fn insert_sandbox(&self, s: &Sandbox) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sandboxes
                (id, node_id, pool, template, tier, token, volume, port, created_at, last_active_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                s.id,
                s.node_id,
                s.pool,
                s.template,
                s.tier.as_str(),
                s.token,
                s.volume,
                s.port,
                s.created_at,
                s.last_active_at
            ],
        )?;
        Ok(())
    }

    pub fn get_sandbox(&self, id: &str) -> Result<Option<Sandbox>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, node_id, pool, template, tier, token, volume, port,
                        created_at, last_active_at
                 FROM sandboxes WHERE id=?1",
                params![id],
                Self::row_to_sandbox,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_sandboxes(&self) -> Result<Vec<Sandbox>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, node_id, pool, template, tier, token, volume, port,
                    created_at, last_active_at
             FROM sandboxes ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_sandbox)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Sandboxes on one node, for re-adoption after a restart.
    pub fn sandboxes_on_node(&self, node_id: &str) -> Result<Vec<Sandbox>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, node_id, pool, template, tier, token, volume, port,
                    created_at, last_active_at
             FROM sandboxes WHERE node_id=?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![node_id], Self::row_to_sandbox)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_tier(&self, id: &str, tier: Tier) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET tier=?2 WHERE id=?1",
            params![id, tier.as_str()],
        )?;
        Ok(())
    }

    /// Called on every request that reaches a sandbox. Promotion to Hot is
    /// the caller's job; this only moves the idle clock.
    pub fn touch(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET last_active_at=?2 WHERE id=?1",
            params![id, now_secs()],
        )?;
        Ok(())
    }

    pub fn delete_sandbox(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sandboxes WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Sandboxes whose idle time has earned them a colder tier.
    /// Returns (sandbox, idle_secs) so the caller can pick the target.
    pub fn idle_candidates(&self, older_than_secs: i64) -> Result<Vec<(Sandbox, i64)>> {
        let cutoff = now_secs() - older_than_secs;
        let mut stmt = self.conn.prepare(
            "SELECT id, node_id, pool, template, tier, token, volume, port,
                    created_at, last_active_at
             FROM sandboxes
             WHERE last_active_at <= ?1 AND tier != 'detached'
             ORDER BY last_active_at",
        )?;
        let now = now_secs();
        let rows = stmt
            .query_map(params![cutoff], Self::row_to_sandbox)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|s| {
                let idle = now - s.last_active_at;
                (s, idle)
            })
            .collect())
    }

    fn row_to_sandbox(r: &rusqlite::Row<'_>) -> rusqlite::Result<Sandbox> {
        let tier: String = r.get(4)?;
        Ok(Sandbox {
            id: r.get(0)?,
            node_id: r.get(1)?,
            pool: r.get(2)?,
            template: r.get(3)?,
            tier: tier.parse().unwrap_or(Tier::Hot),
            token: r.get(5)?,
            volume: r.get(6)?,
            port: r.get::<_, Option<u16>>(7)?,
            created_at: r.get(8)?,
            last_active_at: r.get(9)?,
        })
    }

    // ---- node registration tokens ---------------------------------------

    pub fn create_node_token(&self, token: &str, pool: &str, ttl_secs: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO node_tokens (token, pool, expires_at) VALUES (?1, ?2, ?3)",
            params![token, pool, now_secs() + ttl_secs],
        )?;
        Ok(())
    }

    /// Single-use redemption: returns the pool the token grants, and marks
    /// it consumed in the same transaction so two nodes cannot share one.
    pub fn redeem_node_token(&self, token: &str, node_id: &str) -> Result<Option<String>> {
        let now = now_secs();
        let pool: Option<String> = self
            .conn
            .query_row(
                "SELECT pool FROM node_tokens
                 WHERE token=?1 AND consumed_at IS NULL AND expires_at > ?2",
                params![token, now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(pool) = pool else { return Ok(None) };
        let changed = self.conn.execute(
            "UPDATE node_tokens SET consumed_at=?2, node_id=?3
             WHERE token=?1 AND consumed_at IS NULL",
            params![token, now, node_id],
        )?;
        // Lost the race to another redemption of the same token.
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(pool))
    }

    // ---- templates ------------------------------------------------------

    pub fn put_template(&self, name: &str, rootfs: &str, spec: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO templates (name, rootfs, spec, created_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(name) DO UPDATE SET rootfs=excluded.rootfs, spec=excluded.spec",
            params![name, rootfs, spec, now_secs()],
        )?;
        Ok(())
    }

    pub fn get_template(&self, name: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT rootfs, spec FROM templates WHERE name=?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn list_templates(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM templates ORDER BY name")?;
        let names = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb(id: &str, node: &str) -> Sandbox {
        let t = now_secs();
        Sandbox {
            id: id.into(),
            node_id: node.into(),
            pool: "default".into(),
            template: "python".into(),
            tier: Tier::Hot,
            token: "deadbeef".into(),
            volume: None,
            port: Some(19800),
            created_at: t,
            last_active_at: t,
        }
    }

    #[test]
    fn sandbox_round_trips() {
        let s = Store::open_in_memory().unwrap();
        s.insert_sandbox(&sb("sb1", "node-a")).unwrap();
        let got = s.get_sandbox("sb1").unwrap().unwrap();
        assert_eq!(got.id, "sb1");
        assert_eq!(got.tier, Tier::Hot);
        assert_eq!(got.port, Some(19800));
    }

    #[test]
    fn tier_changes_persist() {
        let s = Store::open_in_memory().unwrap();
        s.insert_sandbox(&sb("sb1", "node-a")).unwrap();
        s.set_tier("sb1", Tier::Snapshot).unwrap();
        assert_eq!(s.get_sandbox("sb1").unwrap().unwrap().tier, Tier::Snapshot);
    }

    #[test]
    fn re_adoption_lists_only_this_nodes_sandboxes() {
        let s = Store::open_in_memory().unwrap();
        s.insert_sandbox(&sb("sb1", "node-a")).unwrap();
        s.insert_sandbox(&sb("sb2", "node-a")).unwrap();
        s.insert_sandbox(&sb("sb3", "node-b")).unwrap();
        assert_eq!(s.sandboxes_on_node("node-a").unwrap().len(), 2);
        assert_eq!(s.sandboxes_on_node("node-b").unwrap().len(), 1);
    }

    /// A registration token is worth exactly one node.
    #[test]
    fn node_token_is_single_use() {
        let s = Store::open_in_memory().unwrap();
        s.create_node_token("tok1", "beijing", 900).unwrap();
        assert_eq!(
            s.redeem_node_token("tok1", "node-a").unwrap().as_deref(),
            Some("beijing")
        );
        assert_eq!(s.redeem_node_token("tok1", "node-b").unwrap(), None);
    }

    #[test]
    fn expired_node_token_is_refused() {
        let s = Store::open_in_memory().unwrap();
        s.create_node_token("old", "default", -1).unwrap();
        assert_eq!(s.redeem_node_token("old", "node-a").unwrap(), None);
    }

    #[test]
    fn idle_candidates_skip_detached() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sb("sb1", "node-a");
        a.last_active_at = now_secs() - 600;
        let mut b = sb("sb2", "node-a");
        b.last_active_at = now_secs() - 600;
        b.tier = Tier::Detached;
        s.insert_sandbox(&a).unwrap();
        s.insert_sandbox(&b).unwrap();
        let got = s.idle_candidates(60).unwrap();
        assert_eq!(got.len(), 1, "detached has nothing left to demote");
        assert_eq!(got[0].0.id, "sb1");
        assert!(got[0].1 >= 600);
    }

    #[test]
    fn touch_moves_the_idle_clock() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sb("sb1", "node-a");
        a.last_active_at = now_secs() - 600;
        s.insert_sandbox(&a).unwrap();
        assert_eq!(s.idle_candidates(60).unwrap().len(), 1);
        s.touch("sb1").unwrap();
        assert_eq!(s.idle_candidates(60).unwrap().len(), 0);
    }
}
