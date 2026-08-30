//! Polls: an admin asks a question, eligible accounts answer it anonymously.
//!
//! Two surfaces, deliberately separated into two scopes so the management
//! verbs never share a path shape with `/{id}` and no route-ordering trap can
//! develop:
//!
//!   * `/api/admin/polls` — create, browse live and past, end early. Gated by
//!     [`AdminUser`], which re-checks the database rather than trusting a token
//!     whose expiry is never validated.
//!   * `/api/polls` — what the player-facing frontend calls: which polls I may
//!     answer, what one says, whether I may vote on it, and casting a vote.
//!
//! **On anonymity.** A vote is a row in `poll_votes` carrying the voter's id,
//! because a vote that can be changed has to be findable to be replaced. That
//! id is read in exactly two places: to tell you what *you* picked, and to
//! replace your own ballot. No response body ever contains it, for any caller,
//! admin included — `poll_response` is the single place a poll turns into
//! JSON, so that guarantee holds in one readable spot rather than across nine
//! handlers. What this does not defend against is someone reading the database
//! directly; hiding it there too would mean storing bare counters, which would
//! make a cast vote final.

use actix_web::{web, HttpResponse};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::error::AppError;
use crate::middleware::admin_auth::AdminUser;
use crate::middleware::auth::AuthUser;
use crate::polls::{self, Audience, Duration, Flags, MAX_OPTIONS, MIN_OPTIONS};
use crate::AppState;

/// Polls per page, in both the live and past lists.
const PAGE_SIZE: i64 = 10;

/// Longest accepted title and option label, matching their `VARCHAR(255)`
/// columns. Checked here so an over-long one is a 400 naming the field rather
/// than a truncation nobody notices or an opaque 500.
const MAX_TITLE: usize = 255;
const MAX_LABEL: usize = 255;

/// Longest accepted description. The column is `TEXT` and would take far more;
/// this is about the poll card staying readable.
const MAX_DESCRIPTION: usize = 4000;

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

fn now() -> NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

fn rfc3339(at: NaiveDateTime) -> String {
    at.and_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `?,?,?` for an `IN` list of `n` values.
///
/// The string is built from a *count*, never from anything a caller sent — the
/// values themselves are always bound. Concatenating user input into SQL is the
/// one thing none of this may do.
fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OptionOut {
    id: i64,
    label: String,
    votes: i64,
    percent: f64,
    /// Whether this option is currently ahead. Never set while every option is
    /// on zero — a five-way tie at nothing has no leader, and marking the first
    /// one would invent a result out of an empty poll.
    leading: bool,
}

#[derive(Debug, Serialize)]
struct PollOut {
    id: i64,
    title: String,
    description: Option<String>,
    duration: String,
    #[serde(rename = "durationLabel")]
    duration_label: String,
    #[serde(rename = "allowMultiple")]
    allow_multiple: bool,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "createdBy")]
    created_by: String,
    #[serde(rename = "closesAt")]
    closes_at: Option<String>,
    #[serde(rename = "endedAt")]
    ended_at: Option<String>,
    /// Whether this poll is still taking votes.
    live: bool,
    /// True when a person ended it before its time was up.
    #[serde(rename = "endedEarly")]
    ended_early: bool,
    audiences: Vec<String>,
    #[serde(rename = "audienceLabels")]
    audience_labels: Vec<String>,
    #[serde(rename = "excludedCount")]
    excluded_count: i64,
    /// Distinct accounts that have answered — the denominator behind every
    /// percentage on this poll.
    voters: i64,
    #[serde(rename = "totalVotes")]
    total_votes: i64,
    options: Vec<OptionOut>,
    /// The *calling* user's own current answer. Empty for admin listings,
    /// which have no single viewer whose ballot it would be.
    selected: Vec<i64>,
}

// ---------------------------------------------------------------------------
// Loading polls
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct PollRow {
    id: i64,
    title: String,
    description: Option<String>,
    duration: String,
    allow_multiple: bool,
    created_at: NaiveDateTime,
    created_by: String,
    closes_at: Option<NaiveDateTime>,
    ended_at: Option<NaiveDateTime>,
}

/// Turns poll rows into their JSON shape, gathering options, tallies,
/// audiences and exclusion counts for the whole batch at once.
///
/// Six queries regardless of how many polls are on the page — a per-poll loop
/// here would be eleven round trips for a full page of ten and would grow with
/// it.
///
/// `viewer` is the calling user's id when there is one, used only to fill in
/// their own `selected` answers.
async fn poll_response(
    pool: &sqlx::MySqlPool,
    rows: Vec<PollRow>,
    viewer: Option<&str>,
) -> Result<Vec<PollOut>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let marks = placeholders(ids.len());

    // Options, in the order the admin wrote them.
    let sql = format!(
        "SELECT poll_id, id, label FROM poll_options
         WHERE poll_id IN ({marks}) ORDER BY poll_id, position, id"
    );
    let mut q = sqlx::query_as::<_, (i64, i64, String)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let option_rows = q.fetch_all(pool).await?;

    // Per-option tallies.
    let sql = format!(
        "SELECT poll_id, option_id, COUNT(*) FROM poll_votes
         WHERE poll_id IN ({marks}) GROUP BY poll_id, option_id"
    );
    let mut q = sqlx::query_as::<_, (i64, i64, i64)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let tally_rows = q.fetch_all(pool).await?;
    let tallies: HashMap<(i64, i64), i64> = tally_rows
        .into_iter()
        .map(|(poll, option, n)| ((poll, option), n))
        .collect();

    // Distinct voters, which is the denominator for every percentage.
    let sql = format!(
        "SELECT poll_id, COUNT(DISTINCT user_id) FROM poll_votes
         WHERE poll_id IN ({marks}) GROUP BY poll_id"
    );
    let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let voters: HashMap<i64, i64> = q.fetch_all(pool).await?.into_iter().collect();

    // Audiences.
    let sql =
        format!("SELECT poll_id, audience FROM poll_audience WHERE poll_id IN ({marks})");
    let mut q = sqlx::query_as::<_, (i64, String)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let mut audiences: HashMap<i64, Vec<String>> = HashMap::new();
    for (poll, audience) in q.fetch_all(pool).await? {
        audiences.entry(poll).or_default().push(audience);
    }

    // How many people were excluded by hand. The count only -- who they are is
    // the admin's business, and naming them in a poll body would put a list of
    // people the admin distrusts in front of anyone who can read a poll.
    let sql = format!(
        "SELECT poll_id, COUNT(*) FROM poll_exclusions
         WHERE poll_id IN ({marks}) GROUP BY poll_id"
    );
    let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let excluded: HashMap<i64, i64> = q.fetch_all(pool).await?.into_iter().collect();

    // The viewer's own answers.
    let mut selected: HashMap<i64, Vec<i64>> = HashMap::new();
    if let Some(user_id) = viewer {
        let sql = format!(
            "SELECT poll_id, option_id FROM poll_votes
             WHERE poll_id IN ({marks}) AND user_id = ?"
        );
        let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
        for id in &ids {
            q = q.bind(id);
        }
        q = q.bind(user_id);
        for (poll, option) in q.fetch_all(pool).await? {
            selected.entry(poll).or_default().push(option);
        }
    }

    let at = now();
    let mut out = Vec::with_capacity(rows.len());

    for row in rows {
        let voters_here = voters.get(&row.id).copied().unwrap_or(0);

        let mut options: Vec<OptionOut> = option_rows
            .iter()
            .filter(|(poll, _, _)| *poll == row.id)
            .map(|(_, option_id, label)| {
                let votes = tallies.get(&(row.id, *option_id)).copied().unwrap_or(0);
                OptionOut {
                    id: *option_id,
                    label: label.clone(),
                    votes,
                    percent: polls::percent(votes, voters_here),
                    leading: false,
                }
            })
            .collect();

        let best = options.iter().map(|o| o.votes).max().unwrap_or(0);
        if best > 0 {
            for option in options.iter_mut() {
                option.leading = option.votes == best;
            }
        }

        let total_votes = options.iter().map(|o| o.votes).sum();

        let mut audience_codes = audiences.remove(&row.id).unwrap_or_default();
        audience_codes.sort();
        let audience_labels = audience_codes
            .iter()
            .map(|code| {
                Audience::parse(code)
                    .map(|a| a.label().to_string())
                    .unwrap_or_else(|| code.clone())
            })
            .collect();

        let duration = Duration::parse(&row.duration);
        let live = polls::is_live(row.ended_at, row.closes_at, at);

        // "Ended early" means a person stopped it while it still had time. A
        // permanent poll has no natural end, so ending one is always early.
        let ended_early = match (row.ended_at, row.closes_at) {
            (Some(ended), Some(closes)) => ended < closes,
            (Some(_), None) => true,
            _ => false,
        };

        out.push(PollOut {
            id: row.id,
            title: row.title,
            description: row.description,
            duration_label: duration
                .map(|d| d.label().to_string())
                .unwrap_or_else(|| row.duration.clone()),
            duration: row.duration,
            allow_multiple: row.allow_multiple,
            created_at: rfc3339(row.created_at),
            created_by: row.created_by,
            closes_at: row.closes_at.map(rfc3339),
            ended_at: row.ended_at.map(rfc3339),
            live,
            ended_early,
            audiences: audience_codes,
            audience_labels,
            excluded_count: excluded.get(&row.id).copied().unwrap_or(0),
            voters: voters_here,
            total_votes,
            options,
            selected: selected.remove(&row.id).unwrap_or_default(),
        });
    }

    Ok(out)
}

const POLL_COLUMNS: &str = "id, title, description, duration, allow_multiple,
     created_at, created_by, closes_at, ended_at";

async fn load_one(pool: &sqlx::MySqlPool, id: i64) -> Result<PollRow, AppError> {
    sqlx::query_as::<_, PollRow>(&format!("SELECT {POLL_COLUMNS} FROM polls WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Poll not found".to_string()))
}

// ---------------------------------------------------------------------------
// Who the caller is
// ---------------------------------------------------------------------------

/// The caller's account id and the flags an eligibility decision needs.
///
/// Read from the database, not from the session token: `is_member` is not a
/// JWT claim at all, and `middleware::auth` disables expiry validation, so the
/// token's `is_admin` can be arbitrarily out of date. `routes::auth::access`
/// re-reads for the same reason.
///
/// Matched on `users.id` first and only then on the username. The id is what
/// every other poll table keys on -- `poll_exclusions`, `poll_votes` -- so
/// resolving the caller by name opened a gap between the row the *poll* tables
/// mean and the row the flags are read from. Two accounts that differ only in
/// the case of their username, or a second row created by the Google sign-in
/// path, are enough: the name lands on the wrong row, `is_member` reads back
/// false, and every members-only poll silently vanishes from that person's
/// list. The username fallback is kept for sessions issued before the id claim
/// could be relied on.
///
/// The three flags are read as `Option<bool>` because their columns are
/// `BOOLEAN DEFAULT FALSE` and nullable: a NULL decoded straight into `bool`
/// is a sqlx error, which would turn one odd row into a 500 on a page that
/// should simply have told them they are not a member.
async fn viewer(pool: &sqlx::MySqlPool, auth: &AuthUser) -> Result<(String, Flags), AppError> {
    const FLAGS: &str = "SELECT id, is_admin, is_member, is_og FROM users";

    let mut row: Option<(String, Option<bool>, Option<bool>, Option<bool>)> = None;

    if !auth.id.is_empty() {
        row = sqlx::query_as(&format!("{FLAGS} WHERE id = ?"))
            .bind(&auth.id)
            .fetch_optional(pool)
            .await?;
    }

    if row.is_none() {
        row = sqlx::query_as(&format!("{FLAGS} WHERE username = ?"))
            .bind(&auth.username)
            .fetch_optional(pool)
            .await?;
    }

    let (id, is_admin, is_member, is_og) =
        row.ok_or_else(|| AppError::Unauthorized("Unknown account".to_string()))?;

    Ok((
        id,
        Flags {
            is_admin: is_admin.unwrap_or(false),
            is_member: is_member.unwrap_or(false),
            is_og: is_og.unwrap_or(false),
        },
    ))
}

/// Why a caller may not vote on a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Named on the poll's exclusion list.
    Excluded,
    /// Does not belong to any audience the poll is open to.
    NotEligible,
    /// Eligible, but the poll is no longer taking votes.
    Closed,
}

impl Refusal {
    fn as_str(&self) -> &'static str {
        match self {
            Refusal::Excluded => "excluded",
            Refusal::NotEligible => "not_eligible",
            Refusal::Closed => "closed",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Refusal::Excluded => "You have been excluded from this poll",
            Refusal::NotEligible => "This poll is not open to your account",
            Refusal::Closed => "This poll has closed",
        }
    }
}

/// Whether this account may *see* the poll: in one of its audiences, and not
/// individually excluded. Separate from whether the poll is still open, so a
/// closed poll stays readable to the people it was for.
async fn eligibility(
    pool: &sqlx::MySqlPool,
    poll_id: i64,
    user_id: &str,
    flags: Flags,
) -> Result<Option<Refusal>, AppError> {
    let excluded: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM poll_exclusions WHERE poll_id = ? AND user_id = ?")
            .bind(poll_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;

    if excluded.is_some() {
        return Ok(Some(Refusal::Excluded));
    }

    let rows: Vec<(String,)> = sqlx::query_as("SELECT audience FROM poll_audience WHERE poll_id = ?")
        .bind(poll_id)
        .fetch_all(pool)
        .await?;

    let audiences: Vec<Audience> = rows
        .iter()
        .filter_map(|(code,)| Audience::parse(code))
        .collect();

    if polls::in_audience(&audiences, flags) {
        Ok(None)
    } else {
        Ok(Some(Refusal::NotEligible))
    }
}

/// The audience codes this account satisfies, for filtering a whole list of
/// polls in one query instead of one per poll.
fn audiences_for(flags: Flags) -> Vec<&'static str> {
    let mut out = vec![Audience::Everyone.as_str()];
    if flags.is_admin {
        out.push(Audience::Admins.as_str());
    }
    if flags.is_member {
        out.push(Audience::Members.as_str());
    }
    if flags.is_og {
        out.push(Audience::Ogs.as_str());
    }
    out
}

// ---------------------------------------------------------------------------
// Creating a poll
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePollBody {
    title: Option<String>,
    description: Option<String>,
    duration: Option<String>,
    #[serde(rename = "allowMultiple", default)]
    allow_multiple: bool,
    options: Option<Vec<String>>,
    audiences: Option<Vec<String>>,
    #[serde(default)]
    exclusions: Vec<String>,
}

/// A create request that has been checked over.
#[derive(Debug, PartialEq, Eq)]
struct NewPoll {
    title: String,
    description: Option<String>,
    duration: Duration,
    allow_multiple: bool,
    options: Vec<String>,
    audiences: Vec<Audience>,
}

/// Checks a create request, returning the message to send back on refusal.
///
/// Split out from the handler so every rule below is testable without a
/// database or an HTTP request.
fn validate_new_poll(body: &CreatePollBody) -> Result<NewPoll, String> {
    let title = body.title.as_deref().unwrap_or("").trim().to_string();
    if title.is_empty() {
        return Err("A title is required".to_string());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!("Title must be {MAX_TITLE} characters or fewer"));
    }

    let description = match body.description.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => {
            if text.chars().count() > MAX_DESCRIPTION {
                return Err(format!(
                    "Description must be {MAX_DESCRIPTION} characters or fewer"
                ));
            }
            Some(text.to_string())
        }
    };

    let duration = match body.duration.as_deref() {
        None | Some("") => return Err("A duration is required".to_string()),
        Some(raw) => Duration::parse(raw)
            .ok_or_else(|| format!("duration must be one of: {}", Duration::allowed()))?,
    };

    let raw_options = body.options.clone().unwrap_or_default();
    let mut options: Vec<String> = Vec::with_capacity(raw_options.len());
    for option in &raw_options {
        let label = option.trim();
        // Not skipped: a blank row means the admin left one half-filled, and
        // dropping it silently would publish a poll that asks something other
        // than what they were looking at.
        if label.is_empty() {
            return Err("Options cannot be blank".to_string());
        }
        if label.chars().count() > MAX_LABEL {
            return Err(format!("Options must be {MAX_LABEL} characters or fewer"));
        }
        // Two identical options split the same answer across two bars, and
        // nobody can read the result.
        if options
            .iter()
            .any(|seen: &String| seen.to_lowercase() == label.to_lowercase())
        {
            return Err(format!("Duplicate option: {label}"));
        }
        options.push(label.to_string());
    }

    if options.len() < MIN_OPTIONS {
        return Err(format!("A poll needs at least {MIN_OPTIONS} options"));
    }
    if options.len() > MAX_OPTIONS {
        return Err(format!("A poll can have at most {MAX_OPTIONS} options"));
    }

    let raw_audiences = body.audiences.clone().unwrap_or_default();
    let mut audiences: Vec<Audience> = Vec::new();
    for code in &raw_audiences {
        let audience = Audience::parse(code.trim())
            .ok_or_else(|| format!("audiences must be from: {}", Audience::allowed()))?;
        if !audiences.contains(&audience) {
            audiences.push(audience);
        }
    }
    if audiences.is_empty() {
        return Err("Choose at least one group who can vote".to_string());
    }

    Ok(NewPoll {
        title,
        description,
        duration,
        allow_multiple: body.allow_multiple,
        options,
        audiences,
    })
}

/// POST /api/admin/polls — open a poll (admin only)
async fn create_poll(
    state: web::Data<AppState>,
    admin: AdminUser,
    body: web::Json<CreatePollBody>,
) -> Result<HttpResponse, AppError> {
    let new = validate_new_poll(&body).map_err(AppError::BadRequest)?;

    // Resolve the exclusion list to account ids.
    //
    // An unknown name is refused rather than dropped: silently ignoring a
    // typo'd username is how somebody ends up voting who was meant to be kept
    // out, and the admin would have no way to tell.
    let mut wanted: Vec<String> = Vec::new();
    for name in &body.exclusions {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if !wanted.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
            wanted.push(name.to_string());
        }
    }

    let mut exclusion_ids: Vec<String> = Vec::new();
    if !wanted.is_empty() {
        let marks = placeholders(wanted.len());
        let sql = format!("SELECT id, username FROM users WHERE username IN ({marks})");
        let mut q = sqlx::query_as::<_, (String, String)>(&sql);
        for name in &wanted {
            q = q.bind(name);
        }
        let found = q.fetch_all(&state.pool).await?;

        let missing: Vec<&str> = wanted
            .iter()
            .filter(|name| {
                !found
                    .iter()
                    .any(|(_, username)| username.eq_ignore_ascii_case(name))
            })
            .map(|s| s.as_str())
            .collect();

        if !missing.is_empty() {
            return Err(AppError::BadRequest(format!(
                "No such account: {}",
                missing.join(", ")
            )));
        }

        exclusion_ids = found.into_iter().map(|(id, _)| id).collect();
    }

    let created_at = now();
    let closes_at = new.duration.closes_at(created_at);

    // A transaction, because a poll is not one row. If the options insert
    // failed after the poll row landed, the result would be a question with no
    // answers -- visible in the panel, unanswerable, and needing a hand to
    // clean up.
    let mut tx = state.pool.begin().await?;

    let result = sqlx::query(
        "INSERT INTO polls
            (title, description, duration, allow_multiple, created_at, created_by, closes_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&new.title)
    .bind(&new.description)
    .bind(new.duration.as_str())
    .bind(new.allow_multiple)
    .bind(created_at)
    .bind(&admin.username)
    .bind(closes_at)
    .execute(&mut *tx)
    .await?;

    let poll_id = result.last_insert_id() as i64;

    let values = (0..new.options.len())
        .map(|_| "(?,?,?)")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT INTO poll_options (poll_id, position, label) VALUES {values}");
    let mut q = sqlx::query(&sql);
    for (position, label) in new.options.iter().enumerate() {
        q = q.bind(poll_id).bind(position as i32).bind(label);
    }
    q.execute(&mut *tx).await?;

    let values = (0..new.audiences.len())
        .map(|_| "(?,?)")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT INTO poll_audience (poll_id, audience) VALUES {values}");
    let mut q = sqlx::query(&sql);
    for audience in &new.audiences {
        q = q.bind(poll_id).bind(audience.as_str());
    }
    q.execute(&mut *tx).await?;

    if !exclusion_ids.is_empty() {
        let values = (0..exclusion_ids.len())
            .map(|_| "(?,?)")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("INSERT INTO poll_exclusions (poll_id, user_id) VALUES {values}");
        let mut q = sqlx::query(&sql);
        for user_id in &exclusion_ids {
            q = q.bind(poll_id).bind(user_id);
        }
        q.execute(&mut *tx).await?;
    }

    tx.commit().await?;

    log::info!(
        "poll {} opened by {} ({} options, {} excluded)",
        poll_id,
        admin.username,
        new.options.len(),
        exclusion_ids.len()
    );

    let row = load_one(&state.pool, poll_id).await?;
    let out = poll_response(&state.pool, vec![row], None).await?;

    Ok(HttpResponse::Created().json(serde_json::json!({
        "id": poll_id,
        "poll": out.into_iter().next(),
    })))
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    page: Option<i64>,
}

/// Reads the `status` filter shared by both listings.
///
/// `true` for the live half, `false` for the past half. Absent means live,
/// which is the answer to "what can I do right now".
fn wants_live(status: Option<&str>) -> Result<bool, AppError> {
    match status {
        None | Some("live") => Ok(true),
        Some("past") => Ok(false),
        Some(_) => Err(AppError::BadRequest(
            "status must be live or past".to_string(),
        )),
    }
}

/// GET /api/admin/polls?status=live|past&page=N — browse polls (admin only)
async fn list_polls_admin(
    state: web::Data<AppState>,
    _admin: AdminUser,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let live = wants_live(query.status.as_deref())?;

    let page = query.page.unwrap_or(1).max(1);
    let at = now();

    // Live polls are ordered by how soon they close, so the ones needing
    // attention are at the top and permanent ones -- which never need it --
    // sort last. Past polls are newest first.
    let (where_sql, order_sql) = if live {
        (
            "ended_at IS NULL AND (closes_at IS NULL OR closes_at > ?)",
            "ORDER BY closes_at IS NULL, closes_at ASC, id DESC",
        )
    } else {
        (
            "ended_at IS NOT NULL OR (closes_at IS NOT NULL AND closes_at <= ?)",
            "ORDER BY COALESCE(ended_at, closes_at) DESC, id DESC",
        )
    };

    let total: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM polls WHERE {where_sql}"))
        .bind(at)
        .fetch_one(&state.pool)
        .await?;
    let total = total.0;

    let pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
    let page = page.min(pages);

    let rows: Vec<PollRow> = sqlx::query_as(&format!(
        "SELECT {POLL_COLUMNS} FROM polls WHERE {where_sql} {order_sql} LIMIT ? OFFSET ?"
    ))
    .bind(at)
    .bind(PAGE_SIZE)
    .bind((page - 1) * PAGE_SIZE)
    .fetch_all(&state.pool)
    .await?;

    let out = poll_response(&state.pool, rows, None).await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "polls": out,
        "page": page,
        "pages": pages,
        "total": total,
    })))
}

/// How many closed polls the player-facing listing hands back.
///
/// Not pagination: this is the "what did we decide" view, not an archive, and
/// nobody scrolls past two dozen finished polls. The admin listing is the one
/// that pages through everything.
const PAST_LIMIT: i64 = 25;

/// GET /api/polls?status=live|past — polls this account may answer, or the
/// closed ones it was entitled to answer
///
/// Eligibility governs both halves: a poll you could never have voted on does
/// not appear once it closes either.
async fn list_my_polls(
    state: web::Data<AppState>,
    auth: AuthUser,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let live = wants_live(query.status.as_deref())?;
    let (user_id, flags) = viewer(&state.pool, &auth).await?;

    let codes = audiences_for(flags);
    let marks = placeholders(codes.len());

    // The liveness test is parenthesised because it is ANDed with the audience
    // and exclusion checks -- without the brackets the `OR` in the past branch
    // would swallow them and show every closed poll to everyone.
    let (when_sql, order_sql, limit_sql) = if live {
        (
            "(p.ended_at IS NULL AND (p.closes_at IS NULL OR p.closes_at > ?))",
            "ORDER BY p.closes_at IS NULL, p.closes_at ASC, p.id DESC",
            String::new(),
        )
    } else {
        (
            "(p.ended_at IS NOT NULL OR (p.closes_at IS NOT NULL AND p.closes_at <= ?))",
            "ORDER BY COALESCE(p.ended_at, p.closes_at) DESC, p.id DESC",
            format!("LIMIT {PAST_LIMIT}"),
        )
    };

    let sql = format!(
        "SELECT {POLL_COLUMNS} FROM polls p
         WHERE {when_sql}
           AND NOT EXISTS (
                 SELECT 1 FROM poll_exclusions e
                 WHERE e.poll_id = p.id AND e.user_id = ?)
           AND EXISTS (
                 SELECT 1 FROM poll_audience a
                 WHERE a.poll_id = p.id AND a.audience IN ({marks}))
         {order_sql} {limit_sql}"
    );
    let mut q = sqlx::query_as::<_, PollRow>(&sql).bind(now()).bind(&user_id);
    for code in &codes {
        q = q.bind(code);
    }

    let rows = q.fetch_all(&state.pool).await?;
    let out = poll_response(&state.pool, rows, Some(&user_id)).await?;

    // How many polls of this half exist that this account was filtered out of.
    //
    // An empty list is otherwise three different situations wearing the same
    // face: nobody has opened a poll, none of the open ones are for you, and
    // you were excluded from every one of them. The page said "Nothing open
    // right now" to all three, so an account that had silently stopped
    // matching any audience looked exactly like a quiet week -- which is how
    // one went unnoticed until somebody complained. Only counted when the
    // visible list is empty, because that is the only time it changes what the
    // page says.
    //
    // A bare number, never the titles: which polls exist and who else was kept
    // out is not this caller's business, for the same reason `excludedCount`
    // is a count.
    let hidden = if out.is_empty() {
        let total: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM polls p WHERE {when_sql}"
        ))
        .bind(now())
        .fetch_one(&state.pool)
        .await?;
        total.0
    } else {
        0
    };

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "polls": out,
        // Saves the page a second request purely to learn whose session it is.
        // The caller's own name is not a disclosure -- they typed it to log in.
        "you": auth.username,
        "hiddenCount": hidden,
    })))
}

/// GET /api/polls/{id} — one poll, with results and your own answer
async fn get_poll(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    let poll_id = path.into_inner();
    let (user_id, flags) = viewer(&state.pool, &auth).await?;
    let row = load_one(&state.pool, poll_id).await?;

    // Eligibility gates reading; being open gates voting. Nothing here checks
    // whether the poll has closed, deliberately -- a finished poll stays
    // readable to the people it was for, which is the point of keeping past
    // results at all.
    if let Some(refusal) = eligibility(&state.pool, poll_id, &user_id, flags).await? {
        return Err(AppError::Forbidden(refusal.message().to_string()));
    }

    let out = poll_response(&state.pool, vec![row], Some(&user_id)).await?;

    out.into_iter()
        .next()
        .map(|poll| HttpResponse::Ok().json(poll))
        .ok_or_else(|| AppError::NotFound("Poll not found".to_string()))
}

/// GET /api/polls/{id}/access — may this account vote on this poll?
///
/// Shaped like `routes::auth::access`: 200 when allowed, 403 when not, and
/// `allowed` present in the body either way so one branch of frontend code
/// reads both. `reason` and `eligible` are separate so the caller can tell
/// "not for you" from "you were eligible, but it has closed".
async fn poll_access(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    let poll_id = path.into_inner();
    let (user_id, flags) = viewer(&state.pool, &auth).await?;
    let row = load_one(&state.pool, poll_id).await?;

    let open = polls::is_live(row.ended_at, row.closes_at, now());
    let refusal = eligibility(&state.pool, poll_id, &user_id, flags)
        .await?
        .or(if open { None } else { Some(Refusal::Closed) });

    let selected: Vec<(i64,)> =
        sqlx::query_as("SELECT option_id FROM poll_votes WHERE poll_id = ? AND user_id = ?")
            .bind(poll_id)
            .bind(&user_id)
            .fetch_all(&state.pool)
            .await?;
    let selected: Vec<i64> = selected.into_iter().map(|(id,)| id).collect();

    let body = serde_json::json!({
        "allowed": refusal.is_none(),
        "reason": refusal.map(|r| r.as_str()).unwrap_or("ok"),
        "eligible": !matches!(refusal, Some(Refusal::Excluded) | Some(Refusal::NotEligible)),
        "open": open,
        "hasVoted": !selected.is_empty(),
        "allowMultiple": row.allow_multiple,
        "selected": selected,
    });

    Ok(match refusal {
        None => HttpResponse::Ok().json(body),
        Some(_) => HttpResponse::Forbidden().json(body),
    })
}

// ---------------------------------------------------------------------------
// Voting
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct VoteBody {
    #[serde(rename = "optionIds", default)]
    option_ids: Vec<i64>,
}

/// Reduces a submitted set of choices to what will be stored.
///
/// Duplicates are folded rather than refused -- picking the same option twice
/// means the same thing as picking it once -- but an empty ballot is a
/// mistake, and more than one choice on a single-answer poll is a client bug
/// that must not be resolved by quietly keeping whichever came first.
fn normalise_choice(ids: &[i64], allow_multiple: bool) -> Result<Vec<i64>, String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for id in ids {
        if seen.insert(*id) {
            out.push(*id);
        }
    }

    if out.is_empty() {
        return Err("Choose an option".to_string());
    }
    if !allow_multiple && out.len() > 1 {
        return Err("This poll accepts one answer".to_string());
    }
    if out.len() > MAX_OPTIONS {
        return Err(format!("At most {MAX_OPTIONS} options can be chosen"));
    }

    Ok(out)
}

/// POST /api/polls/{id}/vote — cast or change your answer
async fn cast_vote(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<i64>,
    body: web::Json<VoteBody>,
) -> Result<HttpResponse, AppError> {
    let poll_id = path.into_inner();
    let (user_id, flags) = viewer(&state.pool, &auth).await?;
    let row = load_one(&state.pool, poll_id).await?;

    if let Some(refusal) = eligibility(&state.pool, poll_id, &user_id, flags).await? {
        return Err(AppError::Forbidden(refusal.message().to_string()));
    }

    if !polls::is_live(row.ended_at, row.closes_at, now()) {
        return Err(AppError::Conflict(Refusal::Closed.message().to_string()));
    }

    let chosen = normalise_choice(&body.option_ids, row.allow_multiple)
        .map_err(AppError::BadRequest)?;

    // Every chosen option must belong to *this* poll. Without this an id from
    // another poll would be accepted and counted somewhere nobody is looking.
    let marks = placeholders(chosen.len());
    let sql = format!("SELECT id FROM poll_options WHERE poll_id = ? AND id IN ({marks})");
    let mut q = sqlx::query_as::<_, (i64,)>(&sql).bind(poll_id);
    for id in &chosen {
        q = q.bind(id);
    }
    if q.fetch_all(&state.pool).await?.len() != chosen.len() {
        return Err(AppError::BadRequest(
            "Those options do not belong to this poll".to_string(),
        ));
    }

    // Replace the ballot rather than amend it, so a first vote and a changed
    // vote are the same two statements for both kinds of poll.
    //
    // In a transaction: between the delete and the insert the voter has no
    // vote at all, and a failure there would silently discard an answer they
    // had already given.
    let mut tx = state.pool.begin().await?;

    sqlx::query("DELETE FROM poll_votes WHERE poll_id = ? AND user_id = ?")
        .bind(poll_id)
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;

    let values = (0..chosen.len())
        .map(|_| "(?,?,?)")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT INTO poll_votes (poll_id, user_id, option_id) VALUES {values}");
    let mut q = sqlx::query(&sql);
    for option_id in &chosen {
        q = q.bind(poll_id).bind(&user_id).bind(option_id);
    }
    q.execute(&mut *tx).await?;

    tx.commit().await?;

    // Reload rather than adjust a count locally, so what the voter sees is what
    // the database holds.
    let row = load_one(&state.pool, poll_id).await?;
    let out = poll_response(&state.pool, vec![row], Some(&user_id)).await?;

    out.into_iter()
        .next()
        .map(|poll| HttpResponse::Ok().json(poll))
        .ok_or_else(|| AppError::NotFound("Poll not found".to_string()))
}

/// POST /api/admin/polls/{id}/end — close a poll now (admin only)
async fn end_poll(
    state: web::Data<AppState>,
    admin: AdminUser,
    path: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    let poll_id = path.into_inner();
    let row = load_one(&state.pool, poll_id).await?;

    if !polls::is_live(row.ended_at, row.closes_at, now()) {
        return Err(AppError::Conflict("This poll has already closed".to_string()));
    }

    // `ended_at IS NULL` in the WHERE clause, not just the check above: two
    // admins can press the button at once, and the second must not overwrite
    // the first one's closing time.
    let result = sqlx::query("UPDATE polls SET ended_at = ? WHERE id = ? AND ended_at IS NULL")
        .bind(now())
        .bind(poll_id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::Conflict("This poll has already closed".to_string()));
    }

    log::info!("poll {} ended early by {}", poll_id, admin.username);

    let row = load_one(&state.pool, poll_id).await?;
    let out = poll_response(&state.pool, vec![row], None).await?;

    out.into_iter()
        .next()
        .map(|poll| HttpResponse::Ok().json(poll))
        .ok_or_else(|| AppError::NotFound("Poll not found".to_string()))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/admin/polls")
            .route("", web::post().to(create_poll))
            .route("", web::get().to(list_polls_admin))
            .route("/{id}/end", web::post().to(end_poll)),
    )
    .service(
        web::scope("/polls")
            .route("", web::get().to(list_my_polls))
            .route("/{id}", web::get().to(get_poll))
            .route("/{id}/access", web::get().to(poll_access))
            .route("/{id}/vote", web::post().to(cast_vote)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(options: &[&str], audiences: &[&str]) -> CreatePollBody {
        CreatePollBody {
            title: Some("New spawn build?".to_string()),
            description: None,
            duration: Some("3d".to_string()),
            allow_multiple: false,
            options: Some(options.iter().map(|s| s.to_string()).collect()),
            audiences: Some(audiences.iter().map(|s| s.to_string()).collect()),
            exclusions: Vec::new(),
        }
    }

    fn valid() -> CreatePollBody {
        body(&["Medieval", "Modern"], &["members"])
    }

    // ------------------------------------------------------------ placeholders

    #[test]
    fn placeholders_match_the_number_of_values() {
        assert_eq!(placeholders(1), "?");
        assert_eq!(placeholders(3), "?,?,?");
    }

    /// The string is built from a count and holds nothing else -- no quotes, no
    /// values, nothing a caller could have influenced.
    #[test]
    fn placeholders_contain_only_placeholders() {
        let sql = placeholders(5);

        assert_eq!(sql.matches('?').count(), 5);
        assert!(sql.chars().all(|c| c == '?' || c == ','));
    }

    // ------------------------------------------------------------- validation

    #[test]
    fn a_complete_request_is_accepted() {
        let poll = validate_new_poll(&valid()).unwrap();

        assert_eq!(poll.title, "New spawn build?");
        assert_eq!(poll.duration, Duration::ThreeDays);
        assert_eq!(poll.options, vec!["Medieval", "Modern"]);
        assert_eq!(poll.audiences, vec![Audience::Members]);
        assert!(!poll.allow_multiple);
    }

    #[test]
    fn a_title_is_required_and_whitespace_is_not_one() {
        let mut b = valid();
        b.title = None;
        assert!(validate_new_poll(&b).is_err());

        b.title = Some("   ".to_string());
        assert!(validate_new_poll(&b).is_err());
    }

    #[test]
    fn titles_and_options_are_trimmed() {
        let mut b = valid();
        b.title = Some("  Spacey  ".to_string());
        b.options = Some(vec!["  A  ".to_string(), "B".to_string()]);

        let poll = validate_new_poll(&b).unwrap();

        assert_eq!(poll.title, "Spacey");
        assert_eq!(poll.options, vec!["A", "B"]);
    }

    /// The column is VARCHAR(255); without this the database would truncate and
    /// the admin would never be told.
    #[test]
    fn an_over_long_title_is_refused_rather_than_truncated() {
        let mut b = valid();
        b.title = Some("x".repeat(MAX_TITLE + 1));

        assert!(validate_new_poll(&b).is_err());

        b.title = Some("x".repeat(MAX_TITLE));
        assert!(validate_new_poll(&b).is_ok(), "exactly at the limit is fine");
    }

    #[test]
    fn an_empty_description_becomes_none_rather_than_an_empty_string() {
        let mut b = valid();

        b.description = Some("   ".to_string());
        assert_eq!(validate_new_poll(&b).unwrap().description, None);

        b.description = Some(" Concept art in #builds ".to_string());
        assert_eq!(
            validate_new_poll(&b).unwrap().description.as_deref(),
            Some("Concept art in #builds")
        );
    }

    #[test]
    fn the_duration_must_be_one_we_recognise() {
        let mut b = valid();

        b.duration = Some("14d".to_string());
        assert!(validate_new_poll(&b).is_err());

        b.duration = None;
        assert!(validate_new_poll(&b).is_err());

        for good in ["1d", "3d", "7d", "permanent"] {
            b.duration = Some(good.to_string());
            assert!(validate_new_poll(&b).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn a_poll_needs_between_two_and_ten_options() {
        let audiences = ["members"];

        assert!(validate_new_poll(&body(&["Only one"], &audiences)).is_err());
        assert!(validate_new_poll(&body(&["A", "B"], &audiences)).is_ok());

        let ten: Vec<String> = (0..MAX_OPTIONS).map(|i| format!("Option {i}")).collect();
        let refs: Vec<&str> = ten.iter().map(|s| s.as_str()).collect();
        assert!(validate_new_poll(&body(&refs, &audiences)).is_ok());

        let eleven: Vec<String> = (0..MAX_OPTIONS + 1).map(|i| format!("Option {i}")).collect();
        let refs: Vec<&str> = eleven.iter().map(|s| s.as_str()).collect();
        assert!(validate_new_poll(&body(&refs, &audiences)).is_err());
    }

    /// A half-filled row is a mistake in front of the admin's eyes. Dropping it
    /// would publish a different question from the one on screen.
    #[test]
    fn a_blank_option_is_refused_not_skipped() {
        assert!(validate_new_poll(&body(&["Medieval", "  ", "Modern"], &["members"])).is_err());
    }

    /// Two identical options split one answer across two bars, and the result
    /// becomes unreadable.
    #[test]
    fn duplicate_options_are_refused_ignoring_case() {
        assert!(validate_new_poll(&body(&["Medieval", "Modern"], &["members"])).is_ok());
        assert!(validate_new_poll(&body(&["Medieval", "medieval"], &["members"])).is_err());
        assert!(validate_new_poll(&body(&["Medieval", " Medieval "], &["members"])).is_err());
    }

    #[test]
    fn at_least_one_audience_is_required() {
        let mut b = valid();

        b.audiences = Some(vec![]);
        assert!(validate_new_poll(&b).is_err());

        b.audiences = None;
        assert!(validate_new_poll(&b).is_err());
    }

    #[test]
    fn an_unknown_audience_is_refused() {
        let mut b = valid();
        b.audiences = Some(vec!["members".to_string(), "moderators".to_string()]);

        assert!(validate_new_poll(&b).is_err());
    }

    #[test]
    fn repeated_audiences_are_folded() {
        let mut b = valid();
        b.audiences = Some(vec![
            "members".to_string(),
            "members".to_string(),
            "ogs".to_string(),
        ]);

        assert_eq!(
            validate_new_poll(&b).unwrap().audiences,
            vec![Audience::Members, Audience::Ogs]
        );
    }

    // ------------------------------------------------------ audiences_for

    #[test]
    fn every_account_is_in_the_everyone_audience() {
        assert_eq!(audiences_for(Flags::default()), vec!["everyone"]);
    }

    #[test]
    fn an_accounts_flags_become_the_audiences_it_matches() {
        let flags = Flags {
            is_admin: true,
            is_member: false,
            is_og: true,
        };

        assert_eq!(audiences_for(flags), vec!["everyone", "admins", "ogs"]);
    }

    // --------------------------------------------------------- vote choices

    #[test]
    fn a_single_choice_is_kept() {
        assert_eq!(normalise_choice(&[7], false).unwrap(), vec![7]);
    }

    #[test]
    fn an_empty_ballot_is_refused() {
        assert!(normalise_choice(&[], false).is_err());
        assert!(normalise_choice(&[], true).is_err());
    }

    /// Picking the same option twice means what picking it once means, so this
    /// folds rather than refusing -- unlike two *different* options on a
    /// single-answer poll, which is a real disagreement about intent.
    #[test]
    fn repeated_choices_fold_but_keep_their_order() {
        assert_eq!(normalise_choice(&[4, 2, 4, 2], true).unwrap(), vec![4, 2]);
        assert_eq!(
            normalise_choice(&[9, 9, 9], false).unwrap(),
            vec![9],
            "the same option repeated is still one answer"
        );
    }

    #[test]
    fn a_single_answer_poll_refuses_two_different_options() {
        assert!(normalise_choice(&[1, 2], false).is_err());
        assert!(normalise_choice(&[1, 2], true).is_ok());
    }

    #[test]
    fn no_more_choices_than_a_poll_can_have_options() {
        let too_many: Vec<i64> = (1..=(MAX_OPTIONS as i64 + 1)).collect();

        assert!(normalise_choice(&too_many, true).is_err());
    }

    // ------------------------------------------------------------- refusals

    /// These strings reach the frontend, which branches on them to explain
    /// itself. Renaming one turns a specific message into a generic one.
    #[test]
    fn refusal_reasons_have_stable_names() {
        assert_eq!(Refusal::Excluded.as_str(), "excluded");
        assert_eq!(Refusal::NotEligible.as_str(), "not_eligible");
        assert_eq!(Refusal::Closed.as_str(), "closed");
    }
}
