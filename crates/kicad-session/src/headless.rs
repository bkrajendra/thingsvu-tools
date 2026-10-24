// SPDX-License-Identifier: GPL-3.0-or-later
//! The headless KiCad API server, as a child process this process owns.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use kicad_domain::{ErrorCode, ToolError};
use kicad_ipc::proto::kiapi::common::commands::Ping;
use kicad_ipc::Client;
use tokio::process::{Child, Command};

pub(crate) struct HeadlessServer {
    child: Child,
    socket: PathBuf,
    #[cfg(windows)]
    _job: crate::job::KillOnCloseJob,
}

impl HeadlessServer {
    /// Starts `kicad-cli api-server --socket <socket>` and waits until it answers a `Ping`.
    pub async fn start(
        cli: &Path,
        socket: PathBuf,
        client_name: &str,
        start_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<(Self, Client), ToolError> {
        let mut command = Command::new(cli);
        command
            .arg("api-server")
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let child = command.spawn().map_err(|e| {
            ToolError::new(
                ErrorCode::KicadNotFound,
                format!("could not start {} api-server: {e}", cli.display()),
            )
        })?;

        #[cfg(windows)]
        let job = {
            let job = crate::job::KillOnCloseJob::new()
                .map_err(|e| ToolError::new(ErrorCode::KicadError, format!("could not create job object: {e}")))?;
            if let Some(handle) = child.raw_handle() {
                job.assign(handle).map_err(|e| {
                    ToolError::new(
                        ErrorCode::KicadError,
                        format!("could not tie the KiCad server to this process: {e}"),
                    )
                })?;
            }
            job
        };
        let mut server = Self {
            child,
            socket,
            #[cfg(windows)]
            _job: job,
        };

        // The server prints nothing when its output is redirected: poll until it answers.
        let deadline = Instant::now() + start_timeout;
        let mut last_error = String::from("not reachable yet");
        while Instant::now() < deadline {
            if let Ok(Some(status)) = server.child.try_wait() {
                return Err(ToolError::new(
                    ErrorCode::KicadError,
                    format!("kicad-cli api-server exited during startup ({status})"),
                ));
            }
            match Client::connect(&server.socket, client_name, request_timeout).await {
                Ok(mut client) => match client.call_any(&Ping {}).await {
                    Ok(_) => return Ok((server, client)),
                    Err(e) => last_error = e.to_string(),
                },
                Err(e) => last_error = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        server.stop().await;
        Err(ToolError::new(
            ErrorCode::KicadError,
            format!("kicad-cli api-server did not become ready within {start_timeout:?}: {last_error}"),
        )
        .retryable())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
    }
}
