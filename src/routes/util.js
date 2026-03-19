const express = require('express');
const fs = require('fs');
const path = require('path');
const { hashPassword, comparePassword } = require('../utils/hash');
const router = express.Router();
const authMiddleware = require('../middleware/auth');
const { apiKeyAuth } = require('../middleware/apiKeyAuth');
const fileController = require('../controllers/fileController');
const CommandController = require('../controllers/overviewController');
const commandController = new CommandController();
const multer = require('multer');
const { exec } = require('child_process');
const upload = multer({ dest: 'C:/server/media' });
const https = require('https');
const FormData = require('form-data');
// Endpoint moved to /api/v1/check-updates
// Endpoint moved to /api/v1/games
router.post('/check-updates', async (req, res) => {
    const clientVersion = req.body.version;
    if (!clientVersion) {
        return res.status(400).json({ error: 'No version supplied in request body' });
    }

    const dataDir = '/home/useradmin/api/mainapi/src/content/Highway63';
    const versionFile = path.join(dataDir, 'Highway63.version');

    const diskVersionRaw = await fs.readFile(versionFile, 'utf8');
    const diskVersion = diskVersionRaw.trim();

    if (clientVersion === diskVersion) {
        return res.status(200).send('Latest version installed');
    } else {
        return res.status(200).json({
            update: 'available',
            url: 'https://bakosmp.go.ro/content/Highway63.exe'
        });
    }
});
router.get('/games', async (req, res) => {
    // Move the games endpoint logic here
    const dataFolder = "/home/useradmin/api/mainapi/src/data/";
    const games = [];

    fs.readdir(dataFolder, (err, files) => {
        if (err) {
            console.log(`Error reading data folder: ${err}`);
            return res.status(500).json({ error: 'Unable to read data folder' });
        }

        const gameFiles = files.reduce((acc, file) => {
            const [name, ext] = file.split('.');
            if (!acc[name]) acc[name] = {};
            acc[name][ext] = file;
            return acc;
        }, {});

        for (const [name, fileTypes] of Object.entries(gameFiles)) {
            const gameData = {
                title: fs.readFileSync(path.join(dataFolder, fileTypes.title), 'utf8').replace(/\n/g, ''),
                description: fs.readFileSync(path.join(dataFolder, fileTypes.description), 'utf8').replace(/\n/g, ''),
                image: `https://bakosmp.go.ro/data/${fileTypes.jpg}`,
                file: `https://bakosmp.go.ro/data/${fileTypes.exe}`
            };
            games.push({ [name]: gameData });
        }

        res.json(games);
    });
});
function loginToRouter(routerPasswordHash) {
  return new Promise((resolve, reject) => {
    const qs = `form=login&operation=login&password=${routerPasswordHash}`;

    const options = {
      hostname: '192.168.0.1',
      path: `/cgi-bin/luci/;stok=/login?${qs}`,
      method: 'GET',
      rejectUnauthorized: false,
      headers: {
        'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36',
        'Accept': '*/*',
        'Connection': 'close'
      }
    };
    https.get(options, res => {
      let body = '';
      res.on('data', chunk => body += chunk);
      res.on('end', () => {
        try {
          const json = JSON.parse(body);
          if (json.success && json.data?.stok) {
            resolve(json.data.stok);
          } else {
            reject(new Error('Login failed: ' + body));
          }
        } catch (e) {
          reject(e);
        }
      });
    }).on('error', reject);
  });
}

function fetchPerf(stok) {
  return new Promise((resolve, reject) => {
    const form = new FormData();
    form.append('form', 'perf');
    const options = {
      hostname: '192.168.0.1',
      path: `/cgi-bin/luci/;stok=${stok}/admin/status`,
      headers: form.getHeaders(),
      method: 'POST',
      rejectUnauthorized: false,
      headers: {
        'User-Agent': 'PostmanRuntime/7.45.0'
      }
    };

    https.get(options, res => {
      let body = '';
      res.on('data', chunk => body += chunk);
      res.on('end', () => {
        try {
          resolve(JSON.parse(body));
        } catch (e) {
          reject(e);
        }
      });
    }).on('error', reject);
  });
}
router.get('/router/perf', async (req, res) => {
  try {
    const routerPassword = "81328db1ef19a4e17b19ecff256840fce17d0cf24240b58193e1933020f4a5430bf44decd10bbc02017317de09c98f42d2fa3620098b04d80348c57a7bc84720f4a55ff1934297119355a0bf74c1366598bd8562b0ae1dc8f0b79a572daca5e40f01f6938583ca885fa1b952308a360fc4fe4c8db83d4e9c2d94deb5f05f789e";
    if (!routerPassword) {
      return res.status(500).json({ error: 'No router password' });
    }

    const stok = await loginToRouter(routerPassword);
    const perf = await fetchPerf(stok);
    res.json(perf);
  } catch (err) {
    console.error('Router perf error:', err);
    res.status(500).json({ error: 'Failed to fetch router performance data' });
  }
});
module.exports = router;