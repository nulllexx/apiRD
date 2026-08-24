//! The parts of polling that are decisions rather than queries.
//!
//! How long a poll runs, who is allowed to answer it, and what a result bar
//! means are all things worth being sure about, so they live here as plain
//! functions with no database behind them. `routes::polls` does the SQL and
//! calls into this.

use chrono::NaiveDateTime;

/// How many answers a poll may offer.
///
/// Two because a one-option poll asks nothing, ten because past that the bars
/// stop being readable and the question probably wants splitting.
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 10;

/// How long a poll stays open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duration {
    OneDay,
    ThreeDays,
    SevenDays,
    /// Open until somebody ends it by hand.
    Permanent,
}

impl Duration {
    /// The spelling stored in `polls.duration`.
    ///
    /// Changing one of these strings orphans every poll already written with
    /// the old one, which is why a test pins them.
    pub fn as_str(&self) -> &'static str {
        match self {
            Duration::OneDay => "1d",
            Duration::ThreeDays => "3d",
            Duration::SevenDays => "7d",
            Duration::Permanent => "permanent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1d" => Some(Duration::OneDay),
            "3d" => Some(Duration::ThreeDays),
            "7d" => Some(Duration::SevenDays),
            "permanent" => Some(Duration::Permanent),
            _ => None,
        }
    }

    /// Every value the API accepts, for the error message when it does not.
    pub fn allowed() -> &'static str {
        "1d, 3d, 7d, permanent"
    }

    /// When a poll opened at `from` stops accepting votes.
    ///
    /// `None` for a permanent poll -- it has no closing time, which is a
    /// different thing from having one in the past.
    pub fn closes_at(&self, from: NaiveDateTime) -> Option<NaiveDateTime> {
        let days = match self {
            Duration::OneDay => 1,
            Duration::ThreeDays => 3,
            Duration::SevenDays => 7,
            Duration::Permanent => return None,
        };
        Some(from + chrono::Duration::days(days))
    }

    /// Wording for the panel, so "3d" is never shown to a person.
    pub fn label(&self) -> &'static str {
        match self {
            Duration::OneDay => "1 day",
            Duration::ThreeDays => "3 days",
            Duration::SevenDays => "7 days",
            Duration::Permanent => "Permanent",
        }
    }
}

/// A group of accounts a poll is open to.
///
/// These mirror the boolean flags on `users`; there is no rank table, and no
/// link between an account and a Minecraft rank, so these are what "who can
/// vote" can currently mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Everyone,
    Admins,
    Members,
    Ogs,
}

impl Audience {
    /// The spelling stored in `poll_audience.audience`. Pinned by a test for
    /// the same reason as [`Duration::as_str`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Audience::Everyone => "everyone",
            Audience::Admins => "admins",
            Audience::Members => "members",
            Audience::Ogs => "ogs",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "everyone" => Some(Audience::Everyone),
            "admins" => Some(Audience::Admins),
            "members" => Some(Audience::Members),
            "ogs" => Some(Audience::Ogs),
            _ => None,
        }
    }

    pub fn allowed() -> &'static str {
        "everyone, admins, members, ogs"
    }

    /// Wording for the panel.
    pub fn label(&self) -> &'static str {
        match self {
            Audience::Everyone => "Everyone",
            Audience::Admins => "Admins",
            Audience::Members => "Members",
            Audience::Ogs => "OGs",
        }
    }
}

/// The account flags an eligibility decision is made from.
///
/// Read from the database rather than the session token: `is_member` is not a
/// JWT claim at all, and the token's expiry is not validated, so its copy of
/// `is_admin` can be arbitrarily old.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub is_admin: bool,
    pub is_member: bool,
    pub is_og: bool,
}

/// Whether these flags satisfy any one of `audiences`.
///
/// Union, not intersection: a poll open to Admins and OGs means either
/// qualifies, not that you must be both. An empty audience list matches nobody
/// -- a poll nobody may answer is a mistake worth surfacing rather than
/// quietly reading as "everyone".
pub fn in_audience(audiences: &[Audience], flags: Flags) -> bool {
    audiences.iter().any(|a| match a {
        Audience::Everyone => true,
        Audience::Admins => flags.is_admin,
        Audience::Members => flags.is_member,
        Audience::Ogs => flags.is_og,
    })
}

/// Share of *voters* who picked an option, 0-100, rounded to one decimal.
///
/// The denominator is distinct voters rather than vote rows, for both kinds of
/// poll. On a single-answer poll the two are the same number and the bars sum
/// to 100. On a multi-answer poll they are not, and the bars can sum past 100
/// -- which is correct, because "62% of voters wanted this" is the reading that
/// stays true either way, and it is the one Discord shows.
///
/// No voters yields 0.0, not a NaN that would reach the panel as "NaN%".
pub fn percent(option_votes: i64, voters: i64) -> f64 {
    if voters <= 0 || option_votes <= 0 {
        return 0.0;
    }
    let pct = (option_votes as f64) * 100.0 / (voters as f64);
    (pct * 10.0).round() / 10.0
}

/// Whether a poll is still accepting votes, given the two columns that decide
/// it and the current time.
///
/// Mirrors the SQL in `routes::polls`; kept here so the rule exists once in a
/// form that can be tested without a database.
pub fn is_live(
    ended_at: Option<NaiveDateTime>,
    closes_at: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> bool {
    if ended_at.is_some() {
        return false;
    }
    match closes_at {
        None => true,
        Some(at) => at > now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    /// These strings are in the database. Renaming one does not migrate the
    /// rows already holding the old spelling -- it splits them off silently,
    /// and a poll created last week stops parsing.
    #[test]
    fn stored_spellings_are_fixed() {
        assert_eq!(Duration::OneDay.as_str(), "1d");
        assert_eq!(Duration::ThreeDays.as_str(), "3d");
        assert_eq!(Duration::SevenDays.as_str(), "7d");
        assert_eq!(Duration::Permanent.as_str(), "permanent");

        assert_eq!(Audience::Everyone.as_str(), "everyone");
        assert_eq!(Audience::Admins.as_str(), "admins");
        assert_eq!(Audience::Members.as_str(), "members");
        assert_eq!(Audience::Ogs.as_str(), "ogs");
    }

    #[test]
    fn every_stored_spelling_parses_back() {
        for d in [
            Duration::OneDay,
            Duration::ThreeDays,
            Duration::SevenDays,
            Duration::Permanent,
        ] {
            assert_eq!(Duration::parse(d.as_str()), Some(d));
        }
        for a in [
            Audience::Everyone,
            Audience::Admins,
            Audience::Members,
            Audience::Ogs,
        ] {
            assert_eq!(Audience::parse(a.as_str()), Some(a));
        }
    }

    #[test]
    fn unknown_spellings_are_rejected_rather_than_defaulted() {
        assert_eq!(Duration::parse("14d"), None);
        assert_eq!(Duration::parse(""), None);
        assert_eq!(Duration::parse("1D"), None, "parsing is exact, not lenient");
        assert_eq!(Audience::parse("staff"), None);
        assert_eq!(Audience::parse("Everyone"), None);
    }

    #[test]
    fn a_duration_closes_that_many_days_later() {
        let opened = at("2026-08-24 12:00:00");

        assert_eq!(
            Duration::OneDay.closes_at(opened),
            Some(at("2026-08-25 12:00:00"))
        );
        assert_eq!(
            Duration::ThreeDays.closes_at(opened),
            Some(at("2026-08-27 12:00:00"))
        );
        assert_eq!(
            Duration::SevenDays.closes_at(opened),
            Some(at("2026-08-31 12:00:00"))
        );
    }

    /// Not "closes at some far-off date" -- genuinely no closing time, so the
    /// clock can never end it and only a person can.
    #[test]
    fn a_permanent_poll_has_no_closing_time() {
        assert_eq!(Duration::Permanent.closes_at(at("2026-08-24 12:00:00")), None);
    }

    #[test]
    fn everyone_matches_an_account_with_no_flags_at_all() {
        assert!(in_audience(&[Audience::Everyone], Flags::default()));
    }

    /// Ticking two boxes means either qualifies. Reading it as "both" would
    /// exclude nearly everyone the admin meant to include.
    #[test]
    fn several_audiences_are_a_union_not_an_intersection() {
        let audiences = [Audience::Admins, Audience::Ogs];

        let og_only = Flags { is_og: true, ..Flags::default() };
        let admin_only = Flags { is_admin: true, ..Flags::default() };
        let both = Flags { is_admin: true, is_og: true, ..Flags::default() };

        assert!(in_audience(&audiences, og_only));
        assert!(in_audience(&audiences, admin_only));
        assert!(in_audience(&audiences, both));
    }

    #[test]
    fn an_account_matching_no_audience_is_not_eligible() {
        let members_only = [Audience::Members];
        let og_not_member = Flags { is_og: true, ..Flags::default() };

        assert!(!in_audience(&members_only, og_not_member));
        assert!(!in_audience(&members_only, Flags::default()));
    }

    /// A poll open to nobody is a mistake the admin should see, not something
    /// to reinterpret as "everyone" on their behalf.
    #[test]
    fn no_audiences_matches_nobody_including_an_admin() {
        let admin = Flags { is_admin: true, is_member: true, is_og: true };

        assert!(!in_audience(&[], admin));
    }

    /// The bar has to render before anyone has voted, and 0/0 is where a
    /// percentage turns into NaN.
    #[test]
    fn percentages_are_zero_before_anyone_votes() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(0, 12), 0.0);
    }

    #[test]
    fn a_single_answer_poll_sums_to_one_hundred() {
        // 31 + 14 + 5 == 50 voters.
        let bars = [percent(31, 50), percent(14, 50), percent(5, 50)];

        assert_eq!(bars, [62.0, 28.0, 10.0]);
        assert_eq!(bars.iter().sum::<f64>(), 100.0);
    }

    /// Voters, not votes, is the denominator -- so on a multi-answer poll the
    /// bars can and should sum past 100. Each one still reads correctly on its
    /// own: 75% of voters picked this.
    #[test]
    fn a_multi_answer_poll_may_sum_past_one_hundred() {
        let voters = 4;
        let bars = [percent(3, voters), percent(3, voters), percent(2, voters)];

        assert_eq!(bars, [75.0, 75.0, 50.0]);
        assert!(bars.iter().sum::<f64>() > 100.0);
    }

    #[test]
    fn percentages_carry_one_decimal() {
        assert_eq!(percent(1, 3), 33.3);
        assert_eq!(percent(2, 3), 66.7);
        assert_eq!(percent(1, 7), 14.3);
    }

    /// The one-voter case, which no storage scheme can hide: the result simply
    /// is that person's answer. Worth a test only so nobody later mistakes it
    /// for a bug and "fixes" it into something misleading.
    #[test]
    fn one_voter_reads_as_one_hundred_percent() {
        assert_eq!(percent(1, 1), 100.0);
    }

    #[test]
    fn a_poll_is_live_until_its_closing_time_passes() {
        let now = at("2026-08-24 12:00:00");

        assert!(is_live(None, Some(at("2026-08-24 12:00:01")), now));
        assert!(!is_live(None, Some(at("2026-08-24 11:59:59")), now));
    }

    #[test]
    fn a_closing_time_exactly_now_has_passed() {
        let now = at("2026-08-24 12:00:00");

        assert!(
            !is_live(None, Some(now), now),
            "a poll that closes at noon is not open at noon"
        );
    }

    #[test]
    fn a_permanent_poll_stays_live_until_ended_by_hand() {
        let now = at("2030-01-01 00:00:00");

        assert!(is_live(None, None, now));
        assert!(!is_live(Some(at("2026-08-24 12:00:00")), None, now));
    }

    /// Ending early has to beat the clock, or the button would do nothing to a
    /// poll that still had days left.
    #[test]
    fn ending_early_closes_a_poll_that_had_time_left() {
        let now = at("2026-08-24 12:00:00");
        let ended = at("2026-08-24 11:00:00");
        let far_off = at("2026-12-31 00:00:00");

        assert!(is_live(None, Some(far_off), now));
        assert!(!is_live(Some(ended), Some(far_off), now));
    }
}
