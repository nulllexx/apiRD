const { db, query, queryOne } = require('./db');

async function queryDatabase() {
    try {
        const command = process.argv[2]?.toLowerCase();
        
        switch (command) {
            case 'components':
                const components = await query('SELECT * FROM components ORDER BY name');
                console.table(components);
                break;

            case 'incidents':
                const incidents = await query(`
                    SELECT 
                        i.*, 
                        (SELECT COUNT(*) FROM incident_status_history WHERE incident_id = i.id) as status_updates
                    FROM incidents i 
                    ORDER BY started_at DESC
                `);
                console.table(incidents);
                break;

            case 'incident':
                const incidentId = process.argv[3];
                if (!incidentId) {
                    console.error('Please provide an incident ID');
                    break;
                }
                const incident = await queryOne('SELECT * FROM incidents WHERE id = ?', [incidentId]);
                console.log('\nIncident Details:');
                console.table(incident);
                
                const history = await query(
                    'SELECT * FROM incident_status_history WHERE incident_id = ? ORDER BY status_updated_at',
                    [incidentId]
                );
                console.log('\nStatus History:');
                console.table(history);
                break;

            case 'status':
                const componentId = process.argv[3];
                if (!componentId) {
                    console.error('Please provide a component ID');
                    break;
                }
                const status = await queryOne('SELECT * FROM components WHERE id = ?', [componentId]);
                console.table(status);
                break;

            case 'delete':
                const type = process.argv[3]?.toLowerCase();
                const id = process.argv[4];
                
                if (!type || !id) {
                    console.error('Please provide both type (incident/component) and ID');
                    break;
                }

                if (type === 'incident') {
                    // Delete incident history first due to foreign key constraint
                    await query('DELETE FROM incident_status_history WHERE incident_id = ?', [id]);
                    await query('DELETE FROM incidents WHERE id = ?', [id]);
                    console.log(`Deleted incident ${id} and its history`);
                } else if (type === 'component') {
                    await query('DELETE FROM components WHERE id = ?', [id]);
                    console.log(`Deleted component ${id}`);
                } else {
                    console.error('Type must be either "incident" or "component"');
                }
                break;

            default:
                console.log(`
Available commands:
    components                      - List all components and their status
    incidents                      - List all incidents
    incident <incident_id>         - Show details of a specific incident
    status <component_id>          - Show status of a specific component
    delete incident <incident_id>  - Delete a specific incident and its history
    delete component <component_id> - Delete a specific component
                `);
        }
    } catch (err) {
        console.error('Error querying database:', err);
    } finally {
        await db.close();
    }
}

queryDatabase().catch(err => {
    console.error('Failed to query database:', err);
    process.exit(1);
});
