const fs = require('fs');
const path = require('path');
const { v4: uuidv4 } = require('uuid');
const { db } = require('../db');

// detect correct or legacy folder name (allow env override)
const DEFAULT_CORRECT = '/usr/share/nginx/html/raindrippy/projects/public';
const DEFAULT_LEGACY = '/usr/share/nginx/html/raindripply/projects/public';
let STORAGE_BASE = "/usr/share/nginx/html/raindrippy/projects/public";
const MAX_PROJECTS_PER_USER = 3;
const MAX_BYTES_PER_USER = 1_000_000_000; // 1 GB
const PUBLIC_URL_BASE = process.env.PUBLIC_URL_BASE || (STORAGE_BASE.includes('raindripply') ? '/raindripply/projects/public' : '/raindrippy/projects/public');
class ProjectController {
    async _getUserByReq(req) {
        if (!req.user || !req.user.username) {
            throw new Error('Unauthenticated');
        }
        const row = await db.prepare('SELECT id, username, is_projallowed FROM users WHERE username = ?').get([req.user.username]);
        if (!row) {
            throw new Error('User not found');
        }
        return row;
    }

    async _getUserUsageBytes(userId) {
        const r = await db.prepare(
            `SELECT COALESCE(SUM(f.size), 0) AS total_bytes
             FROM project_files f
             JOIN projects p ON f.project_id = p.id
             WHERE p.user_id = ?`
        ).get([userId]);
        return Number(r && r.total_bytes ? r.total_bytes : 0);
    }

    async _ensureDir(dir) {
        await fs.promises.mkdir(dir, { recursive: true });
        return dir;
    }

    async _findProjectDirForUser(username, projectId) {
        const userDir = path.join(STORAGE_BASE, username);
        try {
            const entries = await fs.promises.readdir(userDir, { withFileTypes: true });
            for (const d of entries) {
                if (!d.isDirectory()) continue;
                const metaPath = path.join(userDir, d.name, 'metadata.json');
                try {
                    const raw = await fs.promises.readFile(metaPath, 'utf8');
                    const meta = JSON.parse(raw);
                    if (meta.projectId === projectId) return path.join(userDir, d.name);
                } catch (e) {
                    // ignore unreadable metadata
                }
            }
        } catch (e) {
            // user dir does not exist or unreadable
        }
        return null;
    }

    async _nextUserProjectIndex(username) {
        const userDir = path.join(STORAGE_BASE, username);
        try {
            const entries = await fs.promises.readdir(userDir, { withFileTypes: true });
            const nums = entries
                .filter(d => d.isDirectory() && /^\d+$/.test(d.name))
                .map(d => Number(d.name));
            return nums.length === 0 ? 1 : Math.max(...nums) + 1;
        } catch (e) {
            return 1;
        }
    }

    async createProject(req, res) {
        try {
            const user = await this._getUserByReq(req);
            if (!user.is_projallowed) return res.status(403).json({ error: 'Project creation not allowed' });

            const { name, description } = req.body;
            if (!name) return res.status(400).json({ error: 'Missing project name' });

            const cnt = await db.prepare('SELECT COUNT(*) AS c FROM projects WHERE user_id = ?').get([user.id]);
            if (Number(cnt.c) >= MAX_PROJECTS_PER_USER) {
                return res.status(403).json({ error: `Max projects (${MAX_PROJECTS_PER_USER}) reached` });
            }

            const projectId = uuidv4();
            await db.prepare('INSERT INTO projects (id, user_id, name, description, created_at) VALUES (?, ?, ?, ?, ?)').run([
                projectId,
                user.id,
                name,
                description || null,
                new Date().toISOString().slice(0, 19).replace('T', ' ')
            ]);

            const userDir = await this._ensureDir(path.join(STORAGE_BASE, user.username));
            const index = await this._nextUserProjectIndex(user.username);
            const projectDir = path.join(userDir, String(index));
            await this._ensureDir(projectDir);

            const metadata = { projectId, name, description: description || null, createdAt: new Date().toISOString() };
            await fs.promises.writeFile(path.join(projectDir, 'metadata.json'), JSON.stringify(metadata, null, 2), 'utf8');

            return res.status(201).json({ id: projectId, storageIndex: index });
        } catch (err) {
            console.error('createProject', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async listProjects(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const rows = await db.prepare('SELECT id, name, description, created_at FROM projects WHERE user_id = ? ORDER BY created_at DESC').all([user.id]);
            return res.json(rows);
        } catch (err) {
            console.error('listProjects', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async getProject(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const projectId = req.params.id;
            const project = await db.prepare('SELECT id, name, description, created_at FROM projects WHERE id = ? AND user_id = ?').get([projectId, user.id]);
            if (!project) return res.status(404).json({ error: 'Project not found' });

            let files = await db.prepare('SELECT id, original_name, mime, size, uploaded_at FROM project_files WHERE project_id = ? ORDER BY uploaded_at DESC').all([projectId]);

            // Normalize BigInt sizes for JSON serialization
            files = (files || []).map(f => ({
                ...f,
                size: typeof f.size === 'bigint' ? Number(f.size) : f.size
            }));

            return res.json({ project, files });
        } catch (err) {
            console.error('getProject', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async updateProject(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const projectId = req.params.id;
            const { name, description } = req.body;
            if (!name && typeof description === 'undefined') return res.status(400).json({ error: 'Nothing to update' });

            const proj = await db.prepare('SELECT id FROM projects WHERE id = ? AND user_id = ?').get([projectId, user.id]);
            if (!proj) return res.status(404).json({ error: 'Project not found' });

            await db.prepare('UPDATE projects SET name = COALESCE(?, name), description = COALESCE(?, description) WHERE id = ?').run([name, description, projectId]);
            return res.json({ message: 'Project updated' });
        } catch (err) {
            console.error('updateProject', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async deleteProject(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const projectId = req.params.id;
            const proj = await db.prepare('SELECT id FROM projects WHERE id = ? AND user_id = ?').get([projectId, user.id]);
            if (!proj) return res.status(404).json({ error: 'Project not found' });

            // remove filesystem folder if present
            const projectDir = await this._findProjectDirForUser(user.username, projectId);
            if (projectDir && fs.existsSync(projectDir)) {
                await fs.promises.rm(projectDir, { recursive: true, force: true });
            }

            await db.prepare('DELETE FROM projects WHERE id = ?').run([projectId]);
            return res.json({ message: 'Project deleted' });
        } catch (err) {
            console.error('deleteProject', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async _moveFile(src, dest) {
        // ensure destination dir exists
        await fs.promises.mkdir(path.dirname(dest), { recursive: true });
        try {
            await fs.promises.rename(src, dest);
        } catch (err) {
            if (err && err.code === 'EXDEV') {
                // cross-device, fallback to copy + unlink
                await fs.promises.copyFile(src, dest);
                await fs.promises.unlink(src).catch(() => {});
            } else {
                throw err;
            }
        }
    }
    async replaceFile(req, res) {
    try {
      const user = await this._getUserByReq(req);
      const projectId = req.params.id;
      const fileId = req.params.fileId;

      if (!req.file) return res.status(400).json({ error: 'No file uploaded' });

      // Ensure file belongs to user's project
      const row = await db.prepare(`
        SELECT f.id AS fid, f.path AS old_path, f.size AS old_size, p.id AS pid, p.user_id
        FROM project_files f
        JOIN projects p ON f.project_id = p.id
        WHERE f.id = ? AND p.id = ? AND p.user_id = ?
      `).get([fileId, projectId, user.id]);

      if (!row) {
        // cleanup temp
        await fs.promises.unlink(req.file.path).catch(()=>{});
        return res.status(404).json({ error: 'File not found' });
      }

      // quota check: used - old + new <= MAX_BYTES_PER_USER
      const oldSize = typeof row.old_size === 'bigint' ? Number(row.old_size) : Number(row.old_size || 0);
      const newSize = Number(req.file.size || 0);
      const used = await this._getUserUsageBytes(user.id);
      if (used - oldSize + newSize > MAX_BYTES_PER_USER) {
        await fs.promises.unlink(req.file.path).catch(()=>{});
        return res.status(413).json({ error: 'Replace would exceed your 1GB quota' });
      }

      // prepare destination (same directory as old file)
      const destDir = path.dirname(row.old_path);
      await fs.promises.mkdir(destDir, { recursive: true });
      const destPath = path.join(destDir, `${Date.now()}-${Math.round(Math.random()*1e9)}-${path.basename(req.file.originalname)}`);

      // move file robustly (handles EXDEV)
      if (typeof this._moveFile === 'function') {
        await this._moveFile(req.file.path, destPath);
      } else {
        await fs.promises.rename(req.file.path, destPath).catch(async (err) => {
          if (err && err.code === 'EXDEV') {
            await fs.promises.copyFile(req.file.path, destPath);
            await fs.promises.unlink(req.file.path).catch(()=>{});
          } else throw err;
        });
      }

      // remove old file from disk
      await fs.promises.unlink(row.old_path).catch(()=>{});

      // update DB
      await db.prepare(`
        UPDATE project_files
        SET filename = ?, original_name = ?, mime = ?, size = ?, path = ?, uploaded_at = ?
        WHERE id = ?
      `).run([
        path.basename(destPath),
        req.file.originalname,
        req.file.mimetype || null,
        newSize,
        destPath,
        new Date().toISOString().slice(0,19).replace('T',' '),
        fileId
      ]);

      return res.json({ message: 'File replaced', id: fileId });
    } catch (err) {
      console.error('replaceFile', err);
      if (req.file && req.file.path) await fs.promises.unlink(req.file.path).catch(()=>{});
      return res.status(500).json({ error: 'Server error' });
    }
  }
    async uploadFile(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const projectId = req.params.id;
            if (!req.file) return res.status(400).json({ error: 'No file uploaded' });

            const projectDir = await this._findProjectDirForUser(user.username, projectId);
            if (!projectDir) {
                await fs.promises.unlink(req.file.path).catch(() => {});
                console.log('projdir: ' + projectDir);
                return res.status(404).json({ error: 'Project storage folder not found' });
            }

            const uploadedSize = Number(req.file.size || 0);
            const used = await this._getUserUsageBytes(user.id);
            if (used + uploadedSize > MAX_BYTES_PER_USER) {
                await fs.promises.unlink(req.file.path).catch(() => {});
                return res.status(413).json({ error: 'Upload would exceed your 1GB quota' });
            }

            const safeName = path.basename(req.file.originalname);
            const destPath = path.join(projectDir, `${Date.now()}-${Math.round(Math.random() * 1e9)}-${safeName}`);

            // use robust move to handle cross-device
            await this._moveFile(req.file.path, destPath);

            const fileId = uuidv4();
            await db.prepare(
                `INSERT INTO project_files (id, project_id, filename, original_name, mime, size, path, uploaded_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)`
            ).run([fileId, projectId, path.basename(destPath), req.file.originalname, req.file.mimetype || null, uploadedSize, destPath, new Date().toISOString().slice(0, 19).replace('T', ' ')]);

            return res.json({
                id: fileId,
                originalName: req.file.originalname,
                mime: req.file.mimetype,
                size: uploadedSize,
                uploadedAt: new Date().toISOString()
            });
        } catch (err) {
            console.error('uploadFile', err);
            if (req.file && req.file.path) await fs.promises.unlink(req.file.path).catch(() => {});
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async listFiles(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const projectId = req.params.id;
            const proj = await db.prepare('SELECT id FROM projects WHERE id = ? AND user_id = ?').get([projectId, user.id]);
            if (!proj) return res.status(404).json({ error: 'Project not found' });

            const files = await db.prepare('SELECT id, original_name, mime, size, uploaded_at FROM project_files WHERE project_id = ? ORDER BY uploaded_at DESC').all([projectId]);
            return res.json(files);
        } catch (err) {
            console.error('listFiles', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async downloadFile(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const fileId = req.params.fileId;
            const file = await db.prepare(
                `SELECT f.id, f.original_name, f.path
                 FROM project_files f
                 JOIN projects p ON f.project_id = p.id
                 WHERE f.id = ? AND p.user_id = ?`
            ).get([fileId, user.id]);

            if (!file) return res.status(404).json({ error: 'File not found' });
            if (!fs.existsSync(file.path)) return res.status(410).json({ error: 'File missing on server' });

            return res.download(file.path, file.original_name);
        } catch (err) {
            console.error('downloadFile', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async deleteFile(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const fileId = req.params.fileId;
            const file = await db.prepare(
                `SELECT f.id, f.path
                 FROM project_files f
                 JOIN projects p ON f.project_id = p.id
                 WHERE f.id = ? AND p.user_id = ?`
            ).get([fileId, user.id]);

            if (!file) return res.status(404).json({ error: 'File not found' });

            await fs.promises.unlink(file.path).catch(() => {});
            await db.prepare('DELETE FROM project_files WHERE id = ?').run([fileId]);

            return res.json({ message: 'File deleted' });
        } catch (err) {
            console.error('deleteFile', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }

    async getQuota(req, res) {
        try {
            const user = await this._getUserByReq(req);
            const used = await this._getUserUsageBytes(user.id);
            return res.json({
                usedBytes: used,
                maxBytes: MAX_BYTES_PER_USER,
                remainingBytes: Math.max(0, MAX_BYTES_PER_USER - used)
            });
        } catch (err) {
            console.error('getQuota', err);
            return res.status(500).json({ error: 'Server error' });
        }
    }
    async getPublicProject(req, res) {
    try {
      const user = await this._getUserByReq(req);
      const projectId = req.params.id;
      const projectDir = await this._findProjectDirForUser(user.username, projectId);
      if (!projectDir) return res.status(404).json({ error: 'Project storage folder not found' });

      const index = path.basename(projectDir);
      const url = `${PUBLIC_URL_BASE}/${encodeURIComponent(user.username)}/${encodeURIComponent(index)}/`;
      return res.json({ url });
    } catch (err) {
      console.error('getPublicProject', err);
      return res.status(500).json({ error: 'Server error' });
    }
  }

  // return public URL for a specific file inside a project
  async getPublicFile(req, res) {
    try {
      const user = await this._getUserByReq(req);
      const projectId = req.params.id;
      const fileId = req.params.fileId;

      const fileRow = await db.prepare(`
        SELECT f.filename, f.path
        FROM project_files f
        JOIN projects p ON f.project_id = p.id
        WHERE f.id = ? AND p.id = ? AND p.user_id = ?
      `).get([fileId, projectId, user.id]);

      if (!fileRow) return res.status(404).json({ error: 'File not found' });

      const projectDir = await this._findProjectDirForUser(user.username, projectId);
      if (!projectDir) return res.status(404).json({ error: 'Project storage folder not found' });

      const index = path.basename(projectDir);
      // filename stored in DB is the stored filename (basename)
      const filename = fileRow.filename;
      const url = `${PUBLIC_URL_BASE}/${encodeURIComponent(user.username)}/${encodeURIComponent(index)}/${encodeURIComponent(filename)}`;

      return res.json({ url });
    } catch (err) {
      console.error('getPublicFile', err);
      return res.status(500).json({ error: 'Server error' });
    }
  }
}

module.exports = ProjectController;