//! Database side of access control: reading an account's grants, the one-time
//! migration off `users.is_admin`, and the access audit trail.

use sqlx::{MySql, MySqlConnection, MySqlPool};

use super::{Permission, PermissionSet, SUPERUSER_ROLE};
use crate::error::AppError;

/// `meta` key recording that `users.is_admin` has been carried over into
/// roles. Once set, the old column is never read again.
const MIGRATED_KEY: &str = "rbac_migrated_from_is_admin";

/// The role names an account holds and the permissions they add up to.
///
/// Always read fresh: a role revoked a moment ago must stop working on the very
/// next request, which is the main thing this design buys over a JWT claim.
pub async fn grants_for<'e, E>(db: E, user_id: &str) -> Result<(Vec<String>, PermissionSet), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT r.name, rp.permission
           FROM user_roles ur
           JOIN roles r ON r.id = ur.role_id
           LEFT JOIN role_permissions rp ON rp.role_id = r.id
           WHERE ur.user_id = ?
           ORDER BY r.name"#,
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;

    let (set, unknown) =
        PermissionSet::from_grants(rows.iter().map(|(r, p)| (r.as_str(), p.as_deref())));
    if !unknown.is_empty() {
        log::warn!("account {user_id} holds unrecognised permissions {unknown:?}; ignoring them");
    }

    let mut roles: Vec<String> = rows.into_iter().map(|(r, _)| r).collect();
    roles.dedup();
    Ok((roles, set))
}

/// A role and what it grants.
#[derive(Debug, Clone)]
pub struct RoleRecord {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub is_system: bool,
    pub permissions: PermissionSet,
}

/// Looks a role up by name, with its permissions.
pub async fn load_role(conn: &mut MySqlConnection, name: &str) -> Result<Option<RoleRecord>, sqlx::Error> {
    let row: Option<(i64, String, String, bool)> =
        sqlx::query_as("SELECT id, name, description, is_system FROM roles WHERE name = ?")
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;

    let Some((id, name, description, is_system)) = row else {
        return Ok(None);
    };

    let perms: Vec<(String,)> =
        sqlx::query_as("SELECT permission FROM role_permissions WHERE role_id = ?")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;

    let (permissions, _) = PermissionSet::from_grants(
        std::iter::once((name.as_str(), None))
            .chain(perms.iter().map(|(p,)| (name.as_str(), Some(p.as_str())))),
    );

    Ok(Some(RoleRecord { id, name, description, is_system, permissions }))
}

/// How many accounts hold the superuser role, locking those rows so two
/// concurrent demotions cannot both see "one other admin remains" and leave
/// none. Must be called inside a transaction.
pub async fn count_superusers_for_update(conn: &mut MySqlConnection) -> Result<i64, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"SELECT ur.user_id FROM user_roles ur
           JOIN roles r ON r.id = ur.role_id
           WHERE r.name = ?
           FOR UPDATE"#,
    )
    .bind(SUPERUSER_ROLE)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.len() as i64)
}

/// Refuses unless `actor` holds everything the target account holds.
///
/// Guards the actions that would otherwise be a way up: resetting an
/// administrator's password is taking over their account; banning or deleting
/// one removes the people who could undo it.
pub async fn ensure_can_act_on(
    pool: &MySqlPool,
    actor: &PermissionSet,
    target_user_id: &str,
) -> Result<(), AppError> {
    let (_, target) = grants_for(pool, target_user_id).await?;
    if actor.covers(&target) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "That account has access you do not hold, so you cannot act on it".to_string(),
        ))
    }
}

/// Appends one line to the access audit trail.
///
/// No foreign keys, for the reason `console_audit` gives: who changed whose
/// access must outlive both accounts.
pub async fn record<'e, E>(
    db: E,
    actor: &str,
    action: &str,
    target: &str,
    detail: Option<&str>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    sqlx::query(
        "INSERT INTO access_audit (at, actor, action, target, detail) VALUES (NOW(3), ?, ?, ?, ?)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(db)
    .await?;
    Ok(())
}

/// Grants or revokes the superuser role, refusing to remove the last holder.
/// Returns whether anything changed. Runs inside the caller's transaction.
pub async fn set_superuser(
    conn: &mut MySqlConnection,
    actor: &str,
    target_user_id: &str,
    target_username: &str,
    grant: bool,
) -> Result<bool, AppError> {
    let admin = load_role(conn, SUPERUSER_ROLE)
        .await?
        .ok_or_else(|| AppError::Internal("The admin role is missing".to_string()))?;

    let holders = count_superusers_for_update(conn).await?;
    let held: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM user_roles WHERE user_id = ? AND role_id = ?")
            .bind(target_user_id)
            .bind(admin.id)
            .fetch_optional(&mut *conn)
            .await?;

    match (grant, held.is_some()) {
        (true, true) | (false, false) => Ok(false),
        (true, false) => {
            sqlx::query("INSERT INTO user_roles (user_id, role_id, granted_by) VALUES (?, ?, ?)")
                .bind(target_user_id)
                .bind(admin.id)
                .bind(actor)
                .execute(&mut *conn)
                .await?;
            record(&mut *conn, actor, "user.roles", target_username, Some("+admin")).await?;
            Ok(true)
        }
        (false, true) => {
            if holders <= 1 {
                return Err(AppError::Conflict(
                    "This is the last administrator; grant the admin role to someone else first"
                        .to_string(),
                ));
            }
            sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
                .bind(target_user_id)
                .bind(admin.id)
                .execute(&mut *conn)
                .await?;
            record(&mut *conn, actor, "user.roles", target_username, Some("-admin")).await?;
            Ok(true)
        }
    }
}

/// Creates the built-in roles and, once, moves every `is_admin = 1` account
/// into the admin role.
///
/// Runs on every start-up. The admin role is re-created if missing; the
/// carry-over from `is_admin` runs only until it has succeeded once, so an
/// admin demoted afterwards is not promoted again by a stale column — and a
/// starter role an operator deletes stays deleted.
pub async fn migrate(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT IGNORE INTO roles (name, description, is_system) VALUES (?, ?, TRUE)")
        .bind(SUPERUSER_ROLE)
        .bind("Full access to everything, including permissions added later")
        .execute(pool)
        .await?;

    let done: Option<(String,)> = sqlx::query_as("SELECT `key` FROM meta WHERE `key` = ?")
        .bind(MIGRATED_KEY)
        .fetch_optional(pool)
        .await?;
    if done.is_some() {
        return Ok(());
    }

    let mut tx = pool.begin().await?;

    let moved = sqlx::query(
        r#"INSERT IGNORE INTO user_roles (user_id, role_id, granted_by)
           SELECT u.id, r.id, 'migration'
           FROM users u JOIN roles r ON r.name = ?
           WHERE u.is_admin = 1"#,
    )
    .bind(SUPERUSER_ROLE)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // A starting point for delegating moderation without handing out the
    // console. Seeded once, here, so it can be edited or deleted for good.
    sqlx::query("INSERT IGNORE INTO roles (name, description, is_system) VALUES ('moderator', ?, FALSE)")
        .bind("Reviews and moderates accounts")
        .execute(&mut *tx)
        .await?;
    for p in [Permission::PanelAccess, Permission::UsersView, Permission::UsersModerate] {
        sqlx::query(
            r#"INSERT IGNORE INTO role_permissions (role_id, permission)
               SELECT id, ? FROM roles WHERE name = 'moderator'"#,
        )
        .bind(p.as_str())
        .execute(&mut *tx)
        .await?;
    }

    record(
        &mut *tx,
        "system",
        "migrate",
        "users.is_admin",
        Some(&format!("{moved} account(s) moved into the admin role")),
    )
    .await?;

    sqlx::query("INSERT INTO meta (`key`, `value`) VALUES (?, NOW())")
        .bind(MIGRATED_KEY)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    log::info!("Roles migration: {moved} admin account(s) moved into the '{SUPERUSER_ROLE}' role");
    Ok(())
}
