use actix_web::web;

mod access;
mod account;
mod admin;
mod common;
mod dashboard;
mod games;
mod password;
mod session;
mod skins;
mod wiki;

use access::*;
use account::*;
use admin::*;
use games::*;
use password::*;
use session::*;
use skins::*;
use wiki::*;

pub use dashboard::{serve_dashboard, serve_polls, serve_rdadmin};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Auth routes are registered directly (no sub-scope) because in the
    // original Node.js they are mounted at /api (the parent scope).
    cfg.route("/register", web::post().to(register))
        .route("/login", web::post().to(login))
        .route("/v-creds", web::post().to(validate_credentials))
        .route("/update-admin-status", web::post().to(update_admin_status))
        .route("/validate", web::get().to(validate))
        .route("/account-status", web::get().to(account_status))
        .route("/account-data", web::get().to(account_data))
        .route("/delete-account", web::delete().to(delete_account))
        .route("/accstatus-cuser", web::get().to(accstatus_cuser))
        .route("/get-key", web::get().to(get_key))
        .route("/profile", web::post().to(profile))
        .route("/api-usage", web::get().to(api_usage))
        .route("/logged-in", web::get().to(logged_in))
        .route("/proj/allowed", web::get().to(proj_allowed))
        .route("/getGames", web::get().to(get_games))
        .route("/plex/allowed", web::get().to(plex_allowed))
        .route("/refresh-token", web::post().to(refresh_token))
        .route("/logout", web::post().to(logout))
        .route("/purge-logout", web::post().to(purge_logout))
        .route("/startserver", web::post().to(start_server))
        .route("/fetch-worlds", web::get().to(fetch_worlds))
        .route("/uploadskinfile", web::post().to(upload_skin_file))
        .route("/delskin", web::post().to(delete_skin))
        .route("/userskins/{username}", web::get().to(user_skins))
        .route("/admin/delete-user", web::delete().to(admin_delete_user))
        .route("/admin/moderate", web::post().to(admin_moderate))
        .route("/admin/unban", web::post().to(admin_unban))
        .route("/api/moderation-list", web::get().to(moderation_list))
        .route("/admin/list-users", web::get().to(admin_list_users))
        .route(
            "/admin/update-member-status",
            web::patch().to(admin_update_member_status),
        )
        .route(
            "/admin/update-proj-status",
            web::patch().to(admin_update_proj_status),
        )
        .route(
            "/admin/update-plex-status",
            web::patch().to(admin_update_plex_status),
        )
        .route(
            "/admin/update-og-status",
            web::patch().to(admin_update_og_status),
        )
        .route("/admin/gen-pwd-reset", web::post().to(admin_gen_pwd_reset))
        .route("/reset-password", web::post().to(reset_password))
        .route("/forgot-password", web::post().to(forgot_password))
        .route("/history/wiki/edit", web::patch().to(history_wiki_edit))
        .route("/history/wiki/view", web::get().to(history_wiki_view));
}
