const { exec } = require('child_process');
const fs = require('fs');
const path = require('path');
class CommandController {
    async playerCount(req, res) {
        const command = 'docker compose run minecraft /getplayers';
        exec(command, (error, stdout, stderr) => {
            if (error) {
                return res.status(500).json({ message: 'Error executing command', error });
            }
            const filePath = '/home/useradmin/Desktop/server/playercount.integer';
            fs.readFile(filePath, 'utf8', (readError, data) => {
                if (readError) {
                    return res.status(500).json({ message: 'Error reading file', readError });
                }
                const playerCount = parseInt(data, 10);
                res.status(200).json({ playerCount });
            });
        });
    }
    async serverRunning(req, res) {
        exec('docker ps --filter "name=minecraft" --format "{{.Names}}"', (error, stdout, stderr) => {
            if (error) {
                return res.status(500).json({ message: 'Error checking server status', error });
            }
            const isRunning = stdout.trim() === 'minecraft';
            res.status(200).json({ running: isRunning });
        });
    }
    async startServer(req, res) {
        exec(`docker compose up -d minecraft`, (error, stdout, stderr) => {
            if (error) {
                console.error(`Error starting server: ${error.message}`);
                return res.status(500).json({ message: 'Error starting server', error });
            }
            console.log('Server started successfully:', stdout);
            res.status(200).json({ message: 'Server started successfully' });
        });
    }
}

module.exports = CommandController;

