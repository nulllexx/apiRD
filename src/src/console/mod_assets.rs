//! Item textures read out of the mod jars the server already runs.
//!
//! For a modded server this is a better source than any mirror, and not only
//! because no mirror carries `create:cogwheel`. A jar's layout is fixed by the
//! resource system — `assets/<namespace>/textures/<kind>/<name>.png` — so where
//! a texture lives is a rule rather than the pile of naming conventions the
//! vanilla lookup has to reverse-engineer. The files are already on disk, so
//! there is no network, nothing to redistribute, and no version to keep in step
//! with the server.
//!
//! ## Why an index, and why it is only namespaces
//!
//! A modpack is a few hundred jars. Opening all of them for every unknown item
//! would be unaffordable, so one pass at startup records which namespaces each
//! jar declares. That is a small map — a few hundred entries — and it narrows
//! any later lookup to the one or two jars that could possibly answer it.
//!
//! Indexing every *texture* instead would allow exact lookups with no zip
//! reopening, but a large pack has tens of thousands of them and the map would
//! be megabytes of strings held forever to save a few milliseconds on a path
//! that is already cached on disk after its first hit.
//!
//! ## Exact first, then a scan
//!
//! The same candidate paths the vanilla lookup uses are tried by name, which is
//! O(1) against the zip's central directory. When none of them match, the jar's
//! entry list is scanned for a file named after the item. That fallback is only
//! possible here: a remote mirror cannot be listed, only guessed at. It is what
//! finds `block/cogwheel_side.png` for an item whose texture nobody would have
//! predicted the name of.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::OnceCell;

/// Ceiling on one extracted texture. Same reasoning as the mirror's cap: a
/// texture is a few hundred bytes, and a jar entry claiming to be far larger is
/// not one.
const MAX_TEXTURE_BYTES: u64 = 256 * 1024;

/// Jars scanned in one indexing pass. A modpack is hundreds, not thousands;
/// this is a backstop against pointing the setting at something enormous.
const MAX_JARS: usize = 2_000;

/// Mod jars on disk, indexed by the namespaces they provide.
pub struct ModAssets {
    dir: PathBuf,
    /// Built once, on the first lookup rather than at boot: a server with no
    /// mods should not pay to discover that, and a server with three hundred
    /// should not delay startup for it.
    index: OnceCell<HashMap<String, Vec<PathBuf>>>,
}

impl ModAssets {
    pub fn new(dir: String) -> Arc<Self> {
        Arc::new(Self {
            dir: PathBuf::from(dir),
            index: OnceCell::new(),
        })
    }

    async fn index(&self) -> &HashMap<String, Vec<PathBuf>> {
        self.index
            .get_or_init(|| async {
                let dir = self.dir.clone();
                // Opening a few hundred zips is blocking work measured in
                // seconds; doing it on a worker thread would stall every other
                // request on that thread.
                tokio::task::spawn_blocking(move || build_index(&dir))
                    .await
                    .unwrap_or_else(|e| {
                        log::error!("mod assets: indexing panicked: {e}");
                        HashMap::new()
                    })
            })
            .await
    }

    /// Look one texture up across the jars that declare `namespace`.
    pub async fn get(
        &self,
        namespace: &str,
        name: &str,
        candidates: Vec<String>,
    ) -> Option<Vec<u8>> {
        let jars = self.index().await.get(namespace)?.clone();
        if jars.is_empty() {
            return None;
        }

        let namespace = namespace.to_string();
        let name = name.to_string();

        tokio::task::spawn_blocking(move || {
            for jar in &jars {
                if let Some(bytes) = read_from_jar(jar, &namespace, &name, &candidates) {
                    return Some(bytes);
                }
            }
            None
        })
        .await
        .unwrap_or_else(|e| {
            log::error!("mod assets: lookup panicked: {e}");
            None
        })
    }

    /// How many namespaces were found, for the startup log line.
    pub async fn namespace_count(&self) -> usize {
        self.index().await.len()
    }
}

/// The namespace an `assets/<ns>/textures/...` entry belongs to.
///
/// Only texture-bearing entries count, so a jar shipping nothing but code or
/// recipes for a namespace is not recorded as a place to look for its art.
fn namespace_of(entry: &str) -> Option<&str> {
    let rest = entry.strip_prefix("assets/")?;
    let (namespace, tail) = rest.split_once('/')?;

    if !tail.starts_with("textures/") || !entry.ends_with(".png") {
        return None;
    }
    // A namespace is a path segment that becomes part of a lookup key, so it
    // gets the same allowlist the HTTP side applies. `.` has to be permitted for
    // namespaces like `some.mod`, which means `..` has to be refused explicitly
    // rather than falling out of the character rule.
    let ok = !namespace.is_empty()
        && !namespace.contains("..")
        && namespace
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'));

    ok.then_some(namespace)
}

fn build_index(dir: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        // Not an error: a vanilla or plugin-only server simply has no mods
        // directory, and the vanilla mirror answers everything.
        log::info!("mod assets: no jars at {} — skipping", dir.display());
        return index;
    };

    let mut jars = 0usize;
    for entry in entries.flatten() {
        if jars >= MAX_JARS {
            log::warn!("mod assets: stopped indexing at {MAX_JARS} jars");
            break;
        }

        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jar") {
            continue;
        }
        jars += 1;

        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let Ok(archive) = zip::ZipArchive::new(file) else {
            // A disabled mod is often left in place as a truncated or renamed
            // file; that is not worth failing the whole index over.
            log::debug!("mod assets: {} is not readable as a zip", path.display());
            continue;
        };

        // `file_names` reads the central directory only — no decompression, and
        // no per-entry setup.
        let namespaces: HashSet<&str> = archive.file_names().filter_map(namespace_of).collect();

        for namespace in namespaces {
            index
                .entry(namespace.to_string())
                .or_default()
                .push(path.clone());
        }
    }

    log::info!(
        "mod assets: indexed {} namespace(s) across {jars} jar(s) in {}",
        index.len(),
        dir.display()
    );
    index
}

/// Pick the entry that best answers `name` from a jar's file list.
///
/// Ranking, in order: the file named exactly after the item, then the shortest
/// name that starts with it. The second rule is what finds `cogwheel_side` for
/// `cogwheel`, and preferring the shortest keeps it from landing on
/// `cogwheel_side_connected_powered` — the least qualified variant is the one
/// closest to the plain face.
///
/// Anything that merely *contains* the name is rejected. A wrong picture is
/// worse than no picture, because the fallback tile reads as "unknown" while a
/// texture reads as fact.
fn best_match<'a>(
    names: impl Iterator<Item = &'a str>,
    prefix: &str,
    name: &str,
) -> Option<String> {
    let qualified = format!("{name}_");
    let mut best: Option<(u8, usize, &str)> = None;

    for entry in names {
        let Some(rest) = entry.strip_prefix(prefix) else {
            continue;
        };
        let Some(stem) = rest.strip_suffix(".png") else {
            continue;
        };
        let file_stem = stem.rsplit('/').next().unwrap_or(stem);

        let rank = if file_stem == name {
            0
        } else if file_stem.starts_with(&qualified) {
            1
        } else {
            continue;
        };

        let better = best
            .as_ref()
            .is_none_or(|(r, len, _)| (rank, file_stem.len()) < (*r, *len));
        if better {
            best = Some((rank, file_stem.len(), entry));
        }
    }

    best.map(|(_, _, entry)| entry.to_string())
}

/// Pull one texture out of a single jar.
fn read_from_jar(
    jar: &Path,
    namespace: &str,
    name: &str,
    candidates: &[String],
) -> Option<Vec<u8>> {
    let file = std::fs::File::open(jar).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    let prefix = format!("assets/{namespace}/textures/");

    // The predictable paths first, straight off the central directory.
    let exact = candidates
        .iter()
        .map(|candidate| format!("{prefix}{candidate}"))
        .find(|path| archive.index_for_name(path).is_some());

    // Then the fallback a remote mirror could never offer: ask the jar what it
    // actually contains and take anything named after this item.
    let found = exact.or_else(|| best_match(archive.file_names(), &prefix, name))?;

    let mut entry = archive.by_name(&found).ok()?;
    if entry.size() > MAX_TEXTURE_BYTES {
        log::warn!("mod assets: {found} in {} is implausibly large", jar.display());
        return None;
    }

    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes).ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', b'-', b'b', b'o', b'd', b'y'];

    /// Write a jar containing exactly the given entry paths.
    fn write_jar(dir: &Path, jar_name: &str, entries: &[&str]) -> PathBuf {
        let path = dir.join(jar_name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);

        for entry in entries {
            zip.start_file::<_, ()>(*entry, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(PNG).unwrap();
        }

        zip.finish().unwrap();
        path
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apird-mods-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /* ------------------------------------------------------------ indexing */

    #[test]
    fn a_namespace_is_recorded_only_when_it_ships_textures() {
        let dir = temp_dir();
        write_jar(
            &dir,
            "create.jar",
            &[
                "assets/create/textures/block/cogwheel.png",
                // Data for another namespace, but no art - nothing to find
                // there, so it must not become a place we look.
                "assets/quark/recipes/thing.json",
                "data/create/recipes/cogwheel.json",
                "META-INF/MANIFEST.MF",
            ],
        );

        let index = build_index(&dir);

        assert!(index.contains_key("create"));
        assert!(!index.contains_key("quark"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_jar_can_provide_several_namespaces() {
        // Common for a library mod or a bundled addon.
        let dir = temp_dir();
        write_jar(
            &dir,
            "bundle.jar",
            &[
                "assets/create/textures/item/wrench.png",
                "assets/createaddition/textures/item/wire.png",
            ],
        );

        let index = build_index(&dir);
        assert_eq!(index.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_indexes_to_nothing() {
        // A plugin-only or vanilla server, which is not an error.
        let index = build_index(Path::new("definitely/not/a/mods/directory"));
        assert!(index.is_empty());
    }

    #[test]
    fn an_unreadable_jar_does_not_sink_the_whole_index() {
        // Disabled mods are routinely left in place truncated or renamed.
        let dir = temp_dir();
        std::fs::write(dir.join("broken.jar"), b"not a zip at all").unwrap();
        write_jar(&dir, "good.jar", &["assets/create/textures/item/wrench.png"]);

        let index = build_index(&dir);
        assert!(
            index.contains_key("create"),
            "the readable jar must still index"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_jar_files_are_ignored() {
        let dir = temp_dir();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("config.toml"), b"x").unwrap();

        assert!(build_index(&dir).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /* ------------------------------------------------------------- lookups */

    #[tokio::test]
    async fn reads_a_texture_at_a_predictable_path() {
        let dir = temp_dir();
        write_jar(
            &dir,
            "create.jar",
            &["assets/create/textures/item/wrench.png"],
        );

        let assets = ModAssets::new(dir.to_string_lossy().into_owned());
        let found = assets
            .get("create", "wrench", vec!["item/wrench.png".to_string()])
            .await;

        assert_eq!(found.as_deref(), Some(PNG));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn falls_back_to_scanning_when_no_candidate_matches() {
        // The reason local jars beat a mirror: nobody would guess
        // `cogwheel_side`, but the jar can simply be asked what it contains.
        let dir = temp_dir();
        write_jar(
            &dir,
            "create.jar",
            &["assets/create/textures/block/cogwheel_side.png"],
        );

        let assets = ModAssets::new(dir.to_string_lossy().into_owned());
        let found = assets
            .get(
                "create",
                "cogwheel",
                vec![
                    "item/cogwheel.png".to_string(),
                    "block/cogwheel.png".to_string(),
                ],
            )
            .await;

        assert_eq!(found.as_deref(), Some(PNG));

        std::fs::remove_dir_all(&dir).ok();
    }

    const PREFIX: &str = "assets/create/textures/";

    #[test]
    fn an_exact_name_outranks_a_qualified_variant() {
        let names = [
            "assets/create/textures/block/cogwheel_side_powered.png",
            "assets/create/textures/block/cogwheel.png",
            "assets/create/textures/block/cogwheel_side.png",
        ];

        assert_eq!(
            best_match(names.into_iter(), PREFIX, "cogwheel").as_deref(),
            Some("assets/create/textures/block/cogwheel.png")
        );
    }

    #[test]
    fn the_least_qualified_variant_wins_when_there_is_no_exact_match() {
        // The plain face rather than a state-specific one.
        let names = [
            "assets/create/textures/block/cogwheel_side_connected_powered.png",
            "assets/create/textures/block/cogwheel_side.png",
        ];

        assert_eq!(
            best_match(names.into_iter(), PREFIX, "cogwheel").as_deref(),
            Some("assets/create/textures/block/cogwheel_side.png")
        );
    }

    #[test]
    fn a_merely_similar_name_is_not_a_match() {
        // `large_cogwheel` contains `cogwheel` but is a different item, and
        // showing its picture would be a confident lie.
        let names = [
            "assets/create/textures/block/large_cogwheel.png",
            "assets/create/textures/block/andesite_casing.png",
        ];

        assert_eq!(best_match(names.into_iter(), PREFIX, "cogwheel"), None);
    }

    #[test]
    fn entries_outside_the_namespace_prefix_are_skipped() {
        let names = [
            "assets/mekanism/textures/item/cogwheel.png",
            "data/create/textures/block/cogwheel.png",
        ];

        assert_eq!(best_match(names.into_iter(), PREFIX, "cogwheel"), None);
    }

    #[tokio::test]
    async fn an_unrelated_item_is_not_matched_by_the_scan() {
        // The scan must not hand back the nearest-looking picture; a wrong
        // texture is worse than the fallback tile, because it reads as fact.
        let dir = temp_dir();
        write_jar(
            &dir,
            "create.jar",
            &["assets/create/textures/block/andesite_casing.png"],
        );

        let assets = ModAssets::new(dir.to_string_lossy().into_owned());
        let found = assets
            .get("create", "cogwheel", vec!["item/cogwheel.png".to_string()])
            .await;

        assert!(found.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_namespace_no_jar_provides_is_nothing() {
        let dir = temp_dir();
        write_jar(
            &dir,
            "create.jar",
            &["assets/create/textures/item/wrench.png"],
        );

        let assets = ModAssets::new(dir.to_string_lossy().into_owned());
        assert!(assets
            .get("mekanism", "ingot", vec!["item/ingot.png".to_string()])
            .await
            .is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn textures_are_kept_inside_their_own_namespace() {
        // assets/create/... must never answer for `mekanism:wrench`, or a
        // modpack would be full of confidently wrong pictures.
        let dir = temp_dir();
        write_jar(
            &dir,
            "both.jar",
            &[
                "assets/create/textures/item/wrench.png",
                "assets/mekanism/textures/item/configurator.png",
            ],
        );

        let assets = ModAssets::new(dir.to_string_lossy().into_owned());
        assert!(assets
            .get("mekanism", "wrench", vec!["item/wrench.png".to_string()])
            .await
            .is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn namespace_parsing_accepts_only_texture_entries() {
        assert_eq!(
            namespace_of("assets/create/textures/item/wrench.png"),
            Some("create")
        );
        assert_eq!(namespace_of("assets/create/models/item/wrench.json"), None);
        assert_eq!(namespace_of("data/create/recipes/x.json"), None);
        assert_eq!(
            namespace_of("assets/create/textures/item/wrench.mcmeta"),
            None
        );
        assert_eq!(namespace_of("META-INF/MANIFEST.MF"), None);
        assert_eq!(namespace_of("assets/"), None);
        // A namespace that could not be a lookup key is not recorded.
        assert_eq!(namespace_of("assets/../textures/item/x.png"), None);
    }
}
