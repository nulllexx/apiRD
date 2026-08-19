//! Container lifecycle control, brokered through the `mc-control` sidecar.
//!
//! The API container deliberately has no access to the Docker socket: an RCE
//! here would otherwise be root on the host, since `/containers/create` accepts
//! arbitrary bind mounts. Instead the sidecar holds the socket and accepts only
//! three fixed verbs, matched against an allowlist in its shell loop. The worst
//! an attacker can do through this channel is start, stop or restart the
//! Minecraft container.
//!
//! Transport is a spool directory on a shared volume rather than a FIFO —
//! writing a file never blocks, so a sidecar that is down or restarting cannot
//! wedge an API worker.

use std::path::{Path, PathBuf};

/// The complete set of operations the sidecar will act on. Anything else it
/// reads is logged and discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Start,
    Stop,
    Restart,
}

impl PowerAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerAction::Start => "start",
            PowerAction::Stop => "stop",
            PowerAction::Restart => "restart",
        }
    }

    /// Parse a URL path segment. Returns `None` for anything unrecognised, so
    /// an unknown verb never reaches the spool directory in the first place.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "start" => Some(PowerAction::Start),
            "stop" => Some(PowerAction::Stop),
            "restart" => Some(PowerAction::Restart),
            _ => None,
        }
    }
}

/// Reported state of the Minecraft container, as written by the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Running,
    Stopped,
    /// The sidecar has not written a status, or wrote something unexpected —
    /// reported distinctly so "sidecar is down" is not shown as "stopped".
    Unknown,
}

impl ServerState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerState::Running => "running",
            ServerState::Stopped => "stopped",
            ServerState::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "running" | "true" => ServerState::Running,
            "stopped" | "false" => ServerState::Stopped,
            _ => ServerState::Unknown,
        }
    }
}

pub fn queue_dir(control_dir: &str) -> PathBuf {
    Path::new(control_dir).join("queue")
}

pub fn status_path(control_dir: &str) -> PathBuf {
    Path::new(control_dir).join("status")
}

/// Queue an action for the sidecar.
///
/// The filename carries a nonce so concurrent requests cannot overwrite one
/// another, and is written with a `.tmp` suffix first so the sidecar never
/// reads a half-written command.
pub async fn request(control_dir: &str, action: PowerAction) -> std::io::Result<()> {
    let queue = queue_dir(control_dir);
    tokio::fs::create_dir_all(&queue).await?;

    let nonce = uuid::Uuid::new_v4();
    let temp = queue.join(format!("{nonce}.tmp"));
    let final_path = queue.join(format!("{nonce}.cmd"));

    tokio::fs::write(&temp, action.as_str()).await?;
    // Rename is atomic within a directory, so the sidecar's glob only ever
    // matches a complete command.
    tokio::fs::rename(&temp, &final_path).await?;
    Ok(())
}

/// Read the state the sidecar last observed.
///
/// A missing or unreadable status file is `Unknown` rather than an error: the
/// sidecar may simply not have run its first poll yet.
pub async fn status(control_dir: &str) -> ServerState {
    match tokio::fs::read_to_string(status_path(control_dir)).await {
        Ok(contents) => ServerState::parse(&contents),
        Err(_) => ServerState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_supported_actions() {
        assert_eq!(PowerAction::parse("start"), Some(PowerAction::Start));
        assert_eq!(PowerAction::parse("stop"), Some(PowerAction::Stop));
        assert_eq!(PowerAction::parse("restart"), Some(PowerAction::Restart));
    }

    #[test]
    fn action_parsing_is_case_insensitive_and_trimmed() {
        assert_eq!(PowerAction::parse("  ReStArT "), Some(PowerAction::Restart));
    }

    #[test]
    fn rejects_anything_outside_the_allowlist() {
        // The sidecar has its own allowlist, but nothing unrecognised should
        // reach the spool directory to begin with.
        for bogus in ["", "kill", "rm -rf /", "start; rm -rf /", "exec", "../start"] {
            assert_eq!(PowerAction::parse(bogus), None, "{bogus:?} must be rejected");
        }
    }

    #[test]
    fn status_distinguishes_stopped_from_unknown() {
        assert_eq!(ServerState::parse("running"), ServerState::Running);
        assert_eq!(ServerState::parse("stopped"), ServerState::Stopped);
        // The sidecar writes docker inspect's raw boolean.
        assert_eq!(ServerState::parse("true\n"), ServerState::Running);
        assert_eq!(ServerState::parse("false\n"), ServerState::Stopped);
        // A down sidecar must not look like a cleanly stopped server.
        assert_eq!(ServerState::parse(""), ServerState::Unknown);
        assert_eq!(ServerState::parse("garbage"), ServerState::Unknown);
    }

    #[tokio::test]
    async fn request_writes_a_complete_command_file() {
        let dir = std::env::temp_dir().join(format!("apird-control-{}", uuid::Uuid::new_v4()));
        let control = dir.to_string_lossy().into_owned();

        request(&control, PowerAction::Restart).await.unwrap();

        let mut entries = tokio::fs::read_dir(queue_dir(&control)).await.unwrap();
        let entry = entries.next_entry().await.unwrap().expect("one command file");

        // Only finished commands are visible; the .tmp staging file is gone.
        assert_eq!(
            entry.path().extension().and_then(|e| e.to_str()),
            Some("cmd")
        );
        assert_eq!(
            tokio::fs::read_to_string(entry.path()).await.unwrap(),
            "restart"
        );

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn concurrent_requests_do_not_overwrite_each_other() {
        let dir = std::env::temp_dir().join(format!("apird-control-{}", uuid::Uuid::new_v4()));
        let control = dir.to_string_lossy().into_owned();

        for action in [PowerAction::Start, PowerAction::Stop, PowerAction::Restart] {
            request(&control, action).await.unwrap();
        }

        let mut entries = tokio::fs::read_dir(queue_dir(&control)).await.unwrap();
        let mut count = 0;
        while entries.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 3, "each request needs its own file");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn missing_status_file_reads_as_unknown() {
        let missing = std::env::temp_dir().join("apird-control-does-not-exist");
        assert_eq!(
            status(&missing.to_string_lossy()).await,
            ServerState::Unknown
        );
    }

    #[tokio::test]
    async fn status_reflects_what_the_sidecar_wrote() {
        let dir = std::env::temp_dir().join(format!("apird-status-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let control = dir.to_string_lossy().into_owned();

        tokio::fs::write(status_path(&control), "running\n").await.unwrap();
        assert_eq!(status(&control).await, ServerState::Running);

        tokio::fs::write(status_path(&control), "stopped\n").await.unwrap();
        assert_eq!(status(&control).await, ServerState::Stopped);

        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}
