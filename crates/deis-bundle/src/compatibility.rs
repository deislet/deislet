//! The `compatibility` block of a manifest: which day's platform an
//! application was written against.
//!
//! Every application on this platform runs on whatever the node's build
//! happens to be. Without this block a fix that changes an observable
//! behaviour is a fleet-wide breaking change with no gradual path: the last
//! dependency upgrade alone moved the meaning of an empty permission list, the
//! meaning of an empty deny list and the module shape of the web globals, and
//! every deployed application took all three at once on a node restart nobody
//! scheduled.
//!
//! An application therefore says which day it was written against, and the
//! platform keeps the answers it gave on that day:
//!
//! ```json
//! "compatibility": { "date": "2026-09-01", "flags": ["single_set_cookie_header"] }
//! ```
//!
//! The date moves a whole batch of behaviours at once; a flag names one of
//! them and overrides the date in either direction, which is how an
//! application takes one fix early or holds one back late.
//!
//! # Where this shape lives, and why here
//!
//! Beside the manifest it is a field of, for the same reason
//! [`crate::limits::DeclaredLimits`] lives here: four components have to agree
//! exactly on what it means. The build writes it out of the project file, the
//! control plane refuses one it cannot honour at upload and carries it to
//! every node with the deployment, the node resolves it once when a version is
//! installed, and the isolate asks the resolved answer on every request. Four
//! copies of one table would drift, and the symptom - an application getting a
//! behaviour its manifest says it did not ask for - is invisible until
//! somebody's cookie goes missing in production.
//!
//! # Adding a flag
//!
//! One row in [`FLAGS`], one variant in [`Flag`], and the branch at the point
//! of use asking [`CompatibilityProfile::is_on`]. Nothing else. The table is
//! meant to be read top to bottom when it holds thirty rows, so a row carries
//! the two names, the date the behaviour becomes the default, and one line
//! saying what actually changes for an application.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A calendar day, with no time and no zone.
///
/// Its own type rather than a `String` because it is compared - a flag is on
/// when the application's date is on or after the flag's - and two dates
/// compared as text are only accidentally in order. Its own type rather than a
/// `chrono::NaiveDate` because this crate is linked by the build, the control
/// plane and every node, and the manifest format should not drag a date
/// library into all three to hold three integers.
///
/// Field order is what `Ord` derives from, so year-month-day is load bearing:
/// it makes the derived comparison chronological.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompatibilityDate {
    year: u16,
    month: u8,
    day: u8,
}

impl CompatibilityDate {
    /// Build a date without checking it, for the constants in [`FLAGS`] and
    /// for [`PLATFORM_KNOWN_THROUGH`].
    ///
    /// `const` so those can be constants, which is why it cannot return a
    /// `Result`. Nothing outside this file's own tables uses it: everything
    /// that comes from a manifest or a project file arrives through
    /// [`FromStr`], which does check. `every_flag_names_a_real_date` in the
    /// tests below is what stops a typo in a table from becoming a date no
    /// calendar has.
    pub const fn new(year: u16, month: u8, day: u8) -> Self {
        Self { year, month, day }
    }

    /// Whether this is a day that exists. Not called on the request path -
    /// only where a date is first read out of text.
    fn is_real(&self) -> bool {
        if self.month < 1 || self.month > 12 || self.day < 1 {
            return false;
        }
        self.day <= days_in_month(self.year, self.month)
    }
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

impl fmt::Display for CompatibilityDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl std::str::FromStr for CompatibilityDate {
    type Err = String;

    /// `YYYY-MM-DD`, and nothing else.
    ///
    /// Deliberately strict about the shape: `2026-9-1` is refused rather than
    /// read as September the 1st, because a date is compared against the
    /// dates in [`FLAGS`] and a spelling this accepted loosely today is a
    /// spelling something else would order differently tomorrow.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || {
            format!(
                "compatibility date {text:?} is not a date; write it as YYYY-MM-DD, \
                 for example {PLATFORM_KNOWN_THROUGH}"
            )
        };
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(bad());
        }
        let year: u16 = text[0..4].parse().map_err(|_| bad())?;
        let month: u8 = text[5..7].parse().map_err(|_| bad())?;
        let day: u8 = text[8..10].parse().map_err(|_| bad())?;
        let date = Self { year, month, day };
        if !date.is_real() {
            return Err(format!(
                "compatibility date {text:?} is not a day that exists"
            ));
        }
        Ok(date)
    }
}

impl Serialize for CompatibilityDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CompatibilityDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The newest compatibility date this build of the platform knows.
///
/// Bumped when a release ships a behaviour change that a date turns on, and
/// never otherwise. It is what makes "a date this build cannot honour" a
/// question with an answer: a manifest naming a later day is asking for
/// semantics that do not exist in this binary, and every component refuses it
/// rather than running the application on the newest answers it happens to
/// have. That refusal is also what a date in the future means - the platform's
/// knowledge stops at its own release, so next week and next year are the same
/// unanswerable question, and no component here needs a clock to say so.
///
/// A mixed fleet reads this twice: the control plane's copy decides what may
/// be uploaded, and each node's copy decides what that node may run. A node on
/// an older build than the date an application names refuses the deployment
/// and says which two dates disagree, which is the whole point - a node that
/// pretended would serve that application semantics it did not ask for, and
/// nothing downstream would ever say so.
pub const PLATFORM_KNOWN_THROUGH: CompatibilityDate = CompatibilityDate::new(2026, 9, 1);

/// One behaviour that has an old answer and a better one.
///
/// The discriminant is the index into [`FLAGS`] and into the bitset
/// [`CompatibilityProfile`] keeps, which `flags_are_in_enum_order` below
/// checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Flag {
    /// Send every `Set-Cookie` header the handler set, instead of only the
    /// last one. See the row in [`FLAGS`].
    MultipleSetCookieHeaders = 0,
}

/// One row of the registry.
pub struct FlagSpec {
    pub flag: Flag,
    /// What an application writes in `compatibility.flags` to turn the
    /// behaviour on ahead of its date.
    pub enable: &'static str,
    /// What it writes to hold the behaviour off past its date.
    pub disable: &'static str,
    /// The date from which this behaviour is the default. An application
    /// whose date is this day or later gets it without naming anything.
    pub default_on: CompatibilityDate,
    /// One line, in terms of what changes for an application - not in terms
    /// of the code that changed.
    pub summary: &'static str,
}

/// Every behaviour a date or a flag can move.
///
/// Ordered by discriminant, which is also the bit each flag occupies. Add to
/// the end.
pub const FLAGS: &[FlagSpec] = &[FlagSpec {
    flag: Flag::MultipleSetCookieHeaders,
    enable: "multiple_set_cookie_headers",
    disable: "single_set_cookie_header",
    default_on: CompatibilityDate::new(2026, 9, 1),
    summary: "every Set-Cookie header a handler set is sent to the client; before this date \
              only the last one was, and the rest were dropped without a trace",
}];

/// One bit per flag, so the check on the request path is a mask and not a
/// date comparison. Sixty-four is not a limit anyone will reach soon, and the
/// assertion below turns reaching it into a build failure rather than a flag
/// that silently never turns on.
const _: () = assert!(FLAGS.len() <= 64, "CompatibilityProfile holds one u64");

/// The registry row for one behaviour.
///
/// The way anything outside this module reads the table: the date a flag turns
/// on by default, the two names, and the line describing what changes. A
/// caller that wants one of those asks for the row rather than for a
/// per-field accessor, so a row that gains a field does not need a new
/// function to reach it.
pub fn flag_spec(flag: Flag) -> &'static FlagSpec {
    &FLAGS[flag as usize]
}

/// The `compatibility` object of a bundle manifest, exactly as written.
///
/// This is the declaration, not the answer: [`CompatibilityProfile::resolve`]
/// turns it into the answers. Kept apart because the two are read by different
/// people - an operator asks what an application *declared*, and the runtime
/// asks what is *on*.
///
/// `deny_unknown_fields` for the reason [`crate::limits::DeclaredLimits`] has
/// it: a misspelled key that parsed and was dropped would leave an author
/// reading their own manifest and believing in a pin that was never in force.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredCompatibility {
    /// The day this application was written against. Absent means it predates
    /// the mechanism; see [`CompatibilityProfile::resolve`] for what that
    /// buys it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<CompatibilityDate>,

    /// Names from [`FLAGS`] - an `enable` to take one behaviour ahead of the
    /// date, a `disable` to hold one back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

impl DeclaredCompatibility {
    /// True when the application declared nothing at all.
    pub fn is_empty(&self) -> bool {
        self.date.is_none() && self.flags.is_empty()
    }

    /// Refuse a declaration this build cannot honour.
    ///
    /// Two separate refusals, because they are two different mistakes:
    ///
    /// * a flag name nothing knows, or a pair that contradicts each other -
    ///   caught by [`CompatibilityProfile::resolve`], which is where the
    ///   names are read;
    /// * a date after [`PLATFORM_KNOWN_THROUGH`] - semantics this binary does
    ///   not have.
    ///
    /// Called wherever a manifest is read, so a bundle that no component could
    /// run is refused at the build, at the upload and at the node, rather than
    /// on the one request whose answer would have differed.
    pub fn validate(&self) -> Result<(), String> {
        CompatibilityProfile::resolve(self)?;
        if let Some(date) = self.date
            && date > PLATFORM_KNOWN_THROUGH
        {
            return Err(format!(
                "compatibility date {date} is later than {PLATFORM_KNOWN_THROUGH}, which is the \
                 newest date this build of the platform knows. It cannot give an application the \
                 behaviour of a day it has never seen, and running it on today's answers instead \
                 would be exactly the silent change the date exists to prevent. Name a date up to \
                 {PLATFORM_KNOWN_THROUGH}, or upgrade the platform"
            ));
        }
        Ok(())
    }
}

/// The resolved answers for one application: which behaviours are on.
///
/// **This is the one place the runtime asks.** A date comparison happens here,
/// once, when a version is installed; everything downstream calls
/// [`Self::is_on`] with a [`Flag`] and gets a bit. Scattering
/// `date >= "2026-09-01"` through the runtime would mean thirty different
/// spellings of the same question in five years, and no single place to read
/// what an application is actually getting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "DeclaredCompatibility", try_from = "DeclaredCompatibility")]
pub struct CompatibilityProfile {
    date: Option<CompatibilityDate>,
    /// Bit `n` is `FLAGS[n]`.
    on: u64,
}

impl Default for CompatibilityProfile {
    /// The oldest behaviour - every flag off.
    ///
    /// This is what an application that declares no date gets, and it is
    /// deliberately the same thing a path that forgot to carry a profile
    /// would get: the direction that cannot change an application's behaviour
    /// under it.
    fn default() -> Self {
        Self { date: None, on: 0 }
    }
}

impl CompatibilityProfile {
    /// Work out what is on for one declaration.
    ///
    /// **A declaration with no date gets the oldest behaviour**: every flag
    /// off, whatever today's default would be. The alternatives were weighed
    /// and both are worse. Giving it today's behaviour is the fleet-wide
    /// breaking change this whole mechanism exists to prevent, and it would
    /// keep happening - the same undated application would behave differently
    /// again on the next release. Refusing it outright would break every
    /// application already deployed, on the first node restart after this
    /// shipped, which is an outage caused by the machinery for avoiding
    /// outages.
    ///
    /// The price is real and is paid deliberately: an undated application is
    /// frozen and no later fix will ever reach it. That is why nothing here is
    /// quiet about it - the control plane logs an undated version at every
    /// upload and every delivery, `deis deploy` says so as it deploys, and
    /// `deis versions` prints the date, or `none`, for every version.
    ///
    /// Flags are applied after the date and win over it, in either direction,
    /// which is what lets an application take one fix early or hold one back.
    /// A declaration naming both names of one flag is refused rather than
    /// resolved by order: picking one silently is how a manifest ends up
    /// meaning something nobody wrote.
    pub fn resolve(declared: &DeclaredCompatibility) -> Result<Self, String> {
        let mut on: u64 = 0;
        if let Some(date) = declared.date {
            for spec in FLAGS {
                if date >= spec.default_on {
                    on |= 1 << (spec.flag as u8);
                }
            }
        }

        // Which flags an explicit name has already spoken for, so that the
        // second, contradicting name is caught rather than silently applied
        // last.
        let mut named: u64 = 0;
        for name in &declared.flags {
            let name = name.trim();
            let Some(spec) = FLAGS
                .iter()
                .find(|spec| spec.enable == name || spec.disable == name)
            else {
                return Err(format!(
                    "compatibility flag {name:?} is not one this build knows. Known flags: {}",
                    known_flag_names()
                ));
            };
            let bit = 1u64 << (spec.flag as u8);
            let enabling = spec.enable == name;
            if named & bit != 0 && ((on & bit != 0) != enabling) {
                return Err(format!(
                    "compatibility flags name both {:?} and {:?}, which are the two sides of one \
                     behaviour; the manifest does not say which was meant",
                    spec.enable, spec.disable
                ));
            }
            named |= bit;
            if enabling {
                on |= bit;
            } else {
                on &= !bit;
            }
        }

        Ok(Self {
            date: declared.date,
            on,
        })
    }

    /// Is this behaviour on for this application?
    ///
    /// A mask and a compare. Safe to call per request, which is the point:
    /// the gate at the point of use reads as a plain condition and carries no
    /// date arithmetic of its own.
    pub fn is_on(&self, flag: Flag) -> bool {
        self.on & (1 << (flag as u8)) != 0
    }

    /// The day this application was written against, or `None` when it
    /// declares no date and is held to the oldest behaviour.
    ///
    /// This is what an operator asking "which applications have no date" is
    /// reading, so it stays on the resolved profile rather than only on the
    /// declaration.
    pub fn date(&self) -> Option<CompatibilityDate> {
        self.date
    }

    /// The `enable` names of every behaviour that is on, in table order.
    pub fn enabled_flag_names(&self) -> Vec<&'static str> {
        FLAGS
            .iter()
            .filter(|spec| self.is_on(spec.flag))
            .map(|spec| spec.enable)
            .collect()
    }

    /// A one-line rendering for an operator's log.
    pub fn describe(&self) -> String {
        let enabled = self.enabled_flag_names();
        let flags = if enabled.is_empty() {
            "no behaviour flags on".to_string()
        } else {
            enabled.join(", ")
        };
        match self.date {
            Some(date) => format!("{date}, {flags}"),
            None => format!("no date, oldest behaviour ({flags})"),
        }
    }
}

impl From<CompatibilityProfile> for DeclaredCompatibility {
    /// Renders the *answers*, not the original declaration: the flags listed
    /// are everything that is on, however it got there. That is what makes a
    /// serialised profile readable in a log or a dump - the question anyone
    /// asks of one is what this application is getting, not what it typed.
    fn from(profile: CompatibilityProfile) -> Self {
        Self {
            date: profile.date,
            flags: profile
                .enabled_flag_names()
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

impl TryFrom<DeclaredCompatibility> for CompatibilityProfile {
    type Error = String;

    fn try_from(declared: DeclaredCompatibility) -> Result<Self, Self::Error> {
        Self::resolve(&declared)
    }
}

/// Every name a manifest may write, with what each one does, for the message
/// that refuses one it may not.
///
/// The summaries are here rather than left to documentation because this
/// string is what somebody who has just misspelled a flag actually reads. A
/// bare list of names would tell them which spellings exist and nothing about
/// which one they meant.
pub fn known_flag_names() -> String {
    FLAGS
        .iter()
        .map(|spec| {
            format!(
                "{} / {} (on by default from {}): {}",
                spec.enable, spec.disable, spec.default_on, spec.summary
            )
        })
        .collect::<Vec<String>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(date: Option<&str>, flags: &[&str]) -> DeclaredCompatibility {
        DeclaredCompatibility {
            date: date.map(|d| d.parse().expect("a test date must parse")),
            flags: flags.iter().map(|f| f.to_string()).collect(),
        }
    }

    /// The bit a flag occupies is its position in the table. A row inserted in
    /// the middle without moving the discriminants would hand every flag after
    /// it somebody else's answer.
    #[test]
    fn flags_are_in_enum_order() {
        for (index, spec) in FLAGS.iter().enumerate() {
            assert_eq!(
                spec.flag as usize, index,
                "FLAGS[{index}] carries a variant whose discriminant is {}",
                spec.flag as usize
            );
        }
    }

    #[test]
    fn every_flag_name_is_unique_and_the_two_sides_differ() {
        let mut seen: Vec<&str> = Vec::new();
        for spec in FLAGS {
            assert_ne!(spec.enable, spec.disable, "{}", spec.enable);
            for name in [spec.enable, spec.disable] {
                assert!(!seen.contains(&name), "{name} names two behaviours");
                seen.push(name);
            }
        }
    }

    #[test]
    fn every_flag_names_a_real_date_this_build_knows() {
        for spec in FLAGS {
            assert!(spec.default_on.is_real(), "{}", spec.enable);
            assert!(
                spec.default_on <= PLATFORM_KNOWN_THROUGH,
                "{} becomes the default on {}, after the newest date this build admits to \
                 knowing ({PLATFORM_KNOWN_THROUGH}); no application could ever name a date that \
                 turns it on",
                spec.enable,
                spec.default_on
            );
        }
    }

    #[test]
    fn a_date_orders_chronologically_not_alphabetically() {
        let older: CompatibilityDate = "2026-09-09".parse().unwrap();
        let newer: CompatibilityDate = "2026-09-10".parse().unwrap();
        assert!(
            older < newer,
            "text order would put these the other way round"
        );
    }

    #[test]
    fn a_date_must_be_a_real_day_written_in_full() {
        for text in [
            "2026-9-1",
            "2026-13-01",
            "2026-02-30",
            "26-09-01",
            "",
            "today",
        ] {
            assert!(
                text.parse::<CompatibilityDate>().is_err(),
                "{text:?} was accepted as a date"
            );
        }
        assert_eq!("2024-02-29".parse::<CompatibilityDate>().unwrap().day, 29);
    }

    #[test]
    fn no_date_means_the_oldest_behaviour() {
        let profile = CompatibilityProfile::resolve(&declared(None, &[])).unwrap();
        assert_eq!(profile.date(), None);
        assert!(!profile.is_on(Flag::MultipleSetCookieHeaders));
        assert_eq!(profile.enabled_flag_names(), Vec::<&str>::new());
    }

    #[test]
    fn a_date_on_or_after_a_flags_own_date_turns_it_on() {
        let on_the_day = flag_spec(Flag::MultipleSetCookieHeaders)
            .default_on
            .to_string();
        let profile = CompatibilityProfile::resolve(&declared(Some(&on_the_day), &[])).unwrap();
        assert!(profile.is_on(Flag::MultipleSetCookieHeaders));

        let profile = CompatibilityProfile::resolve(&declared(Some("2026-01-01"), &[])).unwrap();
        assert!(
            !profile.is_on(Flag::MultipleSetCookieHeaders),
            "an application older than the fix keeps the answer it was written against"
        );
    }

    #[test]
    fn a_flag_takes_a_behaviour_early_and_holds_one_back_late() {
        let early = CompatibilityProfile::resolve(&declared(
            Some("2026-01-01"),
            &["multiple_set_cookie_headers"],
        ))
        .unwrap();
        assert!(early.is_on(Flag::MultipleSetCookieHeaders));

        let late = CompatibilityProfile::resolve(&declared(
            Some("2026-09-01"),
            &["single_set_cookie_header"],
        ))
        .unwrap();
        assert!(!late.is_on(Flag::MultipleSetCookieHeaders));
    }

    /// An application with no date but one named flag is a real case, not an
    /// oversight: it predates the mechanism and wants exactly one fix.
    #[test]
    fn a_flag_works_without_a_date() {
        let profile =
            CompatibilityProfile::resolve(&declared(None, &["multiple_set_cookie_headers"]))
                .unwrap();
        assert_eq!(profile.date(), None);
        assert!(profile.is_on(Flag::MultipleSetCookieHeaders));
    }

    #[test]
    fn a_flag_nobody_knows_is_refused_with_the_list_of_ones_that_are() {
        let err = CompatibilityProfile::resolve(&declared(None, &["multipl_set_cookie"]))
            .expect_err("a misspelled flag must not be dropped in silence");
        assert!(err.contains("multipl_set_cookie"), "{err}");
        assert!(err.contains("multiple_set_cookie_headers"), "{err}");
        assert!(
            err.contains(flag_spec(Flag::MultipleSetCookieHeaders).summary),
            "the list has to say what each flag does, not only that it exists: {err}"
        );
    }

    #[test]
    fn naming_both_sides_of_one_behaviour_is_refused() {
        let err = CompatibilityProfile::resolve(&declared(
            None,
            &["multiple_set_cookie_headers", "single_set_cookie_header"],
        ))
        .expect_err("a contradiction must not be settled by declaration order");
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn naming_the_same_side_twice_is_not_a_contradiction() {
        let profile = CompatibilityProfile::resolve(&declared(
            None,
            &["multiple_set_cookie_headers", "multiple_set_cookie_headers"],
        ))
        .unwrap();
        assert!(profile.is_on(Flag::MultipleSetCookieHeaders));
    }

    #[test]
    fn a_date_this_build_does_not_know_is_refused_naming_both_days() {
        let err = declared(Some("2099-01-01"), &[])
            .validate()
            .expect_err("a date beyond this build must not be waved through");
        assert!(err.contains("2099-01-01"), "{err}");
        assert!(err.contains(&PLATFORM_KNOWN_THROUGH.to_string()), "{err}");
    }

    #[test]
    fn a_date_this_build_does_know_is_accepted() {
        assert!(
            declared(Some(&PLATFORM_KNOWN_THROUGH.to_string()), &[])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn a_profile_serialises_as_the_answers_it_gives() {
        let profile = CompatibilityProfile::resolve(&declared(Some("2026-09-01"), &[])).unwrap();
        let json = serde_json::to_string(&profile).unwrap();
        assert_eq!(
            json,
            r#"{"date":"2026-09-01","flags":["multiple_set_cookie_headers"]}"#
        );
        let back: CompatibilityProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }

    #[test]
    fn the_default_profile_is_the_oldest_behaviour() {
        let profile = CompatibilityProfile::default();
        assert_eq!(profile.date(), None);
        for spec in FLAGS {
            assert!(
                !profile.is_on(spec.flag),
                "{} is on by default, so a path that forgot to carry a profile would change an \
                 application's behaviour",
                spec.enable
            );
        }
    }

    #[test]
    fn describe_says_the_date_and_what_is_on() {
        let dated = CompatibilityProfile::resolve(&declared(Some("2026-09-01"), &[])).unwrap();
        assert_eq!(dated.describe(), "2026-09-01, multiple_set_cookie_headers");
        assert_eq!(
            CompatibilityProfile::default().describe(),
            "no date, oldest behaviour (no behaviour flags on)"
        );
    }

    #[test]
    fn a_manifest_block_refuses_a_key_nobody_reads() {
        let err =
            serde_json::from_str::<DeclaredCompatibility>(r#"{"compatibilityDate":"2026-09-01"}"#)
                .expect_err("a key nobody reads must not be accepted");
        assert!(err.to_string().contains("compatibilityDate"), "{err}");
    }

    #[test]
    fn an_absent_field_is_left_out_of_the_manifest_entirely() {
        let block = declared(Some("2026-09-01"), &[]);
        assert_eq!(
            serde_json::to_string(&block).unwrap(),
            r#"{"date":"2026-09-01"}"#
        );
    }
}
