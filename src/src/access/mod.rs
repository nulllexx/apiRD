//! Roles-based access control: the permission catalog and the rules for
//! combining and comparing grants.
//!
//! Everything in this module is pure — no database, no HTTP — so the rules
//! that decide who may do what can be tested exhaustively. Loading a caller's
//! grants from the database and turning them into an extractor lives in
//! `middleware::permission`; the API for managing roles lives in
//! `routes::roles`.
//!
//! # Model
//!
//! * A **permission** is one capability, named by a dotted string such as
//!   `console.command`. The set of permissions is fixed here, in code, so a
//!   route cannot be gated on a misspelled one.
//! * A **role** is a named bundle of permissions, stored in the `roles` and
//!   `role_permissions` tables and editable at runtime.
//! * An account holds any number of roles (`user_roles`); what it may do is the
//!   union of their permissions.
//! * The built-in [`SUPERUSER_ROLE`] holds every permission implicitly —
//!   including any added to this catalog later — and cannot be edited or
//!   deleted. It replaces the old `users.is_admin` switch.
//!
//! # No escalation
//!
//! The rule that keeps delegated administration safe is
//! [`PermissionSet::covers`]: an actor may only hand out, take away, or act
//! upon access they themselves hold. Without it, anyone allowed to manage
//! roles could grant themselves everything, and anyone allowed to reset
//! passwords could take over an administrator's account.

use std::collections::BTreeSet;
use std::fmt;

pub mod store;

/// The role that implicitly holds every permission. Created at start-up and
/// protected from edits and deletion.
pub const SUPERUSER_ROLE: &str = "admin";

/// Every capability the API gates on.
///
/// The string forms are stored in `role_permissions`, so renaming one strands
/// the existing grants — `permission_strings_are_stable` pins them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// Open the admin panel and status dashboard pages.
    PanelAccess,
    /// Browse accounts and the moderation list.
    UsersView,
    /// Ban, unban and poison accounts.
    UsersModerate,
    /// Delete accounts.
    UsersDelete,
    /// Generate password-reset links for other accounts.
    UsersResetPassword,
    /// Toggle member, OG, project and chat access on accounts.
    UsersManageAccess,
    /// Create, edit and delete roles, and assign them to accounts.
    RolesManage,
    /// Create, list and delete API keys.
    ApiKeysManage,
    /// Post incidents and change component status.
    StatusManage,
    /// Watch the server console, stats, player list and inventories.
    ConsoleView,
    /// Run arbitrary commands on the game server.
    ConsoleCommand,
    /// Start, stop and restart the game server.
    ConsolePower,
    /// Op, kick, ban and change the vitals of in-game players.
    ConsolePlayers,
    /// Browse, view and download the game server's files. Separate from
    /// `ConsoleView` because those files include world data, configs and mod
    /// settings that the console never shows.
    ConsoleFiles,
    /// Edit the game server's text config files. High trust: configs decide
    /// how the server and its mods behave. What stays locked regardless --
    /// code, op/whitelist/ban lists, RCON settings, existing secrets -- is in
    /// `console::files_write`.
    ConsoleEdit,
    /// Upload new config files and images to the game server. Replacing an
    /// existing file also needs `ConsoleEdit`.
    ConsoleUpload,
    /// Open, extend and end polls.
    PollsManage,
    /// Edit the AI group chat's pop-up prompt.
    ChatPromptManage,
    /// Edit the history wiki. OG accounts can also edit it without this.
    WikiEdit,
}

impl Permission {
    pub const ALL: [Permission; 19] = [
        Permission::PanelAccess,
        Permission::UsersView,
        Permission::UsersModerate,
        Permission::UsersDelete,
        Permission::UsersResetPassword,
        Permission::UsersManageAccess,
        Permission::RolesManage,
        Permission::ApiKeysManage,
        Permission::StatusManage,
        Permission::ConsoleView,
        Permission::ConsoleCommand,
        Permission::ConsolePower,
        Permission::ConsolePlayers,
        Permission::ConsoleFiles,
        Permission::ConsoleEdit,
        Permission::ConsoleUpload,
        Permission::PollsManage,
        Permission::ChatPromptManage,
        Permission::WikiEdit,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Permission::PanelAccess => "panel.access",
            Permission::UsersView => "users.view",
            Permission::UsersModerate => "users.moderate",
            Permission::UsersDelete => "users.delete",
            Permission::UsersResetPassword => "users.reset_password",
            Permission::UsersManageAccess => "users.manage_access",
            Permission::RolesManage => "roles.manage",
            Permission::ApiKeysManage => "api_keys.manage",
            Permission::StatusManage => "status.manage",
            Permission::ConsoleView => "console.view",
            Permission::ConsoleCommand => "console.command",
            Permission::ConsolePower => "console.power",
            Permission::ConsolePlayers => "console.players",
            Permission::ConsoleFiles => "console.files",
            Permission::ConsoleEdit => "console.edit",
            Permission::ConsoleUpload => "console.upload",
            Permission::PollsManage => "polls.manage",
            Permission::ChatPromptManage => "chat_prompt.manage",
            Permission::WikiEdit => "wiki.edit",
        }
    }

    pub fn parse(s: &str) -> Option<Permission> {
        Permission::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// One line for the role editor, saying what the permission unlocks.
    pub fn description(self) -> &'static str {
        match self {
            Permission::PanelAccess => "Open the admin panel and status dashboard",
            Permission::UsersView => "Browse accounts and the moderation list",
            Permission::UsersModerate => "Ban, unban and poison accounts",
            Permission::UsersDelete => "Delete accounts permanently",
            Permission::UsersResetPassword => "Generate password-reset links for other accounts",
            Permission::UsersManageAccess => "Toggle member, OG, project and chat access",
            Permission::RolesManage => "Create and edit roles, and assign them to accounts",
            Permission::ApiKeysManage => "Create, list and delete API keys",
            Permission::StatusManage => "Post incidents and change component status",
            Permission::ConsoleView => "Watch the server console, stats, players and inventories",
            Permission::ConsoleCommand => "Run any command on the game server",
            Permission::ConsolePower => "Start, stop and restart the game server",
            Permission::ConsolePlayers => "Op, kick, ban, heal and feed in-game players",
            Permission::ConsoleFiles => "Browse, view and download the game server's files",
            Permission::ConsoleEdit => {
                "Edit the game server's config files (high trust: configs control how mods behave)"
            }
            Permission::ConsoleUpload => "Upload new config files and images to the game server",
            Permission::PollsManage => "Open, extend and end polls",
            Permission::ChatPromptManage => "Edit the AI group chat pop-up",
            Permission::WikiEdit => "Edit the history wiki",
        }
    }

    /// Heading the role editor files the permission under.
    pub fn group(self) -> &'static str {
        match self {
            Permission::PanelAccess => "Panel",
            Permission::UsersView
            | Permission::UsersModerate
            | Permission::UsersDelete
            | Permission::UsersResetPassword
            | Permission::UsersManageAccess => "Accounts",
            Permission::RolesManage => "Roles",
            Permission::ApiKeysManage | Permission::StatusManage => "Site",
            Permission::ConsoleView
            | Permission::ConsoleCommand
            | Permission::ConsolePower
            | Permission::ConsolePlayers
            | Permission::ConsoleFiles
            | Permission::ConsoleEdit
            | Permission::ConsoleUpload => "Game server",
            Permission::PollsManage | Permission::ChatPromptManage | Permission::WikiEdit => {
                "Community"
            }
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an account may do: the union of its roles' permissions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSet {
    superuser: bool,
    granted: BTreeSet<Permission>,
}

impl PermissionSet {
    /// The set held by the superuser role.
    pub fn superuser() -> Self {
        PermissionSet { superuser: true, granted: BTreeSet::new() }
    }

    pub fn from_permissions<I: IntoIterator<Item = Permission>>(perms: I) -> Self {
        PermissionSet { superuser: false, granted: perms.into_iter().collect() }
    }

    /// Builds a set from `(role name, permission string)` rows, as the
    /// role-loading query returns them. A role with no permissions comes back
    /// with `None`. Strings this build does not recognise — a grant left behind
    /// by a newer or older version — are skipped rather than failing the
    /// request, and returned so the caller can log them.
    pub fn from_grants<'a, I>(rows: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (&'a str, Option<&'a str>)>,
    {
        let mut set = PermissionSet::default();
        let mut unknown = Vec::new();
        for (role, perm) in rows {
            if role == SUPERUSER_ROLE {
                set.superuser = true;
            }
            if let Some(perm) = perm {
                match Permission::parse(perm) {
                    Some(p) => {
                        set.granted.insert(p);
                    }
                    None => unknown.push(perm.to_string()),
                }
            }
        }
        (set, unknown)
    }

    pub fn is_superuser(&self) -> bool {
        self.superuser
    }

    pub fn has(&self, p: Permission) -> bool {
        self.superuser || self.granted.contains(&p)
    }

    /// Every permission held, superuser included, in catalog order.
    pub fn effective(&self) -> Vec<Permission> {
        Permission::ALL.into_iter().filter(|p| self.has(*p)).collect()
    }

    /// Whether `self` holds everything `other` holds.
    ///
    /// The no-escalation rule: an actor may only grant, revoke, or act upon
    /// access they hold themselves. A superuser covers everything; only a
    /// superuser covers a superuser — even one whose explicit grants are
    /// empty, since its real reach includes permissions not yet invented.
    pub fn covers(&self, other: &PermissionSet) -> bool {
        if self.superuser {
            return true;
        }
        if other.superuser {
            return false;
        }
        other.granted.is_subset(&self.granted)
    }

    /// The permissions in `other` that `self` lacks, for error messages.
    pub fn missing_from(&self, other: &PermissionSet) -> Vec<Permission> {
        if self.superuser {
            return Vec::new();
        }
        other.effective().into_iter().filter(|p| !self.has(*p)).collect()
    }
}

/// The permission strings a set holds, for JSON responses.
pub fn permission_names(set: &PermissionSet) -> Vec<&'static str> {
    set.effective().into_iter().map(Permission::as_str).collect()
}

/// A role name: lowercase letters, digits, `-` and `_`, starting with a letter
/// or digit, 2–32 characters. Kept narrow because names appear in URLs, audit
/// lines and the panel's markup.
pub fn valid_role_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (2..=32).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

/// Parses a list of permission strings from a request, refusing the whole list
/// if any entry is unknown so a typo is reported instead of silently dropped.
pub fn parse_permissions(list: &[String]) -> Result<BTreeSet<Permission>, String> {
    let mut out = BTreeSet::new();
    for s in list {
        match Permission::parse(s.trim()) {
            Some(p) => {
                out.insert(p);
            }
            None => return Err(format!("Unknown permission: {}", s)),
        }
    }
    Ok(out)
}

/// Compile-time names for permissions, so a handler can declare what it needs
/// in its signature: `actor: Require<perm::ConsoleView>`.
pub mod perm {
    use super::Permission;

    pub trait PermissionMarker {
        const PERMISSION: Permission;
    }

    macro_rules! markers {
        ($($name:ident),* $(,)?) => {
            $(
                pub struct $name;
                impl PermissionMarker for $name {
                    const PERMISSION: Permission = Permission::$name;
                }
            )*
        };
    }

    markers!(
        PanelAccess,
        UsersView,
        UsersModerate,
        UsersDelete,
        UsersResetPassword,
        UsersManageAccess,
        RolesManage,
        ApiKeysManage,
        StatusManage,
        ConsoleView,
        ConsoleCommand,
        ConsolePower,
        ConsolePlayers,
        ConsoleFiles,
        ConsoleEdit,
        ConsoleUpload,
        PollsManage,
        ChatPromptManage,
        WikiEdit,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(perms: &[Permission]) -> PermissionSet {
        PermissionSet::from_permissions(perms.iter().copied())
    }

    /// Stored grants are matched on these strings. Changing one silently
    /// revokes it from every role that held it.
    #[test]
    fn permission_strings_are_stable() {
        let strings: Vec<&str> = Permission::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(
            strings,
            [
                "panel.access",
                "users.view",
                "users.moderate",
                "users.delete",
                "users.reset_password",
                "users.manage_access",
                "roles.manage",
                "api_keys.manage",
                "status.manage",
                "console.view",
                "console.command",
                "console.power",
                "console.players",
                "console.files",
                "console.edit",
                "console.upload",
                "polls.manage",
                "chat_prompt.manage",
                "wiki.edit",
            ]
        );
    }

    #[test]
    fn every_permission_round_trips_through_its_string() {
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.as_str()), Some(p));
        }
        assert_eq!(Permission::parse("console.*"), None);
        assert_eq!(Permission::parse(""), None);
    }

    #[test]
    fn the_superuser_holds_everything_including_future_permissions() {
        let su = PermissionSet::superuser();
        for p in Permission::ALL {
            assert!(su.has(p), "superuser lacks {p}");
        }
        assert_eq!(su.effective(), Permission::ALL.to_vec());
    }

    #[test]
    fn an_empty_set_holds_nothing() {
        let none = PermissionSet::default();
        for p in Permission::ALL {
            assert!(!none.has(p));
        }
    }

    #[test]
    fn grants_from_several_roles_are_unioned() {
        let (s, unknown) = PermissionSet::from_grants([
            ("moderator", Some("users.view")),
            ("moderator", Some("users.moderate")),
            ("console", Some("console.view")),
            ("empty-role", None),
        ]);
        assert!(unknown.is_empty());
        assert!(!s.is_superuser());
        assert_eq!(
            s.effective(),
            vec![Permission::UsersView, Permission::UsersModerate, Permission::ConsoleView]
        );
    }

    #[test]
    fn holding_the_admin_role_makes_a_superuser_even_with_no_rows() {
        let (s, _) = PermissionSet::from_grants([(SUPERUSER_ROLE, None)]);
        assert!(s.is_superuser());
        assert!(s.has(Permission::ConsoleCommand));
    }

    /// A stale grant must not take a request down: it is skipped and reported.
    #[test]
    fn unknown_grant_strings_are_skipped_and_reported() {
        let (s, unknown) =
            PermissionSet::from_grants([("r", Some("users.view")), ("r", Some("launch.missiles"))]);
        assert!(s.has(Permission::UsersView));
        assert_eq!(unknown, vec!["launch.missiles".to_string()]);
    }

    #[test]
    fn covers_is_subset_for_ordinary_sets() {
        let big = set(&[Permission::UsersView, Permission::UsersModerate]);
        let small = set(&[Permission::UsersView]);
        assert!(big.covers(&small));
        assert!(!small.covers(&big));
        assert!(big.covers(&big));
        assert!(small.covers(&PermissionSet::default()));
    }

    /// The escalation this blocks: someone with every explicit permission is
    /// still not allowed to hand out, or act on, the superuser role.
    #[test]
    fn only_a_superuser_covers_a_superuser() {
        let everything = set(&Permission::ALL);
        let su = PermissionSet::superuser();
        assert!(!everything.covers(&su));
        assert!(su.covers(&everything));
        assert!(su.covers(&su));
    }

    #[test]
    fn missing_from_names_what_the_actor_lacks() {
        let actor = set(&[Permission::UsersView]);
        let target = set(&[Permission::UsersView, Permission::ConsoleCommand]);
        assert_eq!(actor.missing_from(&target), vec![Permission::ConsoleCommand]);
        assert!(PermissionSet::superuser().missing_from(&target).is_empty());
    }

    #[test]
    fn role_names_are_narrow() {
        for ok in ["admin", "moderator", "staff-2", "sr_mod", "ab"] {
            assert!(valid_role_name(ok), "{ok} should be valid");
        }
        for bad in ["", "a", "Admin", "-mod", "_mod", "mod role", "mod<script>", "é", &"a".repeat(33)] {
            assert!(!valid_role_name(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn a_typo_in_a_permission_list_rejects_the_list() {
        let ok = parse_permissions(&["users.view".into(), " console.view ".into()]).unwrap();
        assert_eq!(ok.len(), 2);
        let err = parse_permissions(&["users.view".into(), "users.veiw".into()]).unwrap_err();
        assert!(err.contains("users.veiw"));
    }

    #[test]
    fn every_permission_has_a_group_and_description() {
        for p in Permission::ALL {
            assert!(!p.group().is_empty());
            assert!(!p.description().is_empty());
        }
    }
}
