//! Resolve only the installed helper and retain the existing owned-process cleanup guard.

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;

use anyhow::Result;
use anyhow::ensure;
use codex_install_context::CodexPackageLayout;
use codex_utils_pty::ProcessHandle;
use codex_utils_pty::SpawnedProcess;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::Message;
use crate::encode_frame;
use crate::message_reader::MessageReader;

const DEADLINE: Duration = Duration::from_secs(/*secs*/ 5);

/// Owns one helper. Dropping it terminates the process and leaves its waiter to reap it.
/// A successful handshake establishes compatibility only, not an active audio session.
pub struct VoiceHost {
    process: ProcessHandle,
    output: MessageReader,
    exit: oneshot::Receiver<i32>,
}

impl VoiceHost {
    pub async fn connect(package: &CodexPackageLayout, build_commit: &str) -> Result<Self> {
        let root = package.package_dir.as_path().canonicalize()?;
        let name = if cfg!(windows) {
            "codex-voice-host.exe"
        } else {
            "codex-voice-host"
        };
        let path = root.join("codex-resources/voice/bin").join(name);
        ensure!(
            path.canonicalize()? == path,
            "voice helper must be inside the physical package"
        );
        let environment = child_environment(std::env::vars_os());
        let SpawnedProcess {
            session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = codex_utils_pty::spawn_pipe_process(
            &path,
            &[],
            &root,
            &environment,
            /*arg0*/ &None,
            &[],
        )
        .await?;
        drop(stderr_rx); // Drain and discard diagnostics rather than logging untyped child output.
        let mut host = Self {
            process: session,
            output: MessageReader::new(stdout_rx),
            exit: exit_rx,
        };
        host.exchange(
            Message::Hello {
                protocol: 1,
                build_commit: build_commit.to_owned(),
            },
            Message::Ready {},
        )
        .await?;
        Ok(host)
    }

    pub async fn close(mut self) -> Result<()> {
        let result = self.exchange(Message::Close {}, Message::Closed {}).await;
        if result.is_err() {
            self.process.terminate();
        }
        let code = timeout(DEADLINE, &mut self.exit).await??;
        result?;
        ensure!(code == 0, "voice helper failed during shutdown");
        Ok(())
    }

    async fn exchange(&mut self, request: Message, expected: Message) -> Result<()> {
        timeout(DEADLINE, async {
            self.process
                .writer_sender()
                .send(encode_frame(&request)?)
                .await
                .map_err(|_| anyhow::anyhow!("voice helper input closed"))?;
            let response = self.output.next().await?;
            ensure!(response == expected, "unexpected voice helper response");
            Ok(())
        })
        .await?
    }
}

fn child_environment(vars: impl Iterator<Item = (OsString, OsString)>) -> HashMap<String, String> {
    vars.filter_map(|(key, value)| {
        let key = key.into_string().ok()?;
        matches!(
            key.to_ascii_uppercase().as_str(),
            "SYSTEMROOT"
                | "WINDIR"
                | "HOME"
                | "USERPROFILE"
                | "LOCALAPPDATA"
                | "APPDATA"
                | "TEMP"
                | "TMP"
                | "TMPDIR"
                | "XDG_RUNTIME_DIR"
                | "PULSE_SERVER"
                | "PULSE_COOKIE"
                | "PIPEWIRE_REMOTE"
                | "DBUS_SESSION_BUS_ADDRESS"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "REQUESTS_CA_BUNDLE"
                | "CURL_CA_BUNDLE"
        )
        .then(|| Some((key, value.into_string().ok()?)))
        .flatten()
    })
    .collect()
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
