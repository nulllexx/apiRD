// src/middleware/adminAuth.js
const jwt = require('jsonwebtoken');
const dotenv = require('dotenv');
const { db } = require('../db');
dotenv.config();
// verify cookie-based JWT and ensure username is one of privUsers
module.exports = async function adminAuth(req, res, next) {
  try {
    const token = req.cookies.userToken;
    if (!token) return res.status(401).json({ error: 'No auth token' });

    const secret = process.env.JWT_SECRET;
    if (!secret) {
      console.error('JWT_SECRET missing');
      return res.status(500).json({ error: 'Server config error' });
    }

    const payload = jwt.verify(token, secret);
    if (!payload || !payload.username) return res.status(401).json({ error: 'Invalid token' });

    const isAdmin = await db.prepare('SELECT id FROM users WHERE username = ? AND is_admin = 1').get([payload.username]);
    if (!isAdmin) return res.status(403).json({ error: 'Not an admin user' });

    // attach auth info for controllers
    req.admin = { username: payload.username };
    next();
  } catch (err) {
    console.error('adminAuth error:', err);
    return res.status(401).json({ error: 'Invalid or expired token' });
  }
}
