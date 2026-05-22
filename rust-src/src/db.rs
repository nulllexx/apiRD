use sqlx::mysql::MySqlPoolOptions;
use sqlx::MySqlPool;
use std::time::Duration;

use crate::config::AppConfig;

pub async fn create_pool(config: &AppConfig) -> Result<MySqlPool, sqlx::Error> {
    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(20))
        .idle_timeout(Duration::from_secs(300))
        .connect(&config.database_url())
        .await?;
    Ok(pool)
}

pub async fn init_database(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    setup_database(pool).await?;
    fix_database_schema(pool).await?;
    create_api_keys_table(pool).await?;
    seed_components(pool).await?;
    Ok(())
}

async fn setup_database(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    let tables = vec![
        r#"CREATE TABLE IF NOT EXISTS components (
            id VARCHAR(255) PRIMARY KEY,
            name VARCHAR(255) NOT NULL,
            status VARCHAR(50) NOT NULL,
            last_updated TIMESTAMP NOT NULL
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS users (
            id VARCHAR(255) PRIMARY KEY,
            username VARCHAR(255) UNIQUE NOT NULL,
            password_hash VARCHAR(255) NOT NULL,
            is_admin BOOLEAN DEFAULT FALSE,
            apiKeyId VARCHAR(255) NULL,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            is_member BOOLEAN DEFAULT FALSE,
            is_projallowed BOOLEAN DEFAULT FALSE,
            is_plexallowed BOOLEAN DEFAULT FALSE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS password_reset_sessions (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            username VARCHAR(255) NOT NULL,
            session_token VARCHAR(255) UNIQUE NOT NULL,
            expires_at TIMESTAMP NOT NULL,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (username) REFERENCES users(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS user_moderation (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            user_id VARCHAR(255) NOT NULL,
            type ENUM('1d','3d','7d','14d','perm','poison') NOT NULL,
            mod_note TEXT,
            moderated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            expires_at TIMESTAMP NULL,
            created_by VARCHAR(255) NOT NULL,
            incriminatory JSON NULL,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS poison_hwids (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            hwid VARCHAR(255) UNIQUE NOT NULL,
            user_id VARCHAR(255) NOT NULL,
            poisoned_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS projects (
            id VARCHAR(255) PRIMARY KEY,
            user_id VARCHAR(255) NOT NULL,
            name VARCHAR(255) NOT NULL,
            description TEXT NULL,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS incidents (
            id VARCHAR(255) PRIMARY KEY,
            title VARCHAR(255) NOT NULL,
            impact VARCHAR(50) NOT NULL,
            status VARCHAR(50) NOT NULL,
            status_text TEXT,
            status_updated_at TIMESTAMP NOT NULL,
            started_at TIMESTAMP NOT NULL,
            ended_at TIMESTAMP NULL,
            created_by VARCHAR(255) NOT NULL,
            created_at TIMESTAMP NOT NULL
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS project_files (
            id VARCHAR(255) PRIMARY KEY,
            project_id VARCHAR(255) NOT NULL,
            filename VARCHAR(1024) NOT NULL,
            original_name VARCHAR(1024) NOT NULL,
            mime VARCHAR(255) NULL,
            size BIGINT NOT NULL,
            path TEXT NOT NULL,
            uploaded_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS incident_updates (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            incident_id VARCHAR(255) NOT NULL,
            time TIMESTAMP NOT NULL,
            body TEXT NOT NULL,
            author VARCHAR(255) NOT NULL,
            FOREIGN KEY (incident_id) REFERENCES incidents(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS incident_status_history (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            incident_id VARCHAR(255) NOT NULL,
            status VARCHAR(50) NOT NULL,
            status_text TEXT,
            status_updated_at TIMESTAMP NOT NULL,
            FOREIGN KEY (incident_id) REFERENCES incidents(id) ON DELETE CASCADE
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS incident_affected_components (
            incident_id VARCHAR(255) NOT NULL,
            component_id VARCHAR(255) NOT NULL,
            PRIMARY KEY (incident_id, component_id),
            FOREIGN KEY (incident_id) REFERENCES incidents(id) ON DELETE CASCADE,
            FOREIGN KEY (component_id) REFERENCES components(id)
        ) ENGINE=InnoDB"#,
        r#"CREATE TABLE IF NOT EXISTS meta (
            `key` VARCHAR(255) PRIMARY KEY,
            `value` TEXT
        ) ENGINE=InnoDB"#,
    ];

    for create_table in &tables {
        log::info!("Running: {}", &create_table[..create_table.len().min(60)]);
        sqlx::query(create_table).execute(pool).await?;
        log::info!("OK");
    }

    log::info!("Database tables created/verified successfully");
    Ok(())
}

async fn create_api_keys_table(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS api_keys (
            id VARCHAR(36) PRIMARY KEY,
            name VARCHAR(100) NOT NULL,
            api_key VARCHAR(64) NOT NULL UNIQUE,
            hourly_limit INT NOT NULL DEFAULT 100,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            last_reset TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            request_count INT DEFAULT 0
        )"#,
    )
    .execute(pool)
    .await?;
    log::info!("API keys table created/verified successfully");
    Ok(())
}

async fn fix_database_schema(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct ColumnInfo {
        #[sqlx(rename = "COLUMN_NAME")]
        column_name: String,
        #[sqlx(rename = "COLUMN_DEFAULT")]
        column_default: Option<String>,
        #[sqlx(rename = "IS_NULLABLE")]
        is_nullable: String,
    }

    let columns: Vec<ColumnInfo> = sqlx::query_as(
        r#"SELECT COLUMN_NAME, COLUMN_DEFAULT, IS_NULLABLE
           FROM INFORMATION_SCHEMA.COLUMNS
           WHERE TABLE_SCHEMA = DATABASE()
           AND TABLE_NAME = 'users'"#,
    )
    .fetch_all(pool)
    .await?;

    let column_names: Vec<&str> = columns.iter().map(|c| c.column_name.as_str()).collect();
    log::info!("Current users table columns: {:?}", column_names);

    // Rename password -> password_hash if needed
    if column_names.contains(&"password") && !column_names.contains(&"password_hash") {
        log::info!("Renaming password column to password_hash...");
        sqlx::query("ALTER TABLE users CHANGE password password_hash VARCHAR(255) NOT NULL")
            .execute(pool)
            .await?;
        log::info!("Column renamed successfully");
    }

    // Rename isAdmin -> is_admin if needed
    if column_names.contains(&"isAdmin") && !column_names.contains(&"is_admin") {
        log::info!("Renaming isAdmin column to is_admin...");
        sqlx::query("ALTER TABLE users CHANGE isAdmin is_admin BOOLEAN DEFAULT FALSE")
            .execute(pool)
            .await?;
        log::info!("Column renamed successfully");
    }

    // Fix created_at default
    if let Some(col) = columns.iter().find(|c| c.column_name == "created_at") {
        if col.column_default.is_none() && col.is_nullable == "NO" {
            log::info!("Fixing created_at column to have default value...");
            sqlx::query(
                "ALTER TABLE users MODIFY created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP",
            )
            .execute(pool)
            .await?;
            log::info!("created_at column fixed successfully");
        }
    }

    // Add is_member if missing
    if !column_names.contains(&"is_member") {
        log::info!("Adding is_member column...");
        sqlx::query("ALTER TABLE users ADD is_member BOOLEAN DEFAULT FALSE")
            .execute(pool)
            .await?;
        log::info!("Column is_member added successfully");
    }

    // Add email column if missing (used by Google OAuth + auto-link)
    if !column_names.contains(&"email") {
        log::info!("Adding email column...");
        sqlx::query("ALTER TABLE users ADD email VARCHAR(255) NULL")
            .execute(pool)
            .await?;
        log::info!("Column email added successfully");
    }

    // Add oauth_provider column if missing
    if !column_names.contains(&"oauth_provider") {
        log::info!("Adding oauth_provider column...");
        sqlx::query("ALTER TABLE users ADD oauth_provider VARCHAR(50) NULL")
            .execute(pool)
            .await?;
        log::info!("Column oauth_provider added successfully");
    }

    // Add oauth_subject column if missing
    if !column_names.contains(&"oauth_subject") {
        log::info!("Adding oauth_subject column...");
        sqlx::query("ALTER TABLE users ADD oauth_subject VARCHAR(255) NULL")
            .execute(pool)
            .await?;
        log::info!("Column oauth_subject added successfully");
    }

    // Add unique indexes if missing. Check INFORMATION_SCHEMA.STATISTICS;
    // MariaDB allows multiple NULLs in a unique index, which is what we want
    // because existing rows have NULL email / oauth_subject.
    let index_names: Vec<(String,)> = sqlx::query_as(
        r#"SELECT DISTINCT INDEX_NAME FROM INFORMATION_SCHEMA.STATISTICS
           WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users'"#,
    )
    .fetch_all(pool)
    .await?;
    let index_names: Vec<&str> = index_names.iter().map(|(n,)| n.as_str()).collect();

    if !index_names.contains(&"idx_users_email") {
        log::info!("Adding unique index idx_users_email...");
        sqlx::query("CREATE UNIQUE INDEX idx_users_email ON users (email)")
            .execute(pool)
            .await?;
        log::info!("Index idx_users_email added successfully");
    }

    if !index_names.contains(&"idx_users_oauth") {
        log::info!("Adding unique index idx_users_oauth...");
        sqlx::query("CREATE UNIQUE INDEX idx_users_oauth ON users (oauth_provider, oauth_subject)")
            .execute(pool)
            .await?;
        log::info!("Index idx_users_oauth added successfully");
    }

    Ok(())
}

async fn seed_components(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM components")
        .fetch_one(pool)
        .await?;

    if count.0 == 0 {
        let now = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let seeds = vec![
            ("login", "Login"),
            ("developer-login", "Developer Login"),
            ("public-api", "Public API"),
            ("minecraft-server", "Minecraft Server"),
            ("storage", "Storage"),
            ("private-api", "Private API"),
        ];

        for (id, name) in seeds {
            sqlx::query(
                "INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, 'operational', ?)",
            )
            .bind(id)
            .bind(name)
            .bind(&now)
            .execute(pool)
            .await?;
        }
        log::info!("Seeded database with initial components");
    } else {
        log::info!("Components already exist, skipping seeding");
    }

    Ok(())
}
