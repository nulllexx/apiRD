const { db } = require('../db');

class ApiKeyManager {
    static async getKeyUsage(apiKey) {
        try {
            // First, check if we need to reset and do it
            await this._resetIfNeeded(apiKey);
            
            // Now get the current usage
            const result = await db.prepare(`
                SELECT 
                    name,
                    hourly_limit,
                    request_count,
                    last_reset,
                    TIMESTAMPDIFF(MINUTE, last_reset, NOW()) as minutes_since_reset
                FROM api_keys 
                WHERE api_key = ?
            `).get([apiKey]);

            if (!result) {
                return { error: 'Invalid API key' };
            }

            // Convert BigInt values to regular numbers
            const minutesSinceReset = Number(result.minutes_since_reset);
            const requestCount = Number(result.request_count);
            const hourlyLimit = Number(result.hourly_limit);
            
            // Calculate remaining requests
            const remainingRequests = Math.max(0, hourlyLimit - requestCount);
            
            // Calculate minutes until reset
            const minutesUntilReset = Math.max(0, 60 - minutesSinceReset);

            return {
                name: result.name,
                hourlyLimit: hourlyLimit,
                remainingRequests,
                minutesUntilReset,
                resetsIn: `${Math.floor(minutesUntilReset / 60)}h ${minutesUntilReset % 60}m`
            };
        } catch (err) {
            console.error('Error getting API key usage:', err);
            return { error: 'Internal server error' };
        }
    }

    // Helper method to reset counter if needed
    static async _resetIfNeeded(apiKey) {
        try {
            await db.prepare(`
                UPDATE api_keys 
                SET request_count = 0,
                    last_reset = NOW()
                WHERE api_key = ? 
                AND TIMESTAMPDIFF(MINUTE, last_reset, NOW()) >= 60
            `).run([apiKey]);
        } catch (err) {
            console.error('Error resetting API key:', err);
        }
    }

    static async validateAndTrackUsage(apiKey) {
        try {
            // Get API key details and check if reset is needed
            const result = await db.prepare(`
                SELECT 
                    id,
                    hourly_limit,
                    request_count,
                    last_reset,
                    TIMESTAMPDIFF(MINUTE, last_reset, NOW()) as minutes_since_reset
                FROM api_keys 
                WHERE api_key = ?
            `).get([apiKey]);

            if (!result) {
                return { valid: false, error: 'Invalid API key' };
            }

            // Check if we need to reset the counter (it's been an hour)
            const minutesSinceReset = Number(result.minutes_since_reset);
            const requestCount = Number(result.request_count);
            const hourlyLimit = Number(result.hourly_limit);
            
            if (minutesSinceReset >= 60) {
                await db.prepare(`
                    UPDATE api_keys 
                    SET request_count = 1,
                        last_reset = NOW()
                    WHERE id = ?
                `).run([result.id]);
                return { valid: true };
            }

            // Check if we're over the limit
            if (requestCount >= hourlyLimit) {
                return { valid: false, error: 'Hourly rate limit exceeded' };
            }

            // Increment the counter
            await db.prepare(`
                UPDATE api_keys 
                SET request_count = request_count + 1
                WHERE id = ?
            `).run([result.id]);

            return { valid: true };
        } catch (err) {
            console.error('API key validation error:', err);
            return { valid: false, error: 'Internal server error' };
        }
    }
}

const apiKeyAuth = async (req, res, next) => {
    const apiKey = req.header('X-API-Key');
        
    if (!apiKey) {
        return res.status(401).json({ error: 'API key is required' });
    }

    const result = await ApiKeyManager.validateAndTrackUsage(apiKey);
        
    if (!result.valid) {
        return res.status(401).json({ error: result.error });
    }

    next();
};

module.exports = { apiKeyAuth, ApiKeyManager };