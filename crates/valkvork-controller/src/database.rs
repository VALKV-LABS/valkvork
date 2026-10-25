//! SQLite database layer for Valkvork controller.
//!
//! This module implements the SQLite state store as per FR-CON-01.
//!
//! Key requirements:
//! - Desired state MUST be persisted in SQLite in WAL mode, with synchronous = FULL
//! - All writes MUST go through one writer task
//! - Reads use a separate read-only connection pool
//! - Schema changes MUST go through versioned migrations

use std::path::Path;
use std::sync::Arc;
use rusqlite::{Connection, OpenFlags, params, Result};
use valkvork_common::errors::{ValkvorkError, DatabaseError};
use valkvork_common::models::{Service, Workload, Node, ServiceStatus, WorkloadStatus, DesiredState, NodeStatus, ClusterEvent, EventKind, SubjectType, ScalingEvent};
use chrono::{DateTime, Utc};

// ============================================================================
// Schema
// ============================================================================

const SCHEMA_VERSION: u32 = 1;

/// Initialize the database schema.
pub fn initialize_schema(conn: &Connection) -> Result<(), DatabaseError> {
    // Set PRAGMA settings as per spec
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    
    // Create schema_migrations table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )"
    )?;
    
    // Check current version
    let current_version: u32 = conn.query_row(
        "SELECT MAX(version) FROM schema_migrations",
        [],
        |row| row.get(0)
    ).unwrap_or(0);
    
    // Apply migrations if needed
    if current_version < SCHEMA_VERSION {
        apply_migrations(conn, current_version)?;
    }
    
    // Create tables if they don't exist
    create_tables(conn)?;
    
    Ok(())
}

/// Apply database migrations.
fn apply_migrations(conn: &Connection, from_version: u32) -> Result<(), DatabaseError> {
    for version in (from_version + 1)..=SCHEMA_VERSION {
        apply_migration(conn, version)?;
    }
    Ok(())
}

/// Apply a single migration.
fn apply_migration(conn: &Connection, version: u32) -> Result<(), DatabaseError> {
    // In a real implementation, each migration would have its own SQL
    // For now, just record the migration
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?, ?)",
        params![version, now],
    )?;
    Ok(())
}

/// Create database tables as per spec §12.
fn create_tables(conn: &Connection) -> Result<(), DatabaseError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS nodes (
            node_id                   TEXT PRIMARY KEY,
            hostname                  TEXT NOT NULL,
            ip_address                TEXT NOT NULL,
            node_class                TEXT NOT NULL,
            agent_version             TEXT NOT NULL,
            runtime                   TEXT NOT NULL,
            allocatable_cpu_millicores INTEGER NOT NULL CHECK (allocatable_cpu_millicores > 0),
            allocatable_memory_bytes  INTEGER NOT NULL CHECK (allocatable_memory_bytes > 0),
            host_volumes              TEXT NOT NULL DEFAULT '[]',
            status                    TEXT NOT NULL DEFAULT 'HEALTHY' CHECK (status IN ('HEALTHY', 'UNHEALTHY')),
            draining                  INTEGER NOT NULL DEFAULT 0 CHECK (draining IN (0, 1)),
            last_heartbeat_at         INTEGER NOT NULL,
            created_at                INTEGER NOT NULL,
            updated_at                INTEGER NOT NULL
        );
        
        CREATE TABLE IF NOT EXISTS services (
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
            CHECK (autoscale_enabled = 0 OR (min_replicas >= 1 AND max_replicas >= min_replicas AND target_cpu_percent > 0 AND target_cpu_percent < 100))
        );
        
        CREATE TABLE IF NOT EXISTS workloads (
            workload_id          TEXT PRIMARY KEY,
            service_id           TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
            service_version      INTEGER NOT NULL,
            node_id              TEXT REFERENCES nodes(node_id) ON DELETE SET NULL,
            container_id         TEXT,
            desired_state        TEXT NOT NULL DEFAULT 'RUN' CHECK (desired_state IN ('RUN', 'STOP')),
            status               TEXT NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING', 'ASSIGNED', 'RUNNING', 'STOPPING', 'LOST', 'STOPPED', 'FAILED')),
            status_reason        TEXT,
            replaces_workload_id TEXT,
            restart_count        INTEGER NOT NULL DEFAULT 0,
            created_at           INTEGER NOT NULL,
            started_at           INTEGER,
            updated_at           INTEGER NOT NULL
        );
        
        CREATE TABLE IF NOT EXISTS scaling_events (
            event_id       INTEGER PRIMARY KEY AUTOINCREMENT,
            service_id     TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
            from_replicas  INTEGER NOT NULL,
            to_replicas    INTEGER NOT NULL,
            metric_value   REAL,
            reason         TEXT NOT NULL,
            created_at     INTEGER NOT NULL
        );
        
        CREATE TABLE IF NOT EXISTS cluster_events (
            event_id       INTEGER PRIMARY KEY AUTOINCREMENT,
            kind           TEXT NOT NULL CHECK (kind IN ('NODE', 'WORKLOAD', 'DEPLOY', 'SCALE', 'AUDIT')),
            subject_type   TEXT NOT NULL CHECK (subject_type IN ('node', 'service', 'workload')),
            subject_id     TEXT NOT NULL,
            message        TEXT NOT NULL,
            actor          TEXT NOT NULL,
            created_at     INTEGER NOT NULL
        );
        
        CREATE INDEX IF NOT EXISTS idx_workloads_service_status ON workloads(service_id, status);
        CREATE INDEX IF NOT EXISTS idx_cluster_events_time ON cluster_events(created_at);
        CREATE INDEX IF NOT EXISTS idx_workloads_node ON workloads(node_id) WHERE node_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_workloads_status ON workloads(status);
        CREATE INDEX IF NOT EXISTS idx_nodes_class_status ON nodes(node_class, status);
        CREATE INDEX IF NOT EXISTS idx_scaling_events_service ON scaling_events(service_id, created_at);
        "
    )?;
    
    Ok(())
}

// ============================================================================
// Database Connection Pool
// ============================================================================

/// Database connection pool.
#[derive(Clone)]
pub struct DatabasePool {
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Mutex<Vec<Connection>>>,
}

impl DatabasePool {
    pub fn new(path: &Path) -> Result<Self, ValkvorkError> {
        let writer = open_writer_connection(path)?;
        let readers = Arc::new(Mutex::new(Vec::new()));
        
        Ok(Self { writer, readers })
    }
    
    /// Get a writer connection.
    pub fn writer(&self) -> std::sync::MutexGuard<Connection> {
        self.writer.lock().unwrap()
    }
    
    /// Get a reader connection from the pool.
    pub fn reader(&self) -> Result<std::sync::MutexGuard<Connection>, ValkvorkError> {
        let mut readers = self.readers.lock().unwrap();
        
        // Try to get an existing connection
        if let Some(conn) = readers.pop() {
            return Ok(std::sync::MutexGuard::new(readers, conn));
        }
        
        // Create a new connection
        Err(ValkvorkError::Database(DatabaseError::Connection(
            "No reader connections available".to_string()
        )))
    }
    
    /// Return a reader connection to the pool.
    pub fn return_reader(&mut self, conn: Connection) {
        let mut readers = self.readers.lock().unwrap();
        readers.push(conn);
    }
}

/// Open writer connection with WAL mode.
fn open_writer_connection(path: &Path) -> Result<Connection, ValkvorkError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI,
    )?;
    
    // Set PRAGMA settings
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    
    // Initialize schema
    initialize_schema(&conn)?;
    
    Ok(conn)
}

// ============================================================================
// Node Operations
// ============================================================================

impl DatabasePool {
    pub fn upsert_node(&self, node: &Node) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        
        let host_volumes_json = serde_json::to_string(&node.host_volumes)?;
        let status_str = match node.status {
            NodeStatus::Healthy => "HEALTHY",
            NodeStatus::Unhealthy => "UNHEALTHY",
        };
        
        writer.execute(
            "INSERT INTO nodes (node_id, hostname, ip_address, node_class, agent_version, runtime, allocatable_cpu_millicores, allocatable_memory_bytes, host_volumes, status, draining, last_heartbeat_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(node_id) DO UPDATE SET
                 hostname = excluded.hostname,
                 ip_address = excluded.ip_address,
                 node_class = excluded.node_class,
                 agent_version = excluded.agent_version,
                 runtime = excluded.runtime,
                 allocatable_cpu_millicores = excluded.allocatable_cpu_millicores,
                 allocatable_memory_bytes = excluded.allocatable_memory_bytes,
                 host_volumes = excluded.host_volumes,
                 status = excluded.status,
                 draining = excluded.draining,
                 last_heartbeat_at = excluded.last_heartbeat_at,
                 updated_at = excluded.updated_at",
            params![
                &node.node_id,
                &node.hostname,
                &node.ip_address,
                &node.node_class,
                &node.agent_version,
                &node.runtime,
                node.allocatable_cpu_millicores as i64,
                node.allocatable_memory_bytes as i64,
                &host_volumes_json,
                status_str,
                node.draining as i64,
                now,
                now,
                now,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn get_node(&self, node_id: &str) -> Result<Option<Node>, ValkvorkError> {
        let reader = self.reader()?;
        
        let node = reader.query_row(
            "SELECT node_id, hostname, ip_address, node_class, agent_version, runtime, allocatable_cpu_millicores, allocatable_memory_bytes, host_volumes, status, draining, last_heartbeat_at, created_at, updated_at FROM nodes WHERE node_id = ?",
            [node_id],
            |row| {
                let host_volumes: Vec<String> = serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default();
                let status = match row.get::<_, String>(9)? {
                    s if s == "HEALTHY" => NodeStatus::Healthy,
                    _ => NodeStatus::Unhealthy,
                };
                let draining = row.get::<_, i64>(10)? != 0;
                
                Ok(Node {
                    node_id: row.get(0)?,
                    hostname: row.get(1)?,
                    ip_address: row.get(2)?,
                    node_class: row.get(3)?,
                    agent_version: row.get(4)?,
                    runtime: row.get(5)?,
                    allocatable_cpu_millicores: row.get(6)?,
                    allocatable_memory_bytes: row.get(7)?,
                    host_volumes,
                    status,
                    draining,
                    last_heartbeat_at: DateTime::from_timestamp_millis(row.get(11)?).unwrap_or_else(|| Utc::now()),
                    created_at: DateTime::from_timestamp_millis(row.get(12)?).unwrap_or_else(|| Utc::now()),
                    updated_at: DateTime::from_timestamp_millis(row.get(13)?).unwrap_or_else(|| Utc::now()),
                })
            },
        )?;
        
        Ok(node)
    }
    
    pub fn list_nodes(&self) -> Result<Vec<Node>, ValkvorkError> {
        let reader = self.reader()?;
        
        let mut stmt = reader.prepare(
            "SELECT node_id, hostname, ip_address, node_class, agent_version, runtime, allocatable_cpu_millicores, allocatable_memory_bytes, host_volumes, status, draining, last_heartbeat_at, created_at, updated_at FROM nodes"
        )?;
        
        let nodes: Vec<Node> = stmt.query_map([], |row| {
            let host_volumes: Vec<String> = serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default();
            let status = match row.get::<_, String>(9)? {
                s if s == "HEALTHY" => NodeStatus::Healthy,
                _ => NodeStatus::Unhealthy,
            };
            let draining = row.get::<_, i64>(10)? != 0;
            
            Ok(Node {
                node_id: row.get(0)?,
                hostname: row.get(1)?,
                ip_address: row.get(2)?,
                node_class: row.get(3)?,
                agent_version: row.get(4)?,
                runtime: row.get(5)?,
                allocatable_cpu_millicores: row.get(6)?,
                allocatable_memory_bytes: row.get(7)?,
                host_volumes,
                status,
                draining,
                last_heartbeat_at: DateTime::from_timestamp_millis(row.get(11)?).unwrap_or_else(|| Utc::now()),
                created_at: DateTime::from_timestamp_millis(row.get(12)?).unwrap_or_else(|| Utc::now()),
                updated_at: DateTime::from_timestamp_millis(row.get(13)?).unwrap_or_else(|| Utc::now()),
            })
        })?;
        
        nodes.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn update_node_heartbeat(&self, node_id: &str) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        
        writer.execute(
            "UPDATE nodes SET last_heartbeat_at = ?, updated_at = ? WHERE node_id = ?",
            params![now, now, node_id],
        )?;
        
        Ok(())
    }
    
    pub fn set_node_status(&self, node_id: &str, status: NodeStatus) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        let status_str = match status {
            NodeStatus::Healthy => "HEALTHY",
            NodeStatus::Unhealthy => "UNHEALTHY",
        };
        
        writer.execute(
            "UPDATE nodes SET status = ?, updated_at = ? WHERE node_id = ?",
            params![status_str, now, node_id],
        )?;
        
        Ok(())
    }
    
    pub fn set_node_draining(&self, node_id: &str, draining: bool) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        
        writer.execute(
            "UPDATE nodes SET draining = ?, updated_at = ? WHERE node_id = ?",
            params![draining as i64, now, node_id],
        )?;
        
        Ok(())
    }
}

// ============================================================================
// Service Operations
// ============================================================================

impl DatabasePool {
    pub fn create_service(&self, service: &Service) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        let spec_json = serde_json::to_string(&service.spec)?;
        let status_str = match service.status {
            ServiceStatus::Active => "ACTIVE",
            ServiceStatus::Degraded => "DEGRADED",
            ServiceStatus::Deleting => "DELETING",
        };
        let rollout_status_str = match service.rollout_status {
            valkvork_common::models::RolloutStatus::None => "NONE",
            valkvork_common::models::RolloutStatus::InProgress => "IN_PROGRESS",
            valkvork_common::models::RolloutStatus::Halted => "HALTED",
        };
        
        writer.execute(
            "INSERT INTO services (service_id, version, spec_json, spec_hash, image, cpu_millicores, memory_bytes, required_class, target_replicas, autoscale_enabled, min_replicas, max_replicas, target_cpu_percent, last_scaled_at, status, rollout_status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &service.service_id,
                service.version as i64,
                &spec_json,
                &service.spec_hash,
                &service.spec.image,
                service.spec.cpu_millicores as i64,
                service.spec.memory_bytes as i64,
                service.spec.required_class.as_deref(),
                service.target_replicas as i64,
                service.autoscale_enabled() as i64,
                service.min_replicas.map(|v| v as i64),
                service.max_replicas.map(|v| v as i64),
                service.target_cpu_percent,
                service.last_scaled_at.map(|t| t.timestamp_millis()),
                status_str,
                rollout_status_str,
                now,
                now,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn update_service(&self, service: &Service) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        let spec_json = serde_json::to_string(&service.spec)?;
        let status_str = match service.status {
            ServiceStatus::Active => "ACTIVE",
            ServiceStatus::Degraded => "DEGRADED",
            ServiceStatus::Deleting => "DELETING",
        };
        let rollout_status_str = match service.rollout_status {
            valkvork_common::models::RolloutStatus::None => "NONE",
            valkvork_common::models::RolloutStatus::InProgress => "IN_PROGRESS",
            valkvork_common::models::RolloutStatus::Halted => "HALTED",
        };
        
        writer.execute(
            "UPDATE services SET version = ?, spec_json = ?, spec_hash = ?, image = ?, cpu_millicores = ?, memory_bytes = ?, required_class = ?, target_replicas = ?, autoscale_enabled = ?, min_replicas = ?, max_replicas = ?, target_cpu_percent = ?, last_scaled_at = ?, status = ?, rollout_status = ?, updated_at = ? WHERE service_id = ?",
            params![
                service.version as i64,
                &spec_json,
                &service.spec_hash,
                &service.spec.image,
                service.spec.cpu_millicores as i64,
                service.spec.memory_bytes as i64,
                service.spec.required_class.as_deref(),
                service.target_replicas as i64,
                service.autoscale_enabled() as i64,
                service.min_replicas.map(|v| v as i64),
                service.max_replicas.map(|v| v as i64),
                service.target_cpu_percent,
                service.last_scaled_at.map(|t| t.timestamp_millis()),
                status_str,
                rollout_status_str,
                now,
                &service.service_id,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn get_service(&self, service_id: &str) -> Result<Option<Service>, ValkvorkError> {
        let reader = self.reader()?;
        
        // This is a simplified implementation
        // In practice, we'd need to reconstruct the full Service object
        unimplemented!()
    }
    
    pub fn list_services(&self) -> Result<Vec<Service>, ValkvorkError> {
        let reader = self.reader()?;
        
        // This is a simplified implementation
        unimplemented!()
    }
    
    pub fn delete_service(&self, service_id: &str) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        
        writer.execute(
            "UPDATE services SET status = 'DELETING', updated_at = ? WHERE service_id = ?",
            params![now, service_id],
        )?;
        
        Ok(())
    }
}

// ============================================================================
// Workload Operations
// ============================================================================

impl DatabasePool {
    pub fn create_workload(&self, workload: &Workload) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        let desired_state_str = match workload.desired_state {
            DesiredState::Run => "RUN",
            DesiredState::Stop => "STOP",
        };
        let status_str = match workload.status {
            WorkloadStatus::Pending => "PENDING",
            WorkloadStatus::Assigned => "ASSIGNED",
            WorkloadStatus::Running => "RUNNING",
            WorkloadStatus::Stopping => "STOPPING",
            WorkloadStatus::Lost => "LOST",
            WorkloadStatus::Stopped => "STOPPED",
            WorkloadStatus::Failed => "FAILED",
        };
        
        writer.execute(
            "INSERT INTO workloads (workload_id, service_id, service_version, node_id, container_id, desired_state, status, status_reason, replaces_workload_id, restart_count, created_at, started_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &workload.workload_id,
                &workload.service_id,
                workload.service_version as i64,
                workload.node_id.as_deref(),
                workload.container_id.as_deref(),
                desired_state_str,
                status_str,
                workload.status_reason.as_deref(),
                workload.replaces_workload_id.as_deref(),
                workload.restart_count as i64,
                now,
                workload.started_at.map(|t| t.timestamp_millis()),
                now,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn update_workload(&self, workload: &Workload) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        let desired_state_str = match workload.desired_state {
            DesiredState::Run => "RUN",
            DesiredState::Stop => "STOP",
        };
        let status_str = match workload.status {
            WorkloadStatus::Pending => "PENDING",
            WorkloadStatus::Assigned => "ASSIGNED",
            WorkloadStatus::Running => "RUNNING",
            WorkloadStatus::Stopping => "STOPPING",
            WorkloadStatus::Lost => "LOST",
            WorkloadStatus::Stopped => "STOPPED",
            WorkloadStatus::Failed => "FAILED",
        };
        
        writer.execute(
            "UPDATE workloads SET node_id = ?, container_id = ?, desired_state = ?, status = ?, status_reason = ?, replaces_workload_id = ?, restart_count = ?, started_at = ?, updated_at = ? WHERE workload_id = ?",
            params![
                workload.node_id.as_deref(),
                workload.container_id.as_deref(),
                desired_state_str,
                status_str,
                workload.status_reason.as_deref(),
                workload.replaces_workload_id.as_deref(),
                workload.restart_count as i64,
                workload.started_at.map(|t| t.timestamp_millis()),
                now,
                &workload.workload_id,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn list_workloads(&self) -> Result<Vec<Workload>, ValkvorkError> {
        let reader = self.reader()?;
        
        // This is a simplified implementation
        unimplemented!()
    }
    
    pub fn list_workloads_by_service(&self, service_id: &str) -> Result<Vec<Workload>, ValkvorkError> {
        let reader = self.reader()?;
        
        // This is a simplified implementation
        unimplemented!()
    }
}

// ============================================================================
// Event Operations
// ============================================================================

impl DatabasePool {
    pub fn create_cluster_event(&self, event: &ClusterEvent) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        let kind_str = match event.kind {
            EventKind::Node => "NODE",
            EventKind::Workload => "WORKLOAD",
            EventKind::Deploy => "DEPLOY",
            EventKind::Scale => "SCALE",
            EventKind::Audit => "AUDIT",
        };
        let subject_type_str = match event.subject_type {
            SubjectType::Node => "node",
            SubjectType::Service => "service",
            SubjectType::Workload => "workload",
        };
        
        writer.execute(
            "INSERT INTO cluster_events (kind, subject_type, subject_id, message, actor, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                kind_str,
                subject_type_str,
                &event.subject_id,
                &event.message,
                &event.actor,
                now,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn create_scaling_event(&self, event: &ScalingEvent) -> Result<(), ValkvorkError> {
        let writer = self.writer();
        let now = Utc::now().timestamp_millis();
        
        writer.execute(
            "INSERT INTO scaling_events (service_id, from_replicas, to_replicas, metric_value, reason, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                &event.service_id,
                event.from_replicas as i64,
                event.to_replicas as i64,
                event.metric_value,
                &event.reason,
                now,
            ],
        )?;
        
        Ok(())
    }
    
    pub fn list_cluster_events(&self, limit: usize) -> Result<Vec<ClusterEvent>, ValkvorkError> {
        let reader = self.reader()?;
        
        // This is a simplified implementation
        unimplemented!()
    }
}
