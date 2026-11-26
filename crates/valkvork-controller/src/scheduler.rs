//! Spread scheduler (FR-CON-06).
//!
//! Pure functions over a snapshot of node capacity. The core builds a
//! [`Snapshot`] once per reconcile pass and calls [`Snapshot::reserve`] after
//! each placement, so placements within one pass account for each other.

use std::collections::{BTreeMap, HashMap, HashSet};
use valkvork_common::models::{NodeStatus, ServiceSpec};

/// Per-node capacity and current reservations.
#[derive(Debug, Clone)]
pub struct NodeCapacity {
    pub node_id: String,
    pub node_class: String,
    pub healthy: bool,
    pub draining: bool,
    pub allocatable_cpu: u32,
    pub allocatable_mem: u64,
    pub host_volumes: HashSet<String>,
    /// Manifest features the node's agent supports.
    pub features: HashSet<String>,
    pub alloc_cpu: u64,
    pub alloc_mem: u64,
    pub used_ports: HashSet<u16>,
    /// service_id -> number of that service's workloads on the node.
    pub per_service: HashMap<String, u32>,
    /// Host volume name -> workloads using it, as (single-writer, all).
    pub volume_users: HashMap<String, (u32, u32)>,
    /// Availability zone; empty off a cloud.
    pub zone: String,
    pub max_block_volumes: u32,
    /// Block volumes attached here, or to be by placements in this pass.
    pub block_volumes: HashSet<String>,
}

impl NodeCapacity {
    pub fn new(
        node_id: &str,
        node_class: &str,
        status: NodeStatus,
        draining: bool,
        allocatable_cpu: u32,
        allocatable_mem: u64,
        host_volumes: &[String],
    ) -> Self {
        Self {
            node_id: node_id.to_string(),
            node_class: node_class.to_string(),
            healthy: status == NodeStatus::Healthy,
            draining,
            allocatable_cpu,
            allocatable_mem,
            host_volumes: host_volumes.iter().cloned().collect(),
            features: HashSet::new(),
            alloc_cpu: 0,
            alloc_mem: 0,
            used_ports: HashSet::new(),
            per_service: HashMap::new(),
            volume_users: HashMap::new(),
            zone: String::new(),
            max_block_volumes: 0,
            block_volumes: HashSet::new(),
        }
    }

    /// `max(cpu_ratio, mem_ratio)` after adding the given request.
    fn ratio_with(&self, cpu: u64, mem: u64) -> f64 {
        let c = (self.alloc_cpu + cpu) as f64 / self.allocatable_cpu.max(1) as f64;
        let m = (self.alloc_mem + mem) as f64 / self.allocatable_mem.max(1) as f64;
        c.max(m)
    }

    pub fn alloc_ratio(&self) -> f64 {
        self.ratio_with(0, 0)
    }
}

/// Capacity view of the whole cluster, keyed (and therefore ordered) by node id.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub nodes: BTreeMap<String, NodeCapacity>,
    /// Block volume name -> the zone it is in, or will be created in by a
    /// placement in this pass.
    pub block_zones: HashMap<String, String>,
    /// Block volume name -> live workloads using it, on any node: block
    /// volumes are single-writer cluster-wide.
    pub block_users: HashMap<String, u32>,
}

/// Why a placement failed, rendered into `status_reason`.
#[derive(Debug, Default)]
struct Rejections {
    total: usize,
    unhealthy: usize,
    draining: usize,
    class: usize,
    cpu: usize,
    mem: usize,
    port: Option<u16>,
    port_count: usize,
    volume: Option<String>,
    volume_count: usize,
    volume_busy: Option<String>,
    volume_busy_count: usize,
    feature: Option<&'static str>,
    feature_count: usize,
    no_zone: usize,
    zone: Option<(String, String)>,
    zone_count: usize,
    block_slots: usize,
}

impl NodeCapacity {
    /// The manifest features the node's agent reported.
    pub fn with_features(mut self, features: &[String]) -> Self {
        self.features = features.iter().cloned().collect();
        self
    }

    /// The zone and block-volume slots the node's agent reported.
    pub fn with_topology(mut self, zone: &str, max_block_volumes: u32) -> Self {
        self.zone = zone.to_string();
        self.max_block_volumes = max_block_volumes;
        self
    }
}

impl Snapshot {
    /// Record a reservation (a placed or already-running workload).
    pub fn reserve(&mut self, node_id: &str, spec: &ServiceSpec, holds_resources: bool) {
        for v in spec.volumes.iter().filter(|v| v.is_block()) {
            *self.block_users.entry(v.source.clone()).or_default() += 1;
            if let Some(n) = self.nodes.get_mut(node_id) {
                n.block_volumes.insert(v.source.clone());
                if !n.zone.is_empty() {
                    self.block_zones
                        .entry(v.source.clone())
                        .or_insert_with(|| n.zone.clone());
                }
            }
        }
        if let Some(n) = self.nodes.get_mut(node_id) {
            if holds_resources {
                n.alloc_cpu += spec.cpu_millicores as u64;
                n.alloc_mem += spec.memory_bytes;
                *n.per_service.entry(spec.id.clone()).or_default() += 1;
            }
            // Ports stay bound until the container is actually gone, so they
            // are reserved even for workloads that are stopping.
            n.used_ports.extend(spec.ports.iter().map(|p| p.host_port));
            // Likewise the directory is in use until the container is gone.
            for v in spec.volumes.iter().filter(|v| v.is_host()) {
                let u = n.volume_users.entry(v.source.clone()).or_default();
                u.0 += v.single_writer() as u32;
                u.1 += 1;
            }
        }
    }

    /// Choose a node for one workload of `spec`. Returns the node id or a
    /// `NO_NODE: …` reason.
    pub fn place(&self, spec: &ServiceSpec) -> Result<String, String> {
        let cpu = spec.cpu_millicores as u64;
        let mem = spec.memory_bytes;
        let mut rej = Rejections::default();
        let mut best: Option<(f64, u32, &str)> = None;
        let needs = spec.required_features();
        let blocks: Vec<&str> = spec
            .volumes
            .iter()
            .filter(|v| v.is_block())
            .map(|v| v.source.as_str())
            .collect();
        // Single-writer cluster-wide: a block volume another live workload
        // uses rules out every node.
        if let Some(v) = blocks
            .iter()
            .find(|v| self.block_users.get(**v).copied().unwrap_or(0) > 0)
        {
            return Err(format!(
                "NO_NODE: block volume {v} is in use by another workload"
            ));
        }

        for n in self.nodes.values() {
            rej.total += 1;
            if !n.healthy {
                rej.unhealthy += 1;
                continue;
            }
            if n.draining {
                rej.draining += 1;
                continue;
            }
            if let Some(class) = &spec.required_class {
                if &n.node_class != class {
                    rej.class += 1;
                    continue;
                }
            }
            if let Some(f) = needs.iter().find(|f| !n.features.contains(**f)) {
                rej.feature = Some(f);
                rej.feature_count += 1;
                continue;
            }
            if n.alloc_cpu + cpu > n.allocatable_cpu as u64 {
                rej.cpu += 1;
                continue;
            }
            if n.alloc_mem + mem > n.allocatable_mem {
                rej.mem += 1;
                continue;
            }
            if let Some(p) = spec
                .ports
                .iter()
                .find(|p| n.used_ports.contains(&p.host_port))
            {
                rej.port = Some(p.host_port);
                rej.port_count += 1;
                continue;
            }
            if let Some(v) = spec
                .volumes
                .iter()
                .find(|v| v.is_host() && !n.host_volumes.contains(&v.source))
            {
                rej.volume = Some(v.source.clone());
                rej.volume_count += 1;
                continue;
            }
            // A single-writer volume admits no other user; a shared one
            // admits no single-writer user.
            if let Some(v) = spec.volumes.iter().filter(|v| v.is_host()).find(|v| {
                let (single, all) = n.volume_users.get(&v.source).copied().unwrap_or_default();
                if v.single_writer() {
                    all > 0
                } else {
                    single > 0
                }
            }) {
                rej.volume_busy = Some(v.source.clone());
                rej.volume_busy_count += 1;
                continue;
            }
            if !blocks.is_empty() {
                if n.zone.is_empty() {
                    rej.no_zone += 1;
                    continue;
                }
                if let Some((v, z)) = blocks.iter().find_map(|v| {
                    self.block_zones
                        .get(*v)
                        .filter(|z| **z != n.zone)
                        .map(|z| (*v, z))
                }) {
                    rej.zone = Some((v.to_string(), z.clone()));
                    rej.zone_count += 1;
                    continue;
                }
                let new = blocks
                    .iter()
                    .filter(|v| !n.block_volumes.contains(**v))
                    .count();
                if n.block_volumes.len() + new > n.max_block_volumes as usize {
                    rej.block_slots += 1;
                    continue;
                }
            }
            let score = (
                n.ratio_with(cpu, mem),
                n.per_service.get(&spec.id).copied().unwrap_or(0),
                n.node_id.as_str(),
            );
            // Lowest ratio, then fewest same-service workloads, then node_id.
            let better = match &best {
                None => true,
                Some(b) => {
                    score.0 < b.0 - 1e-9
                        || ((score.0 - b.0).abs() <= 1e-9 && (score.1, score.2) < (b.1, b.2))
                }
            };
            if better {
                best = Some(score);
            }
        }

        match best {
            Some((_, _, id)) => Ok(id.to_string()),
            None => Err(reason(spec, &rej)),
        }
    }
}

fn reason(spec: &ServiceSpec, r: &Rejections) -> String {
    let class = spec
        .required_class
        .as_ref()
        .map(|c| format!(" in class {c}"))
        .unwrap_or_default();
    let detail = if r.total == 0 {
        "no nodes registered".to_string()
    } else if r.unhealthy + r.draining == r.total {
        "no healthy, non-draining nodes".to_string()
    } else if r.class + r.unhealthy + r.draining == r.total {
        format!("no healthy nodes{class}")
    } else {
        let mut parts = Vec::new();
        if r.mem > 0 {
            parts.push(format!("insufficient memory on {}", r.mem));
        }
        if r.cpu > 0 {
            parts.push(format!("insufficient cpu on {}", r.cpu));
        }
        if let Some(p) = r.port {
            parts.push(format!("host_port {p} in use on {}", r.port_count));
        }
        if let Some(v) = &r.volume {
            parts.push(format!("host volume {v} missing on {}", r.volume_count));
        }
        if let Some(f) = r.feature {
            parts.push(format!(
                "agent lacks {f} (upgrade it) on {}",
                r.feature_count
            ));
        }
        if let Some(v) = &r.volume_busy {
            parts.push(format!(
                "single-writer host volume {v} in use on {}",
                r.volume_busy_count
            ));
        }
        if let Some((v, z)) = &r.zone {
            parts.push(format!(
                "block volume {v} is in zone {z}, not that of {}",
                r.zone_count
            ));
        }
        if r.no_zone > 0 {
            parts.push(format!(
                "no zone reported (block volumes need one) by {}",
                r.no_zone
            ));
        }
        if r.block_slots > 0 {
            parts.push(format!("no free block volume slot on {}", r.block_slots));
        }
        format!("{}{class}", parts.join(", "))
    };
    format!("NO_NODE: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use valkvork_common::models::{PortSpec, RestartPolicy, UpdatePolicy, VolumeSpec};

    fn all_features() -> Vec<String> {
        valkvork_common::models::AGENT_FEATURES
            .iter()
            .map(|f| f.to_string())
            .collect()
    }

    fn spec(cpu: u32, mem_mib: u64) -> ServiceSpec {
        ServiceSpec {
            id: "svc".into(),
            service_type: Default::default(),
            image: "img:1".into(),
            entrypoint: None,
            command: None,
            replicas: 1,
            kill_timeout_seconds: 10,
            cpu_millicores: cpu,
            memory_bytes: mem_mib << 20,
            disk_bytes: 1 << 30,
            required_class: None,
            env: Default::default(),
            secrets: Default::default(),
            secret_files: Default::default(),
            ports: vec![],
            volumes: vec![],
            autoscale: None,
            update: UpdatePolicy::default(),
            restart: RestartPolicy::default(),
            check: None,
            security: None,
            logging: None,
            sysctls: Default::default(),
        }
    }

    fn snap(nodes: &[(&str, &str, u32, u64)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (id, class, cpu, mem_mib) in nodes {
            s.nodes.insert(
                id.to_string(),
                NodeCapacity::new(
                    id,
                    class,
                    NodeStatus::Healthy,
                    false,
                    *cpu,
                    mem_mib << 20,
                    &[],
                )
                .with_features(&all_features()),
            );
        }
        s
    }

    #[test]
    fn spreads_across_nodes() {
        let mut s = snap(&[
            ("a", "g", 2000, 4096),
            ("b", "g", 2000, 4096),
            ("c", "g", 2000, 4096),
        ]);
        let sp = spec(500, 512);
        let mut placed = vec![];
        for _ in 0..3 {
            let n = s.place(&sp).unwrap();
            s.reserve(&n, &sp, true);
            placed.push(n);
        }
        placed.sort();
        assert_eq!(placed, vec!["a", "b", "c"]);
    }

    #[test]
    fn least_allocated_wins() {
        let mut s = snap(&[("a", "g", 2000, 4096), ("b", "g", 2000, 4096)]);
        s.reserve("a", &spec(1000, 100), true);
        assert_eq!(s.place(&spec(100, 100)).unwrap(), "b");
    }

    #[test]
    fn filters() {
        let mut s = snap(&[
            ("a", "general", 1000, 1024),
            ("b", "high-perf", 8000, 16384),
        ]);
        let mut sp = spec(4000, 8192);
        sp.required_class = Some("general".into());
        let err = s.place(&sp).unwrap_err();
        assert!(err.starts_with("NO_NODE"), "{err}");
        assert!(err.contains("in class general"), "{err}");

        sp.required_class = Some("high-perf".into());
        assert_eq!(s.place(&sp).unwrap(), "b");

        // Static host port: one per node.
        let mut p = spec(10, 10);
        p.ports.push(PortSpec {
            name: "http".into(),
            container_port: 80,
            host_port: 8080,
            host_ip: None,
        });
        s.reserve("a", &p, true);
        s.reserve("b", &p, true);
        assert!(s.place(&p).unwrap_err().contains("host_port 8080"));

        // Host volume must exist.
        let mut v = spec(10, 10);
        v.volumes.push(VolumeSpec {
            kind: "host".into(),
            source: "pgdata".into(),
            target: "/d".into(),
            read_only: false,
            shared: false,
            size_bytes: None,
            class: None,
            iops: None,
            throughput_mibps: None,
        });
        assert!(s.place(&v).unwrap_err().contains("pgdata"));
        s.nodes
            .get_mut("a")
            .unwrap()
            .host_volumes
            .insert("pgdata".into());
        assert_eq!(s.place(&v).unwrap(), "a");

        // Single-writer: a second user of a's pgdata is refused, even while
        // the first only holds the directory (stopping).
        s.reserve("a", &v, false);
        assert!(s
            .place(&v)
            .unwrap_err()
            .contains("single-writer host volume pgdata in use on 1"));
        // Shared users may coexist with each other, not with a single writer.
        let mut shared = v.clone();
        shared.volumes[0].shared = true;
        assert!(s.place(&shared).is_err());
        s.nodes
            .get_mut("b")
            .unwrap()
            .host_volumes
            .insert("pgdata".into());
        s.reserve("b", &shared, true);
        assert_eq!(s.place(&shared).unwrap(), "b");
        assert!(s.place(&v).is_err(), "b has a shared user, a a writer");
    }

    #[test]
    fn unhealthy_and_draining_excluded() {
        let mut s = snap(&[("a", "g", 2000, 4096), ("b", "g", 2000, 4096)]);
        s.nodes.get_mut("a").unwrap().healthy = false;
        s.nodes.get_mut("b").unwrap().draining = true;
        assert_eq!(
            s.place(&spec(1, 4)).unwrap_err(),
            "NO_NODE: no healthy, non-draining nodes"
        );
    }

    /// An ephemeral volume needs nothing from the node and blocks no one.
    #[test]
    fn ephemeral_volume_places_anywhere() {
        let mut s = snap(&[("a", "general", 4000, 4096)]);
        let mut v = spec(10, 10);
        v.volumes.push(VolumeSpec {
            kind: "ephemeral".into(),
            source: "cache".into(),
            target: "/c".into(),
            read_only: false,
            shared: false,
            size_bytes: None,
            class: None,
            iops: None,
            throughput_mibps: None,
        });
        assert_eq!(s.place(&v).unwrap(), "a");
        s.reserve("a", &v, true);
        assert_eq!(
            s.place(&v).unwrap(),
            "a",
            "a second replica on the same node"
        );
    }

    fn block(name: &str) -> VolumeSpec {
        VolumeSpec {
            kind: "block".into(),
            source: name.into(),
            target: "/data".into(),
            read_only: false,
            shared: false,
            size_bytes: Some(10 << 30),
            class: Some("gp3".into()),
            iops: None,
            throughput_mibps: None,
        }
    }

    /// Nodes in zones, able to take block volumes.
    fn zoned(nodes: &[(&str, &str, u32)]) -> Snapshot {
        let mut s = Snapshot::default();
        let mut f = all_features();
        f.push("block_volumes".into());
        for (id, zone, max) in nodes {
            s.nodes.insert(
                id.to_string(),
                NodeCapacity::new(
                    id,
                    "general",
                    NodeStatus::Healthy,
                    false,
                    4000,
                    8 << 30,
                    &[],
                )
                .with_features(&f)
                .with_topology(zone, *max),
            );
        }
        s
    }

    #[test]
    fn block_volume_needs_the_feature_and_a_zone() {
        let mut v = spec(100, 64);
        v.volumes.push(block("db"));
        let s = snap(&[("a", "general", 1000, 1024)]);
        assert!(s
            .place(&v)
            .unwrap_err()
            .contains("agent lacks block_volumes"));
        let s = zoned(&[("a", "", 20)]);
        assert!(s.place(&v).unwrap_err().contains("no zone reported"));
    }

    #[test]
    fn block_volume_stays_in_its_zone() {
        let mut v = spec(100, 64);
        v.volumes.push(block("db"));
        let mut s = zoned(&[("a", "z1", 20), ("b", "z2", 20)]);
        s.block_zones.insert("db".into(), "z2".into());
        assert_eq!(s.place(&v).unwrap(), "b");
        s.nodes.get_mut("b").unwrap().healthy = false;
        let e = s.place(&v).unwrap_err();
        assert!(e.contains("block volume db is in zone z2"), "{e}");
    }

    #[test]
    fn a_new_block_volume_takes_the_zone_of_its_first_placement() {
        let mut v = spec(100, 64);
        v.volumes.push(block("db"));
        let mut s = zoned(&[("a", "z1", 20), ("b", "z2", 20)]);
        let first = s.place(&v).unwrap();
        s.reserve(&first, &v, true);
        assert_eq!(s.block_zones["db"], s.nodes[&first].zone);
    }

    #[test]
    fn block_volume_is_single_writer_across_nodes() {
        let mut v = spec(100, 64);
        v.volumes.push(block("db"));
        let mut s = zoned(&[("a", "z1", 20), ("b", "z1", 20)]);
        let first = s.place(&v).unwrap();
        s.reserve(&first, &v, true);
        let e = s.place(&v).unwrap_err();
        assert!(
            e.contains("block volume db is in use by another workload"),
            "{e}"
        );
        // Another service with another volume still places.
        let mut w = spec(100, 64);
        w.id = "other".into();
        w.volumes.push(block("cache"));
        assert!(s.place(&w).is_ok());
    }

    #[test]
    fn block_volume_slots_are_counted_per_node() {
        let mut v = spec(100, 64);
        v.volumes.push(block("db"));
        let mut s = zoned(&[("a", "z1", 1), ("b", "z1", 1)]);
        // a holds another volume (attached, unused): no slot left there.
        s.nodes
            .get_mut("a")
            .unwrap()
            .block_volumes
            .insert("old".into());
        assert_eq!(s.place(&v).unwrap(), "b");
        s.nodes
            .get_mut("b")
            .unwrap()
            .block_volumes
            .insert("other".into());
        let e = s.place(&v).unwrap_err();
        assert!(e.contains("no free block volume slot on 2"), "{e}");
        // A volume already attached to a node needs no new slot there.
        s.nodes.get_mut("a").unwrap().block_volumes = ["db".to_string()].into();
        assert_eq!(s.place(&v).unwrap(), "a");
    }
}
