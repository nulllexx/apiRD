//! Starting the next season once the last one is archived: the new seed in
//! `server.properties`, the season number and theme ServerSideUtils reads on
//! startup, and the old main world moved out of the way so the server
//! generates a fresh one.
//!
//! Only the main world and its own nether and end are touched. Every other
//! world on the server (an event hub, a test world, the auth world) is left
//! exactly where it is.
//!
//! Each step can be undone, and is undone if a later one fails before the
//! server starts again: the old properties and override are kept in memory,
//! and the worlds are moved aside with a rename rather than deleted, so a
//! failure part way leaves the season as it was. They are deleted only once
//! everything else has succeeded.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::console::files_write::java_properties;

/// Longest theme accepted. It is shown on the in-game scoreboard.
pub const MAX_THEME: usize = 64;
/// Longest text seed accepted. Minecraft hashes any text; numbers are used as-is.
pub const MAX_SEED: usize = 64;

/// What ServerSideUtils reads on startup in place of its built-in season and
/// theme. Field names are shared with the plugin; change both together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeasonOverride {
    pub season: u32,
    pub theme: String,
}

/// Check a theme for the next season.
pub fn clean_theme(theme: &str) -> Result<String, String> {
    let theme = theme.trim();
    if theme.is_empty() {
        return Err("The next season needs a theme".to_string());
    }
    if theme.chars().count() > MAX_THEME {
        return Err(format!("The theme can be at most {MAX_THEME} characters"));
    }
    if theme.chars().any(char::is_control) {
        return Err("The theme can't contain line breaks or control characters".to_string());
    }
    Ok(theme.to_string())
}

/// Check a seed. Empty means a random one (`None`).
///
/// Printable ASCII only: the server reads `server.properties` as Latin-1, so
/// anything else would not come back as typed.
pub fn clean_seed(seed: Option<&str>) -> Result<Option<String>, String> {
    let seed = seed.map(str::trim).unwrap_or("");
    if seed.is_empty() {
        return Ok(None);
    }
    if seed.len() > MAX_SEED {
        return Err(format!("The seed can be at most {MAX_SEED} characters"));
    }
    if !seed.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err("The seed can only contain letters, digits, spaces and ASCII punctuation".to_string());
    }
    Ok(Some(seed.to_string()))
}

/// The seed currently configured, if any.
pub fn current_seed(properties: &str) -> Option<String> {
    java_properties(properties)
        .into_iter()
        .filter(|(k, _)| k == "level-seed")
        .last()
        .map(|(_, v)| v)
        .filter(|v| !v.trim().is_empty())
}

/// Escape a value the way `Properties.store` would, so Java reads back
/// exactly `value`.
fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for (i, c) in value.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            ' ' if i == 0 => out.push_str("\\ "),
            '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `properties` with `level-seed` set to `seed` (blank for random) and every
/// other line exactly as it was.
///
/// Every existing `level-seed` entry is removed, continuation lines included,
/// and one is appended. The result is then read back the way Java reads it
/// and refused unless the seed is right and every other key is unchanged.
pub fn with_level_seed(properties: &str, seed: Option<&str>) -> Result<String, String> {
    let newline = if properties.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(properties.len() + 32);
    let mut continuing = false;
    let mut dropping = false;

    for line in properties.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let starts_entry = !continuing;
        if starts_entry {
            let trimmed = body.trim_start_matches([' ', '\t', '\x0c']);
            let is_comment = trimmed.starts_with('#') || trimmed.starts_with('!');
            dropping = !is_comment && !trimmed.is_empty() && is_level_seed_line(trimmed);
        }
        // A line continues if it ends in an odd number of backslashes, and
        // only entries (not comments) continue.
        let trailing = body.chars().rev().take_while(|&c| c == '\\').count();
        let trimmed_start = body.trim_start_matches([' ', '\t', '\x0c']);
        let comment = starts_entry && (trimmed_start.starts_with('#') || trimmed_start.starts_with('!'));
        continuing = !comment && trailing % 2 == 1;
        if !dropping {
            out.push_str(line);
        }
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(newline);
    }
    out.push_str("level-seed=");
    out.push_str(&escape_value(seed.unwrap_or("")));
    out.push_str(newline);

    // Read back as the server will.
    let before: Vec<(String, String)> =
        java_properties(properties).into_iter().filter(|(k, _)| k != "level-seed").collect();
    let after_all = java_properties(&out);
    let after: Vec<(String, String)> = after_all.iter().filter(|(k, _)| k != "level-seed").cloned().collect();
    let seeds: Vec<&String> = after_all.iter().filter(|(k, _)| k == "level-seed").map(|(_, v)| v).collect();
    if before != after || seeds.len() != 1 || seeds[0] != seed.unwrap_or("") {
        return Err("server.properties couldn't be updated safely; it was left alone".to_string());
    }
    Ok(out)
}

/// Whether a logical line (leading space trimmed) defines `level-seed`.
fn is_level_seed_line(line: &str) -> bool {
    // The key ends at the first unescaped '=', ':' or whitespace. Escapes in
    // the key are rare; reading the line with the real parser settles them.
    java_properties(line).first().is_some_and(|(k, _)| k == "level-seed")
        || line.strip_prefix("level-seed").is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(['=', ':', ' ', '\t', '\x0c'])
        })
}

/// Write `bytes` to `path` atomically (temp file beside it, then rename).
pub fn replace_file(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no parent folder"))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let written = options.open(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|_| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// What a file held before it was changed, to put back on failure.
#[derive(Debug)]
pub struct Saved {
    path: PathBuf,
    before: Option<Vec<u8>>,
}

impl Saved {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let before = match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        Ok(Saved { path: path.to_path_buf(), before })
    }

    pub fn before(&self) -> Option<&[u8]> {
        self.before.as_deref()
    }

    /// Put the file back as it was. Errors are returned for the job to report.
    pub fn restore(&self) -> io::Result<()> {
        match &self.before {
            Some(bytes) => replace_file(&self.path, bytes, 0o644),
            None => match fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        }
    }
}

/// The main world folders, moved aside into `staging` (inside the server
/// folder, so it is a rename on the same disk rather than a copy).
#[derive(Debug)]
pub struct MovedWorlds {
    moves: Vec<(PathBuf, PathBuf)>,
    staging: PathBuf,
}

/// Move `worlds` (folder names directly under `root`) into a new hidden
/// folder. All or nothing: if one cannot be moved, those already moved go
/// back.
pub fn move_worlds_aside(root: &Path, worlds: &[String], staging_name: &str) -> Result<MovedWorlds, String> {
    let staging = root.join(staging_name);
    fs::create_dir(&staging).map_err(|e| format!("A folder for the old world couldn't be made: {e}"))?;
    let mut moved = MovedWorlds { moves: Vec::new(), staging };
    for name in worlds {
        let from = root.join(name);
        let meta = match fs::symlink_metadata(&from) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                let _ = moved.put_back();
                return Err(format!("`{name}` couldn't be checked: {e}"));
            }
        };
        if !meta.is_dir() {
            let _ = moved.put_back();
            return Err(format!("`{name}` is no longer a plain folder; nothing was removed"));
        }
        let to = moved.staging.join(name);
        if let Err(e) = fs::rename(&from, &to) {
            let undo = moved.put_back();
            return Err(format!(
                "`{name}` couldn't be moved ({e}){}",
                match undo {
                    Ok(()) => "; the world was left as it was".to_string(),
                    Err(u) => format!(". Putting the others back also failed: {u}"),
                }
            ));
        }
        moved.moves.push((from, to));
    }
    Ok(moved)
}

impl MovedWorlds {
    /// Undo the move.
    pub fn put_back(&mut self) -> Result<(), String> {
        let mut failed = Vec::new();
        while let Some((original, aside)) = self.moves.pop() {
            if let Err(e) = fs::rename(&aside, &original) {
                failed.push(format!("{} ({e})", original.display()));
            }
        }
        if failed.is_empty() {
            let _ = fs::remove_dir(&self.staging);
            Ok(())
        } else {
            Err(format!(
                "these are still in {}: {}",
                self.staging.display(),
                failed.join(", ")
            ))
        }
    }

    /// Delete the old worlds for good. Symlinks inside are removed, never
    /// followed.
    pub fn delete(self) -> Result<(), String> {
        fs::remove_dir_all(&self.staging)
            .map_err(|e| format!("The old world couldn't be fully deleted from {} ({e}); delete it by hand", self.staging.display()))
    }
}

/// Every other world on the server: folders under `root` holding a
/// `level.dat`, other than `excluded`. Shown so it is clear what survives.
pub fn other_worlds(root: &Path, excluded: &[String]) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.') && !excluded.contains(name))
        .filter(|name| root.join(name).join("level.dat").is_file())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_and_themes_are_checked() {
        assert_eq!(clean_seed(None).unwrap(), None);
        assert_eq!(clean_seed(Some("  ")).unwrap(), None);
        assert_eq!(clean_seed(Some(" -4172144997902289642 ")).unwrap().as_deref(), Some("-4172144997902289642"));
        assert_eq!(clean_seed(Some("glacier")).unwrap().as_deref(), Some("glacier"));
        assert!(clean_seed(Some("a\nb")).is_err());
        assert!(clean_seed(Some("ä")).is_err());
        assert!(clean_seed(Some(&"9".repeat(65))).is_err());
        assert_eq!(clean_theme(" Ocean ").unwrap(), "Ocean");
        assert!(clean_theme("").is_err() && clean_theme("a\tb").is_err());
        assert!(clean_theme(&"x".repeat(65)).is_err());
    }

    #[test]
    fn the_seed_is_replaced_and_nothing_else_changes() {
        let props = "#Minecraft server properties\nlevel-name=world\nlevel-seed=123\nrcon.password=hunter2\nmotd=Hi\\: there\n";
        let out = with_level_seed(props, Some("glacier")).unwrap();
        assert_eq!(out, "#Minecraft server properties\nlevel-name=world\nrcon.password=hunter2\nmotd=Hi\\: there\nlevel-seed=glacier\n");
        assert_eq!(current_seed(&out).as_deref(), Some("glacier"));

        let random = with_level_seed(&out, None).unwrap();
        assert_eq!(current_seed(&random), None);
        assert!(random.ends_with("level-seed=\n"));
    }

    #[test]
    fn every_spelling_of_level_seed_is_replaced() {
        // Continued, spaced, colon-separated and duplicated entries all count.
        let props = "a=1\n  level-seed : 5\nlevel-seed=6\\\n  7\nb=2\\\n level-seed=not-a-key\nlevel-seed\n";
        let out = with_level_seed(props, Some("x = y")).unwrap();
        let parsed = java_properties(&out);
        assert_eq!(parsed.iter().filter(|(k, _)| k == "level-seed").count(), 1);
        assert_eq!(current_seed(&out).as_deref(), Some("x = y"));
        assert!(parsed.contains(&("b".to_string(), "2level-seed=not-a-key".to_string())), "a continuation is not a key");
        assert!(out.contains("level-seed=x \\= y"));
    }

    #[test]
    fn awkward_seeds_round_trip() {
        for seed in [" leading", "back\\slash", "#hash", "!bang", "a:b", "trailing "] {
            let out = with_level_seed("level-name=world\r\n", Some(seed)).unwrap();
            assert_eq!(java_properties(&out).last().unwrap().1, seed, "{out:?}");
            assert!(out.ends_with("\r\n"), "keeps the file's line endings");
        }
        assert_eq!(with_level_seed("", Some("1")).unwrap(), "level-seed=1\n");
        assert_eq!(with_level_seed("motd=x", Some("1")).unwrap(), "motd=x\nlevel-seed=1\n");
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apird-reset-{}", uuid::Uuid::new_v4()));
        for d in ["world/region", "world_nether/DIM-1", "eventhub", "testing"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in ["world/level.dat", "world/region/r.0.0.mca", "world_nether/level.dat", "eventhub/level.dat", "testing/level.dat"] {
            fs::write(dir.join(f), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn only_the_main_world_is_moved_and_deleted() {
        let root = scratch();
        let worlds = vec!["world".to_string(), "world_nether".to_string(), "world_the_end".to_string()];
        assert_eq!(other_worlds(&root, &worlds), ["eventhub", "testing"]);

        let moved = move_worlds_aside(&root, &worlds, ".old-season").unwrap();
        assert!(!root.join("world").exists() && !root.join("world_nether").exists());
        assert!(root.join(".old-season/world/region/r.0.0.mca").is_file());
        moved.delete().unwrap();
        assert!(!root.join(".old-season").exists());
        assert!(root.join("eventhub/level.dat").is_file() && root.join("testing/level.dat").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_move_puts_everything_back() {
        let root = scratch();
        // A file where a world folder is expected stops the move part way.
        fs::write(root.join("world_the_end"), b"not a folder").unwrap();
        let worlds = vec!["world".to_string(), "world_nether".to_string(), "world_the_end".to_string()];
        let err = move_worlds_aside(&root, &worlds, ".old-season").unwrap_err();
        assert!(err.contains("world_the_end"), "{err}");
        assert!(root.join("world/level.dat").is_file() && root.join("world_nether/level.dat").is_file());
        assert!(!root.join(".old-season").exists());

        let mut moved = move_worlds_aside(&root, &worlds[..2], ".old-season").unwrap();
        moved.put_back().unwrap();
        assert!(root.join("world/region/r.0.0.mca").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn deleting_never_follows_a_symlink_out() {
        let root = scratch();
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("world/link")).unwrap();
        move_worlds_aside(&root, &["world".to_string()], ".old").unwrap().delete().unwrap();
        assert!(outside.join("keep.txt").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saved_files_restore_or_disappear() {
        let root = scratch();
        let path = root.join("season-override.json");
        let saved = Saved::capture(&path).unwrap();
        replace_file(&path, b"{}", 0o644).unwrap();
        saved.restore().unwrap();
        assert!(!path.exists());

        fs::write(&path, b"old").unwrap();
        let saved = Saved::capture(&path).unwrap();
        replace_file(&path, b"new", 0o644).unwrap();
        saved.restore().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let _ = fs::remove_dir_all(&root);
    }
}
