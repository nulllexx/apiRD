//! Reading a player's saved inventory out of their `playerdata` NBT file.
//!
//! Minecraft stores one gzipped NBT file per player who has ever joined, in
//! `<world>/playerdata/<uuid>.dat`. That file — not RCON — is the only source
//! that works for *everyone*: `data get entity` needs the player to be online,
//! whereas the `.dat` is there for someone who last logged in two years ago.
//!
//! The cost of using the file is freshness. The server writes it on disconnect
//! and on autosave, so an online player's inventory is as of their last save,
//! not as of this instant. Rather than paper over that, every snapshot carries
//! the file's mtime as `savedAt` and the caller shows it.
//!
//! ## Why the parsing is deliberately untyped
//!
//! The shape of an item stack has changed three times in recent memory:
//!
//! * up to 1.20.4 — `{id, Count: 1b, tag: {...}}`
//! * 1.20.5-1.21.4 — `{id, count: 1, components: {"minecraft:enchantments": {levels: {...}}}}`
//! * 1.21.5+ — enchantments became a bare map, and armour moved out of
//!   `Inventory` slots 100-103 into a top-level `equipment` compound
//!
//! Modelling that with `#[derive(Deserialize)]` structs means a server upgrade
//! turns the whole panel into an error. Walking [`fastnbt::Value`] instead lets
//! each field degrade on its own: an unrecognised enchantment layout costs the
//! enchantment list, not the inventory.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use fastnbt::Value;
use serde::Serialize;

use super::players::{level_name, read_optional, server_dir};

/// Slot counts, which double as the grid sizes the UI draws.
pub const HOTBAR_SLOTS: usize = 9;
pub const MAIN_SLOTS: usize = 27;
pub const ENDER_SLOTS: usize = 27;

/// Head, chest, legs, feet — the order they are worn down the body, which is
/// also the order the UI stacks them.
pub const ARMOUR_SLOTS: usize = 4;

/// Vanilla's numbering inside `Inventory` before 1.21.5 moved equipment out.
const ARMOUR_SLOT_BASE: i32 = 100;
const OFFHAND_SLOT: i32 = -106;

/// Refuse to read a `.dat` larger than this. A player file is tens of
/// kilobytes; anything at this size is a corrupt or hostile file, and the
/// decompressed cap is what actually stops a zip bomb from becoming an
/// out-of-memory kill.
const MAX_COMPRESSED: u64 = 8 * 1024 * 1024;
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;

/// Cap on nested container contents (shulker boxes, bundles) reported per item,
/// so one crafted stack cannot blow up the response.
const MAX_NESTED_ITEMS: usize = 27;

#[derive(Debug)]
pub enum InventoryError {
    /// No `.dat` for this UUID — a player who has never joined.
    Missing,
    /// Present but unreadable: truncated, not NBT, or a decompression failure.
    Unreadable(String),
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InventoryError::Missing => write!(f, "no saved player data"),
            InventoryError::Unreadable(why) => write!(f, "{why}"),
        }
    }
}

/* ------------------------------------------------------------------ shapes */

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Enchantment {
    pub id: String,
    /// `id` prettified for display — "minecraft:fire_aspect" -> "Fire Aspect".
    pub name: String,
    pub level: i32,
    /// True for an enchanted book, whose enchantments are stored rather than
    /// applied. The UI words those differently.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stored: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    /// The raw namespaced id, kept alongside the pretty name because it is what
    /// an operator types back into a `/give`.
    pub id: String,
    pub name: String,
    pub count: i32,
    /// An anvil-renamed or otherwise named stack, flattened to plain text.
    #[serde(rename = "customName", skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    /// Durability used, for tools that carry it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub damage: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enchantments: Vec<Enchantment>,
    /// Contents of a shulker box or bundle, one level deep. Nested containers
    /// are where confiscated loot actually hides, so a moderator who cannot see
    /// inside them is looking at the wrong half of the inventory.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contents: Vec<Item>,
}

/// Health, hunger and the rest of the header line above the grid.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Vitals {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub food: Option<i32>,
    #[serde(rename = "xpLevel", skip_serializing_if = "Option::is_none")]
    pub xp_level: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i32>,
    /// "survival", "creative", … or `None` when the field is absent.
    #[serde(rename = "gameMode", skip_serializing_if = "Option::is_none")]
    pub game_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<String>,
    /// Rounded block coordinates; fractional position is noise at this size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<[i64; 3]>,
}

/// Everything one "See inventory" click returns.
///
/// The three grids are fixed-length with `null` holes rather than a sparse list
/// keyed by slot, because the UI draws a fixed grid either way and doing the
/// gap-filling here keeps one slot-numbering convention in one place.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerSnapshot {
    pub hotbar: Vec<Option<Item>>,
    pub main: Vec<Option<Item>>,
    /// Head, chest, legs, feet.
    pub armour: Vec<Option<Item>>,
    pub offhand: Option<Item>,
    #[serde(rename = "enderChest")]
    pub ender_chest: Vec<Option<Item>>,
    /// Which hotbar slot they were holding, so the UI can mark it.
    #[serde(rename = "selectedSlot", skip_serializing_if = "Option::is_none")]
    pub selected_slot: Option<i32>,
    pub vitals: Vitals,
}

impl PlayerSnapshot {
    fn empty() -> Self {
        PlayerSnapshot {
            hotbar: vec![None; HOTBAR_SLOTS],
            main: vec![None; MAIN_SLOTS],
            armour: vec![None; ARMOUR_SLOTS],
            offhand: None,
            ender_chest: vec![None; ENDER_SLOTS],
            selected_slot: None,
            vitals: Vitals::default(),
        }
    }

    /// Total stacks held, for the summary line.
    pub fn item_count(&self) -> usize {
        let filled = |slots: &[Option<Item>]| slots.iter().filter(|s| s.is_some()).count();
        filled(&self.hotbar)
            + filled(&self.main)
            + filled(&self.armour)
            + filled(&self.ender_chest)
            + usize::from(self.offhand.is_some())
    }
}

/* ------------------------------------------------------------ NBT accessors */

fn as_i32(value: &Value) -> Option<i32> {
    match value {
        Value::Byte(v) => Some(i32::from(*v)),
        Value::Short(v) => Some(i32::from(*v)),
        Value::Int(v) => Some(*v),
        Value::Long(v) => i32::try_from(*v).ok(),
        _ => None,
    }
}

fn as_f32(value: &Value) -> Option<f32> {
    match value {
        Value::Float(v) => Some(*v),
        Value::Double(v) => Some(*v as f32),
        _ => as_i32(value).map(|v| v as f32),
    }
}

fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(v) => Some(v.as_str()),
        _ => None,
    }
}

fn as_compound(value: &Value) -> Option<&HashMap<String, Value>> {
    match value {
        Value::Compound(map) => Some(map),
        _ => None,
    }
}

fn as_list(value: &Value) -> Option<&[Value]> {
    match value {
        Value::List(items) => Some(items.as_slice()),
        _ => None,
    }
}

/// Look a key up trying each spelling in turn.
///
/// Mojang renamed several of these across versions in case alone (`Count` ->
/// `count`), so every read that spans the 1.20.5 boundary goes through here.
fn field<'a>(map: &'a HashMap<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| map.get(*key))
}

/* ------------------------------------------------------------- presentation */

/// "minecraft:diamond_sword" -> "Diamond Sword".
///
/// Deliberately mechanical: the real display names live in the client's
/// language files, which the server does not have, so a derived name that is
/// occasionally imperfect beats shipping a stale hardcoded table of two
/// thousand ids.
pub fn pretty_name(id: &str) -> String {
    let bare = id.rsplit(':').next().unwrap_or(id);

    bare.split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Flatten a text component down to the string a human reads.
///
/// Custom names arrive as a JSON string before 1.21.5 and as an NBT compound
/// after it, and either can be a tree of `extra` children. Only the text is
/// wanted here — colour and formatting are the client's business.
fn plain_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            // A component serialised as JSON.
            Ok(json) => plain_text_json(&json),
            // Pre-1.13 names were the literal string.
            Err(_) => Some(raw.clone()),
        },
        Value::Compound(map) => {
            let mut text = field(map, &["text"]).and_then(as_str).unwrap_or("").to_string();
            if let Some(extra) = field(map, &["extra"]).and_then(as_list) {
                for child in extra {
                    if let Some(part) = plain_text(child) {
                        text.push_str(&part);
                    }
                }
            }
            Some(text)
        }
        _ => None,
    }?;

    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn plain_text_json(json: &serde_json::Value) -> Option<String> {
    match json {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            Some(parts.iter().filter_map(plain_text_json).collect::<String>())
        }
        serde_json::Value::Object(map) => {
            let mut text = map
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(extra) = map.get("extra").and_then(serde_json::Value::as_array) {
                for child in extra {
                    if let Some(part) = plain_text_json(child) {
                        text.push_str(&part);
                    }
                }
            }
            Some(text)
        }
        _ => None,
    }
}

/* -------------------------------------------------------------- item parsing */

/// Pull enchantments out of whichever of the three layouts this server uses.
fn read_enchantments(item: &HashMap<String, Value>) -> Vec<Enchantment> {
    let mut found = Vec::new();

    // Legacy (<=1.20.4): a list of {id, lvl} under `tag`.
    if let Some(tag) = field(item, &["tag"]).and_then(as_compound) {
        for (key, stored) in [("Enchantments", false), ("StoredEnchantments", true)] {
            let Some(entries) = tag.get(key).and_then(as_list) else {
                continue;
            };
            for entry in entries {
                let Some(entry) = as_compound(entry) else { continue };
                let Some(id) = entry.get("id").and_then(as_str) else { continue };
                let level = entry.get("lvl").and_then(as_i32).unwrap_or(1);
                found.push(Enchantment {
                    name: pretty_name(id),
                    id: id.to_string(),
                    level,
                    stored,
                });
            }
        }
    }

    // Modern (>=1.20.5): a component, holding either `{levels: {id: lvl}}`
    // (1.20.5-1.21.4) or the id -> level map directly (1.21.5+).
    if let Some(components) = field(item, &["components"]).and_then(as_compound) {
        for (key, stored) in [
            ("minecraft:enchantments", false),
            ("minecraft:stored_enchantments", true),
        ] {
            let Some(component) = components.get(key).and_then(as_compound) else {
                continue;
            };
            let levels = component
                .get("levels")
                .and_then(as_compound)
                .unwrap_or(component);

            for (id, level) in levels {
                // In the bare-map layout the compound also carries
                // `show_in_tooltip`, which is a number under a key that is not
                // an id. Every real enchantment id is namespaced.
                if !id.contains(':') {
                    continue;
                }
                let Some(level) = as_i32(level) else { continue };
                found.push(Enchantment {
                    name: pretty_name(id),
                    id: id.clone(),
                    level,
                    stored,
                });
            }
        }
    }

    // The map layouts iterate in hash order, which would otherwise reshuffle
    // the tooltip on every request.
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// Contents of a shulker box or bundle, one level deep.
fn read_contents(item: &HashMap<String, Value>) -> Vec<Item> {
    // Legacy: tag.BlockEntityTag.Items for a shulker, tag.Items for a bundle.
    if let Some(tag) = field(item, &["tag"]).and_then(as_compound) {
        let nested = field(tag, &["BlockEntityTag"])
            .and_then(as_compound)
            .and_then(|block| block.get("Items"))
            .or_else(|| tag.get("Items"))
            .and_then(as_list);

        if let Some(entries) = nested {
            return entries
                .iter()
                .filter_map(as_compound)
                .filter_map(|entry| read_item(entry, false))
                .take(MAX_NESTED_ITEMS)
                .collect();
        }
    }

    // Modern: a container component holding {slot, item} pairs, or a bundle
    // component holding the stacks directly.
    if let Some(components) = field(item, &["components"]).and_then(as_compound) {
        if let Some(entries) = components.get("minecraft:container").and_then(as_list) {
            return entries
                .iter()
                .filter_map(as_compound)
                .filter_map(|entry| entry.get("item").and_then(as_compound))
                .filter_map(|entry| read_item(entry, false))
                .take(MAX_NESTED_ITEMS)
                .collect();
        }
        if let Some(entries) = components
            .get("minecraft:bundle_contents")
            .and_then(as_list)
        {
            return entries
                .iter()
                .filter_map(as_compound)
                .filter_map(|entry| read_item(entry, false))
                .take(MAX_NESTED_ITEMS)
                .collect();
        }
    }

    Vec::new()
}

/// Turn one item compound into an [`Item`], or `None` if it is not one.
///
/// `recurse` stops at one level of nesting: a shulker inside a shulker is not
/// possible in vanilla, and refusing to recurse means a hand-edited file cannot
/// make this walk forever.
fn read_item(item: &HashMap<String, Value>, recurse: bool) -> Option<Item> {
    let id = field(item, &["id"]).and_then(as_str)?;
    if id.is_empty() || id == "minecraft:air" {
        return None;
    }

    // `Count` is a byte before 1.20.5 and `count` an int after it. A stack that
    // records no count at all is one item, which is what the server assumes.
    let count = field(item, &["count", "Count"]).and_then(as_i32).unwrap_or(1);
    if count <= 0 {
        return None;
    }

    let custom_name = field(item, &["components"])
        .and_then(as_compound)
        .and_then(|components| components.get("minecraft:custom_name"))
        .or_else(|| {
            field(item, &["tag"])
                .and_then(as_compound)
                .and_then(|tag| tag.get("display"))
                .and_then(as_compound)
                .and_then(|display| display.get("Name"))
        })
        .and_then(plain_text);

    let damage = field(item, &["components"])
        .and_then(as_compound)
        .and_then(|components| components.get("minecraft:damage"))
        .or_else(|| {
            field(item, &["tag"])
                .and_then(as_compound)
                .and_then(|tag| tag.get("Damage"))
        })
        .and_then(as_i32)
        .filter(|damage| *damage > 0);

    Some(Item {
        name: pretty_name(id),
        id: id.to_string(),
        count,
        custom_name,
        damage,
        enchantments: read_enchantments(item),
        contents: if recurse { read_contents(item) } else { Vec::new() },
    })
}

/* --------------------------------------------------------- snapshot assembly */

/// Place one stack into the grid its slot number names.
///
/// Everything about vanilla's slot numbering is contained here: 0-8 hotbar,
/// 9-35 the main grid, 100-103 armour counted up from the feet, -106 offhand.
fn place(snapshot: &mut PlayerSnapshot, slot: i32, item: Item) {
    match slot {
        OFFHAND_SLOT => snapshot.offhand = Some(item),
        0..=8 => snapshot.hotbar[slot as usize] = Some(item),
        9..=35 => snapshot.main[(slot - 9) as usize] = Some(item),
        100..=103 => {
            // Stored feet-first; displayed head-first.
            let from_feet = (slot - ARMOUR_SLOT_BASE) as usize;
            snapshot.armour[ARMOUR_SLOTS - 1 - from_feet] = Some(item);
        }
        _ => {}
    }
}

/// 1.21.5+ keeps worn gear in a top-level `equipment` compound instead of
/// numbered inventory slots.
fn read_equipment(snapshot: &mut PlayerSnapshot, root: &HashMap<String, Value>) {
    let Some(equipment) = root.get("equipment").and_then(as_compound) else {
        return;
    };

    for (index, key) in ["head", "chest", "legs", "feet"].iter().enumerate() {
        if let Some(item) = equipment.get(*key).and_then(as_compound).and_then(|i| read_item(i, true)) {
            snapshot.armour[index] = Some(item);
        }
    }

    if let Some(item) = equipment
        .get("offhand")
        .and_then(as_compound)
        .and_then(|i| read_item(i, true))
    {
        snapshot.offhand = Some(item);
    }
}

fn read_vitals(root: &HashMap<String, Value>) -> Vitals {
    let game_mode = root.get("playerGameType").and_then(as_i32).map(|mode| {
        match mode {
            0 => "survival",
            1 => "creative",
            2 => "adventure",
            3 => "spectator",
            _ => "unknown",
        }
        .to_string()
    });

    let position = root.get("Pos").and_then(as_list).and_then(|pos| {
        let coord = |i: usize| match pos.get(i) {
            Some(Value::Double(v)) => Some(v.floor() as i64),
            Some(Value::Float(v)) => Some(v.floor() as i64),
            other => other.and_then(as_i32).map(i64::from),
        };
        Some([coord(0)?, coord(1)?, coord(2)?])
    });

    Vitals {
        health: root.get("Health").and_then(as_f32),
        food: root.get("foodLevel").and_then(as_i32),
        xp_level: root.get("XpLevel").and_then(as_i32),
        score: root.get("Score").and_then(as_i32),
        game_mode,
        dimension: root
            .get("Dimension")
            .and_then(as_str)
            .map(str::to_string),
        position,
    }
}

/// Parse a decompressed player `.dat` into a snapshot.
pub fn parse_player_data(nbt: &[u8]) -> Result<PlayerSnapshot, InventoryError> {
    let root: HashMap<String, Value> = fastnbt::from_bytes(nbt)
        .map_err(|e| InventoryError::Unreadable(format!("not valid player NBT: {e}")))?;

    Ok(snapshot_from_root(&root))
}

/// Build a snapshot from a player's root compound.
///
/// Split out from [`parse_player_data`] so the live view can feed it a root
/// assembled from `/data get` replies instead of from a file. Both paths then
/// read items, slots and versions through exactly the same code, which is the
/// point: two implementations would eventually disagree about something like
/// which end of the armour row is the helmet.
pub fn snapshot_from_root(root: &HashMap<String, Value>) -> PlayerSnapshot {
    let mut snapshot = PlayerSnapshot::empty();

    for entry in root.get("Inventory").and_then(as_list).unwrap_or_default() {
        let Some(entry) = as_compound(entry) else { continue };
        let Some(slot) = entry.get("Slot").and_then(as_i32) else { continue };
        if let Some(item) = read_item(entry, true) {
            place(&mut snapshot, slot, item);
        }
    }

    // After `Inventory`, so on a 1.21.5 world the equipment compound wins over
    // anything a converted world left behind in slots 100-103.
    read_equipment(&mut snapshot, root);

    for entry in root.get("EnderItems").and_then(as_list).unwrap_or_default() {
        let Some(entry) = as_compound(entry) else { continue };
        let Some(slot) = entry.get("Slot").and_then(as_i32) else { continue };
        if !(0..ENDER_SLOTS as i32).contains(&slot) {
            continue;
        }
        if let Some(item) = read_item(entry, true) {
            snapshot.ender_chest[slot as usize] = Some(item);
        }
    }

    snapshot.selected_slot = root
        .get("SelectedItemSlot")
        .and_then(as_i32)
        .filter(|slot| (0..HOTBAR_SLOTS as i32).contains(slot));
    snapshot.vitals = read_vitals(root);

    snapshot
}

/* ------------------------------------------------------------- live reading */

/// The entity paths the live view asks the server for, in the order it asks.
///
/// Four small `/data get` commands rather than one `data get entity <name>`:
/// the unfiltered dump of a player entity includes their whole recipe book and
/// attribute list, which is both enormous and none of this panel's business.
/// Each path also fails independently, so a server too old to have `equipment`
/// still answers the other three.
pub const LIVE_PATHS: [&str; 4] = ["Inventory", "EnderItems", "equipment", "SelectedItemSlot"];

/// Build the `/data get` command for one path.
///
/// The player name is interpolated into a command string, so it goes through
/// the same validation the moderation actions use — a name with a space in it
/// would silently become extra arguments. Returns `None` rather than a mangled
/// command for anything that does not pass.
pub fn live_command(player: &str, path: &str) -> Option<String> {
    // `path` is only ever one of LIVE_PATHS, but checking it here means a future
    // caller cannot turn this into a way to run arbitrary command text.
    if !LIVE_PATHS.contains(&path) || !super::players::is_valid_name(player) {
        return None;
    }
    Some(format!("data get entity {player} {path}"))
}

/// Assemble a root compound out of `/data get` replies.
///
/// Each reply is independent: one that is prose rather than data — "No entity
/// was found" when the player logged off mid-request, or the error an older
/// server gives for a path it does not have — is dropped, and the rest still
/// build a snapshot. The caller decides what an empty result means, because
/// "no live data at all" and "a genuinely empty inventory" are different
/// answers and only the caller knows which one it asked for.
pub fn live_root(replies: &[(&str, String)]) -> HashMap<String, Value> {
    let mut root = HashMap::new();

    for (path, reply) in replies {
        let Some(body) = super::snbt::strip_reply_prefix(reply) else {
            continue;
        };
        match super::snbt::parse(body) {
            Ok(value) => {
                root.insert((*path).to_string(), value);
            }
            Err(e) => {
                // Worth a log line: this is how a format change in a future
                // Minecraft first shows up.
                log::debug!("inventory: live reply for {path} did not parse: {e}");
            }
        }
    }

    root
}

/// Whether a live root carries anything worth showing.
///
/// An inventory can legitimately be empty, so the test is whether the server
/// answered any of the *container* paths at all — not whether they had items in
/// them. `SelectedItemSlot` alone is not enough to call a view live.
pub fn live_root_is_usable(root: &HashMap<String, Value>) -> bool {
    ["Inventory", "EnderItems", "equipment"]
        .iter()
        .any(|path| root.contains_key(*path))
}

/* ---------------------------------------------------------------- file I/O */

/// Decompress a player file.
///
/// Vanilla gzips these, but converted or third-party worlds are occasionally
/// zlib-compressed or plain, so the magic bytes decide rather than the
/// extension. The read is capped: NBT gives no size up front, so an
/// unbounded decompress of an attacker-supplied file is an OOM waiting to
/// happen.
pub fn decompress(raw: &[u8]) -> Result<Vec<u8>, InventoryError> {
    let mut out = Vec::new();

    let read = match raw {
        [0x1f, 0x8b, ..] => flate2::read::GzDecoder::new(raw)
            .take(MAX_DECOMPRESSED)
            .read_to_end(&mut out),
        // zlib, which some converters emit even though vanilla does not.
        [0x78, _, ..] => flate2::read::ZlibDecoder::new(raw)
            .take(MAX_DECOMPRESSED)
            .read_to_end(&mut out),
        // Already plain NBT: a compound tag, so the first byte is 0x0a.
        _ => {
            out.extend_from_slice(raw);
            Ok(out.len())
        }
    };

    read.map_err(|e| InventoryError::Unreadable(format!("cannot decompress: {e}")))?;

    if out.is_empty() {
        return Err(InventoryError::Unreadable("player data file is empty".to_string()));
    }
    if out.len() as u64 >= MAX_DECOMPRESSED {
        return Err(InventoryError::Unreadable(
            "player data expands past the size limit".to_string(),
        ));
    }

    Ok(out)
}

/// The `playerdata` file for one UUID, and its `.dat_old` fallback.
///
/// The server writes `.dat_old` before replacing `.dat`, so if a save was
/// interrupted the backup is the more recent *complete* file of the two.
pub fn playerdata_paths(server_properties_path: &str, properties: &str, uuid: &str) -> [PathBuf; 2] {
    let dir = server_dir(server_properties_path)
        .join(level_name(properties))
        .join("playerdata");

    [dir.join(format!("{uuid}.dat")), dir.join(format!("{uuid}.dat_old"))]
}

/// Whether a string is a canonical hyphenated UUID.
///
/// This is the path-traversal guard: the value becomes a filename, so anything
/// that is not exactly 8-4-4-4-12 hex is refused rather than sanitised. A
/// rejected UUID is a 400, which is the honest answer for a value that could
/// never name a player file.
pub fn is_canonical_uuid(raw: &str) -> bool {
    let groups: Vec<&str> = raw.split('-').collect();
    if groups.len() != 5 {
        return false;
    }

    [8usize, 4, 4, 4, 12]
        .iter()
        .zip(&groups)
        .all(|(len, group)| {
            group.len() == *len && group.chars().all(|c| c.is_ascii_hexdigit())
        })
}

/// Read, decompress and parse one player's saved data.
///
/// Returns the snapshot and the file's mtime, which is when the server last
/// wrote it — the "saved at" the UI shows so nobody mistakes an hour-old
/// autosave for live state.
pub async fn load_snapshot(
    server_properties_path: &str,
    uuid: &str,
) -> Result<(PlayerSnapshot, Option<String>), InventoryError> {
    let properties = read_optional(Path::new(server_properties_path)).await;
    let paths = playerdata_paths(server_properties_path, &properties, uuid);

    let mut last_error: Option<InventoryError> = None;

    for path in &paths {
        let metadata = match tokio::fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        if metadata.len() > MAX_COMPRESSED {
            last_error = Some(InventoryError::Unreadable(
                "player data file is implausibly large".to_string(),
            ));
            continue;
        }

        let raw = match tokio::fs::read(path).await {
            Ok(raw) => raw,
            Err(e) => {
                last_error = Some(InventoryError::Unreadable(format!("cannot read: {e}")));
                continue;
            }
        };

        let saved_at = metadata.modified().ok().map(|modified| {
            let stamp: chrono::DateTime<chrono::Utc> = modified.into();
            stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });

        match decompress(&raw).and_then(|nbt| parse_player_data(&nbt)) {
            Ok(snapshot) => return Ok((snapshot, saved_at)),
            // Fall through to `.dat_old`: a half-written `.dat` is exactly the
            // case the backup exists for.
            Err(e) => last_error = Some(e),
        }
    }

    Err(last_error.unwrap_or(InventoryError::Missing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /* ------------------------------------------------------------ fixtures */

    fn compound(fields: &[(&str, Value)]) -> Value {
        Value::Compound(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn string(value: &str) -> Value {
        Value::String(value.to_string())
    }

    /// A stack in the pre-1.20.5 shape: byte count, everything else under `tag`.
    fn legacy_item(id: &str, count: i8, slot: i32, tag: Option<Value>) -> Value {
        let mut fields = vec![
            ("id", string(id)),
            ("Count", Value::Byte(count)),
            ("Slot", Value::Int(slot)),
        ];
        if let Some(tag) = tag {
            fields.push(("tag", tag));
        }
        compound(&fields)
    }

    /// A stack in the 1.20.5+ shape: int count, everything else a component.
    fn modern_item(id: &str, count: i32, slot: i32, components: Option<Value>) -> Value {
        let mut fields = vec![
            ("id", string(id)),
            ("count", Value::Int(count)),
            ("Slot", Value::Int(slot)),
        ];
        if let Some(components) = components {
            fields.push(("components", components));
        }
        compound(&fields)
    }

    fn parse(root: Value) -> PlayerSnapshot {
        let bytes = fastnbt::to_bytes(&root).expect("fixture serialises");
        parse_player_data(&bytes).expect("fixture parses")
    }

    fn player(fields: &[(&str, Value)]) -> PlayerSnapshot {
        parse(compound(fields))
    }

    /* ---------------------------------------------------------- item names */

    #[test]
    fn ids_become_readable_names() {
        assert_eq!(pretty_name("minecraft:diamond_sword"), "Diamond Sword");
        assert_eq!(pretty_name("minecraft:tnt"), "Tnt");
        assert_eq!(pretty_name("stone"), "Stone");
        // A modded id keeps its own namespace stripped just the same.
        assert_eq!(pretty_name("create:copper_backtank"), "Copper Backtank");
    }

    #[test]
    fn a_nameless_id_does_not_panic() {
        assert_eq!(pretty_name(""), "");
        assert_eq!(pretty_name("minecraft:"), "");
        assert_eq!(pretty_name("__"), "");
    }

    /* --------------------------------------------------------- item shapes */

    #[test]
    fn reads_a_legacy_stack() {
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![legacy_item("minecraft:cobblestone", 64, 0, None)]),
        )]);

        let item = snapshot.hotbar[0].as_ref().expect("slot 0 filled");
        assert_eq!(item.id, "minecraft:cobblestone");
        assert_eq!(item.name, "Cobblestone");
        assert_eq!(item.count, 64);
    }

    #[test]
    fn reads_a_modern_stack() {
        // The rename of Count -> count in 1.20.5 is the single most likely thing
        // to silently zero out the whole panel after a server upgrade.
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![modern_item("minecraft:cobblestone", 32, 0, None)]),
        )]);

        assert_eq!(snapshot.hotbar[0].as_ref().unwrap().count, 32);
    }

    #[test]
    fn a_stack_with_no_count_is_one_item() {
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![compound(&[
                ("id", string("minecraft:stone")),
                ("Slot", Value::Int(0)),
            ])]),
        )]);

        assert_eq!(snapshot.hotbar[0].as_ref().unwrap().count, 1);
    }

    #[test]
    fn air_and_empty_stacks_are_not_items() {
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![
                legacy_item("minecraft:air", 1, 0, None),
                legacy_item("minecraft:stone", 0, 1, None),
                compound(&[("Slot", Value::Int(2))]),
            ]),
        )]);

        assert!(snapshot.hotbar.iter().all(Option::is_none));
        assert_eq!(snapshot.item_count(), 0);
    }

    /* --------------------------------------------------------------- slots */

    #[test]
    fn slots_land_in_the_grid_they_name() {
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![
                legacy_item("minecraft:stone", 1, 0, None),
                legacy_item("minecraft:dirt", 1, 8, None),
                legacy_item("minecraft:oak_log", 1, 9, None),
                legacy_item("minecraft:sand", 1, 35, None),
                legacy_item("minecraft:shield", 1, OFFHAND_SLOT, None),
            ]),
        )]);

        assert_eq!(snapshot.hotbar[0].as_ref().unwrap().id, "minecraft:stone");
        assert_eq!(snapshot.hotbar[8].as_ref().unwrap().id, "minecraft:dirt");
        assert_eq!(snapshot.main[0].as_ref().unwrap().id, "minecraft:oak_log");
        assert_eq!(snapshot.main[26].as_ref().unwrap().id, "minecraft:sand");
        assert_eq!(snapshot.offhand.as_ref().unwrap().id, "minecraft:shield");
    }

    #[test]
    fn armour_is_stored_feet_first_and_shown_head_first() {
        // Vanilla numbers 100..103 upward from the boots; the panel draws the
        // player top down, so getting this backwards is a silent mix-up rather
        // than an error.
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![
                legacy_item("minecraft:diamond_boots", 1, 100, None),
                legacy_item("minecraft:diamond_leggings", 1, 101, None),
                legacy_item("minecraft:diamond_chestplate", 1, 102, None),
                legacy_item("minecraft:diamond_helmet", 1, 103, None),
            ]),
        )]);

        let worn: Vec<&str> = snapshot
            .armour
            .iter()
            .map(|slot| slot.as_ref().unwrap().id.as_str())
            .collect();
        assert_eq!(
            worn,
            vec![
                "minecraft:diamond_helmet",
                "minecraft:diamond_chestplate",
                "minecraft:diamond_leggings",
                "minecraft:diamond_boots",
            ]
        );
    }

    #[test]
    fn an_out_of_range_slot_is_ignored_rather_than_panicking() {
        // A hand-edited or modded file can carry anything here, and indexing a
        // fixed-size grid with it would be a panic in a request handler.
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![
                legacy_item("minecraft:stone", 1, 50, None),
                legacy_item("minecraft:stone", 1, 9999, None),
                legacy_item("minecraft:stone", 1, -50, None),
            ]),
        )]);

        assert_eq!(snapshot.item_count(), 0);
    }

    #[test]
    fn equipment_compound_supersedes_numbered_armour_slots() {
        // 1.21.5 moved worn gear out of Inventory. A world converted to it can
        // still have the old slots lying around, so the new field must win.
        let snapshot = player(&[
            (
                "Inventory",
                Value::List(vec![legacy_item("minecraft:leather_helmet", 1, 103, None)]),
            ),
            (
                "equipment",
                compound(&[
                    ("head", modern_item("minecraft:netherite_helmet", 1, 0, None)),
                    (
                        "offhand",
                        modern_item("minecraft:totem_of_undying", 1, 0, None),
                    ),
                ]),
            ),
        ]);

        assert_eq!(
            snapshot.armour[0].as_ref().unwrap().id,
            "minecraft:netherite_helmet"
        );
        assert_eq!(
            snapshot.offhand.as_ref().unwrap().id,
            "minecraft:totem_of_undying"
        );
    }

    #[test]
    fn reads_the_ender_chest() {
        let snapshot = player(&[(
            "EnderItems",
            Value::List(vec![
                legacy_item("minecraft:gold_ingot", 12, 0, None),
                legacy_item("minecraft:emerald", 3, 26, None),
                // Out of range for a 27-slot chest.
                legacy_item("minecraft:stone", 1, 40, None),
            ]),
        )]);

        assert_eq!(snapshot.ender_chest[0].as_ref().unwrap().count, 12);
        assert_eq!(
            snapshot.ender_chest[26].as_ref().unwrap().id,
            "minecraft:emerald"
        );
        assert_eq!(
            snapshot.ender_chest.iter().filter(|s| s.is_some()).count(),
            2
        );
    }

    /* -------------------------------------------------------- enchantments */

    #[test]
    fn reads_legacy_enchantments() {
        let tag = compound(&[(
            "Enchantments",
            Value::List(vec![compound(&[
                ("id", string("minecraft:sharpness")),
                ("lvl", Value::Short(5)),
            ])]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![legacy_item("minecraft:diamond_sword", 1, 0, Some(tag))]),
        )]);

        let enchants = &snapshot.hotbar[0].as_ref().unwrap().enchantments;
        assert_eq!(enchants.len(), 1);
        assert_eq!(enchants[0].name, "Sharpness");
        assert_eq!(enchants[0].level, 5);
        assert!(!enchants[0].stored);
    }

    #[test]
    fn reads_the_1_20_5_levels_component() {
        let components = compound(&[(
            "minecraft:enchantments",
            compound(&[(
                "levels",
                compound(&[("minecraft:efficiency", Value::Int(4))]),
            )]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![modern_item(
                "minecraft:diamond_pickaxe",
                1,
                0,
                Some(components),
            )]),
        )]);

        let enchants = &snapshot.hotbar[0].as_ref().unwrap().enchantments;
        assert_eq!(enchants.len(), 1);
        assert_eq!(enchants[0].id, "minecraft:efficiency");
        assert_eq!(enchants[0].level, 4);
    }

    #[test]
    fn reads_the_1_21_5_bare_map_and_skips_its_flags() {
        // The bare map holds `show_in_tooltip` alongside the enchantments, and
        // it is a number too — only the namespace tells them apart.
        let components = compound(&[(
            "minecraft:enchantments",
            compound(&[
                ("minecraft:unbreaking", Value::Int(3)),
                ("show_in_tooltip", Value::Byte(1)),
            ]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![modern_item("minecraft:elytra", 1, 0, Some(components))]),
        )]);

        let enchants = &snapshot.hotbar[0].as_ref().unwrap().enchantments;
        assert_eq!(enchants.len(), 1, "the flag must not become an enchantment");
        assert_eq!(enchants[0].id, "minecraft:unbreaking");
    }

    #[test]
    fn a_books_enchantments_are_marked_as_stored() {
        let components = compound(&[(
            "minecraft:stored_enchantments",
            compound(&[("minecraft:mending", Value::Int(1))]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![modern_item(
                "minecraft:enchanted_book",
                1,
                0,
                Some(components),
            )]),
        )]);

        assert!(snapshot.hotbar[0].as_ref().unwrap().enchantments[0].stored);
    }

    #[test]
    fn enchantments_come_back_in_a_stable_order() {
        // They live in a hash map, so without sorting the tooltip would
        // reshuffle between two requests for the same unchanged item.
        let components = compound(&[(
            "minecraft:enchantments",
            compound(&[
                ("minecraft:sharpness", Value::Int(5)),
                ("minecraft:looting", Value::Int(3)),
                ("minecraft:mending", Value::Int(1)),
                ("minecraft:unbreaking", Value::Int(3)),
            ]),
        )]);
        let root = compound(&[(
            "Inventory",
            Value::List(vec![modern_item(
                "minecraft:netherite_sword",
                1,
                0,
                Some(components),
            )]),
        )]);

        let first = parse(root.clone()).hotbar[0].clone().unwrap().enchantments;
        let second = parse(root).hotbar[0].clone().unwrap().enchantments;
        assert_eq!(first, second);
        let ids: Vec<&str> = first.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "minecraft:looting",
                "minecraft:mending",
                "minecraft:sharpness",
                "minecraft:unbreaking"
            ]
        );
    }

    /* ---------------------------------------------------- names and damage */

    #[test]
    fn reads_a_legacy_json_custom_name() {
        let tag = compound(&[
            (
                "display",
                compound(&[("Name", string("{\"text\":\"Bonk\",\"color\":\"red\"}"))]),
            ),
            ("Damage", Value::Int(37)),
        ]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![legacy_item("minecraft:stick", 1, 0, Some(tag))]),
        )]);

        let item = snapshot.hotbar[0].as_ref().unwrap();
        assert_eq!(item.custom_name.as_deref(), Some("Bonk"));
        assert_eq!(item.damage, Some(37));
    }

    #[test]
    fn reads_a_custom_name_stored_as_a_component_tree() {
        // 1.21.5 stores text components as NBT rather than as a JSON string.
        let components = compound(&[
            (
                "minecraft:custom_name",
                compound(&[
                    ("text", string("Sword of ")),
                    (
                        "extra",
                        Value::List(vec![compound(&[("text", string("Doom"))])]),
                    ),
                ]),
            ),
            ("minecraft:damage", Value::Int(5)),
        ]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![modern_item(
                "minecraft:iron_sword",
                1,
                0,
                Some(components),
            )]),
        )]);

        let item = snapshot.hotbar[0].as_ref().unwrap();
        assert_eq!(item.custom_name.as_deref(), Some("Sword of Doom"));
        assert_eq!(item.damage, Some(5));
    }

    #[test]
    fn an_undamaged_item_reports_no_damage() {
        let tag = compound(&[("Damage", Value::Int(0))]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![legacy_item("minecraft:iron_axe", 1, 0, Some(tag))]),
        )]);

        assert_eq!(snapshot.hotbar[0].as_ref().unwrap().damage, None);
    }

    /* ---------------------------------------------------------- containers */

    #[test]
    fn looks_inside_a_legacy_shulker_box() {
        // Where confiscated loot actually is, so an inventory view that stops at
        // the box is showing a moderator the wrong half of the picture.
        let tag = compound(&[(
            "BlockEntityTag",
            compound(&[(
                "Items",
                Value::List(vec![legacy_item("minecraft:diamond", 64, 0, None)]),
            )]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![legacy_item("minecraft:shulker_box", 1, 0, Some(tag))]),
        )]);

        let contents = &snapshot.hotbar[0].as_ref().unwrap().contents;
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].id, "minecraft:diamond");
        assert_eq!(contents[0].count, 64);
    }

    #[test]
    fn looks_inside_a_modern_container_component() {
        let components = compound(&[(
            "minecraft:container",
            Value::List(vec![compound(&[
                ("slot", Value::Int(0)),
                ("item", modern_item("minecraft:netherite_ingot", 5, 0, None)),
            ])]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![modern_item(
                "minecraft:shulker_box",
                1,
                0,
                Some(components),
            )]),
        )]);

        let contents = &snapshot.hotbar[0].as_ref().unwrap().contents;
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].id, "minecraft:netherite_ingot");
    }

    #[test]
    fn nesting_stops_after_one_level() {
        // A hand-edited file can nest boxes inside boxes forever; the walk must
        // not follow it.
        let inner = compound(&[(
            "BlockEntityTag",
            compound(&[(
                "Items",
                Value::List(vec![legacy_item("minecraft:diamond", 1, 0, None)]),
            )]),
        )]);
        let outer = compound(&[(
            "BlockEntityTag",
            compound(&[(
                "Items",
                Value::List(vec![legacy_item("minecraft:shulker_box", 1, 0, Some(inner))]),
            )]),
        )]);
        let snapshot = player(&[(
            "Inventory",
            Value::List(vec![legacy_item("minecraft:shulker_box", 1, 0, Some(outer))]),
        )]);

        let contents = &snapshot.hotbar[0].as_ref().unwrap().contents;
        assert_eq!(contents.len(), 1);
        assert!(
            contents[0].contents.is_empty(),
            "the second level must not be walked"
        );
    }

    /* -------------------------------------------------------------- vitals */

    #[test]
    fn reads_the_header_fields() {
        let snapshot = player(&[
            ("Health", Value::Float(17.5)),
            ("foodLevel", Value::Int(12)),
            ("XpLevel", Value::Int(30)),
            ("Score", Value::Int(451)),
            ("playerGameType", Value::Int(1)),
            ("Dimension", string("minecraft:the_nether")),
            ("SelectedItemSlot", Value::Int(3)),
            (
                "Pos",
                Value::List(vec![
                    Value::Double(103.7),
                    Value::Double(64.0),
                    Value::Double(-88.2),
                ]),
            ),
        ]);

        assert_eq!(snapshot.vitals.health, Some(17.5));
        assert_eq!(snapshot.vitals.food, Some(12));
        assert_eq!(snapshot.vitals.xp_level, Some(30));
        assert_eq!(snapshot.vitals.score, Some(451));
        assert_eq!(snapshot.vitals.game_mode.as_deref(), Some("creative"));
        assert_eq!(
            snapshot.vitals.dimension.as_deref(),
            Some("minecraft:the_nether")
        );
        // Floored, so a negative coordinate lands in the block the player is in.
        assert_eq!(snapshot.vitals.position, Some([103, 64, -89]));
        assert_eq!(snapshot.selected_slot, Some(3));
    }

    #[test]
    fn a_player_file_with_nothing_in_it_is_an_empty_snapshot_not_an_error() {
        // A brand-new player who has not moved yet, and the degraded case for
        // any field this parser does not recognise.
        let snapshot = player(&[("XpLevel", Value::Int(0))]);

        assert_eq!(snapshot.item_count(), 0);
        assert_eq!(snapshot.hotbar.len(), HOTBAR_SLOTS);
        assert_eq!(snapshot.main.len(), MAIN_SLOTS);
        assert_eq!(snapshot.ender_chest.len(), ENDER_SLOTS);
        assert_eq!(snapshot.vitals.health, None);
        assert_eq!(snapshot.vitals.game_mode, None);
    }

    #[test]
    fn a_selected_slot_outside_the_hotbar_is_dropped() {
        let snapshot = player(&[("SelectedItemSlot", Value::Int(40))]);
        assert_eq!(snapshot.selected_slot, None);
    }

    /* --------------------------------------------------------- compression */

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn gzipped_player_data_round_trips() {
        let bytes = fastnbt::to_bytes(&compound(&[("XpLevel", Value::Int(7))])).unwrap();
        let snapshot = parse_player_data(&decompress(&gzip(&bytes)).unwrap()).unwrap();
        assert_eq!(snapshot.vitals.xp_level, Some(7));
    }

    #[test]
    fn uncompressed_player_data_is_passed_through() {
        // Not what vanilla writes, but some converters and test fixtures do.
        let bytes = fastnbt::to_bytes(&compound(&[("XpLevel", Value::Int(7))])).unwrap();
        assert_eq!(decompress(&bytes).unwrap(), bytes);
    }

    #[test]
    fn an_empty_or_truncated_file_is_an_error_not_a_blank_inventory() {
        // Showing an empty inventory for an unreadable file would read as "this
        // player owns nothing", which is a moderation decision made on a lie.
        assert!(matches!(decompress(&[]), Err(InventoryError::Unreadable(_))));
        assert!(matches!(
            decompress(&[0x1f, 0x8b, 0x08, 0x00, 0x00]),
            Err(InventoryError::Unreadable(_))
        ));
        assert!(matches!(
            parse_player_data(b"not nbt at all"),
            Err(InventoryError::Unreadable(_))
        ));
    }

    /* ----------------------------------------------------------- uuid guard */

    #[test]
    fn accepts_a_canonical_uuid() {
        assert!(is_canonical_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf5"));
        assert!(is_canonical_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(is_canonical_uuid("069A79F4-44E9-4726-A5BE-FCA90E38AAF5"));
    }

    #[test]
    fn refuses_anything_that_could_name_another_file() {
        // This value becomes a path segment, so the traversal cases matter more
        // than the merely malformed ones.
        for bogus in [
            "../../../../etc/passwd",
            "069a79f4-44e9-4726-a5be-fca90e38aaf5/../../secret",
            "069a79f4-44e9-4726-a5be-fca90e38aaf5.dat",
            "069a79f444e94726a5befca90e38aaf5",
            "069a79f4-44e9-4726-a5be-fca90e38aaf",
            "069a79g4-44e9-4726-a5be-fca90e38aaf5",
            "",
            "-",
        ] {
            assert!(!is_canonical_uuid(bogus), "{bogus:?} must be refused");
        }
    }

    /* -------------------------------------------------------------- on disk */

    #[tokio::test]
    async fn falls_back_to_the_dat_old_backup() {
        // The server writes .dat_old before replacing .dat, so a save caught
        // half-written leaves the backup as the only complete file.
        let root = std::env::temp_dir().join(format!("apird-inv-{}", uuid::Uuid::new_v4()));
        let playerdata = root.join("world").join("playerdata");
        tokio::fs::create_dir_all(&playerdata).await.unwrap();
        tokio::fs::write(root.join("server.properties"), "level-name=world")
            .await
            .unwrap();

        let uuid = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
        let good = gzip(&fastnbt::to_bytes(&compound(&[("XpLevel", Value::Int(9))])).unwrap());
        tokio::fs::write(
            playerdata.join(format!("{uuid}.dat")),
            b"truncated garbage",
        )
        .await
        .unwrap();
        tokio::fs::write(playerdata.join(format!("{uuid}.dat_old")), &good)
            .await
            .unwrap();

        let properties = root.join("server.properties");
        let (snapshot, saved_at) = load_snapshot(properties.to_str().unwrap(), uuid)
            .await
            .expect("the backup should be read");

        assert_eq!(snapshot.vitals.xp_level, Some(9));
        assert!(saved_at.is_some(), "the mtime is what dates the snapshot");

        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn a_player_who_has_never_joined_is_missing_not_an_error() {
        let root = std::env::temp_dir().join(format!("apird-inv-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(root.join("world").join("playerdata"))
            .await
            .unwrap();
        tokio::fs::write(root.join("server.properties"), "level-name=world")
            .await
            .unwrap();

        let properties = root.join("server.properties");
        let result = load_snapshot(
            properties.to_str().unwrap(),
            "069a79f4-44e9-4726-a5be-fca90e38aaf5",
        )
        .await;

        assert!(matches!(result, Err(InventoryError::Missing)));

        tokio::fs::remove_dir_all(&root).await.ok();
    }
}
