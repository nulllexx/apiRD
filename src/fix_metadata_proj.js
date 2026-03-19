const fs = require('fs').promises;
const fsSync = require('fs');
const path = require('path');
const { db } = require('./db'); // adjust if your db module path differs

const DEFAULT_CORRECT = process.env.PROJECT_STORAGE_BASE || '/usr/share/nginx/html/raindrippy/projects/public';
const DEFAULT_LEGACY = '/usr/share/nginx/html/raindripply/projects/public'; // legacy spelling

const TARGETS = [DEFAULT_CORRECT]; // will attempt both
const FORCE = process.argv.includes('--force'); // pass --force to overwrite existing metadata

async function nextIndexForUser(dir) {
  try {
    const entries = await fs.readdir(dir, { withFileTypes: true });
    const nums = entries.filter(d => d.isDirectory() && /^\d+$/.test(d.name)).map(d => Number(d.name));
    return nums.length === 0 ? 1 : Math.max(...nums) + 1;
  } catch (e) {
    return 1;
  }
}

(async () => {
  try {
    // read projects + owner username from DB
    const rows = await db.prepare(`
      SELECT p.id AS projectId, p.name AS projectName, p.description, p.created_at,
             u.id AS userId, u.username
      FROM projects p
      JOIN users u ON p.user_id = u.id
      ORDER BY u.username, p.created_at
    `).all();

    if (!rows || rows.length === 0) {
      console.log('No projects found in DB. Nothing to do.');
      process.exit(0);
    }

    for (const targetBase of TARGETS) {
      // ensure base exists (create if not)
      try {
        await fs.mkdir(targetBase, { recursive: true });
      } catch (e) {
        console.error(`Unable to create or access target base ${targetBase}:`, e.message);
        continue;
      }

      console.log('Using target base:', targetBase);

      const userIndex = {}; // username -> next index
      for (const r of rows) {
        const username = r.username;
        const projectId = r.projectId;
        const projectName = r.projectName || 'Untitled';
        const description = r.description || null;
        const createdAt = r.created_at || new Date().toISOString();

        const userDir = path.join(targetBase, username);
        if (!fsSync.existsSync(userDir)) {
          await fs.mkdir(userDir, { recursive: true });
          console.log('Created user dir:', userDir);
        }

        if (userIndex[username] === undefined) {
          userIndex[username] = await nextIndexForUser(userDir);
        }
        const index = userIndex[username]++;

        const projectDir = path.join(userDir, String(index));
        if (!fsSync.existsSync(projectDir)) {
          await fs.mkdir(projectDir, { recursive: true });
          console.log('Created project dir:', projectDir);
        } else {
          console.log('Project dir exists:', projectDir);
        }

        const metaPath = path.join(projectDir, 'metadata.json');
        if (!FORCE && fsSync.existsSync(metaPath)) {
          console.log('Skipping existing metadata:', metaPath);
          continue;
        }

        const metadata = {
          projectId,
          name: projectName,
          description,
          createdAt
        };

        await fs.writeFile(metaPath, JSON.stringify(metadata, null, 2), 'utf8');
        console.log('Written metadata for', projectId, '->', metaPath);
      }
    }

    console.log('Done. Verify nginx serves the target paths and fix permissions if necessary.');
    console.log('Example permission fix (on host): sudo chown -R www-data:www-data /usr/share/nginx/html/raindrippy');
  } catch (err) {
    console.error('Error:', err);
    process.exit(1);
  } finally {
    process.exit(0);
  }
})();