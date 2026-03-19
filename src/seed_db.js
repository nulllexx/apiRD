const { db, query, queryOne } = require('./db');

function formatDate(date) {
    return date.toISOString().slice(0, 19).replace('T', ' ');
}

async function seedDatabase(override = false) {
    try {
        console.log('Starting database seeding...');

        // Check existing data
        const existingComponents = await queryOne('SELECT COUNT(*) as c FROM components');
        console.log(`Current component count: ${existingComponents.c}`);
        
        if (existingComponents.c > 0 && override) {
            console.log('Clearing existing components...');
            await query('DELETE FROM components');
        }
        
        const now = formatDate(new Date());
            
            // Define components to seed
            const components = [
                { id: 'login', name: 'Login', status: 'operational' },
                { id: 'developer-login', name: 'Developer Login', status: 'operational' },
                { id: 'public-api', name: 'Public API', status: 'operational' },
                { id: 'minecraft-server', name: 'Minecraft Server', status: 'operational' },
                { id: 'storage', name: 'Storage', status: 'operational' },
                { id: 'private-api', name: 'Private API', status: 'operational' }
            ];

            // Insert components
            for (const component of components) {
                await query(
                    'INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)',
                    [component.id, component.name, component.status, now]
                );
                console.log(`Created component: ${component.name}`);
            }

            // Verify seeding
            const finalCount = await queryOne('SELECT COUNT(*) as c FROM components');

        // Create a sample incident if none exist
        const existingIncidents = await queryOne('SELECT COUNT(*) as c FROM incidents');
        console.log(`Current incident count: ${existingIncidents.c}`);
        
        if (existingIncidents.c > 0 && override) {
            console.log('Clearing existing incidents...');
            await query('DELETE FROM incident_status_history');
            await query('DELETE FROM incidents');
        }

        if (existingIncidents.c === 0 || override) {
            const incidentId = 'sample-incident-001';
            
            // Create sample incident
            await query(`
                INSERT INTO incidents (
                    id, title, impact, status, status_text, 
                    status_updated_at, started_at, created_by, created_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            `, [
                incidentId,
                'Sample Incident',
                'minimal_outage',
                'resolved',
                'This is a sample incident',
                now,
                now,
                'system',
                now
            ]);

            // Add status history
            await query(`
                INSERT INTO incident_status_history (
                    incident_id, status, status_text, status_updated_at
                ) VALUES 
                (?, 'investigating', 'Investigation started', ?),
                (?, 'identified', 'Issue identified', ?),
                (?, 'monitoring', 'Monitoring resolution', ?),
                (?, 'resolved', 'Issue resolved', ?)
            `, [
                incidentId, formatDate(new Date()),
                incidentId, formatDate(new Date(Date.now() + 5000)),
                incidentId, formatDate(new Date(Date.now() + 10000)),
                incidentId, formatDate(new Date(Date.now() + 15000))
            ]);

            console.log('Created sample incident with status history');
        }

        console.log('Seeding completed successfully');
    } catch (err) {
        console.error('Error seeding database:', err);
        throw err;
    } finally {
        await db.close();
    }
}

// Run the seeding with override
const override = process.argv.includes('--override');
seedDatabase(override).catch(err => {
    console.error('Failed to seed database:', err);
    process.exit(1);
});