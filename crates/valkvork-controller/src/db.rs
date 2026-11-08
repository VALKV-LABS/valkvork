//! SQLite state store (FR-CON-01, spec §12).
//!
//! The [`Db`] write connection is owned by the core actor, so there is exactly
//! one writer. Admin queries that need history (events) use a separate
//! read-only connection ([`EventReader`]).

use crate::model::{Node, Service, Workload};
use rusqlite::{params, Connection, OpenFlags, Row};
use std::path::Path;
use valkvork_common::models::{now_ms, EventKind, ServiceSpec, SubjectType};
use valkvork_common::ValkvorkError;

/// Versioned migrations. Append only: never edit an applied entry.
const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r#"
CREATE TABLE nodes (
    node_id                    TEXT PRIMARY KEY,
    hostname                   TEXT NOT NULL,
    ip_address                 TEXT NOT NULL,
    node_class                 TEXT NOT NULL,
    agent_version              TEXT NOT NULL,
    runtime                    TEXT NOT NULL,
    allocatable_cpu_millicores INTEGER NOT NULL CHECK (allocatable_cpu_millicores > 0),
    allocatable_memory_bytes   INTEGER NOT NULL CHECK (allocatable_memory_bytes > 0),
    host_volumes               TEXT NOT NULL DEFAULT '[]',
    status                     TEXT NOT NULL DEFAULT 'HEALTHY' CHECK (status IN ('HEALTHY', 'UNHEALTHY')),
    draining                   INTEGER NOT NULL DEFAULT 0 CHECK (draining IN (0, 1)),
    last_heartbeat_at          INTEGER NOT NULL,
    created_at                 INTEGER NOT NULL,
    updated_at                 INTEGER NOT NULL
);

CREATE TABLE services (
    service_id          TEXT PRIMARY KEY,
    version             INTEGER NOT NULL DEFAULT 1,
    spec_json           TEXT NOT NULL,
    spec_hash           TEXT NOT NULL,
    image               TEXT NOT NULL,
    cpu_millicores      INTEGER NOT NULL CHECK (cpu_millicores > 0),
    memory_bytes        INTEGER NOT NULL CHECK (memory_bytes > 0),
    required_class      TEXT,
    target_replicas     INTEGER NOT NULL CHECK (target_replicas >= 0),
    autoscale_enabled   INTEGER NOT NULL DEFAULT 0 CHECK (autoscale_enabled IN (0, 1)),
    min_replicas        INTEGER,
    max_replicas        INTEGER,
    target_cpu_percent  REAL,
    last_scaled_at      INTEGER,
    status              TEXT NOT NULL DEFAULT 'ACTIVE' CHECK (status IN ('ACTIVE', 'DEGRADED', 'DELETING')),
    rollout_status      TEXT NOT NULL DEFAULT 'NONE' CHECK (rollout_status IN ('NONE', 'IN_PROGRESS', 'HALTED')),
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (autoscale_enabled = 0
           OR (min_replicas >= 1 AND max_replicas >= min_replicas
               AND target_cpu_percent > 0 AND target_cpu_percent < 100))
);

CREATE TABLE workloads (
    workload_id          TEXT PRIMARY KEY,
    service_id           TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
    service_version      INTEGER NOT NULL,
    node_id              TEXT REFERENCES nodes(node_id) ON DELETE SET NULL,
    container_id         TEXT,
    desired_state        TEXT NOT NULL DEFAULT 'RUN' CHECK (desired_state IN ('RUN', 'STOP')),
    status               TEXT NOT NULL DEFAULT 'PENDING'
                         CHECK (status IN ('PENDING', 'ASSIGNED', 'RUNNING', 'STOPPING', 'LOST', 'STOPPED', 'FAILED')),
    status_reason        TEXT,
    replaces_workload_id TEXT,
    restart_count        INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL,
    started_at           INTEGER,
    updated_at           INTEGER NOT NULL
);

CREATE TABLE scaling_events (
    event_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    service_id     TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
    from_replicas  INTEGER NOT NULL,
    to_replicas    INTEGER NOT NULL,
    metric_value   REAL,
    reason         TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE TABLE cluster_events (
    event_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    kind           TEXT NOT NULL CHECK (kind IN ('NODE', 'WORKLOAD', 'DEPLOY', 'SCALE', 'AUDIT')),
    subject_type   TEXT NOT NULL CHECK (subject_type IN ('node', 'service', 'workload')),
    subject_id     TEXT NOT NULL,
    message        TEXT NOT NULL,
    actor          TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE INDEX idx_workloads_service_status ON workloads(service_id, status);
CREATE INDEX idx_cluster_events_time      ON cluster_events(created_at);
CREATE INDEX idx_workloads_node           ON workloads(node_id) WHERE node_id IS NOT NULL;
CREATE INDEX idx_workloads_status         ON workloads(status);
CREATE INDEX idx_nodes_class_status       ON nodes(node_class, status);
CREATE INDEX idx_scaling_events_service   ON scaling_events(service_id, created_at);
"#,
    ),
    (
        2,
        // COMPLETED (batch workloads). SQLite cannot change a CHECK, so the
        // table is rebuilt. Nothing references workloads.
        r#"
CREATE TABLE workloads_v2 (
    workload_id          TEXT PRIMARY KEY,
    service_id           TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
    service_version      INTEGER NOT NULL,
    node_id              TEXT REFERENCES nodes(node_id) ON DELETE SET NULL,
    container_id         TEXT,
    desired_state        TEXT NOT NULL DEFAULT 'RUN' CHECK (desired_state IN ('RUN', 'STOP')),
    status               TEXT NOT NULL DEFAULT 'PENDING'
                         CHECK (status IN ('PENDING', 'ASSIGNED', 'RUNNING', 'STOPPING', 'LOST', 'STOPPED', 'FAILED', 'COMPLETED')),
    status_reason        TEXT,
    replaces_workload_id TEXT,
    restart_count        INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL,
    started_at           INTEGER,
    updated_at           INTEGER NOT NULL
);
INSERT INTO workloads_v2 SELECT workload_id, service_id, service_version, node_id, container_id,
    desired_state, status, status_reason, replaces_workload_id, restart_count, created_at,
    started_at, updated_at FROM workloads;
DROP TABLE workloads;
ALTER TABLE workloads_v2 RENAME TO workloads;
CREATE INDEX idx_workloads_service_status ON workloads(service_id, status);
CREATE INDEX idx_workloads_node           ON workloads(node_id) WHERE node_id IS NOT NULL;
CREATE INDEX idx_workloads_status         ON workloads(status);
"#,
    ),
];

fn db_err(e: impl std::fmt::Display) -> ValkvorkError {
    ValkvorkError::Database(e.to_string())
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    // Bound the page cache (NFR-01): negative = KiB.
    conn.pragma_update(None, "cache_size", -8192)?;
    Ok(())
}

/// A cluster event waiting to be written.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub kind: EventKind,
    pub subject_type: SubjectType,
    pub subject_id: String,
    pub message: String,
    pub actor: String,
}

#[derive(Debug, Clone)]
pub struct NewScalingEvent {
    pub service_id: String,
    pub from_replicas: u32,
    pub to_replicas: u32,
    pub metric_value: f64,
    pub reason: String,
}

/// Everything one commit writes, in one transaction.
#[derive(Default)]
pub struct Batch<'a> {
    pub nodes: Vec<&'a Node>,
    pub services: Vec<&'a Service>,
    pub workloads: Vec<&'a Workload>,
    pub deleted_workloads: Vec<String>,
    pub deleted_services: Vec<String>,
    /// Removed nodes. Deleted after the workload upserts; `ON DELETE SET
    /// NULL` clears `node_id` on their remaining workload rows.
    pub deleted_nodes: Vec<String>,
    pub events: Vec<NewEvent>,
    pub scaling_events: Vec<NewScalingEvent>,
    /// (node_id, last_heartbeat_at) — lazy heartbeat flush.
    pub heartbeats: Vec<(String, i64)>,
}

impl Batch<'_> {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.services.is_empty()
            && self.workloads.is_empty()
            && self.deleted_workloads.is_empty()
            && self.deleted_services.is_empty()
            && self.deleted_nodes.is_empty()
            && self.events.is_empty()
            && self.scaling_events.is_empty()
            && self.heartbeats.is_empty()
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, ValkvorkError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path).map_err(db_err)?;
        // Persistent; set once (spec §12).
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(db_err)?;
        if mode != "wal" && path != Path::new(":memory:") {
            tracing::warn!(mode, "SQLite did not switch to WAL mode");
        }
        configure(&conn).map_err(db_err)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self, ValkvorkError> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        configure(&conn).map_err(db_err)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<(), ValkvorkError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
            )
            .map_err(db_err)?;
        let current: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
            let tx = self.conn.transaction().map_err(db_err)?;
            tx.execute_batch(sql).map_err(db_err)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, now_ms()],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            tracing::info!(version, "applied schema migration");
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Startup load (FR-CON-04)
    // ------------------------------------------------------------------

    pub fn load_nodes(&self) -> Result<Vec<Node>, ValkvorkError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT node_id, hostname, ip_address, node_class, agent_version, runtime,
                        allocatable_cpu_millicores, allocatable_memory_bytes, host_volumes,
                        status, draining, last_heartbeat_at, created_at, updated_at FROM nodes",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Node {
                    node_id: r.get(0)?,
                    hostname: r.get(1)?,
                    ip_address: r.get(2)?,
                    node_class: r.get(3)?,
                    agent_version: r.get(4)?,
                    runtime: r.get(5)?,
                    allocatable_cpu_millicores: r.get(6)?,
                    allocatable_memory_bytes: r.get::<_, i64>(7)? as u64,
                    host_volumes: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                    status: parse_col(r, 9)?,
                    draining: r.get::<_, i64>(10)? != 0,
                    last_heartbeat_at: r.get(11)?,
                    created_at: r.get(12)?,
                    updated_at: r.get(13)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    pub fn load_services(&self) -> Result<Vec<Service>, ValkvorkError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT service_id, version, spec_json, spec_hash, target_replicas, status,
                        rollout_status, last_scaled_at, created_at, updated_at FROM services",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                let spec_json: String = r.get(2)?;
                let spec: ServiceSpec = serde_json::from_str(&spec_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(Service {
                    service_id: r.get(0)?,
                    version: r.get::<_, i64>(1)? as u64,
                    spec,
                    spec_hash: r.get(3)?,
                    target_replicas: r.get(4)?,
                    status: parse_col(r, 5)?,
                    rollout_status: parse_col(r, 6)?,
                    last_scaled_at: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    pub fn load_workloads(&self) -> Result<Vec<Workload>, ValkvorkError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT workload_id, service_id, service_version, node_id, container_id, desired_state,
                        status, status_reason, replaces_workload_id, restart_count, created_at,
                        started_at, updated_at FROM workloads",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Workload {
                    workload_id: r.get(0)?,
                    service_id: r.get(1)?,
                    service_version: r.get::<_, i64>(2)? as u64,
                    node_id: r.get(3)?,
                    container_id: r.get(4)?,
                    desired_state: parse_col(r, 5)?,
                    status: parse_col(r, 6)?,
                    status_reason: r.get(7)?,
                    replaces_workload_id: r.get(8)?,
                    restart_count: r.get(9)?,
                    created_at: r.get(10)?,
                    started_at: r.get(11)?,
                    updated_at: r.get(12)?,
                    health: Default::default(),
                    healthy_since: None,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    // ------------------------------------------------------------------
    // Writes
    // ------------------------------------------------------------------

    /// Write a batch in one transaction (fsync on commit, `synchronous=FULL`).
    pub fn commit(&mut self, b: &Batch<'_>) -> Result<(), ValkvorkError> {
        if b.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction().map_err(db_err)?;
        for n in &b.nodes {
            tx.prepare_cached(
                "INSERT INTO nodes (node_id, hostname, ip_address, node_class, agent_version, runtime,
                    allocatable_cpu_millicores, allocatable_memory_bytes, host_volumes, status, draining,
                    last_heartbeat_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(node_id) DO UPDATE SET
                    hostname = excluded.hostname, ip_address = excluded.ip_address,
                    node_class = excluded.node_class, agent_version = excluded.agent_version,
                    runtime = excluded.runtime,
                    allocatable_cpu_millicores = excluded.allocatable_cpu_millicores,
                    allocatable_memory_bytes = excluded.allocatable_memory_bytes,
                    host_volumes = excluded.host_volumes, status = excluded.status,
                    draining = excluded.draining, last_heartbeat_at = excluded.last_heartbeat_at,
                    updated_at = excluded.updated_at",
            )
            .and_then(|mut s| {
                s.execute(params![
                    n.node_id,
                    n.hostname,
                    n.ip_address,
                    n.node_class,
                    n.agent_version,
                    n.runtime,
                    n.allocatable_cpu_millicores,
                    n.allocatable_memory_bytes as i64,
                    serde_json::to_string(&n.host_volumes).unwrap_or_else(|_| "[]".into()),
                    n.status.as_str(),
                    n.draining as i64,
                    n.last_heartbeat_at,
                    n.created_at,
                    n.updated_at,
                ])
            })
            .map_err(db_err)?;
        }
        for (node_id, at) in &b.heartbeats {
            tx.prepare_cached("UPDATE nodes SET last_heartbeat_at = ?1 WHERE node_id = ?2")
                .and_then(|mut s| s.execute(params![at, node_id]))
                .map_err(db_err)?;
        }
        for s in &b.services {
            let a = s.spec.autoscale.as_ref();
            tx.prepare_cached(
                "INSERT INTO services (service_id, version, spec_json, spec_hash, image, cpu_millicores,
                    memory_bytes, required_class, target_replicas, autoscale_enabled, min_replicas,
                    max_replicas, target_cpu_percent, last_scaled_at, status, rollout_status,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT(service_id) DO UPDATE SET
                    version = excluded.version, spec_json = excluded.spec_json,
                    spec_hash = excluded.spec_hash, image = excluded.image,
                    cpu_millicores = excluded.cpu_millicores, memory_bytes = excluded.memory_bytes,
                    required_class = excluded.required_class, target_replicas = excluded.target_replicas,
                    autoscale_enabled = excluded.autoscale_enabled, min_replicas = excluded.min_replicas,
                    max_replicas = excluded.max_replicas, target_cpu_percent = excluded.target_cpu_percent,
                    last_scaled_at = excluded.last_scaled_at, status = excluded.status,
                    rollout_status = excluded.rollout_status, updated_at = excluded.updated_at",
            )
            .and_then(|mut st| {
                st.execute(params![
                    s.service_id,
                    s.version as i64,
                    s.spec.canonical_json(),
                    s.spec_hash,
                    s.spec.image,
                    s.spec.cpu_millicores,
                    s.spec.memory_bytes as i64,
                    s.spec.required_class,
                    s.target_replicas,
                    a.is_some() as i64,
                    a.map(|a| a.min_replicas),
                    a.map(|a| a.max_replicas),
                    a.map(|a| a.target_cpu_percent),
                    s.last_scaled_at,
                    s.status.as_str(),
                    s.rollout_status.as_str(),
                    s.created_at,
                    s.updated_at,
                ])
            })
            .map_err(db_err)?;
        }
        for w in &b.workloads {
            tx.prepare_cached(
                "INSERT INTO workloads (workload_id, service_id, service_version, node_id, container_id,
                    desired_state, status, status_reason, replaces_workload_id, restart_count,
                    created_at, started_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(workload_id) DO UPDATE SET
                    node_id = excluded.node_id, container_id = excluded.container_id,
                    desired_state = excluded.desired_state, status = excluded.status,
                    status_reason = excluded.status_reason,
                    replaces_workload_id = excluded.replaces_workload_id,
                    restart_count = excluded.restart_count, started_at = excluded.started_at,
                    updated_at = excluded.updated_at",
            )
            .and_then(|mut s| {
                s.execute(params![
                    w.workload_id,
                    w.service_id,
                    w.service_version as i64,
                    w.node_id,
                    w.container_id,
                    w.desired_state.as_str(),
                    w.status.as_str(),
                    w.status_reason,
                    w.replaces_workload_id,
                    w.restart_count,
                    w.created_at,
                    w.started_at,
                    w.updated_at,
                ])
            })
            .map_err(db_err)?;
        }
        for id in &b.deleted_workloads {
            tx.execute("DELETE FROM workloads WHERE workload_id = ?1", params![id])
                .map_err(db_err)?;
        }
        for id in &b.deleted_nodes {
            tx.execute("DELETE FROM nodes WHERE node_id = ?1", params![id])
                .map_err(db_err)?;
        }
        let now = now_ms();
        for e in &b.scaling_events {
            tx.execute(
                "INSERT INTO scaling_events (service_id, from_replicas, to_replicas, metric_value, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![e.service_id, e.from_replicas, e.to_replicas, e.metric_value, e.reason, now],
            )
            .map_err(db_err)?;
        }
        for id in &b.deleted_services {
            tx.execute("DELETE FROM services WHERE service_id = ?1", params![id])
                .map_err(db_err)?;
        }
        for e in &b.events {
            tx.execute(
                "INSERT INTO cluster_events (kind, subject_type, subject_id, message, actor, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![e.kind.as_str(), e.subject_type.as_str(), e.subject_id, e.message, e.actor, now],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    #[cfg(test)]
    pub fn count_events_for_test(&self, subject_id: &str) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM cluster_events WHERE subject_id = ?1",
                params![subject_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize
    }

    #[cfg(test)]
    pub fn count_rows(&self, table: &str) -> i64 {
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    /// FR-CON-11: prune terminal workloads older than 24 h beyond the newest
    /// 10 per service, and events older than 30 days. Returns pruned workload ids.
    pub fn prune_history(&mut self, now: i64) -> Result<Vec<String>, ValkvorkError> {
        let day = 24 * 3600 * 1000;
        let tx = self.conn.transaction().map_err(db_err)?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT workload_id FROM (
                        SELECT workload_id, updated_at,
                               ROW_NUMBER() OVER (PARTITION BY service_id ORDER BY updated_at DESC) AS rn
                        FROM workloads WHERE status IN ('STOPPED', 'FAILED', 'COMPLETED')
                     ) WHERE rn > 10 AND updated_at < ?1",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![now - day], |r| r.get(0))
                .map_err(db_err)?;
            rows.collect::<Result<_, _>>().map_err(db_err)?
        };
        for id in &ids {
            tx.execute("DELETE FROM workloads WHERE workload_id = ?1", params![id])
                .map_err(db_err)?;
        }
        tx.execute(
            "DELETE FROM cluster_events WHERE created_at < ?1",
            params![now - 30 * day],
        )
        .map_err(db_err)?;
        tx.execute(
            "DELETE FROM scaling_events WHERE created_at < ?1",
            params![now - 30 * day],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(ids)
    }
}

fn parse_col<T: std::str::FromStr<Err = String>>(r: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let s: String = r.get(idx)?;
    s.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
    })
}

// ============================================================================
// Read-only access for history queries
// ============================================================================

#[derive(Debug, Clone)]
pub struct EventRow {
    pub event_id: i64,
    pub kind: String,
    pub subject_type: String,
    pub subject_id: String,
    pub message: String,
    pub actor: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct ScalingRow {
    pub event_id: i64,
    pub from_replicas: u32,
    pub to_replicas: u32,
    pub metric_value: f64,
    pub reason: String,
    pub created_at: i64,
}

/// Separate read-only connection (FR-CON-01), so history queries never
/// contend with the writer.
pub struct EventReader {
    conn: std::sync::Mutex<Connection>,
}

impl EventReader {
    pub fn open(path: &Path) -> Result<Self, ValkvorkError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db_err)?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(db_err)?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }

    pub fn events(
        &self,
        kind: Option<&str>,
        since: i64,
        limit: u32,
    ) -> Result<Vec<EventRow>, ValkvorkError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT event_id, kind, subject_type, subject_id, message, actor, created_at
                 FROM cluster_events
                 WHERE (?1 IS NULL OR kind = ?1) AND created_at >= ?2
                 ORDER BY event_id DESC LIMIT ?3",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![kind, since, limit], |r| {
                Ok(EventRow {
                    event_id: r.get(0)?,
                    kind: r.get(1)?,
                    subject_type: r.get(2)?,
                    subject_id: r.get(3)?,
                    message: r.get(4)?,
                    actor: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    pub fn scaling_events(
        &self,
        service_id: &str,
        limit: u32,
    ) -> Result<Vec<ScalingRow>, ValkvorkError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT event_id, from_replicas, to_replicas, COALESCE(metric_value, 0), reason, created_at
                 FROM scaling_events WHERE service_id = ?1 ORDER BY event_id DESC LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![service_id, limit], |r| {
                Ok(ScalingRow {
                    event_id: r.get(0)?,
                    from_replicas: r.get(1)?,
                    to_replicas: r.get(2)?,
                    metric_value: r.get(3)?,
                    reason: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A v1 database with a service and a workload: migration 2 keeps them
    /// and allows COMPLETED.
    #[test]
    fn v1_database_migrates_with_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0].1).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
                 INSERT INTO schema_migrations VALUES (1, 0);
                 INSERT INTO services (service_id, spec_json, spec_hash, image, cpu_millicores,
                     memory_bytes, target_replicas, created_at, updated_at)
                     VALUES ('web', '{}', 'h', 'nginx:1', 100, 1048576, 1, 0, 0);
                 INSERT INTO workloads (workload_id, service_id, service_version, status, created_at, updated_at)
                     VALUES ('w1', 'web', 1, 'RUNNING', 0, 0);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let (status, n): (String, i64) = db
            .conn
            .query_row(
                "SELECT status, (SELECT MAX(version) FROM schema_migrations) FROM workloads WHERE workload_id = 'w1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((status.as_str(), n), ("RUNNING", 2));
        db.conn
            .execute(
                "UPDATE workloads SET status = 'COMPLETED' WHERE workload_id = 'w1'",
                [],
            )
            .unwrap();
        assert!(
            db.conn
                .execute(
                    "UPDATE workloads SET status = 'BOGUS' WHERE workload_id = 'w1'",
                    []
                )
                .is_err(),
            "the CHECK is still there"
        );
    }
}
