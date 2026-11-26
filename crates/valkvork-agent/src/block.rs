//! Block devices for cloud block volumes (docs/block-volumes.md, stage 3).
//!
//! The controller attaches an EBS volume to this instance before it sends
//! `StartWorkload`; the agent then finds the device by the volume's id,
//! formats it only if it holds no filesystem, mounts it at
//! `<data_dir>/block/<name>`, and bind-mounts that into the container. After
//! the last container using it is gone, it unmounts it. A volume counts as
//! in use while it is mounted, so the controller never detaches a mounted
//! filesystem.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

pub struct BlockDevices {
    /// `<data_dir>/block`: one mount point per volume name.
    root: PathBuf,
    /// Where devices and their sysfs entries are (`/`, or a test tree).
    dev_root: PathBuf,
    /// How long an attached device may take to appear.
    wait: Duration,
    pub fs: String,
    /// Serialises mounts and unmounts: a start on the same node must not
    /// reuse a mount that the previous workload's stop is taking down.
    lock: tokio::sync::Mutex<()>,
}

impl BlockDevices {
    pub fn new(data_dir: &Path, fs: &str) -> Result<Self, String> {
        let root = data_dir.join("block");
        std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        Ok(Self {
            root,
            dev_root: PathBuf::from("/"),
            wait: Duration::from_secs(60),
            fs: fs.to_string(),
            lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn mount_point(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The device of EBS volume `cloud_id`: the Nitro NVMe by-id link, or
    /// an NVMe controller whose serial is the id (without the dash).
    pub fn device_for(&self, cloud_id: &str) -> Option<PathBuf> {
        let serial = cloud_id.replace('-', "");
        let by_id = self
            .dev_root
            .join("dev/disk/by-id")
            .join(format!("nvme-Amazon_Elastic_Block_Store_{serial}"));
        if by_id.exists() {
            return Some(std::fs::canonicalize(&by_id).unwrap_or(by_id));
        }
        let sys = self.dev_root.join("sys/block");
        for e in std::fs::read_dir(&sys).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("nvme") {
                continue;
            }
            let s = std::fs::read_to_string(e.path().join("device/serial")).unwrap_or_default();
            if s.trim() == serial {
                return Some(self.dev_root.join("dev").join(name));
            }
        }
        None
    }

    /// Wait for the device, format it if blank, and mount it; returns the
    /// mount point. Already mounted there (an agent restart) is fine.
    pub async fn mount(&self, name: &str, cloud_id: &str) -> Result<PathBuf, String> {
        let _serial = self.lock.lock().await;
        let deadline = tokio::time::Instant::now() + self.wait;
        let dev = loop {
            if let Some(d) = self.device_for(cloud_id) {
                break d;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "the device of {cloud_id} did not appear within {} s",
                    self.wait.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        let mnt = self.mount_point(name);
        let mounts = mountinfo();
        if let Some((_, src)) = mounts.iter().find(|(m, _)| *m == mnt) {
            if same_device(src, &dev) {
                return Ok(mnt);
            }
            return Err(format!("{} is already a mount of {src}", mnt.display()));
        }
        if let Some((m, _)) = mounts.iter().find(|(_, s)| same_device(s, &dev)) {
            return Err(format!(
                "{} is mounted elsewhere, at {}",
                dev.display(),
                m.display()
            ));
        }
        if fs_type(&dev).await?.is_none() {
            tracing::info!(volume = name, device = %dev.display(), fs = %self.fs, "formatting a blank volume");
            run(
                "mkfs",
                &[&format!("-t{}", self.fs), "-q", &dev.to_string_lossy()],
            )
            .await?;
        }
        std::fs::create_dir_all(&mnt).map_err(|e| format!("{}: {e}", mnt.display()))?;
        run(
            "mount",
            &[
                "-o",
                "noatime",
                &dev.to_string_lossy(),
                &mnt.to_string_lossy(),
            ],
        )
        .await?;
        tracing::info!(volume = name, device = %dev.display(), mount = %mnt.display(), "mounted block volume");
        Ok(mnt)
    }

    /// Unmount `name` unless `still_used()` (checked under the lock, so a
    /// start that arrived meanwhile keeps it). A busy filesystem stays
    /// mounted, and in use: something on the node still has it open.
    pub async fn release(&self, name: &str, still_used: impl Fn() -> bool) -> Result<(), String> {
        let _serial = self.lock.lock().await;
        if still_used() {
            return Ok(());
        }
        self.unmount_locked(name).await
    }

    async fn unmount_locked(&self, name: &str) -> Result<(), String> {
        let mnt = self.mount_point(name);
        if !mountinfo().iter().any(|(m, _)| *m == mnt) {
            return Ok(());
        }
        run("umount", &[&mnt.to_string_lossy()]).await?;
        let _ = std::fs::remove_dir(&mnt);
        tracing::info!(volume = name, "unmounted block volume");
        Ok(())
    }

    /// Volume names mounted under `<data_dir>/block`.
    pub fn mounted(&self) -> Vec<String> {
        mountinfo()
            .into_iter()
            .filter_map(|(m, _)| {
                (m.parent() == Some(self.root.as_path()))
                    .then(|| m.file_name().map(|n| n.to_string_lossy().to_string()))
                    .flatten()
            })
            .collect()
    }
}

/// The filesystem on `dev`, or None if blank (`blkid` exits 2 then).
async fn fs_type(dev: &Path) -> Result<Option<String>, String> {
    let out = Command::new("blkid")
        .args(["-p", "-o", "value", "-s", "TYPE"])
        .arg(dev)
        .output()
        .await
        .map_err(|e| format!("blkid: {e}"))?;
    match out.status.code() {
        Some(0) => {
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Ok((!t.is_empty()).then_some(t))
        }
        Some(2) => Ok(None),
        _ => Err(format!(
            "blkid {}: {}",
            dev.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

async fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// (mount point, source) for every mount, from /proc/self/mountinfo.
fn mountinfo() -> Vec<(PathBuf, String)> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mount_point = l.split(' ').nth(4)?;
            let source = l.split(" - ").nth(1)?.split(' ').nth(1)?;
            Some((PathBuf::from(unescape(mount_point)), source.to_string()))
        })
        .collect()
}

/// mountinfo escapes spaces and the like as `\\040`.
fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn same_device(source: &str, dev: &Path) -> bool {
    let a = std::fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
    let b = std::fs::canonicalize(dev).unwrap_or_else(|_| dev.to_path_buf());
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_device_by_id_or_serial() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("dev/disk/by-id")).unwrap();
        std::fs::create_dir_all(root.join("sys/block/nvme2n1/device")).unwrap();
        std::fs::write(
            root.join("sys/block/nvme2n1/device/serial"),
            "vol0abc123  \n",
        )
        .unwrap();
        std::fs::write(root.join("dev/nvme1n1"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.join("dev/nvme1n1"),
            root.join("dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol0def456"),
        )
        .unwrap();
        let b = BlockDevices {
            root: root.join("block"),
            dev_root: root.to_path_buf(),
            wait: Duration::from_millis(10),
            fs: "ext4".into(),
            lock: tokio::sync::Mutex::new(()),
        };
        #[cfg(unix)]
        assert_eq!(
            b.device_for("vol-0def456").unwrap(),
            std::fs::canonicalize(root.join("dev/nvme1n1")).unwrap()
        );
        assert_eq!(
            b.device_for("vol-0abc123").unwrap(),
            root.join("dev/nvme2n1")
        );
        assert!(b.device_for("vol-0999").is_none());
    }

    #[tokio::test]
    async fn a_missing_device_fails_after_the_wait() {
        let d = tempfile::tempdir().unwrap();
        let b = BlockDevices {
            root: d.path().join("block"),
            dev_root: d.path().to_path_buf(),
            wait: Duration::from_millis(20),
            fs: "ext4".into(),
            lock: tokio::sync::Mutex::new(()),
        };
        let e = b.mount("db", "vol-0missing").await.unwrap_err();
        assert!(e.contains("did not appear"), "{e}");
    }

    #[test]
    fn unescapes_mount_points() {
        assert_eq!(unescape("/a\\040b"), "/a b");
    }
}
