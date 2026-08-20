//! Item textures for the inventory viewer, fetched once and then served local.
//!
//! The panel has no artwork of its own and the *server* jar carries none either
//! — item textures are a client asset. So the first time a texture is asked
//! for, it is pulled from a public mirror of the vanilla assets, written to a
//! cache directory, and served from there forever after. An admin's browser
//! only ever talks to this server, and a mirror that is down or blocked costs
//! nothing once the cache is warm.
//!
//! ## Finding the right file
//!
//! An item id does not map onto one predictable path. `diamond_sword` is
//! `textures/item/diamond_sword.png`, `stone` is `textures/block/stone.png`,
//! and `grass_block` is neither — its item form is a 3D render the assets do
//! not contain, so the closest flat stand-in is `block/grass_block_side.png`.
//! Rather than ship a mapping table that would go stale every release, each
//! candidate is tried in turn and the first hit is cached under the id.
//!
//! A miss is cached too, as a marker file holding the time it was recorded.
//! Without that, every render of an inventory containing an unmappable item
//! would re-ask the mirror for something that was never there.
//!
//! ## Only real misses are remembered
//!
//! A marker is written only when the mirror *answered* and had nothing at any
//! candidate path. A mirror that could not be reached at all — DNS not up yet
//! in a just-started container, egress blocked, a timeout — is not a miss and
//! must never be recorded as one. Getting that wrong is unusually costly here:
//! one bad moment during the first inventory render would write a permanent
//! marker for every id on screen, and every texture would 404 forever after,
//! long after connectivity came back.
//!
//! Markers also expire, so a texture added in a later assets release is picked
//! up eventually rather than being written off for good.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Mutex;

/// Only vanilla assets exist upstream, so a modded id is a miss without a
/// lookup. It also keeps a namespace from ever reaching the URL.
const VANILLA: &str = "minecraft";

/// Longest id segment accepted. Real ids are far shorter; this bounds the path
/// that gets built from one.
const MAX_SEGMENT: usize = 64;

/// A 16x16 PNG is a few hundred bytes. This is generous enough for the
/// high-resolution texture packs some mirrors carry and small enough that a
/// wrong URL cannot stream something huge into the cache.
const MAX_TEXTURE_BYTES: usize = 256 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a recorded miss is trusted. Long enough that an unmappable id is
/// not re-walked on every render, short enough that a texture added in a later
/// assets release eventually appears without anyone clearing the cache.
const MISS_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug)]
pub enum TextureError {
    /// The id could never name a texture — rejected before any I/O.
    BadId,
    /// Looked for and genuinely not there; the caller renders its own fallback.
    Missing,
    /// The mirror could not be consulted. Distinct from [`TextureError::Missing`]
    /// because it must not be cached, and because an operator staring at a wall
    /// of failed textures needs to be able to tell the two apart.
    Unreachable(String),
    /// The cache directory could not be written.
    Cache(String),
}

impl std::fmt::Display for TextureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextureError::BadId => write!(f, "not a valid item id"),
            TextureError::Missing => write!(f, "no texture for that item"),
            TextureError::Unreachable(why) => write!(f, "texture mirror unreachable: {why}"),
            TextureError::Cache(why) => write!(f, "texture cache unwritable: {why}"),
        }
    }
}

/// The outcome of asking the mirror for one candidate path.
enum Fetch {
    Found(Vec<u8>),
    /// The mirror answered and does not have this path.
    NotFound,
    /// The mirror could not be asked. Says nothing about whether the texture
    /// exists.
    Unreachable(String),
}

/// Where textures come from and where they are kept.
pub struct TextureCache {
    dir: PathBuf,
    /// Assets are published per Minecraft version, and item ids come and go
    /// between them, so the version is part of the cache path — changing it
    /// does not serve last version's textures from a stale cache.
    version: String,
    /// Empty disables fetching entirely, leaving a pre-populated cache as the
    /// only source. That is the air-gapped deployment, not an error.
    base_url: String,
    client: reqwest::Client,
    /// One in-flight fetch per id. Opening an inventory asks for forty textures
    /// at once, and several of those are usually the same item — without this,
    /// a cold cache would send the mirror a burst of duplicate requests.
    in_flight: DashMap<String, Arc<Mutex<()>>>,
}

impl TextureCache {
    pub fn new(dir: String, version: String, base_url: String) -> Arc<Self> {
        Arc::new(Self {
            dir: PathBuf::from(dir),
            version,
            base_url,
            client: reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent("apiRD-admin-panel")
                .build()
                .unwrap_or_default(),
            in_flight: DashMap::new(),
        })
    }

    /// Cached bytes for one item id, fetching it if this is the first ask.
    pub async fn get(&self, namespace: &str, name: &str) -> Result<Vec<u8>, TextureError> {
        if !is_valid_segment(namespace) || !is_valid_segment(name) {
            return Err(TextureError::BadId);
        }
        if namespace != VANILLA {
            return Err(TextureError::Missing);
        }

        let hit = self.cache_path(name);
        let miss = self.miss_path(name);

        if let Some(bytes) = read_cached(&hit).await {
            return Ok(bytes);
        }
        if is_fresh_miss(&miss).await {
            return Err(TextureError::Missing);
        }

        // Serialise concurrent asks for the same id, then re-check: whoever
        // held the lock has usually just written the file this caller wants.
        let guard = self
            .in_flight
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _held = guard.lock().await;

        if let Some(bytes) = read_cached(&hit).await {
            self.in_flight.remove(name);
            return Ok(bytes);
        }
        if is_fresh_miss(&miss).await {
            self.in_flight.remove(name);
            return Err(TextureError::Missing);
        }

        let result = self.fetch_and_store(name, &hit, &miss).await;
        self.in_flight.remove(name);
        result
    }

    async fn fetch_and_store(
        &self,
        name: &str,
        hit: &Path,
        miss: &Path,
    ) -> Result<Vec<u8>, TextureError> {
        if self.base_url.is_empty() {
            return Err(TextureError::Missing);
        }

        for candidate in candidate_paths(name) {
            let url = format!(
                "{}/{}/assets/minecraft/textures/{}",
                self.base_url.trim_end_matches('/'),
                self.version,
                candidate
            );

            match self.download(&url).await {
                Fetch::Found(bytes) => {
                    write_cached(hit, &bytes).await?;
                    return Ok(bytes);
                }
                Fetch::NotFound => continue,
                // Bail on the first unreachable rather than working through the
                // remaining candidates: if the mirror is down they will all
                // fail, and five timeouts in a row is forty seconds of an
                // operator waiting for a picture of a sword.
                Fetch::Unreachable(why) => {
                    // Logged at error rather than warn on purpose: the service
                    // runs with no RUST_LOG set, so `env_logger` shows error
                    // and nothing else. A diagnostic nobody can see is not a
                    // diagnostic, and this is the one condition here an
                    // operator actually has to know about.
                    log::error!("textures: could not reach {url}: {why}");
                    return Err(TextureError::Unreachable(why));
                }
            }
        }

        // Every candidate was answered and none existed. That is a real miss,
        // and the only case worth remembering.
        write_cached(miss, now_seconds().to_string().as_bytes()).await?;
        Err(TextureError::Missing)
    }

    /// Ask the mirror for one candidate URL.
    ///
    /// The distinction that matters is between a reply and no reply. A 404 is a
    /// reply and means the texture is not there; a refused connection is not,
    /// and says nothing at all about the texture.
    async fn download(&self, url: &str) -> Fetch {
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(e) => return Fetch::Unreachable(transport_reason(&e)),
        };

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Fetch::NotFound;
        }
        if !status.is_success() {
            // Rate limiting and mirror-side errors are transient, so they are
            // emphatically not a missing texture.
            return Fetch::Unreachable(format!("mirror returned HTTP {status}"));
        }

        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => return Fetch::Unreachable(transport_reason(&e)),
        };

        if bytes.len() > MAX_TEXTURE_BYTES || !bytes.starts_with(PNG_MAGIC) {
            // A success response that is not a PNG means this URL is wrong
            // rather than unavailable — an error page, say — so move on to the
            // next candidate instead of giving up on the mirror.
            log::warn!("textures: {url} answered 200 with something that is not a PNG");
            return Fetch::NotFound;
        }

        Fetch::Found(bytes.to_vec())
    }

    fn cache_path(&self, name: &str) -> PathBuf {
        self.dir.join(&self.version).join(format!("{name}.png"))
    }

    fn miss_path(&self, name: &str) -> PathBuf {
        self.dir.join(&self.version).join(format!("{name}.miss"))
    }
}

const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G'];

/// Whether one id segment is safe to build a path and a URL out of.
///
/// Both a filename and a URL are derived from this, so the rule is an allowlist
/// rather than an attempt to strip the dangerous parts: lowercase alphanumerics
/// plus `_`, `-` and `.`, with `..` refused outright.
pub fn is_valid_segment(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_SEGMENT
        && !raw.contains("..")
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// Where a given id might live, best guess first.
///
/// Items come first because most held things are items; the block fallbacks
/// follow because a placeable block's inventory icon is rendered from its
/// faces, and a face texture is the nearest flat thing to show.
pub fn candidate_paths(name: &str) -> Vec<String> {
    vec![
        format!("item/{name}.png"),
        format!("block/{name}.png"),
        format!("block/{name}_side.png"),
        format!("block/{name}_top.png"),
        format!("block/{name}_front.png"),
    ]
}

fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

fn transport_reason(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "could not connect".to_string()
    } else {
        e.to_string()
    }
}

/// Whether a miss marker exists and is still within its TTL.
///
/// A marker that is empty, unparseable or past its TTL reads as absent, so the
/// texture is looked up again. That is what makes a cache poisoned by an
/// outage self-healing: the markers written before this distinction existed
/// hold no timestamp, so they expire the moment this code runs.
async fn is_fresh_miss(path: &Path) -> bool {
    let Ok(contents) = tokio::fs::read_to_string(path).await else {
        return false;
    };

    let Ok(recorded) = contents.trim().parse::<i64>() else {
        return false;
    };

    now_seconds().saturating_sub(recorded) < MISS_TTL_SECONDS
}

async fn read_cached(path: &Path) -> Option<Vec<u8>> {
    let bytes = tokio::fs::read(path).await.ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

async fn write_cached(path: &Path, bytes: &[u8]) -> Result<(), TextureError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| TextureError::Cache(e.to_string()))?;
    }
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| TextureError::Cache(e.to_string()))
}

/// Split `minecraft:diamond_sword` into its namespace and name.
///
/// An id with no namespace is vanilla, which is how Minecraft itself reads one.
pub fn split_id(id: &str) -> (String, String) {
    match id.split_once(':') {
        Some((namespace, name)) => (namespace.to_ascii_lowercase(), name.to_ascii_lowercase()),
        None => (VANILLA.to_string(), id.to_ascii_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_item_names() {
        for name in ["diamond_sword", "stone", "oak_log", "music_disc_11", "tnt"] {
            assert!(is_valid_segment(name), "{name} should be accepted");
        }
    }

    #[test]
    fn refuses_anything_that_could_escape_the_cache_directory() {
        // This becomes both a filename and part of a URL, so traversal and
        // scheme-smuggling are the cases that matter.
        for bogus in [
            "..",
            "../../etc/passwd",
            "a/../../b",
            "item/../../secret",
            "Diamond_Sword",
            "sword?x=1",
            "sword.png#",
            "http://evil",
            "",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_segment(bogus), "{bogus:?} must be refused");
        }
    }

    #[test]
    fn a_dot_is_allowed_but_a_double_dot_is_not() {
        assert!(is_valid_segment("1.21"));
        assert!(!is_valid_segment("a..b"));
    }

    #[test]
    fn splits_a_namespaced_id() {
        assert_eq!(
            split_id("minecraft:diamond_sword"),
            ("minecraft".to_string(), "diamond_sword".to_string())
        );
        // Unqualified ids are vanilla, which is how Minecraft reads them.
        assert_eq!(
            split_id("stone"),
            ("minecraft".to_string(), "stone".to_string())
        );
        assert_eq!(
            split_id("Create:Copper_Backtank"),
            ("create".to_string(), "copper_backtank".to_string())
        );
    }

    #[test]
    fn items_are_looked_for_before_blocks() {
        let paths = candidate_paths("stone");
        assert_eq!(paths[0], "item/stone.png");
        assert_eq!(paths[1], "block/stone.png");
        // grass_block has no flat texture of its own; a face is the stand-in.
        assert!(candidate_paths("grass_block").contains(&"block/grass_block_side.png".to_string()));
    }

    #[tokio::test]
    async fn a_modded_id_is_a_miss_without_touching_the_network() {
        // The base URL is deliberately unreachable: reaching it would be the
        // bug this pins down.
        let cache = TextureCache::new(
            std::env::temp_dir()
                .join(format!("apird-tex-{}", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            "1.21.4".to_string(),
            "http://127.0.0.1:1".to_string(),
        );

        assert!(matches!(
            cache.get("create", "copper_backtank").await,
            Err(TextureError::Missing)
        ));
    }

    #[tokio::test]
    async fn a_cached_texture_is_served_without_fetching() {
        let dir = std::env::temp_dir().join(format!("apird-tex-{}", uuid::Uuid::new_v4()));
        let cache = TextureCache::new(
            dir.to_string_lossy().into_owned(),
            "1.21.4".to_string(),
            // Unreachable, so a cache hit is the only way this can succeed.
            "http://127.0.0.1:1".to_string(),
        );

        let png = [PNG_MAGIC, b"-body"].concat();
        let path = dir.join("1.21.4").join("diamond_sword.png");
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&path, &png).await.unwrap();

        assert_eq!(cache.get("minecraft", "diamond_sword").await.unwrap(), png);

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// Build a cache whose mirror is a dead port, so any attempt to reach it
    /// fails immediately and visibly.
    fn offline_cache(dir: &Path) -> Arc<TextureCache> {
        TextureCache::new(
            dir.to_string_lossy().into_owned(),
            "1.21.4".to_string(),
            "http://127.0.0.1:1".to_string(),
        )
    }

    async fn write_marker(dir: &Path, name: &str, contents: &[u8]) {
        let marker = dir.join("1.21.4").join(format!("{name}.miss"));
        tokio::fs::create_dir_all(marker.parent().unwrap()).await.unwrap();
        tokio::fs::write(&marker, contents).await.unwrap();
    }

    #[tokio::test]
    async fn a_recorded_miss_is_not_looked_up_again() {
        let dir = std::env::temp_dir().join(format!("apird-tex-{}", uuid::Uuid::new_v4()));
        let cache = offline_cache(&dir);

        write_marker(&dir, "nonexistent_item", now_seconds().to_string().as_bytes()).await;

        // Missing, not Unreachable: a fresh marker means the mirror is never
        // consulted, which is the whole point of recording one.
        assert!(matches!(
            cache.get("minecraft", "nonexistent_item").await,
            Err(TextureError::Missing)
        ));

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn an_unreachable_mirror_is_never_recorded_as_a_miss() {
        // The bug this pins down: one outage while an inventory was rendering
        // wrote a permanent marker for every id on screen, and every texture
        // 404d forever afterwards even once the network came back.
        let dir = std::env::temp_dir().join(format!("apird-tex-{}", uuid::Uuid::new_v4()));
        let cache = offline_cache(&dir);

        assert!(
            matches!(
                cache.get("minecraft", "diamond_sword").await,
                Err(TextureError::Unreachable(_))
            ),
            "a dead mirror must report itself, not claim the texture is absent"
        );

        let marker = dir.join("1.21.4").join("diamond_sword.miss");
        assert!(
            tokio::fs::metadata(&marker).await.is_err(),
            "an unreachable mirror must not poison the cache"
        );

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_stale_or_timestampless_marker_is_retried() {
        // Markers written before misses carried a timestamp hold nothing, and
        // must read as expired — that is what makes an already-poisoned cache
        // heal itself on deploy rather than needing to be cleared by hand.
        let dir = std::env::temp_dir().join(format!("apird-tex-{}", uuid::Uuid::new_v4()));
        let cache = offline_cache(&dir);

        for (label, contents) in [
            ("empty", Vec::new()),
            ("not a number", b"poisoned".to_vec()),
            (
                "past its ttl",
                (now_seconds() - MISS_TTL_SECONDS - 1).to_string().into_bytes(),
            ),
        ] {
            write_marker(&dir, "diamond_sword", &contents).await;

            // Reaching the (dead) mirror at all is the proof it was retried.
            assert!(
                matches!(
                    cache.get("minecraft", "diamond_sword").await,
                    Err(TextureError::Unreachable(_))
                ),
                "a {label} marker must be retried, not trusted"
            );
        }

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn fetching_disabled_serves_the_cache_and_nothing_else() {
        // The air-gapped deployment: an empty base URL is a configuration, not
        // a failure, so it must not spend the timeout dialling anything.
        let dir = std::env::temp_dir().join(format!("apird-tex-{}", uuid::Uuid::new_v4()));
        let cache = TextureCache::new(
            dir.to_string_lossy().into_owned(),
            "1.21.4".to_string(),
            String::new(),
        );

        // Missing rather than Unreachable: not configuring a mirror is a
        // deliberate state, not a failure to talk to one.
        assert!(matches!(
            cache.get("minecraft", "diamond_sword").await,
            Err(TextureError::Missing)
        ));

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_hostile_id_never_reaches_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("apird-tex-{}", uuid::Uuid::new_v4()));
        let cache = TextureCache::new(
            dir.to_string_lossy().into_owned(),
            "1.21.4".to_string(),
            "http://127.0.0.1:1".to_string(),
        );

        assert!(matches!(
            cache.get("minecraft", "../../../../etc/passwd").await,
            Err(TextureError::BadId)
        ));
        assert!(matches!(
            cache.get("../..", "stone").await,
            Err(TextureError::BadId)
        ));
        assert!(
            tokio::fs::metadata(&dir).await.is_err(),
            "a rejected id must not create anything"
        );
    }
}

/// Network-touching checks, excluded from the normal run.
///
/// `cargo test --lib textures::network -- --ignored --nocapture` exercises the
/// real mirror. Kept out of the default suite so CI and an offline machine do
/// not fail on someone else's uptime.
#[cfg(test)]
mod network {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn fetches_real_textures_end_to_end() {
        let dir = std::env::temp_dir().join(format!("apird-net-{}", uuid::Uuid::new_v4()));
        let cache = TextureCache::new(
            dir.to_string_lossy().into_owned(),
            "1.21.4".to_string(),
            "https://raw.githubusercontent.com/InventivetalentDev/minecraft-assets".to_string(),
        );

        for name in ["diamond_sword", "stone", "grass_block", "iron_helmet"] {
            match cache.get("minecraft", name).await {
                Ok(bytes) => println!("{name}: OK, {} bytes", bytes.len()),
                Err(e) => println!("{name}: FAILED -> {e:?}"),
            }
        }

        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}
