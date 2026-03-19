const express = require('express');
const router = express.Router();
const { apiKeyAuth } = require('../middleware/apiKeyAuth');
const authMiddleware = require('../middleware/auth');
const FileController = require('../controllers/fileController');
const fileController = new FileController();
const multer = require('multer');
const { exec } = require('child_process');
const upload = multer({ dest: 'C:/server/media' });
const CommandController = require('../controllers/overviewController');
const commandController = new CommandController();
// Apply API key authentication to all routes in this router
router.use(apiKeyAuth);

// Example protected endpoints

router.get('/playercount', (req, res) => {
    res.json({ players: playerCount, maxPlayers: maxPlayers });
});


router.post('/hash', (req, res) => {
    const { password } = req.body;
    if (!password) {
        return res.status(400).json({ error: 'Password is required' });
    }
    const hashed = hashPassword(password);
    res.json({ output: hashed });
});
router.post('/files', authMiddleware, upload.single('file'), fileController.uploadFile.bind(fileController));
router.get('/files', fileController.getFiles.bind(fileController));
// Endpoint moved to /api/v1/playercount
router.get('/serverrunning', commandController.serverRunning.bind(commandController));
router.post('/restart', authMiddleware, (req, res) => {
    exec(`docker compose down minecraft`, (error) => {
        if (error) {
            console.error(`Error shutting down docker container: ${error.message}`);
            return res.status(500).send("Failed to shut down Minecraft server.");
        }
        console.log("Server stopped successfully.");
    });
    exec(`docker compose up -d minecraft`, (error) => {
        if (error) {
            console.error(`Error starting server: ${error.message}`);
            return res.status(500).send("Failed to start Minecraft server.");
        }
        console.log("Server started successfully.");
        res.send("Minecraft server restarted successfully.");
    });
});
router.post('/startserver', authMiddleware, commandController.startServer.bind(commandController));
module.exports = router;
