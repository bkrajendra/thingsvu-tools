// SPDX-License-Identifier: GPL-3.0-or-later
//! Registrations of running KiCad GUI instances, written by the `kicad-ai-plugin` action.
//!
//! KiCad gives an executable plugin the socket and instance token of the editor that launched
//! it. The plugin records them here so the MCP server can connect to exactly that instance. The
//! token identifies the instance (KiCad refuses requests carrying another instance's token); it
//! is not an access secret, but it is still kept out of logs and tool results.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// KiCad API socket path (without the `ipc://` prefix).
    pub socket_path: PathBuf,
    pub token: String,
    pub kicad_version: String,
    /// Seconds since the Unix epoch.
    pub registered_at: u64,
    pub documents: Vec<RegisteredDocument>,
    pub plugin_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredDocument {
    /// `schematic` or `pcb`.
    pub kind: String,
    pub project: Option<String>,
    pub project_path: Option<String>,
}

/// Directory holding one JSON file per registered instance. `THINGSVU_REGISTRY_DIR` overrides it.
pub fn registry_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("THINGSVU_REGISTRY_DIR") {
        return PathBuf::from(dir);
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("thingsvu")
        .join("kicad-ai")
        .join("instances")
}

/// Folder for the MCP server's rotating log files. `THINGSVU_LOG_DIR` overrides it.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("THINGSVU_LOG_DIR") {
        return PathBuf::from(dir);
    }
    // `<state>/thingsvu/kicad-ai/instances` and `.../logs` are siblings.
    registry_dir().with_file_name("logs")
}

/// KiCad passes sockets as `ipc://<path>`.
pub fn socket_path_from_env(value: &str) -> PathBuf {
    PathBuf::from(value.strip_prefix("ipc://").unwrap_or(value))
}

fn file_for(dir: &Path, socket: &Path) -> PathBuf {
    // FNV-1a over the socket path: a stable file name per KiCad instance endpoint.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in socket.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    dir.join(format!("{hash:016x}.json"))
}

pub fn write(dir: &Path, registration: &Registration) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = file_for(dir, &registration.socket_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(registration).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn remove(dir: &Path, socket: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(file_for(dir, socket)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Every readable registration, newest first. Unreadable files are skipped.
pub fn list(dir: &Path) -> Vec<Registration> {
    let mut out: Vec<Registration> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.registered_at));
    out
}

/// Whether the registered KiCad still answers, as the same instance (token pinned).
pub async fn is_reachable(registration: &Registration, timeout: std::time::Duration) -> bool {
    use kicad_ipc::proto::kiapi::common::commands::{GetVersion, GetVersionResponse};
    let Ok(mut client) =
        kicad_ipc::Client::connect(&registration.socket_path, "thingsvu-kicad-ai-check", timeout).await
    else {
        return false;
    };
    client.pin_instance_token(registration.token.clone());
    client.call::<_, GetVersionResponse>(&GetVersion {}).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_list_remove_round_trip() {
        let dir = std::env::temp_dir().join(format!("thingsvu-registry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = Registration {
            socket_path: socket_path_from_env(r"ipc://C:\Users\me\AppData\Local\Temp\kicad\api.sock"),
            token: "t".into(),
            kicad_version: "10.99.0".into(),
            registered_at: 1,
            documents: vec![RegisteredDocument {
                kind: "schematic".into(),
                project: Some("demo".into()),
                project_path: None,
            }],
            plugin_version: "0.0.1".into(),
        };
        assert_eq!(
            reg.socket_path,
            PathBuf::from(r"C:\Users\me\AppData\Local\Temp\kicad\api.sock")
        );
        write(&dir, &reg).unwrap();
        // Re-registering the same instance replaces its record.
        write(
            &dir,
            &Registration {
                registered_at: 2,
                ..reg.clone()
            },
        )
        .unwrap();
        let listed = list(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].registered_at, 2);
        assert!(remove(&dir, &reg.socket_path).unwrap());
        assert!(!remove(&dir, &reg.socket_path).unwrap());
        assert!(list(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
