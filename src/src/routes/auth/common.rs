use actix_web::{web, HttpRequest};
use chrono::Utc;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};

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

    let mut players: Vec<serde_json::Value> = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => {
            log::error!("Error parsing authedPlayers file, treating as empty array");
            let _ = fs2::FileExt::unlock(&file);
            return;
        }
    };

    update_fn(&mut players);

    // Truncate and write back
    if let Err(e) = file.set_len(0) {
        log::error!("Error truncating authedPlayers file: {}", e);
        let _ = fs2::FileExt::unlock(&file);
        return;
    }
    let mut writer = std::io::BufWriter::new(&file);
    if let Err(e) = writer.write_all(
        serde_json::to_string_pretty(&players)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        log::error!("Error writing authedPlayers file: {}", e);
    }

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

