//! Cloud block volume drivers (docs/block-volumes.md).
//!
//! The core decides what should happen to each volume; the volume manager
//! (stage 2) carries it out through a [`VolumeDriver`]. Every call is safe to
//! repeat: a controller restart resumes a volume's transition by sending the
//! same call again, so drivers describe before they act.

use async_trait::async_trait;
use std::path::PathBuf;

/// A volume as the cloud reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudVolume {
    pub cloud_id: String,
    pub zone: String,
    pub size_bytes: u64,
    pub state: CloudVolumeState,
    /// The instance it is attached to, if any.
    pub attached_to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudVolumeState {
    Creating,
    Available,
    InUse,
    Deleting,
    Error,
}

/// Provisioning beyond size and class, applied at creation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeOptions {
    pub iops: Option<u32>,
    pub throughput_mibps: Option<u32>,
}

/// An instance's state, for fencing (FR-STO-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    Pending,
    Running,
    Stopping,
    Stopped,
    ShuttingDown,
    Terminated,
    /// The cloud does not know it.
    NotFound,
}

impl InstanceState {
    /// Fenced: the instance can no longer write to anything attached to it.
    pub fn is_fenced(self) -> bool {
        matches!(self, Self::Stopped | Self::Terminated | Self::NotFound)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverError(pub String);

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type Result<T> = std::result::Result<T, DriverError>;

#[async_trait]
pub trait VolumeDriver: Send + Sync + 'static {
    /// `aws-ebs` or `local`: recorded with each volume.
    fn name(&self) -> &'static str;
    /// Create, or find the one already created for `name` (idempotent).
    async fn create(
        &self,
        name: &str,
        size_bytes: u64,
        class: &str,
        zone: &str,
        opts: &VolumeOptions,
    ) -> Result<CloudVolume>;
    async fn describe(&self, cloud_id: &str) -> Result<CloudVolume>;
    /// Attach to `instance_id`; already attached there is success, attached
    /// elsewhere is an error.
    async fn attach(&self, cloud_id: &str, instance_id: &str) -> Result<()>;
    /// Detach from `instance_id`; `force` detaches even if the instance does
    /// not let go (fencing). Not attached is success.
    async fn detach(&self, cloud_id: &str, instance_id: &str, force: bool) -> Result<()>;
    /// Delete; refused while attached. Already gone is success.
    async fn delete(&self, cloud_id: &str) -> Result<()>;
    async fn instance_state(&self, instance_id: &str) -> Result<InstanceState>;
    /// For drivers without real devices (`local`): the directory the agent
    /// bind-mounts in place of the device.
    fn local_path(&self, _cloud_id: &str) -> Option<PathBuf> {
        None
    }
}

// ============================================================================
// The volume manager: carries out the core's intents
// ============================================================================

/// What the core asks for. One intent at a time per volume: the core only
/// sends one when it moves the volume into a transitional state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeOp {
    Create {
        size_bytes: u64,
        class: String,
        zone: String,
        opts: VolumeOptions,
    },
    Attach {
        cloud_id: String,
        instance_id: String,
    },
    /// The node unmounted it: detach normally.
    Detach {
        cloud_id: String,
        instance_id: String,
    },
    /// The node is gone or unreachable: force-detach (FR-STO-13). An empty
    /// instance id means "whatever the cloud says it is attached to".
    Fence {
        cloud_id: String,
        instance_id: String,
    },
    /// `valkvork volume delete`: detach from `instance_id` if attached
    /// (forced when that node is gone), then destroy the volume.
    Delete {
        cloud_id: String,
        instance_id: String,
        force: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeIntent {
    pub name: String,
    pub op: VolumeOp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeOutcome {
    Created { cloud_id: String, zone: String },
    Attached,
    Detached,
    Deleted,
}

pub type VolumeReply = std::result::Result<VolumeOutcome, String>;

pub type IntentSender = tokio::sync::mpsc::UnboundedSender<VolumeIntent>;

/// Run each intent through `driver` and report the outcome to the core as
/// `CoreMsg::VolumeResult`. Intents for different volumes run concurrently.
pub fn spawn_manager(
    driver: std::sync::Arc<dyn VolumeDriver>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<VolumeIntent>,
    core: crate::core::CoreHandle,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(intent) = rx.recv().await {
            let (driver, core) = (driver.clone(), core.clone());
            tokio::spawn(async move {
                let name = intent.name.clone();
                tracing::info!(volume = %name, op = ?intent.op, "volume operation");
                let result = carry_out(driver.as_ref(), &name, intent.op).await;
                if let Err(e) = &result {
                    tracing::warn!(volume = %name, error = %e, "volume operation failed");
                }
                let _ = core
                    .send(crate::core::CoreMsg::VolumeResult { name, result })
                    .await;
            });
        }
    })
}

pub async fn carry_out(driver: &dyn VolumeDriver, name: &str, op: VolumeOp) -> VolumeReply {
    let e = |e: DriverError| e.0;
    match op {
        VolumeOp::Create {
            size_bytes,
            class,
            zone,
            opts,
        } => {
            let v = driver
                .create(name, size_bytes, &class, &zone, &opts)
                .await
                .map_err(e)?;
            Ok(VolumeOutcome::Created {
                cloud_id: v.cloud_id,
                zone: v.zone,
            })
        }
        VolumeOp::Attach {
            cloud_id,
            instance_id,
        } => {
            driver.attach(&cloud_id, &instance_id).await.map_err(e)?;
            Ok(VolumeOutcome::Attached)
        }
        VolumeOp::Detach {
            cloud_id,
            instance_id,
        } => {
            driver
                .detach(&cloud_id, &instance_id, false)
                .await
                .map_err(e)?;
            Ok(VolumeOutcome::Detached)
        }
        VolumeOp::Fence {
            cloud_id,
            mut instance_id,
        } => {
            if instance_id.is_empty() {
                instance_id = driver
                    .describe(&cloud_id)
                    .await
                    .map_err(e)?
                    .attached_to
                    .unwrap_or_default();
            }
            if instance_id.is_empty() {
                return Ok(VolumeOutcome::Detached);
            }
            let state = driver.instance_state(&instance_id).await.map_err(e)?;
            tracing::warn!(
                cloud_id, instance = %instance_id, ?state,
                "fencing: force-detaching the volume from its previous node"
            );
            driver
                .detach(&cloud_id, &instance_id, true)
                .await
                .map_err(e)?;
            Ok(VolumeOutcome::Detached)
        }
        VolumeOp::Delete {
            cloud_id,
            instance_id,
            force,
        } => {
            if !instance_id.is_empty() {
                driver
                    .detach(&cloud_id, &instance_id, force)
                    .await
                    .map_err(e)?;
            }
            driver.delete(&cloud_id).await.map_err(e)?;
            Ok(VolumeOutcome::Deleted)
        }
    }
}

// ============================================================================
// local: directories and records, for tests and `make e2e`
// ============================================================================

/// Volumes are directories under `root/volumes/<cloud_id>`, with their zone,
/// size and attachment in `meta.json` beside the data. An instance's state is
/// the content of `root/instances/<instance_id>` (`running` when missing),
/// so a test can stop an instance by writing `stopped` there.
pub struct LocalDriver {
    root: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LocalMeta {
    zone: String,
    size_bytes: u64,
    attached_to: Option<String>,
}

impl LocalDriver {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for d in ["volumes", "instances"] {
            std::fs::create_dir_all(root.join(d)).map_err(io)?;
        }
        Ok(Self { root })
    }

    /// Where a volume's data lives: what the agent mounts in place of a
    /// block device.
    pub fn data_dir(&self, cloud_id: &str) -> PathBuf {
        self.root.join("volumes").join(cloud_id).join("data")
    }

    fn meta_path(&self, cloud_id: &str) -> PathBuf {
        self.root.join("volumes").join(cloud_id).join("meta.json")
    }

    fn read(&self, cloud_id: &str) -> Result<LocalMeta> {
        let text = std::fs::read_to_string(self.meta_path(cloud_id))
            .map_err(|_| DriverError(format!("volume {cloud_id} not found")))?;
        serde_json::from_str(&text).map_err(|e| DriverError(e.to_string()))
    }

    fn write(&self, cloud_id: &str, m: &LocalMeta) -> Result<()> {
        let tmp = self.meta_path(cloud_id).with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(m).unwrap()).map_err(io)?;
        std::fs::rename(tmp, self.meta_path(cloud_id)).map_err(io)
    }
}

fn io(e: std::io::Error) -> DriverError {
    DriverError(e.to_string())
}

#[async_trait]
impl VolumeDriver for LocalDriver {
    fn name(&self) -> &'static str {
        "local"
    }

    fn local_path(&self, cloud_id: &str) -> Option<PathBuf> {
        Some(self.data_dir(cloud_id))
    }

    async fn create(
        &self,
        name: &str,
        size_bytes: u64,
        _class: &str,
        zone: &str,
        _opts: &VolumeOptions,
    ) -> Result<CloudVolume> {
        let id = format!("vol-local-{name}");
        if self.read(&id).is_err() {
            std::fs::create_dir_all(self.data_dir(&id)).map_err(io)?;
            self.write(
                &id,
                &LocalMeta {
                    zone: zone.to_string(),
                    size_bytes,
                    attached_to: None,
                },
            )?;
        }
        self.describe(&id).await
    }

    async fn describe(&self, cloud_id: &str) -> Result<CloudVolume> {
        let m = self.read(cloud_id)?;
        Ok(CloudVolume {
            cloud_id: cloud_id.to_string(),
            zone: m.zone,
            size_bytes: m.size_bytes,
            state: if m.attached_to.is_some() {
                CloudVolumeState::InUse
            } else {
                CloudVolumeState::Available
            },
            attached_to: m.attached_to,
        })
    }

    async fn attach(&self, cloud_id: &str, instance_id: &str) -> Result<()> {
        let mut m = self.read(cloud_id)?;
        match m.attached_to.as_deref() {
            Some(i) if i == instance_id => Ok(()),
            Some(other) => Err(DriverError(format!(
                "volume {cloud_id} is attached to {other}"
            ))),
            None => {
                m.attached_to = Some(instance_id.to_string());
                self.write(cloud_id, &m)
            }
        }
    }

    async fn detach(&self, cloud_id: &str, instance_id: &str, force: bool) -> Result<()> {
        let mut m = self.read(cloud_id)?;
        match m.attached_to.as_deref() {
            None => Ok(()),
            Some(i) if i == instance_id || force => {
                m.attached_to = None;
                self.write(cloud_id, &m)
            }
            Some(other) => Err(DriverError(format!(
                "volume {cloud_id} is attached to {other}, not {instance_id}"
            ))),
        }
    }

    async fn delete(&self, cloud_id: &str) -> Result<()> {
        match self.read(cloud_id) {
            Err(_) => Ok(()),
            Ok(m) if m.attached_to.is_some() => {
                Err(DriverError(format!("volume {cloud_id} is attached")))
            }
            Ok(_) => std::fs::remove_dir_all(self.root.join("volumes").join(cloud_id)).map_err(io),
        }
    }

    async fn instance_state(&self, instance_id: &str) -> Result<InstanceState> {
        let path = self.root.join("instances").join(instance_id);
        Ok(
            match std::fs::read_to_string(path).as_deref().map(str::trim) {
                Err(_) | Ok("running") => InstanceState::Running,
                Ok("pending") => InstanceState::Pending,
                Ok("stopping") => InstanceState::Stopping,
                Ok("stopped") => InstanceState::Stopped,
                Ok("shutting-down") => InstanceState::ShuttingDown,
                Ok("terminated") => InstanceState::Terminated,
                Ok(_) => InstanceState::NotFound,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_driver_lifecycle() {
        let d = tempfile::tempdir().unwrap();
        let drv = LocalDriver::new(d.path()).unwrap();
        let v = drv
            .create(
                "orders-db",
                1 << 30,
                "gp3",
                "zone-a",
                &VolumeOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(v.cloud_id, "vol-local-orders-db");
        assert_eq!(v.state, CloudVolumeState::Available);
        // Idempotent: the same volume, with its data.
        std::fs::write(drv.data_dir(&v.cloud_id).join("row"), "1").unwrap();
        let again = drv
            .create(
                "orders-db",
                1 << 30,
                "gp3",
                "zone-a",
                &VolumeOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(again, v);
        assert!(drv.data_dir(&v.cloud_id).join("row").exists());

        drv.attach(&v.cloud_id, "i-a").await.unwrap();
        drv.attach(&v.cloud_id, "i-a").await.unwrap();
        assert!(
            drv.attach(&v.cloud_id, "i-b").await.is_err(),
            "single attachment"
        );
        assert!(drv.detach(&v.cloud_id, "i-b", false).await.is_err());
        assert!(drv.delete(&v.cloud_id).await.is_err(), "attached");
        assert_eq!(
            drv.describe(&v.cloud_id)
                .await
                .unwrap()
                .attached_to
                .as_deref(),
            Some("i-a")
        );

        // Fencing: a force-detach takes it from whoever holds it.
        drv.detach(&v.cloud_id, "i-b", true).await.unwrap();
        drv.attach(&v.cloud_id, "i-b").await.unwrap();
        drv.detach(&v.cloud_id, "i-b", false).await.unwrap();
        drv.detach(&v.cloud_id, "i-b", false).await.unwrap();
        drv.delete(&v.cloud_id).await.unwrap();
        drv.delete(&v.cloud_id).await.unwrap();
        assert!(drv.describe(&v.cloud_id).await.is_err());
    }

    #[tokio::test]
    async fn the_manager_creates_attaches_and_fences() {
        let d = tempfile::tempdir().unwrap();
        let drv = LocalDriver::new(d.path()).unwrap();
        let created = carry_out(
            &drv,
            "db",
            VolumeOp::Create {
                size_bytes: 1 << 30,
                class: "gp3".into(),
                zone: "z1".into(),
                opts: VolumeOptions::default(),
            },
        )
        .await
        .unwrap();
        let VolumeOutcome::Created { cloud_id, zone } = created else {
            panic!("{created:?}")
        };
        assert_eq!(zone, "z1");
        let attach = |i: &str| VolumeOp::Attach {
            cloud_id: cloud_id.clone(),
            instance_id: i.into(),
        };
        assert_eq!(
            carry_out(&drv, "db", attach("i-a")).await,
            Ok(VolumeOutcome::Attached)
        );
        assert!(carry_out(&drv, "db", attach("i-b")).await.is_err());
        // Fencing an unknown holder: the cloud says who it is.
        let fence = VolumeOp::Fence {
            cloud_id: cloud_id.clone(),
            instance_id: String::new(),
        };
        assert_eq!(
            carry_out(&drv, "db", fence).await,
            Ok(VolumeOutcome::Detached)
        );
        assert_eq!(
            carry_out(&drv, "db", attach("i-b")).await,
            Ok(VolumeOutcome::Attached)
        );
    }

    #[tokio::test]
    async fn local_instance_states() {
        let d = tempfile::tempdir().unwrap();
        let drv = LocalDriver::new(d.path()).unwrap();
        assert_eq!(
            drv.instance_state("i-a").await.unwrap(),
            InstanceState::Running
        );
        std::fs::write(d.path().join("instances/i-a"), "stopped\n").unwrap();
        let s = drv.instance_state("i-a").await.unwrap();
        assert!(s.is_fenced());
        assert!(!InstanceState::Running.is_fenced());
    }
}
