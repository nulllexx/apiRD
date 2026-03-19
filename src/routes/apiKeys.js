const express = require('express');
const router = express.Router();
const { v4: uuidv4 } = require('uuid');
const { db } = require('../db');
const adminAuth = require('../middleware/adminAuth');

// Create new API key (admin only)
router.post('/', adminAuth, async (req, res) => {
    try {
        const { name, hourlyLimit } = req.body;
        
        if (!name) {
            return res.status(400).json({ error: 'Name is required' });
        }

        const id = uuidv4();
        const apiKey = uuidv4(); // In production, you might want a more secure key generation method
        
        await db.prepare(`
            INSERT INTO api_keys (id, name, api_key, hourly_limit)
            VALUES (?, ?, ?, ?)
        `).run([id, name, apiKey, hourlyLimit || 100]);

        res.status(201).json({ 
            id,
            name,
            apiKey,
            hourlyLimit: hourlyLimit || 100
        });
    } catch (err) {
        console.error('Error creating API key:', err);
        res.status(500).json({ error: 'Failed to create API key' });
    }
});

// List all API keys (admin only)
router.get('/', adminAuth, async (req, res) => {
    try {
        const keys = await db.prepare(`
            SELECT id, name, api_key, hourly_limit, created_at, request_count
            FROM api_keys
            ORDER BY created_at DESC
        `).all();
        
        res.json(keys);
    } catch (err) {
        console.error('Error listing API keys:', err);
        res.status(500).json({ error: 'Failed to list API keys' });
    }
});

// Delete API key (admin only)
router.delete('/:id', adminAuth, async (req, res) => {
    try {
        await db.prepare('DELETE FROM api_keys WHERE id = ?').run([req.params.id]);
        res.json({ message: 'API key deleted successfully' });
    } catch (err) {
        console.error('Error deleting API key:', err);
        res.status(500).json({ error: 'Failed to delete API key' });
    }
});

module.exports = router;
