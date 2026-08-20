//! Resolving an item id to its texture by reading the game's own model files.
//!
//! Every naming rule elsewhere in this module tree is a guess: that a stair is
//! drawn with planks, that a bed lives under `entity/`. The model files state
//! the answer outright, and they are what the game itself reads:
//!
//! ```json
//! // assets/minecraft/models/block/jungle_stairs.json
//! { "parent": "minecraft:block/stairs",
//!   "textures": { "side": "minecraft:block/jungle_planks", ... } }
//! ```
//!
//! Guessing cannot survive contact with mods. Create draws `create:shaft` with
//! a texture called `axis`, and no rule derived from the word "shaft" will ever
//! find it — but `models/item/shaft.json` says so in one line.
//!
//! ## Resolution
//!
//! Start at `models/item/<name>.json`, walk up the `parent` chain, and merge
//! each model's `textures` map with the child winning. Then pick a slot by
//! priority: `layer0` is the flat sprite a normal item is drawn from, `all` and
//! `side` are the faces of a block, and `particle` is the last resort because
//! it is only ever a colour hint — for a shield it is the plank texture of a
//! crafting ingredient.
//!
//! Values may point at another slot with a `#` reference, which is how a
//! template model defers to whatever its children fill in.
//!
//! This is the 1.21.1 layout. 1.21.4 moved item models to `assets/<ns>/items/`
//! and would need a second entry point; the parent-and-textures machinery below
//! is unchanged by that.

use std::collections::HashMap;

use serde::Deserialize;

/// Longest `parent` chain followed. Vanilla's deepest is about four; this stops
/// a hand-edited or circular model from looping forever.
pub const MAX_PARENT_DEPTH: usize = 12;

/// How many `#reference` hops are followed before giving up.
const MAX_REFERENCE_DEPTH: usize = 8;

/// Texture slots in the order they best represent an item in a grid.
///
/// `layer0` first because a flat item is exactly its sprite. Then the block
/// faces, most representative first — `all` covers a uniform cube, `side` is
/// what you see of most blocks. `particle` is last and deliberately so: models
/// with no other slot are the built-in-rendered ones, where the particle
/// texture is a loose association rather than the item's appearance.
const SLOT_PRIORITY: &[&str] = &[
    "layer0", "all", "side", "texture", "north", "end", "top", "front", "cross", "particle",
];

#[derive(Debug, Default, Deserialize)]
pub struct Model {
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub textures: HashMap<String, String>,
}

/// Parse one model file, tolerating the fields this does not care about.
pub fn parse_model(json: &str) -> Option<Model> {
    serde_json::from_str(json).ok()
}

/// Split `minecraft:block/stone` into its namespace and path.
///
/// An unqualified location is vanilla, exactly as the game reads it.
pub fn split_location(location: &str) -> (String, String) {
    match location.split_once(':') {
        Some((namespace, path)) => (namespace.to_ascii_lowercase(), path.to_string()),
        None => ("minecraft".to_string(), location.to_string()),
    }
}

/// The jar-relative path of a model, given its resource location.
pub fn model_asset_path(location: &str) -> (String, String) {
    let (namespace, path) = split_location(location);
    (namespace, format!("models/{path}.json"))
}

/// The namespace and `textures/`-relative filename a texture location names.
///
/// Returned in the same shape the texture lookup already uses for its guesses,
/// so a resolved answer slots in beside them.
pub fn texture_candidate(location: &str) -> (String, String) {
    let (namespace, path) = split_location(location);
    (namespace, format!("{path}.png"))
}

/// Fold a model's textures into the accumulated map, child winning.
///
/// The walk runs from the item model upwards, so a slot already present came
/// from a more specific model and must not be overwritten by its parent.
pub fn merge_textures(into: &mut HashMap<String, String>, from: HashMap<String, String>) {
    for (slot, value) in from {
        into.entry(slot).or_insert(value);
    }
}

/// Choose the texture that best stands for this item, following `#references`.
pub fn pick_texture(textures: &HashMap<String, String>) -> Option<String> {
    let ordered = SLOT_PRIORITY
        .iter()
        .map(|slot| (*slot).to_string())
        // Anything unrecognised is still better than nothing; sorted so the
        // choice does not change between two identical requests.
        .chain({
            let mut rest: Vec<String> = textures
                .keys()
                .filter(|slot| !SLOT_PRIORITY.contains(&slot.as_str()))
                .cloned()
                .collect();
            rest.sort();
            rest
        });

    for slot in ordered {
        if let Some(resolved) = resolve_slot(textures, &slot) {
            return Some(resolved);
        }
    }

    None
}

/// Read one slot, following `#other_slot` indirection to a real location.
fn resolve_slot(textures: &HashMap<String, String>, slot: &str) -> Option<String> {
    let mut current = textures.get(slot)?.as_str();

    for _ in 0..MAX_REFERENCE_DEPTH {
        match current.strip_prefix('#') {
            // A reference that goes nowhere is a template slot nobody filled
            // in, which is a miss rather than an error.
            Some(next) => current = textures.get(next)?.as_str(),
            None => {
                return (!current.is_empty()).then(|| current.to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn textures(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /* ------------------------------------------------------------- parsing */

    #[test]
    fn reads_a_parent_and_textures() {
        let model = parse_model(
            r#"{"parent":"minecraft:block/stairs","textures":{"side":"minecraft:block/jungle_planks"}}"#,
        )
        .unwrap();

        assert_eq!(model.parent.as_deref(), Some("minecraft:block/stairs"));
        assert_eq!(
            model.textures.get("side").map(String::as_str),
            Some("minecraft:block/jungle_planks")
        );
    }

    #[test]
    fn ignores_the_fields_this_does_not_need() {
        // Real models are mostly display transforms and geometry.
        let model = parse_model(
            r#"{"parent":"item/generated","display":{"gui":{"scale":[1,1,1]}},
                "elements":[{"from":[0,0,0],"to":[16,16,16]}],
                "textures":{"layer0":"item/stick"}}"#,
        )
        .unwrap();

        assert_eq!(model.textures.len(), 1);
    }

    #[test]
    fn a_model_with_neither_field_is_still_a_model() {
        // template_bed is exactly this: display transforms and nothing else.
        let model = parse_model(r#"{"display":{}}"#).unwrap();
        assert!(model.parent.is_none());
        assert!(model.textures.is_empty());
    }

    #[test]
    fn malformed_json_is_not_a_model() {
        assert!(parse_model("{ truncated").is_none());
        assert!(parse_model("").is_none());
    }

    #[test]
    fn a_json_array_yields_nothing_to_resolve() {
        // serde accepts a sequence for a struct whose fields all have defaults,
        // so this parses rather than failing. What matters is that it cannot
        // produce a texture, leaving the caller to fall through to its guesses.
        let model = parse_model("[]").expect("parses as an empty model");
        assert!(model.parent.is_none() && model.textures.is_empty());
        assert_eq!(pick_texture(&model.textures), None);
    }

    /* ------------------------------------------------------------ locations */

    #[test]
    fn splits_qualified_and_bare_locations() {
        assert_eq!(
            split_location("create:block/axis"),
            ("create".to_string(), "block/axis".to_string())
        );
        // Bare locations are vanilla, as the game reads them.
        assert_eq!(
            split_location("block/stone"),
            ("minecraft".to_string(), "block/stone".to_string())
        );
    }

    #[test]
    fn builds_asset_paths_from_locations() {
        assert_eq!(
            model_asset_path("create:item/shaft"),
            ("create".to_string(), "models/item/shaft.json".to_string())
        );
        assert_eq!(
            texture_candidate("create:block/axis"),
            ("create".to_string(), "block/axis.png".to_string())
        );
        assert_eq!(
            texture_candidate("block/jungle_planks"),
            ("minecraft".to_string(), "block/jungle_planks.png".to_string())
        );
    }

    /* -------------------------------------------------------------- merging */

    #[test]
    fn a_child_slot_wins_over_its_parent() {
        // The walk goes upward, so whatever is already there is more specific.
        let mut merged = textures(&[("side", "create:block/axis")]);
        merge_textures(
            &mut merged,
            textures(&[("side", "minecraft:block/stone"), ("top", "block/oak")]),
        );

        assert_eq!(merged.get("side").unwrap(), "create:block/axis");
        assert_eq!(merged.get("top").unwrap(), "block/oak");
    }

    /* ------------------------------------------------------------- picking */

    #[test]
    fn a_flat_item_is_its_layer0_sprite() {
        let picked = pick_texture(&textures(&[
            ("layer0", "minecraft:item/diamond_sword"),
            ("particle", "minecraft:block/stone"),
        ]));
        assert_eq!(picked.as_deref(), Some("minecraft:item/diamond_sword"));
    }

    #[test]
    fn a_block_falls_to_its_faces_in_a_sensible_order() {
        assert_eq!(
            pick_texture(&textures(&[("all", "block/stone")])).as_deref(),
            Some("block/stone")
        );
        // Stairs carry bottom/side/top; side is the representative face.
        assert_eq!(
            pick_texture(&textures(&[
                ("bottom", "block/jungle_planks"),
                ("side", "block/jungle_planks"),
                ("top", "block/jungle_planks"),
            ]))
            .as_deref(),
            Some("block/jungle_planks")
        );
    }

    #[test]
    fn particle_is_the_last_resort() {
        // A shield's model carries nothing but a particle texture, and that
        // texture is dark oak planks - a crafting association, not the item.
        // Anything else present must beat it.
        let picked = pick_texture(&textures(&[
            ("particle", "block/dark_oak_planks"),
            ("side", "block/iron_block"),
        ]));
        assert_eq!(picked.as_deref(), Some("block/iron_block"));

        // Alone, it is still better than giving up.
        assert_eq!(
            pick_texture(&textures(&[("particle", "block/dark_oak_planks")])).as_deref(),
            Some("block/dark_oak_planks")
        );
    }

    #[test]
    fn an_unrecognised_slot_is_used_rather_than_nothing() {
        // Mods invent slot names freely.
        let picked = pick_texture(&textures(&[("gearbox", "create:block/gearbox")]));
        assert_eq!(picked.as_deref(), Some("create:block/gearbox"));
    }

    #[test]
    fn an_unrecognised_slot_is_chosen_deterministically() {
        // Two requests for an unchanged item must not disagree, which a hash
        // map's iteration order would guarantee they eventually do.
        let map = textures(&[("zeta", "create:z"), ("alpha", "create:a")]);
        assert_eq!(pick_texture(&map).as_deref(), Some("create:a"));
        assert_eq!(pick_texture(&map).as_deref(), Some("create:a"));
    }

    #[test]
    fn a_reference_is_followed_to_a_real_location() {
        // How template models defer to whatever a child fills in.
        let picked = pick_texture(&textures(&[
            ("all", "#texture"),
            ("texture", "create:block/andesite_casing"),
        ]));
        assert_eq!(picked.as_deref(), Some("create:block/andesite_casing"));
    }

    #[test]
    fn a_dangling_reference_falls_through_to_the_next_slot() {
        // An unfilled template slot is a miss for that slot, not for the model.
        let picked = pick_texture(&textures(&[
            ("all", "#nobody_filled_this_in"),
            ("side", "block/stone"),
        ]));
        assert_eq!(picked.as_deref(), Some("block/stone"));
    }

    #[test]
    fn a_circular_reference_terminates() {
        let picked = pick_texture(&textures(&[("all", "#side"), ("side", "#all")]));
        assert_eq!(picked, None);
    }

    #[test]
    fn nothing_to_pick_is_none() {
        assert_eq!(pick_texture(&HashMap::new()), None);
        assert_eq!(pick_texture(&textures(&[("layer0", "")])), None);
    }
}
