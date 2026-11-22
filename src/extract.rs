use std::{borrow::Cow, ops::Range, sync::LazyLock};

use {
    jiff::fmt::{rfc2822, temporal},
    regex_automata::{PatternID, meta::Regex},
    regex_syntax::hir::Hir,
};

use crate::{
    args::{self, Configurable, Usage},
    timezone,
};

/// A searcher for finding one of a number of different kinds of tags.
#[derive(Clone, Debug)]
pub struct Extractor {
    regex: Regex,
    validators: Vec<Validator>,
    tag_group_indices: Vec<usize>,
    all: bool,
}

impl Extractor {
    /// Return an iterator of tags extracted from the haystack given.
    pub fn find_iter(
        &self,
        haystack: &[u8],
    ) -> impl Iterator<Item = Range<usize>> {
        // I have a suspicion that this could end up being a bit slower
        // than is ideal when there are a lot of matches. But maybe it
        // doesn't matter. If so, we should do the minorly annoying
        // work necessary to use `self.regex.find_iter` when we know
        // that all of the values in `tag_group_indices` are `0`. (That
        // is, none of the regexes use a capture group named `tag`.)
        self.regex
            .captures_iter(haystack)
            .filter_map(|caps| {
                let pid = caps.pattern()?;
                let span = caps.get_group(self.tag_group_indices[pid])?;
                if !self.validators[pid](&haystack[span.range()]) {
                    return None;
                }
                Some(span.range())
            })
            .take(if self.all { usize::MAX } else { 1 })
    }
}

/// Defines CLI flags for building an extractor from regexes.
#[derive(Clone, Debug, Default)]
pub struct ExtractorBuilder {
    auto: Option<Auto>,
    patterns: Vec<Pattern>,
    all: bool,
}

impl ExtractorBuilder {
    /// Turn this builder into an extractor that can produce tags via regex.
    pub fn build(&self) -> anyhow::Result<Extractor> {
        let mut validators: Vec<Validator> = vec![];
        let mut patterns: Vec<Cow<'_, Hir>> =
            self.patterns.iter().map(|p| Cow::Borrowed(&p.hir)).collect();
        // Add validators in correspondence with user provided patterns.
        // This just makes sure that pattern IDs in matches are correct
        // indices for `validators`.
        for _ in self.patterns.iter() {
            validators.push(validate_user_provided);
        }
        // We specifically add any automatic patterns after patterns
        // given explicitly by the end user. This gives priority to
        // user provided patterns by virtue of regex's leftmost-first
        // match semantics.
        match self.auto() {
            Auto::None => {}
            Auto::DateTime => {
                for &(pattern, validator) in AUTO_DATE_TIME.iter() {
                    let pattern: Pattern = pattern.parse()?;
                    patterns.push(Cow::Owned(pattern.hir));
                    validators.push(validator);
                }
            }
            Auto::TimeZone => {
                // For time zones, we don't bother with making each IANA
                // identifier its own pattern. We could, but there's no real
                // benefit to it. And I believe stick them all in one regex
                // will permit more optimizations at the NFA level, but I
                // didn't try it.
                //
                // Also, you'd think this would be an ideal task for
                // `aho-corasick`. But interestingly, it isn't! It turns out
                // that the `regex` engine will use its lazy DFA for this
                // case, which works out really well as long as it doesn't
                // thrash. And the number of IANA ids is low enough (<1000)
                // that it generally shouldn't thrash.
                //
                // I did try `aho-corasick` here. The contiguous NFA is
                // (as expected) a bit slower, almost 2x. The DFA in
                // `aho-corasick` is about the same speed as the lazy DFA,
                // which is also expected. So we just stick to regex here.
                let pattern = AUTO_TIME_ZONE
                    .iter()
                    // I believe the escaping is not necessary here (since I
                    // don't think IANA ids can have regex meta characters in
                    // them), but we do so for robustness reasons.
                    .map(|name| regex_syntax::escape(&name))
                    .collect::<Vec<String>>()
                    .join("|");
                let pattern: Pattern = pattern.parse()?;
                patterns.push(Cow::Owned(pattern.hir));
                validators.push(validate_time_zone);
            }
        }
        let regex = Regex::builder()
            // Counter-intuitive, but the prefilters extracted for
            // datetime regexes tend to be shit. They don't make things
            // a *lot* slower (which is good, because it means the heuristics
            // in `regex-automata` are bad), but in my ad hoc benchmarking,
            // it was around 10% faster with prefilters disabled. I think the
            // main issue is that you wind up with a prefilter for `[0-9]`,
            // which ends up generating a ton of false positives.
            .configure(Regex::config().auto_prefilter(false))
            .build_many_from_hir(&patterns)?;

        let mut tag_group_indices = vec![];
        for pid in (0..regex.pattern_len()).map(PatternID::new_unchecked) {
            if let Some(i) = regex.group_info().to_index(pid, "tag") {
                tag_group_indices.push(i);
            } else {
                tag_group_indices.push(0);
            }
        }
        Ok(Extractor { regex, validators, tag_group_indices, all: self.all })
    }

    /// Returns the "automatic" setting to use.
    ///
    /// When no patterns or given and no explicit value is given, then
    /// the automatic setting defaults to `DateTime`. Otherwise, if a
    /// pattern is given but `--auto` isn't given, then this defaults to
    /// `None`.
    fn auto(&self) -> Auto {
        self.auto.unwrap_or_else(|| {
            if self.patterns.is_empty() { Auto::DateTime } else { Auto::None }
        })
    }
}

impl Configurable for ExtractorBuilder {
    fn configure(
        &mut self,
        p: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Long("auto") => {
                self.auto = Some(args::parse(p, "--auto")?);
            }
            lexopt::Arg::Long("all") => {
                self.all = true;
            }
            lexopt::Arg::Short('e')
            | lexopt::Arg::Long("regex")
            | lexopt::Arg::Long("regexp") => {
                self.patterns.push(args::parse(p, "-e/--regex")?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        const ALL: Usage = Usage::flag(
            "--all",
            "Find all matching tags.",
            r#"
Find all matching tags in each line given.

By default, only the first tag found is extracted. Subsequent tags on a line
are ignored. This flag overrides that behavior and finds all tags on each line.

The downside of this option is that multiple tags for each datum can lead to
some confusing behavior in other commands. For example, `bttf time cmp lt` will
allow tagged data through when *any* of its tags are less than the target
datetime.
"#,
        );

        &[Auto::USAGE, Pattern::USAGE, ALL]
    }
}

/// Checks whether a matched tag is "valid" or not.
///
/// This currently only applies for patterns derived from automatic extraction.
type Validator = fn(&[u8]) -> bool;

#[derive(Clone, Copy, Debug)]
enum Auto {
    None,
    DateTime,
    TimeZone,
}

impl Auto {
    const USAGE: Usage = Usage::flag(
        "--auto <kind>",
        "Automatically extract tags, e.g., `datetime`.",
        r#"
Automatically extract tags.

Currently, the supported values are `none`, `datetime` or `timezone`.

For `datetime`, only definitive datetime strings are recognized. For example,
RFC 9557, RFC 3339 or RFC 2822 timestamps.

For `timezone`, each IANA time zone identifier available in the time zone
database used by bttf are recognized.

When this is not provided *and* `-e/--regex` is not provided, then this
defaults to `datetime`. Otherwise, if `-e/--regex` is provided, then this
defaults to `none`. Note though that this flag may be provided in addition to
`-e/--regex`.
"#,
    );
}

impl std::str::FromStr for Auto {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Auto> {
        match s {
            "none" => Ok(Auto::None),
            "datetime" => Ok(Auto::DateTime),
            "timezone" => Ok(Auto::TimeZone),
            unk => anyhow::bail!("unknown automatic extractor name `{unk}`"),
        }
    }
}

#[derive(Clone, Debug)]
struct Pattern {
    hir: Hir,
}

impl Pattern {
    const USAGE: Usage = Usage::flag(
        "-e/--regex <pattern>",
        "A pattern for extracting tags.",
        r#"
A pattern for extracting tags.

Matches of this pattern are treated as tags. If the pattern has a capture
group named `tag`, then the value of that group is used instead.

Multiple patterns may be given.

Note that matches are not validated. For example, if one uses a regex like
`[0-9]{4}-[0-9]{2}-[0-9]{2}`, then this will match `2025-13-01`, which is
not a valid Gregorian date. bttf will allow this tag to exist, but they can
be validated by other commands. For example, `bttf time parse` can be used
to validate tags as datetimes, and invalid datetimes can be dropped with the
`-i/--ignore-invalid` flag.
"#,
    );
}

impl std::str::FromStr for Pattern {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Pattern> {
        let hir = regex_syntax::Parser::new().parse(s)?;
        Ok(Pattern { hir })
    }
}

/// Returns a list of IANA time zone identifiers sorted in order of descending
/// length.
///
/// They are sorted this way so that the leftmost-first match semantics of
/// regex will end up corresponding to leftmost-longest for this specific set
/// of literals. I think in practice this doesn't matter, since I don't think
/// one IANA id can be a prefix of another. But this seems more robust and it
/// doesn't really cost us anything.
static AUTO_TIME_ZONE: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut names = timezone::available().to_vec();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
});

/// The regexes to use when doing automatic detection for datetimes.
///
/// We specifically only support well known and _specified_ datetime formats.
/// That is, if we find one of these, it is very very likely to be an actual
/// datetime.
///
/// These regexes are permitted to have false positives. When a match is
/// found, the matched text is parsed as the corresponding datetime, which
/// will filter out false positives.
///
/// Ideally these regexes do not have any false negatives, but they probably
/// do have some corner cases. We should probably try to fix those, but only
/// if it's practical to do so in the regex.
static AUTO_DATE_TIME: &[(&str, Validator)] = {
    // e.g., 2025-11-05T16:13:00.123456789-04:00[America/New_York]
    static RFC9557: &str = r#"(?x)
        [0-9]{4}-?[0-9]{2}-?[0-9]{2}
        (?:T|\x20)
        [0-9]{2}(:?[0-9]{2}(:?[0-9]{2})?)?(?:[.,][0-9]{1,9})?
        (?:Z|[-+][0-9]{2}(:?[0-9]{2})?)?
        \[[^\]]+\]
    "#;

    // e.g., 2025-11-05T16:13:00.123456789Z
    static RFC3339: &str = r#"(?x)
        [0-9]{4}-?[0-9]{2}-?[0-9]{2}
        (?:T|\x20)
        [0-9]{2}(:?[0-9]{2}(:?[0-9]{2})?)?(?:[.,][0-9]{1,9})?
        (?:Z|[-+][0-9]{2}(:?[0-9]{2})?)
    "#;

    // e.g., Fri, 04 Mar 2025 20:12:47 GMT
    static RFC9110: &str = r#"(?x)
        (?:Sun|Mon|Tue|Wed|Thu|Fri|Sat),
        \x20
        [0-9]{2}
        \x20
        (?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)
        \x20
        [0-9]{4}
        \x20
        [0-9]{2}:[0-9]{2}:[0-9]{2}
        \x20
        GMT
    "#;

    // e.g., Sat, 13 Jul 2024 15:09:59 -0400
    static RFC2822: &str = r#"(?x)
        (?:(?:Sun|Mon|Tue|Wed|Thu|Fri|Sat),\x20)?
        [0-9]{1,2}
        \x20
        (?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)
        \x20
        [0-9]{4}
        \x20
        [0-9]{2}:[0-9]{2}:[0-9]{2}
        \x20
        (?:
            (?:[-+][0-9]{4})
            |
            (?:UT|GMT|EST|EDT|CST|CDT|MST|MDT|PST|PDT)
        )
    "#;
    &[
        (RFC9557, validate_rfc9557),
        (RFC3339, validate_rfc3339),
        (RFC9110, validate_rfc2822),
        (RFC2822, validate_rfc2822),
    ]
};

static TEMPORAL_PARSER: temporal::DateTimeParser =
    temporal::DateTimeParser::new();
static RFC2822_PARSER: rfc2822::DateTimeParser =
    rfc2822::DateTimeParser::new();

fn validate_user_provided(_bytes: &[u8]) -> bool {
    true
}

fn validate_rfc9557(bytes: &[u8]) -> bool {
    TEMPORAL_PARSER.parse_zoned_datetime(bytes).is_ok()
}

fn validate_rfc3339(bytes: &[u8]) -> bool {
    TEMPORAL_PARSER.parse_timestamp(bytes).is_ok()
}

// Also used for RFC 9110. (Jiff doesn't have a separate RFC 9110 parser.)
fn validate_rfc2822(bytes: &[u8]) -> bool {
    RFC2822_PARSER.parse_zoned_datetime(bytes).is_ok()
}

// We don't do any extra validation since every match is a true positive.
fn validate_time_zone(_bytes: &[u8]) -> bool {
    true
}
