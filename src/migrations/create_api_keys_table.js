const { db } = require('../db');

async function createApiKeysTable() {
    try {
        await db.prepare(`
            CREATE TABLE IF NOT EXISTS api_keys (
                id VARCHAR(36) PRIMARY KEY,
                name VARCHAR(100) NOT NULL,
                api_key VARCHAR(64) NOT NULL UNIQUE,
                hourly_limit INT NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                last_reset TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                request_count INT DEFAULT 0
            )
        `).run();
        
        console.log('API keys table created successfully');
    } catch (err) {
        console.error('Error creating API keys table:', err);
        throw err;
    }
}

module.exports = { createApiKeysTable };
