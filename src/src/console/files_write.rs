//! Writing the game server's files from the admin panel: editing text configs
//! (`console.edit`) and uploading new files (`console.upload`).
//!
//! Reading is in [`super::files`]; everything here builds on its checks --
//! every path is resolved, confined and re-verified exactly as a read is.
//!
//! ## What this protects, and what it does not
//!
//! Editing a server's configs is, unavoidably, control over how the server
//! and its mods behave; a mod can be configured into almost anything. That is
//! what the permissions are for, and why they are trusted ones. What these
//! rules guarantee is narrower and holds whoever is asking:
//!
//! 1. **No code.** Only text config formats (and, for uploads, images that
//!    really are images) are written. Jars, scripts and the folders the server
//!    loads code or commands from -- `mods/`, `kubejs/`, datapacks -- are
//!    read-only. Writing a jar is arbitrary code execution as the server's
//!    user, which here is root.
//! 2. **No reaching other panel permissions.** `ops.json`, the whitelist and
//!    the ban lists stay with the Players view (`console.players`). The
//!    `server.properties` keys that decide who can connect or command the
//!    server -- RCON, query, online mode, whitelist, op levels -- are locked
//!    ([`LOCKED_PROPERTIES`]), parsed the way Java parses them so an escaped
//!    or line-continued key cannot slip past.
//! 3. **Secrets stay secret, and stay put.** Editors see masked values. A
//!    masked value sent back is restored to the real one; an existing secret
//!    cannot be changed, removed, moved to another key or copied into another
//!    file. Nothing reveals one: not the errors, and not the file version,
//!    which is a keyed hash ([`file_version`]).
//! 4. **Nothing is lost.** Every write is atomic (temp file, then rename),
//!    refused if the file changed since it was loaded, and preceded by a
//!    backup outside the server directory. If the backup cannot be made, the
//!    write does not happen.
//! 5. **Attributed.** The routes record every attempt -- refusals included --
//!    in the console audit log before anything is written.
//!
//! Everything here is blocking filesystem work; callers run it on the
//! blocking pool. Writes are serialized process-wide, so two saves cannot
//! interleave their version check and rename.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::files::{
    self as files, base_name, handle_path, io_error, is_mask, join, secret_line, FileError,
    Resolved, ServerFiles, DENIED_CREDENTIALS, MASK,
};

/// Largest file the editor loads or saves: everything the viewer shows.
pub const MAX_EDIT_BYTES: usize = files::MAX_PREVIEW_BYTES;

/// Largest single upload.
pub const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Backups kept per file. Older ones are pruned on the next write.
const BACKUPS_KEPT: usize = 20;

/// Formats the editor opens and uploads accept as text.
pub const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "yml", "yaml", "json", "json5", "jsonc", "toml", "properties", "cfg", "conf",
    "config", "ini", "hocon", "snbt", "mcmeta", "csv", "lang",
];

/// Image formats uploads accept, with the bytes each must start with. Checked
/// so a file only gets in under an image name if it is one.
const IMAGE_SIGNATURES: &[(&str, &[u8])] = &[
    ("png", b"\x89PNG\r\n\x1a\n"),
    ("jpg", b"\xff\xd8\xff"),
    ("jpeg", b"\xff\xd8\xff"),
    ("gif", b"GIF8"),
    ("webp", b"RIFF"),
];

const ACCESS_LISTS: &str =
    "Who is op, whitelisted or banned is managed from the Players view, not by editing the file";
const SERVER_LOGS: &str = "Logs are the server's own record and are never edited";
const SERVER_CODE: &str =
    "This folder holds code or commands the server runs, which the panel never writes";

/// Paths the panel shows but never writes, with the reason given. Same
/// pattern syntax as the deny list: a bare name matches at any depth, and a
/// folder protects everything inside it.
pub const BUILTIN_READONLY: &[(&str, &str)] = &[
    ("ops.json", ACCESS_LISTS),
    ("whitelist.json", ACCESS_LISTS),
    ("banned-players.json", ACCESS_LISTS),
    ("banned-ips.json", ACCESS_LISTS),
    ("logs", SERVER_LOGS),
    ("crash-reports", SERVER_LOGS),
    (
        "plrCount.json",
        "Written by the server for the API; editing it would misreport the player count",
    ),
    ("mods", SERVER_CODE),
    ("kubejs", SERVER_CODE),
    ("scripts", SERVER_CODE),
    ("datapacks", SERVER_CODE),
    ("openloader", SERVER_CODE),
    ("global_packs", SERVER_CODE),
    ("libraries", SERVER_CODE),
    ("versions", SERVER_CODE),
];

/// `server.properties` keys the panel never changes: who can connect to the
/// server, and who can command it.
pub const LOCKED_PROPERTIES: &[&str] = &[
    "enable-rcon",
    "rcon.port",
    "rcon.password",
    "broadcast-rcon-to-ops",
    "enable-query",
    "query.port",
    "management-server-enabled",
    "management-server-host",
    "management-server-port",
    "management-server-secret",
    "management-server-tls-enabled",
    "management-server-tls-keystore",
    "management-server-tls-keystore-password",
    "server-ip",
    "server-port",
    "online-mode",
    "white-list",
    "enforce-whitelist",
    "op-permission-level",
    "function-permission-level",
];

/// The result of a save.
#[derive(Debug, Serialize)]
pub struct Saved {
    pub path: String,
    pub size: u64,
    /// The new contents' version, for the next save.
    pub version: String,
    /// Masked values put back to their real ones.
    pub restored: usize,
    /// Nothing differed, so nothing was written.
    pub unchanged: bool,
}

/// The result of an upload.
#[derive(Debug, Serialize)]
pub struct Uploaded {
    pub path: String,
    pub size: u64,
    /// An existing file was replaced (and backed up).
    pub replaced: bool,
    pub restored: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadKind {
    Text,
    Image,
}

fn write_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    // A panic mid-write leaves nothing half-done that the lock protects --
    // the rename either happened or it did not -- so a poisoned lock is safe
    // to take.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Identifies a file's exact contents without revealing anything about them.
///
/// Keyed with a random per-process secret: a plain hash of `server.properties`,
/// whose every byte but the masked password is on screen, would let its
/// holder brute-force that password offline. The cost is that versions do not
/// survive a restart, which makes an editor left open across one reload first.
pub fn file_version(bytes: &[u8]) -> String {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    let key = KEY.get_or_init(rand::random);
    let mut hash = Sha256::new();
    hash.update(key);
    hash.update(bytes);
    hash.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())
}

/// Whether a file name is a format the editor works on.
pub fn is_text_name(name: &str) -> bool {
    extension(name).is_some_and(|ext| TEXT_EXTENSIONS.contains(&ext.as_str()))
}

fn is_server_properties(rel: &str) -> bool {
    rel.eq_ignore_ascii_case("server.properties")
}

impl ServerFiles {
    /// Why `rel` can never be written, whatever is asking. Path policy only.
    pub(super) fn write_block(&self, rel: &str) -> Option<&'static str> {
        if rel.is_empty() {
            return Some("The server folder itself can't be written");
        }
        if self.is_denied(rel) {
            return Some(DENIED_CREDENTIALS);
        }
        if rel.split('/').any(|part| part.starts_with('.')) {
            return Some("Hidden files and folders are read-only in the panel");
        }
        for (pattern, why) in BUILTIN_READONLY {
            let pattern = pattern.to_ascii_lowercase();
            if files::matches_patterns([pattern.as_str()], rel) {
                return Some(why);
            }
        }
        if files::matches_patterns(self.readonly.iter().map(String::as_str), rel) {
            return Some("This path is marked read-only on this server");
        }
        None
    }

    /// Why the editor cannot open `rel` for editing: [`write_block`], plus
    /// format and configuration.
    ///
    /// [`write_block`]: ServerFiles::write_block
    pub(super) fn edit_block(&self, rel: &str) -> Option<&'static str> {
        if self.backup_dir.is_none() {
            return Some("Editing is turned off on this server");
        }
        self.write_block(rel).or_else(|| {
            (!is_text_name(&base_name(rel))).then_some("Only text config formats can be edited here")
        })
    }

    /// The backup folder, created if needed, and refused if it is inside the
    /// server directory: the browser would serve its unmasked copies.
    fn backup_root(&self, server_root: &Path) -> Result<PathBuf, FileError> {
        let dir = self.backup_dir.as_ref().ok_or(FileError::WritesDisabled(
            "Editing and uploads are turned off on this server",
        ))?;
        fs::create_dir_all(dir).map_err(|e| {
            log::error!("files: cannot create backup folder {}: {e}", dir.display());
            FileError::WritesDisabled("The backup folder can't be created, so nothing is written")
        })?;
        let canonical = fs::canonicalize(dir).map_err(io_error)?;
        if canonical.starts_with(server_root) {
            log::error!(
                "files: backup folder {} is inside the server directory; refusing writes",
                canonical.display()
            );
            return Err(FileError::WritesDisabled(
                "The backup folder is inside the server folder, where its unmasked copies could \
                 be read; nothing is written until it is moved",
            ));
        }
        Ok(canonical)
    }

    /// The verified handle of the folder `resolved` sits in.
    fn parent_of(&self, resolved: &Resolved) -> Result<(Resolved, File), FileError> {
        let parent = match resolved.rel.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => String::new(),
        };
        let (dir, handle, meta) = self.open(&parent)?;
        if !meta.is_dir() {
            return Err(FileError::NotADirectory);
        }
        Ok((dir, handle))
    }

    /// Replace a text file with edited contents.
    ///
    /// `text` is what the editor holds -- secrets still masked -- and
    /// `version` the [`file_version`] it was loaded at.
    pub fn save(&self, raw: &str, text: &str, version: &str) -> Result<Saved, FileError> {
        let _guard = write_lock();

        if text.len() > MAX_EDIT_BYTES {
            return Err(too_large_to_edit());
        }
        let (resolved, file, meta) = self.open(raw)?;
        if meta.is_dir() {
            return Err(FileError::IsADirectory);
        }
        if let Some(why) = self.edit_block(&resolved.rel) {
            return Err(FileError::ReadOnly(why));
        }
        let backups = self.backup_root(&resolved.root)?;
        if meta.len() > MAX_EDIT_BYTES as u64 {
            return Err(too_large_to_edit());
        }

        let mut bytes = Vec::new();
        file.take(MAX_EDIT_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(io_error)?;
        if bytes.len() > MAX_EDIT_BYTES {
            return Err(too_large_to_edit());
        }
        if file_version(&bytes) != version {
            return Err(FileError::Conflict);
        }
        if files::looks_binary(&bytes) {
            return Err(FileError::Rejected("Binary files can't be edited".to_string()));
        }
        let original = String::from_utf8(bytes)
            .map_err(|_| FileError::Rejected("Only UTF-8 text can be edited".to_string()))?;

        let (updated, restored) = apply_write_rules(&resolved.rel, Some(&original), text)?;
        if updated == original {
            return Ok(Saved {
                path: resolved.rel,
                size: meta.len(),
                version: version.to_string(),
                restored,
                unchanged: true,
            });
        }

        let (_, dir) = self.parent_of(&resolved)?;
        backup(&backups, &resolved.rel, original.as_bytes())?;
        let name = base_name(&resolved.rel);
        write_into(&dir, &resolved, &name, updated.as_bytes(), Some(&meta), Mode::Replace)?;

        Ok(Saved {
            version: file_version(updated.as_bytes()),
            size: updated.len() as u64,
            path: resolved.rel,
            restored,
            unchanged: false,
        })
    }

    /// Put an uploaded file into the folder `dir_raw`.
    ///
    /// Without `overwrite`, an existing file is never touched. With it, the
    /// existing file is replaced under the same rules as an edit -- the
    /// caller must also have checked the editing permission.
    pub fn upload(
        &self,
        dir_raw: &str,
        name: &str,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<Uploaded, FileError> {
        let _guard = write_lock();

        check_upload_name(name)?;
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(FileError::TooLarge(format!(
                "Uploads can be at most {} MiB",
                MAX_UPLOAD_BYTES / (1024 * 1024)
            )));
        }
        let kind = upload_kind(name, bytes)?;

        let (dir_res, dir, meta) = self.open(dir_raw)?;
        if !meta.is_dir() {
            return Err(FileError::NotADirectory);
        }
        let rel = join(&dir_res.rel, name);
        if let Some(why) = self.write_block(&rel) {
            return Err(FileError::ReadOnly(why));
        }
        let backups = self.backup_root(&dir_res.root)?;

        let dir_path = handle_path(&dir).unwrap_or_else(|| dir_res.abs.clone());
        let target = Resolved { root: dir_res.root.clone(), abs: dir_res.abs.join(name), rel };

        match fs::symlink_metadata(dir_path.join(name)) {
            Ok(existing) => {
                if !overwrite {
                    return Err(FileError::Exists);
                }
                if !existing.is_file() {
                    return Err(FileError::ReadOnly("Only a regular file can be replaced"));
                }
                // Through `open`, so the existing file gets every check a read
                // does -- deny list, hard links -- before it is read or backed up.
                let (_, old_file, old_meta) = self.open(&target.rel)?;
                if old_meta.len() > MAX_UPLOAD_BYTES as u64 {
                    return Err(FileError::TooLarge(
                        "The existing file is too large to replace by upload".to_string(),
                    ));
                }
                let mut old = Vec::new();
                old_file
                    .take(MAX_UPLOAD_BYTES as u64)
                    .read_to_end(&mut old)
                    .map_err(io_error)?;

                let (data, restored) = match kind {
                    UploadKind::Image => (bytes.to_vec(), 0),
                    UploadKind::Text => {
                        let original = std::str::from_utf8(&old).map_err(|_| {
                            FileError::Rejected(
                                "The existing file isn't UTF-8 text, so it can't be replaced by \
                                 upload"
                                    .to_string(),
                            )
                        })?;
                        let new = std::str::from_utf8(bytes).expect("checked by upload_kind");
                        let (text, restored) = apply_write_rules(&target.rel, Some(original), new)?;
                        (text.into_bytes(), restored)
                    }
                };

                backup(&backups, &target.rel, &old)?;
                write_into(&dir, &target, name, &data, Some(&old_meta), Mode::Replace)?;
                Ok(Uploaded { size: data.len() as u64, path: target.rel, replaced: true, restored })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let data = match kind {
                    UploadKind::Image => bytes.to_vec(),
                    UploadKind::Text => {
                        let new = std::str::from_utf8(bytes).expect("checked by upload_kind");
                        apply_write_rules(&target.rel, None, new)?.0.into_bytes()
                    }
                };
                write_into(&dir, &target, name, &data, None, Mode::CreateNew)?;
                Ok(Uploaded { size: data.len() as u64, path: target.rel, replaced: false, restored: 0 })
            }
            Err(e) => Err(io_error(e)),
        }
    }
}

fn too_large_to_edit() -> FileError {
    FileError::TooLarge(format!("Files over {} KiB can't be edited here", MAX_EDIT_BYTES / 1024))
}

/// A name for a new file: one plain, visible path component.
pub fn check_upload_name(name: &str) -> Result<(), FileError> {
    let bad = |why: &str| Err(FileError::Rejected(format!("Invalid file name: {why}")));
    if name.is_empty() || name.len() > 255 {
        return bad("it must be 1 to 255 bytes long");
    }
    if name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        return bad("it may not contain slashes or control characters");
    }
    if name.starts_with('.') {
        return bad("hidden names, starting with '.', can't be uploaded");
    }
    if name != name.trim() {
        return bad("it may not start or end with spaces");
    }
    Ok(())
}

/// What an upload is, by its extension, confirmed by its contents.
fn upload_kind(name: &str, bytes: &[u8]) -> Result<UploadKind, FileError> {
    let Some(ext) = extension(name) else {
        return Err(FileError::Rejected("Files without an extension can't be uploaded".into()));
    };
    if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
            return Err(FileError::Rejected(format!(
                "`{name}` isn't UTF-8 text, which a .{ext} file must be"
            )));
        }
        return Ok(UploadKind::Text);
    }
    if let Some((_, magic)) = IMAGE_SIGNATURES.iter().find(|(e, _)| *e == ext) {
        let webp_ok = ext != "webp" || bytes.get(8..12) == Some(b"WEBP");
        if !bytes.starts_with(magic) || !webp_ok {
            return Err(FileError::Rejected(format!("`{name}` isn't a valid .{ext} image")));
        }
        return Ok(UploadKind::Image);
    }
    Err(FileError::Rejected(format!(
        "`.{ext}` files can't be uploaded. Allowed: text and config formats ({}) and images \
         (png, jpg, gif, webp). Mods, plugins and scripts never can be.",
        TEXT_EXTENSIONS.join(", ")
    )))
}

/// Every rule a text write must pass, applied to what the writer sent.
///
/// Returns the text to write -- secrets restored, line endings matched to the
/// original -- and how many masked values were restored.
pub(super) fn apply_write_rules(
    rel: &str,
    original: Option<&str>,
    new: &str,
) -> Result<(String, usize), FileError> {
    if new.contains('\0') {
        return Err(FileError::Rejected("Text files can't contain NUL bytes".into()));
    }

    // A browser's textarea hands back LF whatever it was given; keep a CRLF
    // file CRLF rather than rewriting every line of it.
    let mut text = new.to_string();
    if original.is_some_and(|o| o.contains("\r\n")) && !text.contains('\r') {
        text = text.replace('\n', "\r\n");
    }

    let (text, restored) = if files::is_maskable(&base_name(rel)) {
        restore_secrets(original, &text)?
    } else {
        (text, 0)
    };

    if is_server_properties(rel) {
        check_locked_properties(original, &text)?;
    }
    Ok((text, restored))
}

/// Put masked values back, and refuse any change to an existing secret.
///
/// Secrets are matched by key, in order, so the third masked `token` gets the
/// third original `token`'s value. That a masked value can only land under its
/// own key, in its own file, is what stops it being moved somewhere it would
/// be displayed or used differently. And a key that held a secret cannot be
/// given a new value alongside -- otherwise "move the masked line elsewhere
/// and set my own value here" would replace a secret one step at a time.
fn restore_secrets(original: Option<&str>, new: &str) -> Result<(String, usize), FileError> {
    let mut held: HashMap<&str, VecDeque<&str>> = HashMap::new();
    if let Some(original) = original {
        for line in original.split_inclusive('\n') {
            if let Some(found) = secret_line(line) {
                held.entry(found.key).or_default().push_back(&line[found.value]);
            }
        }
    }
    let had: HashSet<&str> = held.keys().copied().collect();

    let mut out = String::with_capacity(new.len());
    let mut restored = 0;
    for (n, line) in new.split_inclusive('\n').enumerate() {
        match secret_line(line) {
            Some(found) if is_mask(&line[found.value.clone()]) => {
                let Some(value) = held.get_mut(found.key).and_then(VecDeque::pop_front) else {
                    return Err(FileError::Rejected(format!(
                        "Line {}: `{}` shows a masked value with no saved secret behind it. A \
                         masked value can only stay where it was, not be copied or added.",
                        n + 1,
                        found.key
                    )));
                };
                out.push_str(&line[..found.value.start]);
                out.push_str(value);
                out.push_str(&line[found.value.end..]);
                restored += 1;
            }
            Some(found) if had.contains(found.key) => {
                return Err(FileError::Rejected(format!(
                    "Line {}: `{}` holds a secret, and existing secrets can't be changed from \
                     the panel. Leave it showing {MASK}.",
                    n + 1,
                    found.key
                )));
            }
            _ => out.push_str(line),
        }
    }

    let mut removed: Vec<&str> =
        held.iter().filter(|(_, left)| !left.is_empty()).map(|(key, _)| *key).collect();
    if !removed.is_empty() {
        removed.sort_unstable();
        return Err(FileError::Rejected(format!(
            "`{}` was removed. Existing secrets can't be removed from the panel; put the masked \
             line back.",
            removed.join("`, `")
        )));
    }
    Ok((out, restored))
}

/// Refuse a `server.properties` that changes, adds or drops a locked key.
fn check_locked_properties(original: Option<&str>, new: &str) -> Result<(), FileError> {
    let before = locked_values(original.unwrap_or(""));
    let after = locked_values(new);
    for key in LOCKED_PROPERTIES {
        if before.get(*key) != after.get(*key) {
            return Err(FileError::Rejected(format!(
                "`{key}` can't be changed from the panel: it controls who can connect to or \
                 command the server."
            )));
        }
    }
    Ok(())
}

fn locked_values(text: &str) -> HashMap<String, String> {
    // Later entries win, as they do for Java.
    java_properties(text)
        .into_iter()
        .filter(|(key, _)| LOCKED_PROPERTIES.contains(&key.as_str()))
        .collect()
}

/// The locked keys present in a file, for the editor to point out.
pub fn locked_keys_in(rel: &str, text: &str) -> Vec<String> {
    if !is_server_properties(rel) {
        return Vec::new();
    }
    let present = locked_values(text);
    LOCKED_PROPERTIES
        .iter()
        .filter(|key| present.contains_key(**key))
        .map(|key| key.to_string())
        .collect()
}

/// Parse `.properties` text the way `java.util.Properties` does, which is how
/// the server reads it.
///
/// Matters because a simpler reading is a bypass: `rcon\u002Epassword=x`, or
/// `rcon.pass\` continued onto a line starting `word=x`, are both
/// `rcon.password` to Java.
fn java_properties(text: &str) -> Vec<(String, String)> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    let mut logical = String::new();
    let mut continuing = false;

    for line in text.split('\n') {
        let line = line.trim_start_matches([' ', '\t', '\x0c']);
        if continuing {
            logical.push_str(line);
        } else {
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            logical = line.to_string();
        }
        let slashes = logical.chars().rev().take_while(|&c| c == '\\').count();
        if slashes % 2 == 1 {
            logical.pop();
            continuing = true;
            continue;
        }
        continuing = false;
        out.push(parse_property(&logical));
    }
    if continuing {
        out.push(parse_property(&logical));
    }
    out
}

fn parse_property(line: &str) -> (String, String) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '=' | ':' => break,
            c if c.is_whitespace() => break,
            _ => i += 1,
        }
    }
    let key_end = i.min(chars.len());
    let mut j = key_end;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    if j < chars.len() && (chars[j] == '=' || chars[j] == ':') {
        j += 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
    }
    (unescape_property(&chars[..key_end]), unescape_property(&chars[j..]))
}

fn unescape_property(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        i += 2;
        match next {
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            'f' => out.push('\x0c'),
            'u' => {
                let hex: String = chars[i..chars.len().min(i + 4)].iter().collect();
                match (hex.len() == 4).then(|| u32::from_str_radix(&hex, 16).ok()).flatten() {
                    Some(code) => {
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    // Java rejects a malformed \u outright. Keeping it as
                    // written can only make the key differ from a locked one.
                    None => out.push_str("\\u"),
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Copy the current contents aside before they are replaced.
///
/// Kept per file, newest [`BACKUPS_KEPT`] only, owner-readable only: these
/// are unmasked. A failure here stops the write.
fn backup(root: &Path, rel: &str, bytes: &[u8]) -> Result<(), FileError> {
    let encoded = urlencoding::encode(rel).into_owned();
    let folder_name = if encoded.len() > 200 {
        // Past what a file name can hold. Keep the readable end, and a hash
        // so two long paths cannot share a folder.
        let digest: String =
            Sha256::digest(rel.as_bytes())[..8].iter().map(|b| format!("{b:02x}")).collect();
        let tail: String = encoded.chars().rev().take(150).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{digest}-{tail}")
    } else {
        encoded
    };
    let folder = root.join(folder_name);
    let failed = |e: io::Error| {
        log::error!("files: cannot back up {rel}: {e}");
        FileError::WritesDisabled("The previous version could not be backed up, so nothing was written")
    };
    fs::create_dir_all(&folder).map_err(failed)?;
    set_mode(&folder, 0o700);

    // Time, then a counter: writes are serialized, so the counter orders
    // backups made within one clock tick, and names sort oldest first.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
    let path = folder.join(format!("{stamp}-{seq:010}.bak"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(failed)?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(failed)?;

    // Prune. Names sort by time.
    if let Ok(entries) = fs::read_dir(&folder) {
        let mut names: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "bak"))
            .collect();
        names.sort();
        let excess = names.len().saturating_sub(BACKUPS_KEPT);
        for old in &names[..excess] {
            let _ = fs::remove_file(old);
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Atomically replace whatever is at the name.
    Replace,
    /// Fail with [`FileError::Exists`] if anything is.
    CreateNew,
}

/// Write `data` as `name` inside the verified folder `dir`.
///
/// Written to a temporary file beside it, flushed, then renamed into place, so
/// the server never reads half a file and a crash leaves the old one intact.
/// Everything goes through `dir`'s handle rather than its path on Linux, so
/// the folder written into is the one that was checked.
fn write_into(
    dir: &File,
    target: &Resolved,
    name: &str,
    data: &[u8],
    previous: Option<&Metadata>,
    mode: Mode,
) -> Result<(), FileError> {
    let dir_path = handle_path(dir).unwrap_or_else(|| {
        target.abs.parent().map(Path::to_path_buf).unwrap_or_else(|| target.root.clone())
    });
    let tmp = dir_path.join(format!(".rdtmp-{}", uuid::Uuid::new_v4().simple()));
    let dest = dir_path.join(name);

    let result = (|| -> io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            options.custom_flags(libc::O_NOFOLLOW);
            options.mode(previous.map_or(0o644, |m| m.mode() & 0o7777));
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;

        // Keep the owner the server expects: the previous file's, or for a new
        // one the folder's. Only possible as root; harmless to fail otherwise.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owner = match previous {
                Some(m) => Some((m.uid(), m.gid())),
                None => dir.metadata().ok().map(|m| (m.uid(), m.gid())),
            };
            if let Some((uid, gid)) = owner {
                let _ = std::os::unix::fs::fchown(&file, Some(uid), Some(gid));
            }
        }
        file.sync_all()?;
        drop(file);

        match mode {
            Mode::Replace => fs::rename(&tmp, &dest)?,
            // A hard link fails if the name is taken, which a rename would
            // not: the no-replace guarantee comes from the kernel, not from a
            // check made earlier.
            Mode::CreateNew => {
                fs::hard_link(&tmp, &dest)?;
                fs::remove_file(&tmp)?;
            }
        }
        let _ = dir.sync_all();
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(match e.kind() {
            io::ErrorKind::AlreadyExists if mode == Mode::CreateNew => FileError::Exists,
            io::ErrorKind::PermissionDenied => {
                FileError::WritesDisabled("The API is not allowed to write in this folder")
            }
            _ => {
                log::error!("files: cannot write {}: {e}", target.rel);
                FileError::Io(e)
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch server directory and backup folder, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("apird-writes-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(dir.join("server")).unwrap();
            Scratch(dir)
        }
        fn write(&self, rel: &str, contents: &[u8]) {
            let path = self.0.join("server").join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.0.join("server").join(rel)).unwrap()
        }
        fn files(&self) -> ServerFiles {
            ServerFiles::new(self.0.join("server"), &[])
                .with_writes(&["plugins/Locked".to_string()], self.0.join("backups").to_str().unwrap())
        }
        fn backups(&self, rel: &str) -> Vec<String> {
            let folder = self.0.join("backups").join(urlencoding::encode(rel).as_ref());
            let mut names: Vec<String> = fs::read_dir(folder)
                .map(|d| d.filter_map(Result::ok).map(|e| fs::read_to_string(e.path()).unwrap()).collect())
                .unwrap_or_default();
            names.sort();
            names
        }
        /// Open in the viewer, then save `edit(viewer text)`.
        fn edit(&self, rel: &str, edit: impl FnOnce(&str) -> String) -> Result<Saved, FileError> {
            let files = self.files();
            let preview = files.preview(rel).unwrap();
            assert!(preview.editable, "{rel}: {:?}", preview.readonly_reason);
            let text = edit(preview.text.as_deref().unwrap());
            files.save(rel, &text, preview.version.as_deref().unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rejected<T: std::fmt::Debug>(r: Result<T, FileError>) -> String {
        match r {
            Err(FileError::Rejected(why)) => why,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    const PROPS: &str = "#Minecraft server properties\nmotd=Hello\nmax-players=20\n\
                         enable-rcon=true\nrcon.password=hunter2\nrcon.port=29875\nonline-mode=true\n";

    #[test]
    fn a_masked_file_saves_with_its_secrets_intact() {
        let s = Scratch::new();
        s.write("server.properties", PROPS.as_bytes());
        let saved = s.edit("server.properties", |t| t.replace("motd=Hello", "motd=Welcome")).unwrap();
        assert_eq!(saved.restored, 1);
        assert!(!saved.unchanged);
        assert_eq!(s.read("server.properties"), PROPS.replace("motd=Hello", "motd=Welcome"));
        assert_eq!(s.backups("server.properties"), [PROPS.to_string()], "the old version is kept");
    }

    #[test]
    fn saving_without_changes_writes_nothing() {
        let s = Scratch::new();
        s.write("server.properties", PROPS.as_bytes());
        let saved = s.edit("server.properties", str::to_string).unwrap();
        assert!(saved.unchanged);
        assert!(s.backups("server.properties").is_empty());
    }

    #[test]
    fn locked_server_properties_cannot_change_however_spelt() {
        let s = Scratch::new();
        s.write("server.properties", PROPS.as_bytes());
        let edits: [fn(&str) -> String; 7] = [
            |t: &str| t.replace("online-mode=true", "online-mode=false"),
            |t: &str| t.replace("rcon.port=29875", "rcon.port=1"),
            |t: &str| t.replace("enable-rcon=true\n", ""),
            |t: &str| format!("{t}white-list=false\n"),
            // Java reads each of these as a locked key.
            |t: &str| format!("{t}online\\u002Dmode=false\n"),
            |t: &str| format!("{t}online-\\\n    mode=false\n"),
            |t: &str| format!("{t}  online-mode : false\n"),
        ];
        for edit in edits {
            let why = rejected(s.edit("server.properties", edit));
            assert!(why.contains("can't be changed from the panel"), "{why}");
        }
        assert_eq!(s.read("server.properties"), PROPS);
    }

    #[test]
    fn existing_secrets_cannot_be_changed_removed_or_swapped() {
        let s = Scratch::new();
        let yml = "web:\n  password: hunter2\nbot:\n  token: \"abc\"\n";
        s.write("plugins/Thing/config.yml", yml.as_bytes());
        let rel = "plugins/Thing/config.yml";

        let why = rejected(s.edit(rel, |t| t.replace("password: ********", "password: mine")));
        assert!(why.contains("existing secrets can't be changed"), "{why}");

        let why = rejected(s.edit(rel, |t| t.replace("  password: ********\n", "")));
        assert!(why.contains("was removed"), "{why}");

        // Moving the masked line away and setting a value in its place.
        let why = rejected(s.edit(rel, |t| {
            t.replace("web:\n  password: ********\n", "unused:\n  password: ********\nweb:\n  password: mine\n")
        }));
        assert!(why.contains("existing secrets can't be changed"), "{why}");

        // Copying a mask under another key, or adding one out of nowhere.
        let why = rejected(s.edit(rel, |t| format!("{t}other:\n  apikey: ********\n")));
        assert!(why.contains("no saved secret behind it"), "{why}");

        assert_eq!(s.read(rel), yml);
    }

    #[test]
    fn a_masked_value_cannot_be_carried_into_another_file() {
        let s = Scratch::new();
        s.write("server.properties", PROPS.as_bytes());
        s.write("config/leak.toml", b"note = \"\"\n");
        let files = s.files();
        let leak = files.preview("config/leak.toml").unwrap();
        let why = rejected(files.save(
            "config/leak.toml",
            "note = \"\"\nrcon.password = ********\n",
            leak.version.as_deref().unwrap(),
        ));
        assert!(why.contains("no saved secret behind it"), "{why}");
    }

    #[test]
    fn new_secrets_and_moves_within_a_key_are_allowed() {
        let s = Scratch::new();
        let yml = "a:\n  token: abc\n";
        s.write("config/bot.yml", yml.as_bytes());
        s.edit("config/bot.yml", |t| format!("# moved\n{t}b:\n  webhook: https://example.test/x\n")).unwrap();
        assert_eq!(
            s.read("config/bot.yml"),
            "# moved\na:\n  token: abc\nb:\n  webhook: https://example.test/x\n"
        );
    }

    #[test]
    fn duplicate_secret_keys_restore_in_order() {
        let s = Scratch::new();
        let pretty = "{\n  \"token\": \"first\",\n  \"x\": 1,\n  \"token\": \"second\"\n}\n";
        s.write("config/a.json", pretty.as_bytes());
        s.edit("config/a.json", |t| t.replace("\"x\": 1", "\"x\": 2")).unwrap();
        assert_eq!(s.read("config/a.json"), pretty.replace("\"x\": 1", "\"x\": 2"));
    }

    #[test]
    fn a_stale_version_is_a_conflict_and_a_bogus_one_too() {
        let s = Scratch::new();
        s.write("config/a.toml", b"x = 1\n");
        let files = s.files();
        let preview = files.preview("config/a.toml").unwrap();
        s.write("config/a.toml", b"x = 2\n");
        assert!(matches!(
            files.save("config/a.toml", "x = 3\n", preview.version.as_deref().unwrap()),
            Err(FileError::Conflict)
        ));
        assert!(matches!(files.save("config/a.toml", "x = 3\n", "nope"), Err(FileError::Conflict)));
        assert_eq!(s.read("config/a.toml"), "x = 2\n");
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let s = Scratch::new();
        s.write("config/w.ini", b"a=1\r\nb=2\r\n");
        s.edit("config/w.ini", |t| t.replace("\r\n", "\n").replace("b=2", "b=3")).unwrap();
        assert_eq!(s.read("config/w.ini"), "a=1\r\nb=3\r\n");
    }

    #[test]
    fn protected_paths_and_formats_are_read_only() {
        let s = Scratch::new();
        for (rel, contents) in [
            ("ops.json", "[]"),
            ("whitelist.json", "[]"),
            ("logs/latest.log", "x"),
            ("mods/thing.toml", "x = 1"),
            ("kubejs/server_scripts/a.js", "x"),
            ("world/datapacks/p/pack.mcmeta", "{}"),
            ("plugins/Locked/config.yml", "a: 1"),
            (".hidden/a.yml", "a: 1"),
            ("start.sh", "x"),
        ] {
            s.write(rel, contents.as_bytes());
            let preview = s.files().preview(rel).unwrap();
            assert!(!preview.editable, "{rel} must not be editable");
            assert!(preview.readonly_reason.is_some(), "{rel}");
            assert!(preview.version.is_none(), "{rel}");
            assert!(
                matches!(s.files().save(rel, "changed", "x"), Err(FileError::ReadOnly(_))),
                "{rel}"
            );
        }
    }

    #[test]
    fn without_a_backup_folder_nothing_is_written() {
        let s = Scratch::new();
        s.write("config/a.toml", b"x = 1\n");
        let off = ServerFiles::new(s.0.join("server"), &[]);
        let preview = off.preview("config/a.toml").unwrap();
        assert!(!preview.editable);
        assert!(off.upload("config", "b.toml", b"y = 1\n", false).is_err());

        // A backup folder inside the server folder would be served unmasked.
        let inside = ServerFiles::new(s.0.join("server"), &[])
            .with_writes(&[], s.0.join("server/backups").to_str().unwrap());
        let preview = inside.preview("config/a.toml").unwrap();
        assert!(matches!(
            inside.save("config/a.toml", "x = 2\n", preview.version.as_deref().unwrap()),
            Err(FileError::WritesDisabled(_))
        ));
        assert_eq!(s.read("config/a.toml"), "x = 1\n");
    }

    #[cfg(unix)]
    #[test]
    fn saving_through_a_symlink_writes_the_real_file_and_keeps_the_link() {
        let s = Scratch::new();
        s.write("config/real.toml", b"x = 1\n");
        std::os::unix::fs::symlink("real.toml", s.0.join("server/config/link.toml")).unwrap();
        s.edit("config/link.toml", |t| t.replace('1', "2")).unwrap();
        assert_eq!(s.read("config/real.toml"), "x = 2\n");
        assert!(fs::symlink_metadata(s.0.join("server/config/link.toml")).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_are_neither_read_nor_written() {
        let s = Scratch::new();
        s.write("authedPlayers.json", b"{\"secret\":1}");
        fs::create_dir_all(s.0.join("server/config")).unwrap();
        fs::hard_link(s.0.join("server/authedPlayers.json"), s.0.join("server/config/x.json"))
            .unwrap();
        assert!(matches!(s.files().preview("config/x.json"), Err(FileError::Restricted(_))));
        assert!(matches!(s.files().save("config/x.json", "{}", "x"), Err(FileError::Restricted(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_save_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new();
        s.write("config/a.toml", b"x = 1\n");
        let path = s.0.join("server/config/a.toml");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        s.edit("config/a.toml", |t| t.replace('1', "2")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn backups_are_capped_per_file() {
        let s = Scratch::new();
        s.write("config/a.toml", b"x = 0\n");
        for n in 1..=(BACKUPS_KEPT + 3) {
            s.edit("config/a.toml", |_| format!("x = {n}\n")).unwrap();
        }
        let kept = s.backups("config/a.toml");
        assert_eq!(kept.len(), BACKUPS_KEPT);
        assert!(kept.contains(&format!("x = {}\n", BACKUPS_KEPT + 2)), "the newest is kept");
        assert!(!kept.contains(&"x = 0\n".to_string()), "the oldest went");
    }

    #[test]
    fn uploads_create_new_files_and_never_replace_unasked() {
        let s = Scratch::new();
        s.write("config/keep.toml", b"x = 1\n");
        let files = s.files();
        let up = files.upload("config", "new.toml", b"y = 2\n", false).unwrap();
        assert_eq!((up.path.as_str(), up.replaced), ("config/new.toml", false));
        assert_eq!(s.read("config/new.toml"), "y = 2\n");

        assert!(matches!(files.upload("config", "keep.toml", b"x = 9\n", false), Err(FileError::Exists)));
        assert_eq!(s.read("config/keep.toml"), "x = 1\n");

        let up = files.upload("config", "keep.toml", b"x = 9\n", true).unwrap();
        assert!(up.replaced);
        assert_eq!(s.read("config/keep.toml"), "x = 9\n");
        assert_eq!(s.backups("config/keep.toml"), ["x = 1\n".to_string()]);
        let leftovers: Vec<_> = fs::read_dir(s.0.join("server/config"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".rdtmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files are cleaned up");
    }

    #[test]
    fn an_empty_upload_creates_an_empty_file_under_the_same_rules() {
        let s = Scratch::new();
        s.write("config/existing.toml", b"x = 1\n");
        let files = s.files();
        let made = files.upload("config", "new.yml", b"", false).unwrap();
        assert_eq!((made.path.as_str(), made.size, made.replaced), ("config/new.yml", 0, false));
        assert_eq!(s.read("config/new.yml"), "");
        assert!(files.preview("config/new.yml").unwrap().editable, "it opens straight in the editor");

        assert!(matches!(files.upload("config", "existing.toml", b"", false), Err(FileError::Exists)));
        assert_eq!(s.read("config/existing.toml"), "x = 1\n");
        assert!(matches!(files.upload("config", "run.sh", b"", false), Err(FileError::Rejected(_))));
        fs::create_dir_all(s.0.join("server/mods")).unwrap();
        assert!(matches!(files.upload("mods", "a.toml", b"", false), Err(FileError::ReadOnly(_))));
    }

    #[test]
    fn uploads_refuse_code_bad_names_and_impostor_images() {
        let s = Scratch::new();
        fs::create_dir_all(s.0.join("server/plugins")).unwrap();
        let files = s.files();
        for (dir, name, bytes) in [
            ("plugins", "evil.jar", &b"PK\x03\x04"[..]),
            ("", "run.sh", b"#!/bin/sh\n"),
            ("", "script.js", b"x"),
            ("", "noext", b"x"),
            ("", ".env", b"A=1"),
            ("", "a/b.txt", b"x"),
            ("", "a\\b.txt", b"x"),
            ("", " spaced.txt", b"x"),
            ("", "nul.txt", b"a\0b"),
            ("", "bad-utf8.yml", b"\xff\xfe"),
            ("", "fake.png", b"<html>not a png</html>"),
            ("", "fake.webp", b"RIFF\0\0\0\0WAVE"),
        ] {
            let result = files.upload(dir, name, bytes, false);
            assert!(matches!(result, Err(FileError::Rejected(_))), "{dir}/{name}: {result:?}");
        }
        assert!(files.upload("", "server-icon.png", b"\x89PNG\r\n\x1a\n....", false).is_ok());
    }

    #[test]
    fn uploads_follow_the_same_path_and_secret_rules_as_edits() {
        let s = Scratch::new();
        s.write("server.properties", PROPS.as_bytes());
        s.write("logs/latest.log", b"x");
        let files = s.files();

        assert!(matches!(files.upload("logs", "fake.txt", b"x", false), Err(FileError::ReadOnly(_))));
        assert!(matches!(files.upload("", "ops.json", b"[]", true), Err(FileError::ReadOnly(_))));
        assert!(matches!(files.upload("", "authedPlayers.json", b"{}", false), Err(FileError::ReadOnly(_))));
        assert!(matches!(files.upload("../", "x.txt", b"x", false), Err(FileError::BadPath(_))));

        // Replacing server.properties wholesale is still an edit of it.
        let mine = PROPS.replace("rcon.password=hunter2", "rcon.password=mine");
        rejected(files.upload("", "server.properties", mine.as_bytes(), true));
        let masked = PROPS.replace("rcon.password=hunter2", "rcon.password=********").replace("max-players=20", "max-players=30");
        files.upload("", "server.properties", masked.as_bytes(), true).unwrap();
        assert_eq!(s.read("server.properties"), PROPS.replace("max-players=20", "max-players=30"));
    }

    #[test]
    fn a_new_server_properties_cannot_bring_locked_keys() {
        let s = Scratch::new();
        fs::create_dir_all(s.0.join("server/sub")).unwrap();
        let files = s.files();
        rejected(files.upload("", "server.properties", b"enable-rcon=true\n", false));
        // Only the root one is the server's.
        files.upload("sub", "server.properties", b"enable-rcon=true\n", false).unwrap();
    }

    #[test]
    fn java_properties_parsing_matches_java() {
        let parsed = java_properties(
            "# comment\n! also\n  a = 1\nb:2\nc 3\nd\\u002Ee=4\nf\\\n   g=5\nh=six \\\n  seven\nkey\\ with\\ space=8\n",
        );
        let want = [
            ("a", "1"),
            ("b", "2"),
            ("c", "3"),
            ("d.e", "4"),
            ("fg", "5"),
            ("h", "six seven"),
            ("key with space", "8"),
        ];
        let got: Vec<(&str, &str)> = parsed.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn versions_are_keyed_not_plain_hashes() {
        let v = file_version(b"rcon.password=hunter2\n");
        assert_eq!(v, file_version(b"rcon.password=hunter2\n"));
        assert_ne!(v, file_version(b"rcon.password=hunter3\n"));
        let plain: String = Sha256::digest(b"rcon.password=hunter2\n")[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_ne!(v, plain);
    }

    #[test]
    fn server_properties_reports_its_locked_keys() {
        assert_eq!(
            locked_keys_in("server.properties", PROPS),
            ["enable-rcon", "rcon.port", "rcon.password", "online-mode"]
        );
        assert!(locked_keys_in("config/server.properties", PROPS).is_empty());
    }
}
