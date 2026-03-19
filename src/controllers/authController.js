const bcrypt = require('bcryptjs');
const jwt = require('jsonwebtoken');
const { db } = require('../db');
const { v4: uuidv4 } = require('uuid');

class AuthController {
  async updateAdminStatus(req, res) {
    const { username, isAdmin } = req.body;
    if (!req.user.isAdmin) return res.status(403).json({ message: 'Access Denied' });

    try {
      const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
      if (!user) return res.status(404).json({ message: 'User not found' });

      await db.prepare('UPDATE users SET is_admin = ? WHERE id = ?').run([isAdmin ? 1 : 0, user.id]);
      res.status(200).json({ message: 'Admin status updated' });
    } catch (err) {
      console.error('Admin update error:', err);
      res.status(500).json({ message: 'Internal server error' });
    }
  }

  async logout(req, res) {
    res.clearCookie('userToken', { httpOnly: true, secure: true, sameSite: 'Strict' });
    res.status(200).json({ message: 'Logout successful' });
  }
}

module.exports = AuthController;
