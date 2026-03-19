const express = require('express');
const multer = require('multer');
const fs = require('fs');
const path = require('path');
const ProjectController = require('../controllers/projectController');
const authMiddleware = require('../middleware/auth');

const router = express.Router();
const ctrl = new (require('../controllers/projectController'))();

// store uploads to tmp and let controller move into nginx folder
const UPLOAD_TMP = process.env.UPLOAD_TMP || path.join(__dirname, '..', '..', 'data', 'uploads');
fs.mkdirSync(UPLOAD_TMP, { recursive: true });
const upload = multer({ dest: UPLOAD_TMP });
// Projects
router.post('/', authMiddleware, ctrl.createProject.bind(ctrl));
router.get('/', authMiddleware, ctrl.listProjects.bind(ctrl));
router.get('/quota', authMiddleware, ctrl.getQuota.bind(ctrl));
router.get('/:id', authMiddleware, ctrl.getProject.bind(ctrl));
router.patch('/:id', authMiddleware, ctrl.updateProject.bind(ctrl));
router.delete('/:id', authMiddleware, ctrl.deleteProject.bind(ctrl));

// Files (field name "file")
router.post('/:id/files', authMiddleware, upload.single('file'), ctrl.uploadFile.bind(ctrl));
router.get('/:id/files', authMiddleware, ctrl.listFiles.bind(ctrl));
router.get('/:id/files/:fileId', authMiddleware, ctrl.downloadFile.bind(ctrl));
router.patch('/:id/files/:fileId', authMiddleware, upload.single('file'), ctrl.replaceFile.bind(ctrl));
router.delete('/:id/files/:fileId', authMiddleware, ctrl.deleteFile.bind(ctrl));
router.get('/:id/public', authMiddleware, ctrl.getPublicProject.bind(ctrl));
router.get('/:id/files/:fileId/public', authMiddleware, ctrl.getPublicFile.bind(ctrl));
module.exports = router;