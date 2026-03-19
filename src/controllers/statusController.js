// src/controllers/statusController.js
const { db } = require('../db');
const { v4: uuidv4 } = require('uuid');

class StatusController {
  // Helper function to format time to PDT
  formatTimeToPDT(isoString) {
    const date = new Date(isoString);
    // Convert to PDT (UTC-7)
    const pdtTime = new Date(date.getTime() - (7 * 60 * 60 * 1000));
    const hours = pdtTime.getUTCHours();
    const minutes = pdtTime.getUTCMinutes();
    const ampm = hours >= 12 ? 'PM' : 'AM';
    const formattedHours = hours % 12 || 12;
    return `${formattedHours}:${minutes.toString().padStart(2, '0')} ${ampm} (PDT)`;
  }

  // public status: components + active incidents + meta
  async getStatus(req, res) {
    try {
      const components = await db.prepare('SELECT * FROM components').all();
      const incidents = await db.prepare(`
        SELECT * FROM incidents
        WHERE status != 'resolved'
        ORDER BY started_at DESC
      `).all();

      // Add status history to each incident
      for (let incident of incidents) {
        const statusHistory = await db.prepare(`
          SELECT status, status_text, status_updated_at 
          FROM incident_status_history 
          WHERE incident_id = ? 
          ORDER BY status_updated_at ASC
        `).all(incident.id);
        
        incident.history_status = statusHistory.map(item => ({
          status: item.status,
          context: item.status_text,
          time: this.formatTimeToPDT(item.status_updated_at)
        }));
      }

      // Initialize or update the generated_at timestamp
      const now = new Date().toISOString().slice(0, 19).replace('T', ' ');;
      await db.prepare('INSERT INTO meta (`key`, `value`) VALUES (?, ?) ON DUPLICATE KEY UPDATE `value` = ?')
        .run(['generated_at', now, now]);
      
      const meta = { generated_at: now };

      res.json({ components, incidents, meta });
    } catch (err) {
      console.error('getStatus', err);
      res.status(500).json({ error: 'server error' });
    }
  }

  // list incidents (including resolved)
  async listIncidents(req, res) {
    try {
      const incidents = await db.prepare('SELECT * FROM incidents ORDER BY started_at DESC').all();
      
      // Add status history to each incident
      for (let incident of incidents) {
        const statusHistory = await db.prepare(`
          SELECT status, status_text, status_updated_at 
          FROM incident_status_history 
          WHERE incident_id = ? 
          ORDER BY status_updated_at ASC
        `).all(incident.id);
        
        incident.history_status = statusHistory.map(item => ({
          status: item.status,
          context: item.status_text,
          time: this.formatTimeToPDT(item.status_updated_at)
        }));
      }
      
      res.json(incidents);
    } catch (err) {
      console.error('listIncidents', err);
      res.status(500).json({ error: 'server error' });
    }
  }

  // get single incident with updates
  async getIncident(req, res) {
    try {
      const id = req.params.id;
      const incident = await db.prepare('SELECT * FROM incidents WHERE id = ?').get(id);
      if (!incident) return res.status(404).json({ error: 'not found' });
      
      // Get status history
      const statusHistory = await db.prepare(`
        SELECT status, status_text, status_updated_at 
        FROM incident_status_history 
        WHERE incident_id = ? 
        ORDER BY status_updated_at ASC
      `).all(id);
      
      incident.history_status = statusHistory.map(item => ({
        status: item.status,
        context: item.status_text,
        time: this.formatTimeToPDT(item.status_updated_at)
      }));
      
      const updates = await db.prepare('SELECT time, body, author FROM incident_updates WHERE incident_id = ? ORDER BY time ASC').all(id);
      res.json({ incident, updates });
    } catch (err) {
      console.error('getIncident', err);
      res.status(500).json({ error: 'server error' });
    }
  }

  // create new incident (admin only)
  async createIncident(req, res) {
  try {
    const { title, impact = 'partial_outage', status = 'investigating', started_at } = req.body;
    if (!title) return res.status(400).json({ error: 'title required' });
    const allowedImpacts = ['minimal_outage', 'partial_outage', 'full_outage'];
    if (!allowedImpacts.includes(impact)) {
      return res.status(400).json({ error: 'impact must be one of: minimal_outage, partial_outage, full_outage' });
    }

    const id = uuidv4();

    const statusText = req.body.status_text || "No context provided";

    // Get the current UTC time and format it properly for MariaDB
    const now = new Date().toISOString();
    const formattedNow = now.replace('T', ' ').slice(0, 19); // '2025-08-21 15:04:58'

    // Use formattedNow for the created_at and status_updated_at fields
    const started = started_at || formattedNow;
    const statusUpdatedAt = formattedNow;
    const createdAt = formattedNow;  // Same formatting applied to created_at

    await db.prepare(`
      INSERT INTO incidents (id, title, impact, status, status_text, status_updated_at, started_at, created_by, created_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
    `).run([id, title, impact, status, statusText, statusUpdatedAt, started, req.admin.username, createdAt]);

    // Insert initial status history
    await db.prepare(`
      INSERT INTO incident_status_history (incident_id, status, status_text, status_updated_at)
      VALUES (?, ?, ?, ?)
    `).run([id, status, statusText, statusUpdatedAt]);

    // optional initial update body
    if (req.body.update) {
      await db.prepare(`
        INSERT INTO incident_updates (incident_id, time, body, author)
        VALUES (?, ?, ?, ?)
      `).run([id, now, req.body.update, req.admin.username]);
    }

    // Update component statuses if provided
    if (req.body.componentUpdates && Array.isArray(req.body.componentUpdates)) {
      const allowedStatuses = ['operational', 'degraded', 'disruption'];
      for (const update of req.body.componentUpdates) {
        const { componentId, status, name } = update;
        if (!componentId || !status || !allowedStatuses.includes(status)) continue;

        const existing = await db.prepare('SELECT id FROM components WHERE id = ?').get(componentId);
        if (existing) {
          await db.prepare('UPDATE components SET status = ?, last_updated = ? WHERE id = ?')
            .run([status, formattedNow, componentId]);
        } else {
          await db.prepare('INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)')
            .run([componentId, name || componentId, status, formattedNow]);
        }

        // Track this component as affected by this incident if it's not operational
        if (status !== 'operational') {
          await db.prepare('INSERT IGNORE INTO incident_affected_components (incident_id, component_id) VALUES (?, ?)')
            .run([id, componentId]);
        }
      }
    }

    return res.status(201).json({ id });
  } catch (err) {
    console.error('createIncident', err);
    res.status(500).json({ error: 'server error' });
  }
}


  // add an update to incident (admin only)
  // add an update to incident (admin only)
 async addUpdate(req, res) {
  try {
    const id = req.params.id;
    const { body } = req.body;
    if (!body) return res.status(400).json({ error: 'body required' });
    const incident = await db.prepare('SELECT id FROM incidents WHERE id = ?').get(id);
    if (!incident) return res.status(404).json({ error: 'incident not found' });

    // MOVE nowTime declaration to the top level
    const nowTime = new Date().toISOString().slice(0, 19).replace('T', ' ');;

    // Add update
    await db.prepare(`
      INSERT INTO incident_updates (incident_id, time, body, author)
      VALUES (?, ?, ?, ?)
    `).run([id, nowTime, body, req.admin.username]);

    // update status if provided
    if (req.body.status) {
      const newStatus = req.body.status;
      const newStatusText = req.body.status_text || "No context provided";
      
      // Update incident
      await db.prepare('UPDATE incidents SET status = ?, status_text = ?, status_updated_at = ? WHERE id = ?')
        .run([newStatus, newStatusText, nowTime, id]);
      
      // Add to status history
      await db.prepare(`
        INSERT INTO incident_status_history (incident_id, status, status_text, status_updated_at)
        VALUES (?, ?, ?, ?)
      `).run([id, newStatus, newStatusText, nowTime]);
      
      if (newStatus === 'resolved') {
        // Set end time
        await db.prepare('UPDATE incidents SET ended_at = ? WHERE id = ?')
          .run([nowTime, id]);
        
        // Reset all affected components back to operational
        const affectedComponents = await db.prepare('SELECT component_id FROM incident_affected_components WHERE incident_id = ?')
          .all(id);
        
        for (const comp of affectedComponents) {
          await db.prepare('UPDATE components SET status = ?, last_updated = ? WHERE id = ?')
            .run(['operational', nowTime, comp.component_id]);
        }
        
        // Clear the affected components tracking
        await db.prepare('DELETE FROM incident_affected_components WHERE incident_id = ?')
          .run(id);
      }
    }

    // Update component statuses if provided - nowTime is now available here
    if (req.body.componentUpdates && Array.isArray(req.body.componentUpdates)) {
      const allowedStatuses = ['operational', 'degraded', 'disruption'];
      for (const update of req.body.componentUpdates) {
        const { componentId, status, name } = update;
        if (!componentId || !status || !allowedStatuses.includes(status)) continue;
        
        const existing = await db.prepare('SELECT id FROM components WHERE id = ?').get(componentId);
        if (existing) {
          await db.prepare('UPDATE components SET status = ?, last_updated = ? WHERE id = ?')
            .run([status, nowTime, componentId]);
        } else {
          await db.prepare('INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)')
            .run([componentId, name || componentId, status, nowTime]);
        }

        // Track this component as affected by this incident if it's not operational
        if (status !== 'operational') {
          await db.prepare('INSERT IGNORE INTO incident_affected_components (incident_id, component_id) VALUES (?, ?)')
            .run([id, componentId]);
        }
      }
    }

    return res.json({ ok: true });
  } catch (err) {
    console.error('addUpdate', err);
    res.status(500).json({ error: 'server error' });
  }
}

  // debug database (temp endpoint for troubleshooting)
  async debugDatabase(req, res) {
    try {
      const components = await db.prepare('SELECT * FROM components').all();
      const incidents = await db.prepare('SELECT * FROM incidents').all();
      const updates = await db.prepare('SELECT * FROM incident_updates').all();
      const statusHistory = await db.prepare('SELECT * FROM incident_status_history').all();
      const affectedComponents = await db.prepare('SELECT * FROM incident_affected_components').all();
      const meta = await db.prepare('SELECT * FROM meta').all();
      
      // Database file info
      const fs = require('fs');
      const { dbPath } = require('../db');
      let fileInfo = null;
      if (fs.existsSync(dbPath)) {
        const stats = fs.statSync(dbPath);
        fileInfo = {
          path: dbPath,
          size: stats.size,
          modified: stats.mtime,
          exists: true
        };
      } else {
        fileInfo = {
          path: dbPath,
          exists: false
        };
      }
      
      res.json({
        database: fileInfo,
        tables: {
          components: components.length,
          incidents: incidents.length,
          incident_updates: updates.length,
          incident_status_history: statusHistory.length,
          incident_affected_components: affectedComponents.length,
          meta: meta.length
        },
        data: {
          components,
          incidents: incidents.slice(0, 5), // First 5 incidents
          recent_updates: updates.slice(-10) // Last 10 updates
        }
      });
    } catch (err) {
      console.error('debugDatabase', err);
      res.status(500).json({ error: err.message });
    }
  }

  // update component status (admin only)
  async updateComponent(req, res) {
    try {
      const { id } = req.params;
      const { status } = req.body;
      if (!status) return res.status(400).json({ error: 'status required' });
      const allowedStatuses = ['operational', 'degraded', 'disruption'];
      if (!allowedStatuses.includes(status)) {
        return res.status(400).json({ error: 'status must be one of: operational, degraded, disruption' });
      }
      const now = new Date().toISOString().slice(0, 19).replace('T', ' ');;
      const existing = await db.prepare('SELECT id FROM components WHERE id = ?').get(id);
      if (existing) {
        await db.prepare('UPDATE components SET status = ?, last_updated = ? WHERE id = ?').run([status, now, id]);
      } else {
        // allow create
        const name = req.body.name || id;
        await db.prepare('INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)').run([id, name, status, now]);
      }
      return res.json({ ok: true });
    } catch (err) {
      console.error('updateComponent', err);
      res.status(500).json({ error: 'server error' });
    }
  }

  // list resolved incidents
  async listResolvedIncidents(req, res) {
    try {
      const incidents = await db.prepare("SELECT * FROM incidents WHERE status = 'resolved' ORDER BY started_at DESC").all();
      
      // Add status history to each incident
      for (let incident of incidents) {
        const statusHistory = await db.prepare(`
          SELECT status, status_text, status_updated_at 
          FROM incident_status_history 
          WHERE incident_id = ? 
          ORDER BY status_updated_at ASC
        `).all(incident.id);
        
        incident.history_status = statusHistory.map(item => ({
          status: item.status,
          context: item.status_text,
          time: this.formatTimeToPDT(item.status_updated_at)
        }));
      }
      
      res.json(incidents);
    } catch (err) {
      console.error('listResolvedIncidents', err);
      res.status(500).json({ error: 'server error' });
    }
  }
}

module.exports = StatusController;