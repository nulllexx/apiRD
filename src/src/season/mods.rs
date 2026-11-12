//! What mods a season ran: names and shortened versions, read from the jars.
//!
//! Each jar's own metadata is preferred -- NeoForge/Forge `mods.toml`, Fabric
//! `fabric.mod.json`, then the manifest -- because file names are inconsistent
//! (`mekanism-1.21.1-10.7.7.64.jar`, `sodium-neoforge-0.6.0+mc1.21.1.jar`).
//! The file name is the fallback.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Serialize;

use super::archive::ModEntry;

/// Largest metadata file read out of a jar.
const MAX_METADATA: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ModInfo {
    pub name: String,
    /// Shortened, see [`short_version`].
    pub version: String,
    /// The jar, as it is in `mods/`.
    pub file: String,
}

impl From<&ModInfo> for ModEntry {
    fn from(m: &ModInfo) -> Self {
        ModEntry { name: m.name.clone(), version: m.version.clone() }
    }
}

/// Every `*.jar` directly in `dir` that is a regular file, read and sorted
/// by name. Disabled jars (`.jar.disabled`, `.bak`) are not running, so they
/// are not listed.
pub fn read_mods(dir: &Path, minecraft: Option<&str>) -> Vec<ModInfo> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut mods: Vec<ModInfo> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            if !file.to_ascii_lowercase().ends_with(".jar") {
                return None;
            }
            let stem = &file[..file.len() - 4];
            let (fallback_name, fallback_version) = from_file_name(stem);
            let (name, raw) = read_jar(&e.path()).unwrap_or((None, None));
            let raw = raw.filter(|v| !v.contains("${"));
            let version = raw.as_deref().unwrap_or(&fallback_version);
            Some(ModInfo {
                name: name.filter(|n| !n.trim().is_empty()).unwrap_or(fallback_name),
                version: short_version(version, minecraft),
                file,
            })
        })
        .collect();
    mods.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    mods
}

/// Name and raw version from a jar's metadata.
fn read_jar(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let mut jar = zip::ZipArchive::new(File::open(path).ok()?).ok()?;
    let mut read = |name: &str| -> Option<String> {
        let entry = jar.by_name(name).ok()?;
        let mut text = String::new();
        entry.take(MAX_METADATA).read_to_string(&mut text).ok()?;
        Some(text)
    };

    let manifest_version = read("META-INF/MANIFEST.MF").and_then(|m| manifest_value(&m, "Implementation-Version"));

    for toml in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        if let Some(text) = read(toml) {
            let (id, display, version) = first_mod_in_toml(&text);
            let version = match version {
                Some(v) if v.contains("${") => manifest_version.clone(),
                other => other,
            };
            return Some((display.or(id), version));
        }
    }
    if let Some(text) = read("fabric.mod.json") {
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let field = |k: &str| json.get(k).and_then(|v| v.as_str()).map(str::to_string);
        return Some((field("name").or_else(|| field("id")), field("version")));
    }
    let manifest = read("META-INF/MANIFEST.MF")?;
    Some((manifest_value(&manifest, "Implementation-Title"), manifest_version))
}

fn manifest_value(manifest: &str, key: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_string()).filter(|v| !v.is_empty())
    })
}

/// `modId`, `displayName` and `version` of the first `[[mods]]` entry.
fn first_mod_in_toml(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut id, mut display, mut version) = (None, None, None);
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[mods]]") {
            if inside {
                break;
            }
            inside = true;
            continue;
        }
        if line.starts_with('[') {
            if inside {
                break;
            }
            continue;
        }
        if !inside {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let Some(value) = toml_string(value.trim()) else { continue };
        match key.trim() {
            "modId" => id = Some(value),
            "displayName" => display = Some(value),
            "version" => version = Some(value),
            _ => {}
        }
    }
    (id, display, version)
}

/// A one-line TOML string value; multi-line ones are not names or versions.
fn toml_string(value: &str) -> Option<String> {
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    if value.starts_with("\"\"\"") || value.starts_with("'''") {
        return None;
    }
    let rest = &value[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// `mekanism-1.21.1-10.7.7.64` to (`mekanism`, `1.21.1-10.7.7.64`): the name
/// is everything before the first part that starts with a digit.
fn from_file_name(stem: &str) -> (String, String) {
    let parts: Vec<&str> = stem.split(['-', '_']).collect();
    let at = parts
        .iter()
        .position(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit()) || p.starts_with("mc1"))
        .unwrap_or(parts.len());
    let name = parts[..at.max(1)].join("-");
    (name, parts[at.min(parts.len())..].join("-"))
}

/// Words in version strings that say what a jar is built for, not which
/// version of the mod it is.
const LOADERS: &[&str] = &["neoforge", "forge", "fabric", "quilt", "universal", "all", "mc"];

/// Shorten a mod version to at most `major.minor.patch` of the mod itself.
///
/// Drops what is not the mod's version -- the Minecraft version
/// (`1.21.1-…`, `mc1.21.1`), loader names, build metadata after `+` -- and
/// keeps the first three numeric parts of what is left:
/// `1.21.1-10.7.7.64` is `10.7.7`, `0.6.0+mc1.21.1` is `0.6.0`. A version
/// that is only the Minecraft version is kept as such; one with no numbers
/// at all is kept as written.
pub fn short_version(raw: &str, minecraft: Option<&str>) -> String {
    let trimmed = raw.trim();
    let base = trimmed.split('+').next().unwrap_or(trimmed);
    let tokens: Vec<&str> = base.split(['-', '_', ' ']).filter(|t| !t.is_empty()).collect();

    let is_minecraft = |token: &str| {
        let t = token.strip_prefix("mc").or_else(|| token.strip_prefix("MC")).unwrap_or(token);
        match minecraft {
            Some(mc) => t == mc || mc.starts_with(&format!("{t}.")) && t.matches('.').count() >= 1,
            None => token.starts_with("mc1") || token.starts_with("MC1"),
        }
    };
    let numeric = |t: &str| t.trim_start_matches(['v', 'V']).chars().next().is_some_and(|c| c.is_ascii_digit());

    let chosen = tokens
        .iter()
        .find(|t| numeric(t) && !is_minecraft(t) && !LOADERS.contains(&t.to_ascii_lowercase().as_str()))
        .or_else(|| tokens.iter().find(|t| numeric(t)));

    let Some(token) = chosen else {
        let kept: String = trimmed.chars().take(40).collect();
        return if kept.is_empty() { "unknown".to_string() } else { kept };
    };
    let token = token.trim_start_matches(['v', 'V']);
    let parts: Vec<&str> = token
        .split('.')
        .map(|p| p.split(|c: char| !c.is_ascii_digit()).next().unwrap_or(""))
        .take_while(|p| !p.is_empty())
        .take(3)
        .collect();
    if parts.is_empty() {
        token.chars().take(40).collect()
    } else {
        parts.join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn versions_are_shortened_to_the_mods_own() {
        let mc = Some("1.21.1");
        for (raw, want) in [
            ("1.21.1-10.7.7.64", "10.7.7"),
            ("21.1.90", "21.1.90"),
            ("mc1.21.1-0.6.0-neoforge", "0.6.0"),
            ("0.6.0+mc1.21.1", "0.6.0"),
            ("v2.0.1-beta.3", "2.0.1"),
            ("neoforge-1.21.1-3.4.5", "3.4.5"),
            ("1.21-2.3", "2.3"),
            ("4.0.0-beta1", "4.0.0"),
            ("12.4", "12.4"),
            ("1.21.1", "1.21.1"),
            ("release", "release"),
            ("", "unknown"),
            ("1.2.3rc1", "1.2.3"),
        ] {
            assert_eq!(short_version(raw, mc), want, "{raw}");
        }
        assert_eq!(short_version("mc1.20.1-5.0", None), "5.0");
    }

    #[test]
    fn file_names_split_into_name_and_version() {
        assert_eq!(from_file_name("mekanism-1.21.1-10.7.7.64"), ("mekanism".into(), "1.21.1-10.7.7.64".into()));
        assert_eq!(from_file_name("sodium-neoforge-0.6.0+mc1.21.1"), ("sodium-neoforge".into(), "0.6.0+mc1.21.1".into()));
        assert_eq!(from_file_name("jei_1.21.1-neoforge-19.21"), ("jei".into(), "1.21.1-neoforge-19.21".into()));
        assert_eq!(from_file_name("noversion"), ("noversion".into(), "".into()));
    }

    #[test]
    fn toml_takes_the_first_mod_only() {
        let toml = r#"
modLoader="javafml"
[[mods]]
modId="mekanism" # comment
displayName='Mekanism'
version="${file.jarVersion}"
description='''
multi
'''
[[dependencies.mekanism]]
modId="neoforge"
[[mods]]
modId="other"
"#;
        assert_eq!(
            first_mod_in_toml(toml),
            (Some("mekanism".into()), Some("Mekanism".into()), Some("${file.jarVersion}".into()))
        );
    }

    fn jar(dir: &Path, name: &str, entries: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(File::create(dir.join(name)).unwrap());
        for (path, body) in entries {
            zip.start_file::<_, ()>(*path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn reads_neoforge_fabric_manifest_and_falls_back_to_the_name() {
        let dir = std::env::temp_dir().join(format!("apird-mods-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        jar(&dir, "mekanism-1.21.1-10.7.7.64.jar", &[
            ("META-INF/neoforge.mods.toml", "[[mods]]\nmodId=\"mekanism\"\ndisplayName=\"Mekanism\"\nversion=\"${file.jarVersion}\"\n"),
            ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\nImplementation-Version: 1.21.1-10.7.7.64\n"),
        ]);
        jar(&dir, "sodium.jar", &[("fabric.mod.json", r#"{"id":"sodium","name":"Sodium","version":"0.6.0+mc1.21.1"}"#)]);
        jar(&dir, "plain-lib-2.5.1.jar", &[("a.txt", "x")]);
        fs::write(dir.join("broken-3.0.jar"), b"not a zip").unwrap();
        fs::write(dir.join("old-1.0.jar.disabled"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::create_dir_all(dir.join("folder.jar")).unwrap();

        let got: Vec<(String, String)> =
            read_mods(&dir, Some("1.21.1")).into_iter().map(|m| (m.name, m.version)).collect();
        assert_eq!(
            got,
            [
                ("broken".to_string(), "3.0".to_string()),
                ("Mekanism".into(), "10.7.7".into()),
                ("plain-lib".into(), "2.5.1".into()),
                ("Sodium".into(), "0.6.0".into()),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
