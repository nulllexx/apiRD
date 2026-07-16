use sqlx::MySqlPool;

use crate::error::AppError;

pub struct ApiKeyValidation {
    pub valid: bool,
    pub error: Option<String>,
}

/// Validate API key and track usage (replicates ApiKeyManager.validateAndTrackUsage)
pub async fn validate_and_track_usage(
    pool: &MySqlPool,
    api_key: &str,
) -> Result<ApiKeyValidation, AppError> {
    #[derive(sqlx::FromRow)]
    struct KeyInfo {
        id: String,
        hourly_limit: i32,
        request_count: i32,
        minutes_since_reset: Option<i64>,
    }

    let result: Option<KeyInfo> = sqlx::query_as(
        r#"SELECT
            id,
            hourly_limit,
            request_count,
            TIMESTAMPDIFF(MINUTE, last_reset, NOW()) as minutes_since_reset
        FROM api_keys
        WHERE api_key = ?"#,
    )
    .bind(api_key)
    .fetch_optional(pool)
    .await?;

    let key_info = match result {
        Some(k) => k,
        None => {
            return Ok(ApiKeyValidation {
                valid: false,
                error: Some("Invalid API key".to_string()),
            });
        }
    };

    let minutes_since_reset = key_info.minutes_since_reset.unwrap_or(0);

    // Reset counter if it's been an hour
    if minutes_since_reset >= 60 {
        sqlx::query(
            "UPDATE api_keys SET request_count = 1, last_reset = NOW() WHERE id = ?",
        )
        .bind(&key_info.id)
        .execute(pool)
        .await?;

        return Ok(ApiKeyValidation {
            valid: true,
            error: None,
        });
    }

    // Check rate limit
    if key_info.request_count >= key_info.hourly_limit {
        return Ok(ApiKeyValidation {
            valid: false,
            error: Some("Hourly rate limit exceeded".to_string()),
        });
    }

    // Increment counter
    sqlx::query("UPDATE api_keys SET request_count = request_count + 1 WHERE id = ?")
        .bind(&key_info.id)
        .execute(pool)
        .await?;

    Ok(ApiKeyValidation {
        valid: true,
        error: None,
    })
}

/// Get API key usage stats (replicates ApiKeyManager.getKeyUsage)
pub async fn get_key_usage(
    pool: &MySqlPool,
    api_key: &str,
) -> Result<serde_json::Value, AppError> {
    // Reset if needed
    sqlx::query(
        r#"UPDATE api_keys
        SET request_count = 0, last_reset = NOW()
        WHERE api_key = ?
        AND TIMESTAMPDIFF(MINUTE, last_reset, NOW()) >= 60"#,
    )
    .bind(api_key)
    .execute(pool)
    .await?;

    #[derive(sqlx::FromRow)]
    struct UsageInfo {
        name: String,
        hourly_limit: i32,
        request_count: i32,
        minutes_since_reset: Option<i64>,
    }

    let result: Option<UsageInfo> = sqlx::query_as(
        r#"SELECT
            name,
            hourly_limit,
            request_count,
            TIMESTAMPDIFF(MINUTE, last_reset, NOW()) as minutes_since_reset
        FROM api_keys
        WHERE api_key = ?"#,
    )
    .bind(api_key)
    .fetch_optional(pool)
    .await?;

    match result {
        Some(info) => {
            let minutes_since_reset = info.minutes_since_reset.unwrap_or(0);
            let remaining = (info.hourly_limit - info.request_count).max(0);
            let minutes_until_reset = (60 - minutes_since_reset).max(0);

            Ok(serde_json::json!({
                "name": info.name,
                "hourlyLimit": info.hourly_limit,
                "remainingRequests": remaining,
                "minutesUntilReset": minutes_until_reset,
                "resetsIn": format!("{}h {}m", minutes_until_reset / 60, minutes_until_reset % 60)
            }))
        }
        None => Ok(serde_json::json!({ "error": "Invalid API key" })),
    }
}
