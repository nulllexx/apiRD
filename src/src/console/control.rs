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

    /// Whether the server should be forced to save before this runs.
    ///
    /// Stopping and restarting both take the server down, and a shutdown that
    /// gets killed before it finishes loses every player's inventory back to
    /// the last autosave. Starting has nothing to save yet.
    pub fn saves_first(self) -> bool {
        matches!(self, PowerAction::Stop | PowerAction::Restart)
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

pub fn new_job_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn is_job_id(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        })
}

pub async fn request(
    control_dir: &str,
    action: PowerAction,
    job_id: &str,
) -> std::io::Result<()> {
    let queue = queue_dir(control_dir);
    tokio::fs::create_dir_all(&queue).await?;

    let temp = queue.join(format!("{job_id}.tmp"));
    let final_path = queue.join(format!("{job_id}.cmd"));

    tokio::fs::write(&temp, action.as_str()).await?;
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

pub fn result_dir(control_dir: &str) -> PathBuf {
    Path::new(control_dir).join("result")
}

#[derive(Debug, Clone)]
pub struct PowerResult {
    pub code: i32,
    pub output: String,
}

impl PowerResult {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}
pub async fn result(control_dir: &str, job_id: &str) -> Option<PowerResult> {
    if !is_job_id(job_id) {
        return None;
    }

    let contents = tokio::fs::read_to_string(result_dir(control_dir).join(job_id))
        .await
        .ok()?;

    let (code, output) = contents.split_once('\n')?;
    Some(PowerResult {
        code: code.trim().parse().ok()?,
        output: output.trim().to_string(),
    })
}

/// How far along one power action is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Saving,
    Queued,
    Done,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Saving => "saving",
            Phase::Queued => "queued",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }

    /// Whether there is anything left to wait for.
    pub fn is_final(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed)
    }
}

#[derive(Debug, Clone)]
pub struct PowerJob {
    pub action: &'static str,
    pub phase: Phase,
    pub save_error: Option<String>,
    pub error: Option<String>,
    pub output: Option<String>,
    started: std::time::Instant,
}

pub struct PowerJobs {
    inner: std::sync::Mutex<std::collections::HashMap<String, PowerJob>>,
}

const JOB_RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 60);

impl PowerJobs {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Register a new job and return its id.
    pub fn open(&self, action: PowerAction, phase: Phase) -> String {
        let id = new_job_id();
        let job = PowerJob {
            action: action.as_str(),
            phase,
            save_error: None,
            error: None,
            output: None,
            started: std::time::Instant::now(),
        };

        if let Ok(mut jobs) = self.inner.lock() {
            jobs.retain(|_, existing| existing.started.elapsed() < JOB_RETENTION);
            jobs.insert(id.clone(), job);
        }
        id
    }

    pub fn get(&self, id: &str) -> Option<PowerJob> {
        self.inner.lock().ok()?.get(id).cloned()
    }

    /// Apply a change to one job. A job that has already expired is silently
    /// ignored: the action still ran, there is simply nobody left to tell.
    pub fn update(&self, id: &str, change: impl FnOnce(&mut PowerJob)) {
        if let Ok(mut jobs) = self.inner.lock() {
            if let Some(job) = jobs.get_mut(id) {
                change(job);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which actions force a save first.
    ///
    /// Both of the ones that take the server down, and neither more. Getting
    /// this wrong is silent: a stop that skipped the save looks identical until
    /// players report losing their inventories.
    #[test]
    fn the_actions_that_stop_the_server_save_first() {
        assert!(PowerAction::Stop.saves_first(), "stop takes the server down");
        assert!(PowerAction::Restart.saves_first(), "so does restart");
        assert!(
            !PowerAction::Start.saves_first(),
            "a stopped server has nothing to save"
        );
    }

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

        let job = new_job_id();
        request(&control, PowerAction::Restart, &job).await.unwrap();

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
            request(&control, action, &new_job_id()).await.unwrap();
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
    /// The sidecar writes the exit status on the first line and whatever
    /// docker printed on the rest. On success that output is just the
    /// container name.
    #[tokio::test]
    async fn a_verdict_from_the_sidecar_is_read_back() {
        let dir = std::env::temp_dir().join(format!("apird-result-{}", uuid::Uuid::new_v4()));
        let control = dir.to_string_lossy().into_owned();
        tokio::fs::create_dir_all(result_dir(&control)).await.unwrap();

        let job = new_job_id();
        tokio::fs::write(result_dir(&control).join(&job), "0\nminecraft\n")
            .await
            .unwrap();

        let verdict = result(&control, &job).await.expect("a verdict");
        assert!(verdict.ok());
        assert_eq!(verdict.output, "minecraft");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// The case this whole channel exists for. Docker prints the reason across
    /// two lines and all of it has to survive to the panel, because "No such
    /// container" is the difference between "still going" and "never ran".
    #[tokio::test]
    async fn a_failed_command_carries_dockers_reason() {
        let dir = std::env::temp_dir().join(format!("apird-result-{}", uuid::Uuid::new_v4()));
        let control = dir.to_string_lossy().into_owned();
        tokio::fs::create_dir_all(result_dir(&control)).await.unwrap();

        let job = new_job_id();
        tokio::fs::write(
            result_dir(&control).join(&job),
            "1\nError response from daemon: No such container: minecraft\n\
             failed to start containers: minecraft\n",
        )
        .await
        .unwrap();

        let verdict = result(&control, &job).await.expect("a verdict");
        assert!(!verdict.ok());
        assert_eq!(verdict.code, 1);
        assert!(verdict.output.contains("No such container"));
        assert!(verdict.output.contains("failed to start containers"));

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// Absent and unfinished are the same answer here, and neither is a
    /// failure: a `docker restart -t 180` can be three minutes from writing
    /// anything, and reporting that as failed would be worse than saying
    /// nothing.
    #[tokio::test]
    async fn a_verdict_that_has_not_arrived_is_not_a_failure() {
        let dir = std::env::temp_dir().join(format!("apird-result-{}", uuid::Uuid::new_v4()));
        let control = dir.to_string_lossy().into_owned();
        assert!(result(&control, &new_job_id()).await.is_none());
    }

    /// The sidecar stages its verdict as `.tmp` and renames, so this should
    /// never be observed -- but a truncated file must read as "not yet", not
    /// as a spurious success.
    #[tokio::test]
    async fn a_half_written_verdict_reads_as_absent() {
        let dir = std::env::temp_dir().join(format!("apird-result-{}", uuid::Uuid::new_v4()));
        let control = dir.to_string_lossy().into_owned();
        tokio::fs::create_dir_all(result_dir(&control)).await.unwrap();

        let job = new_job_id();
        tokio::fs::write(result_dir(&control).join(&job), "0").await.unwrap();
        assert!(result(&control, &job).await.is_none());

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[test]
    fn a_fresh_job_id_is_a_valid_one() {
        assert!(is_job_id(&new_job_id()));
    }

    /// Job ids come off the URL and become a path segment, so anything that
    /// could climb out of the result directory has to be refused before it is
    /// ever joined onto a path.
    #[test]
    fn job_ids_that_could_escape_the_spool_are_refused() {
        for bogus in [
            "",
            "..",
            "../../etc/passwd",
            "../status",
            "a/b",
            "status",
            // Right length, wrong alphabet.
            "ZZZZZZZZ-ffff-ffff-ffff-ffffffffffff",
            // Uppercase hex: `new_job_id` never produces it, so it is not one
            // of ours and the filename would not match on a case-sensitive fs.
            "AAAAAAAA-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
            // Hyphens in the wrong places.
            "aaaaaaaaffff-ffff-ffff-ffffffffffff-",
        ] {
            assert!(!is_job_id(bogus), "{bogus:?} must not pass as a job id");
        }
    }

    /// A job read back has to carry what was put into it, because the panel
    /// renders these fields verbatim.
    #[test]
    fn a_job_tracks_its_phase_and_its_errors() {
        let jobs = PowerJobs::new();
        let id = jobs.open(PowerAction::Restart, Phase::Saving);

        let job = jobs.get(&id).expect("just opened");
        assert_eq!(job.action, "restart");
        assert_eq!(job.phase, Phase::Saving);
        assert!(!job.phase.is_final());

        jobs.update(&id, |job| {
            job.save_error = Some("the server did not respond".to_string());
            job.phase = Phase::Queued;
        });

        let job = jobs.get(&id).expect("still open");
        assert_eq!(job.phase, Phase::Queued);
        assert_eq!(job.save_error.as_deref(), Some("the server did not respond"));

        // A failed save does not make the action a failure: the stop still
        // happens, the warning just travels with it.
        assert!(job.error.is_none());
    }

    #[test]
    fn only_finished_phases_stop_the_panel_polling() {
        assert!(!Phase::Saving.is_final());
        assert!(!Phase::Queued.is_final());
        assert!(Phase::Done.is_final());
        assert!(Phase::Failed.is_final());
    }

    #[test]
    fn an_unknown_job_is_simply_absent() {
        let jobs = PowerJobs::new();
        assert!(jobs.get(&new_job_id()).is_none());
        // Updating one that has expired must not panic; the action ran, there
        // is just nobody left to tell.
        jobs.update(&new_job_id(), |job| job.phase = Phase::Done);
    }

    /// Each request gets its own id, so a second button press cannot overwrite
    /// the first one's command file or its verdict.
    #[test]
    fn every_job_gets_its_own_id() {
        let jobs = PowerJobs::new();
        let first = jobs.open(PowerAction::Stop, Phase::Saving);
        let second = jobs.open(PowerAction::Stop, Phase::Saving);
        assert_ne!(first, second);
    }
}
