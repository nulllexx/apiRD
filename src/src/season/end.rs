//! "End season": package the world and its mods as `Season_<n>.zip` in the
//! website's content folder, write the season's archive record, then start
//! the next season on a fresh world.
//!
//! Runs as one background job at a time, reporting progress the panel polls.
//! The zip is public, so what goes in it is narrow on purpose: the main world
//! folders and the `mods/*.jar` files, nothing reached through a symlink, and
//! nothing on the file browser's deny list.
//!
//! The order is chosen so that nothing is lost if a step fails:
//!
//! 1. The server is stopped (players warned, world saved first), so the world
//!    zipped is exactly the world removed. Power actions from the console are
//!    refused until the job finishes.
//! 2. The zip is written beside its final name and its contents checked.
//! 3. The record is written, then the zip published. If publishing fails the
//!    new record is removed again.
//! 4. The next season's seed and number/theme are written, keeping the old
//!    contents in memory.
//! 5. Only the main world, its nether and its end are moved aside (a rename,
//!    undone if any of them cannot be moved, along with step 4), then deleted.
//!    Other worlds -- an event hub, a test world -- are neither zipped nor
//!    touched.
//! 6. The server is started, and generates the new world.
//!
//! If anything fails after the server was stopped, it is started again.

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::archive::{self, Archive, ArchiveStore, ModEntry, CONTENT_URL};
use super::mods::{read_mods, ModInfo};
use super::reset::{self, SeasonOverride};
use crate::config::AppConfig;
use crate::console::control::{self, PowerAction, ServerState};
use crate::console::files::ServerFiles;

/// Free space required beyond the data itself.
const SPACE_MARGIN: u64 = 256 * 1024 * 1024;
/// More files than any real world has; a guard against a runaway walk.
const MAX_FILES: usize = 2_000_000;
const COPY_BUFFER: usize = 1024 * 1024;
/// How long the server gets to stop. The sidecar gives it 180 s before Docker
/// kills it; this waits a little longer than that.
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Highest season number accepted for the next season.
const MAX_NEXT_SEASON: u32 = 100_000;

/// What ServerSideUtils publishes in `season.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SeasonInfo {
    pub season: u32,
    pub theme: String,
    #[serde(default)]
    pub minecraft: Option<String>,
    #[serde(default)]
    pub world: Option<String>,
}

pub fn read_season_info(path: &str) -> Result<SeasonInfo, String> {
    let bytes = fs::read(path).map_err(|e| {
        log::warn!("season: cannot read {path}: {e}");
        "ServerSideUtils hasn't published the season details (season.json). Update the plugin \
         and restart the server once."
            .to_string()
    })?;
    let info: SeasonInfo = serde_json::from_slice(&bytes)
        .map_err(|e| format!("ServerSideUtils' season.json can't be read: {e}"))?;
    if info.season == 0 {
        return Err("ServerSideUtils reports season 0".to_string());
    }
    Ok(info)
}

/// One file going into the zip.
#[derive(Debug, Clone)]
struct Item {
    abs: PathBuf,
    name: String,
    size: u64,
}

/// Everything an end of season would do, worked out without doing it. Shown
/// in the confirmation dialog, then carried out by the job.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub season: u32,
    pub theme: String,
    pub minecraft: Option<String>,
    /// World folders that go in, e.g. `world`, `world_nether`.
    pub worlds: Vec<String>,
    /// Dimensions stored inside those folders (vanilla/Forge layout), e.g.
    /// `world/DIM-1 (Nether)`. Archived and removed with their world.
    pub dimensions: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    pub mods: Vec<ModInfo>,
    pub zip_name: String,
    pub zip_url: String,
    pub record_url: String,
    pub zip_exists: bool,
    pub record_exists: bool,
    pub free_bytes: Option<u64>,
    pub enough_space: bool,
    /// The suggested number for the season that follows.
    pub next_season: u32,
    /// The seed in `server.properties` now, if one is set.
    pub current_seed: Option<String>,
    /// Worlds that are neither archived nor removed.
    pub kept_worlds: Vec<String>,
    /// `running`, `stopped` or `unknown` (server control unavailable).
    pub server_state: &'static str,
    #[serde(skip)]
    items: Vec<Item>,
}

/// A single plain name: what a world folder must be.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
}

fn main_world_name(cfg: &AppConfig, info: &SeasonInfo) -> String {
    if let Some(world) = info.world.as_deref().filter(|w| plain_name(w)) {
        return world.to_string();
    }
    fs::read_to_string(&cfg.server_properties_path)
        .ok()
        .and_then(|text| {
            crate::console::files_write::java_properties(&text)
                .into_iter()
                .filter(|(k, _)| k == "level-name")
                .last()
                .map(|(_, v)| v)
        })
        .filter(|w| plain_name(w))
        .unwrap_or_else(|| "world".to_string())
}

pub fn plan(cfg: &AppConfig) -> Result<Plan, String> {
    let info = read_season_info(&cfg.season_info_path)?;
    let root = fs::canonicalize(&cfg.minecraft_server_dir)
        .map_err(|_| "The server directory is not available".to_string())?;
    let browser = ServerFiles::new(&root, &cfg.server_files_deny);
    let prefix = format!("Season_{}", info.season);

    let base = main_world_name(cfg, &info);
    let mut worlds = Vec::new();
    let mut dimensions = Vec::new();
    let mut items = Vec::new();
    for name in [base.clone(), format!("{base}_nether"), format!("{base}_the_end")] {
        let path = root.join(&name);
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() {
            continue;
        }
        walk(&root, &path, &name, &prefix, &browser, &mut items)?;
        if name == base {
            // world_nether/DIM-1 is just Bukkit's layout inside its own folder.
            dimensions.extend(dimensions_in(&path, &name));
        }
        worlds.push(name);
    }
    if worlds.is_empty() {
        return Err(format!("The world folder `{base}` was not found on the server"));
    }

    let mods_dir = PathBuf::from(&cfg.mods_dir);
    let mods = read_mods(&mods_dir, info.minecraft.as_deref());
    for m in &mods {
        let path = mods_dir.join(&m.file);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.is_file() {
                items.push(Item { abs: path, name: format!("{prefix}/mods/{}", m.file), size: meta.len() });
            }
        }
    }

    let bytes: u64 = items.iter().map(|i| i.size).sum();
    let free_bytes = fs2::available_space(&cfg.seasons_path).ok();
    let store = ArchiveStore::new(&cfg.archives_dir, &cfg.seasons_path);
    let zip_name = archive::zip_name(info.season);
    Ok(Plan {
        season: info.season,
        theme: info.theme,
        minecraft: info.minecraft,
        files: items.len() as u64,
        bytes,
        mods,
        zip_url: format!("{CONTENT_URL}/{zip_name}"),
        record_url: format!("/archives/{}", archive::record_name(info.season)),
        zip_exists: fs::symlink_metadata(Path::new(&cfg.seasons_path).join(&zip_name)).is_ok(),
        record_exists: store.exists(info.season),
        enough_space: free_bytes.map_or(true, |free| free >= bytes.saturating_add(SPACE_MARGIN)),
        free_bytes,
        next_season: info.season.saturating_add(1),
        current_seed: fs::read_to_string(&cfg.server_properties_path)
            .ok()
            .and_then(|text| reset::current_seed(&text)),
        kept_worlds: reset::other_worlds(&root, &worlds),
        server_state: fs::read_to_string(control::status_path(&cfg.control_dir))
            .map(|text| ServerState::parse(&text))
            .unwrap_or(ServerState::Unknown)
            .as_str(),
        zip_name,
        items,
        worlds,
        dimensions,
    })
}

/// The dimensions kept inside a world folder: vanilla's `DIM-1` and `DIM1`,
/// and mods' `dimensions/<namespace>/<name>`. Bukkit keeps the Nether and End
/// in sibling folders instead; those are listed as worlds.
fn dimensions_in(world: &Path, name: &str) -> Vec<String> {
    let is_dir = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let mut out = Vec::new();
    for (dir, label) in [("DIM-1", "Nether"), ("DIM1", "The End")] {
        if is_dir(&world.join(dir)) {
            out.push(format!("{name}/{dir} ({label})"));
        }
    }
    let mut modded = Vec::new();
    if let Ok(namespaces) = fs::read_dir(world.join("dimensions")) {
        for ns in namespaces.flatten() {
            if !is_dir(&ns.path()) {
                continue;
            }
            let ns_name = ns.file_name().to_string_lossy().into_owned();
            let Ok(dims) = fs::read_dir(ns.path()) else { continue };
            for dim in dims.flatten() {
                if is_dir(&dim.path()) {
                    modded.push(format!("{name}/dimensions/{ns_name}/{}", dim.file_name().to_string_lossy()));
                }
            }
        }
    }
    modded.sort();
    out.extend(modded);
    out
}

/// Every regular file under `dir`, never following a symlink.
fn walk(
    root: &Path,
    dir: &Path,
    rel: &str,
    prefix: &str,
    browser: &ServerFiles,
    out: &mut Vec<Item>,
) -> Result<(), String> {
    let mut stack = vec![(dir.to_path_buf(), rel.to_string())];
    while let Some((path, rel)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = format!("{rel}/{name}");
            if browser.is_denied(&child) || name == "session.lock" {
                continue;
            }
            if meta.is_dir() {
                stack.push((entry.path(), child));
            } else if meta.is_file() {
                if out.len() >= MAX_FILES {
                    return Err("The world has more files than can be archived".to_string());
                }
                debug_assert!(entry.path().starts_with(root));
                out.push(Item { abs: entry.path(), name: format!("{prefix}/{child}"), size: meta.len() });
            }
        }
    }
    Ok(())
}

/// What the person ending the season adds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndRequest {
    /// The season the dialog showed. If the plugin now says otherwise, stop.
    pub season: u32,
    pub started: Option<NaiveDate>,
    pub ended: Option<NaiveDate>,
    pub additional_info: String,
    /// The number and theme of the season that starts on the new world.
    pub next_season: u32,
    pub next_theme: String,
    /// Seed for the new world; empty or absent for a random one.
    #[serde(default)]
    pub seed: Option<String>,
}

/// The next season's settings, checked.
#[derive(Debug, Clone)]
pub struct Next {
    pub season: u32,
    pub theme: String,
    pub seed: Option<String>,
}

impl EndRequest {
    /// Check what can be checked without the server: the dates and text, and
    /// the next season's number, theme and seed.
    pub fn validate(&self) -> Result<Next, String> {
        let probe = archive::ArchiveInput {
            season: self.season,
            theme: String::new(),
            started: self.started,
            ended: self.ended,
            minecraft_version: None,
            has_world: true,
            world_url: None,
            mods: Vec::new(),
            additional_info: self.additional_info.clone(),
        };
        archive::validate(&probe)?;
        if self.next_season <= self.season || self.next_season > MAX_NEXT_SEASON {
            return Err(format!(
                "The next season's number must be higher than {} (and at most {MAX_NEXT_SEASON})",
                self.season
            ));
        }
        Ok(Next {
            season: self.next_season,
            theme: reset::clean_theme(&self.next_theme)?,
            seed: reset::clean_seed(self.seed.as_deref())?,
        })
    }
}

/// The job's steps, with how much of the bar each takes.
pub const STAGES: &[(&str, &str, f32)] = &[
    ("check", "Checking the season, files and free space", 2.0),
    ("stop", "Saving the world and stopping the server", 6.0),
    ("zip", "Zipping the world and mods", 72.0),
    ("record", "Writing the archive record", 3.0),
    ("publish", "Publishing the download", 3.0),
    ("configure", "Setting the next season's number, theme and seed", 2.0),
    ("reset", "Removing the old world", 6.0),
    ("start", "Starting the server on a new world", 6.0),
];

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub id: String,
    pub season: u32,
    pub started_by: String,
    pub started_at: String,
    /// `running`, `done` or `failed`.
    pub state: &'static str,
    pub stage: &'static str,
    pub stage_label: &'static str,
    pub stage_index: usize,
    pub stages: Vec<&'static str>,
    pub percent: f32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub current_file: Option<String>,
    pub notes: Vec<String>,
    pub error: Option<String>,
    pub zip_url: Option<String>,
    pub record_url: Option<String>,
    pub finished_at: Option<String>,
}

pub type Job = Arc<Mutex<Progress>>;

fn jobs() -> &'static Mutex<Option<Job>> {
    static CURRENT: Mutex<Option<Job>> = Mutex::new(None);
    &CURRENT
}

fn locked(job: &Job) -> std::sync::MutexGuard<'_, Progress> {
    job.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a season is being ended right now.
pub fn is_running() -> bool {
    let slot = jobs().lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref().is_some_and(|job| locked(job).state == "running")
}

/// The running job, or the last one to finish.
pub fn current() -> Option<Progress> {
    let slot = jobs().lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref().map(|job| locked(job).clone())
}

/// Claim the single job slot, or `None` if a job is already running.
pub fn begin(season: u32, by: &str) -> Option<Job> {
    let mut slot = jobs().lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|job| locked(job).state == "running") {
        return None;
    }
    let job = Arc::new(Mutex::new(Progress {
        id: uuid::Uuid::new_v4().to_string(),
        season,
        started_by: by.to_string(),
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        state: "running",
        stage: STAGES[0].0,
        stage_label: STAGES[0].1,
        stage_index: 0,
        stages: STAGES.iter().map(|s| s.1).collect(),
        percent: 0.0,
        bytes_done: 0,
        bytes_total: 0,
        files_done: 0,
        files_total: 0,
        current_file: None,
        notes: Vec::new(),
        error: None,
        zip_url: None,
        record_url: None,
        finished_at: None,
    }));
    *slot = Some(job.clone());
    Some(job)
}

fn stage_base(index: usize) -> f32 {
    STAGES[..index].iter().map(|s| s.2).sum()
}

fn set_stage(job: &Job, key: &str) {
    let index = STAGES.iter().position(|s| s.0 == key).expect("known stage");
    let mut p = locked(job);
    p.stage = STAGES[index].0;
    p.stage_label = STAGES[index].1;
    p.stage_index = index;
    p.percent = stage_base(index);
    p.current_file = None;
}

fn note(job: &Job, message: impl Into<String>) {
    locked(job).notes.push(message.into());
}

/// Mark a job that never started as failed, freeing the slot.
pub fn abandon(job: &Job, why: String) {
    finish(job, Some(why));
}

fn finish(job: &Job, error: Option<String>) {
    let mut p = locked(job);
    p.state = if error.is_some() { "failed" } else { "done" };
    if error.is_none() {
        p.percent = 100.0;
    }
    p.error = error;
    p.current_file = None;
    p.finished_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
}

/// Compressing these gains nothing: they are compressed already.
fn store_only(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".mca", ".mcc", ".jar", ".zip", ".gz", ".png", ".jpg", ".ogg", ".dat", ".dat_old"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Write the zip to `tmp`, reporting progress. Returns its size and the
/// files it holds, with their sizes.
fn write_zip(items: &[Item], tmp: &Path, job: &Job) -> io::Result<(u64, Vec<(String, u64)>)> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut zip = zip::ZipWriter::new(BufWriter::new(options.open(tmp)?));
    let mut buffer = vec![0u8; COPY_BUFFER];
    let total: u64 = items.iter().map(|i| i.size).sum();
    let (base, weight) = (stage_base(2), STAGES[2].2);
    let mut done: u64 = 0;
    let mut skipped = 0;
    let mut written = Vec::with_capacity(items.len());

    for (index, item) in items.iter().enumerate() {
        {
            let mut p = locked(job);
            p.current_file = Some(item.name.clone());
            p.files_done = index as u64;
        }
        // Opened without following a final symlink: one swapped in since the
        // walk must not pull a file from elsewhere into a public download.
        let opened = {
            let mut o = fs::OpenOptions::new();
            o.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            o.open(&item.abs)
        };
        let mut file = match opened.and_then(|f| f.metadata().map(|m| (f, m))) {
            Ok((f, m)) if m.is_file() => f,
            _ => {
                skipped += 1;
                done += item.size;
                continue;
            }
        };
        let method = if store_only(&item.name) { CompressionMethod::Stored } else { CompressionMethod::Deflated };
        let opts = SimpleFileOptions::default()
            .compression_method(method)
            .unix_permissions(0o644)
            .large_file(item.size >= u32::MAX as u64);
        zip.start_file(item.name.as_str(), opts)?;
        let mut copied: u64 = 0;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buffer[..n])?;
            copied += n as u64;
            done += n as u64;
            let mut p = locked(job);
            p.bytes_done = done;
            p.percent = base + weight * if total == 0 { 1.0 } else { (done as f32 / total as f32).min(1.0) };
        }
        written.push((item.name.clone(), copied));
    }
    let writer = zip.finish()?;
    let file = writer.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    if skipped > 0 {
        note(job, format!("{skipped} file(s) vanished or changed type while zipping and were left out"));
    }
    locked(job).files_done = items.len() as u64;
    Ok((file.metadata()?.len(), written))
}

/// Read the finished zip's directory back and compare it with what was
/// written: every file present once, at its full size. The world is deleted
/// on the strength of this, so it is not skipped.
fn verify_zip(path: &Path, written: &[(String, u64)]) -> Result<(), String> {
    let file = fs::File::open(path).map_err(|e| format!("The zip couldn't be reopened to check it: {e}"))?;
    let mut zip = zip::ZipArchive::new(io::BufReader::new(file))
        .map_err(|e| format!("The zip is not readable: {e}"))?;
    if zip.len() != written.len() {
        return Err(format!("The zip holds {} files, not {}", zip.len(), written.len()));
    }
    for (name, size) in written {
        let entry = zip.by_name(name).map_err(|_| format!("`{name}` is missing from the zip"))?;
        if entry.size() != *size {
            return Err(format!("`{name}` is {} bytes in the zip, not {size}", entry.size()));
        }
    }
    Ok(())
}

/// Ask the sidecar to start or stop the server and wait for its verdict.
async fn power(state: &crate::AppState, action: PowerAction, timeout: std::time::Duration) -> Result<(), String> {
    let id = control::new_job_id();
    control::request(&state.config.control_dir, action, &id)
        .await
        .map_err(|e| format!("Server control is unavailable ({e})"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(result) = control::result(&state.config.control_dir, &id).await {
            return if result.ok() {
                Ok(())
            } else if result.output.is_empty() {
                Err(format!("server control exited with status {}", result.code))
            } else {
                Err(result.output)
            };
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("server control gave no answer within {} s", timeout.as_secs()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Warn players, save, and stop the server. Errors before the stop is queued
/// mean nothing was done.
async fn stop_server(state: &crate::AppState, job: &Job, season: u32) -> Result<(), String> {
    if state.rcon.is_configured() {
        let _ = state
            .rcon
            .execute(&format!(
                "say Season {season} is over! The server is closing to archive the world and start a new one."
            ))
            .await;
        match state
            .rcon
            .execute_within("save-all flush", crate::rcon::SLOW_COMMAND_TIMEOUT)
            .await
        {
            Ok(_) => {}
            Err(e) => note(job, format!("The world couldn't be saved before stopping ({}); the shutdown saves it", e.user_message())),
        }
    } else {
        note(job, "RCON isn't configured, so players weren't warned; the shutdown saves the world");
    }
    power(state, PowerAction::Stop, STOP_TIMEOUT)
        .await
        .map_err(|e| format!("The server couldn't be stopped ({e}). Nothing was changed."))
}

/// Carry out the end of the season. Errors are written to the job.
pub async fn run(state: actix_web::web::Data<crate::AppState>, job: Job, request: EndRequest, by: String) {
    let mut stopped_by_us = false;
    let result = run_steps(&state, &job, request, &by, &mut stopped_by_us).await;
    let result = match result {
        Err(e) if stopped_by_us => {
            // Whatever failed, the server does not stay down because of it.
            {
                let mut p = locked(&job);
                p.stage_label = "Starting the server again";
            }
            match power(&state, PowerAction::Start, START_TIMEOUT).await {
                Ok(()) => Err(format!("{e} The server was started again.")),
                Err(why) => Err(format!(
                    "{e} Starting the server again ALSO failed ({why}); start it from the Console."
                )),
            }
        }
        other => other,
    };
    if let Err(e) = &result {
        log::error!("season: ending the season failed: {e}");
    }
    finish(&job, result.err());
}

async fn run_steps(
    state: &crate::AppState,
    job: &Job,
    request: EndRequest,
    by: &str,
    stopped_by_us: &mut bool,
) -> Result<(), String> {
    let cfg = state.config.clone();

    set_stage(job, "check");
    let next = request.validate()?;
    let plan = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || plan(&cfg))
            .await
            .map_err(|e| format!("The check failed: {e}"))??
    };
    if plan.season != request.season {
        return Err(format!(
            "ServerSideUtils now reports season {}, not {}. Reopen the dialog to check the details.",
            plan.season, request.season
        ));
    }
    if plan.zip_exists {
        return Err(format!("{} already exists, so nothing was done", plan.zip_name));
    }
    if plan.record_exists {
        return Err(format!("Season {} already has an archive record, so nothing was done", plan.season));
    }
    if !plan.enough_space {
        return Err("There isn't enough free disk space for the zip".to_string());
    }
    let initial = control::status(&cfg.control_dir).await;
    if initial == ServerState::Unknown {
        return Err(
            "Server control isn't reporting the server's state, so it can't be stopped and started \
             safely. Nothing was done."
                .to_string(),
        );
    }
    // Both files are rewritten later; find out now if they can't be read.
    let properties_path = PathBuf::from(&cfg.server_properties_path);
    let override_path = PathBuf::from(&cfg.season_override_path);
    let saved_properties = reset::Saved::capture(&properties_path)
        .map_err(|e| format!("server.properties can't be read ({e}); nothing was done"))?;
    let saved_override = reset::Saved::capture(&override_path)
        .map_err(|e| format!("The season override can't be read ({e}); nothing was done"))?;
    let properties_text = String::from_utf8_lossy(saved_properties.before().unwrap_or_default()).into_owned();
    let new_properties = reset::with_level_seed(&properties_text, next.seed.as_deref())?;
    {
        let mut p = locked(job);
        p.bytes_total = plan.bytes;
        p.files_total = plan.files;
    }

    set_stage(job, "stop");
    if initial == ServerState::Running {
        stop_server(state, job, plan.season).await?;
    } else {
        note(job, "The server was already stopped");
    }
    // Started at the end either way, to generate the new world. On failure
    // it is only started again if this job is what stopped it.
    *stopped_by_us = initial == ServerState::Running;

    set_stage(job, "zip");
    let content = PathBuf::from(&cfg.seasons_path);
    let tmp = content.join(format!(".{}.{}.partial", plan.zip_name, uuid::Uuid::new_v4().simple()));
    let zipped = {
        let (items, tmp, job) = (plan.items.clone(), tmp.clone(), job.clone());
        tokio::task::spawn_blocking(move || {
            let (size, written) = write_zip(&items, &tmp, &job).map_err(|e| format!("Zipping failed: {e}"))?;
            verify_zip(&tmp, &written)?;
            Ok::<u64, String>(size)
        })
        .await
        .map_err(|e| format!("Zipping failed: {e}"))
        .and_then(|r| r)
    };
    let size = match zipped {
        Ok(size) => size,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(format!("{e}. Nothing was changed."));
        }
    };

    set_stage(job, "record");
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut mods: Vec<ModEntry> = plan.mods.iter().map(ModEntry::from).collect();
    archive::sort_mods(&mut mods);
    let input = archive::ArchiveInput {
        season: plan.season,
        theme: plan.theme.clone(),
        started: request.started,
        ended: request.ended,
        minecraft_version: plan.minecraft.clone(),
        has_world: true,
        world_url: None,
        mods,
        additional_info: request.additional_info,
    };
    // The same validation a hand-made record gets.
    let input = match archive::validate(&input) {
        Ok(input) => input,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(format!("The archive record was refused ({e}). Nothing was changed."));
        }
    };
    let store = ArchiveStore::new(&cfg.archives_dir, &cfg.seasons_path);
    let mut world = store.world_for(plan.season);
    world.size = Some(size);
    let record = Archive {
        schema: archive::SCHEMA,
        season: input.season,
        theme: input.theme,
        started: input.started,
        ended: input.ended,
        minecraft_version: input.minecraft_version,
        world: Some(world),
        mods: input.mods,
        additional_info: input.additional_info,
        created_at: stamp.clone(),
        created_by: by.to_string(),
        updated_at: stamp,
        updated_by: by.to_string(),
    };
    let stored = {
        let (store, record) = (ArchiveStore::new(&cfg.archives_dir, &cfg.seasons_path), record.clone());
        tokio::task::spawn_blocking(move || store.create(&record)).await
    };
    if !matches!(stored, Ok(Ok(()))) {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "The archive record couldn't be written ({}). Nothing was changed.",
            match stored {
                Ok(Err(e)) => format!("{e:?}"),
                Err(e) => e.to_string(),
                Ok(Ok(())) => unreachable!(),
            }
        ));
    }

    set_stage(job, "publish");
    let target = content.join(&plan.zip_name);
    if let Err(e) = archive::publish(&tmp, &target, true) {
        let _ = fs::remove_file(&tmp);
        let _ = store.remove_new(plan.season);
        return Err(match e.kind() {
            io::ErrorKind::AlreadyExists => format!("{} appeared while zipping; it was left alone. Nothing was changed.", plan.zip_name),
            _ => format!("The zip couldn't be published ({e}). Nothing was changed."),
        });
    }
    {
        let mut p = locked(job);
        p.zip_url = Some(plan.zip_url.clone());
        p.record_url = Some(plan.record_url.clone());
    }
    // From here the season is archived; a failure below leaves the old world
    // in place, and says so.
    let archived = format!("Season {} is archived, but the new world wasn't set up:", plan.season);

    set_stage(job, "configure");
    let override_json = serde_json::to_vec_pretty(&SeasonOverride { season: next.season, theme: next.theme.clone() })
        .map_err(|e| format!("{archived} {e}"))?;
    if let Err(e) = reset::replace_file(&properties_path, new_properties.as_bytes(), 0o644) {
        return Err(format!("{archived} server.properties couldn't be written ({e}). The old world is untouched."));
    }
    if let Err(e) = reset::replace_file(&override_path, &override_json, 0o644) {
        let undo = saved_properties.restore();
        return Err(format!(
            "{archived} the season override couldn't be written ({e}). The old world is untouched{}",
            undo.err().map(|u| format!("; restoring server.properties also failed ({u})")).unwrap_or_else(|| ".".into())
        ));
    }
    note(job, match &next.seed {
        Some(seed) => format!("Season {} ({}) starts with seed {seed}", next.season, next.theme),
        None => format!("Season {} ({}) starts with a random seed", next.season, next.theme),
    });

    set_stage(job, "reset");
    let root = PathBuf::from(&cfg.minecraft_server_dir);
    let staging = format!(".season-{}-old-world-{}", plan.season, uuid::Uuid::new_v4().simple());
    let moved = {
        let (root, worlds, staging) = (root.clone(), plan.worlds.clone(), staging.clone());
        tokio::task::spawn_blocking(move || reset::move_worlds_aside(&root, &worlds, &staging))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
    };
    let moved = match moved {
        Ok(moved) => moved,
        Err(e) => {
            let mut problems = Vec::new();
            if let Err(u) = saved_properties.restore() {
                problems.push(format!("server.properties couldn't be restored ({u})"));
            }
            if let Err(u) = saved_override.restore() {
                problems.push(format!("the season override couldn't be restored ({u})"));
            }
            return Err(format!(
                "{archived} {e}. The season settings were put back{}.",
                if problems.is_empty() { String::new() } else { format!(", except: {}", problems.join("; ")) }
            ));
        }
    };
    note(job, format!("Removed {}", plan.worlds.join(", ")));
    if !plan.kept_worlds.is_empty() {
        note(job, format!("Kept {}", plan.kept_worlds.join(", ")));
    }
    let deleted = tokio::task::spawn_blocking(move || moved.delete())
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    if let Err(e) = deleted {
        log::error!("season: {e}");
        note(job, e);
    }

    set_stage(job, "start");
    power(state, PowerAction::Start, START_TIMEOUT).await.map_err(|e| {
        format!("Season {} is archived and the new world is set up, but the server couldn't be started ({e}).", plan.season)
    })?;
    *stopped_by_us = false;
    note(job, "The server is starting and will generate the new world");
    log::info!("season: season {} ended by {by}; season {} begins", plan.season, next.season);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("apird-season-{}", uuid::Uuid::new_v4()));
            for d in ["server/world/region", "server/world_nether/DIM-1", "server/mods", "server/plugins/ServerSideUtils", "content", "outside"] {
                fs::create_dir_all(dir.join(d)).unwrap();
            }
            Scratch(dir)
        }
        fn write(&self, rel: &str, body: &[u8]) {
            fs::write(self.0.join(rel), body).unwrap();
        }
        fn config(&self) -> AppConfig {
            let base = self.0.clone();
            AppConfig::build(move |key| {
                let p = |rel: &str| Some(base.join(rel).to_string_lossy().into_owned());
                match key {
                    "DB_HOST" | "DB_USER" | "DB_PASSWORD" | "DB_NAME" | "JWT_SECRET" => Some("x".into()),
                    "MINECRAFT_SERVER_DIR" => p("server"),
                    "MODS_DIR" => p("server/mods"),
                    "SERVER_PROPERTIES_PATH" => p("server/server.properties"),
                    "SEASON_INFO_PATH" => p("server/plugins/ServerSideUtils/season.json"),
                    "SEASONS_PATH" => p("content"),
                    "ARCHIVES_DIR" => p("archives"),
                    _ => None,
                }
            })
            .unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample() -> Scratch {
        let s = Scratch::new();
        s.write("server/plugins/ServerSideUtils/season.json", br#"{"season":9,"theme":"S8 reboot","minecraft":"1.21.1","world":"world"}"#);
        s.write("server/world/level.dat", b"level");
        for d in ["server/world/DIM1/region", "server/world/dimensions/twilightforest/twilight_forest/region"] {
            fs::create_dir_all(s.0.join(d)).unwrap();
        }
        s.write("server/world/DIM1/region/r.0.0.mca", b"end");
        s.write("server/world/session.lock", b"lock");
        s.write("server/world/region/r.0.0.mca", &[7u8; 5000]);
        s.write("server/world_nether/DIM-1/x.mca", b"nether");
        s.write("server/world/authedPlayers.json", b"{\"creds\":1}");
        s.write("server/mods/thing-1.21.1-2.3.4.jar", b"not really a jar");
        s.write("server/mods/old.jar.disabled", b"x");
        s.write("server/server.properties", b"rcon.password=hunter2\n");
        s.write("outside/secret.txt", b"secret");
        fs::create_dir_all(s.0.join("server/eventhub/region")).unwrap();
        s.write("server/eventhub/level.dat", b"hub");
        s.write("server/eventhub/region/r.0.0.mca", b"hub region");
        #[cfg(unix)]
        std::os::unix::fs::symlink(s.0.join("outside"), s.0.join("server/world/escape")).unwrap();
        s
    }

    #[test]
    fn the_plan_takes_only_the_world_and_mods() {
        let s = sample();
        let plan = plan(&s.config()).unwrap();
        assert_eq!((plan.season, plan.theme.as_str()), (9, "S8 reboot"));
        assert_eq!(plan.worlds, ["world", "world_nether"]);
        assert_eq!(plan.dimensions, ["world/DIM1 (The End)", "world/dimensions/twilightforest/twilight_forest"]);
        let mut names: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "Season_9/mods/thing-1.21.1-2.3.4.jar",
                "Season_9/world/DIM1/region/r.0.0.mca",
                "Season_9/world/level.dat",
                "Season_9/world/region/r.0.0.mca",
                "Season_9/world_nether/DIM-1/x.mca",
            ],
            "no session.lock, no denied file, nothing through the symlink, no disabled jar, no server.properties"
        );
        assert_eq!(plan.mods[0].version, "2.3.4");
        assert_eq!(plan.zip_name, "Season_9.zip");
        assert!(!plan.zip_exists && !plan.record_exists);
        assert_eq!(plan.kept_worlds, ["eventhub"], "other worlds are neither archived nor removed");
        assert_eq!(plan.next_season, 10);
        assert_eq!(plan.server_state, "unknown", "no sidecar status in the scratch folder");
    }

    #[test]
    fn the_world_name_falls_back_to_level_name() {
        let s = sample();
        s.write("server/plugins/ServerSideUtils/season.json", br#"{"season":9,"theme":"t","world":"../etc"}"#);
        fs::rename(s.0.join("server/world"), s.0.join("server/survival")).unwrap();
        s.write("server/server.properties", b"level-name=survival\n");
        assert_eq!(plan(&s.config()).unwrap().worlds, ["survival"]);
    }

    #[test]
    fn missing_plugin_info_says_what_to_do() {
        let s = Scratch::new();
        let err = plan(&s.config()).unwrap_err();
        assert!(err.contains("ServerSideUtils"), "{err}");
    }

    #[test]
    fn the_zip_holds_the_planned_files_and_reports_progress() {
        let s = sample();
        let plan = plan(&s.config()).unwrap();
        let job = begin(9, "test").expect("free slot");
        let tmp = s.0.join("content/.partial");
        let (size, written) = write_zip(&plan.items, &tmp, &job).unwrap();
        assert!(size > 0);
        verify_zip(&tmp, &written).unwrap();
        let mut wrong = written.clone();
        wrong[0].1 += 1;
        assert!(verify_zip(&tmp, &wrong).is_err(), "a size mismatch is caught");
        assert!(verify_zip(&tmp, &written[1..]).is_err(), "so is an extra file");
        let p = locked(&job).clone();
        assert_eq!(p.bytes_done, plan.bytes);
        assert!((p.percent - stage_base(3)).abs() < 0.01, "the zip stage ends full");
        finish(&job, None);

        let mut zip = zip::ZipArchive::new(File::open(&tmp).unwrap()).unwrap();
        assert_eq!(zip.len(), 5);
        let mut region = Vec::new();
        zip.by_name("Season_9/world/region/r.0.0.mca").unwrap().read_to_end(&mut region).unwrap();
        assert_eq!(region, vec![7u8; 5000]);
    }

    fn request(next: u32, theme: &str, seed: Option<&str>) -> EndRequest {
        EndRequest {
            season: 9,
            started: None,
            ended: None,
            additional_info: String::new(),
            next_season: next,
            next_theme: theme.to_string(),
            seed: seed.map(str::to_string),
        }
    }

    #[test]
    fn the_next_season_is_checked() {
        let next = request(10, " Ocean ", Some(" 42 ")).validate().unwrap();
        assert_eq!((next.season, next.theme.as_str(), next.seed.as_deref()), (10, "Ocean", Some("42")));
        assert_eq!(request(10, "Ocean", Some("")).validate().unwrap().seed, None);
        assert!(request(9, "Ocean", None).validate().is_err(), "must move forward");
        assert!(request(10, "", None).validate().is_err(), "needs a theme");
        assert!(request(10, "Ocean", Some("é")).validate().is_err());
    }

    #[test]
    fn short_and_plain_names() {
        assert!(plain_name("world") && plain_name("my world"));
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0"] {
            assert!(!plain_name(bad), "{bad:?}");
        }
        assert!(store_only("r.0.0.mca") && store_only("X.JAR") && !store_only("level.txt"));
    }
}
