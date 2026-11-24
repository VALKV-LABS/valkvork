//! Secrets delivered as files (`[secrets] NAME = { ref = "...", file = true }`).
//!
//! The agent mounts one in-memory filesystem at `<data_dir>/secrets` (root
//! `0700`, `nosuid,nodev,noexec`) and gives each workload a directory in it,
//! bind-mounted read-only at `/run/secrets` in its container. Each value is a
//! file named after the secret, with the manifest's owner and mode. So a value
//! is never part of the container's definition (`docker inspect`, Docker's
//! `config.v2.json` on disk), never in the process environment, and never
//! written to a disk. Files are replaced atomically (written beside, then
//! renamed), so a reader sees the old value or the new one, never part of one.

use std::path::{Path, PathBuf};

/// Where file secrets appear in the container.
pub const MOUNT_TARGET: &str = "/run/secrets";
/// Containers with file secrets carry this label: a JSON list of
/// [`LabelEntry`] (names and references only, never values), so the refresh
/// loop finds them after an agent restart too.
pub const LABEL_SECRET_FILES: &str = "valkvork.secret_files";
/// Size of the in-memory filesystem holding every workload's secrets.
const TMPFS_SIZE: &str = "16m";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelEntry {
    pub name: String,
    pub reference: String,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

pub struct SecretFiles {
    root: PathBuf,
    /// Whether `root` is an in-memory filesystem (false only where the agent
    /// cannot mount one: development on macOS, or inside a container).
    pub in_memory: bool,
}

impl SecretFiles {
    /// Create `<data_dir>/secrets` and, with `tmpfs`, mount an in-memory
    /// filesystem there unless one already is (an agent restart).
    pub fn prepare(data_dir: &Path, tmpfs: bool) -> Result<Self, String> {
        let root = data_dir.join("secrets");
        std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        set_mode(&root, 0o700)?;
        if tmpfs && !is_tmpfs_mount(&root) {
            let out = std::process::Command::new("mount")
                .args(["-t", "tmpfs", "-o"])
                .arg(format!("size={TMPFS_SIZE},mode=0700,nosuid,nodev,noexec"))
                .args(["valkvork-secrets"])
                .arg(&root)
                .output()
                .map_err(|e| format!("mount: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "cannot mount an in-memory filesystem at {}: {} (set agent.toml secrets.files_tmpfs = false to keep file secrets on disk, for development only)",
                    root.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        Ok(Self {
            root,
            in_memory: tmpfs,
        })
    }

    pub fn dir(&self, workload_id: &str) -> PathBuf {
        self.root.join(workload_id)
    }

    /// Write (or replace) one workload's files. The directory is `0755`, so
    /// the container's user can reach the files its mode allows.
    pub fn write(&self, workload_id: &str, entries: &[(&LabelEntry, &str)]) -> Result<(), String> {
        let dir = self.dir(workload_id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        set_mode(&dir, 0o755)?;
        for (e, value) in entries {
            self.write_one(&dir, e, value)?;
        }
        Ok(())
    }

    /// Replace one file if its content differs; returns whether it changed.
    pub fn update(&self, workload_id: &str, e: &LabelEntry, value: &str) -> Result<bool, String> {
        let dir = self.dir(workload_id);
        let path = dir.join(&e.name);
        if std::fs::read(&path).is_ok_and(|old| old == value.as_bytes()) {
            return Ok(false);
        }
        self.write_one(&dir, e, value)?;
        Ok(true)
    }

    fn write_one(&self, dir: &Path, e: &LabelEntry, value: &str) -> Result<(), String> {
        use std::io::Write;
        let path = dir.join(&e.name);
        let tmp = dir.join(format!(".{}.tmp", e.name));
        let err = |x: std::io::Error| format!("secret file {}: {x}", e.name);
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(err)?;
        f.write_all(value.as_bytes()).map_err(err)?;
        drop(f);
        #[cfg(unix)]
        {
            // Owner first, then mode: chown clears setuid bits, and the file
            // must not be readable by others before it has its owner.
            let me = current_ids();
            if (e.uid, e.gid) != me {
                if me.0 == 0 {
                    std::os::unix::fs::chown(&tmp, Some(e.uid), Some(e.gid)).map_err(err)?;
                } else if (e.uid, e.gid) != (0, 0) {
                    // An unprivileged agent (development) can't give files away;
                    // root, the default owner, stays the agent's own user.
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!(
                        "secret file {}: owner {}:{} needs the agent to run as root",
                        e.name, e.uid, e.gid
                    ));
                }
            }
            set_mode(&tmp, e.mode)?;
        }
        std::fs::rename(&tmp, &path).map_err(err)?;
        Ok(())
    }

    pub fn remove(&self, workload_id: &str) {
        let _ = std::fs::remove_dir_all(self.dir(workload_id));
    }
}

#[cfg(unix)]
fn current_ids() -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    // The agent's own ids: a file it creates has them, so chown is a no-op
    // (and unprivileged development runs never need it).
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(std::env::temp_dir()))
        .map(|m| (m.uid(), m.gid()))
        .unwrap_or((u32::MAX, u32::MAX))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether `path` is itself the mount point of a tmpfs (Linux mountinfo).
fn is_tmpfs_mount(path: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let want = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    info.lines().any(|l| {
        // "<id> <parent> <dev> <root> <mount point> <opts> ... - <fstype> ..."
        let mut f = l.split(' ');
        let mount_point = f.nth(4);
        let fstype = l.split(" - ").nth(1).and_then(|r| r.split(' ').next());
        mount_point.is_some_and(|m| Path::new(m) == want) && fstype == Some("tmpfs")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, mode: u32) -> LabelEntry {
        let (uid, gid) = current_ids();
        LabelEntry {
            name: name.into(),
            reference: format!("ssm:/x/{name}"),
            uid,
            gid,
            mode,
        }
    }

    #[test]
    fn writes_replaces_and_removes() {
        let d = tempfile::tempdir().unwrap();
        let sf = SecretFiles::prepare(d.path(), false).unwrap();
        let (a, b) = (entry("DB_PASSWORD", 0o400), entry("API_KEY", 0o444));
        sf.write("w1", &[(&a, "hunter2"), (&b, "k1")]).unwrap();
        let dir = sf.dir("w1");
        assert_eq!(
            std::fs::read_to_string(dir.join("DB_PASSWORD")).unwrap(),
            "hunter2"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join("DB_PASSWORD")), 0o400);
            assert_eq!(mode(&dir.join("API_KEY")), 0o444);
            assert_eq!(mode(&dir), 0o755);
            assert_eq!(mode(&d.path().join("secrets")), 0o700);
        }
        assert!(!sf.update("w1", &b, "k1").unwrap(), "unchanged");
        assert!(sf.update("w1", &b, "k2").unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("API_KEY")).unwrap(), "k2");
        assert!(!dir.join(".API_KEY.tmp").exists());
        sf.remove("w1");
        assert!(!dir.exists());
    }

    #[test]
    fn label_round_trips() {
        let e = vec![entry("A", 0o400)];
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("hunter2"));
        let back: Vec<LabelEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }
}
