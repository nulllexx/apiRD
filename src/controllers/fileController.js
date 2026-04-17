const fs = require('fs');
const path = require('path');
const multer = require('multer');

const upload = multer({ dest: 'C:/server/media' });

class FileController {
    async getFiles(req, res) {
        const directoryPath = 'C:/server/media';

        fs.readdir(directoryPath, (err, files) => {
            if (err) {
                return res.status(500).json({ message: 'Unable to scan files', error: err });
            }

            res.status(200).json({ files });
        });
    }

    async uploadFile(req, res) {
        if (!req.file) {
            return res.status(400).json({ message: 'No file uploaded' });
        }

        res.status(200).json({ message: 'File uploaded successfully', file: req.file });
    }
}

module.exports = FileController;