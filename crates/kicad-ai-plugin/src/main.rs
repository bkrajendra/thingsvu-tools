// SPDX-License-Identifier: GPL-3.0-or-later
//! KiCad 11 executable plugin for the KiCad AI tools.
//!
//! KiCad runs this from a toolbar action with `KICAD_API_SOCKET` and `KICAD_API_TOKEN` set for the
//! editor instance that launched it. `register` checks that the instance answers, then records it
//! (see `kicad_session::registry`) so `kicad-ai-mcp` can connect to exactly this KiCad. The
//! process exits right away; nothing keeps running inside or beside KiCad.
//!
//! KiCad reports an action only when it fails (non-zero exit, with stderr as detail), and shows
//! stderr after a success as an error. So failures go to stderr with exit code 1, and success is
//! confirmed with a small message box (see `notify`) plus a line on stdout.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kicad_ipc::proto::kiapi::common::commands::{
    GetOpenDocuments, GetOpenDocumentsResponse, GetVersion, GetVersionResponse,
};
use kicad_ipc::proto::kiapi::common::types::DocumentType;
use kicad_ipc::Client;
use kicad_session::registry::{self, RegisteredDocument, Registration};

const USAGE: &str = "usage: kicad-ai-plugin [register|unregister|list]

Run by KiCad from the Schematic or PCB Editor toolbar:
  register     record this KiCad instance for the KiCad AI tools (default)
  unregister   remove this KiCad instance's record
  list         print the recorded instances (for diagnostics)";

fn main() {
    let command = std::env::args().nth(1).unwrap_or_else(|| "register".into());
    let result = match command.as_str() {
        "register" => run(register()),
        "unregister" => unregister(),
        "list" => {
            list();
            Ok(())
        }
        "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n{USAGE}")),
    };
    if let Err(message) = result {
        // KiCad shows a failing action's stderr as the error detail; stdout is not shown.
        eprintln!("KiCad AI: {message}");
        std::process::exit(1);
    }
}

fn run<F: std::future::Future<Output = Result<(), String>>>(future: F) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(future)
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .map_err(|_| format!("{name} is not set; run this from a KiCad 11 editor toolbar button, not by hand"))
}

async fn register() -> Result<(), String> {
    let socket = registry::socket_path_from_env(&env("KICAD_API_SOCKET")?);
    let token = env("KICAD_API_TOKEN")?;
    let mut client = Client::connect(&socket, "thingsvu-kicad-ai-plugin", Duration::from_secs(10))
        .await
        .map_err(|e| format!("cannot reach KiCad: {e}"))?;
    client.pin_instance_token(token.clone());
    let version: GetVersionResponse = client
        .call(&GetVersion {})
        .await
        .map_err(|e| format!("KiCad did not answer: {e}"))?;
    let kicad_version = version.version.unwrap_or_default().full_version;

    let mut documents = Vec::new();
    for (kind, doc_type) in [
        ("schematic", DocumentType::DoctypeSchematic),
        ("pcb", DocumentType::DoctypePcb),
    ] {
        // An editor type with nothing open answers "unhandled"; that is not an error here.
        if let Ok(r) = client
            .call::<_, GetOpenDocumentsResponse>(&GetOpenDocuments {
                r#type: doc_type as i32,
            })
            .await
        {
            documents.extend(r.documents.into_iter().map(|d| RegisteredDocument {
                kind: kind.into(),
                project: d.project.as_ref().map(|p| p.name.clone()),
                project_path: d.project.as_ref().map(|p| p.path.clone()),
            }));
        }
    }

    let registration = Registration {
        socket_path: socket,
        token,
        kicad_version: kicad_version.clone(),
        registered_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        documents: documents.clone(),
        plugin_version: env!("CARGO_PKG_VERSION").into(),
    };
    registry::write(&registry::registry_dir(), &registration)
        .map_err(|e| format!("cannot save the registration: {e}"))?;

    let projects: Vec<String> = documents.iter().filter_map(|d| d.project.clone()).collect();
    let what = if projects.is_empty() {
        "no open project".to_owned()
    } else {
        projects.join(", ")
    };
    notify(&format!(
        "Connected. AI tools can now work with KiCad {kicad_version} ({what})."
    ));
    Ok(())
}

fn unregister() -> Result<(), String> {
    let socket = registry::socket_path_from_env(&env("KICAD_API_SOCKET")?);
    match registry::remove(&registry::registry_dir(), &socket) {
        Ok(true) => notify("Disconnected. AI tools can no longer use this KiCad."),
        Ok(false) => notify("This KiCad was not connected."),
        Err(e) => return Err(format!("cannot remove the registration: {e}")),
    }
    Ok(())
}

fn list() {
    let dir = registry::registry_dir();
    println!("registry: {}", dir.display());
    for r in registry::list(&dir) {
        let projects: Vec<_> = r.documents.iter().filter_map(|d| d.project.clone()).collect();
        println!(
            "KiCad {} at {} — {:?}",
            r.kicad_version,
            r.socket_path.display(),
            projects
        );
    }
}

/// Tells the user an action succeeded. KiCad shows nothing for a successful plugin action, so on
/// Windows a small message box confirms it (it blocks only this process, not KiCad). Set
/// `THINGSVU_NO_DIALOG` to skip the box, e.g. in tests. The text always goes to stdout too.
fn notify(message: &str) {
    println!("KiCad AI: {message}");
    #[cfg(windows)]
    if std::env::var_os("THINGSVU_NO_DIALOG").is_none() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
        };
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(message), wide("KiCad AI"));
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
            );
        }
    }
}
