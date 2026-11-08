//! Managing roles and who holds them.
//!
//! * `/api/access/me` — any signed-in account: its own roles and permissions,
//!   so a page can hide what the caller cannot use. Hiding is cosmetic; every
//!   route enforces its own permission.
//! * `/api/admin/access/*` — the role editor. Gated by `roles.manage`, and
//!   within that by the no-escalation rule (`PermissionSet::covers`): you can
//!   only create, edit, delete, grant or revoke roles whose permissions you
//!   hold yourself, and only change the roles of accounts that do not outrank
//!   you. The built-in admin role cannot be edited or deleted, only an admin
//!   can grant or revoke it, and the last one cannot be removed.
//!
//! Every change is written to `access_audit` in the same transaction as the
//! change itself.

use std::collections::BTreeSet;

use actix_web::{web, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::access::store::{self, RoleRecord};
use crate::access::{
    parse_permissions, perm, permission_names, valid_role_name, Permission, PermissionSet,
    SUPERUSER_ROLE,
};
use crate::error::AppError;
use crate::middleware::permission::{Principal, Require};
use crate::AppState;

type Manager = Require<perm::RolesManage>;

const AUDIT_PAGE_SIZE: u32 = 50;
const MAX_DESCRIPTION: usize = 255;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn not_covered(actor: &PermissionSet, wanted: &PermissionSet, what: &str) -> AppError {
    let missing: Vec<&str> = actor.missing_from(wanted).into_iter().map(Permission::as_str).collect();
    if wanted.is_superuser() || missing.is_empty() {
        AppError::Forbidden(format!("Only an admin can {what}"))
    } else {
        AppError::Forbidden(format!(
            "You cannot {what}: it involves permissions you do not hold ({})",
            missing.join(", ")
        ))
    }
}

fn role_json(role: &RoleRecord, members: i64, actor: &PermissionSet) -> serde_json::Value {
    json!({
        "name": role.name,
        "description": role.description,
        "system": role.is_system,
        "superuser": role.permissions.is_superuser(),
        "permissions": if role.permissions.is_superuser() {
            Vec::new()
        } else {
            permission_names(&role.permissions)
        },
        "members": members,
        // Whether this caller could grant, revoke, edit or delete it. The
        // panel greys out what it cannot do; the routes refuse regardless.
        "manageable": actor.covers(&role.permissions),
    })
}

fn clean_description(d: Option<&str>) -> Result<String, AppError> {
    let d = d.unwrap_or("").trim();
    if d.chars().count() > MAX_DESCRIPTION {
        return Err(AppError::BadRequest(format!(
            "Description is longer than {MAX_DESCRIPTION} characters"
        )));
    }
    Ok(d.to_string())
}

fn diff_detail(added: &[String], removed: &[String]) -> String {
    added
        .iter()
        .map(|a| format!("+{a}"))
        .chain(removed.iter().map(|r| format!("-{r}")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn perm_strings(set: &BTreeSet<Permission>) -> Vec<String> {
    set.iter().map(|p| p.as_str().to_string()).collect()
}

// ---------------------------------------------------------------------------
// Me
// ---------------------------------------------------------------------------

/// GET /api/access/me
async fn me(principal: Principal) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(json!({
        "username": principal.username,
        "roles": principal.roles,
        "superuser": principal.is_superuser(),
        "permissions": permission_names(&principal.permissions),
    })))
}

// ---------------------------------------------------------------------------
// Catalog and roles
// ---------------------------------------------------------------------------

/// GET /api/admin/access/permissions — every permission, for the role editor.
async fn list_permissions(actor: Manager) -> Result<HttpResponse, AppError> {
    let list: Vec<_> = Permission::ALL
        .iter()
        .map(|p| {
            json!({
                "key": p.as_str(),
                "group": p.group(),
                "description": p.description(),
                "held": actor.has(*p),
            })
        })
        .collect();
    Ok(HttpResponse::Ok().json(list))
}

/// GET /api/admin/access/roles
async fn list_roles(state: web::Data<AppState>, actor: Manager) -> Result<HttpResponse, AppError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT r.name, (SELECT COUNT(*) FROM user_roles ur WHERE ur.role_id = r.id)
           FROM roles r
           ORDER BY r.is_system DESC, r.name"#,
    )
    .fetch_all(&state.pool)
    .await?;

    let mut conn = state.pool.acquire().await?;
    let mut out = Vec::with_capacity(rows.len());
    for (name, members) in rows {
        if let Some(role) = store::load_role(&mut conn, &name).await? {
            out.push(role_json(&role, members, &actor.permissions));
        }
    }
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleBody {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

/// POST /api/admin/access/roles
async fn create_role(
    state: web::Data<AppState>,
    actor: Manager,
    body: web::Json<CreateRoleBody>,
) -> Result<HttpResponse, AppError> {
    let name = body.name.as_deref().unwrap_or("").trim().to_string();
    if !valid_role_name(&name) {
        return Err(AppError::BadRequest(
            "Role names are 2-32 lowercase letters, digits, '-' or '_'".to_string(),
        ));
    }
    let description = clean_description(body.description.as_deref())?;
    let perms = parse_permissions(&body.permissions).map_err(AppError::BadRequest)?;

    let wanted = PermissionSet::from_permissions(perms.iter().copied());
    if !actor.permissions.covers(&wanted) {
        return Err(not_covered(&actor.permissions, &wanted, "create this role"));
    }

    let mut tx = state.pool.begin().await?;
    if store::load_role(&mut tx, &name).await?.is_some() {
        return Err(AppError::Conflict(format!("A role named {name} already exists")));
    }

    let id = sqlx::query("INSERT INTO roles (name, description) VALUES (?, ?)")
        .bind(&name)
        .bind(&description)
        .execute(&mut *tx)
        .await?
        .last_insert_id() as i64;

    for p in &perms {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES (?, ?)")
            .bind(id)
            .bind(p.as_str())
            .execute(&mut *tx)
            .await?;
    }

    store::record(
        &mut *tx,
        &actor.username,
        "role.create",
        &name,
        Some(&perm_strings(&perms).join(", ")),
    )
    .await?;
    tx.commit().await?;

    log::info!("{} created role {}", actor.username, name);
    Ok(HttpResponse::Created().json(json!({ "message": "Role created", "name": name })))
}

#[derive(Debug, Deserialize)]
pub struct UpdateRoleBody {
    description: Option<String>,
    permissions: Option<Vec<String>>,
}

/// PATCH /api/admin/access/roles/{name}
async fn update_role(
    state: web::Data<AppState>,
    actor: Manager,
    path: web::Path<String>,
    body: web::Json<UpdateRoleBody>,
) -> Result<HttpResponse, AppError> {
    let name = path.into_inner();
    let new_perms = match &body.permissions {
        Some(list) => Some(parse_permissions(list).map_err(AppError::BadRequest)?),
        None => None,
    };
    let description = match &body.description {
        Some(d) => Some(clean_description(Some(d))?),
        None => None,
    };

    let mut tx = state.pool.begin().await?;
    let role = store::load_role(&mut tx, &name)
        .await?
        .ok_or_else(|| AppError::NotFound("Role not found".to_string()))?;

    if role.is_system {
        return Err(AppError::Forbidden("Built-in roles cannot be edited".to_string()));
    }
    if !actor.permissions.covers(&role.permissions) {
        return Err(not_covered(&actor.permissions, &role.permissions, "edit this role"));
    }

    let mut detail = Vec::new();

    if let Some(d) = &description {
        if *d != role.description {
            sqlx::query("UPDATE roles SET description = ? WHERE id = ?")
                .bind(d)
                .bind(role.id)
                .execute(&mut *tx)
                .await?;
            detail.push("description changed".to_string());
        }
    }

    if let Some(perms) = &new_perms {
        let wanted = PermissionSet::from_permissions(perms.iter().copied());
        if !actor.permissions.covers(&wanted) {
            return Err(not_covered(&actor.permissions, &wanted, "give this role those permissions"));
        }

        let before: BTreeSet<Permission> = role.permissions.effective().into_iter().collect();
        let added: Vec<String> = perm_strings(&perms.difference(&before).copied().collect());
        let removed: Vec<String> = perm_strings(&before.difference(perms).copied().collect());

        if !added.is_empty() || !removed.is_empty() {
            sqlx::query("DELETE FROM role_permissions WHERE role_id = ?")
                .bind(role.id)
                .execute(&mut *tx)
                .await?;
            for p in perms {
                sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES (?, ?)")
                    .bind(role.id)
                    .bind(p.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            detail.push(diff_detail(&added, &removed));
        }
    }

    if !detail.is_empty() {
        store::record(&mut *tx, &actor.username, "role.update", &name, Some(&detail.join("; ")))
            .await?;
    }
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Role updated" })))
}

/// DELETE /api/admin/access/roles/{name}
async fn delete_role(
    state: web::Data<AppState>,
    actor: Manager,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let name = path.into_inner();

    let mut tx = state.pool.begin().await?;
    let role = store::load_role(&mut tx, &name)
        .await?
        .ok_or_else(|| AppError::NotFound("Role not found".to_string()))?;

    if role.is_system {
        return Err(AppError::Forbidden("Built-in roles cannot be deleted".to_string()));
    }
    if !actor.permissions.covers(&role.permissions) {
        return Err(not_covered(&actor.permissions, &role.permissions, "delete this role"));
    }

    let (members,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_roles WHERE role_id = ?")
        .bind(role.id)
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query("DELETE FROM roles WHERE id = ?")
        .bind(role.id)
        .execute(&mut *tx)
        .await?;

    store::record(
        &mut *tx,
        &actor.username,
        "role.delete",
        &name,
        Some(&format!("removed from {members} account(s)")),
    )
    .await?;
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Role deleted", "removedFrom": members })))
}

/// GET /api/admin/access/roles/{name}/members
async fn role_members(
    state: web::Data<AppState>,
    _actor: Manager,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let name = path.into_inner();
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT u.username, CAST(ur.granted_at AS CHAR), ur.granted_by
           FROM user_roles ur
           JOIN roles r ON r.id = ur.role_id
           JOIN users u ON u.id = ur.user_id
           WHERE r.name = ?
           ORDER BY u.username
           LIMIT 500"#,
    )
    .bind(&name)
    .fetch_all(&state.pool)
    .await?;

    let members: Vec<_> = rows
        .into_iter()
        .map(|(username, granted_at, granted_by)| {
            json!({ "username": username, "grantedAt": granted_at, "grantedBy": granted_by })
        })
        .collect();
    Ok(HttpResponse::Ok().json(members))
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

async fn find_user(pool: &sqlx::MySqlPool, username: &str) -> Result<(String, String), AppError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, username FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(pool)
            .await?;
    row.ok_or_else(|| AppError::NotFound("User not found".to_string()))
}

/// GET /api/admin/access/users/{username}
async fn user_access(
    state: web::Data<AppState>,
    _actor: Manager,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let (id, username) = find_user(&state.pool, &path.into_inner()).await?;
    let (roles, perms) = store::grants_for(&state.pool, &id).await?;
    Ok(HttpResponse::Ok().json(json!({
        "username": username,
        "roles": roles,
        "superuser": perms.is_superuser(),
        "permissions": permission_names(&perms),
    })))
}

#[derive(Debug, Deserialize)]
pub struct SetRolesBody {
    roles: Option<Vec<String>>,
}

/// PUT /api/admin/access/users/{username}/roles — set the exact list of roles.
async fn set_user_roles(
    state: web::Data<AppState>,
    actor: Manager,
    path: web::Path<String>,
    body: web::Json<SetRolesBody>,
) -> Result<HttpResponse, AppError> {
    let wanted_names: BTreeSet<String> = body
        .roles
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Missing roles".to_string()))?
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();

    let (user_id, username) = find_user(&state.pool, &path.into_inner()).await?;

    // You may not rearrange the roles of someone who outranks you, even to
    // take away something you could hand out yourself.
    store::ensure_can_act_on(&state.pool, &actor.permissions, &user_id).await?;

    let mut tx = state.pool.begin().await?;

    let (current, _) = store::grants_for(&mut *tx, &user_id).await?;
    let current: BTreeSet<String> = current.into_iter().collect();

    let added: Vec<String> = wanted_names.difference(&current).cloned().collect();
    let removed: Vec<String> = current.difference(&wanted_names).cloned().collect();

    if added.is_empty() && removed.is_empty() {
        return Ok(HttpResponse::Ok().json(json!({ "message": "No change", "roles": current })));
    }

    let mut changes = Vec::with_capacity(added.len() + removed.len());
    for name in added.iter().chain(removed.iter()) {
        let role = store::load_role(&mut tx, name)
            .await?
            .ok_or_else(|| AppError::BadRequest(format!("Unknown role: {name}")))?;
        if !actor.permissions.covers(&role.permissions) {
            let verb = if added.contains(name) { "grant" } else { "revoke" };
            return Err(not_covered(
                &actor.permissions,
                &role.permissions,
                &format!("{verb} the {name} role"),
            ));
        }
        changes.push((role, added.contains(name)));
    }

    if removed.iter().any(|r| r == SUPERUSER_ROLE)
        && store::count_superusers_for_update(&mut tx).await? <= 1
    {
        return Err(AppError::Conflict(
            "This is the last administrator; grant the admin role to someone else first"
                .to_string(),
        ));
    }

    for (role, grant) in &changes {
        if *grant {
            sqlx::query("INSERT INTO user_roles (user_id, role_id, granted_by) VALUES (?, ?, ?)")
                .bind(&user_id)
                .bind(role.id)
                .bind(&actor.username)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM user_roles WHERE user_id = ? AND role_id = ?")
                .bind(&user_id)
                .bind(role.id)
                .execute(&mut *tx)
                .await?;
        }
    }

    let detail = diff_detail(&added, &removed);
    store::record(&mut *tx, &actor.username, "user.roles", &username, Some(&detail)).await?;
    tx.commit().await?;

    log::info!("{} changed roles of {}: {}", actor.username, username, detail);
    Ok(HttpResponse::Ok().json(json!({ "message": "Roles updated", "roles": wanted_names })))
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    page: Option<u32>,
}

/// GET /api/admin/access/audit?page=N — every access change, newest first.
async fn audit(
    state: web::Data<AppState>,
    _actor: Manager,
    query: web::Query<AuditQuery>,
) -> Result<HttpResponse, AppError> {
    let page = query.page.unwrap_or(1).max(1);
    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT CAST(at AS CHAR), actor, action, target, detail
           FROM access_audit
           ORDER BY id DESC
           LIMIT ? OFFSET ?"#,
    )
    .bind(AUDIT_PAGE_SIZE)
    .bind((page - 1) * AUDIT_PAGE_SIZE)
    .fetch_all(&state.pool)
    .await?;

    let entries: Vec<_> = rows
        .into_iter()
        .map(|(at, actor, action, target, detail)| {
            json!({ "at": at, "actor": actor, "action": action, "target": target, "detail": detail })
        })
        .collect();
    Ok(HttpResponse::Ok().json(json!({ "page": page, "pageSize": AUDIT_PAGE_SIZE, "entries": entries })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/access/me", web::get().to(me)).service(
        web::scope("/admin/access")
            .route("/permissions", web::get().to(list_permissions))
            .route("/roles", web::get().to(list_roles))
            .route("/roles", web::post().to(create_role))
            .route("/roles/{name}", web::patch().to(update_role))
            .route("/roles/{name}", web::delete().to(delete_role))
            .route("/roles/{name}/members", web::get().to(role_members))
            .route("/users/{username}", web::get().to(user_access))
            .route("/users/{username}/roles", web::put().to(set_user_roles))
            .route("/audit", web::get().to(audit)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_detail_lists_grants_then_revocations() {
        let d = diff_detail(&["moderator".into()], &["admin".into(), "support".into()]);
        assert_eq!(d, "+moderator -admin -support");
    }

    #[test]
    fn descriptions_are_trimmed_and_capped() {
        assert_eq!(clean_description(Some("  hi  ")).unwrap(), "hi");
        assert_eq!(clean_description(None).unwrap(), "");
        assert!(clean_description(Some(&"x".repeat(MAX_DESCRIPTION + 1))).is_err());
    }

    #[test]
    fn the_refusal_names_what_the_actor_lacks() {
        let actor = PermissionSet::from_permissions([Permission::UsersView]);
        let wanted =
            PermissionSet::from_permissions([Permission::UsersView, Permission::ConsolePower]);
        match not_covered(&actor, &wanted, "create this role") {
            AppError::Forbidden(msg) => assert!(msg.contains("console.power"), "{msg}"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
        match not_covered(&actor, &PermissionSet::superuser(), "grant the admin role") {
            AppError::Forbidden(msg) => assert!(msg.contains("Only an admin"), "{msg}"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }
}
