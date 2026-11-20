//! Device authentication for Linux desktop clients.

use std::{io, process::Stdio, time::Duration};

pub struct Subject {
    pid: u32,
    start_time: u64,
    uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("System authentication was canceled.")]
    Canceled,
    #[error("System authentication was denied.")]
    Denied,
    #[error("System authentication timed out.")]
    TimedOut,
    #[error(
        "System authentication is unavailable. Check that the polkit policy and a desktop authentication agent are installed."
    )]
    Unavailable,
}

impl Subject {
    /// Capture the process start time alongside its OS supplied PID and UID.
    pub fn new(pid: u32, uid: u32) -> Result<Self, AuthError> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| AuthError::Unavailable)?;
        // Field 2 (comm) can contain spaces and parentheses. Field 22 is
        // the process start time in clock ticks since boot.
        let start_time = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|value| value.parse().ok())
            .ok_or(AuthError::Unavailable)?;
        Ok(Self {
            pid,
            start_time,
            uid,
        })
    }

    pub async fn authenticate(self, action: &str) -> Result<(), AuthError> {
        let subject = format!("{},{},{}", self.pid, self.start_time, self.uid);
        let mut check = tokio::process::Command::new("pkcheck");
        check
            .args([
                "--action-id",
                action,
                "--process",
                &subject,
                "--allow-user-interaction",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let status = tokio::time::timeout(Duration::from_secs(120), check.status())
            .await
            .map_err(|_| AuthError::TimedOut)?
            .map_err(|error| {
                if error.kind() != io::ErrorKind::NotFound {
                    tracing::debug!(%error, "Could not run pkcheck");
                }
                AuthError::Unavailable
            })?;
        match status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(AuthError::Denied),
            Some(3) => Err(AuthError::Canceled),
            _ => Err(AuthError::Unavailable),
        }
    }
}
