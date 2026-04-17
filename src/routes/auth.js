const express = require('express');
const AuthController = require('../controllers/authController');
const { exec } = require("child_process");
const rateLimit = require('express-rate-limit');
const { db } = require('../db');
const router = express.Router();
const authController = new AuthController();
const FileController = require('../controllers/fileController');
const multer = require('multer');
const fileController = new FileController();
const upload = multer({ dest: 'C:/server/media' });
const authMiddleware = require('../middleware/auth');
const permissiveAuthMiddleware = require('../middleware/auth_failopen');
const CommandController = require('../controllers/overviewController');
const commandController = new CommandController();
const path = require('path');
const fs = require('fs');
const bcrypt = require('bcryptjs');
const jwt = require('jsonwebtoken');
const chokidar = require('chokidar');
const readline = require('readline');
const { truncate } = require('fs/promises');
const dotenv = require('dotenv');
const { v4: uuidv4 } = require('uuid');
const { apiKeyAuth, ApiKeyManager} = require('../middleware/apiKeyAuth');
const crypto = require('crypto');
const os = require('os');
const { createCanvas } = require('canvas');
const lockfile = require('lockfile');
dotenv.config();
const limiter = rateLimit({
    windowMs: 60 * 1000, // 1 minute
    max: 50,
    message: { error: 'Too many requests, please try again in approx. 1 minute.' }
});
function genCustomUUID() {
  const canvas = createCanvas(200, 50);
  const ctx = canvas.getContext('2d');
  ctx.textBaseline = 'top';
  ctx.font = '14px Arial';
  ctx.fillText('Device fingerprint', 2, 2);

  const fingerprint = [
    `Node.js/${process.version} (${os.platform()}; ${os.arch()})`,
    process.env.LANG || process.env.LC_ALL || 'en-US',
    '1920x1080',
    new Date().getTimezoneOffset(),
    os.cpus().length,
    canvas.toDataURL()
  ].join('|');

  // Add strong randomness
  const randomBytes = crypto.randomBytes(16).toString('hex');

  // Mix fingerprint + randomness, then hash for consistent length
  const combined = fingerprint + '|' + randomBytes;
  const hash = crypto.createHash('sha256').update(combined).digest('base64');

  return hash.substring(0, 32); // trim to 32 chars like before
}
const skinStorage = multer.diskStorage({
  destination: (req, file, cb) => {
    cb(null, '/mcserver/plugins/SkinsRestorer/skins');
  },
  filename: (req, file, cb) => {
    const uniqueSuffix = Date.now() + '-' + Math.round(Math.random() * 1E9);
    cb(null, file.fieldname + '-' + uniqueSuffix + path.extname(file.originalname));
  }
});
const authedPlayersFilePath = "/mcserver/authedPlayers.json";
const lockFile = (callback) => {
  lockfile.lock(authedPlayersFilePath + '.lock', { retries: 3, retryWait: 100 }, (err) => {
    if (err) return console.error('File lock error:', err);
    callback();
  });
};
const plrFilePath = "/mcserver/plrCount.json";
const maxPlayersFile = "/mcserver/server.properties";
let playerCount = 0;
let maxPlayers = 0
function loadPlayerCount() {
  fs.readFile(plrFilePath, 'utf8', (err, data) => {
    if (err) {
      console.error(`error reading file: ${err}`);
      return;
    }
    try {
      const parsed = JSON.parse(data);
      if (typeof parsed.players === 'number') {
        playerCount = parsed.players;
      }
    } catch (e) {
      console.error(`invalid json format: ${e}`);
    }
  });
}
function loadMaxPlayers() {
  // line by line reading to find max-players=(num)
  const rl = readline.createInterface({
    input: fs.createReadStream(maxPlayersFile),
    crlfDelay: Infinity,
  })
  rl.on('line', (line) => {
    const match = line.match(/^max-players=(\d+)/)
    if (match) {
      maxPlayers = parseInt(match[1], 10)
      rl.close() // stop reading after found
    }
  })

  rl.on('error', (err) => {
    console.error(`error reading maxPlayers file: ${err}`)
  })
}
const skinUpload = multer({
  storage: skinStorage,
  fileFilter: (req, file, cb) => {
    const ext = path.extname(file.originalname).toLowerCase();
    if (ext === '.skin' || ext === '.skinfile' || ext == '.customskin') {
      cb(null, true);
    } else {
      cb(new Error('only .skin or .skinfile allowed'), false);
    }
  }
});

const allowedTokens = new Set();
router.use(limiter);
loadPlayerCount();
loadMaxPlayers();
chokidar.watch(plrFilePath, { ignoreInitial: true }).on('change', () => {
  loadPlayerCount();
});
chokidar.watch(maxPlayersFile, { ignoreInitial: true }).on('change', () => {
  loadMaxPlayers()
})

router.post('/register', async (req, res) => {
    try {
        const { username, password, hwid } = req.body;
        if (!username || !password) return res.status(400).json({ error: 'Missing username or password' });

        // Check HWID poison
        if (hwid) {
            const poisoned = await db.prepare('SELECT id FROM poison_hwids WHERE hwid = ?').get([hwid]);
            if (poisoned) return res.status(403).json({ error: 'This device is banned.' });
        } else {
          return res.status(401).json({ error: 'Could not validate your device' });
        }

        // Check if username exists
        const existing = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
        if (existing) return res.status(409).json({ error: 'Username already taken' });

        const hashedPassword = await bcrypt.hash(password, 10);
        const userId = uuidv4();

        await db.prepare('INSERT INTO users (id, username, password_hash) VALUES (?, ?, ?)').run([userId, username, hashedPassword]);

        const token = jwt.sign({ username, id: userId }, process.env.JWT_SECRET, { expiresIn: '7d' });

        res.cookie('userToken', token, { httpOnly: true, secure: true, sameSite: 'Strict', maxAge: 7 * 24 * 60 * 60 * 1000 });
        return res.json({ message: 'Registration successful', token });
    } catch (err) {
        console.error('Register error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
router.post('/login', async (req, res) => {
    try {
        const { username, password } = req.body;
        if (!username || !password) 
            return res.status(400).json({ error: 'Missing username or password' });

        const user = await db.prepare(
            'SELECT id, password_hash, is_admin FROM users WHERE username = ?'
        ).get([username]);

        if (!user) return res.status(401).json({ error: 'Invalid credentials' });

        const valid = await bcrypt.compare(password, user.password_hash);
        if (!valid) return res.status(401).json({ error: 'Invalid credentials' });

        const token = jwt.sign(
            { username, id: user.id, isAdmin: !!user.is_admin },
            process.env.JWT_SECRET,
            { expiresIn: '7d' }
        );

        res.cookie('userToken', token, { 
            httpOnly: true, 
            secure: true, 
            sameSite: 'Strict', 
            maxAge: 7 * 24 * 60 * 60 * 1000 
        });

        return res.json({ 
            message: 'Login successful', 
            token, 
            isAdmin: !!user.is_admin // cosmetic + front-end logic
        });
    } catch (err) {
        console.error('Login error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
router.post('/v-creds', async (req, res) => {
    try {
        const { username, password } = req.body;
        if (!username || !password) 
            return res.status(400).json({ error: 'Missing username or password' });

        const user = await db.prepare(
            'SELECT id, password_hash, is_admin, is_member FROM users WHERE username = ?'
        ).get([username]);

        if (!user) return res.status(401).json({ error: 'Invalid credentials' });

        const valid = await bcrypt.compare(password, user.password_hash);
        if (!valid) return res.status(401).json({ error: 'Invalid credentials' });

        return res.json({ 
            message: 'Credentials valid', 
            isAdmin: !!user.is_admin,
            isMember: !!user.is_member
        });
    } catch (err) {
        console.error('v-creds error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});

router.post('/update-admin-status', authMiddleware, authController.updateAdminStatus.bind(authController));
router.get('/validate', async (req, res) => {
  const token = req.cookies.userToken;
  if (!token) return res.status(401).json({ error: 'Not logged in' });

  try {
    const payload = jwt.verify(token, process.env.JWT_SECRET);

    // Fetch user from DB
    const user = await db.prepare('SELECT username, is_admin FROM users WHERE id = ?').get([payload.id]);
    if (!user) return res.status(401).json({ error: 'Invalid user' });
    res.json({
      username: user.username,
      isAdmin: !!user.is_admin,
    });
  } catch (err) {
    console.error('Validate error:', err);
    res.status(401).json({ error: 'Invalid token' });
  }
});

router.get('/account-status', permissiveAuthMiddleware, async (req, res) => {
    try {
        const userToken = req.cookies.userToken;
        if (!userToken) return res.json({ accountStatus: null });

        const payload = jwt.verify(userToken, process.env.JWT_SECRET);
        const hwid = req.cookies.hwid;
        // Check normal moderation
        const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([payload.username]);
        if (!user) return res.json({ accountStatus: null });

        const moderation = await db.prepare(
            'SELECT * FROM user_moderation WHERE user_id = ? ORDER BY moderated_at DESC LIMIT 1'
        ).get([user.id]);

        if (!moderation) return res.json({ accountStatus: 'ok' });
        if (!hwid) return res.status(400).json({ error: 'Missing hwid param' });
        // Check poison HWID
        if (moderation.type === 'poison') {
            const poisoned = await db.prepare('SELECT id FROM poison_hwids WHERE hwid = ?').get([hwid]);
            if (!poisoned) {
                await db.prepare('INSERT IGNORE INTO poison_hwids (hwid, user_id) VALUES (?, ?)').run([hwid, user.id]);
            }
        }
        return res.status(403).json({
            accountStatus: 'moderated',
            banInfo: {
                type: moderation.type,
                moderatedTimePDT: moderation.moderated_at,
                modNote: moderation.mod_note,
                incriminatory: (() => {
                  if (!moderation.incriminatory) return null;
                  if (typeof moderation.incriminatory === 'string') {
                    try {
                      return JSON.parse(moderation.incriminatory);
                    } catch (e) {
                      console.error("Invalid incriminatory JSON:", moderation.incriminatory);
                      return null;
                    }
                  }
                  // already parsed (array/object)
                  return moderation.incriminatory;
                })()

            }
        });
    } catch (err) {
        console.error('Error checking account status:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});

router.get('/account-data', authMiddleware, async (req, res) => {
    try {
        const user = await db.prepare('SELECT id, username, is_admin, is_member, created_at, apiKeyId FROM users WHERE username = ?').get([req.user.username]);
        if (!user) return res.status(404).json({ error: 'User not found' });
        return res.json({
            id: user.id,
            username: user.username,
            isAdmin: !!user.is_admin,
            isMember: !!user.is_member,
            createdAt: user.created_at,
            hasApiKey: !!user.apiKeyId
        });
    } catch (err) {
        console.error('Account data error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
router.delete('/delete-account', authMiddleware, async (req, res) => {
    try {
        const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([req.user.username]);
        if (!user) return res.status(404).json({ error: 'User not found' });
        await db.prepare('DELETE FROM users WHERE id = ?').run([user.id]);
        await db.prepare('DELETE FROM user_moderation WHERE user_id = ?').run([user.id]);
        return res.json({ message: 'Account deleted successfully' });
    } catch (err) {
        console.error('Delete account error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
router.get('/accstatus-cuser', async (req, res) => {
  const username = req.query.username;
  if (!username) return res.status(400).json({ error: "Missing username param" });

  try {
    // Check in the database first
    const user = await db.prepare('SELECT id, username FROM users WHERE username = ?').get([username]);
    if (!user) return res.json(null);

    // Check if there's any recent moderation
    const ban = await db.prepare(
      'SELECT * FROM user_moderation WHERE user_id = ? ORDER BY moderated_at DESC LIMIT 1'
    ).get([user.id]);

    // Helper to safely read & update the authedPlayersFile
    const updatePlayersFile = async (updateFn) => {
      return new Promise((resolve) => {
        lockFile(() => {
          fs.readFile(authedPlayersFilePath, 'utf8', (err, data) => {
            if (err) {
              console.error('Error reading file:', err);
              lockfile.unlock(authedPlayersFilePath + '.lock', () => {});
              return resolve();
            }

            let players;
            try {
              players = JSON.parse(data);
              if (!Array.isArray(players)) throw new Error('players is not an array');
            } catch (parseErr) {
              console.error('Error parsing players file:', parseErr);
              lockfile.unlock(authedPlayersFilePath + '.lock', () => {});
              return resolve();
            }

            updateFn(players);

            fs.writeFile(authedPlayersFilePath, JSON.stringify(players, null, 2), 'utf8', (err) => {
              if (err) console.error('Error writing to file:', err);
              lockfile.unlock(authedPlayersFilePath + '.lock', (unlockErr) => {
                if (unlockErr) console.error('Error unlocking file:', unlockErr);
                resolve();
              });
            });
          });
        });
      });
    };

    if (!ban) {
      // No ban → mark accountStatus ok
      await updatePlayersFile((players) => {
        const player = players.find(p => p.username === username);
        if (player) player.moderation = { accountStatus: 'ok' };
      });
      return res.json({ accountStatus: 'ok' });
    }

    // Format moderated time
    const moderatedTimePDT = new Date(ban.moderated_at).toLocaleString('en-US', {
      timeZone: 'America/Los_Angeles',
      month: 'short',
      day: 'numeric',
      year: 'numeric',
      hour: 'numeric',
      minute: '2-digit'
    });

    // Update player file with moderation info
    await updatePlayersFile((players) => {
      const player = players.find(p => p.username === username);
      if (player) {
        player.moderation = {
          accountStatus: 'moderated',
          banInfo: {
            type: ban.type,
            moderatedTimePDT,
            modNote: ban.mod_note,
            incriminatory: JSON.parse(ban.incriminatory || 'null')
          }
        };
      }
    });

    return res.status(403).json({
      accountStatus: 'moderated',
      banInfo: {
        type: ban.type,
        moderatedTimePDT,
        modNote: ban.mod_note,
        incriminatory: JSON.parse(ban.incriminatory || 'null')
      }
    });

  } catch (err) {
    console.error('Account status error:', err);
    res.status(500).json({ error: 'Server error' });
  }
});




// Get or generate API key for authenticated user
router.get('/get-key', authMiddleware, async (req, res) => {
  try {
    // Get the authenticated user using MariaDB syntax
    const user = await db.prepare('SELECT id, username, apiKeyId FROM users WHERE username = ?').get([req.user.username]);
    if (!user) {
      return res.status(404).json({ error: 'User not found' });
    }

    // If user already has an API key, fetch and return it
    if (user.apiKeyId) {
      const existingKey = await db.prepare('SELECT api_key FROM api_keys WHERE id = ?').get([user.apiKeyId]);
      if (existingKey) {
        return res.json({ 
          apiKey: existingKey.api_key,
          message: 'Existing API key retrieved'
        });
      }
      // If we get here, the apiKeyId exists in user but not in api_keys table
      // Clear it so we can generate a new one
      await db.prepare('UPDATE users SET apiKeyId = NULL WHERE id = ?').run([user.id]);
    }

    // Generate new API key
    const id = uuidv4();
    const apiKey = genCustomUUID();
    const name = `${user.username}'s API Key`;
    
    // Insert the new API key
    await db.prepare(`
      INSERT INTO api_keys (id, name, api_key, hourly_limit)
      VALUES (?, ?, ?, ?)
    `).run([id, name, apiKey, 100]); // Default hourly limit of 100

    // Update user with API key reference
    await db.prepare('UPDATE users SET apiKeyId = ? WHERE id = ?').run([id, user.id]);

    res.json({ 
      apiKey,
      message: 'New API key generated'
    });
  } catch (err) {
    console.error('Error managing API key:', err);
    res.status(500).json({ error: 'Failed to manage API key' });
  }
});
router.post('/profile', async (req, res) => {
  try {
    // Get token from cookies
    const token = req.cookies.userToken;
    
    if (!token) {
      return res.json({ username: null });
    }

    // Verify the token
    jwt.verify(token, process.env.JWT_SECRET, (err, decoded) => {
      if (err) {
        return res.json({ username: null });
      }
      
      return res.json({ username: decoded.username });
    });
  } catch (err) {
    console.error('Error in /profile:', err);
    return res.json({ username: null });
  }
});
// Get API key usage stats
router.get('/api-usage', authMiddleware, async (req, res) => {
  try {
    // Get the authenticated user using MariaDB syntax
    const user = await db.prepare('SELECT id, username, apiKeyId FROM users WHERE username = ?').get([req.user.username]);
    if (!user) {
      return res.status(404).json({ error: 'User not found' });
    }

    if (!user.apiKeyId) {
      return res.status(404).json({ error: 'No API key found for this user' });
    }

    // Get the API key from api_keys table
    const apiKey = await db.prepare('SELECT api_key FROM api_keys WHERE id = ?').get([user.apiKeyId]);
    if (!apiKey) {
      // Clean up the invalid reference
      await db.prepare('UPDATE users SET apiKeyId = NULL WHERE id = ?').run([user.id]);
      return res.status(404).json({ error: 'API key not found' });
    }

    // Get usage stats
    const usageStats = await ApiKeyManager.getKeyUsage(apiKey.api_key);
    if (usageStats.error) {
      return res.status(500).json({ error: usageStats.error });
    }

    res.json(usageStats);
  } catch (err) {
    console.error('Error getting API key usage:', err);
    res.status(500).json({ error: 'Failed to get API key usage' });
  }
});

const DYNMAP_URL = 'http://localhost:8123/up/world/world/';
/*router.get('/players', async (req, res) => {
    try {
        const response = await fetch(DYNMAP_URL);
        const data = await response.json();
        res.json(data.players || []);
    } catch (error) {
        res.status(500).json({ error: 'Failed to fetch player data' });
    }
});
*/
router.get('/logged-in', permissiveAuthMiddleware, async (req, res) => {
  // Check if auth Middleware set req.user
  if (req.user === null || req.user === undefined) return res.json({ loggedIn: false });
  const username = req.user.username;
  if (!username) return res.json({ loggedIn: false });
  const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
  if (!user) return res.json({ loggedIn: false });
  else return res.json({ loggedIn: true });
});
router.get('/proj/allowed', permissiveAuthMiddleware, async (req, res) => {
  if (req.user === null || req.user === undefined) return res.status(401).json({ allowed: false });
  const username = req.user.username;
  if (!username) return res.status(401).json({ allowed: false });
  const allowed = await db.prepare('SELECT is_projallowed FROM users WHERE username = ?').get([username]);
  if (!allowed === 0) return res.status(403).json({ allowed: false });
  else return res.json({ allowed: true });
});
router.get('/plex/allowed', permissiveAuthMiddleware, async (req, res) => {
  if (req.user === null || req.user === undefined) return res.status(401).json({ allowed: false });
  const username = req.user.username;
  if (!username) return res.status(401).json({ allowed: false });
  const allowed = await db.prepare('SELECT is_plexallowed FROM users WHERE username = ?').get([username]);
  if (!allowed === 0) return res.status(403).json({ allowed: false });
  else return res.json({ allowed: true });
});
router.post('/refresh-token', (req, res) => {
    const token = req.cookies.refreshToken; // Get refresh token from cookie
    if (!token) {
        console.error("Refresh token not found in cookies");
        return res.status(401).json({ error: 'No refresh token' });
    }
    try {
        const decoded = jwt.verify(token, process.env.JWT_SECRET);
        const newAccessToken = jwt.sign({ username: decoded.username }, process.env.JWT_SECRET, { expiresIn: '15m' });

        res.cookie('authToken', newAccessToken, { 
            httpOnly: false, 
            secure: true,        
            sameSite: 'none', 
            // domain: '.kransmp.go.ro',
            maxAge: 900000,    // 15 minutes
            path: '/'
        });
        return res.json({ accessToken: newAccessToken });
    } catch (err) {
        console.error("Refresh token error:", err);
        return res.status(403).json({ error: 'Invalid refresh token' });
    }
});
router.post('/logout', (req, res) => {
    res.clearCookie('userToken', { httpOnly: true, secure: true, sameSite: 'Strict' });
    return res.json({ message: 'Logged out successfully' });
});
router.post('/purge-logout', (req, res) => {
    res.clearCookie('userToken', { httpOnly: true, secure: true, sameSite: 'Strict' });
    res.clearCookie('authToken', { httpOnly: false, secure: true, sameSite: 'strict' });
    res.clearCookie('refreshToken', { httpOnly: false, secure: true, sameSite: 'strict' });
    return res.json({ message: 'Logged out successfully' });
});
router.post('/startserver', authMiddleware, commandController.startServer.bind(commandController));
const uploadDir = '/plugins/SkinsRestorer/skins';
const logPath = '/home/useradmin/api/uploadlogs.json';
// multer png upload

router.get('/fetch-worlds', async (req, res) => {
  try {
    const worldsDir = '/usr/share/nginx/html/raindrippy/content';
    
    // Read all files in the folder
    const files = fs.readdirSync(worldsDir);

    // Filter only the ones matching "Season_(number).zip"
    const seasonFiles = files.filter(f => /^Season_\d+\.zip$/.test(f));

    // Map to the JSON objects you want
    const result = seasonFiles.map(f => {
      // extract the number from "Season_(number).zip"
      const match = f.match(/^Season_(\d+)\.zip$/);
      const number = match ? match[1] : '?';
      
      return {
        path: `raindrippy/content/${f}`, // relative path
        name: `Season ${number}`
      };
    });

    res.json(result);

  } catch (err) {
    console.error(err);
    res.status(500).json({ error: 'Something went wrong' });
  }
});

router.post('/uploadskinfile', skinUpload.single('file'), async (req, res) => {
  const { username, password } = req.body;
  if (!username || !password || !req.file) {
    if (req.file) fs.unlink(req.file.path, () => {});
    return res.status(400).json({ error: 'missing info or no file' });
  }

  // Replace User.findOne with MariaDB syntax
  const user = await db.prepare('SELECT id, username, password_hash FROM users WHERE username = ?').get([username]);
  if (!user || !(await bcrypt.compare(password, user.password_hash))) {
    fs.unlink(req.file.path, () => {});
    return res.status(401).json({ error: 'Bad creds' });
  }

  const filename = path.basename(req.file.filename);

  // log it
  const logs = fs.existsSync(logPath)
    ? JSON.parse(fs.readFileSync(logPath, 'utf8'))
    : [];
  logs.push({ timestamp: new Date().toISOString(), username, file: filename });
  fs.writeFileSync(logPath, JSON.stringify(logs, null, 2));

  res.json({ message: 'skinfile uploaded', file: filename });
});

router.post('/delskin', async (req, res) => {
  const { username, password, filename } = req.body;
  const logPath = '/home/useradmin/api/uploadlogs.json';
  const skinPath = path.join('/plugins/SkinsRestorer/skins', filename);

  if (!username || !password || !filename) {
    return res.status(400).json({ error: 'missing fields vro' });
  }

  try {
    // Replace User.findOne with MariaDB syntax
    const user = await db.prepare('SELECT id, username, password_hash FROM users WHERE username = ?').get([username]);
    if (!user || !(await bcrypt.compare(password, user.password_hash))) {
      return res.status(401).json({ error: 'invalid credentials' });
    }

    // read log
    let logs = fs.existsSync(logPath)
      ? JSON.parse(fs.readFileSync(logPath, 'utf8'))
      : [];

    const match = logs.find(log => log.username === username && log.file === filename);
    if (!match) return res.status(403).json({ error: 'unauthorized file' });

    // delete file
    if (fs.existsSync(skinPath)) {
      fs.unlinkSync(skinPath);
    }

    // update log
    logs = logs.filter(log => !(log.username === username && log.file === filename));
    fs.writeFileSync(logPath, JSON.stringify(logs, null, 2));

    return res.json({ message: 'skin file deleted' });
  } catch (err) {
    console.error('error in delskin', err);
    return res.status(500).json({ error: 'server fried rn' });
  }
});

router.get('/userskins/:username', (req, res) => {
    const username = req.params.username;

    if (!username) {
        return res.status(400).json({ error: 'Username is required' });
    }

    try {
        let logs = [];
        if (fs.existsSync(logPath)) {
            logs = JSON.parse(fs.readFileSync(logPath, 'utf8'));
        }

        const userFiles = logs
            .filter(entry => entry.username === username)
            .map(entry => entry.file);

        return res.json({ files: userFiles });
    } catch (err) {
        console.error('Error in /userskins:', err);
        return res.status(500).json({ error: 'Internal server error' });
    }
});
// Delete account (admin-only)
router.delete('/admin/delete-user', authMiddleware, async (req, res) => {
  try {
    if (!req.user?.isAdmin) return res.status(403).json({ message: 'Access denied' });
    const { username } = req.body;
    if (!username) return res.status(400).json({ error: 'Missing username' });
    const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
    if (!user) return res.status(404).json({ error: 'User not found' });
    await db.prepare('DELETE FROM users WHERE id = ?').run([user.id]);
    await db.prepare('DELETE FROM user_moderation WHERE user_id = ?').run([user.id]);
    return res.json({ message: 'User deleted successfully' });
  } catch (err) {
    console.error('Delete user error:', err);
    return res.status(500).json({ error: 'Server error' });
  }
});
// Ban/unban/terminate (admin-only)
router.post('/admin/moderate', authMiddleware, async (req, res) => {
  try {
    if (!req.user?.isAdmin) return res.status(403).json({ message: 'Access denied' });

    const { username, type, modNote, incriminatory, poisonHWID, makeAdmin } = req.body;
    if (!username || !type) return res.status(400).json({ error: 'Missing required fields' });

    const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
    if (!user) return res.status(404).json({ error: 'User not found' });

    const now = new Date().toISOString().slice(0, 19).replace('T', ' ');

    // -----------------------
    // Handle moderation logic
    // -----------------------
    let expiration = null;
    let message = '';
    let moderationData = {};

    if (type === 'poison') {
      message = 'Poison ban applied successfully';
      moderationData = { type: 'poison', modNote: modNote || '', moderatedAt: now, incriminatory: incriminatory ? JSON.stringify(incriminatory) : null };
      
    } else if (type === 'perm') {
      message = 'Permanent ban (termination) applied successfully';
      moderationData = { type: 'perm', modNote: modNote || '', moderatedAt: now, incriminatory: incriminatory ? JSON.stringify(incriminatory) : null };
      
    } else if (['1d', '3d', '7d', '14d'].includes(type)) {
      const msMap = { 
        '1d': 24 * 60 * 60 * 1000, 
        '3d': 3 * 24 * 60 * 60 * 1000, 
        '7d': 7 * 24 * 60 * 60 * 1000, 
        '14d': 14 * 24 * 60 * 60 * 1000 
      };
      
      expiration = new Date(Date.now() + msMap[type]).toISOString().slice(0, 19).replace('T', ' ');
      message = `${type} temporary ban applied successfully (expires: ${expiration})`;
      moderationData = { type, modNote: modNote || '', moderatedAt: now, expiresAt: expiration, incriminatory: incriminatory ? JSON.stringify(incriminatory) : null };
      
    } else {
      return res.status(400).json({ error: `Invalid moderation type: ${type}` });
    }

    // -----------------------
    // Handle moderation in the database
    // -----------------------
    const existingModeration = await db.prepare('SELECT id FROM user_moderation WHERE user_id = ?').get([user.id]);
    if (existingModeration) {
      await db.prepare(`
        UPDATE user_moderation 
        SET type = ?, mod_note = ?, moderated_at = ?, expires_at = ?, created_by = ?, incriminatory = ?
        WHERE user_id = ?
      `).run([moderationData.type, moderationData.modNote, moderationData.moderatedAt, moderationData.expiresAt || null, req.user.username, moderationData.incriminatory || null, user.id]);
    } else {
      await db.prepare(`
        INSERT INTO user_moderation (user_id, type, mod_note, moderated_at, expires_at, created_by, incriminatory)
        VALUES (?, ?, ?, ?, ?, ?, ?)
      `).run([user.id, moderationData.type, moderationData.modNote, moderationData.moderatedAt, moderationData.expiresAt || null, req.user.username, moderationData.incriminatory || null]);
    }

    // -----------------------
    // Update moderation in the local file with locking
    // -----------------------
    lockFile(() => {
      fs.readFile(authedPlayersFilePath, 'utf8', (err, data) => {
        if (err) return console.error('Error reading file:', err);

        let players = [];
        try {
          const parsed = JSON.parse(data);
          players = Object.values(parsed);
        } catch (e) {
          return console.error('Invalid JSON format in players file:', e);
        }
        let player = players.find(p => p.username === username);
        if (player) {
          player.moderation = moderationData; // Update moderation data
          fs.writeFile(authedPlayersFilePath, JSON.stringify(players, null, 2), 'utf8', (err) => {
            if (err) return console.error('Error writing to file:', err);
            lockfile.unlock(authedPlayersFilePath + '.lock', (unlockErr) => {
              if (unlockErr) return console.error('Error unlocking file:', unlockErr);
              console.log('Updated moderation for user:', username);
            });
          });
        } else {
          lockfile.unlock(authedPlayersFilePath + '.lock', (unlockErr) => {
            if (unlockErr) return console.error('Error unlocking file:', unlockErr);
          });
        }
      });
    });

    // -----------------------
    // Handle admin toggle (independent of moderation)
    // -----------------------
    if (typeof makeAdmin === 'boolean') {
      await db.prepare('UPDATE users SET is_admin = ? WHERE id = ?').run([makeAdmin ? 1 : 0, user.id]);
      console.log(`Admin status changed for ${username}: ${makeAdmin}`);
    }

    console.log(`Moderation applied: ${message}`);
    return res.json({ message });

  } catch (err) {
    console.error('Moderation error:', err);
    console.error('Error details:', {
      username: req.body.username,
      type: req.body.type,
      error: err.message,
      stack: err.stack
    });
    res.status(500).json({ error: 'Server error', details: err.message });
  }
});
// Unban / clear moderation (admin-only)
router.post('/admin/unban', authMiddleware, async (req, res) => {
    try {
        if (!req.user?.isAdmin) return res.status(403).json({ message: 'Access denied' });

        const { username } = req.body;
        if (!username) return res.status(400).json({ error: 'Missing username' });

        const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
        if (!user) return res.status(404).json({ error: 'User not found' });

        await db.prepare('DELETE FROM user_moderation WHERE user_id = ? AND type != "poison"').run([user.id]);

        return res.json({ message: 'User unbanned / moderation cleared' });
    } catch (err) {
        console.error('Unban error:', err);
        res.status(500).json({ error: 'Server error' });
    }
});
// List moderations (admin-only)
router.get('/api/moderation-list', authMiddleware, async (req, res) => {
  try {
    if (!req.user?.isAdmin) return res.status(403).json({ message: 'Access denied' });

    const rows = await db.prepare(`
      SELECT 
        u.username,
        m.type,
        m.mod_note,
        m.moderated_at,
        m.created_by
      FROM user_moderation m
      JOIN users u ON m.user_id = u.id
      ORDER BY m.moderated_at DESC
      LIMIT 100
    `).all();

    res.json(rows);
  } catch (err) {
    console.error('Moderation list error:', err);
    res.status(500).json({ error: 'Server error' });
  }
});
  router.get('/admin/list-users', authMiddleware, async (req, res) => {
    try {
        // Make sure requester is admin
        if (!req.user?.isAdmin) return res.status(403).json({ message: 'Access denied' });

        const search = req.query.search ? `%${req.query.search}%` : '%';
        const page = parseInt(req.query.page) || 1;
        const limit = 10;
        const offset = (page - 1) * limit;

        // Query all users matching search
        const users = await db.prepare(`
            SELECT u.id, u.username, u.is_admin, u.is_member, u.is_projallowed, u.is_plexallowed,
                m.type AS moderation_type
            FROM users u
            LEFT JOIN (
                SELECT user_id, type
                FROM user_moderation
                WHERE moderated_at = (
                    SELECT MAX(moderated_at)
                    FROM user_moderation m2
                    WHERE m2.user_id = user_moderation.user_id
                )
            ) m ON u.id = m.user_id
            WHERE u.username LIKE ?
            ORDER BY u.username ASC
            LIMIT ? OFFSET ?
        `).all([search, limit, offset]);

        // Map to format your admin page expects
        const result = users.map(u => ({
            username: u.username,
            accountStatus: u.moderation_type ? 'moderated' : 'ok',
            isAdmin: !!u.is_admin,
            isMember: !!u.is_member,
            isProjAllowed: !!u.is_projallowed,
            isPlexAllowed: !!u.is_plexallowed
        }));

        res.json(result);

    } catch (err) {
        console.error('Error fetching user list:', err);
        res.status(500).json({ error: 'Server error' });
    }
});
router.patch('/admin/update-member-status', authMiddleware, async (req, res) => {
    try {
        const { username, isMember } = req.body;
        
        if (!username || typeof isMember !== 'boolean') {
            return res.status(400).json({ error: 'Invalid request body. Username and isMember are required.' });
        }

        // Update user member status
        const result = await db.prepare(`
            UPDATE users SET is_member = ? WHERE username = ?
        `).run([isMember ? 1 : 0, username]);

        if (result.changes === 0) {
            return res.status(404).json({ error: 'User not found' });
        }

        res.json({ message: 'User member status updated successfully' });
    } catch (err) {
        console.error('Error updating member status:', err);
        res.status(500).json({ error: 'Server error' });
    }
});
router.patch('/admin/update-proj-status', authMiddleware, async (req, res) => {
    try {
      const { username, isAllowed } = req.body;

      if (!username || typeof isAllowed !== 'boolean') return res.status(400).json({ error: 'Invalid request body. Username and isAllowed are required.'});
      const result = await db.prepare(`
        UPDATE users SET is_projallowed = ? WHERE username = ?
      `).run([isAllowed ? 1 : 0, username]); 

      if (result.changes === 0) return res.status(404).json({ error: 'User not found' });
      res.json({ message: 'User project status updated successfully' });
    } catch (err) {
      console.error('Error updating project status:', err);
        res.status(500).json({ error: 'Server error' });
    }
});
router.patch('/admin/update-plex-status', authMiddleware, async (req, res) => {
  try {
      const { username, isAllowed } = req.body;

      if (!username || typeof isAllowed !== 'boolean') return res.status(400).json({ error: 'Invalid request body. Username and isAllowed are required.'});
      const result = await db.prepare(`
        UPDATE users SET is_plexallowed = ? WHERE username = ?
      `).run([isAllowed ? 1 : 0, username]); 

      if (result.changes === 0) return res.status(404).json({ error: 'User not found' });
      res.json({ message: 'User Plex status updated successfully' });
    } catch (err) {
      console.error('Error updating Plex status:', err);
        res.status(500).json({ error: 'Server error' });
    }
});
router.post('/admin/gen-pwd-reset', authMiddleware, async (req, res) => {
    try {
        if (!req.user?.isAdmin) return res.status(403).json({ message: 'Access denied' });

        const { username } = req.body;
        if (!username) return res.status(400).json({ error: 'Missing username' });
        const user = await db.prepare('SELECT id FROM users WHERE username = ?').get([username]);
        if (!user) return res.status(404).json({ error: 'User not found' });
        const sessionToken = crypto.randomBytes(32).toString('hex');
        const expiresAt = new Date(Date.now() + 15 * 60 * 1000).toISOString().slice(0, 19).replace('T', ' '); // 15 minutes from now
        await db.prepare('INSERT INTO password_reset_sessions (username, session_token, expires_at) VALUES (?, ?, ?)').run([user.id, sessionToken, expiresAt]);
        return res.json({ 
            message: 'Password reset session created',
            resetSession: sessionToken,
            expiresAt
        });
    } catch (err) {
        console.error('Generate password reset error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
router.post('/reset-password', async (req, res) => {
    const { username, oldPassword, newPassword } = req.body;
    if (!username || !oldPassword || !newPassword) {
        return res.status(400).json({ error: 'Missing fields' });
    }
    try {
        const user = await db.prepare('SELECT id, password_hash FROM users WHERE username = ?').get([username]);
        if (!user) return res.status(404).json({ error: 'User not found' });
        const valid = await bcrypt.compare(oldPassword, user.password_hash);
        if (!valid) return res.status(401).json({ error: 'Invalid credentials' });
        const hashedPassword = await bcrypt.hash(newPassword, 10);
        await db.prepare('UPDATE users SET password_hash = ? WHERE id = ?').run([hashedPassword, user.id]);
        return res.json({ message: 'Password reset successfully' });
    } catch (err) {
        console.error('Reset password error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
router.post('/forgot-password', async (req, res) => {
    const { newPassword, resetSession } = req.body;
    if (!newPassword || !resetSession) {
        return res.status(400).json({ error: 'Missing fields' });
    }
    try {
        const session = await db.prepare('SELECT username, expires_at FROM password_reset_sessions WHERE session_token = ?').get([resetSession]);
        if (!session) return res.status(400).json({ error: 'Invalid session token' });
        if (new Date(session.expires_at) < new Date()) {
            await db.prepare('DELETE FROM password_reset_sessions WHERE session_token = ?').run([resetSession]);
            return res.status(400).json({ error: 'Session token expired' });
        }
        const user = await db.prepare('SELECT id FROM users WHERE id = ?').get([session.username]);
        if (!user) return res.status(404).json({ error: 'User not found' });
        const hashedPassword = await bcrypt.hash(newPassword, 10);
        await db.prepare('UPDATE users SET password_hash = ? WHERE id = ?').run([hashedPassword, user.id]);
        await db.prepare('DELETE FROM password_reset_sessions WHERE session_token = ?').run([resetSession]);
        return res.json({ message: 'Password changed successfully' });
    } catch (err) {
        console.error('Forgot password error:', err);
        return res.status(500).json({ error: 'Server error' });
    }
});
module.exports = router;
