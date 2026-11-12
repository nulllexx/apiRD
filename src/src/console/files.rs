//! Read-only access to the game server's files, for the admin panel's file
//! browser.
//!
//! The server directory is the most sensitive thing this API can reach. Beside
//! the world it holds `server.properties` — whose `rcon.password` opens the
//! RCON port that is published on the host, i.e. arbitrary commands without
//! `console.command` — and ServerSideUtils' `authedPlayers.json`, every
//! player's login credentials. So "browse the files" is built as a set of
//! independent defences, any one of which still holds if another is wrong:
//!
//! 1. **Gated on its own permission.** `console.files`, not `console.view`:
//!    watching the console never exposed configs, and granting it should not
//!    start to.
//! 2. **Read-only.** There is no write, upload, rename or delete path here at
//!    all, so nothing in this module can change the server.
//! 3. **Confined to the root.** A requested path is split into plain names
//!    (`..`, NUL and over-long paths are refused before the filesystem is
//!    touched), resolved with every symlink followed, and refused unless the
//!    result is still inside the canonical root. The file is then opened with
//!    `O_NOFOLLOW`, and on Linux the *opened handle* is checked again through
//!    `/proc/self/fd` — so a symlink swapped in between the check and the open
//!    (by a compromised server, which shares this directory and runs as root)
//!    is caught rather than followed.
//! 4. **Credential stores are never served.** [`BUILTIN_DENY`] plus anything
//!    in `SERVER_FILES_DENY`. Checked against the path as requested *and* as
//!    resolved, so a harmless-looking symlink to a denied file is denied too,
//!    and against every ancestor, so a denied directory hides its contents.
//! 5. **Secrets in configs are masked** — in previews *and* downloads, which
//!    share one code path — by [`redact_secrets`]. It masks the value of any
//!    `key = value` / `key: value` line whose key reads like a credential.
//!    A heuristic, which is why it is the last line rather than the only one.
//! 6. **Never rendered.** Downloads go out as `application/octet-stream`
//!    attachments with `nosniff` and a sandboxing CSP. Anything can write a
//!    file into the server directory — a mod, a plugin, a player's book via
//!    some exporter — and serving an `.html` file inline from the API's own
//!    origin would be stored XSS against whoever opened it.
//!
//! Reading contents is attributed in the console audit log by the routes;
//! listings are not, since a directory listing reveals nothing the
//! permission does not already imply.
//!
//! Everything here is blocking filesystem work; callers run it on the
//! blocking pool.

use std::fs::{self, File, FileType, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::read::GzDecoder;
use serde::Serialize;

use crate::error::AppError;

/// Entries returned for one directory. Far beyond any directory a person
/// pages through by hand; `playerdata/` on a large server is the realistic
/// worst case and stays well under it.
pub const MAX_LIST_ENTRIES: usize = 5_000;

/// Names read from one directory before giving up, so that sorting a
/// pathological directory cannot pin unbounded memory.
const MAX_SCAN_ENTRIES: usize = 100_000;

/// How much of a file the in-panel viewer shows. Download for the rest.
pub const MAX_PREVIEW_BYTES: usize = 512 * 1024;

/// Largest config file that is downloaded through the masking path, which
/// has to hold the whole file in memory. Config files are kilobytes; this is
/// only here so a mislabelled multi-gigabyte `.json` cannot exhaust memory.
pub const MAX_MASKED_DOWNLOAD: u64 = 8 * 1024 * 1024;

const MAX_PATH_LEN: usize = 4096;
const MAX_DEPTH: usize = 64;

/// How much of a file is inspected for NUL bytes to decide it is binary.
const BINARY_SNIFF: usize = 8 * 1024;

/// What a masked value is replaced with.
pub const MASK: &str = "********";

/// Files the browser refuses to serve, whatever the configuration says.
///
/// A pattern without a `/` matches a file or directory name anywhere; one
/// with a `/` matches a path from the server root. `*` and `?` do not cross
/// `/`. Matching ignores ASCII case.
pub const BUILTIN_DENY: &[&str] = &[
    // ServerSideUtils: every player's login credentials.
    "authedPlayers.json",
    // The itzg image writes rcon-cli's config into /data, RCON password
    // included, whatever server.properties says.
    ".rcon-cli.*",
    // Environment files and key material, should any ever land here.
    ".env",
    "*.env",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    ".ssh",
    "id_rsa*",
    "id_ecdsa*",
    "id_ed25519*",
    ".git-credentials",
    ".netrc",
];

/// Why a request for a file was refused.
#[derive(Debug)]
pub enum FileError {
    /// The server directory itself is missing or unreadable.
    Unavailable,
    BadPath(&'static str),
    NotFound,
    /// Denied by policy: a credential store, or outside the server directory.
    Restricted(&'static str),
    /// The operating system refused to open it.
    Unreadable,
    NotADirectory,
    IsADirectory,
    /// A socket, FIFO or device. Opening one for reading can block forever.
    NotRegular,
    TooLarge(String),
    /// Readable, but the panel never writes it.
    ReadOnly(&'static str),
    /// A write that breaks a rule -- a locked key, a changed secret. Says which.
    Rejected(String),
    /// The file changed on disk since the editor loaded it.
    Conflict,
    /// An upload would replace a file without being asked to.
    Exists,
    /// Writing is switched off, or misconfigured.
    WritesDisabled(&'static str),
    Io(io::Error),
}

impl From<FileError> for AppError {
    fn from(e: FileError) -> Self {
        match e {
            FileError::Unavailable => {
                AppError::NotFound("The server directory is not available".to_string())
            }
            FileError::BadPath(why) => AppError::BadRequest(format!("Invalid path: {why}")),
            FileError::NotFound => AppError::NotFound("No such file or folder".to_string()),
            FileError::Restricted(why) => AppError::Forbidden(why.to_string()),
            FileError::Unreadable => {
                AppError::Forbidden("The API is not allowed to read this file".to_string())
            }
            FileError::NotADirectory => AppError::BadRequest("Not a folder".to_string()),
            FileError::IsADirectory => AppError::BadRequest("That is a folder".to_string()),
            FileError::NotRegular => {
                AppError::BadRequest("Only regular files and folders can be opened".to_string())
            }
            FileError::TooLarge(why) => AppError::PayloadTooLarge(why),
            FileError::ReadOnly(why) => AppError::Forbidden(why.to_string()),
            FileError::Rejected(why) => AppError::BadRequest(why),
            FileError::Conflict => AppError::Conflict(
                "The file changed on the server since you opened it. Reload it and make your \
                 changes again."
                    .to_string(),
            ),
            FileError::Exists => {
                AppError::Conflict("A file with that name already exists".to_string())
            }
            FileError::WritesDisabled(why) => AppError::Forbidden(why.to_string()),
            FileError::Io(e) => {
                log::error!("files: {e}");
                AppError::Internal("Could not read the server's files".to_string())
            }
        }
    }
}

fn masked_too_large() -> FileError {
    FileError::TooLarge(format!(
        "Config files over {} MiB cannot be downloaded with their secrets masked",
        MAX_MASKED_DOWNLOAD / (1024 * 1024)
    ))
}

pub(super) fn io_error(e: io::Error) -> FileError {
    match e.kind() {
        io::ErrorKind::NotFound => FileError::NotFound,
        io::ErrorKind::PermissionDenied => FileError::Unreadable,
        _ => match e.raw_os_error() {
            // A file used as a folder, e.g. `server.properties/x`.
            #[cfg(unix)]
            Some(libc::ENOTDIR) => FileError::NotFound,
            // A symlink loop, or O_NOFOLLOW meeting a symlink that appeared
            // after the path was resolved -- somebody racing the check.
            #[cfg(unix)]
            Some(libc::ELOOP) => FileError::Restricted("That path cannot be opened safely"),
            _ => FileError::Io(e),
        },
    }
}

pub(super) const DENIED_CREDENTIALS: &str = "This file holds credentials and is never served";
const DENIED_OUTSIDE: &str = "That is outside the server directory";
/// A second name for a file could be a second name for a denied one, so
/// reading through it would bypass the deny list. Minecraft never makes them.
const DENIED_HARDLINK: &str = "Hard-linked files are not served";

/// One row of a directory listing.
#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    /// From the server root, `/`-separated. What to send back to open it.
    pub path: String,
    /// `dir`, `file` or `other`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// RFC 3339, UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Whether this entry is a symlink (to somewhere inside the root).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub link: bool,
    /// Set when the entry will not be served, saying why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restricted: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    /// The directory listed, from the server root. Empty for the root.
    pub path: String,
    pub entries: Vec<Entry>,
    /// Entries in the directory, which exceeds `entries.len()` when truncated.
    pub total: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct Preview {
    pub path: String,
    pub name: String,
    /// On disk, compressed if the file is.
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Holds NUL bytes, so no text is returned.
    pub binary: bool,
    /// Gzip-compressed on disk; `text` is the decompressed contents.
    pub compressed: bool,
    /// Only the first [`MAX_PREVIEW_BYTES`] are included.
    pub truncated: bool,
    /// How many values were masked.
    pub masked: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether the file can be edited in the panel -- by policy and format,
    /// not by who is asking; the save route checks the permission.
    pub editable: bool,
    /// Why not, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly_reason: Option<String>,
    /// Identifies these exact contents. Sent back with an edit so a save
    /// cannot silently overwrite a change made since. A keyed hash: a plain
    /// one of a file whose only unknown is a masked password would let anyone
    /// holding it guess that password offline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Keys in this file whose values the panel never changes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locked: Vec<String>,
}

/// A file ready to send.
pub enum Download {
    /// Sent as it is on disk, streamed from this handle.
    Raw { file: File, path: PathBuf, name: String },
    /// A config file, sent with its secrets masked.
    Masked { bytes: Vec<u8>, name: String, masked: usize },
}

impl Download {
    pub fn name(&self) -> &str {
        match self {
            Download::Raw { name, .. } | Download::Masked { name, .. } => name,
        }
    }
}

/// A path that has been checked and resolved.
pub(super) struct Resolved {
    pub(super) root: PathBuf,
    pub(super) abs: PathBuf,
    pub(super) rel: String,
}

/// The game server's directory, as the file browser sees it.
pub struct ServerFiles {
    root: PathBuf,
    /// Lower-cased, with surrounding `/` trimmed.
    deny: Vec<String>,
    /// Configured read-only patterns, in the same form. The built-in ones
    /// live in `files_write`, with the reason for each.
    pub(super) readonly: Vec<String>,
    /// Where overwritten files are backed up. `None` switches writing off.
    pub(super) backup_dir: Option<PathBuf>,
}

/// Lower-case and trim a pattern list, dropping blanks.
pub(super) fn compile_patterns<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    patterns
        .into_iter()
        .map(|p| p.trim().trim_matches('/').to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Whether `rel` -- or any folder it is inside -- matches one of `patterns`
/// (already compiled). A pattern without a `/` matches a name at any depth;
/// one with a `/` matches a path from the root.
pub(super) fn matches_patterns<'a>(
    patterns: impl IntoIterator<Item = &'a str> + Clone,
    rel: &str,
) -> bool {
    if rel.is_empty() {
        return false;
    }
    let lower = rel.to_ascii_lowercase();
    let ends = lower
        .match_indices('/')
        .map(|(i, _)| i)
        .chain(std::iter::once(lower.len()));
    for end in ends {
        let prefix = &lower[..end];
        let name = prefix.rsplit('/').next().unwrap_or(prefix);
        for pattern in patterns.clone() {
            let subject = if pattern.contains('/') { prefix } else { name };
            if glob(pattern.as_bytes(), subject.as_bytes()) {
                return true;
            }
        }
    }
    false
}

impl ServerFiles {
    /// A read-only browser: writes are refused until [`with_writes`] is called.
    ///
    /// [`with_writes`]: ServerFiles::with_writes
    pub fn new(root: impl Into<PathBuf>, extra_deny: &[String]) -> Self {
        let deny = compile_patterns(
            BUILTIN_DENY.iter().copied().chain(extra_deny.iter().map(String::as_str)),
        );
        ServerFiles { root: root.into(), deny, readonly: Vec::new(), backup_dir: None }
    }

    /// Allow writes, backing up to `backup_dir` (empty: writes stay off) and
    /// treating `readonly` as read-only on top of the built-in list.
    pub fn with_writes(mut self, readonly: &[String], backup_dir: &str) -> Self {
        self.readonly = compile_patterns(readonly.iter().map(String::as_str));
        let dir = backup_dir.trim();
        self.backup_dir = (!dir.is_empty()).then(|| PathBuf::from(dir));
        self
    }

    /// Whether `rel` — or any folder it is inside — matches a deny pattern.
    pub fn is_denied(&self, rel: &str) -> bool {
        matches_patterns(self.deny.iter().map(String::as_str), rel)
    }

    pub(super) fn canonical_root(&self) -> Result<PathBuf, FileError> {
        let root = fs::canonicalize(&self.root).map_err(|e| {
            log::warn!("files: server directory {} is unavailable: {e}", self.root.display());
            FileError::Unavailable
        })?;
        if !root.is_dir() {
            return Err(FileError::Unavailable);
        }
        Ok(root)
    }

    fn resolve(&self, raw: &str) -> Result<Resolved, FileError> {
        let requested = normalize(raw)?;
        if self.is_denied(&requested) {
            return Err(FileError::Restricted(DENIED_CREDENTIALS));
        }
        let root = self.canonical_root()?;
        let joined = if requested.is_empty() { root.clone() } else { root.join(&requested) };
        let abs = fs::canonicalize(&joined).map_err(io_error)?;
        let rel = relative(&root, &abs).ok_or(FileError::Restricted(DENIED_OUTSIDE))?;
        if self.is_denied(&rel) {
            return Err(FileError::Restricted(DENIED_CREDENTIALS));
        }
        Ok(Resolved { root, abs, rel })
    }

    /// Resolve, open and re-verify a path. The returned handle is known to be
    /// inside the root and not denied, whatever happened to the path since.
    pub(super) fn open(&self, raw: &str) -> Result<(Resolved, File, Metadata), FileError> {
        let resolved = self.resolve(raw)?;

        // Refuse FIFOs and devices before opening: opening a FIFO for reading
        // blocks until a writer appears. O_NONBLOCK below covers one being
        // swapped in after this check.
        let pre = fs::metadata(&resolved.abs).map_err(io_error)?;
        if !pre.is_file() && !pre.is_dir() {
            return Err(FileError::NotRegular);
        }

        let file = open_no_follow(&resolved.abs).map_err(io_error)?;
        self.verify_handle(&file, &resolved.root)?;

        let meta = file.metadata().map_err(io_error)?;
        if !meta.is_file() && !meta.is_dir() {
            return Err(FileError::NotRegular);
        }
        if meta.is_file() && link_count(&meta) > 1 {
            return Err(FileError::Restricted(DENIED_HARDLINK));
        }
        Ok((resolved, file, meta))
    }

    /// Where the kernel says an open handle actually points.
    #[cfg(target_os = "linux")]
    pub(super) fn verify_handle(&self, file: &File, root: &Path) -> Result<(), FileError> {
        use std::os::fd::AsRawFd;
        let real = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .map_err(FileError::Io)?;
        let rel = relative(root, &real).ok_or(FileError::Restricted(DENIED_OUTSIDE))?;
        if self.is_denied(&rel) {
            return Err(FileError::Restricted(DENIED_CREDENTIALS));
        }
        Ok(())
    }

    /// No `/proc` to ask. The resolve check and `O_NOFOLLOW` still apply.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn verify_handle(&self, _file: &File, _root: &Path) -> Result<(), FileError> {
        Ok(())
    }

    /// List one directory: folders first, then files, by name.
    pub fn list(&self, raw: &str) -> Result<Listing, FileError> {
        let (resolved, dir, meta) = self.open(raw)?;
        if !meta.is_dir() {
            return Err(FileError::NotADirectory);
        }

        // On Linux, read the directory through the verified handle rather than
        // by path, so the listing is of the directory that was checked.
        let source = handle_path(&dir).unwrap_or_else(|| resolved.abs.clone());

        let mut names: Vec<(std::ffi::OsString, Option<FileType>)> = Vec::new();
        for entry in fs::read_dir(&source).map_err(io_error)? {
            let Ok(entry) = entry else { continue };
            names.push((entry.file_name(), entry.file_type().ok()));
            if names.len() >= MAX_SCAN_ENTRIES {
                break;
            }
        }

        let total = names.len();
        names.sort_by_cached_key(|(name, ft)| {
            let is_dir = ft.map_or(false, |t| t.is_dir());
            (!is_dir, name.to_string_lossy().to_lowercase())
        });
        let truncated = names.len() > MAX_LIST_ENTRIES;
        names.truncate(MAX_LIST_ENTRIES);

        let entries = names
            .into_iter()
            .map(|(name, ft)| self.describe(&resolved, &name, ft))
            .collect();

        Ok(Listing { path: resolved.rel, entries, total, truncated })
    }

    fn describe(&self, parent: &Resolved, name: &std::ffi::OsStr, ft: Option<FileType>) -> Entry {
        let display = name.to_string_lossy().into_owned();
        let rel = join(&parent.rel, &display);
        let is_link = ft.map_or(false, |t| t.is_symlink());
        let mut entry = Entry {
            name: display,
            path: rel,
            kind: match ft {
                Some(t) if t.is_dir() => "dir",
                Some(t) if t.is_file() || t.is_symlink() => "file",
                _ => "other",
            },
            size: None,
            modified: None,
            link: false,
            restricted: None,
        };

        if self.is_denied(&entry.path) {
            entry.restricted = Some(DENIED_CREDENTIALS);
            return entry;
        }

        let abs = parent.abs.join(name);
        let meta = if is_link {
            match fs::canonicalize(&abs) {
                Ok(target) => match relative(&parent.root, &target) {
                    Some(target_rel) if self.is_denied(&target_rel) => {
                        entry.restricted = Some(DENIED_CREDENTIALS);
                        None
                    }
                    Some(_) => {
                        entry.link = true;
                        fs::metadata(&target).ok()
                    }
                    None => {
                        entry.restricted = Some(DENIED_OUTSIDE);
                        None
                    }
                },
                Err(_) => {
                    entry.restricted = Some("Broken link");
                    None
                }
            }
        } else {
            fs::symlink_metadata(&abs).ok()
        };

        if let Some(m) = meta {
            entry.kind = if m.is_dir() {
                "dir"
            } else if m.is_file() {
                "file"
            } else {
                "other"
            };
            if m.is_file() && link_count(&m) > 1 {
                entry.restricted = Some(DENIED_HARDLINK);
                return entry;
            }
            if m.is_file() {
                entry.size = Some(m.len());
            }
            entry.modified = m.modified().ok().map(rfc3339);
        }
        if entry.kind == "other" && entry.restricted.is_none() {
            entry.restricted = Some("Not a regular file");
        }
        entry
    }

    /// The start of a file, as text, for the in-panel viewer.
    pub fn preview(&self, raw: &str) -> Result<Preview, FileError> {
        let (resolved, file, meta) = self.open(raw)?;
        if meta.is_dir() {
            return Err(FileError::IsADirectory);
        }

        let name = base_name(&resolved.rel);
        let (inner, compressed) = strip_gz(&name);

        let mut buf = Vec::new();
        let read = if compressed {
            GzDecoder::new(file)
                .take(MAX_PREVIEW_BYTES as u64 + 1)
                .read_to_end(&mut buf)
        } else {
            file.take(MAX_PREVIEW_BYTES as u64 + 1).read_to_end(&mut buf)
        };

        let mut preview = Preview {
            path: resolved.rel.clone(),
            name: name.clone(),
            size: meta.len(),
            modified: meta.modified().ok().map(rfc3339),
            binary: false,
            compressed,
            truncated: false,
            masked: 0,
            text: None,
            editable: false,
            readonly_reason: None,
            version: None,
            locked: Vec::new(),
        };

        match read {
            Ok(_) => {}
            // Not actually gzip, or corrupt. Show it as the binary it is.
            Err(_) if compressed => {
                preview.binary = true;
                preview.readonly_reason = Some("Compressed files can't be edited".to_string());
                return Ok(preview);
            }
            Err(e) => return Err(io_error(e)),
        }

        preview.truncated = buf.len() > MAX_PREVIEW_BYTES;
        buf.truncate(MAX_PREVIEW_BYTES);

        if looks_binary(&buf) {
            preview.binary = true;
        } else {
            let text = String::from_utf8_lossy(&buf).into_owned();
            let (text, masked) =
                if is_maskable(inner) { redact_secrets(&text) } else { (text, 0) };
            preview.text = Some(text);
            preview.masked = masked;
        }

        // Editable only when the whole file was read, losslessly: what the
        // editor sends back replaces the file entirely.
        let blocked = self
            .edit_block(&resolved.rel)
            .map(str::to_string)
            .or_else(|| compressed.then(|| "Compressed files can't be edited".to_string()))
            .or_else(|| preview.binary.then(|| "Binary files can't be edited".to_string()))
            .or_else(|| {
                preview.truncated.then(|| {
                    format!("Files over {} KiB can't be edited here", MAX_PREVIEW_BYTES / 1024)
                })
            })
            .or_else(|| {
                std::str::from_utf8(&buf)
                    .is_err()
                    .then(|| "Only UTF-8 text can be edited".to_string())
            });
        match blocked {
            Some(why) => preview.readonly_reason = Some(why),
            None => {
                preview.editable = true;
                preview.version = Some(super::files_write::file_version(&buf));
            }
        }
        if let Some(text) = &preview.text {
            preview.locked = super::files_write::locked_keys_in(&resolved.rel, text);
        }
        Ok(preview)
    }

    /// Open a file for download. Config files come back with their secrets
    /// masked; everything else is streamed as-is.
    pub fn download(&self, raw: &str) -> Result<Download, FileError> {
        let (resolved, file, meta) = self.open(raw)?;
        if meta.is_dir() {
            return Err(FileError::IsADirectory);
        }

        let name = base_name(&resolved.rel);
        let (inner, compressed) = strip_gz(&name);

        if !is_maskable(inner) {
            return Ok(Download::Raw { file, path: resolved.abs, name });
        }

        // A config file, possibly gzipped. Read it whole — bounded either way —
        // mask it, and send the masked text. A gzipped one goes out
        // decompressed, since masking means rewriting it regardless.
        if meta.len() > MAX_MASKED_DOWNLOAD {
            return Err(masked_too_large());
        }
        let mut bytes = Vec::new();
        let read = if compressed {
            GzDecoder::new(file).take(MAX_MASKED_DOWNLOAD + 1).read_to_end(&mut bytes)
        } else {
            file.take(MAX_MASKED_DOWNLOAD + 1).read_to_end(&mut bytes)
        };
        read.map_err(io_error)?;
        if bytes.len() as u64 > MAX_MASKED_DOWNLOAD {
            return Err(masked_too_large());
        }

        let text = String::from_utf8_lossy(&bytes);
        let (masked_text, masked) = redact_secrets(&text);
        Ok(Download::Masked {
            bytes: masked_text.into_bytes(),
            name: inner.to_string(),
            masked,
        })
    }
}

/// Split a requested path into plain names, refusing anything that could
/// step outside the root before the filesystem is consulted. A leading `/`
/// means the server root, not the filesystem's.
pub fn normalize(raw: &str) -> Result<String, FileError> {
    if raw.len() > MAX_PATH_LEN {
        return Err(FileError::BadPath("too long"));
    }
    if raw.contains('\0') {
        return Err(FileError::BadPath("contains a NUL byte"));
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(FileError::BadPath("may not contain '..'")),
            name => parts.push(name),
        }
    }
    if parts.len() > MAX_DEPTH {
        return Err(FileError::BadPath("too deep"));
    }
    Ok(parts.join("/"))
}

/// `abs` relative to `root`, `/`-separated, or `None` if it is not inside.
pub(super) fn relative(root: &Path, abs: &Path) -> Option<String> {
    let rest = abs.strip_prefix(root).ok()?;
    let parts: Vec<String> = rest
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

pub fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

pub(super) fn base_name(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// `("config.yml", true)` for `config.yml.gz`.
fn strip_gz(name: &str) -> (&str, bool) {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gz") && name.len() > 3 {
        (&name[..name.len() - 3], true)
    } else {
        (name, false)
    }
}

/// A path through which an open handle can be re-read on Linux.
pub(super) fn handle_path(file: &File) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        Some(PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        None
    }
}

#[cfg(unix)]
pub(super) fn open_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK has no effect on regular files or directories; it only keeps
    // a FIFO from blocking the open.
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
pub(super) fn open_no_follow(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// How many names a file has.
fn link_count(meta: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

fn rfc3339(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(super) fn looks_binary(buf: &[u8]) -> bool {
    buf[..buf.len().min(BINARY_SNIFF)].contains(&0)
}

/// Whether a file name is a config format [`redact_secrets`] understands.
pub fn is_maskable(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.ends_with(".env") {
        return true;
    }
    matches!(
        lower.rsplit_once('.').map(|(_, ext)| ext),
        Some(
            "properties" | "toml" | "yml" | "yaml" | "json" | "json5" | "jsonc" | "cfg" | "conf"
                | "config" | "ini" | "hocon" | "snbt" | "mcmeta"
        )
    )
}

/// Whether a config key names something secret.
///
/// Matches on the key with separators and case removed, so `rcon.password`,
/// `botToken`, `discord_webhook_url` and `API-KEY` all count. Over-matching
/// costs one masked value; under-matching leaks a credential, so it errs wide.
pub fn is_secret_key(key: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "webhook",
        "privatekey",
        "credential",
        "authkey",
        "accesskey",
    ];
    let squashed: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    NEEDLES.iter().any(|n| squashed.contains(n))
        || squashed.ends_with("pass")
        || squashed.ends_with("pwd")
}

/// Mask the value on every `key = value` / `key: value` line whose key looks
/// secret. Returns the text and how many values were masked.
///
/// Line-based so it covers `.properties`, TOML, YAML, INI, HOCON and
/// pretty-printed JSON with one rule. Comments are left alone; quoting and a
/// trailing comma are kept so the file stays well-formed.
pub fn redact_secrets(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut masked = 0;
    for line in text.split_inclusive('\n') {
        match redact_line(line) {
            Some(replaced) => {
                out.push_str(&replaced);
                masked += 1;
            }
            None => out.push_str(line),
        }
    }
    (out, masked)
}

/// Where the secret value on a line sits: the key it is under, and the byte
/// range of the value itself (quotes included, trailing comma not).
///
/// The single definition of "this line holds a secret", shared by masking on
/// the way out and by restoring masked values on the way back in, so the two
/// can never disagree about which lines count.
pub(super) struct SecretLine<'a> {
    pub(super) key: &'a str,
    pub(super) value: std::ops::Range<usize>,
}

pub(super) fn secret_line(line: &str) -> Option<SecretLine<'_>> {
    let body = line.trim_end_matches(['\r', '\n']);

    let trimmed = body.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with(';')
        || trimmed.starts_with('!')
        || trimmed.starts_with("//")
    {
        return None;
    }

    // A YAML list item, `- key: value`.
    let mut key_start = body.len() - trimmed.len();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        key_start += 2 + (rest.len() - rest.trim_start().len());
    }
    let after = &body[key_start..];

    // `sep` is the byte offset of the `:` or `=` in `body`.
    let (key, sep) = match after.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let close = after[1..].find(q)? + 1;
            let rest = &after[close + 1..];
            let gap = rest.len() - rest.trim_start().len();
            if !matches!(rest[gap..].chars().next(), Some(':' | '=')) {
                return None;
            }
            (&after[1..close], key_start + close + 1 + gap)
        }
        _ => {
            let at = after.find([':', '='])?;
            (after[..at].trim(), key_start + at)
        }
    };

    if !is_secret_key(key) {
        return None;
    }

    let region = &body[sep + 1..];
    let start = sep + 1 + (region.len() - region.trim_start().len());
    let value = region.trim();
    let core = match value.strip_suffix(',') {
        Some(v) => v.trim_end(),
        None => value,
    };

    // Nothing worth hiding, or the value is not on this line.
    if matches!(
        core,
        "" | "\"\"" | "''" | "null" | "~" | "true" | "false" | "{" | "[" | "{}" | "[]"
    ) {
        return None;
    }

    Some(SecretLine { key, value: start..start + core.len() })
}

/// What a masked value looks like, keeping the original's quoting.
fn mask_for(value: &str) -> String {
    match value.chars().next() {
        Some('"') => format!("\"{MASK}\""),
        Some('\'') => format!("'{MASK}'"),
        _ => MASK.to_string(),
    }
}

/// Whether a value is exactly a mask, in any of the quotings [`mask_for`] makes.
pub(super) fn is_mask(value: &str) -> bool {
    value == MASK
        || value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) == Some(MASK)
        || value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) == Some(MASK)
}

fn redact_line(line: &str) -> Option<String> {
    let found = secret_line(line)?;
    let body_end = line.trim_end_matches(['\r', '\n']).len();
    let value = &line[found.value.clone()];

    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..found.value.start]);
    out.push_str(&mask_for(value));
    // Whatever followed the value: a trailing comma is kept, an inline
    // comment is dropped -- it can quote the value.
    let rest = &line[found.value.end..body_end];
    if rest.trim_start().starts_with(',') {
        out.push(',');
    }
    out.push_str(&line[body_end..]);
    Some(out)
}

/// Glob match: `*` is any run and `?` any one byte, neither crossing `/`.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == text[t] || (pattern[p] == b'?' && text[t] != b'/')) {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            if text[st] == b'/' {
                return false;
            }
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A scratch server directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("apird-files-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn write(&self, rel: &str, contents: &[u8]) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fn files(&self) -> ServerFiles {
            ServerFiles::new(self.0.join("server"), &["secret-dir".to_string()])
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn denied<T>(e: Result<T, FileError>) -> bool {
        matches!(e, Err(FileError::Restricted(_)))
    }

    #[test]
    fn normalize_keeps_plain_names_only() {
        assert_eq!(normalize("").unwrap(), "");
        assert_eq!(normalize("/").unwrap(), "");
        assert_eq!(normalize("/plugins//Foo/./config.yml").unwrap(), "plugins/Foo/config.yml");
        assert!(matches!(normalize("../etc/passwd"), Err(FileError::BadPath(_))));
        assert!(matches!(normalize("world/../../etc"), Err(FileError::BadPath(_))));
        assert!(matches!(normalize("a\0b"), Err(FileError::BadPath(_))));
        assert!(matches!(normalize(&"a/".repeat(100)), Err(FileError::BadPath(_))));
        assert!(matches!(normalize(&"a".repeat(5000)), Err(FileError::BadPath(_))));
        // Dots inside a name are just a name.
        assert_eq!(normalize("..hidden/a..b").unwrap(), "..hidden/a..b");
    }

    #[test]
    fn glob_does_not_cross_slashes() {
        assert!(glob(b"*.pem", b"server.pem"));
        assert!(glob(b".rcon-cli.*", b".rcon-cli.env"));
        assert!(glob(b"id_rsa*", b"id_rsa"));
        assert!(glob(b"a?c", b"abc"));
        assert!(!glob(b"*.pem", b"pem"));
        assert!(!glob(b"plugins/*", b"plugins/a/b"));
        assert!(glob(b"plugins/*/b", b"plugins/a/b"));
        assert!(!glob(b"a?c", b"a/c"));
    }

    #[test]
    fn deny_matches_names_anywhere_paths_from_root_and_ancestors() {
        let files = ServerFiles::new("/nowhere", &["plugins/LuckPerms".to_string(), " /x/ ".to_string()]);
        assert!(files.is_denied("authedPlayers.json"));
        assert!(files.is_denied("AUTHEDPLAYERS.JSON"), "case-insensitive");
        assert!(files.is_denied("backups/authedPlayers.json"), "a bare name matches at any depth");
        assert!(files.is_denied(".rcon-cli.yaml"));
        assert!(files.is_denied("config/certs/server.pem"));
        assert!(files.is_denied("plugins/LuckPerms"));
        assert!(files.is_denied("plugins/luckperms/storage.db"), "a denied folder hides its contents");
        assert!(files.is_denied("x/y"), "configured patterns are trimmed");
        assert!(!files.is_denied("other/LuckPerms"), "a path pattern is anchored at the root");
        assert!(!files.is_denied(""));
        assert!(!files.is_denied("server.properties"));
        assert!(!files.is_denied("world/level.dat"));
    }

    #[test]
    fn resolves_inside_the_root_and_refuses_everything_else() {
        let s = Scratch::new();
        s.write("server/server.properties", b"motd=hi\n");
        s.write("server/authedPlayers.json", b"{}");
        s.write("outside.txt", b"nope");

        let files = s.files();
        assert!(files.preview("server.properties").is_ok());
        assert!(files.preview("/server.properties").is_ok());
        assert!(matches!(files.preview("missing.txt"), Err(FileError::NotFound)));
        assert!(matches!(files.preview("../outside.txt"), Err(FileError::BadPath(_))));
        assert!(denied(files.preview("authedPlayers.json")));
        assert!(denied(files.download("authedPlayers.json")));
        assert!(matches!(files.preview(""), Err(FileError::IsADirectory)));
        assert!(matches!(files.list("server.properties"), Err(FileError::NotADirectory)));
        assert!(matches!(files.list("server.properties/x"), Err(FileError::NotFound)));
    }

    #[test]
    fn a_missing_root_is_unavailable_not_an_error_page() {
        let files = ServerFiles::new("/definitely/not/here", &[]);
        assert!(matches!(files.list(""), Err(FileError::Unavailable)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_or_launder_a_denied_file() {
        use std::os::unix::fs::symlink;
        let s = Scratch::new();
        s.write("server/world/level.dat", b"\0\x01");
        s.write("server/authedPlayers.json", b"{\"creds\":1}");
        s.write("outside/secret.txt", b"nope");
        let root = s.0.join("server");
        symlink(s.0.join("outside"), root.join("escape")).unwrap();
        symlink(s.0.join("outside/secret.txt"), root.join("escape.txt")).unwrap();
        symlink(root.join("authedPlayers.json"), root.join("innocent.json")).unwrap();
        symlink(root.join("world"), root.join("world-link")).unwrap();

        let files = s.files();
        assert!(denied(files.list("escape")));
        assert!(denied(files.preview("escape/secret.txt")));
        assert!(denied(files.preview("escape.txt")));
        assert!(denied(files.download("innocent.json")), "a link to a denied file is denied");
        assert!(files.preview("world-link/level.dat").is_ok(), "links inside the root work");

        let listing = files.list("").unwrap();
        let by_name = |n: &str| listing.entries.iter().find(|e| e.name == n).unwrap();
        assert_eq!(by_name("escape").restricted, Some(DENIED_OUTSIDE));
        assert_eq!(by_name("escape.txt").restricted, Some(DENIED_OUTSIDE));
        assert_eq!(by_name("innocent.json").restricted, Some(DENIED_CREDENTIALS));
        assert_eq!(by_name("authedPlayers.json").restricted, Some(DENIED_CREDENTIALS));
        assert!(by_name("authedPlayers.json").size.is_none(), "nothing about it is disclosed");
        let link = by_name("world-link");
        assert!(link.link && link.restricted.is_none());
        assert_eq!(link.kind, "dir");
    }

    #[cfg(unix)]
    #[test]
    fn fifos_are_refused_rather_than_hanging_the_worker() {
        let s = Scratch::new();
        s.write("server/keep", b"");
        let fifo = s.0.join("server/pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let files = s.files();
        assert!(matches!(files.preview("pipe"), Err(FileError::NotRegular)));
        let listing = files.list("").unwrap();
        let pipe = listing.entries.iter().find(|e| e.name == "pipe").unwrap();
        assert_eq!(pipe.kind, "other");
        assert!(pipe.restricted.is_some());
    }

    #[test]
    fn configured_deny_patterns_apply() {
        let s = Scratch::new();
        s.write("server/secret-dir/a.txt", b"x");
        let files = s.files();
        assert!(denied(files.list("secret-dir")));
        assert!(denied(files.preview("secret-dir/a.txt")));
    }

    #[test]
    fn listing_puts_folders_first_then_names_case_insensitively() {
        let s = Scratch::new();
        s.write("server/b.txt", b"12345");
        s.write("server/A.txt", b"1");
        s.write("server/world/level.dat", b"x");
        s.write("server/Logs/latest.log", b"x");
        let listing = s.files().list("").unwrap();
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Logs", "world", "A.txt", "b.txt"]);
        assert_eq!(listing.total, 4);
        assert!(!listing.truncated);
        let b = &listing.entries[3];
        assert_eq!((b.kind, b.size, b.path.as_str()), ("file", Some(5), "b.txt"));
        assert!(b.modified.is_some());
        assert_eq!(s.files().list("world").unwrap().entries[0].path, "world/level.dat");
    }

    #[test]
    fn preview_masks_rcon_password_and_flags_binary_and_truncation() {
        let s = Scratch::new();
        s.write(
            "server/server.properties",
            b"#Minecraft server properties\nenable-rcon=true\nrcon.password=hunter2\nrcon.port=29875\nmotd=Hello\n",
        );
        s.write("server/world/level.dat", b"\x1f\x8b\0\0binary");
        s.write("server/logs/big.log", &vec![b'a'; MAX_PREVIEW_BYTES + 10]);
        let files = s.files();

        let p = files.preview("server.properties").unwrap();
        let text = p.text.unwrap();
        assert_eq!(p.masked, 1);
        assert!(!text.contains("hunter2"));
        assert!(text.contains("rcon.password=********\n"));
        assert!(text.contains("rcon.port=29875"), "{text}");
        assert!(text.contains("motd=Hello"));

        let b = files.preview("world/level.dat").unwrap();
        assert!(b.binary && b.text.is_none());

        let big = files.preview("logs/big.log").unwrap();
        assert!(big.truncated);
        assert_eq!(big.text.unwrap().len(), MAX_PREVIEW_BYTES);
        assert_eq!(big.size, (MAX_PREVIEW_BYTES + 10) as u64);
    }

    #[test]
    fn gzipped_logs_are_previewed_decompressed() {
        let s = Scratch::new();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"[12:00:00] [Server thread/INFO]: Done\n").unwrap();
        s.write("server/logs/2026-01-01-1.log.gz", &gz.finish().unwrap());
        s.write("server/logs/fake.log.gz", b"not gzip at all");
        let files = s.files();

        let p = files.preview("logs/2026-01-01-1.log.gz").unwrap();
        assert!(p.compressed);
        assert!(p.text.unwrap().contains("Done"));
        assert!(files.preview("logs/fake.log.gz").unwrap().binary);
    }

    #[test]
    fn downloads_mask_configs_and_stream_everything_else() {
        let s = Scratch::new();
        s.write("server/server.properties", b"rcon.password=hunter2\n");
        s.write("server/mods/thing.jar", b"PK\x03\x04");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"token: abc\n").unwrap();
        s.write("server/old/bot.yml.gz", &gz.finish().unwrap());
        let files = s.files();

        match files.download("server.properties").unwrap() {
            Download::Masked { bytes, name, masked } => {
                assert_eq!(String::from_utf8(bytes).unwrap(), "rcon.password=********\n");
                assert_eq!((name.as_str(), masked), ("server.properties", 1));
            }
            Download::Raw { .. } => panic!("a config must not be streamed raw"),
        }
        match files.download("old/bot.yml.gz").unwrap() {
            Download::Masked { bytes, name, .. } => {
                assert_eq!(String::from_utf8(bytes).unwrap(), "token: ********\n");
                assert_eq!(name, "bot.yml");
            }
            Download::Raw { .. } => panic!("a gzipped config must not be streamed raw"),
        }
        assert!(matches!(files.download("mods/thing.jar").unwrap(), Download::Raw { .. }));
        assert!(matches!(files.download("mods"), Err(FileError::IsADirectory)));
    }

    #[test]
    fn secret_keys_are_recognised_in_any_spelling() {
        for key in [
            "rcon.password", "botToken", "discord_webhook_url", "API-KEY", "client-secret",
            "db_pass", "adminPwd", "private_key", "management-server-secret", "\"token\"",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in ["motd", "rcon.port", "level-name", "max-players", "spawn-protection", "passive"] {
            assert!(!is_secret_key(key), "{key}");
        }
    }

    #[test]
    fn redaction_handles_each_config_syntax() {
        let cases = [
            ("rcon.password=hunter2\n", "rcon.password=********\n"),
            ("rcon.password: hunter2\r\n", "rcon.password: ********\r\n"),
            ("  token: 'abc'\n", "  token: '********'\n"),
            ("bot_token = \"abc\" # note\n", "bot_token = \"********\"\n"),
            ("    \"apiKey\": \"abc\",\n", "    \"apiKey\": \"********\",\n"),
            ("    \"apiKey\" : 12345\n", "    \"apiKey\" : ********\n"),
            ("  - password: abc\n", "  - password: ********\n"),
            ("webhook=https://discord.com/api/webhooks/1/x\n", "webhook=********\n"),
            // The key ends at the first separator, not at one inside the value.
            ("secret=a:b=c\n", "secret=********\n"),
            // Minified JSON: everything after the key goes, which over-masks but
            // never leaks.
            ("{\"token\":\"abc\",\"x\":1}", "{\"token\":\"********\""),
        ];
        for (input, expected) in cases {
            let (out, n) = redact_secrets(input);
            assert_eq!(out, expected, "for {input:?}");
            assert_eq!(n, 1, "for {input:?}");
        }
    }

    #[test]
    fn redaction_leaves_comments_empty_values_and_other_keys_alone() {
        let input = "# password=commented\n! token=also\n; secret=ini\n// apiKey: js\n\
                     rcon.password=\npassword: \"\"\ntoken: null\nuse-password: true\n\
                     secrets:\n  {\n\"minecraft:stone\": 1\nmotd=pass: word\n";
        let (out, n) = redact_secrets(input);
        assert_eq!(out, input);
        assert_eq!(n, 0);
    }

    #[test]
    fn maskable_formats() {
        for name in ["server.properties", "a.TOML", "x.yml", "x.yaml", "ops.json", "a.cfg", ".env", "prod.env", "x.conf", "x.ini"] {
            assert!(is_maskable(name), "{name}");
        }
        for name in ["latest.log", "level.dat", "mod.jar", "readme.txt", "r.0.0.mca"] {
            assert!(!is_maskable(name), "{name}");
        }
    }
}
