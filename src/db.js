const mariadb = require('mariadb');
const fs = require('fs');
const path = require('path');

// Create a connection pool
const pool = mariadb.createPool({
    host: process.env.DB_HOST, // Docker host's default bridge network IP
    port: process.env.DB_PORT || 3306,
    user: process.env.DB_USER,
    password: process.env.DB_PASSWORD,
    database: process.env.DB_NAME,
    connectionLimit: 5,
    connectTimeout: 20000,        // 20 seconds
    acquireTimeout: 20000,        // 20 seconds
    trace: true,                  // Enable trace for debugging
    multipleStatements: true,     // Allow multiple statements
    timezone: 'Z',               // Use UTC
    dateStrings: true,           // Return dates as strings
    resetAfterUse: true,         // Reset connection after use
    retryAttempts: 3,            // Number of retry attempts
    initializationTimeout: 30000, // 30 seconds
    minDelayValidation: 500,     // Minimum delay between retries
    allowPublicKeyRetrieval: true // Allow public key retrieval
});

// Helper function to execute queries
async function query(sql, params) {
    let conn;
    try {
        conn = await pool.getConnection();
        return await conn.query(sql, params);
    } finally {
        if (conn) conn.release();
    }
}

// Helper function to execute a single query and get first result
async function queryOne(sql, params) {
    const results = await query(sql, params);
    return results[0];
}

const db = {
    prepare: (sql) => ({
        all: async (params) => await query(sql, params),
        get: async (params) => await queryOne(sql, params),
        run: async (params) => await query(sql, params)
    }),
    exec: async (sql) => await query(sql),
    close: async () => await pool.end()
};

// Setup database schema and initial data
async function setupDatabase() {
    try {
        // Create tables one by one
        const tables = [
            `CREATE TABLE IF NOT EXISTS components (
                id VARCHAR(255) PRIMARY KEY,
                name VARCHAR(255) NOT NULL,
                status VARCHAR(50) NOT NULL,
                last_updated TIMESTAMP NOT NULL
            ) ENGINE=InnoDB`,
            
            `CREATE TABLE IF NOT EXISTS users (
                id VARCHAR(255) PRIMARY KEY,       
                username VARCHAR(255) UNIQUE NOT NULL,
                password_hash VARCHAR(255) NOT NULL,
                is_admin BOOLEAN DEFAULT FALSE,
                apiKeyId VARCHAR(255) NULL,     
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                is_member BOOLEAN DEFAULT FALSE,
                is_projallowed BOOLEAN DEFAULT FALSE,
                is_plexallowed BOOLEAN DEFAULT FALSE
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS password_reset_sessions (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                username VARCHAR(255) NOT NULL,
                session_token VARCHAR(255) UNIQUE NOT NULL,
                expires_at TIMESTAMP NOT NULL,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (username) REFERENCES users(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS user_moderation (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                user_id VARCHAR(255) NOT NULL,
                type ENUM('1d','3d','7d','14d','perm','poison') NOT NULL,
                mod_note TEXT,
                moderated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                expires_at TIMESTAMP NULL,
                created_by VARCHAR(255) NOT NULL,
                incriminatory JSON NULL,           
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS poison_hwids (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                hwid VARCHAR(255) UNIQUE NOT NULL,
                user_id VARCHAR(255) NOT NULL,
                poisoned_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,
            
            `CREATE TABLE IF NOT EXISTS projects (
                id VARCHAR(255) PRIMARY KEY,
                user_id VARCHAR(255) NOT NULL,
                name VARCHAR(255) NOT NULL,
                description TEXT NULL,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,
            
            `CREATE TABLE IF NOT EXISTS incidents (
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
            ) ENGINE=InnoDB`,
            
            `CREATE TABLE IF NOT EXISTS project_files (
                id VARCHAR(255) PRIMARY KEY,
                project_id VARCHAR(255) NOT NULL,
                filename VARCHAR(1024) NOT NULL,
                original_name VARCHAR(1024) NOT NULL,
                mime VARCHAR(255) NULL,
                size BIGINT NOT NULL,
                path TEXT NOT NULL,
                uploaded_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS incident_updates (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                incident_id VARCHAR(255) NOT NULL,
                time TIMESTAMP NOT NULL,
                body TEXT NOT NULL,
                author VARCHAR(255) NOT NULL,
                FOREIGN KEY (incident_id) REFERENCES incidents(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS incident_status_history (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                incident_id VARCHAR(255) NOT NULL,
                status VARCHAR(50) NOT NULL,
                status_text TEXT,
                status_updated_at TIMESTAMP NOT NULL,
                FOREIGN KEY (incident_id) REFERENCES incidents(id) ON DELETE CASCADE
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS incident_affected_components (
                incident_id VARCHAR(255) NOT NULL,
                component_id VARCHAR(255) NOT NULL,
                PRIMARY KEY (incident_id, component_id),
                FOREIGN KEY (incident_id) REFERENCES incidents(id) ON DELETE CASCADE,
                FOREIGN KEY (component_id) REFERENCES components(id)
            ) ENGINE=InnoDB`,

            `CREATE TABLE IF NOT EXISTS meta (
                \`key\` VARCHAR(255) PRIMARY KEY,
                \`value\` TEXT
            ) ENGINE=InnoDB`
        ];

        // Execute each CREATE TABLE statement separately
        for (const createTable of tables) {
            try {
                console.log("Running:", createTable);
                await db.exec(createTable);
                console.log("OK");
            } catch (err) {
                console.error("FAILED FOR:");
                console.error(createTable);
                console.error(err);
                throw err;
            }

        }
        
        console.log('Database tables created/verified successfully');
        return true;
    } catch (err) {
        console.error('Error creating tables:', err);
        throw err;
    }
}

// Fix existing database schema if needed
async function fixDatabaseSchema() {
    try {
        // Check if users table exists and what columns it has
        const columns = await query(`
            SELECT COLUMN_NAME, COLUMN_DEFAULT, IS_NULLABLE, DATA_TYPE, EXTRA
            FROM INFORMATION_SCHEMA.COLUMNS 
            WHERE TABLE_SCHEMA = DATABASE() 
            AND TABLE_NAME = 'users'
        `);
        
        const columnNames = columns.map(col => col.COLUMN_NAME);
        console.log('Current users table columns:', columnNames);
        
        // If table has 'password' but not 'password_hash', rename the column
        if (columnNames.includes('password') && !columnNames.includes('password_hash')) {
            console.log('Renaming password column to password_hash...');
            await query('ALTER TABLE users CHANGE password password_hash VARCHAR(255) NOT NULL');
            console.log('Column renamed successfully');
        }
        
        // If table has 'isAdmin' but not 'is_admin', rename the column
        if (columnNames.includes('isAdmin') && !columnNames.includes('is_admin')) {
            console.log('Renaming isAdmin column to is_admin...');
            await query('ALTER TABLE users CHANGE isAdmin is_admin BOOLEAN DEFAULT FALSE');
            console.log('Column renamed successfully');
        }
        
        // Check if created_at column needs fixing
        const createdAtColumn = columns.find(col => col.COLUMN_NAME === 'created_at');
        if (createdAtColumn && !createdAtColumn.COLUMN_DEFAULT && createdAtColumn.IS_NULLABLE === 'NO') {
            console.log('Fixing created_at column to have default value...');
            await query('ALTER TABLE users MODIFY created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP');
            console.log('created_at column fixed successfully');
        }

        // Add is_member column if it doesn't exist
        if (!columnNames.includes('is_member')) {
            console.log('Adding is_member column...');
            await query('ALTER TABLE users ADD is_member BOOLEAN DEFAULT FALSE');
            console.log('Column is_member added successfully');
        }

        return true;
    } catch (err) {
        console.error('Error fixing database schema:', err);
        throw err;
    }
}


// Initialize database and seed data
async function initDatabase() {
    try {
        await setupDatabase();
        await fixDatabaseSchema(); // Fix any existing schema issues

        // Check existing data
        const existingComponents = await queryOne('SELECT COUNT(*) as c FROM components');
        const existingIncidents = await queryOne('SELECT COUNT(*) as c FROM incidents');
        console.log(`Existing data - Components: ${existingComponents.c}, Incidents: ${existingIncidents.c}`);

        // Seed components if none exist
        if (existingComponents.c === 0) {
            const now = new Date().toISOString();
            const seedComponents = [
                ['login', 'Login', 'operational', now],
                ['developer-login', 'Developer Login', 'operational', now],
                ['public-api', 'Public API', 'operational', now],
                ['minecraft-server', 'Minecraft Server', 'operational', now],
                ['storage', 'Storage', 'operational', now],
                ['private-api', 'Private API', 'operational', now]
            ];

            for (const [id, name, status, lastUpdated] of seedComponents) {
                await query(
                    'INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)',
                    [id, name, status, lastUpdated]
                );
            }

            console.log('Seeded database with initial components');
        } else {
            console.log('Components already exist, skipping seeding');
            const components = await query('SELECT id, name, status FROM components');
            console.log('Existing components:', components);
        }

        // Show recent incidents
        if (existingIncidents.c > 0) {
            const incidents = await query(
                'SELECT id, title, status, started_at FROM incidents ORDER BY started_at DESC LIMIT 5'
            );
            console.log('Recent incidents:', incidents);
        }

        return true;
    } catch (err) {
        console.error('Error initializing database:', err);
        throw err;
    }
}

// Add graceful shutdown handlers
process.on('SIGINT', async () => {
    console.log('\nClosing database connection...');
    await db.close();
    process.exit(0);
});

process.on('SIGTERM', async () => {
    console.log('\nClosing database connection...');
    await db.close();
    process.exit(0);
});

module.exports = { db, query, queryOne, initDatabase };