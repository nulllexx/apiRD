// src/routes/status.js
const express = require('express');
const StatusController = require('../controllers/statusController');
const adminAuth = require('../middleware/adminAuth');

const router = express.Router();
const controller = new StatusController();

// Public
router.get('/status', controller.getStatus.bind(controller));
router.get('/incidents', controller.listIncidents.bind(controller));
router.get('/incidents/:id', controller.getIncident.bind(controller));
router.get('/incidentHistory', controller.listResolvedIncidents.bind(controller));
router.get('/debug', controller.debugDatabase.bind(controller));
// Admin-only
router.post('/incidents', adminAuth, controller.createIncident.bind(controller));
router.post('/incidents/:id/updates', adminAuth, controller.addUpdate.bind(controller));
router.patch('/components/:id', adminAuth, controller.updateComponent.bind(controller));

module.exports = router;
