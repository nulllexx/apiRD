//! Season archive records: one JSON file per season in the website's
//! `/archives/` folder, written by "End season" or by hand from the panel.
//!
//! Every record has exactly the shape of [`Archive`], whoever wrote it. The
//! struct is the schema: all fields are always written (absent values as
//! `null`, never left out), in the same order, and reading refuses any field it
//! does not know. A person edits [`ArchiveInput`] -- the fields that are
//! theirs to set -- and the server fills in the rest, so a hand-made record
//! for an old season cannot come out shaped differently from a generated one.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bumped only if the shape ever has to change, so readers can tell.
pub const SCHEMA: u32 = 1;

/// Where the world downloads are served from, as the website sees it.
pub const CONTENT_URL: &str = "/raindrippy/content";

const MAX_SEASON: u32 = 100_000;
const MAX_THEME: usize = 200;
const MAX_MODS: usize = 2_000;
const MAX_MOD_NAME: usize = 120;
const MAX_MOD_VERSION: usize = 60;
const MAX_INFO: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub schema: u32,
    pub season: u32,
    pub theme: String,
    /// `YYYY-MM-DD`.
    pub started: Option<NaiveDate>,
    pub ended: Option<NaiveDate>,
    pub minecraft_version: Option<String>,
    /// The season's world download, when there is one.
    pub world: Option<WorldFile>,
    /// Sorted by name.
    pub mods: Vec<ModEntry>,
    pub additional_info: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub created_by: String,
    pub updated_at: String,
    pub updated_by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldFile {
    /// Always `Season_<n>.zip`.
    pub file: String,
    pub url: String,
    /// Bytes, when the file is on disk.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModEntry {
    pub name: String,
    /// Shortened: `10.7.7`, not `1.21.1-10.7.7.64`.
    pub version: String,
}

/// The parts of a record a person sets.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveInput {
    pub season: u32,
    pub theme: String,
    pub started: Option<NaiveDate>,
    pub ended: Option<NaiveDate>,
    pub minecraft_version: Option<String>,
    /// Whether the season has a `Season_<n>.zip` download. The file name and
    /// URL follow from the season; they are not free text, so a record cannot
    /// point a download link anywhere else.
    pub has_world: bool,
    pub mods: Vec<ModEntry>,
    pub additional_info: String,
}

pub fn zip_name(season: u32) -> String {
    format!("Season_{season}.zip")
}

pub fn record_name(season: u32) -> String {
    format!("Season_{season}.json")
}

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

fn clean_line(field: &str, value: &str, max: usize) -> Result<String, String> {
    let value = value.trim();
    if value.chars().count() > max {
        return Err(format!("{field} can be at most {max} characters"));
    }
    if has_control(value) {
        return Err(format!("{field} can't contain line breaks or control characters"));
    }
    Ok(value.to_string())
}

/// Check and tidy what a person sent. Errors name the field.
pub fn validate(input: &ArchiveInput) -> Result<ArchiveInput, String> {
    if input.season == 0 || input.season > MAX_SEASON {
        return Err(format!("Season must be between 1 and {MAX_SEASON}"));
    }
    let theme = clean_line("Theme", &input.theme, MAX_THEME)?;
    if let (Some(start), Some(end)) = (input.started, input.ended) {
        if end < start {
            return Err("The season can't end before it started".to_string());
        }
    }
    let minecraft_version = match input.minecraft_version.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(v) => {
            if v.len() > 32 || !v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
                return Err("Minecraft version can only hold letters, digits, '.', '-' and '_'".to_string());
            }
            Some(v.to_string())
        }
    };
    if input.mods.len() > MAX_MODS {
        return Err(format!("At most {MAX_MODS} mods can be listed"));
    }
    let mut mods = Vec::with_capacity(input.mods.len());
    for (i, m) in input.mods.iter().enumerate() {
        let name = clean_line(&format!("Mod {} name", i + 1), &m.name, MAX_MOD_NAME)?;
        let version = clean_line(&format!("Mod {} version", i + 1), &m.version, MAX_MOD_VERSION)?;
        if name.is_empty() {
            return Err(format!("Mod {} needs a name", i + 1));
        }
        mods.push(ModEntry { name, version });
    }
    sort_mods(&mut mods);
    let additional_info = input.additional_info.trim().replace("\r\n", "\n");
    if additional_info.chars().count() > MAX_INFO {
        return Err(format!("Additional info can be at most {MAX_INFO} characters"));
    }
    if additional_info.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err("Additional info can't contain control characters".to_string());
    }
    Ok(ArchiveInput {
        season: input.season,
        theme,
        started: input.started,
        ended: input.ended,
        minecraft_version,
        has_world: input.has_world,
        mods,
        additional_info,
    })
}

pub fn sort_mods(mods: &mut [ModEntry]) {
    mods.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.name.cmp(&b.name)));
}

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    Exists,
    Conflict,
    Invalid(String),
    Io(io::Error),
}

impl From<StoreError> for crate::error::AppError {
    fn from(e: StoreError) -> Self {
        use crate::error::AppError;
        match e {
            StoreError::NotFound => AppError::NotFound("No archive record for that season".into()),
            StoreError::Exists => AppError::Conflict("That season already has an archive record".into()),
            StoreError::Conflict => AppError::Conflict(
                "The record changed since you opened it. Reload it and make your changes again.".into(),
            ),
            StoreError::Invalid(why) => AppError::BadRequest(why),
            StoreError::Io(e) => {
                log::error!("archives: {e}");
                AppError::Internal("Could not read or write the archive records".into())
            }
        }
    }
}

/// One record as listed: parsed, or why it could not be.
#[derive(Debug, Serialize)]
pub struct Listed {
    pub season: u32,
    pub file: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<Archive>,
    /// Set when the file on disk is not a valid record (edited by hand
    /// outside the panel). Saving it from the panel rewrites it validly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The archive folder and the world downloads beside it.
pub struct ArchiveStore {
    dir: PathBuf,
    content_dir: PathBuf,
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Identifies a record's exact bytes, for the editor's conflict check.
pub fn version_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..12].iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `Season_<n>.json` to `n`.
fn season_of(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("Season_")?.strip_suffix(".json")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) || digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

impl ArchiveStore {
    pub fn new(dir: impl Into<PathBuf>, content_dir: impl Into<PathBuf>) -> Self {
        ArchiveStore { dir: dir.into(), content_dir: content_dir.into() }
    }

    fn path(&self, season: u32) -> PathBuf {
        self.dir.join(record_name(season))
    }

    /// The download for `season`, sized from disk if it is there.
    pub fn world_for(&self, season: u32) -> WorldFile {
        let file = zip_name(season);
        let size = fs::symlink_metadata(self.content_dir.join(&file))
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        WorldFile { url: format!("{CONTENT_URL}/{file}"), file, size }
    }

    pub fn list(&self) -> Result<Vec<Listed>, StoreError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(e)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(season) = season_of(&name) else { continue };
            let (archive, error) = match self.read(season) {
                Ok(Some((archive, _))) => (Some(archive), None),
                Ok(None) => continue,
                Err(StoreError::Invalid(why)) => (None, Some(why)),
                Err(e) => return Err(e),
            };
            out.push(Listed { season, url: format!("/archives/{name}"), file: name, archive, error });
        }
        out.sort_by(|a, b| b.season.cmp(&a.season));
        Ok(out)
    }

    /// The record and its version, or `None` if there is none.
    pub fn read(&self, season: u32) -> Result<Option<(Archive, String)>, StoreError> {
        let path = self.path(season);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e)),
        };
        if !meta.is_file() || meta.len() > 4 * 1024 * 1024 {
            return Err(StoreError::Invalid("Not a regular record file".into()));
        }
        let bytes = fs::read(&path).map_err(StoreError::Io)?;
        let archive: Archive = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Invalid(format!("Not a valid archive record: {e}")))?;
        if archive.season != season {
            return Err(StoreError::Invalid(format!(
                "The file is named for season {season} but records season {}",
                archive.season
            )));
        }
        Ok(Some((archive, version_of(&bytes))))
    }

    /// Remove a record this process has just created, when the step after it
    /// failed. Never used on a record anyone could have edited since.
    pub fn remove_new(&self, season: u32) -> io::Result<()> {
        fs::remove_file(self.path(season))
    }

    pub fn exists(&self, season: u32) -> bool {
        fs::symlink_metadata(self.path(season)).is_ok()
    }

    /// Save a record from the panel's editor.
    ///
    /// `create` makes a new one and refuses to replace; otherwise the record
    /// must exist and still be at `version` -- unless it is unreadable, in
    /// which case saving is how it gets repaired.
    pub fn save(
        &self,
        input: &ArchiveInput,
        version: Option<&str>,
        create: bool,
        by: &str,
    ) -> Result<Archive, StoreError> {
        let input = validate(input).map_err(StoreError::Invalid)?;
        let _guard = lock();

        let existing = match self.read(input.season) {
            Ok(found) => found,
            Err(StoreError::Invalid(_)) if !create => None,
            Err(e) => return Err(e),
        };
        let present = self.exists(input.season);
        if create && present {
            return Err(StoreError::Exists);
        }
        if !create && !present {
            return Err(StoreError::NotFound);
        }
        if let Some((_, current)) = &existing {
            if version != Some(current.as_str()) {
                return Err(StoreError::Conflict);
            }
        }

        let stamp = now();
        let (created_at, created_by) = match &existing {
            Some((old, _)) => (old.created_at.clone(), old.created_by.clone()),
            None => (stamp.clone(), by.to_string()),
        };
        let archive = Archive {
            schema: SCHEMA,
            season: input.season,
            theme: input.theme,
            started: input.started,
            ended: input.ended,
            minecraft_version: input.minecraft_version,
            world: input.has_world.then(|| self.world_for(input.season)),
            mods: input.mods,
            additional_info: input.additional_info,
            created_at,
            created_by,
            updated_at: stamp,
            updated_by: by.to_string(),
        };
        self.write(&archive, create)?;
        Ok(archive)
    }

    /// Write a record built by the server (End season). Never replaces one.
    pub fn create(&self, archive: &Archive) -> Result<(), StoreError> {
        let _guard = lock();
        self.write(archive, true)
    }

    fn write(&self, archive: &Archive, create_new: bool) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir).map_err(StoreError::Io)?;
        let mut bytes = serde_json::to_vec_pretty(archive).map_err(|e| StoreError::Io(e.into()))?;
        bytes.push(b'\n');
        let target = self.path(archive.season);
        write_public_file(&self.dir, &target, &bytes, create_new).map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => StoreError::Exists,
            _ => StoreError::Io(e),
        })
    }
}

/// Write a file the website serves: temporary file, flushed, world-readable,
/// then moved into place -- with `create_new`, only if nothing is there yet.
pub fn write_public_file(dir: &Path, target: &Path, bytes: &[u8], create_new: bool) -> io::Result<()> {
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        publish(&tmp, target, create_new)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Move a finished temporary file into place. With `create_new` the kernel
/// refuses if the name is taken (a hard link cannot replace), so two
/// publishers can never overwrite each other.
pub fn publish(tmp: &Path, target: &Path, create_new: bool) -> io::Result<()> {
    if create_new {
        fs::hard_link(tmp, target)?;
        fs::remove_file(tmp)
    } else {
        fs::rename(tmp, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("apird-archives-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(dir.join("content")).unwrap();
            Scratch(dir)
        }
        fn store(&self) -> ArchiveStore {
            ArchiveStore::new(self.0.join("archives"), self.0.join("content"))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn input(season: u32) -> ArchiveInput {
        ArchiveInput {
            season,
            theme: "  Skyblock ".into(),
            started: NaiveDate::from_ymd_opt(2026, 1, 1),
            ended: NaiveDate::from_ymd_opt(2026, 3, 1),
            minecraft_version: Some("1.21.1".into()),
            has_world: true,
            mods: vec![
                ModEntry { name: "zeta".into(), version: "1.0".into() },
                ModEntry { name: "Alpha".into(), version: " 2.0 ".into() },
            ],
            additional_info: "Line one\r\nLine two".into(),
        }
    }

    const KEYS: [&str; 13] = [
        "schema", "season", "theme", "started", "ended", "minecraft_version", "world", "mods",
        "additional_info", "created_at", "created_by", "updated_at", "updated_by",
    ];

    fn keys_of(path: &Path) -> Vec<String> {
        let text = fs::read_to_string(path).unwrap();
        // Order as written, not as a map would sort it.
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<(usize, String)> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| (text.find(&format!("\"{k}\"")).unwrap(), k.clone()))
            .collect();
        keys.sort();
        keys.into_iter().map(|(_, k)| k).collect()
    }

    #[test]
    fn every_record_has_the_same_keys_in_the_same_order_even_when_empty() {
        let s = Scratch::new();
        let store = s.store();
        store.save(&input(3), None, true, "bako").unwrap();
        let bare = ArchiveInput {
            season: 1,
            theme: String::new(),
            started: None,
            ended: None,
            minecraft_version: None,
            has_world: false,
            mods: vec![],
            additional_info: String::new(),
        };
        store.save(&bare, None, true, "bako").unwrap();
        for season in [1, 3] {
            assert_eq!(keys_of(&s.0.join(format!("archives/Season_{season}.json"))), KEYS);
        }
        let text = fs::read_to_string(s.0.join("archives/Season_1.json")).unwrap();
        assert!(text.contains("\"world\": null") && text.contains("\"started\": null"), "{text}");
    }

    #[test]
    fn saving_tidies_input_and_fills_in_what_is_the_servers() {
        let s = Scratch::new();
        fs::write(s.0.join("content/Season_3.zip"), b"zipzip").unwrap();
        let store = s.store();
        let saved = store.save(&input(3), None, true, "bako").unwrap();
        assert_eq!(saved.theme, "Skyblock");
        assert_eq!(saved.additional_info, "Line one\nLine two");
        assert_eq!(
            saved.mods,
            [ModEntry { name: "Alpha".into(), version: "2.0".into() }, ModEntry { name: "zeta".into(), version: "1.0".into() }]
        );
        assert_eq!(
            saved.world,
            Some(WorldFile { file: "Season_3.zip".into(), url: "/raindrippy/content/Season_3.zip".into(), size: Some(6) })
        );
        assert_eq!((saved.created_by.as_str(), saved.updated_by.as_str()), ("bako", "bako"));
    }

    #[test]
    fn create_never_replaces_and_edit_needs_the_current_version() {
        let s = Scratch::new();
        let store = s.store();
        store.save(&input(3), None, true, "bako").unwrap();
        assert!(matches!(store.save(&input(3), None, true, "x"), Err(StoreError::Exists)));
        assert!(matches!(store.save(&input(4), None, false, "x"), Err(StoreError::NotFound)));

        let (_, version) = store.read(3).unwrap().unwrap();
        assert!(matches!(store.save(&input(3), Some("stale"), false, "x"), Err(StoreError::Conflict)));
        let mut changed = input(3);
        changed.theme = "Fixed typo".into();
        let saved = store.save(&changed, Some(&version), false, "editor").unwrap();
        assert_eq!(saved.theme, "Fixed typo");
        assert_eq!((saved.created_by.as_str(), saved.updated_by.as_str()), ("bako", "editor"));
    }

    #[test]
    fn hand_broken_files_are_listed_with_why_and_can_be_repaired() {
        let s = Scratch::new();
        let store = s.store();
        fs::create_dir_all(s.0.join("archives")).unwrap();
        fs::write(s.0.join("archives/Season_2.json"), r#"{"season":2,"extra":true}"#).unwrap();
        fs::write(s.0.join("archives/notes.json"), "{}").unwrap();
        fs::write(s.0.join("archives/Season_07.json"), "{}").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1, "only Season_<n>.json files count");
        assert!(listed[0].archive.is_none() && listed[0].error.is_some());
        store.save(&input(2), None, false, "bako").unwrap();
        assert!(store.list().unwrap()[0].archive.is_some());
    }

    #[test]
    fn unknown_fields_are_refused_on_input() {
        let json = r#"{"season":1,"theme":"","started":null,"ended":null,"minecraft_version":null,
                       "has_world":false,"mods":[],"additional_info":"","url":"https://evil"}"#;
        assert!(serde_json::from_str::<ArchiveInput>(json).is_err());
        let json = r#"{"season":1,"theme":"","started":null,"ended":null,"minecraft_version":null,
                       "has_world":false,"mods":[{"name":"a","version":"1","url":"x"}],"additional_info":""}"#;
        assert!(serde_json::from_str::<ArchiveInput>(json).is_err());
    }

    #[test]
    fn validation_names_the_problem() {
        let bad = |f: &dyn Fn(&mut ArchiveInput)| {
            let mut i = input(3);
            f(&mut i);
            validate(&i).unwrap_err()
        };
        assert!(bad(&|i| i.season = 0).contains("Season"));
        assert!(bad(&|i| i.theme = "a\nb".into()).contains("Theme"));
        assert!(bad(&|i| i.ended = NaiveDate::from_ymd_opt(2025, 1, 1)).contains("end before"));
        assert!(bad(&|i| i.minecraft_version = Some("1.21 <b>".into())).contains("Minecraft"));
        assert!(bad(&|i| i.mods[0].name = "  ".into()).contains("Mod 1 needs a name"));
        assert!(bad(&|i| i.additional_info = "x\u{0007}".into()).contains("control"));
    }
}
