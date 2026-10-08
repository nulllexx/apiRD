use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use fastnbt::Value;
use serde::Serialize;
use serde_json::Value as Json;

use super::inventory::{
    decompress, place, read_item, InventoryError, PlayerSnapshot, ENDER_SLOTS, MAX_COMPRESSED,
};
use super::players::server_dir;
use super::strip_formatting;

/// The plugin's data folder, relative to the server directory
const PLUGIN_DIR: &str = "plugins/Multiverse-Inventories";

/// Ceiling on world and group folders walked for one listing.
const MAX_CONTAINERS: usize = 500;

/// Profiles in the order the picker offers them; anything else sorts after.
const PROFILE_ORDER: [&str; 4] = ["SURVIVAL", "ADVENTURE", "CREATIVE", "SPECTATOR"];

/// Keys that mark a JSON object as an inventory profile.
const PROFILE_KEYS: [&str; 5] = [
    "inventoryContents",
    "armorContents",
    "enderChestContents",
    "offHandItem",
    "stats",
];

/// Whether a stored inventory belongs to a single world or a group of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerKind {
    World,
    Group,
}

impl ContainerKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "world" => Some(ContainerKind::World),
            "group" => Some(ContainerKind::Group),
            _ => None,
        }
    }

    fn folder(self) -> &'static str {
        match self {
            ContainerKind::World => "worlds",
            ContainerKind::Group => "groups",
        }
    }
}

/// One world or group the player has a stored inventory in.
#[derive(Debug, Clone, Serialize)]
pub struct Container {
    pub name: String,
    pub kind: ContainerKind,
    #[serde(rename = "savedAt", skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<String>,
}

/// A stored inventory, plus which profile of the file it came from.
pub struct StoredInventory {
    pub snapshot: PlayerSnapshot,
    pub saved_at: Option<String>,
    pub profile: String,
    pub profiles: Vec<String>,
}

/* ------------------------------------------------------------------- paths */

fn plugin_dir(server_properties_path: &str) -> PathBuf {
    server_dir(server_properties_path).join(PLUGIN_DIR)
}

/// Whether a world or group name is safe to use as a path segment.
///
/// World names come back from the panel, so this is the traversal guard: a
/// name is a single plain segment or it is refused.
pub fn is_valid_container(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && !raw.starts_with('.')
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
}

/// The files that could hold this player's data inside one container.
fn player_files(dir: &Path, uuid: &str, name: Option<&str>) -> Vec<PathBuf> {
    let mut files = vec![dir.join(format!("{uuid}.json"))];
    if let Some(name) = name {
        files.push(dir.join(format!("{name}.json")));
    }
    files
}

fn rfc3339(modified: std::time::SystemTime) -> String {
    let stamp: chrono::DateTime<chrono::Utc> = modified.into();
    stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The most recently written of the candidate files, with its mtime.
///
/// A server that switched from name-keyed to UUID-keyed files can have both,
/// and the newer one is the one the plugin is still writing.
async fn newest_file(
    dir: &Path,
    uuid: &str,
    name: Option<&str>,
) -> Option<(PathBuf, std::fs::Metadata)> {
    let mut best: Option<(PathBuf, std::fs::Metadata)> = None;

    for path in player_files(dir, uuid, name) {
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let newer = match &best {
            Some((_, current)) => metadata.modified().ok() > current.modified().ok(),
            None => true,
        };
        if newer {
            best = Some((path, metadata));
        }
    }

    best
}

/// Every world and group holding a stored inventory for this player.
///
/// An empty list covers both "the plugin is not installed" and "this player
/// has never left a world behind"; either way there is nothing to pick.
pub async fn list_containers(
    server_properties_path: &str,
    uuid: &str,
    name: Option<&str>,
) -> Vec<Container> {
    let root = plugin_dir(server_properties_path);
    let mut found = Vec::new();

    for kind in [ContainerKind::World, ContainerKind::Group] {
        let Ok(mut entries) = tokio::fs::read_dir(root.join(kind.folder())).await else {
            continue;
        };

        let mut walked = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            walked += 1;
            if walked > MAX_CONTAINERS {
                break;
            }

            let Some(container) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_valid_container(&container) {
                continue;
            }

            if let Some((_, metadata)) = newest_file(&entry.path(), uuid, name).await {
                found.push(Container {
                    name: container,
                    kind,
                    saved_at: metadata.modified().ok().map(rfc3339),
                });
            }
        }
    }

    found.sort_by(|a, b| {
        (a.kind == ContainerKind::Group, a.name.to_ascii_lowercase())
            .cmp(&(b.kind == ContainerKind::Group, b.name.to_ascii_lowercase()))
    });
    found
}

/// Read one player's stored inventory for one world or group.
pub async fn load(
    server_properties_path: &str,
    kind: ContainerKind,
    container: &str,
    uuid: &str,
    name: Option<&str>,
    profile: Option<&str>,
    mod_namespaces: &[String],
) -> Result<StoredInventory, InventoryError> {
    if !is_valid_container(container) {
        return Err(InventoryError::Missing);
    }

    let dir = plugin_dir(server_properties_path)
        .join(kind.folder())
        .join(container);
    let (path, metadata) = newest_file(&dir, uuid, name)
        .await
        .ok_or(InventoryError::Missing)?;

    if metadata.len() > MAX_COMPRESSED {
        return Err(InventoryError::Unreadable(
            "stored inventory file is implausibly large".to_string(),
        ));
    }

    let raw = tokio::fs::read(&path)
        .await
        .map_err(|e| InventoryError::Unreadable(format!("cannot read: {e}")))?;
    let json: Json = serde_json::from_slice(&raw)
        .map_err(|e| InventoryError::Unreadable(format!("not valid JSON: {e}")))?;

    let resolver = Resolver::new(mod_namespaces);
    let (snapshot, profile, profiles) = parse(&json, profile, &resolver)?;

    Ok(StoredInventory {
        snapshot,
        saved_at: metadata.modified().ok().map(rfc3339),
        profile,
        profiles,
    })
}

/* ----------------------------------------------------------------- parsing */

/// A value that may have been stored as an object or as a JSON string of one.
///
/// Older plugin versions wrote `stats` and `lastLocation` as strings holding
/// JSON, newer ones as plain objects.
fn object_of(value: &Json) -> Option<serde_json::Map<String, Json>> {
    match value {
        Json::Object(map) => Some(map.clone()),
        Json::String(raw) => match serde_json::from_str(raw) {
            Ok(Json::Object(map)) => Some(map),
            _ => None,
        },
        _ => None,
    }
}

/// A number that may have been stored as a string.
fn number(value: &Json) -> Option<f64> {
    match value {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
}

fn first_number(maps: &[&serde_json::Map<String, Json>], keys: &[&str]) -> Option<f64> {
    maps.iter()
        .find_map(|map| keys.iter().find_map(|key| map.get(*key).and_then(number)))
}

fn is_profile(value: &Json) -> bool {
    value
        .as_object()
        .is_some_and(|map| PROFILE_KEYS.iter().any(|key| map.contains_key(*key)))
}

/// Pick the profile to show out of a player file.
///
/// Returns the profile's object, its name, and every profile name the file
/// has, in picker order.
fn choose_profile<'a>(
    root: &'a Json,
    wanted: Option<&str>,
) -> Option<(&'a serde_json::Map<String, Json>, String, Vec<String>)> {
    let map = root.as_object()?;

    let mut names: Vec<&String> = map
        .iter()
        .filter(|(_, value)| is_profile(value))
        .map(|(key, _)| key)
        .collect();

    if names.is_empty() {
        // A file that is a single profile with no game-mode wrapper.
        return is_profile(root).then(|| (map, "DEFAULT".to_string(), vec!["DEFAULT".to_string()]));
    }

    let rank = |name: &str| {
        PROFILE_ORDER
            .iter()
            .position(|known| known.eq_ignore_ascii_case(name))
            .unwrap_or(PROFILE_ORDER.len())
    };
    names.sort_by(|a, b| (rank(a), a.as_str()).cmp(&(rank(b), b.as_str())));

    let chosen = wanted
        .and_then(|wanted| names.iter().find(|name| name.eq_ignore_ascii_case(wanted)))
        .unwrap_or(&names[0]);

    Some((
        map.get(*chosen)?.as_object()?,
        (*chosen).clone(),
        names.iter().map(|name| (*name).clone()).collect(),
    ))
}

/// Turns Bukkit's flattened names back into namespaced ids.
///
/// Bukkit names a material `DIAMOND_SWORD`, and a hybrid server names a modded
/// one `MODID_ITEM_NAME` — the namespace and the path squashed together with an
/// underscore. Splitting that back apart needs to know which namespaces exist,
/// which is what the installed mods tell us. Anything that does not start with
/// one of them is vanilla.
pub struct Resolver {
    /// (flattened prefix, real namespace), longest prefix first so that `foo_bar`
    /// wins over `foo` for an item named `FOO_BAR_THING`.
    namespaces: Vec<(String, String)>,
}

impl Resolver {
    pub fn new(mod_namespaces: &[String]) -> Self {
        let mut namespaces: Vec<(String, String)> = mod_namespaces
            .iter()
            .filter(|ns| ns.as_str() != "minecraft" && !ns.is_empty())
            .map(|ns| {
                let flat: String = ns
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                (format!("{flat}_"), ns.clone())
            })
            .collect();
        namespaces.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Resolver { namespaces }
    }

    pub fn id(&self, raw: &str) -> String {
        let lower = raw.trim().to_ascii_lowercase();
        if lower.contains(':') {
            return lower;
        }

        for (prefix, namespace) in &self.namespaces {
            if let Some(rest) = lower.strip_prefix(prefix.as_str()) {
                if !rest.is_empty() {
                    return format!("{namespace}:{rest}");
                }
            }
        }

        format!("minecraft:{lower}")
    }
}

/// Bukkit's pre-1.20.5 enchantment names that do not match the vanilla id.
const LEGACY_ENCHANTMENTS: [(&str, &str); 19] = [
    ("PROTECTION_ENVIRONMENTAL", "protection"),
    ("PROTECTION_FIRE", "fire_protection"),
    ("PROTECTION_FALL", "feather_falling"),
    ("PROTECTION_EXPLOSIONS", "blast_protection"),
    ("PROTECTION_PROJECTILE", "projectile_protection"),
    ("OXYGEN", "respiration"),
    ("WATER_WORKER", "aqua_affinity"),
    ("DAMAGE_ALL", "sharpness"),
    ("DAMAGE_UNDEAD", "smite"),
    ("DAMAGE_ARTHROPODS", "bane_of_arthropods"),
    ("LOOT_BONUS_MOBS", "looting"),
    ("DIG_SPEED", "efficiency"),
    ("DURABILITY", "unbreaking"),
    ("LOOT_BONUS_BLOCKS", "fortune"),
    ("ARROW_DAMAGE", "power"),
    ("ARROW_KNOCKBACK", "punch"),
    ("ARROW_FIRE", "flame"),
    ("ARROW_INFINITE", "infinity"),
    ("LUCK", "luck_of_the_sea"),
];

fn enchantment_id(raw: &str, resolver: &Resolver) -> String {
    LEGACY_ENCHANTMENTS
        .iter()
        .find(|(legacy, _)| *legacy == raw.trim())
        .map(|(_, id)| format!("minecraft:{id}"))
        .unwrap_or_else(|| resolver.id(raw))
}

fn enchantment_component(value: &Json, resolver: &Resolver) -> Option<Value> {
    let map = value.as_object()?;
    let levels: HashMap<String, Value> = map
        .iter()
        .filter_map(|(key, level)| {
            let level = number(level)? as i32;
            Some((enchantment_id(key, resolver), Value::Int(level)))
        })
        .collect();

    (!levels.is_empty()).then(|| {
        Value::Compound(HashMap::from([(
            "levels".to_string(),
            Value::Compound(levels),
        )]))
    })
}

/// A display name as the item parser expects it.
///
/// Recent Bukkit writes a JSON text component; older versions wrote the legacy
/// string with `§` colour codes, which would otherwise show up as garbage.
fn name_component(raw: &str) -> Value {
    if serde_json::from_str::<Json>(raw).is_ok() {
        Value::String(raw.to_string())
    } else {
        Value::String(strip_formatting(raw))
    }
}

/// Decode a stack stored as base64 NBT, which is how Paper's byte
/// serialisation writes it.
fn item_from_bytes(encoded: &str) -> Option<HashMap<String, Value>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let nbt = decompress(&bytes).ok()?;
    fastnbt::from_bytes(&nbt).ok()
}

/// Convert one stored stack into the compound [`read_item`] reads.
///
/// `depth` bounds bundle recursion, mirroring the vanilla reader's one level.
fn item_compound(value: &Json, resolver: &Resolver, depth: u8) -> Option<HashMap<String, Value>> {
    let map = match value {
        Json::String(encoded) => return item_from_bytes(encoded),
        Json::Object(map) => map,
        _ => return None,
    };

    let raw_type = map
        .get("type")
        .or_else(|| map.get("id"))
        .or_else(|| map.get("material"))
        .and_then(Json::as_str)?;
    let id = resolver.id(raw_type);
    if id == "minecraft:air" {
        return None;
    }

    let count = map
        .get("amount")
        .or_else(|| map.get("count"))
        .and_then(number)
        .map(|n| n as i32)
        .unwrap_or(1);

    let mut components: HashMap<String, Value> = HashMap::new();

    if let Some(meta) = map.get("meta").and_then(Json::as_object) {
        if let Some(name) = meta
            .get("display-name")
            .or_else(|| meta.get("item-name"))
            .and_then(Json::as_str)
        {
            components.insert("minecraft:custom_name".to_string(), name_component(name));
        }

        if let Some(damage) = meta.get("Damage").and_then(number) {
            components.insert("minecraft:damage".to_string(), Value::Int(damage as i32));
        }

        for (key, component) in [
            ("enchants", "minecraft:enchantments"),
            ("stored-enchants", "minecraft:stored_enchantments"),
        ] {
            if let Some(levels) = meta.get(key).and_then(|v| enchantment_component(v, resolver)) {
                components.insert(component.to_string(), levels);
            }
        }

        if depth == 0 {
            if let Some(items) = meta.get("items").and_then(Json::as_array) {
                let inner: Vec<Value> = items
                    .iter()
                    .filter_map(|item| item_compound(item, resolver, depth + 1))
                    .map(Value::Compound)
                    .collect();
                if !inner.is_empty() {
                    components.insert("minecraft:bundle_contents".to_string(), Value::List(inner));
                }
            }
        }
    }

    let mut compound = HashMap::from([
        ("id".to_string(), Value::String(id)),
        ("count".to_string(), Value::Int(count)),
    ]);
    if !components.is_empty() {
        compound.insert("components".to_string(), Value::Compound(components));
    }
    Some(compound)
}

/// Slot-numbered stacks out of either a `{"0": item}` map or a plain array.
///
/// Some plugin versions double-encode a whole container as a JSON string, so
/// that is unwrapped too. Owned values, because the unwrapped form has nothing
/// to borrow from; a player's inventory is small enough not to mind the copy.
fn slots(value: Option<&Json>) -> Vec<(i32, Json)> {
    match value {
        Some(Json::Object(map)) => map
            .iter()
            .filter_map(|(key, item)| Some((key.trim().parse().ok()?, item.clone())))
            .collect(),
        Some(Json::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| (index as i32, item.clone()))
            .collect(),
        Some(Json::String(raw)) => match serde_json::from_str::<Json>(raw) {
            Ok(parsed @ (Json::Object(_) | Json::Array(_))) => slots(Some(&parsed)),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn stack(value: &Json, resolver: &Resolver) -> Option<super::inventory::Item> {
    item_compound(value, resolver, 0).and_then(|compound| read_item(&compound, true))
}

/// Parse a player file into a snapshot of one of its profiles.
pub fn parse(
    root: &Json,
    wanted: Option<&str>,
    resolver: &Resolver,
) -> Result<(PlayerSnapshot, String, Vec<String>), InventoryError> {
    let (profile, name, names) = choose_profile(root, wanted).ok_or_else(|| {
        InventoryError::Unreadable("no inventory profile in this file".to_string())
    })?;

    let mut snapshot = PlayerSnapshot::empty();

    // Bukkit's full inventory array: 0-35 the grid, 36-39 armour from the feet
    // up, 40 the offhand. Armour and offhand are usually stored separately as
    // well, and those are read after so they win.
    for (slot, value) in slots(profile.get("inventoryContents")) {
        let Some(item) = stack(&value, resolver) else { continue };
        match slot {
            0..=35 => place(&mut snapshot, slot, item),
            36..=39 => place(&mut snapshot, 100 + (slot - 36), item),
            40 => snapshot.offhand = Some(item),
            _ => {}
        }
    }

    for (slot, value) in slots(profile.get("armorContents")) {
        if (0..4).contains(&slot) {
            if let Some(item) = stack(&value, resolver) {
                place(&mut snapshot, 100 + slot, item);
            }
        }
    }

    if let Some(item) = profile.get("offHandItem").and_then(|v| stack(v, resolver)) {
        snapshot.offhand = Some(item);
    }

    for (slot, value) in slots(profile.get("enderChestContents")) {
        if (0..ENDER_SLOTS as i32).contains(&slot) {
            if let Some(item) = stack(&value, resolver) {
                snapshot.ender_chest[slot as usize] = Some(item);
            }
        }
    }

    let stats = profile.get("stats").and_then(object_of).unwrap_or_default();
    let sources = [&stats, profile];
    snapshot.vitals.health = first_number(&sources, &["hp", "health"]).map(|v| v as f32);
    snapshot.vitals.food = first_number(&sources, &["fl", "foodLevel", "food"]).map(|v| v as i32);
    snapshot.vitals.xp_level = first_number(&sources, &["el", "level", "xpLevel"]).map(|v| v as i32);
    snapshot.vitals.xp_progress = first_number(&sources, &["xp", "exp"]).map(|v| v as f32);

    if let Some(location) = profile.get("lastLocation").and_then(object_of) {
        snapshot.vitals.dimension = location
            .get("world")
            .and_then(Json::as_str)
            .map(str::to_string);
        let coord = |key: &str| location.get(key).and_then(number).map(|v| v.floor() as i64);
        if let (Some(x), Some(y), Some(z)) = (coord("x"), coord("y"), coord("z")) {
            snapshot.vitals.position = Some([x, y, z]);
        }
    }

    Ok((snapshot, name, names))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolver() -> Resolver {
        Resolver::new(&["mekanism".to_string(), "create".to_string()])
    }

    #[test]
    fn container_names_cannot_escape_the_plugin_folder() {
        assert!(is_valid_container("world_nether"));
        assert!(is_valid_container("Survival-2"));
        assert!(!is_valid_container(".."));
        assert!(!is_valid_container("../plugins"));
        assert!(!is_valid_container("a/b"));
        assert!(!is_valid_container("a\\b"));
        assert!(!is_valid_container(""));
    }

    #[test]
    fn materials_resolve_to_namespaced_ids() {
        let r = resolver();
        assert_eq!(r.id("DIAMOND_SWORD"), "minecraft:diamond_sword");
        assert_eq!(r.id("MEKANISM_ATOMIC_DISASSEMBLER"), "mekanism:atomic_disassembler");
        assert_eq!(r.id("create:wrench"), "create:wrench");
        // The namespace alone is not an item.
        assert_eq!(r.id("CREATE_"), "minecraft:create_");
    }

    #[test]
    fn reads_a_bukkit_serialised_profile() {
        let file = json!({
            "SURVIVAL": {
                "inventoryContents": {
                    "0": { "==": "org.bukkit.inventory.ItemStack", "type": "DIAMOND_SWORD",
                           "meta": { "==": "ItemMeta", "display-name": "{\"text\":\"Blade\"}",
                                     "enchants": { "DAMAGE_ALL": 5, "minecraft:mending": 1 },
                                     "Damage": 12 } },
                    "10": { "type": "COBBLESTONE", "amount": 64 },
                    "39": { "type": "IRON_HELMET" }
                },
                "armorContents": { "0": { "type": "DIAMOND_BOOTS" } },
                "enderChestContents": { "3": { "type": "ENCHANTED_BOOK",
                    "meta": { "stored-enchants": { "THORNS": "3" } } } },
                "offHandItem": { "type": "SHIELD" },
                "stats": "{\"hp\":\"14.5\",\"fl\":\"18\",\"el\":\"30\",\"xp\":\"0.25\"}",
                "lastLocation": { "world": "world_nether", "x": 10.7, "y": 64.0, "z": -3.2 }
            },
            "CREATIVE": { "inventoryContents": {} }
        });

        let (snapshot, profile, profiles) = parse(&file, None, &resolver()).unwrap();
        assert_eq!(profile, "SURVIVAL");
        assert_eq!(profiles, vec!["SURVIVAL", "CREATIVE"]);

        let sword = snapshot.hotbar[0].as_ref().unwrap();
        assert_eq!(sword.id, "minecraft:diamond_sword");
        assert_eq!(sword.custom_name.as_deref(), Some("Blade"));
        assert_eq!(sword.damage, Some(12));
        let ids: Vec<_> = sword.enchantments.iter().map(|e| (e.id.as_str(), e.level)).collect();
        assert_eq!(ids, vec![("minecraft:mending", 1), ("minecraft:sharpness", 5)]);

        assert_eq!(snapshot.main[1].as_ref().unwrap().count, 64);
        assert_eq!(snapshot.armour[0].as_ref().unwrap().id, "minecraft:iron_helmet");
        assert_eq!(snapshot.armour[3].as_ref().unwrap().id, "minecraft:diamond_boots");
        assert_eq!(snapshot.offhand.as_ref().unwrap().id, "minecraft:shield");

        let book = snapshot.ender_chest[3].as_ref().unwrap();
        assert!(book.enchantments[0].stored);
        assert_eq!(book.enchantments[0].id, "minecraft:thorns");

        assert_eq!(snapshot.vitals.health, Some(14.5));
        assert_eq!(snapshot.vitals.food, Some(18));
        assert_eq!(snapshot.vitals.xp_level, Some(30));
        assert_eq!(snapshot.vitals.dimension.as_deref(), Some("world_nether"));
        assert_eq!(snapshot.vitals.position, Some([10, 64, -4]));
    }

    #[test]
    fn a_requested_profile_is_honoured_case_insensitively() {
        let file = json!({
            "SURVIVAL": { "inventoryContents": {} },
            "CREATIVE": { "inventoryContents": [ { "type": "BEDROCK" } ] }
        });
        let (snapshot, profile, _) = parse(&file, Some("creative"), &resolver()).unwrap();
        assert_eq!(profile, "CREATIVE");
        assert_eq!(snapshot.hotbar[0].as_ref().unwrap().id, "minecraft:bedrock");
    }

    #[test]
    fn legacy_colour_codes_are_stripped_from_names() {
        let file = json!({ "SURVIVAL": { "inventoryContents": {
            "0": { "type": "STICK", "meta": { "display-name": "\u{a7}cHot Stick" } }
        } } });
        let (snapshot, _, _) = parse(&file, None, &resolver()).unwrap();
        assert_eq!(snapshot.hotbar[0].as_ref().unwrap().custom_name.as_deref(), Some("Hot Stick"));
    }

    #[test]
    fn a_file_with_no_profile_is_unreadable_not_empty() {
        assert!(parse(&json!({ "playerData": {} }), None, &resolver()).is_err());
    }
}
