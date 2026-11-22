use std::borrow::Cow;

use {
    anyhow::Context,
    bstr::{BStr, ByteSlice},
    jiff::{
        ToSpan, Unit, ZonedDateTime, civil, fmt,
        tz::{self, Offset},
    },
};

use crate::{
    NOW, TZ,
    args::{Usage, flags::Weekday},
    parse::{BytesExt, FromBytes},
    span::TimeSpan,
    timezone::TimeZone,
};

static TEMPORAL_PARSER: fmt::temporal::DateTimeParser =
    fmt::temporal::DateTimeParser::new();
static RFC2822_PARSER: fmt::rfc2822::DateTimeParser =
    fmt::rfc2822::DateTimeParser::new();

/// Represents a bttf "datetime" parsed from user input.
///
/// Basically, everything comes down to a physical instant in time. We support
/// a lot of different ways to get to one (including just clock time like
/// `17:30`), but the representation is ultimately an instant in time
///
/// The time zone is always `jiff::tz::TimeZone::system`. That is, we always
/// try to emit datetimes as local time. Users can change what "local" means by
/// setting the `TZ` environment variable.
///
/// This type exists primarily as a target for trait impls for tailoring
/// behavior specific to `bttf`.
///
/// This is like `DateTimeFlexible`, but specifically does not support parsing
/// datetimes implicitly relative to `NOW`. The idea is that doing this on data
/// passed through a shell pipeline is likely to lead to mistakes. For example,
/// accidentally parsing durations that aren't relative to `NOW` as relative to
/// `NOW`.
#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct DateTime {
    /// The actual parsed datetime. i.e., The thing we operate on.
    zdt: ZonedDateTime,
}

impl DateTime {
    // This is phrased in a way where it accounts for datetimes being given
    // as positional arguments OR on stdin. This also implies that a variable
    // number of datetimes can be given.
    //
    // At time of writing (2025-03-16), there is no case where a datetime
    // is accepted as a single positional argument. If that case does arise,
    // then we'll want a different usage string (like we do for a datetime
    // flag).
    pub const ARG_OR_STDIN: Usage = Usage::arg(
        "<datetime>",
        "A datetime string, e.g., `now`, `-1d` or `2025-03-15T00:00Z`.",
        r#"
A datetime string.

Datetimes can either be passed as positional arguments or as line delimited
data on stdin, but not both. That is, datetimes will only be read from stdin
when there are no datetimes provided as positional arguments.

bttf accepts a number of different formats for a datetime automatically.
Specifically, they can be in one of the following formats that each
unambiguously refer to a particular instant in time:

RFC 9557, e.g., `2025-03-15T10:23:00-04:00[America/New_York]`

RFC 3339, e.g., `2025-03-15T10:23:00-04:00`

RFC 2822, e.g., `Sat, 15 Mar 2025 10:23:00 -0400`

When datetimes are read from the command line as positional arguments, then the
following more "flexible" formats are also supported in most cases:

A subset of ISO 8601, e.g., `2025-03-15` or even `08:30`. When a time is
missing, the first instant of the corresponding day is used. (Which is usually
midnight, but not always, for example `2015-10-18` in `America/Sao_Paulo`.)
When a date is missing, the current date is used, even if that would result
in a datetime in the past. In both cases, the datetime is then interpreted as
a local time in your system's configured time zone (which may be overridden
by the `TZ` environment variable).

A relative datetime expressed as a duration from the current time. For example,
to get 1 day from the current time, you can use `1 day`, or more succinctly,
`1d`. To get 1 day in the past from the current time, you can use `1 day ago`,
or more succinctly, `-1d`. Mixing calendar and time units is allowed, for
example, `1 year 1 second` or `1y1s`.

Some special strings are supported as well:

`now` refers to the current datetime to the highest precision supported by
your system. The current datetime is computed once when bttf starts, or if the
`BTTF_NOW` environment variable is set, that time is used instead.

`today` refers to the first instant of the current day.

`yesterday` refers to the first instant of the previous day.

`tomorrow` refers to the first instant of the next day.

Other examples of things that work:

`this thurs` refers to the current day (if it's a Thursday) or the soonest
date that falls on a Thursday.

`last FRIDAY` refers to the previously occurring Friday, up to 1 week in the
past (if the current day is a Friday).

`next saturday` refers to the next Saturday, up to 1 week in the future (if
the current day is a Saturday).

`5pm tomorrow`, `5pm next Wed` or `5pm 1 week` refer to 5pm tomorrow, 5pm on

If you need to parse one of the flexible datetime formats from stdin, then you
can use either `bttf time parse`.
"#,
    );

    pub const ARG: Usage = Usage::arg(
        "<datetime>",
        "A datetime string, e.g., `now`, `-1d` or `2025-03-15T00:00Z`.",
        r#"
A single datetime string.

bttf accepts a number of different formats for a datetime automatically.
Specifically, they can be in one of the following formats that each
unambiguously refer to a particular instant in time:

RFC 9557, e.g., `2025-03-15T10:23:00-04:00[America/New_York]`

RFC 3339, e.g., `2025-03-15T10:23:00-04:00`

RFC 2822, e.g., `Sat, 15 Mar 2025 10:23:00 -0400`

Since this argument must be passed explicitly on the command line, a number of
additional more flexible formats are also accepted:

A subset of ISO 8601, e.g., `2025-03-15` or even `08:30`. When a time is
missing, the first instant of the corresponding day is used. (Which is usually
midnight, but not always, for example `2015-10-18` in `America/Sao_Paulo`.)
When a date is missing, the current date is used, even if that would result
in a datetime in the past. In both cases, the datetime is then interpreted as
a local time in your system's configured time zone (which may be overridden
by the `TZ` environment variable).

A datetime expressed as a duration from the current time. For example, to get
1 day from the current time, you can use `1 day`, or more succinctly, `1d`. To
get 1 day in the past from the current time, you can use `1 day ago`, or more
succinctly, `-1d`. Mixing calendar and time units is allowed, for example, `1
year 1 second` or `1y1s`.

Some special strings are supported as well:

`now` refers to the current datetime to the highest precision supported by
your system. The current datetime is computed once when bttf starts, or if the
`BTTF_NOW` environment variable is set, that time is used instead.

`today` refers to the first instant of the current day.

`yesterday` refers to the first instant of the previous day.

`tomorrow` refers to the first instant of the next day.

Other examples of things that work:

`this thurs` refers to the current day (if it's a Thursday) or the soonest
date that falls on a Thursday.

`last FRIDAY` refers to the previously occurring Friday, up to 1 week in the
past (if the current day is a Friday).

`next saturday` refers to the next Saturday, up to 1 week in the future (if
the current day is a Saturday).

`5pm tomorrow`, `5pm next Wed` or `5pm 1 week` refer to 5pm tomorrow, 5pm on
"#,
    );

    pub const RELATIVE_FLAG: Usage = Usage::flag(
        "-r/--relative <datetime>",
        "A datetime string, e.g., `now`, `-1d` or `2025-03-15T00:00Z`.",
        r#"
A single datetime string.

bttf accepts a number of different formats for a datetime automatically.
Specifically, they can be in one of the following formats that each
unambiguously refer to a particular instant in time:

RFC 9557, e.g., `2025-03-15T10:23:00-04:00[America/New_York]`

RFC 3339, e.g., `2025-03-15T10:23:00-04:00`

RFC 2822, e.g., `Sat, 15 Mar 2025 10:23:00 -0400`

Since this is a flag that must be passed explicitly on the command line, a
number of additional more flexible formats are also accepted:

A subset of ISO 8601, e.g., `2025-03-15` or even `08:30`. When a time is
missing, the first instant of the corresponding day is used. (Which is usually
midnight, but not always, for example `2015-10-18` in `America/Sao_Paulo`.)
When a date is missing, the current date is used, even if that would result
in a datetime in the past. In both cases, the datetime is then interpreted as
a local time in your system's configured time zone (which may be overridden
by the `TZ` environment variable).

A datetime expressed as a duration from the current time. For example, to get
1 day from the current time, you can use `1 day`, or more succinctly, `1d`. To
get 1 day in the past from the current time, you can use `1 day ago`, or more
succinctly, `-1d`. Mixing calendar and time units is allowed, for example, `1
year 1 second` or `1y1s`.

Some special strings are supported as well:

`now` refers to the current datetime to the highest precision supported by
your system. The current datetime is computed once when bttf starts, or if the
`BTTF_NOW` environment variable is set, that time is used instead.

`today` refers to the first instant of the current day.

`yesterday` refers to the first instant of the previous day.

`tomorrow` refers to the first instant of the next day.

Other examples of things that work:

`this thurs` refers to the current day (if it's a Thursday) or the soonest
date that falls on a Thursday.

`last FRIDAY` refers to the previously occurring Friday, up to 1 week in the
past (if the current day is a Friday).

`next saturday` refers to the next Saturday, up to 1 week in the future (if
the current day is a Saturday).

`5pm tomorrow`, `5pm next Wed` or `5pm 1 week` refer to 5pm tomorrow, 5pm on
the next Wednesday or 5pm in 1 week from today.
"#,
    );

    pub fn now() -> DateTime {
        DateTime { zdt: NOW.clone() }
    }

    /// Get the underlying Jiff zoned date time.
    ///
    /// If possible, prefer defining an operation on `DateTime` instead of
    /// using a `ZonedDateTime` directly. This helps centralize the operations
    /// we need, and also helps encourage consistent error reporting.
    pub fn get(&self) -> &ZonedDateTime {
        &self.zdt
    }

    pub fn since(
        &self,
        largest: Unit,
        dt: &DateTime,
    ) -> anyhow::Result<TimeSpan> {
        self.zdt
            .since((largest, &dt.zdt))
            .with_context(|| format!("failed to find span relative to {dt}"))
            .map(TimeSpan::from)
    }

    pub fn until(
        &self,
        largest: Unit,
        dt: &DateTime,
    ) -> anyhow::Result<TimeSpan> {
        self.zdt
            .until((largest, &dt.zdt))
            .with_context(|| format!("failed to find span relative to {dt}"))
            .map(TimeSpan::from)
    }

    pub fn add(&self, span: &TimeSpan) -> anyhow::Result<DateTime> {
        self.zdt
            .checked_add(span.get())
            .with_context(|| format!("failed to add {span} to {self}"))
            .map(DateTime::from)
    }

    pub fn in_tz(&self, tz: &TimeZone) -> DateTime {
        DateTime { zdt: self.zdt.with_time_zone(tz.get().clone()) }
    }

    pub fn tz_preceding(
        &self,
        tz: &TimeZone,
    ) -> impl Iterator<Item = DateTime> {
        tz.get()
            .preceding(self.zdt.timestamp())
            .map(|t| t.timestamp().to_zoned_datetime(tz.get().clone()).into())
    }

    pub fn tz_following(
        &self,
        tz: &TimeZone,
    ) -> impl Iterator<Item = DateTime> {
        tz.get()
            .following(self.zdt.timestamp())
            .map(|t| t.timestamp().to_zoned_datetime(tz.get().clone()).into())
    }

    /// Returns the instant immediately before this one.
    pub fn instant_before(&self) -> anyhow::Result<DateTime> {
        self.zdt
            .checked_sub(1.nanosecond())
            .with_context(|| {
                format!("failed to find instant immediately before {}", self)
            })
            .map(DateTime::from)
    }

    /// Returns the instant immediately after this one.
    pub fn instant_after(&self) -> anyhow::Result<DateTime> {
        self.zdt
            .checked_add(1.nanosecond())
            .with_context(|| {
                format!("failed to find instant immediately after {}", self)
            })
            .map(DateTime::from)
    }
}

impl Default for DateTime {
    fn default() -> DateTime {
        DateTime::now()
    }
}

impl From<ZonedDateTime> for DateTime {
    fn from(zdt: ZonedDateTime) -> DateTime {
        DateTime { zdt }
    }
}

impl From<DateTimeFlexible> for DateTime {
    fn from(dt: DateTimeFlexible) -> DateTime {
        DateTime { zdt: dt.zdt }
    }
}

impl std::fmt::Display for DateTime {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.zdt, f)
    }
}

impl std::str::FromStr for DateTime {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<DateTime> {
        s.as_bytes().parse()
    }
}

impl FromBytes for DateTime {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<DateTime> {
        // We attempt the most specific thing first: an RFC 9557
        // timestamp with a time zone annotation.
        //
        // We do keep the error for this around, since if we later
        // find out that we did have a time zone annotation but
        // something else about it was invalid, then we'll want to
        // return this error.
        let temporal_parse_err = match TEMPORAL_PARSER.parse_zoned_datetime(s)
        {
            Err(err) => err,
            Ok(zdt) => return Ok(DateTime::from(zdt)),
        };
        // This looks a lot like what we do in flexible parsing, except we
        // only permit RFC 3339 timestamps (or things resembling it) here.
        // In particular, we reject dates or datetimes without an offset.
        // Specifically, we very intentionally never use `TZ` or `NOW` either
        // here or indirectly.
        if let Ok(pieces) = fmt::temporal::Pieces::parse(s) {
            // If we parsed a time zone annotation, that means
            // the RFC 9557 parse failed for exciting reasons.
            // Like perhaps, an offset inconsistent with the
            // time zone. Or an invalid time zone name. So we
            // should just return the error that we got above.
            if pieces.time_zone_annotation().is_some() {
                return Err(temporal_parse_err.into());
            }
            let date = pieces.date();
            let time = pieces.time().unwrap_or(civil::Time::midnight());
            let dt = date.to_datetime(time);
            let zdt = match pieces.offset() {
                // This is the case that's different from flexible datetime
                // parsing. If an offset is missing, then this can't be
                // RFC 3339, so we give up.
                None => anyhow::bail!(
                    "RFC 3339 timestamp requires an offset, \
                     but {s} is missing an offset",
                    s = s.as_bstr(),
                ),
                Some(fmt::temporal::PiecesOffset::Zulu) => {
                    dt.to_zoned_datetime(tz::TimeZone::unknown())?
                }
                Some(fmt::temporal::PiecesOffset::Numeric(ref off)) => {
                    if off.offset() == Offset::UTC && off.is_negative() {
                        dt.to_zoned_datetime(tz::TimeZone::unknown())?
                    } else {
                        dt.to_zoned_datetime(tz::TimeZone::fixed(
                            off.offset(),
                        ))?
                    }
                }
                Some(unk) => {
                    anyhow::bail!("unrecognized parsed offset: {unk:?}")
                }
            };
            return Ok(DateTime::from(zdt));
        }
        // N.B. This also includes RFC 9110.
        if let Ok(zdt) = RFC2822_PARSER.parse_zoned_datetime(s) {
            return Ok(DateTime::from(zdt));
        }
        anyhow::bail!("unrecognized datetime `{s}`", s = BStr::new(s))
    }
}

impl serde::Serialize for DateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for DateTime {
    #[inline]
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime, D::Error> {
        use serde::de;

        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = DateTime;

            fn expecting(
                &self,
                f: &mut core::fmt::Formatter,
            ) -> core::fmt::Result {
                f.write_str("a datetime string")
            }

            #[inline]
            fn visit_bytes<E: de::Error>(
                self,
                value: &[u8],
            ) -> Result<DateTime, E> {
                value.parse().map_err(de::Error::custom)
            }

            #[inline]
            fn visit_str<E: de::Error>(
                self,
                value: &str,
            ) -> Result<DateTime, E> {
                self.visit_bytes(value.as_bytes())
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Represents a bttf "datetime" parsed on the CLI.
///
/// This is only for parsing datetimes given to the CLI as positional
/// arguments. In this context, we support specifying datetimes that are
/// implicitly relative to now, e.g., `now` and `-10s` and `1day`.
///
/// Callers should only use this type as a target for parsing. Then you'll
/// want to convert it to `DateTime` (via the `From` impl) as soon as you can.
#[derive(Clone, Debug)]
pub struct DateTimeFlexible {
    /// The actual parsed datetime. i.e., The thing we operate on.
    zdt: ZonedDateTime,
}

impl DateTimeFlexible {
    /// Parses a "flexible" datetime.
    ///
    /// This supports more formats. For example, `-1d` or `next thurs`. When
    /// a relative format is found, it is interpreted relative to the zoned
    /// datetime given.
    ///
    /// This types `FromStr` and `FromBytes` impls are equivalent to calling
    /// this routine with `&crate::NOW`.
    pub fn parse_relative(
        relative: &ZonedDateTime,
        s: &[u8],
    ) -> anyhow::Result<DateTimeFlexible> {
        // First try to parse something that is definitive. If it fails,
        // keep the error and we'll report it below if everything else fails.
        // We specifically try parsing a zoned datetime since my guess is
        // that it's the common case in bttf shell pipelines.
        let temporal_parse_err = match TEMPORAL_PARSER.parse_zoned_datetime(s)
        {
            Err(err) => err,
            Ok(zdt) => return Ok(DateTimeFlexible::from(zdt)),
        };
        // This is somewhat similar to non-flexible parsing, except we'll
        // happily use `TZ` when no offset is found. The non-flexible case
        // requires an unambiguous instant.
        if let Ok(pieces) = fmt::temporal::Pieces::parse(s) {
            // If we parsed a time zone annotation, that means the
            // RFC 9557 parse failed above for exciting reasons. Like
            // perhaps, an offset inconsistent with the time zone. Or
            // an invalid time zone name. So we should just return the
            // error that we got above.
            if pieces.time_zone_annotation().is_some() {
                return Err(temporal_parse_err.into());
            }
            let date = pieces.date();
            let time = pieces.time().unwrap_or(civil::Time::midnight());
            let dt = date.to_datetime(time);
            let zdt = match pieces.offset() {
                None => dt.to_zoned_datetime(TZ.clone())?,
                Some(fmt::temporal::PiecesOffset::Zulu) => {
                    dt.to_zoned_datetime(tz::TimeZone::unknown())?
                }
                Some(fmt::temporal::PiecesOffset::Numeric(ref off)) => {
                    if off.offset() == Offset::UTC && off.is_negative() {
                        dt.to_zoned_datetime(tz::TimeZone::unknown())?
                    } else {
                        dt.to_zoned_datetime(tz::TimeZone::fixed(
                            off.offset(),
                        ))?
                    }
                }
                Some(unk) => {
                    anyhow::bail!("unrecognized parsed offset: {unk:?}")
                }
            };
            return Ok(DateTimeFlexible { zdt });
        }
        // N.B. This also includes RFC 9110.
        if let Ok(zdt) = RFC2822_PARSER.parse_zoned_datetime(s) {
            return Ok(DateTimeFlexible::from(zdt));
        }
        // Now try parsing a relative datetime.
        if let Some(zdt) = parse_relative(relative, s.as_bstr())? {
            return Ok(DateTimeFlexible::from(zdt));
        }
        // Not really sure how to do good error reporting here, since the
        // format is so flexible. We'd somehow need to invest more work into
        // "guessing" the user's intent. Which might mean actually hand-rolling
        // our own holistic parser here, instead of piecing together a bunch of
        // disparate parsers.
        anyhow::bail!("unrecognized datetime `{s}`", s = BStr::new(s))
    }

    /// Parses a strictly relative datetime.
    ///
    /// This only supports relative datetime formats. For example, `-1d` or
    /// `next thurs`. When a relative format is found, it is interpreted
    /// relative to the zoned datetime given.
    pub fn parse_only_relative(
        relative: &ZonedDateTime,
        s: &[u8],
    ) -> anyhow::Result<DateTimeFlexible> {
        if let Some(zdt) = parse_relative(relative, s.as_bstr())? {
            return Ok(DateTimeFlexible::from(zdt));
        }
        anyhow::bail!("unrecognized relative datetime `{s}`", s = BStr::new(s))
    }
}

impl From<ZonedDateTime> for DateTimeFlexible {
    fn from(zdt: ZonedDateTime) -> DateTimeFlexible {
        DateTimeFlexible { zdt }
    }
}

impl From<DateTimeFlexible> for ZonedDateTime {
    fn from(dt: DateTimeFlexible) -> ZonedDateTime {
        dt.zdt
    }
}

impl std::str::FromStr for DateTimeFlexible {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<DateTimeFlexible> {
        s.as_bytes().parse()
    }
}

impl FromBytes for DateTimeFlexible {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<DateTimeFlexible> {
        DateTimeFlexible::parse_relative(&NOW, s)
    }
}

/// Tries to parse a datetime in `s` relative to the one given.
///
/// If one could not be found, then `None` is returned. If one is definitively
/// found, but it could not be processed into a zoned datetime for some
/// reason, then an error is returned.
fn parse_relative(
    relative: &ZonedDateTime,
    s: &BStr,
) -> anyhow::Result<Option<ZonedDateTime>> {
    match &**s {
        b"now" => return Ok(Some(relative.clone())),
        b"today" => return Ok(Some(relative.start_of_day()?)),
        b"yesterday" => {
            return Ok(Some(relative.yesterday()?.start_of_day()?));
        }
        b"tomorrow" => return Ok(Some(relative.tomorrow()?.start_of_day()?)),
        _ => {}
    }
    let mut relative = Cow::Borrowed(relative);
    let Some((first, rest)) = s.split_once_str(" ") else {
        // If we have zero spaces, then we are pretty limited in what this
        // could be. It could be a friendly duration, a time or just a weekday.
        //
        // And note that we should try parsing a relative duration after a
        // time, since, e.g., `14:30:00` is a valid friendly duration, but we
        // should parse that as a time.
        return Ok(if let Some(zdt) = parse_time(&relative, s)? {
            Some(zdt)
        } else if let Some(zdt) = parse_friendly(&relative, s)? {
            Some(zdt)
        } else if let Some(zdt) = parse_day(&relative, s)? {
            Some(zdt)
        } else if let Ok(wd) = s.parse::<Weekday>() {
            Some(relative_weekday(&relative, 0, wd)?)
        } else {
            None
        });
    };

    let mut multiplier = 0;
    if let Some(zdt) = parse_time(&relative, first.as_bstr())? {
        relative = Cow::Owned(zdt);
        if let Some(zdt) = parse_friendly(&relative, rest.as_bstr())? {
            return Ok(Some(zdt));
        }
        let Some((first, rest)) = rest.split_once_str(" ") else {
            return Ok(if let Ok(wd) = rest.parse::<Weekday>() {
                Some(relative_weekday(&relative, 0, wd)?)
            } else if let Some(zdt) = parse_day(&relative, rest.as_bstr())? {
                Some(zdt)
            } else {
                None
            });
        };
        if let Some(n) = parse_multiplier(first.as_bstr())? {
            multiplier = n;
        }
        if let Ok(wd) = rest.parse::<Weekday>() {
            return Ok(Some(relative_weekday(&relative, multiplier, wd)?));
        }
        return Ok(None);
    } else if let Some(zdt) = parse_friendly(&relative, s)? {
        return Ok(Some(zdt));
    } else if let Some(n) = parse_multiplier(first.as_bstr())? {
        multiplier = n;
        if let Ok(wd) = rest.parse::<Weekday>() {
            return Ok(Some(relative_weekday(&relative, multiplier, wd)?));
        }
        return Ok(None);
    }
    Ok(None)
}

/// Finds the next/previous weekday relative to the datetime given.
///
/// The multiplier refers to the "nth" weekday, with a negative multiplier
/// going back in time.
///
/// The zeroth multiplier is a little special. In this case, if the given
/// zoned datetime falls on the given weekday, then the zoned datetime is
/// returned unchanged.
fn relative_weekday(
    relative: &ZonedDateTime,
    mut multiplier: i32,
    weekday: Weekday,
) -> anyhow::Result<ZonedDateTime> {
    if multiplier == 0 {
        if relative.weekday() == weekday.get() {
            return Ok(relative.clone());
        }
        multiplier = 1;
    }
    relative.nth_weekday(multiplier, weekday.get()).with_context(|| {
        format!("failed to get {multiplier} {weekday}s after {relative}")
    })
}

/// Parse a description of a day from `s` (today, yesterday or tomorrow)
/// relative to the datetime given.
fn parse_day(
    relative: &ZonedDateTime,
    s: &BStr,
) -> anyhow::Result<Option<ZonedDateTime>> {
    match &**s {
        b"today" => Ok(Some(relative.clone())),
        b"yesterday" => Ok(Some(relative.yesterday()?)),
        b"tomorrow" => Ok(Some(relative.tomorrow()?)),
        _ => Ok(None),
    }
}

/// Attempts to parse `s` as a multiplier.
///
/// A multiplier can be a signed integer or an English word standing in for
/// a signed integer. Examples:
///
/// * `this` means `0`
/// * `last` means `-1`
/// * `next` means `1`
/// * `first` means `1`
///
/// Note that since this parses a signed integer, it may be ambiguous with a
/// friendly a duration. So before using this, callers should ensure that a
/// friendly duration cannot be parsed.
fn parse_multiplier(s: &BStr) -> anyhow::Result<Option<i32>> {
    if let Ok(n) = parse_i64(s) {
        let n = i32::try_from(n).with_context(|| {
            format!("parsed `{n}` as a integer multiplier, but it's too big")
        })?;
        return Ok(Some(n));
    }
    Ok(Some(match &*s.to_ascii_lowercase() {
        b"this" => 0,
        b"last" => -1,
        b"next" => 1,
        b"first" => 1,
        b"second" => 2,
        b"third" => 3,
        b"fourth" => 4,
        b"fifth" => 5,
        b"sixth" => 6,
        b"seventh" => 7,
        b"eighth" => 8,
        b"ninth" => 9,
        b"tenth" => 10,
        _ => return Ok(None),
    }))
}

/// Parses a friendly duration as a relative datetime.
fn parse_friendly(
    relative: &ZonedDateTime,
    s: &BStr,
) -> anyhow::Result<Option<ZonedDateTime>> {
    if let Ok(span) = s.parse::<TimeSpan>() {
        let zdt = relative.checked_add(span.get()).with_context(|| {
            format!("failed to add `{span:#}` to `{relative}`")
        })?;
        return Ok(Some(zdt));
    }
    Ok(None)
}

/// Parses one of a variety of different clock times, including am/pm.
fn parse_time(
    relative: &ZonedDateTime,
    s: &BStr,
) -> anyhow::Result<Option<ZonedDateTime>> {
    static FORMATS: &[&str] =
        &["%I:%M:%S%P", "%I:%M%P", "%I%P", "%H:%M:%S", "%H:%M"];

    for fmt in FORMATS {
        if let Ok(time) = civil::Time::strptime(fmt, s) {
            return Ok(Some(relative.with().time(time).build()?));
        }
    }
    Ok(None)
}

/// Parses a signed 64-bit integer.
fn parse_i64(bytes: &BStr) -> anyhow::Result<i64> {
    let (sign, bytes) = match bytes.split_first() {
        None => anyhow::bail!("invalid number, no digits found"),
        Some((&b'+', rest)) => (1, rest.as_bstr()),
        Some((&b'-', rest)) => (-1, rest.as_bstr()),
        Some(_) => (1, bytes),
    };
    let mut n: i64 = 0;
    for byte in bytes.bytes() {
        let digit = match byte.checked_sub(b'0') {
            None => {
                anyhow::bail!(
                    "invalid digit, expected 0-9 but got {}",
                    [byte].as_bstr(),
                );
            }
            Some(digit) if digit > 9 => {
                anyhow::bail!(
                    "invalid digit, expected 0-9 but got {}",
                    [byte].as_bstr(),
                );
            }
            Some(digit) => {
                debug_assert!((0..=9).contains(&digit));
                i64::from(digit)
            }
        };
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(digit))
            .with_context(|| {
                format!(
                    "number `{}` too big to parse into 64-bit integer",
                    bytes.as_bstr(),
                )
            })?;
    }
    n = n.checked_mul(sign).with_context(|| {
        format!(
            "number `{}` too big to parse into 64-bit integer",
            bytes.as_bstr(),
        )
    })?;
    Ok(n)
}
