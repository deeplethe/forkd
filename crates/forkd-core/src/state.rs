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
    /// Host port the node's gateway listens on. `port` is inside the
    /// namespace and may repeat across sandboxes; this one is unique on the
    /// node because the master allocates it from the node's range.
    pub gateway_port: Option<u16>,
    pub created_at: i64,
    pub last_active_at: i64,
    /// Why the node could not bring it up, if it could not.
    pub last_error: Option<String>,
    pub error_attempts: u32,
    /// Slot in the node's address plan. Fixes which /30 the sandbox gets,
    /// and so its IP.
    pub net_index: Option<u32>,
    /// The address the node reported for it, which is how anything reaches
    /// it now that ports are per-namespace.
    pub ip: Option<String>,
    /// The tag it was forked from, if it was. A cold start has none.
    pub from_tag: Option<String>,
    /// Per-sandbox memory ceiling, overriding the template's. The caller's
    /// value, which matters more here than in an ordinary deployment: the
    /// denser the node, the more one unbounded sandbox costs everyone on
    /// it.
    pub memory_limit_mib: Option<u64>,
}

/// A named checkpoint, forkable into any number of sandboxes.
#[derive(Debug, Clone)]
pub struct SnapshotTag {
    pub tag: String,
    /// Which node holds the image. Tags are node-local for now, so a fork
    /// from one is placed on that node.
    pub node_id: String,
    pub template: String,
    /// The sandbox it was branched from, when it came from one.
    pub from_sandbox: Option<String>,
    pub created_at: i64,
    pub size_bytes: u64,
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
    /// What the node last reported about its own health. Admission uses
    /// these; without them the master will happily fill a node until it
    /// stops responding, which is exactly what it did on bm at 139
    /// sandboxes.
    pub free_mib: u64,
    pub psi_some_avg10: f64,
}

/// Who a presented client certificate belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertOwner {
    Node(String),
    /// Known, but revoked. Distinguished from `Unknown` so the refusal can
    /// name the reason instead of looking like an unregistered machine.
    Revoked(String),
    Unknown,
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
                connected      INTEGER NOT NULL DEFAULT 0,
                free_mib       INTEGER NOT NULL DEFAULT 0,
                psi_some_avg10 REAL NOT NULL DEFAULT 0
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
                last_active_at INTEGER NOT NULL,
                last_error     TEXT,
                error_attempts INTEGER NOT NULL DEFAULT 0,
                net_index      INTEGER,
                ip             TEXT,
                from_tag       TEXT,
                memory_limit_mib INTEGER
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

            -- A named checkpoint a sandbox can be forked from. Measured on
            -- bm: one 14 MiB image restored into 5 concurrent sandboxes in
            -- 226 ms, all inheriting the parent's memory, and the image
            -- stayed usable afterwards. So a tag is one image, many
            -- children, not a copy per child.
            CREATE TABLE IF NOT EXISTS snapshots (
                tag         TEXT PRIMARY KEY,
                node_id     TEXT NOT NULL,
                template    TEXT NOT NULL,
                from_sandbox TEXT,
                created_at  INTEGER NOT NULL,
                size_bytes  INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS templates (
                name       TEXT PRIMARY KEY,
                rootfs     TEXT NOT NULL,
                spec       TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            "#,
        )?;
        // Older DBs predate these columns. sqlite has no IF NOT EXISTS for
        // ADD COLUMN, and a duplicate is the normal case here, so the error
        // is the check.
        for ddl in [
            "ALTER TABLE nodes ADD COLUMN free_mib INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE nodes ADD COLUMN psi_some_avg10 REAL NOT NULL DEFAULT 0",
            "ALTER TABLE sandboxes ADD COLUMN last_error TEXT",
            "ALTER TABLE sandboxes ADD COLUMN error_attempts INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE sandboxes ADD COLUMN net_index INTEGER",
            // The node's CLIENT CERTIFICATE, not `fingerprint`, which is the
            // host fingerprint a pool is keyed by. This is how a connection
            // is tied to a node: the master computes the SHA-256 of the
            // certificate the peer presented and looks it up here, rather
            // than believing the node id in the message.
            "ALTER TABLE nodes ADD COLUMN cert_sha256 TEXT",
            "ALTER TABLE nodes ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0",
            // The host fingerprint a tag was baked under. A runsc checkpoint
            // refuses to restore across even a one-week release difference,
            // so a tag is only usable on a host that matches. Recorded here
            // so placement can refuse at schedule time instead of letting
            // the restore fail on the node.
            "ALTER TABLE snapshots ADD COLUMN fingerprint TEXT",
            // The HOST port this sandbox's gateway listens on. Distinct from
            // `port`, which is the port inside the namespace and may be 8080
            // for every sandbox on the node.
            "ALTER TABLE sandboxes ADD COLUMN gateway_port INTEGER",
            "ALTER TABLE sandboxes ADD COLUMN ip TEXT",
            "ALTER TABLE sandboxes ADD COLUMN from_tag TEXT",
            "ALTER TABLE sandboxes ADD COLUMN memory_limit_mib INTEGER",
        ] {
            match self.conn.execute(ddl, []) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Record why a sandbox is not coming up, so it shows in the API.
    pub fn set_sandbox_error(&self, id: &str, error: &str, attempts: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET last_error=?2, error_attempts=?3 WHERE id=?1",
            params![id, error, attempts],
        )?;
        Ok(())
    }

    pub fn clear_sandbox_error(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET last_error=NULL, error_attempts=0 WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    // ---- nodes ----------------------------------------------------------

    pub fn upsert_node(&self, n: &Node) -> Result<()> {
        self.conn.execute(
            "INSERT INTO nodes (id, pool, fingerprint, max_sandboxes, last_seen_at,
                                connected, free_mib, psi_some_avg10)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)
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

    /// Called once when the master starts. Connectivity is a live fact that
    /// only a poll can establish, so a restart must not inherit the last
    /// run's view -- otherwise the panel shows a node as connected when
    /// nothing has called in since the process came up.
    pub fn mark_all_nodes_disconnected(&self) -> Result<usize> {
        Ok(self
            .conn
            .execute("UPDATE nodes SET connected=0 WHERE connected=1", [])?)
    }

    pub fn set_node_connected(&self, id: &str, connected: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET connected=?2, last_seen_at=?3 WHERE id=?1",
            params![id, connected as i32, now_secs()],
        )?;
        Ok(())
    }

    /// Record what a node reports about itself on each poll.
    /// The host fingerprints already present in a pool.
    ///
    /// A pool is a migration domain: every node in it must be able to
    /// restore every other's checkpoints. The fingerprint is computed by
    /// the node, never configured, so this is how the master notices that a
    /// machine does not belong with the others.
    pub fn pool_fingerprints(&self, pool: &str) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT fingerprint FROM nodes WHERE pool=?1 AND revoked=0")?;
        let rows = st.query_map(params![pool], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The fingerprint a tag was baked under, if it is known.
    ///
    /// `None` for a tag written before this column existed -- those cannot
    /// be checked, and refusing them would break existing deployments on
    /// upgrade, so they are allowed through with the old behaviour.
    pub fn snapshot_fingerprint(&self, tag: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT fingerprint FROM snapshots WHERE tag=?1",
                params![tag],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn set_snapshot_fingerprint(&self, tag: &str, fp: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE snapshots SET fingerprint=?2 WHERE tag=?1",
            params![tag, fp],
        )?;
        Ok(())
    }

    /// The host fingerprint a node reported at registration.
    pub fn node_fingerprint(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT fingerprint FROM nodes WHERE id=?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Record the certificate a node will present from now on.
    ///
    /// Called when the master issues it, so the master never has to trust a
    /// node id that arrives in a request body: it resolves the connection to
    /// a node through this column instead.
    pub fn set_node_cert(&self, id: &str, cert_sha256: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET cert_sha256=?2, revoked=0 WHERE id=?1",
            params![id, cert_sha256],
        )?;
        Ok(())
    }

    /// Which node, if any, presented this certificate -- and whether it is
    /// still allowed to. A revoked certificate resolves to `Revoked` rather
    /// than to nothing, so the refusal can say why.
    pub fn node_by_cert(&self, cert_sha256: &str) -> Result<CertOwner> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT id, revoked FROM nodes WHERE cert_sha256=?1",
                params![cert_sha256],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            None => CertOwner::Unknown,
            Some((id, 0)) => CertOwner::Node(id),
            Some((id, _)) => CertOwner::Revoked(id),
        })
    }

    /// Revoke a node's certificate. The node keeps its files; what changes
    /// is that the master stops accepting them, which is what matters since
    /// the node is the one that dials.
    pub fn revoke_node(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE nodes SET revoked=1, connected=0 WHERE id=?1",
            params![id],
        )?;
        Ok(n > 0)
    }

    pub fn set_node_health(&self, id: &str, free_mib: u64, psi: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET connected=1, last_seen_at=?2, free_mib=?3, psi_some_avg10=?4
             WHERE id=?1",
            params![id, now_secs(), free_mib as i64, psi],
        )?;
        Ok(())
    }

    pub fn list_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pool, fingerprint, max_sandboxes, last_seen_at, connected,
                    free_mib, psi_some_avg10
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
                    free_mib: r.get::<_, i64>(6)? as u64,
                    psi_some_avg10: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- sandboxes ------------------------------------------------------

    pub fn insert_sandbox(&self, s: &Sandbox) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sandboxes
                (id, node_id, pool, template, tier, token, volume, port,
                 created_at, last_active_at, net_index, from_tag,
                 memory_limit_mib, gateway_port)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
                s.last_active_at,
                s.net_index,
                s.from_tag,
                s.memory_limit_mib.map(|v| v as i64),
                s.gateway_port,
            ],
        )?;
        Ok(())
    }

    pub fn get_sandbox(&self, id: &str) -> Result<Option<Sandbox>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, node_id, pool, template, tier, token, volume, port,
                        created_at, last_active_at, last_error, error_attempts,
                    net_index, ip, from_tag, memory_limit_mib, gateway_port
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
                    created_at, last_active_at, last_error, error_attempts,
                    net_index, ip, from_tag, memory_limit_mib, gateway_port
             FROM sandboxes ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_sandbox)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Sandboxes on one node, for re-adoption after a restart.
    /// How many sandboxes this node is carrying.
    ///
    /// Separate from `sandboxes_on_node` because the scheduler only ever
    /// wanted the number: building a `Sandbox` per row to call `.len()` is
    /// 17 columns deserialised for every candidate node on every create,
    /// which at a few thousand sandboxes is the create path's whole cost.
    pub fn count_sandboxes_on_node(&self, node_id: &str) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sandboxes WHERE node_id=?1",
            params![node_id],
            |r| r.get(0),
        )?)
    }

    /// Sandbox counts keyed by tier name.
    ///
    /// One grouped query rather than four, and over the `tier` index, so a
    /// metrics scrape on a timer stays cheap as the fleet grows. Tiers with
    /// nothing in them are absent; the caller fills them in at zero, since
    /// a missing series and an empty one read differently on a dashboard.
    pub fn count_sandboxes_by_tier(&self) -> Result<std::collections::HashMap<String, u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tier, COUNT(*) FROM sandboxes GROUP BY tier")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sandboxes carrying a `last_error`, i.e. ones the node could not
    /// start and has given up on until someone retries them.
    pub fn count_failed_sandboxes(&self) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sandboxes WHERE last_error IS NOT NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// How many snapshots exist and what they add up to on disk.
    pub fn snapshot_totals(&self) -> Result<(u32, u64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM snapshots",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn sandboxes_on_node(&self, node_id: &str) -> Result<Vec<Sandbox>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, node_id, pool, template, tier, token, volume, port,
                    created_at, last_active_at, last_error, error_attempts,
                    net_index, ip, from_tag, memory_limit_mib, gateway_port
             FROM sandboxes WHERE node_id=?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![node_id], Self::row_to_sandbox)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Lowest free value of `column` in `[lo, hi]` on one node.
    ///
    /// These are node-local resources: a network slot belongs to one node's
    /// address plan, so two nodes may hand out the same one. Allocating per
    /// node rather than globally is what lets a fleet exceed one range.
    ///
    /// `column` is not user input -- it comes from the two callers below --
    /// so interpolating it is safe, and sqlite cannot parameterise a column
    /// name.
    fn allocate_slot(&self, column: &str, node_id: &str, lo: u32, hi: u32) -> Result<Option<u32>> {
        let sql = format!(
            "SELECT {column} FROM sandboxes
             WHERE node_id=?1 AND {column} IS NOT NULL AND {column} BETWEEN ?2 AND ?3
             ORDER BY {column}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let used = stmt
            .query_map(params![node_id, lo, hi], |r| r.get::<_, u32>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Walk the sorted list for the first gap, rather than probing every
        // value in the range.
        let mut want = lo;
        for u in used {
            if u > want {
                break;
            }
            want = match u.checked_add(1) {
                Some(n) => n,
                None => return Ok(None),
            };
        }
        Ok((want <= hi).then_some(want))
    }

    /// The lowest free HOST port for a node's gateway.
    ///
    /// `gateway_port`, not `port`. It queried `port` at first, which is the
    /// port inside the namespace and is 8080 for nearly every sandbox --
    /// never inside the host range -- so the gap finder always saw an empty
    /// set and handed out the bottom of the range to everyone. Five
    /// sandboxes all got 20000; on a node the first gateway binds it and
    /// the rest fail with address-in-use, leaving them unreachable. Named
    /// for the column now so the two cannot be confused again.
    pub fn allocate_gateway_port(&self, node_id: &str, lo: u16, hi: u16) -> Result<Option<u16>> {
        Ok(self
            .allocate_slot("gateway_port", node_id, lo as u32, hi as u32)?
            .map(|v| v as u16))
    }

    /// Which /30 a new sandbox gets on this node.
    pub fn allocate_net_index(&self, node_id: &str, max: u32) -> Result<Option<u32>> {
        self.allocate_slot("net_index", node_id, 0, max)
    }

    /// Record the address the node reported, so callers can reach it.
    pub fn set_sandbox_ip(&self, id: &str, ip: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET ip=?2 WHERE id=?1 AND (ip IS NULL OR ip != ?2)",
            params![id, ip],
        )?;
        Ok(())
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
                    created_at, last_active_at, last_error, error_attempts,
                    net_index, ip, from_tag, memory_limit_mib, gateway_port
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
            last_error: r.get(10)?,
            error_attempts: r.get(11)?,
            net_index: r.get(12)?,
            ip: r.get(13)?,
            from_tag: r.get(14)?,
            memory_limit_mib: r.get::<_, Option<i64>>(15)?.map(|v| v as u64),
            gateway_port: r.get::<_, Option<u16>>(16)?,
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

    // ---- snapshot tags --------------------------------------------------

    pub fn put_snapshot(&self, s: &SnapshotTag) -> Result<()> {
        self.conn.execute(
            "INSERT INTO snapshots (tag, node_id, template, from_sandbox, created_at, size_bytes)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(tag) DO UPDATE SET
                node_id=excluded.node_id, template=excluded.template,
                from_sandbox=excluded.from_sandbox, created_at=excluded.created_at,
                size_bytes=excluded.size_bytes",
            params![
                s.tag,
                s.node_id,
                s.template,
                s.from_sandbox,
                s.created_at,
                s.size_bytes as i64
            ],
        )?;
        Ok(())
    }

    pub fn get_snapshot(&self, tag: &str) -> Result<Option<SnapshotTag>> {
        Ok(self
            .conn
            .query_row(
                "SELECT tag, node_id, template, from_sandbox, created_at, size_bytes
                 FROM snapshots WHERE tag=?1",
                params![tag],
                Self::row_to_snapshot,
            )
            .optional()?)
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotTag>> {
        let mut stmt = self.conn.prepare(
            "SELECT tag, node_id, template, from_sandbox, created_at, size_bytes
             FROM snapshots ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_snapshot)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_snapshot(&self, tag: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM snapshots WHERE tag=?1", params![tag])?
            > 0)
    }

    /// How many sandboxes were forked from this tag and are still around.
    /// A tag with children is still in use, which is why deleting one does
    /// not remove the image from the node.
    pub fn children_of(&self, tag: &str) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sandboxes WHERE from_tag=?1",
            params![tag],
            |r| r.get(0),
        )?)
    }

    fn row_to_snapshot(r: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotTag> {
        Ok(SnapshotTag {
            tag: r.get(0)?,
            node_id: r.get(1)?,
            template: r.get(2)?,
            from_sandbox: r.get(3)?,
            created_at: r.get(4)?,
            size_bytes: r.get::<_, i64>(5)? as u64,
        })
    }

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
            last_error: None,
            error_attempts: 0,
            net_index: None,
            ip: None,
            from_tag: None,
            memory_limit_mib: None,
            gateway_port: None,
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

    /// The scheduler asks for the count on every create, so it has to be
    /// the same number the full query would have produced -- including the
    /// 0 for a node that has nothing, which is a placement candidate and
    /// not an error.
    #[test]
    fn counting_a_nodes_sandboxes_agrees_with_listing_them() {
        let s = Store::open_in_memory().unwrap();
        s.insert_sandbox(&sb("sb1", "node-a")).unwrap();
        s.insert_sandbox(&sb("sb2", "node-a")).unwrap();
        s.insert_sandbox(&sb("sb3", "node-b")).unwrap();
        for node in ["node-a", "node-b", "node-never-seen"] {
            assert_eq!(
                s.count_sandboxes_on_node(node).unwrap() as usize,
                s.sandboxes_on_node(node).unwrap().len(),
                "count and list disagree for {node}"
            );
        }
        assert_eq!(s.count_sandboxes_on_node("node-a").unwrap(), 2);
        assert_eq!(s.count_sandboxes_on_node("node-never-seen").unwrap(), 0);

        // And it follows a delete, which is what makes a freed slot usable.
        s.delete_sandbox("sb1").unwrap();
        assert_eq!(s.count_sandboxes_on_node("node-a").unwrap(), 1);
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
    fn ports_are_allocated_per_node_from_the_lowest_gap() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sb("sb1", "node-a");
        a.gateway_port = Some(20000);
        let mut b = sb("sb2", "node-a");
        b.gateway_port = Some(20001);
        let mut c = sb("sb3", "node-a");
        c.gateway_port = Some(20003);
        for x in [&a, &b, &c] {
            s.insert_sandbox(x).unwrap();
        }
        // 20002 is the gap.
        assert_eq!(
            s.allocate_gateway_port("node-a", 20000, 20010).unwrap(),
            Some(20002)
        );
        // Another node has its own port space.
        assert_eq!(
            s.allocate_gateway_port("node-b", 20000, 20010).unwrap(),
            Some(20000)
        );
    }

    #[test]
    fn an_exhausted_port_range_allocates_nothing() {
        let s = Store::open_in_memory().unwrap();
        for (i, p) in (20000u16..=20002).enumerate() {
            let mut x = sb(&format!("sb{i}"), "node-a");
            x.gateway_port = Some(p);
            s.insert_sandbox(&x).unwrap();
        }
        assert_eq!(
            s.allocate_gateway_port("node-a", 20000, 20002).unwrap(),
            None
        );
        // One more port in the range is enough.
        assert_eq!(
            s.allocate_gateway_port("node-a", 20000, 20003).unwrap(),
            Some(20003)
        );
    }

    /// Two sandboxes on one node must not be handed the same host port.
    ///
    /// This is the case nothing covered. The existing tests pre-populated
    /// the column and checked the gap finder, which was correct -- the bug
    /// was that the production call read a different column, so every
    /// sandbox got the bottom of the range. Allocating twice in a row, the
    /// way `create_sandbox` does, is what catches that.
    #[test]
    fn two_sandboxes_on_a_node_get_different_gateway_ports() {
        let s = Store::open_in_memory().unwrap();
        let mut seen = std::collections::HashSet::new();
        for i in 0..5 {
            let p = s
                .allocate_gateway_port("node-a", 20000, 20010)
                .unwrap()
                .expect("a free port");
            assert!(seen.insert(p), "port {p} was handed out twice");
            let mut x = sb(&format!("sb{i}"), "node-a");
            // The in-namespace port is the same for all of them, which is
            // exactly the condition that hid the bug.
            x.port = Some(8080);
            x.gateway_port = Some(p);
            s.insert_sandbox(&x).unwrap();
        }
        assert_eq!(seen.len(), 5);
    }

    /// A port freed by a deleted sandbox is available again.
    #[test]
    fn deleting_a_sandbox_frees_its_port() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sb("sb1", "node-a");
        a.gateway_port = Some(20000);
        s.insert_sandbox(&a).unwrap();
        assert_eq!(
            s.allocate_gateway_port("node-a", 20000, 20010).unwrap(),
            Some(20001)
        );
        s.delete_sandbox("sb1").unwrap();
        assert_eq!(
            s.allocate_gateway_port("node-a", 20000, 20010).unwrap(),
            Some(20000)
        );
    }

    #[test]
    fn a_range_at_the_top_of_the_port_space_terminates() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sb("sb1", "node-a");
        a.gateway_port = Some(u16::MAX);
        s.insert_sandbox(&a).unwrap();
        assert_eq!(
            s.allocate_gateway_port("node-a", u16::MAX, u16::MAX)
                .unwrap(),
            None
        );
    }

    /// Network slots allocate the same way ports do, and per node.
    #[test]
    fn net_slots_are_allocated_per_node_from_the_lowest_gap() {
        let s = Store::open_in_memory().unwrap();
        for (i, idx) in [(0, 0u32), (1, 1), (2, 3)] {
            let mut x = sb(&format!("sb{i}"), "node-a");
            x.net_index = Some(idx);
            s.insert_sandbox(&x).unwrap();
        }
        assert_eq!(s.allocate_net_index("node-a", 100).unwrap(), Some(2));
        assert_eq!(s.allocate_net_index("node-b", 100).unwrap(), Some(0));
    }

    #[test]
    fn an_exhausted_address_plan_allocates_nothing() {
        let s = Store::open_in_memory().unwrap();
        for i in 0..3u32 {
            let mut x = sb(&format!("sb{i}"), "node-a");
            x.net_index = Some(i);
            s.insert_sandbox(&x).unwrap();
        }
        assert_eq!(s.allocate_net_index("node-a", 2).unwrap(), None);
        assert_eq!(s.allocate_net_index("node-a", 3).unwrap(), Some(3));
    }

    #[test]
    fn the_reported_address_is_stored() {
        let s = Store::open_in_memory().unwrap();
        s.insert_sandbox(&sb("sb1", "node-a")).unwrap();
        assert!(s.get_sandbox("sb1").unwrap().unwrap().ip.is_none());
        s.set_sandbox_ip("sb1", "10.86.0.2").unwrap();
        assert_eq!(
            s.get_sandbox("sb1").unwrap().unwrap().ip.as_deref(),
            Some("10.86.0.2")
        );
    }

    /// The caller's ceiling is remembered, because it is theirs and not
    /// the template's.
    #[test]
    fn a_per_sandbox_memory_limit_round_trips() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sb("sb1", "node-a");
        a.memory_limit_mib = Some(512);
        s.insert_sandbox(&a).unwrap();
        assert_eq!(
            s.get_sandbox("sb1").unwrap().unwrap().memory_limit_mib,
            Some(512)
        );
        // And absent means "whatever the template says".
        s.insert_sandbox(&sb("sb2", "node-a")).unwrap();
        assert_eq!(
            s.get_sandbox("sb2").unwrap().unwrap().memory_limit_mib,
            None
        );
    }

    #[test]
    fn sandbox_errors_are_recorded_and_cleared() {
        let s = Store::open_in_memory().unwrap();
        s.insert_sandbox(&sb("sb1", "node-a")).unwrap();
        s.set_sandbox_error("sb1", "systemd-run: access denied", 7)
            .unwrap();
        let got = s.get_sandbox("sb1").unwrap().unwrap();
        assert_eq!(got.error_attempts, 7);
        assert!(got.last_error.unwrap().contains("access denied"));

        s.clear_sandbox_error("sb1").unwrap();
        let got = s.get_sandbox("sb1").unwrap().unwrap();
        assert!(got.last_error.is_none());
        assert_eq!(got.error_attempts, 0);
    }

    /// Admission needs the node's own view of its health, so a poll must
    /// record it.
    #[test]
    fn a_poll_records_what_the_node_reports_about_itself() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&Node {
            id: "bm-1".into(),
            pool: "default".into(),
            fingerprint: "f".into(),
            max_sandboxes: 0,
            last_seen_at: now_secs(),
            connected: true,
            free_mib: 0,
            psi_some_avg10: 0.0,
        })
        .unwrap();
        s.set_node_health("bm-1", 408, 15.6).unwrap();
        let n = &s.list_nodes().unwrap()[0];
        assert_eq!(n.free_mib, 408);
        assert!((n.psi_some_avg10 - 15.6).abs() < 1e-9);
        assert!(n.connected, "reporting health means it is connected");
    }

    /// A restart must not inherit the last run's connectivity.
    #[test]
    fn restart_clears_connectivity_until_a_node_polls_in() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&Node {
            id: "bm-1".into(),
            pool: "default".into(),
            fingerprint: "f".into(),
            max_sandboxes: 10,
            last_seen_at: now_secs(),
            connected: true,
            free_mib: 0,
            psi_some_avg10: 0.0,
        })
        .unwrap();
        assert_eq!(s.mark_all_nodes_disconnected().unwrap(), 1);
        assert!(!s.list_nodes().unwrap()[0].connected);

        // And a poll brings it back.
        s.set_node_connected("bm-1", true).unwrap();
        assert!(s.list_nodes().unwrap()[0].connected);
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
