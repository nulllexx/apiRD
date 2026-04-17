const express = require('express');
const bodyParser = require('body-parser');
const dotenv = require('dotenv');
const cors = require('cors');
const authRoutes = require('./routes/auth');
const statusRoutes = require('./routes/status');
const apiKeysRoutes = require('./routes/apiKeys');
const apiRoutes = require('./routes/api');
const utilRoutes = require('./routes/util')
const projectsRouter = require('./routes/projects');
const path = require('path');
const cookieParser = require('cookie-parser');
const app = express();
const { db } = require('./db');
const statusMiddleware = require('./middleware/statusDashboard');
const adminMiddleware = require('./middleware/RD');
app.set('trust proxy', 1);
// Middleware
app.use(bodyParser.json());
app.use(cors({
    origin: "https://bakosmp.go.ro",
    credentials: true,
    methods: "GET,HEAD,PUT,PATCH,POST,DELETE",
    allowedHeaders: "Content-Type,Authorization",
    exposedHeaders: "Set-Cookie"
}));
app.use('/content', express.static(path.join(__dirname, '../content')));
app.use(cookieParser());

// Load environment variables
dotenv.config();
const { initDatabase } = require('./db');
const { createApiKeysTable } = require('./migrations/create_api_keys_table');

async function startServer() {
    try {
        await initDatabase();
        await createApiKeysTable();
        // Routes
        app.use('/api', authRoutes);
        app.use('/api/util', utilRoutes);
        app.use('/api/status', statusRoutes);
        app.use('/api/projects', projectsRouter);
        app.use('/api/keys', apiKeysRoutes);
        app.use('/api/v1', apiRoutes); // Protected API endpoints under /api/v1
        app.get('/dashboard.html', statusMiddleware, (req, res) => {
            res.sendFile(path.join(__dirname, 'private', 'dashboard.html'));
        });
        app.get('/rdadmin.html', adminMiddleware, (req, res) => {
            res.sendFile(path.join(__dirname, 'private', 'rdadmin.html'));
        });
        // Account for the poorly-coded redirects
        // Screw you, past me!
        app.get('/dashboard', statusMiddleware, (req, res) => {
            res.sendFile(path.join(__dirname, 'private', 'dashboard.html'));
        });
        app.get('/rdadmin', adminMiddleware, (req, res) => {
            res.sendFile(path.join(__dirname, 'private', 'rdadmin.html'));
        });
        app.disable('x-powered-by');
        // Start the HTTP server
        const PORT = process.env.PORT || 5000;
        app.listen(PORT, '0.0.0.0', () => {
            console.log(`Server is running on port ${PORT}`);
        });
    } catch (err) {
        console.error('Failed to start server:', err);
        process.exit(1);
        }
}

startServer();
setInterval(async () => {
  console.log('Running auto-unban check...');
  try {
    const now = new Date().toISOString().slice(0,19).replace('T',' ');
    const expired = await db.prepare(`
      SELECT user_id FROM user_moderation 
      WHERE expires_at IS NOT NULL AND expires_at <= ?
    `).all([now]);
    console.log(`Found ${expired.length} users to unban.`);
    console.log(expired);
    for (const row of expired) {
      await db.prepare('DELETE FROM user_moderation WHERE user_id = ? AND type != "poison"').run([row.user_id]);
      console.log(`Auto-unbanned user ID: ${row.user_id}`);
    }
  } catch (err) {
    console.error('Error auto-unbanning users:', err);
  }
}, 60*1000); // every 60 seconds
