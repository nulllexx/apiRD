use actix_web::{web, HttpRequest};
use chrono::Utc;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Seek as _, Write as _};

use crate::error::AppError;
use crate::middleware::rate_limit::RateLimiter;

pub(crate) fn check_rate_limit(
    req: &HttpRequest,
    limiter: &web::Data<RateLimiter>,
) -> Result<(), AppError> {
    let ip = get_rate_limiter_ip(req);
    limiter.check(ip)
}

pub(crate) fn get_rate_limiter_ip(req: &HttpRequest) -> std::net::IpAddr {
    req.peer_addr()
        .map(|a| a.ip())
        .unwrap_or_else(|| "127.0.0.1".parse().unwrap())
}

/// Read, update, and write the authedPlayers.json file with advisory locking.
pub(crate) fn update_authed_players_file<F>(path: &str, update_fn: F)
where
    F: FnOnce(&mut Vec<serde_json::Value>),
{
    use std::fs::OpenOptions;

    let file_result = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path);

    let file = match file_result {
        Ok(f) => f,
        Err(e) => {
            log::error!("Error opening authedPlayers file: {}", e);
            return;
        }
    };

    if let Err(e) = fs2::FileExt::lock_exclusive(&file) {
        log::error!("File lock error: {}", e);
        return;
    }

    let mut contents = String::new();
    let mut reader = std::io::BufReader::new(&file);
    if let Err(e) = reader.read_to_string(&mut contents) {
        log::error!("Error reading authedPlayers file: {}", e);
        let _ = fs2::FileExt::unlock(&file);
        return;
    }

    let trimmed = contents.trim_matches(|c: char| c == '\0' || c.is_whitespace());

    let mut players: Vec<serde_json::Value> = if trimmed.is_empty() {
        // The open above creates the file when it is missing, and an empty file
        // is an empty roster rather than a failure.
        Vec::new()
    } else {
        match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Left alone rather than replaced with an empty array: the file
                // still holds a roster the plugin wrote, and overwriting it
                // would turn a parse failure into data loss.
                log::error!("Error parsing authedPlayers file, leaving it unchanged: {}", e);
                let _ = fs2::FileExt::unlock(&file);
                return;
            }
        }
    };

    update_fn(&mut players);

    // Serialized before the file is touched, so a failure here cannot leave a
    // truncated file behind.
    let encoded = match serde_json::to_string_pretty(&players) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Error serializing authedPlayers: {}", e);
            let _ = fs2::FileExt::unlock(&file);
            return;
        }
    };

    if let Err(e) = file.set_len(0) {
        log::error!("Error truncating authedPlayers file: {}", e);
        let _ = fs2::FileExt::unlock(&file);
        return;
    }

    // `set_len` does not move the cursor, which is sitting at the old end of
    // file after the read above. Without this the write lands past the end and
    // pads the front of the file with NULs.
    if let Err(e) = (&file).seek(std::io::SeekFrom::Start(0)) {
        log::error!("Error rewinding authedPlayers file: {}", e);
        let _ = fs2::FileExt::unlock(&file);
        return;
    }

    let mut writer = std::io::BufWriter::new(&file);
    if let Err(e) = writer.write_all(encoded.as_bytes()) {
        log::error!("Error writing authedPlayers file: {}", e);
    } else if let Err(e) = writer.flush() {
        // A BufWriter dropped without flushing discards the error, which would
        // report a half-written file as a success.
        log::error!("Error flushing authedPlayers file: {}", e);
    }
    drop(writer);

    let _ = fs2::FileExt::unlock(&file);
}

pub(crate) fn gen_custom_uuid() -> String {
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 16] = rng.gen();
    let random_hex: String = random_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    let fingerprint = format!(
        "Rust/actix-web ({}; {})|en-US|1920x1080|{}|{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        Utc::now().timestamp(),
        num_cpus_hint(),
    );

    let combined = format!("{}|{}", fingerprint, random_hex);
    let mut hasher = Sha256::new();
    hasher.update(combined.as_bytes());
    let result = hasher.finalize();
    let b64 = base64_encode(&result);
    b64.chars().take(32).collect()
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

fn num_cpus_hint() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch file that cleans itself up, named per-test to avoid collisions.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("apird-authed-test-{name}.json"));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
        fn write_bytes(&self, bytes: &[u8]) {
            std::fs::write(&self.0, bytes).unwrap();
        }
        fn read_bytes(&self) -> Vec<u8> {
            std::fs::read(&self.0).unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn roster(names: &[&str]) -> String {
        let players: Vec<_> = names.iter().map(|n| json!({ "username": n })).collect();
        serde_json::to_string_pretty(&players).unwrap()
    }

    fn mark(path: &str, name: &str, status: &str) {
        let name = name.to_string();
        let status = status.to_string();
        update_authed_players_file(path, |players| {
            if let Some(p) = players
                .iter_mut()
                .find(|p| p.get("username").and_then(|v| v.as_str()) == Some(&name))
            {
                p["moderation"] = json!({ "accountStatus": status });
            }
        });
    }

    /// The regression this function was written wrong for: `set_len(0)` leaves
    /// the cursor at the old end of file, so a write that does not rewind first
    /// lands past the end and pads the front of the file with NULs — after
    /// which every later read fails to parse, permanently.
    #[test]
    fn a_rewritten_file_is_still_valid_json() {
        let file = TempFile::new("rewrite");
        file.write_bytes(roster(&["Joe", "Steve"]).as_bytes());

        mark(&file.path(), "Joe", "moderated");

        let bytes = file.read_bytes();
        assert!(!bytes.contains(&0), "the file was written past its own end");

        let players: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(players.len(), 2);
        assert_eq!(players[0]["moderation"]["accountStatus"], "moderated");
    }

    /// Two updates in a row is the case a truncation-only fix still passes and
    /// a cursor bug does not: the second read has to see the first write.
    #[test]
    fn consecutive_updates_accumulate() {
        let file = TempFile::new("consecutive");
        file.write_bytes(roster(&["Joe", "Steve"]).as_bytes());

        mark(&file.path(), "Joe", "moderated");
        mark(&file.path(), "Steve", "ok");

        let players: Vec<serde_json::Value> =
            serde_json::from_slice(&file.read_bytes()).unwrap();
        assert_eq!(players[0]["moderation"]["accountStatus"], "moderated");
        assert_eq!(players[1]["moderation"]["accountStatus"], "ok");
    }

    /// A shrinking write must not leave the tail of the longer previous
    /// contents behind it.
    #[test]
    fn a_shorter_write_leaves_no_tail_behind() {
        let file = TempFile::new("shrink");
        file.write_bytes(roster(&["Joe", "Steve", "Alex", "Herobrine"]).as_bytes());

        update_authed_players_file(&file.path(), |players| {
            players.truncate(1);
        });

        let players: Vec<serde_json::Value> =
            serde_json::from_slice(&file.read_bytes()).unwrap();
        assert_eq!(players.len(), 1);
    }

    /// Files already damaged in production carry the NUL padding described
    /// above. They have to recover on the next update rather than stay broken.
    #[test]
    fn nul_padding_from_the_old_bug_is_recovered() {
        let file = TempFile::new("nul-padded");
        let mut damaged = vec![0u8; 128];
        damaged.extend_from_slice(roster(&["Joe"]).as_bytes());
        file.write_bytes(&damaged);

        mark(&file.path(), "Joe", "ok");

        let bytes = file.read_bytes();
        assert!(!bytes.contains(&0));
        let players: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(players[0]["moderation"]["accountStatus"], "ok");
    }

    /// The open creates the file when it is missing, and the empty file that
    /// leaves behind is an empty roster, not a parse failure.
    #[test]
    fn a_missing_file_starts_as_an_empty_roster() {
        let file = TempFile::new("missing");

        update_authed_players_file(&file.path(), |players| {
            players.push(json!({ "username": "Joe" }));
        });

        let players: Vec<serde_json::Value> =
            serde_json::from_slice(&file.read_bytes()).unwrap();
        assert_eq!(players[0]["username"], "Joe");
    }

    /// Genuinely corrupt contents are left alone. Replacing them with an empty
    /// array would turn a parse failure into the loss of every entry.
    #[test]
    fn unparseable_contents_are_left_untouched() {
        let file = TempFile::new("corrupt");
        file.write_bytes(b"{ this is not a roster");

        mark(&file.path(), "Joe", "ok");

        assert_eq!(file.read_bytes(), b"{ this is not a roster");
    }
}
