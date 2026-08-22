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
    /// The colour that name is written in, as `#rrggbb`.
    ///
    /// Named after the NBT field it comes from rather than translated, because
    /// it is a passthrough: Minecraft's `color` is either one of sixteen names
    /// or a hex literal, and both end up here as hex.
    #[serde(rename = "customNameColor", skip_serializing_if = "Option::is_none")]
    pub custom_name_color: Option<String>,
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
    /// Progress towards the next level, 0.0 to 1.0 — the fill of the XP bar.
    #[serde(rename = "xpProgress", skip_serializing_if = "Option::is_none")]
    pub xp_progress: Option<f32>,
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

impl Vitals {
    /// Fill anything this set is missing from `other`, and report whether it
    /// had to borrow.
    ///
    /// The live view assembles vitals from one `/data get` per field, and any
    /// one of those can come back unparseable while the rest succeed. This lets
    /// the saved file cover the holes without replacing the fields that *did*
    /// answer, and the returned flag is what lets the panel say "live" only
    /// when it means it.
    pub fn fill_gaps_from(&mut self, other: &Vitals) -> bool {
        let mut borrowed = false;

        macro_rules! fill {
            ($($field:ident),+ $(,)?) => {$(
                if self.$field.is_none() {
                    if other.$field.is_some() {
                        borrowed = true;
                    }
                    self.$field = other.$field.clone();
                }
            )+};
        }

        fill!(
            health,
            food,
            xp_level,
            xp_progress,
            score,
            game_mode,
            dimension,
            position,
        );

        borrowed
    }
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

/// Minecraft's sixteen named colours, as the client draws them.
///
/// A text component's `color` is either one of these or a `#rrggbb` literal.
/// Resolving the names here means the panel only ever deals in hex.
const NAMED_COLOURS: [(&str, &str); 16] = [
    ("black", "#000000"),
    ("dark_blue", "#0000aa"),
    ("dark_green", "#00aa00"),
    ("dark_aqua", "#00aaaa"),
    ("dark_red", "#aa0000"),
    ("dark_purple", "#aa00aa"),
    ("gold", "#ffaa00"),
    ("gray", "#aaaaaa"),
    ("dark_gray", "#555555"),
    ("blue", "#5555ff"),
    ("green", "#55ff55"),
    ("aqua", "#55ffff"),
    ("red", "#ff5555"),
    ("light_purple", "#ff55ff"),
    ("yellow", "#ffff55"),
    ("white", "#ffffff"),
];

/// Resolve a component's colour to `#rrggbb`.
///
/// Only the root component's colour is read. A name assembled out of `extra`
/// children in several colours cannot be described by one value, and the panel
/// falls back to rendering such a name in the default colour rather than
/// picking one of them and being confidently wrong.
fn text_colour(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(raw) => serde_json::from_str::<serde_json::Value>(raw)
            .ok()?
            .get("color")?
            .as_str()?
            .to_string(),
        Value::Compound(map) => field(map, &["color"]).and_then(as_str)?.to_string(),
        _ => return None,
    };

    let raw = raw.trim().to_ascii_lowercase();

    // A hex literal, which the game accepts as `#rrggbb` only.
    if let Some(digits) = raw.strip_prefix('#') {
        return (digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| format!("#{digits}"));
    }

    NAMED_COLOURS
        .iter()
        .find(|(name, _)| *name == raw)
        .map(|(_, hex)| (*hex).to_string())
}

/// Flatten a text component down to the string a human reads.
///
/// Custom names arrive as a JSON string before 1.21.5 and as an NBT compound
/// after it, and either can be a tree of `extra` children. Only the text comes
/// out here; [`text_colour`] reads the colour separately, because the two are
/// wanted in different places and a name is useful without one.
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

    let name_component = field(item, &["components"])
        .and_then(as_compound)
        .and_then(|components| components.get("minecraft:custom_name"))
        .or_else(|| {
            field(item, &["tag"])
                .and_then(as_compound)
                .and_then(|tag| tag.get("display"))
                .and_then(as_compound)
                .and_then(|display| display.get("Name"))
        });

    let custom_name = name_component.and_then(plain_text);
    // Only meaningful next to a name, so it is not read when there is none.
    let custom_name_color = custom_name
        .as_ref()
        .and_then(|_| name_component)
        .and_then(text_colour);

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
        custom_name_color,
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
        xp_progress: root.get("XpP").and_then(as_f32),
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
pub const LIVE_PATHS: [&str; 12] = [
    // The grids.
    "Inventory",
    "EnderItems",
    "equipment",
    "SelectedItemSlot",
    // The header. One `/data get` each, because the command takes a single
    // path and there is no batching form of it. They are small replies on a
    // local socket, and the alternative is an Overview whose health bar shows
    // whatever the player had at the last autosave — which for someone who is
    // online is precisely the number an operator must not act on.
    "Health",
    "foodLevel",
    "XpLevel",
    "XpP",
    "Score",
    "playerGameType",
    "Dimension",
    "Pos",
];

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

/* ------------------------------------------------- editing an offline player

   Health and hunger cannot be *commanded* on someone who is not online: every
   Minecraft command that touches them selects a loaded entity, and an offline
   player is not one. But the state itself is right there in the same file this
   module already reads, and nothing is holding it -- the server keeps an online
   player in memory and writes the file on save, so for anyone offline the file
   *is* the player.

   Which makes the offline versions the exact ones. `Heal` online is an Instant
   Health effect with a huge amplifier; offline it is `Health = max`. `Starve`
   online drains over several seconds because the Hunger effect is all the game
   offers; offline it is `foodLevel = 0`, at once.
*/

/// Vanilla's max health, used when the file does not say otherwise.
const DEFAULT_MAX_HEALTH: f32 = 20.0;

/// A full food bar, which is also the saturation ceiling.
const MAX_FOOD: i32 = 20;

/// What one of the vitals buttons does to a player who is not online.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineVital {
    Heal,
    Feed,
    Starve,
}

impl OfflineVital {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "heal" => Some(OfflineVital::Heal),
            "feed" => Some(OfflineVital::Feed),
            "starve" => Some(OfflineVital::Starve),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OfflineVital::Heal => "heal",
            OfflineVital::Feed => "feed",
            OfflineVital::Starve => "starve",
        }
    }
}

/// The player's maximum health, from their attributes.
///
/// A modpack that raises the ceiling stores it here, and healing such a player
/// to a hardcoded twenty would be a demotion. Only the attribute's *base* is
/// read: modifiers come from equipment and effects that are not applied while
/// they are offline anyway.
///
/// 1.21 renamed the identifying key from `Name` to `id`, so both are tried.
fn max_health(root: &HashMap<String, Value>) -> f32 {
    let attributes = root.get("Attributes").and_then(as_list).unwrap_or(&[]);

    for entry in attributes {
        let Some(map) = as_compound(entry) else { continue };
        let Some(name) = field(map, &["id", "Name"]).and_then(as_str) else {
            continue;
        };
        if name != "minecraft:max_health" && name != "minecraft:generic.max_health" {
            continue;
        }
        if let Some(base) = field(map, &["base", "Base"]).and_then(as_f32) {
            if base.is_finite() && base > 0.0 {
                return base;
            }
        }
    }

    DEFAULT_MAX_HEALTH
}

/// Apply a vital to a parsed player compound, in place.
///
/// Every value is written with the NBT type the game reads it as -- `Health` is
/// a float and `foodLevel` an int, and writing either as the other produces a
/// file the server will not load. Split out from the file handling so the
/// decision can be tested without a filesystem.
pub fn apply_vital(root: &mut HashMap<String, Value>, vital: OfflineVital) {
    match vital {
        OfflineVital::Heal => {
            // Never a reduction: a player already above their attribute maximum
            // (an absorption-like modifier, a mod that grants extra) keeps it.
            let current = root.get("Health").and_then(as_f32).unwrap_or(0.0);
            let full = max_health(root).max(current);
            root.insert("Health".to_string(), Value::Float(full));
        }
        OfflineVital::Feed => {
            root.insert("foodLevel".to_string(), Value::Int(MAX_FOOD));
            root.insert(
                "foodSaturationLevel".to_string(),
                Value::Float(MAX_FOOD as f32),
            );
            // Exhaustion is the meter that eats saturation. Leaving it high
            // would start draining the bar the moment they logged back in.
            root.insert("foodExhaustionLevel".to_string(), Value::Float(0.0));
        }
        OfflineVital::Starve => {
            root.insert("foodLevel".to_string(), Value::Int(0));
            root.insert("foodSaturationLevel".to_string(), Value::Float(0.0));
            root.insert("foodExhaustionLevel".to_string(), Value::Float(0.0));
        }
    }
}

/// Check that this file survives a parse and re-serialise unchanged.
///
/// A player file is full of tags this code knows nothing about -- mod
/// attachments, attribute modifiers, advancement state, recipe books. Writing
/// back a compound that dropped or retyped any of them corrupts a character
/// rather than heals one, and the damage would not show up until they next
/// logged in.
///
/// So rather than trust the round trip, every write proves it first: serialise
/// what was read, read it back, and require the two to be identical. A file
/// this cannot reproduce exactly is refused untouched. Compound *ordering* is
/// not compared, because NBT compounds are unordered maps and the game reads
/// them by key.
fn survives_a_round_trip(root: &HashMap<String, Value>) -> bool {
    let Ok(bytes) = fastnbt::to_bytes(&Value::Compound(root.clone())) else {
        return false;
    };
    match fastnbt::from_bytes::<HashMap<String, Value>>(&bytes) {
        Ok(reparsed) => &reparsed == root,
        Err(_) => false,
    }
}

/// Where the untouched original is kept before a write.
///
/// Deliberately not `.dat_old`: that one belongs to the server, which rotates
/// it on every save, and overwriting it would destroy the backup the game
/// itself falls back to.
fn backup_path(dat: &Path) -> PathBuf {
    let mut name = dat.as_os_str().to_os_string();
    name.push(".apird-backup");
    PathBuf::from(name)
}

/// Heal, feed or starve a player who is not online, by editing their file.
///
/// Returns the snapshot as it now stands, so the caller can show the result
/// without a second read.
///
/// The caller must have established that the player is offline. Editing the
/// file of someone who is playing is not dangerous, but it is pointless: the
/// server holds their state in memory and writes over it at the next autosave.
pub async fn apply_offline_vital(
    server_properties_path: &str,
    uuid: &str,
    vital: OfflineVital,
) -> Result<PlayerSnapshot, InventoryError> {
    let properties = read_optional(Path::new(server_properties_path)).await;
    // Index 0 only. `.dat_old` is the server's backup and is never written.
    let dat = playerdata_paths(server_properties_path, &properties, uuid)[0].clone();

    let metadata = tokio::fs::metadata(&dat)
        .await
        .map_err(|_| InventoryError::Missing)?;
    if metadata.len() > MAX_COMPRESSED {
        return Err(InventoryError::Unreadable(
            "player data file is implausibly large".to_string(),
        ));
    }
    let before = metadata.modified().ok();

    let raw = tokio::fs::read(&dat)
        .await
        .map_err(|e| InventoryError::Unreadable(format!("cannot read: {e}")))?;

    let nbt = decompress(&raw)?;
    let mut root: HashMap<String, Value> = fastnbt::from_bytes(&nbt)
        .map_err(|e| InventoryError::Unreadable(format!("not valid player NBT: {e}")))?;

    if !survives_a_round_trip(&root) {
        return Err(InventoryError::Unreadable(
            "this player's data could not be rewritten without losing part of it,              so it has been left alone"
                .to_string(),
        ));
    }

    apply_vital(&mut root, vital);

    let edited = fastnbt::to_bytes(&Value::Compound(root.clone()))
        .map_err(|e| InventoryError::Unreadable(format!("cannot re-encode: {e}")))?;

    let mut gzipped = Vec::new();
    {
        use std::io::Write as _;
        let mut encoder =
            flate2::write::GzEncoder::new(&mut gzipped, flate2::Compression::default());
        encoder
            .write_all(&edited)
            .and_then(|_| encoder.finish().map(|_| ()))
            .map_err(|e| InventoryError::Unreadable(format!("cannot compress: {e}")))?;
    }

    // Last check before committing: if the file has been touched since it was
    // read, the server has it -- the player logged in, or a save ran -- and
    // this write would either be lost or would clobber newer state.
    if tokio::fs::metadata(&dat).await.ok().and_then(|m| m.modified().ok()) != before {
        return Err(InventoryError::Unreadable(
            "the server wrote this player's data while it was being edited; nothing was changed"
                .to_string(),
        ));
    }

    // Keep the original where it can be put back by hand, then replace the file
    // in one rename so a crash mid-write cannot leave a half-file behind.
    tokio::fs::copy(&dat, backup_path(&dat))
        .await
        .map_err(|e| InventoryError::Unreadable(format!("cannot back up: {e}")))?;

    let temporary = dat.with_extension("dat-apird-tmp");
    tokio::fs::write(&temporary, &gzipped)
        .await
        .map_err(|e| InventoryError::Unreadable(format!("cannot write: {e}")))?;
    tokio::fs::rename(&temporary, &dat)
        .await
        .map_err(|e| InventoryError::Unreadable(format!("cannot replace: {e}")))?;

    Ok(snapshot_from_root(&root))
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

    /* ------------------------------------------------- offline vitals edits */

    fn root_of(fields: &[(&str, Value)]) -> HashMap<String, Value> {
        match compound(fields) {
            Value::Compound(map) => map,
            _ => unreachable!("compound() builds a compound"),
        }
    }

    #[test]
    fn heal_fills_health_to_the_vanilla_maximum() {
        let mut root = root_of(&[("Health", Value::Float(3.5))]);
        apply_vital(&mut root, OfflineVital::Heal);

        assert_eq!(root.get("Health"), Some(&Value::Float(20.0)));
    }

    /// A modpack that raises the ceiling stores it in the attributes, and
    /// healing to a hardcoded twenty would be a demotion.
    #[test]
    fn heal_respects_a_raised_max_health_attribute() {
        let mut root = root_of(&[
            ("Health", Value::Float(4.0)),
            ("Attributes", Value::List(vec![
                compound(&[
                    ("id", Value::String("minecraft:armor".to_string())),
                    ("base", Value::Double(0.0)),
                ]),
                compound(&[
                    ("id", Value::String("minecraft:max_health".to_string())),
                    ("base", Value::Double(30.0)),
                ]),
            ])),
        ]);

        apply_vital(&mut root, OfflineVital::Heal);
        assert_eq!(root.get("Health"), Some(&Value::Float(30.0)));
    }

    /// The pre-1.21 spelling of the same attribute, which an older world still
    /// carries.
    #[test]
    fn heal_reads_the_legacy_attribute_name() {
        let mut root = root_of(&[
            ("Health", Value::Float(1.0)),
            ("Attributes", Value::List(vec![compound(&[
                ("Name", Value::String("minecraft:generic.max_health".to_string())),
                ("Base", Value::Double(24.0)),
            ])])),
        ]);

        apply_vital(&mut root, OfflineVital::Heal);
        assert_eq!(root.get("Health"), Some(&Value::Float(24.0)));
    }

    /// Heal must never take health away from someone already above the
    /// attribute base.
    #[test]
    fn heal_never_reduces_health() {
        let mut root = root_of(&[("Health", Value::Float(26.0))]);
        apply_vital(&mut root, OfflineVital::Heal);

        assert_eq!(root.get("Health"), Some(&Value::Float(26.0)));
    }

    #[test]
    fn feed_fills_food_and_clears_exhaustion() {
        let mut root = root_of(&[
            ("foodLevel", Value::Int(2)),
            ("foodSaturationLevel", Value::Float(0.0)),
            ("foodExhaustionLevel", Value::Float(3.9)),
        ]);

        apply_vital(&mut root, OfflineVital::Feed);

        assert_eq!(root.get("foodLevel"), Some(&Value::Int(20)));
        assert_eq!(root.get("foodSaturationLevel"), Some(&Value::Float(20.0)));
        // Left high, this would start draining the bar the moment they logged in.
        assert_eq!(root.get("foodExhaustionLevel"), Some(&Value::Float(0.0)));
    }

    #[test]
    fn starve_empties_the_bar_exactly() {
        let mut root = root_of(&[
            ("foodLevel", Value::Int(20)),
            ("foodSaturationLevel", Value::Float(20.0)),
        ]);

        apply_vital(&mut root, OfflineVital::Starve);

        assert_eq!(root.get("foodLevel"), Some(&Value::Int(0)));
        assert_eq!(root.get("foodSaturationLevel"), Some(&Value::Float(0.0)));
    }

    /// The game reads `Health` as a float and `foodLevel` as an int. Writing
    /// either as the other produces a file the server refuses to load, and the
    /// damage only shows up when the player next joins.
    #[test]
    fn the_written_types_are_the_ones_the_game_reads() {
        let mut root = root_of(&[]);
        apply_vital(&mut root, OfflineVital::Heal);
        apply_vital(&mut root, OfflineVital::Feed);

        assert!(matches!(root.get("Health"), Some(Value::Float(_))));
        assert!(matches!(root.get("foodLevel"), Some(Value::Int(_))));
        assert!(matches!(root.get("foodSaturationLevel"), Some(Value::Float(_))));
    }

    /// Feeding must not disturb health, and healing must not disturb hunger.
    #[test]
    fn each_vital_touches_only_its_own_fields() {
        let mut root = root_of(&[
            ("Health", Value::Float(7.0)),
            ("foodLevel", Value::Int(3)),
            ("XpLevel", Value::Int(42)),
        ]);

        apply_vital(&mut root, OfflineVital::Feed);
        assert_eq!(root.get("Health"), Some(&Value::Float(7.0)));
        assert_eq!(root.get("XpLevel"), Some(&Value::Int(42)));

        apply_vital(&mut root, OfflineVital::Starve);
        assert_eq!(root.get("Health"), Some(&Value::Float(7.0)));
    }

    #[test]
    fn offline_vital_names_parse_and_round_trip() {
        for vital in [OfflineVital::Heal, OfflineVital::Feed, OfflineVital::Starve] {
            assert_eq!(OfflineVital::parse(vital.as_str()), Some(vital));
        }
        assert_eq!(OfflineVital::parse(" HEAL "), Some(OfflineVital::Heal));
        // Kill has no offline form and must not quietly acquire one.
        for bogus in ["", "kill", "op", "heal me"] {
            assert_eq!(OfflineVital::parse(bogus), None, "{bogus:?} must be refused");
        }
    }

    /* ------------------------------------------- the write path, end to end */

    /// Build a server directory with one player file in it, and return the
    /// fake `server.properties` path the API is configured with.
    async fn server_with_player(label: &str, uuid: &str, root: &HashMap<String, Value>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apird-offline-{label}"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let playerdata = dir.join("world").join("playerdata");
        tokio::fs::create_dir_all(&playerdata).await.unwrap();

        let properties = dir.join("server.properties");
        tokio::fs::write(&properties, "level-name=world\n").await.unwrap();

        let nbt = fastnbt::to_bytes(&Value::Compound(root.clone())).unwrap();
        tokio::fs::write(playerdata.join(format!("{uuid}.dat")), gzip(&nbt))
            .await
            .unwrap();

        properties
    }

    #[tokio::test]
    async fn healing_an_offline_player_rewrites_their_file() {
        let uuid = "11111111-2222-3333-4444-555555555555";
        let root = root_of(&[
            ("Health", Value::Float(2.0)),
            ("foodLevel", Value::Int(4)),
            ("XpLevel", Value::Int(9)),
            // Something this code has no idea about, which must survive.
            ("neoforge:attachments", compound(&[
                ("create:goggles", Value::Byte(1)),
                ("deep", Value::List(vec![Value::Long(-1), Value::Long(2)])),
            ])),
        ]);
        let properties = server_with_player("heal", uuid, &root).await;

        let after = apply_offline_vital(properties.to_str().unwrap(), uuid, OfflineVital::Heal)
            .await
            .expect("the edit succeeds");
        assert_eq!(after.vitals.health, Some(20.0));

        // Read it back off disk the way the panel would.
        let (reloaded, _) = load_snapshot(properties.to_str().unwrap(), uuid)
            .await
            .expect("the rewritten file still parses");
        assert_eq!(reloaded.vitals.health, Some(20.0));
        assert_eq!(reloaded.vitals.food, Some(4), "hunger was not touched");
        assert_eq!(reloaded.vitals.xp_level, Some(9), "XP was not touched");
    }

    /// The point of the round-trip guard: everything this module does not
    /// understand has to come back out of the file byte for byte.
    #[tokio::test]
    async fn an_edit_preserves_tags_this_code_knows_nothing_about() {
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let attachments = compound(&[
            ("create:heat", Value::Double(1234.5)),
            ("bytes", Value::ByteArray(fastnbt::ByteArray::new(vec![-1, 0, 1]))),
            ("longs", Value::LongArray(fastnbt::LongArray::new(vec![i64::MAX]))),
            ("empty", Value::List(Vec::new())),
        ]);
        let root = root_of(&[
            ("Health", Value::Float(1.0)),
            ("recipeBook", attachments.clone()),
        ]);
        let properties = server_with_player("preserve", uuid, &root).await;

        apply_offline_vital(properties.to_str().unwrap(), uuid, OfflineVital::Feed)
            .await
            .expect("the edit succeeds");

        let dat = std::path::Path::new(properties.to_str().unwrap())
            .parent()
            .unwrap()
            .join("world")
            .join("playerdata")
            .join(format!("{uuid}.dat"));
        let raw = tokio::fs::read(&dat).await.unwrap();
        let parsed: HashMap<String, Value> =
            fastnbt::from_bytes(&decompress(&raw).unwrap()).unwrap();

        assert_eq!(parsed.get("recipeBook"), Some(&attachments));
        // And the edit itself landed.
        assert_eq!(parsed.get("foodLevel"), Some(&Value::Int(20)));
    }

    /// The original must be recoverable by hand, and the server's own
    /// `.dat_old` must not be the thing that gets clobbered to do it.
    #[tokio::test]
    async fn the_original_is_kept_and_dat_old_is_left_alone() {
        let uuid = "12341234-1234-1234-1234-123412341234";
        let root = root_of(&[("Health", Value::Float(6.0))]);
        let properties = server_with_player("backup", uuid, &root).await;

        let playerdata = properties.parent().unwrap().join("world").join("playerdata");
        let dat_old = playerdata.join(format!("{uuid}.dat_old"));
        tokio::fs::write(&dat_old, b"the server's own backup").await.unwrap();

        apply_offline_vital(properties.to_str().unwrap(), uuid, OfflineVital::Heal)
            .await
            .expect("the edit succeeds");

        let backup = playerdata.join(format!("{uuid}.dat.apird-backup"));
        let raw = tokio::fs::read(&backup).await.expect("a backup was kept");
        let parsed: HashMap<String, Value> =
            fastnbt::from_bytes(&decompress(&raw).unwrap()).unwrap();
        assert_eq!(parsed.get("Health"), Some(&Value::Float(6.0)), "pre-edit state");

        assert_eq!(
            tokio::fs::read(&dat_old).await.unwrap(),
            b"the server's own backup",
            "the game's own backup must not be touched"
        );

        // And no working file was left lying around.
        assert!(
            !playerdata.join(format!("{uuid}.dat-apird-tmp")).exists(),
            "the temporary file must be renamed away, not left behind"
        );
    }

    #[tokio::test]
    async fn editing_a_player_with_no_file_is_a_miss() {
        let uuid = "99999999-9999-9999-9999-999999999999";
        let root = root_of(&[("Health", Value::Float(1.0))]);
        let properties = server_with_player("missing", "00000000-0000-0000-0000-000000000000", &root).await;

        let result =
            apply_offline_vital(properties.to_str().unwrap(), uuid, OfflineVital::Heal).await;
        assert!(matches!(result, Err(InventoryError::Missing)));
    }

    /// A file that cannot be reproduced exactly is refused rather than
    /// rewritten. Nothing today fails this, which is the point of asserting it.
    #[test]
    fn a_realistic_player_compound_survives_the_guard() {
        let root = root_of(&[
            ("Health", Value::Float(17.5)),
            ("Air", Value::Short(300)),
            ("OnGround", Value::Byte(1)),
            ("UUID", Value::IntArray(fastnbt::IntArray::new(vec![1, 2, 3, 4]))),
            ("Pos", Value::List(vec![Value::Double(1.0), Value::Double(2.0)])),
            ("mod", compound(&[("nested", Value::String("x".to_string()))])),
        ]);

        assert!(survives_a_round_trip(&root));
    }

    /* ------------------------------------------------- custom name colours */

    /// `compound()` returns a Value; `read_item` wants the map inside it.
    fn map_of(fields: &[(&str, Value)]) -> HashMap<String, Value> {
        match compound(fields) {
            Value::Compound(map) => map,
            _ => unreachable!("compound() builds a compound"),
        }
    }

    #[test]
    fn a_named_colour_resolves_to_hex() {
        for (name, hex) in [("gold", "#ffaa00"), ("light_purple", "#ff55ff"), ("black", "#000000")] {
            let component = compound(&[
                ("text", Value::String("Excalibur".to_string())),
                ("color", Value::String(name.to_string())),
            ]);
            assert_eq!(text_colour(&component).as_deref(), Some(hex), "{name}");
        }
    }

    /// 1.21.5 stores the component as NBT; everything before it as a JSON
    /// string. Both have to yield the same colour.
    #[test]
    fn a_colour_is_read_from_either_component_encoding() {
        let as_json = Value::String(r#"{"text":"Excalibur","color":"gold"}"#.to_string());
        assert_eq!(text_colour(&as_json).as_deref(), Some("#ffaa00"));

        let as_nbt = compound(&[
            ("text", Value::String("Excalibur".to_string())),
            ("color", Value::String("gold".to_string())),
        ]);
        assert_eq!(text_colour(&as_nbt).as_deref(), Some("#ffaa00"));
    }

    #[test]
    fn a_hex_literal_passes_through() {
        let component = compound(&[("color", Value::String("#1A2B3C".to_string()))]);
        // Lowercased, so the panel never has to compare case-insensitively.
        assert_eq!(text_colour(&component).as_deref(), Some("#1a2b3c"));
    }

    /// Anything that is not a colour must be dropped rather than passed to the
    /// panel, where it would end up in a style attribute.
    #[test]
    fn a_malformed_colour_is_refused() {
        for bogus in [
            "#12345",
            "#1234567",
            "#GGGGGG",
            "rebeccapurple",
            "red; background: url(x)",
            "",
        ] {
            let component = compound(&[("color", Value::String(bogus.to_string()))]);
            assert_eq!(text_colour(&component), None, "{bogus:?} must be refused");
        }
    }

    #[test]
    fn a_component_with_no_colour_has_none() {
        let component = compound(&[("text", Value::String("Excalibur".to_string()))]);
        assert_eq!(text_colour(&component), None);
    }

    #[test]
    fn an_item_carries_its_name_and_its_colour() {
        let item = map_of(&[
            ("id", Value::String("minecraft:diamond_sword".to_string())),
            ("count", Value::Int(1)),
            ("components", compound(&[(
                "minecraft:custom_name",
                compound(&[
                    ("text", Value::String("Excalibur".to_string())),
                    ("color", Value::String("gold".to_string())),
                ]),
            )])),
        ]);

        let parsed = read_item(&item, false).expect("the stack parses");
        assert_eq!(parsed.custom_name.as_deref(), Some("Excalibur"));
        assert_eq!(parsed.custom_name_color.as_deref(), Some("#ffaa00"));
    }

    /// A colour with no name to paint would be a value the panel has nowhere to
    /// put, so it is not read at all.
    #[test]
    fn a_colour_without_a_name_is_not_reported() {
        let item = map_of(&[
            ("id", Value::String("minecraft:stone".to_string())),
            ("count", Value::Int(1)),
        ]);

        let parsed = read_item(&item, false).expect("the stack parses");
        assert_eq!(parsed.custom_name, None);
        assert_eq!(parsed.custom_name_color, None);
    }

    /* ---------------------------------------------------- round-trip probe */

    /// Can a player file survive being parsed and written back?
    ///
    /// This decides whether editing an offline player is safe at all. Their
    /// file is full of tags this code knows nothing about -- mod attachments,
    /// attribute modifiers, advancement state -- and writing back a compound
    /// that dropped or retyped any of them would corrupt a character rather
    /// than heal one.
    #[test]
    fn probe_round_trip_fidelity() {
        let original = compound(&[
            ("Health", Value::Float(17.5)),
            ("foodLevel", Value::Int(12)),
            ("foodSaturationLevel", Value::Float(2.5)),
            ("foodExhaustionLevel", Value::Float(0.0)),
            ("XpP", Value::Float(0.375)),
            ("XpLevel", Value::Int(30)),
            ("Score", Value::Int(451)),
            ("playerGameType", Value::Int(0)),
            ("Dimension", Value::String("minecraft:the_nether".to_string())),
            ("Air", Value::Short(300)),
            ("OnGround", Value::Byte(1)),
            ("AbsorptionAmount", Value::Float(4.0)),
            ("UUID", Value::IntArray(fastnbt::IntArray::new(vec![1, -2, 3, -4]))),
            ("Pos", Value::List(vec![
                Value::Double(103.5),
                Value::Double(64.0),
                Value::Double(-89.25),
            ])),
            ("Motion", Value::List(vec![
                Value::Double(0.0),
                Value::Double(-0.0784),
                Value::Double(0.0),
            ])),
            ("Rotation", Value::List(vec![Value::Float(90.0), Value::Float(-12.5)])),
            // A long array, which NBT keeps distinct from a list of longs.
            ("Longs", Value::LongArray(fastnbt::LongArray::new(vec![i64::MIN, 0, i64::MAX]))),
            ("Bytes", Value::ByteArray(fastnbt::ByteArray::new(vec![-128, 0, 127]))),
            // What a mod's attachment looks like: nested compounds under a
            // namespaced key, holding types this code never inspects.
            ("neoforge:attachments", compound(&[
                ("create:heat", Value::Double(1234.5)),
                ("nested", compound(&[
                    ("deep", Value::List(vec![
                        compound(&[("id", Value::String("createbigcannons:ap_shell".to_string()))]),
                        compound(&[("count", Value::Byte(64))]),
                    ])),
                ])),
            ])),
            // An empty list and an empty compound: both legal, both easy to
            // lose or retype.
            ("EmptyList", Value::List(Vec::new())),
            ("EmptyCompound", Value::Compound(std::collections::HashMap::new())),
            ("Unicode", Value::String("Robighost01 \u{2014} caf\u{e9}".to_string())),
        ]);

        let bytes = fastnbt::to_bytes(&original).expect("the fixture serialises");
        let parsed: HashMap<String, Value> =
            fastnbt::from_bytes(&bytes).expect("and parses back");

        // The shape this code would actually hold and rewrite.
        let rewritten = fastnbt::to_bytes(&Value::Compound(parsed.clone()))
            .expect("a parsed compound re-serialises");
        let reparsed: HashMap<String, Value> =
            fastnbt::from_bytes(&rewritten).expect("and parses again");

        assert_eq!(parsed, reparsed, "a read/write cycle must lose nothing");

        // Spot-check the types most likely to be quietly widened or collapsed.
        assert!(matches!(reparsed.get("Health"), Some(Value::Float(_))));
        assert!(matches!(reparsed.get("Air"), Some(Value::Short(_))));
        assert!(matches!(reparsed.get("OnGround"), Some(Value::Byte(_))));
        assert!(matches!(reparsed.get("Bytes"), Some(Value::ByteArray(_))));
        assert!(matches!(reparsed.get("Longs"), Some(Value::LongArray(_))));
        assert!(matches!(reparsed.get("UUID"), Some(Value::IntArray(_))));
        assert!(matches!(reparsed.get("EmptyList"), Some(Value::List(_))));
        assert!(matches!(reparsed.get("EmptyCompound"), Some(Value::Compound(_))));
    }

    /* -------------------------------------------------------------- vitals */

    /// The header is only live because these are on the live path. If one is
    /// dropped, the bars silently go back to showing the last autosave.
    #[test]
    fn the_live_paths_cover_the_whole_header() {
        for field in [
            "Health",
            "foodLevel",
            "XpLevel",
            "XpP",
            "playerGameType",
            "Dimension",
            "Pos",
        ] {
            assert!(
                LIVE_PATHS.contains(&field),
                "{field} must be read live or the bars show stale data"
            );
        }
    }

    #[test]
    fn every_live_path_builds_a_command() {
        for path in LIVE_PATHS {
            assert_eq!(
                live_command("Steve", path),
                Some(format!("data get entity Steve {path}")),
                "{path} must build a command"
            );
        }
    }

    #[test]
    fn a_gap_is_filled_from_the_save_and_reported() {
        let mut live = Vitals {
            health: Some(20.0),
            ..Vitals::default()
        };
        let saved = Vitals {
            health: Some(3.0),
            food: Some(11),
            ..Vitals::default()
        };

        assert!(live.fill_gaps_from(&saved), "food came off disk");
        // The field that answered live is not overwritten by the stale one.
        assert_eq!(live.health, Some(20.0));
        assert_eq!(live.food, Some(11));
    }

    #[test]
    fn a_complete_live_header_borrows_nothing() {
        let mut live = Vitals {
            health: Some(20.0),
            food: Some(20),
            ..Vitals::default()
        };
        let saved = live.clone();

        assert!(!live.fill_gaps_from(&saved), "nothing was missing");
    }

    /// A field neither side has is not a borrow — otherwise a player with no
    /// score would make an otherwise-live header report itself as mixed.
    #[test]
    fn a_field_missing_from_both_is_not_a_borrow() {
        let mut live = Vitals::default();
        assert!(!live.fill_gaps_from(&Vitals::default()));
    }

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
