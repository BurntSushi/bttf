use std::{
    ffi::OsString,
    fs::File,
    io,
    num::NonZero,
    path::{Path, PathBuf},
};

use {
    anyhow::Context,
    bstr::{BStr, ByteSlice},
    jiff::{
        ZonedDateTime, civil, fmt,
        tz::{self, Offset},
    },
};

use crate::{
    args::{Configurable, Usage},
    datetime::{DateTime, DateTimeFlexible},
    ical::ByWeekday,
    locale::StrtimeConfig,
    parse::{BytesExt, FromBytes},
    span::TimeSpan,
    timezone::TimeZone,
};

/// A convenience type for parsing *either* a flexible datetime or a span.
///
/// By attempting to parse a span first, this specifically rules out the
/// ability to parse a relative datetime.
///
/// For usage docs on a positional argument, callers should use the usage docs
/// for both `DateTime` and `TimeSpan`.
///
/// This should only be used for flags/arguments given on the CLI, since this
/// will always attempt to parse a flexible datetime. While the above of course
/// excludes relative datetimes, this still allows things like `now` and
/// `14:30`.
#[derive(Clone, Debug)]
pub enum DateTimeOrSpan {
    DateTime(DateTime),
    TimeSpan(TimeSpan),
}

impl FromBytes for DateTimeOrSpan {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<DateTimeOrSpan> {
        // The error reporting here kind of sucks, because if the user
        // *meant* to provide a span, then we'll never provide a span
        // parsing error here. I suppose this could probably be improved
        // with some heuristic regexes that tries to guess the intent, but
        // that seems a little tricky without concrete use cases. We do at
        // least provide a DEBUG-level log, but I'm not sure that's good
        // enough.
        match s.parse::<TimeSpan>() {
            Ok(span) => return Ok(DateTimeOrSpan::TimeSpan(span)),
            Err(err) => {
                log::debug!(
                    "failed to parse `{s}` as time span, \
                     falling back to parsing datetime: {err}",
                    s = s.as_bstr(),
                );
            }
        }
        let dt: DateTimeFlexible = s
            .parse()
            .context("failed to parse as datetime or as time span")?;
        Ok(DateTimeOrSpan::DateTime(dt.into()))
    }
}

/// A convenience type for parsing *either* a flexible datetime or a time zone.
///
/// Generally speaking, a datetime and a time zone ought to have no overlap.
/// So this doesn't have the same challenges as `DateTimeOrSpan`.
///
/// For usage docs on a positional argument, callers should use the usage docs
/// for both `DateTime` and `TimeZone`.
///
/// This should only be used for flags/arguments given on the CLI, since this
/// will always attempt to parse a flexible datetime. While the above of course
/// excludes relative datetimes, this still allows things like `now` and
/// `14:30`.
#[derive(Clone, Debug)]
pub enum DateTimeOrTimeZone {
    DateTime(DateTime),
    TimeZone(TimeZone),
}

impl FromBytes for DateTimeOrTimeZone {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<DateTimeOrTimeZone> {
        // If anything parses as a datetime, then we can be pretty sure that
        // it isn't a time zone. One thing that's somewhat close is time zone
        // offsets. But thankfully, time zone offsets require a leading sign,
        // such that `13:00` on its own is unambiguously a time and not a time
        // zone offset.
        let datetime_err = match s.parse::<DateTimeFlexible>() {
            Ok(dt) => return Ok(DateTimeOrTimeZone::DateTime(dt.into())),
            Err(err) => err,
        };

        match s.parse::<TimeZone>() {
            Ok(tz) => Ok(DateTimeOrTimeZone::TimeZone(tz)),
            Err(tz_err) => {
                // At this point, we've failed to parse a datetime AND failed
                // to parse a time zone. But which error do we show? Showing
                // both is kind of a bummer, so we try to guess the user's
                // intent. We do log both errors though.
                log::debug!(
                    "failed to parse `{s}` as time zone: {tz_err}",
                    s = s.as_bstr(),
                );
                log::debug!(
                    "ALSO failed to parse `{s}` as datetime: {datetime_err}",
                    s = s.as_bstr(),
                );

                // If it's empty for whatever reason, we specialize that case.
                anyhow::ensure!(
                    !s.is_empty(),
                    "an empty string is neither a valid time zone \
                     nor a valid datetime",
                );

                // If the string contains a `/`, it's almost certainly
                // an IANA time zone identifier. Unless there's a `[, in which
                // case, the IAAN id might be inside a TZ annotation of an
                // RFC 9557 timestamp.
                if s.contains_str("/") && !s.contains_str("[") {
                    return Err(tz_err);
                }
                // If it starts with a `+` or a `-`, then it's probably a time
                // zone offset.
                if s.starts_with_str("+") || s.starts_with_str("-") {
                    return Err(tz_err);
                }
                // If it starts with a capital letter and is otherwise entirely
                // ASCII letters, it's probably an IANA id. e.g., `Isreal`.
                if s[0].is_ascii_uppercase()
                    && s.bytes().all(|b| b.is_ascii_alphabetic())
                {
                    return Err(tz_err);
                }
                // Otherwise, give up and report the datetime error.
                Err(datetime_err)
            }
        }
    }
}

/// Provides parsing for bttf's possible set of formats for a datetime.
#[derive(Clone, Debug, Default)]
pub enum Format {
    /// Formats or parses as an RFC 9557 timestamp.
    #[default]
    Rfc9557,
    /// Formats or parses as an RFC 3339 timestamp.
    Rfc3339,
    /// Formats or parses as an RFC 2822 timestamp.
    Rfc2822,
    /// Formats or parses as an RFC 9110 timestamp.
    Rfc9110,
    /// Formats or parses via the `strftime` or `strptime` functions.
    Strtime(Box<str>),
    /// Parses in the "flexible" format.
    ///
    /// This is the same format used for accepting datetimes
    /// on the CLI via positional parameters or flags. The
    /// benefit here is that it can be used to parse datetimes
    /// on stdin.
    ///
    /// When used for formatting, this results in an error.
    Flexible,
}

impl Format {
    pub const USAGE_PRINT: Usage = Usage::flag(
        "-f, --format <kind>",
        "Print datetimes in this format.",
        r#"
Print datetimes in this format.

The legal values for this flag are: `rfc9557` (default), `rfc3339`, `rfc2822`,
`rfc9110` or a `strftime`-style string.

Here are some examples of each type of format:

RFC 9557: `2025-03-15T10:23:00-04:00[America/New_York]`

RFC 3339: `2025-03-15T10:23:00-04:00`

RFC 2822: `Sat, 15 Mar 2025 10:23:00 -0400`

RFC 9110: `Sat, 15 Mar 2025 14:23:00 GMT`

Otherwise, an `strftime`-style format string may be given. For example, the
format string `%A %Y-%m-%d %H:%M:%S %:z %Z %Q` would produce something like
`Saturday 2025-03-15 10:23:00 -04:00 EDT America/New_York`.

In general, the `strftime` format directives supported should generally
match what you'd expect from your POSIX `strftime` implementation. However,
no explicit guarantee of compatibility is provided. Notable omissions, at
present, include locale-aware directives like `%c`, `%x` and `%X`.

Here is the full set of supported formatting directives:

`%%`: A literal `%`.

`%A`, `%a`: The full and abbreviated weekday, respectively.

`%B`, `%b`, `%h`: The full and abbreviated month name, respectively.

`%C`: The century of the year. No padding.

`%c`: The date and clock time via the current locale set by the `BTTF_LOCALE`
environment variable. Supported when formatting only.

`%D`: Equivalent to `%m/%d/%y`.

`%d`, `%e`: The day of the month. `%d` is zero-padded, `%e` is space padded.

`%F`: Equivalent to `%Y-%m-%d`.

`%f`: Fractional seconds, up to nanosecond precision.

`%.f`: Optional fractional seconds, with dot, up to nanosecond precision.

`%G`: An ISO 8601 week-based year. Zero padded to 4 digits.

`%g`: A two-digit ISO 8601 week-based year. Represents only 1969-2068. Zero
padded.

`%H`: The hour in a 24 hour clock. Zero padded.

`%I`: The hour in a 12 hour clock. Zero padded.

`%j`: The day of the year. Range is `1..=366`. Zero padded to 3 digits.

`%k`: The hour in a 24 hour clock. Space padded.

`%l`: The hour in a 12 hour clock. Space padded.

`%M`: The minute. Zero padded.

`%m`: The month. Zero padded.

`%n`: Formats as a newline character. Parses arbitrary whitespace.

`%P`: Whether the time is in the AM or PM, lowercase.

`%p`: Whether the time is in the AM or PM, uppercase.

`%Q`: An IANA time zone identifier, or `%z` if one doesn't exist.

`%:Q`: An IANA time zone identifier, or `%:z` if one doesn't exist.

`%q`: The quarter of the year.

`%R`: Equivalent to `%H:%M`.

`%r`: The 12-hour clock time via the current locale set by the `BTTF_LOCALE`
environment variable. Supported when formatting only.

`%S`: The second. Zero padded.

`%s`: A Unix timestamp, in seconds.

`%T`: Equivalent to `%H:%M:%S`.

`%t`: Formats as a tab character. Parses arbitrary whitespace.

`%U`: Week number. Week 1 is the first week starting with a Sunday. Zero
padded.

`%u`: The day of the week beginning with Monday at `1`.

`%V`: Week number in the ISO 8601 week-based calendar. Zero padded.

`%W`: Week number. Week 1 is the first week starting with a Monday. Zero
padded.

`%w`: The day of the week beginning with Sunday at `0`.

`%X`: The clock time via the current locale set by the `BTTF_LOCALE`
environment variable. Supported when formatting only.

`%x`: The date via the current locale set by the `BTTF_LOCALE`
environment variable. Supported when formatting only.

`%Y`: A full year, including century. Zero padded to 4 digits.

`%y`: A two-digit year. Represents only 1969-2068. Zero padded.

`%Z`: A time zone abbreviation. Supported when formatting only.

`%z`: A time zone offset in the format `[+-]HHMM[SS]`.

`%:z`: A time zone offset in the format `[+-]HH:MM[:SS]`.

`%::z`: A time zone offset in the format `[+-]HH:MM:SS`.

`%:::z`: A time zone offset in the format `[+-]HH:[MM[:SS]]`. The minute and
second components are only written when required. That is, this formats the
time zone offset to the necessary precision.

The following flags can be inserted immediately after the `%` and before the
directive:

`_`: Pad a numeric result to the left with spaces.

`-`: Do not pad a numeric result.

`0`: Pad a numeric result to the left with zeros.

`^`: Use alphabetic uppercase for all relevant strings.

`#`: Swap the case of the result string. This is typically only useful with
`%p` or `%Z`, since they are the only conversion specifiers that emit strings
entirely in uppercase by default.

The above flags override the "default" settings of a specifier. For example,
`%_d` pads with spaces instead of zeros, and `%0e` pads with zeros instead of
spaces. The exceptions are the locale (`%c`, `%r`, `%X`, `%x`), and time zone
(`%z`, `%:z`) specifiers. They are unaffected by any flags.

Moreover, any number of decimal digits can be inserted after the (possibly
absent) flag and before the directive, so long as the parsed number is less
than 256. The number formed by these digits will correspond to the minimum
amount of padding (to the left).

The `%f` and `%.f` flags also support specifying the precision, up to
nanoseconds. For example, `%3f` and `%.3f` will both always print a fractional
second component to exactly 3 decimal places. When no precision is specified,
then `%f` will always emit at least one digit, even if it's zero. But `%.f`
will emit the empty string when the fractional component is zero. Otherwise,
it will include the leading `.`. When using a precision setting, truncation
is used. If you need a different rounding mode, you should use a command like
`bttf time round` first before formatting.
"#,
    );

    pub const USAGE_PARSE: Usage = Usage::flag(
        "-f, --format <kind>",
        "Parse datetimes in this format.",
        r#"
Parse datetimes in this format.

The legal values for this flag are: `rfc9557` (default), `rfc3339`, `rfc2822`,
`rfc9110`, `flexible` or a `strptime`-style string.

Here are some examples of each type of format:

RFC 9557: `2025-03-15T10:23:00-04:00[America/New_York]`

RFC 3339: `2025-03-15T10:23:00-04:00`

RFC 2822: `Sat, 15 Mar 2025 10:23:00 -0400`

RFC 9110: `Sat, 15 Mar 2025 14:23:00 GMT`

Flexible: `next sat`, `9pm 1 week ago`

The flexible format accepts the same relative datetime format that bttf accepts
anywhere users can provide datetimes on the command line (either via positional
or flag arguments). In order to avoid footguns, bttf specifically prohibits
the relative datetime format when datetimes are provided by stdin. Instead,
datetimes on stdin must always be unambiguous instants in time (via RFC 9557,
RFC 3339, RFC 2822 or RFC 9110). But this command permits explicitly opting
into the flexible format and parsing it regardless of where it comes from. By
default, when a relative description of a date like `1 hour ago` is parsed,
it is interpreted relative to the current time. But this can be overridden with
the `-r/--relative` flag.

Otherwise, an `strptime`-style format string may be given. For example, the
format string `%A %Y-%m-%d %H:%M:%S %:z %Q` would parse
`Saturday 2025-03-15 10:23:00 -04:00 America/New_York`.

In general, the `strptime` format directives supported should generally
match what you'd expect from your POSIX `strptime` implementation. However,
no explicit guarantee of compatibility is provided. Notable omissions, at
present, include locale-aware directives like `%c`, `%x` and `%X`.

Here is the full set of supported parsing directives:

`%%`: A literal `%`.

`%A`, `%a`: The full and abbreviated weekday, respectively.

`%B`, `%b`, `%h`: The full and abbreviated month name, respectively.

`%C`: The century of the year. No padding.

`%D`: Equivalent to `%m/%d/%y`.

`%d`, `%e`: The day of the month. `%d` is zero-padded, `%e` is space padded.

`%F`: Equivalent to `%Y-%m-%d`.

`%f`: Fractional seconds, up to nanosecond precision.

`%.f`: Optional fractional seconds, with dot, up to nanosecond precision.

`%G`: An ISO 8601 week-based year. Zero padded to 4 digits.

`%g`: A two-digit ISO 8601 week-based year. Represents only 1969-2068. Zero
padded.

`%H`: The hour in a 24 hour clock. Zero padded.

`%I`: The hour in a 12 hour clock. Zero padded.

`%j`: The day of the year. Range is `1..=366`. Zero padded to 3 digits.

`%k`: The hour in a 24 hour clock. Space padded.

`%l`: The hour in a 12 hour clock. Space padded.

`%M`: The minute. Zero padded.

`%m`: The month. Zero padded.

`%n`: Formats as a newline character. Parses arbitrary whitespace.

`%P`: Whether the time is in the AM or PM, lowercase.

`%p`: Whether the time is in the AM or PM, uppercase.

`%Q`: An IANA time zone identifier, or `%z` if one doesn't exist.

`%:Q`: An IANA time zone identifier, or `%:z` if one doesn't exist.

`%R`: Equivalent to `%H:%M`.

`%S`: The second. Zero padded.

`%s`: A Unix timestamp, in seconds.

`%T`: Equivalent to `%H:%M:%S`.

`%t`: Formats as a tab character. Parses arbitrary whitespace.

`%U`: Week number. Week 1 is the first week starting with a Sunday. Zero
padded.

`%u`: The day of the week beginning with Monday at `1`.

`%V`: Week number in the ISO 8601 week-based calendar. Zero padded.

`%W`: Week number. Week 1 is the first week starting with a Monday. Zero
padded.

`%w`: The day of the week beginning with Sunday at `0`.

`%Y`: A full year, including century. Zero padded to 4 digits.

`%y`: A two-digit year. Represents only 1969-2068. Zero padded.

`%z`: A time zone offset in the format `[+-]HHMM[SS]`.

`%:z`: A time zone offset in the format `[+-]HH:MM[:SS]`.

`%::z`: A time zone offset in the format `[+-]HH:MM:SS`.

`%:::z`: A time zone offset in the format `[+-]HH:[MM[:SS]]`. The minute and
second components are parsed when present but are otherwise optional.

Note that the above list is a proper subset of the corresponding formatting
directives for `strftime`. Namely, `%Z` (a time zone abbreviation) is only
available for formatting and not for parsing. This is because time zone
abbreviations are ambiguous and cannot be reliably resolved to a specific time
zone absent other context.

The following flags can be inserted immediately after the `%` and before the
directive:

`_`: Pad a numeric result to the left with spaces.

`-`: Do not pad a numeric result.

`0`: Pad a numeric result to the left with zeros.

`^`: Use alphabetic uppercase for all relevant strings.

`#`: Swap the case of the result string. This is typically only useful with
`%p` or `%Z`, since they are the only conversion specifiers that emit strings
entirely in uppercase by default.

In general, most of the above flags are only applicable for formatting
datetimes and not parsing. They are still accepted when parsing for consistency
reasons. However, the padding option does impact parsing. For example, if one
wanted to parse `003` as the day `3`, then one should use `%03d`. Otherwise, by
default, `%d` will only try to consume at most 2 digits.
"#,
    );

    pub fn format(
        &self,
        config: &StrtimeConfig,
        dt: &DateTime,
    ) -> anyhow::Result<String> {
        self.format_impl(config, dt).with_context(|| {
            format!("formatting datetime `{}` for format {} failed", dt, self)
        })
    }

    pub fn parse(
        &self,
        relative: &DateTime,
        dt: &BStr,
    ) -> anyhow::Result<DateTime> {
        self.parse_impl(relative.get(), dt)
            .with_context(|| {
                format!("parsing datetime `{}` for format {} failed", dt, self)
            })
            .map(DateTime::from)
    }

    fn format_impl(
        &self,
        config: &StrtimeConfig,
        dt: &DateTime,
    ) -> anyhow::Result<String> {
        static RFC2822: fmt::rfc2822::DateTimePrinter =
            fmt::rfc2822::DateTimePrinter::new();

        let zdt = dt.get();
        Ok(match *self {
            Format::Rfc9557 => zdt.to_string(),
            Format::Rfc3339 => {
                if zdt.time_zone().is_unknown() {
                    zdt.timestamp().to_string()
                } else {
                    zdt.timestamp()
                        .display_with_offset(zdt.offset())
                        .to_string()
                }
            }
            Format::Rfc2822 => RFC2822
                .zoned_datetime_to_string(zdt)
                .context("RFC 2822 formatting failed")?,
            Format::Rfc9110 => RFC2822
                .timestamp_to_rfc9110_string(&zdt.timestamp())
                .context("RFC 9110 formatting failed")?,
            Format::Strtime(ref fmt) => {
                let tm = fmt::strtime::BrokenDownTime::from(zdt);
                tm.to_string_with_config(config, &**fmt)?
            }
            Format::Flexible => anyhow::bail!(
                "flexible format not allowed when formatting a datetime",
            ),
        })
    }

    fn parse_impl(
        &self,
        relative: &ZonedDateTime,
        dt: &BStr,
    ) -> anyhow::Result<ZonedDateTime> {
        static TEMPORAL_PARSER: fmt::temporal::DateTimeParser =
            fmt::temporal::DateTimeParser::new();
        static RFC2822_PARSER: fmt::rfc2822::DateTimeParser =
            fmt::rfc2822::DateTimeParser::new();

        Ok(match *self {
            Format::Rfc9557 => TEMPORAL_PARSER.parse_zoned_datetime(dt)?,
            Format::Rfc3339 => {
                // This is a little weird, but we try to stick specifically
                // to RFC 3339 here. Since bttf's "default" datetime type
                // is RFC 9557, and we don't want to lose the offset in the
                // RFC 3339 timestamp, we have to do a little dance to keep
                // it around.
                //
                // Jiff specifically makes this hard because this is usually
                // not what you want to do. It makes it very easy to do
                // arithmetic incorrectly.
                //
                // bttf does this because the only real alternative is to
                // return an error or drop information. But I feel like that
                // could be more surprising. And users can opt into dropping
                // information or re-interpreting instants in a different
                // time zone with other commands.
                //
                // Anyway... I'm not 100% certain this is the right way to go.
                //
                // (This is similar to what we do when trying to automatically
                // parse a datetime.)
                //
                // And note that if rfc9557 is requested, then RFC 3339
                // timestamps will be rejected.
                let pieces = fmt::temporal::Pieces::parse(dt)?;
                // If we got a time zone annotation, then that's the domain
                // of RFC 9557, not RFC 3339.
                if let Some(ann) = pieces.time_zone_annotation() {
                    anyhow::bail!(
                        "found RFC 9557 time zone annotation \
                         {ann:?} which is not supported in RFC 3339",
                    );
                }
                let Some(offset) = pieces.offset() else {
                    anyhow::bail!(
                        "RFC 3339 timestamp requires an offset, but \
                         none was found",
                    );
                };
                let date = pieces.date();
                let time = pieces.time().unwrap_or(civil::Time::midnight());
                let dt = date.to_datetime(time);
                let zdt = match offset {
                    fmt::temporal::PiecesOffset::Zulu => {
                        dt.to_zoned_datetime(tz::TimeZone::unknown())?
                    }
                    fmt::temporal::PiecesOffset::Numeric(ref off) => {
                        if off.offset() == Offset::UTC && off.is_negative() {
                            dt.to_zoned_datetime(tz::TimeZone::unknown())?
                        } else {
                            dt.to_zoned_datetime(tz::TimeZone::fixed(
                                off.offset(),
                            ))?
                        }
                    }
                    unk => {
                        anyhow::bail!("unrecognized parsed offset: {unk:?}")
                    }
                };
                zdt
            }
            // N.B. Jiff doesn't have a dedicated RFC 9110 parser. But
            // RFC 2822 subsumes it. I'm not really sure it's worth being
            // precise about *parsing* RFC 9110, since you usually want to
            // be flexible.
            Format::Rfc2822 | Format::Rfc9110 => {
                RFC2822_PARSER.parse_zoned_datetime(dt)?
            }
            Format::Strtime(ref fmt) => {
                let tm = fmt::strtime::parse(fmt.as_bytes(), dt)?;
                match tm.to_zoned_datetime() {
                    Ok(zdt) => return Ok(zdt),
                    Err(err) => {
                        // If we parsed an offset or an IANA time zone
                        // identifier but still couldn't get a `ZonedDateTime`,
                        // then the error is probably legit and we should
                        // bubble it up. Otherwise, we can try some more things.
                        if tm.offset().is_some()
                            || tm.iana_time_zone().is_some()
                        {
                            return Err(err.into());
                        }
                    }
                }
                // If we can't get even a civil datetime from a broken down
                // time, then we're kinda hosed. Not much we can do.
                //
                // Note that this routine is "smart." It knows to use midnight
                // if civil time isn't present. It will also automatically
                // convert, e.g., ISO 8601 week dates to Gregorian dates.
                let dt = tm.to_datetime()?;
                // We interpret civil datetimes without offset/time-zone info
                // as local time.
                dt.to_zoned_datetime(crate::TZ.clone())?
            }
            Format::Flexible => {
                DateTimeFlexible::parse_relative(relative, dt)?.into()
            }
        })
    }
}

impl std::str::FromStr for Format {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Format> {
        Ok(match s {
            "rfc9557" => Format::Rfc9557,
            "rfc3339" => Format::Rfc3339,
            "rfc2822" => Format::Rfc2822,
            "rfc9110" => Format::Rfc9110,
            "flexible" => Format::Flexible,
            unk => {
                if unk.contains('%') {
                    Format::Strtime(unk.into())
                } else {
                    anyhow::bail!("unrecognized format `{}`", unk)
                }
            }
        })
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Format::Rfc9557 => write!(f, "rfc9557"),
            Format::Rfc3339 => write!(f, "rfc3339"),
            Format::Rfc2822 => write!(f, "rfc2822"),
            Format::Rfc9110 => write!(f, "rfc9110"),
            Format::Strtime(ref fmt) => write!(f, "`{fmt}`"),
            Format::Flexible => write!(f, "flexible"),
        }
    }
}

/// Provides parsing for the English name of a month.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct Month(i8);

impl Month {
    /// Return the parsed month as an integer in the range `1..=12`.
    pub fn get(&self) -> i8 {
        self.0
    }
}

impl std::str::FromStr for Month {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Month> {
        if s.chars().all(|c| c.is_ascii_digit()) {
            let month = s.parse::<i8>().with_context(|| {
                format!("failed to parse `{s}` as an integer month")
            })?;
            anyhow::ensure!(
                1 <= month && month <= 12,
                "parsed `{month}` as an integer month, but it's not \
                 in the required range of `1..=12`",
            );
            return Ok(Month(month));
        }
        let month = match &*s.to_lowercase() {
            "january" | "jan" => 1,
            "february" | "feb" => 2,
            "march" | "mar" => 3,
            "april" | "apr" => 4,
            "may" => 5,
            "june" | "jun" => 6,
            "july" | "jul" => 7,
            "august" | "aug" => 8,
            "september" | "sept" | "sep" => 9,
            "october" | "oct" => 10,
            "november" | "nov" => 11,
            "december" | "dec" => 12,
            unk => anyhow::bail!("unrecognized month name/number: `{unk}`"),
        };
        Ok(Month(month))
    }
}

/// Provides parsing for Jiff's civil `Weekday` type.
#[derive(Clone, Debug)]
pub struct Weekday {
    weekday: civil::Weekday,
}

impl Weekday {
    pub const USAGE_WEEK_START: Usage = Usage::flag(
        "--week-start <weekday>",
        "The weekday on which weeks start (defaults to Monday).",
        r#"
The weekday on which weeks start (defaults to Monday).

Any day of the week may be given. They can be specified in the following way
(without regard for case):

Sunday, Sun, SU

Monday, Mon, MO

Tuesday, Tues, Tue, TU

Wednesday, Wed, WE

Thursday, Thurs, Thu, TH

Friday, Fri, FR

Saturday, Sat, SA
"#,
    );

    /// Return the parsed weekday.
    pub fn get(&self) -> civil::Weekday {
        self.weekday
    }
}

impl Default for Weekday {
    fn default() -> Weekday {
        Weekday { weekday: civil::Weekday::Monday }
    }
}

impl From<civil::Weekday> for Weekday {
    fn from(weekday: civil::Weekday) -> Weekday {
        Weekday { weekday }
    }
}

impl std::str::FromStr for Weekday {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Weekday> {
        Weekday::from_bytes(s.as_bytes())
    }
}

impl FromBytes for Weekday {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<Weekday> {
        use jiff::civil::Weekday::*;

        let weekday = match &*s.to_ascii_lowercase() {
            b"sunday" | b"sun" | b"su" => Sunday,
            b"monday" | b"mon" | b"mo" => Monday,
            b"tuesday" | b"tues" | b"tue" | b"tu" => Tuesday,
            b"wednesday" | b"wed" | b"we" => Wednesday,
            b"thursday" | b"thurs" | b"thu" | b"th" => Thursday,
            b"friday" | b"fri" | b"fr" => Friday,
            b"saturday" | b"sat" | b"sa" => Saturday,
            unk => anyhow::bail!(
                "unrecognized weekday: `{unk}`",
                unk = unk.as_bstr()
            ),
        };
        Ok(Weekday { weekday })
    }
}

impl std::fmt::Display for Weekday {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        use jiff::civil::Weekday::*;

        let label = match self.get() {
            Sunday => "Sunday",
            Monday => "Monday",
            Tuesday => "Tuesday",
            Wednesday => "Wednesday",
            Thursday => "Thursday",
            Friday => "Friday",
            Saturday => "Saturday",
        };
        write!(f, "{label}")
    }
}

/// Provides parsing for Jiff's `Unit` type.
#[derive(Clone, Debug)]
pub struct Unit {
    unit: jiff::Unit,
}

impl Unit {
    pub const LARGEST: Usage = Usage::flag(
        "-l/--largest <unit>",
        "Set the largest calendar or time unit.",
        r#"
This sets the largest calendar or time unit that is allowed in the time span
returned.

Calendar units are years, months, weeks or days. Here are the different ways
that each calendar unit can be spelled:

years, year, yrs, yr, y

months, month, mos, mo

weeks, week, wks, wk, w

days, day, d

Time units are hours, minutes, seconds, milliseconds, microseconds or
nanoseconds. Here are the different ways that each time unit can be spelled:

hours, hour, hrs, hr, h

minutes, minute, mins, min, m

seconds, second, secs, sec, s

milliseconds, millisecond, millis, milli, msecs, msec, ms

microseconds, microsecond, micros, micro, usecs, µsecs, usec, µsec, us, µs

nanoseconds, nanosecond, nanos, nano, nsecs, nsec, ns
"#,
    );

    pub const SMALLEST: Usage = Usage::flag(
        "-s/--smallest <unit>",
        "Set the smallest calendar or time unit.",
        r#"
This sets the smallest calendar or time unit that is allowed in the time span
returned. This defaults to nanoseconds. When the smallest unit is bigger than
nanoseconds and the span would otherwise have non-zero units less than the
smallest unit, then rounding is performed.

Calendar units are years, months, weeks or days. Here are the different ways
that each calendar unit can be spelled:

years, year, yrs, yr, y

months, month, mos, mo

weeks, week, wks, wk, w

days, day, d

Time units are hours, minutes, seconds, milliseconds, microseconds or
nanoseconds. Here are the different ways that each time unit can be spelled:

hours, hour, hrs, hr, h

minutes, minute, mins, min, m

seconds, second, secs, sec, s

milliseconds, millisecond, millis, milli, msecs, msec, ms

microseconds, microsecond, micros, micro, usecs, µsecs, usec, µsec, us, µs

nanoseconds, nanosecond, nanos, nano, nsecs, nsec, ns
"#,
    );

    /// Return the parsed unit.
    pub fn get(&self) -> jiff::Unit {
        self.unit
    }
}

impl From<jiff::Unit> for Unit {
    fn from(unit: jiff::Unit) -> Unit {
        Unit { unit }
    }
}

impl std::str::FromStr for Unit {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Unit> {
        use jiff::Unit::*;

        // This is what's recognized by the friendly duration format.
        let unit = match &*s.to_lowercase() {
            "years" | "year" | "yrs" | "yr" | "y" => Year,
            "months" | "month" | "mos" | "mo" => Month,
            "weeks" | "week" | "wks" | "wk" | "w" => Week,
            "days" | "day" | "d" => Day,
            "hours" | "hour" | "hrs" | "hr" | "h" => Hour,
            "minutes" | "minute" | "mins" | "min" | "m" => Minute,
            "seconds" | "second" | "secs" | "sec" | "s" => Second,
            "milliseconds" | "millisecond" | "millis" | "milli" | "msecs"
            | "msec" | "ms" => Millisecond,
            "microseconds" | "microsecond" | "micros" | "micro" | "usecs"
            | "µsecs" | "usec" | "µsec" | "us" | "µs" => Microsecond,
            "nanoseconds" | "nanosecond" | "nanos" | "nano" | "nsecs"
            | "nsec" | "ns" => Nanosecond,
            unk => anyhow::bail!("unrecognized span unit: `{unk}`"),
        };
        Ok(Unit { unit })
    }
}

/// A scrappy comma delimited sequence of values.
///
/// This type doesn't have any requirements on `T` other than that it can be
/// parsed and printed. It also requires that `,` cannot appear within the
/// parse format of `T` (since this will try to split the sequence on `,`).
/// That is, there's no support for quoting or escaping the commas.
///
/// This does not impose any requirements on the order of the sequence. It does
/// require that the sequence is not empty though.
///
/// NOTE: At the time I wrote this, I wasn't planning on using it with anything
/// that could include a comma in it (integers, days of the week, months and
/// so on). But if this is ever adapted for datetimes or durations, we need to
/// be careful because a comma can be used as a decimal separator in that
/// context.
#[derive(Clone, Debug)]
pub struct CommaSequence<T>(Vec<T>);

impl<T> CommaSequence<T> {
    /// Returns an iterator over every item in this sequence.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }
}

impl<'a, T> IntoIterator for &'a CommaSequence<T> {
    type IntoIter = std::slice::Iter<'a, T>;
    type Item = &'a T;

    fn into_iter(self) -> std::slice::Iter<'a, T> {
        self.0.iter()
    }
}

impl<T, E> std::str::FromStr for CommaSequence<T>
where
    T: std::str::FromStr<Err = E>,
    Result<T, E>: Context<T, E>,
    E: std::fmt::Display,
{
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<CommaSequence<T>> {
        let mut seq = vec![];
        for item in s.split(",") {
            seq.push(item.parse::<T>().map_err(|err| {
                anyhow::Error::msg(format!(
                    "failed to parse `{item}` \
                     within sequence `{s}`: {err}",
                ))
            })?);
        }
        anyhow::ensure!(!seq.is_empty(), "empty sequences are not allowed",);
        Ok(CommaSequence(seq))
    }
}

/// An inclusive range of integers.
///
/// This type doesn't have any requirements on `T` other than that it can be
/// parsed and printed, and it is assumed to be a signed integer. e.g., `i8`,
/// `i16`, `i32` or `i64`. It also requires that `..` cannot appear within the
/// parse format of `T` (since this will try to split a range based on `..`).
///
/// Note that this supports parsing just a single integer, e.g., `-5`. It
/// will be represented as if it were `-5..-5`.
///
/// If `start > end`, then the parser will return an error.
///
/// The format is `start[..end]`, where `start` and `end` are signed integers.
#[derive(Clone, Debug)]
pub struct NumberRange<T> {
    start: T,
    end: T,
}

impl<T: Copy> NumberRange<T> {
    /// Return this number range as a standard library inclusive range.
    pub fn range(&self) -> std::ops::RangeInclusive<T> {
        self.start..=self.end
    }
}

impl<T, E> std::str::FromStr for NumberRange<T>
where
    T: std::str::FromStr<Err = E> + Copy + PartialOrd,
    Result<T, E>: Context<T, E>,
{
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<NumberRange<T>> {
        let Some((start, end)) = s.split_once("..") else {
            let start = s.parse::<T>().with_context(|| {
                format!("failed to parse `{s}` as a single signed integer")
            })?;
            let end = start;
            return Ok(NumberRange { start, end });
        };
        let start = start.parse::<T>().with_context(|| {
            format!(
                "failed to parse `{start}` \
                 as a single signed integer within the range `{s}`"
            )
        })?;
        let end = end.parse::<T>().with_context(|| {
            format!(
                "failed to parse `{end}` \
                 as a single signed integer within the range `{s}`"
            )
        })?;
        anyhow::ensure!(
            start <= end,
            "parsed ranges must have start <= end, but \
             `{s}` has start > end",
        );
        Ok(NumberRange { start, end })
    }
}

/// A range special purposed to "by weekday" in RFC 5545.
///
/// Specifically, a range of weekdays is allowed, but a ranged of *numbered*
/// weekdays is not. For numbered weekdays, only a singleton is allowed.
///
/// Also, this doesn't have any restrictions on the ranges parsed since any
/// day of the week might be the "start."
///
/// The format is `start[..end]`, where `start` and `end` are weekdays. Or
/// `numbered-weekday` where `numbered-weekday` is a single `ByWeekday`.
#[derive(Copy, Clone, Debug)]
pub enum ByWeekdays {
    /// A range implies that the start/end points *must* not be numbered.
    Range { start: civil::Weekday, end: civil::Weekday },
    /// A singleton weekday, which may be numbered.
    Singleton(ByWeekday),
}

impl std::str::FromStr for ByWeekdays {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<ByWeekdays> {
        let Some((start, end)) = s.split_once("..") else {
            let singleton = s.parse::<ByWeekday>().with_context(|| {
                format!(
                    "failed to parse `{s}` as a \
                     single weekday or numbered weekday"
                )
            })?;
            return Ok(ByWeekdays::Singleton(singleton));
        };

        let start = start.parse::<ByWeekday>().with_context(|| {
            format!(
                "failed to parse `{start}` \
                 as a single weekday within the range `{s}`"
            )
        })?;
        let end = end.parse::<ByWeekday>().with_context(|| {
            format!(
                "failed to parse `{end}` \
                 as a single weekday within the range `{s}`"
            )
        })?;

        let start = match start {
            ByWeekday::Any(weekday) => weekday,
            ByWeekday::Numbered { .. } => {
                anyhow::bail!(
                    "numbered weekday `{start}` is not allowed in a range",
                )
            }
        };
        let end = match end {
            ByWeekday::Any(weekday) => weekday,
            ByWeekday::Numbered { .. } => {
                anyhow::bail!(
                    "numbered weekday `{end}` is not allowed in a range",
                )
            }
        };

        Ok(ByWeekdays::Range { start, end })
    }
}

/// Provides parsing for "start of" or "end of" units.
///
/// This is similar to `Unit`, but:
///
/// * Does not support nanoseconds, since Jiff doesn't support smaller than
/// nanosecond precision. Therefore, "start of"/"end of" nanosecond doesn't
/// really make sense.
/// * Does not support "week," and instead requires "week-sunday" or
/// "week-monday." Otherwise, the "start" or "end" of a week is ambiguous.
///
/// It seems likely it might make sense to support other things in the future
/// as well, but I'd like to wait for use cases.
#[derive(Clone, Copy, Debug)]
pub enum Of {
    Year,
    Month,
    WeekSunday,
    WeekMonday,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
}

impl Of {
    pub const USAGE_ARG_START: Usage = Usage::arg(
        "<start-of>",
        "Find the start of this unit relative to a datetime.",
        r#"
Find the start of this unit relative to a datetime. This can be a calendar or
a time unit.

Calendar units are years, months, weeks that start on Sunday, weeks that start
on Monday or days. Here are the different ways that each calendar unit can be
spelled:

years, year, yrs, yr, y

months, month, mos, mo

week-sunday, wk-sunday, w-sunday

week-monday, wk-monday, w-monday

days, day, d

Time units are hours, minutes, seconds, milliseconds or microseconds. Here are
the different ways that each time unit can be spelled:

hours, hour, hrs, hr, h

minutes, minute, mins, min, m

seconds, second, secs, sec, s

milliseconds, millisecond, millis, milli, msecs, msec, ms

microseconds, microsecond, micros, micro, usecs, µsecs, usec, µsec, us, µs
"#,
    );

    pub const USAGE_ARG_END: Usage = Usage::arg(
        "<start-of>",
        "Find the start of this unit relative to a datetime.",
        r#"
Find the start of this unit relative to a datetime. This can be a calendar or
a time unit.

Calendar units are years, months, weeks that start on Sunday, weeks that start
on Monday or days. Here are the different ways that each calendar unit can be
spelled:

years, year, yrs, yr, y

months, month, mos, mo

week-sunday, wk-sunday, w-sunday

week-monday, wk-monday, w-monday

days, day, d

Time units are hours, minutes, seconds, milliseconds or microseconds. Here are
the different ways that each time unit can be spelled:

hours, hour, hrs, hr, h

minutes, minute, mins, min, m

seconds, second, secs, sec, s

milliseconds, millisecond, millis, milli, msecs, msec, ms

microseconds, microsecond, micros, micro, usecs, µsecs, usec, µsec, us, µs
"#,
    );

    pub fn start(&self, dt: &DateTime) -> anyhow::Result<DateTime> {
        let zdt = dt.get();
        let zdt = match *self {
            Of::Year => zdt.first_of_year()?.start_of_day()?,
            Of::Month => zdt.first_of_month()?.start_of_day()?,
            Of::WeekSunday => zdt
                .tomorrow()?
                .nth_weekday(-1, civil::Weekday::Sunday)?
                .start_of_day()?,
            Of::WeekMonday => zdt
                .tomorrow()?
                .nth_weekday(-1, civil::Weekday::Monday)?
                .start_of_day()?,
            Of::Day => zdt.start_of_day()?,
            Of::Hour => {
                zdt.with().minute(0).second(0).subsec_nanosecond(0).build()?
            }
            Of::Minute => zdt.with().second(0).subsec_nanosecond(0).build()?,
            Of::Second => zdt.with().subsec_nanosecond(0).build()?,
            Of::Millisecond => {
                zdt.with().microsecond(0).nanosecond(0).build()?
            }
            Of::Microsecond => zdt.with().nanosecond(0).build()?,
        };
        Ok(zdt.into())
    }

    pub fn end(&self, dt: &DateTime) -> anyhow::Result<DateTime> {
        let zdt = dt.get();
        let zdt = match *self {
            Of::Year => zdt.last_of_year()?.end_of_day()?,
            Of::Month => zdt.last_of_month()?.end_of_day()?,
            Of::WeekSunday => zdt
                .yesterday()?
                .nth_weekday(1, civil::Weekday::Saturday)?
                .end_of_day()?,
            Of::WeekMonday => zdt
                .yesterday()?
                .nth_weekday(1, civil::Weekday::Sunday)?
                .end_of_day()?,
            Of::Day => zdt.end_of_day()?,
            Of::Hour => zdt
                .with()
                .minute(59)
                .second(59)
                .subsec_nanosecond(999_999_999)
                .build()?,
            Of::Minute => {
                zdt.with().second(59).subsec_nanosecond(999_999_999).build()?
            }
            Of::Second => zdt.with().subsec_nanosecond(999_999_999).build()?,
            Of::Millisecond => {
                zdt.with().microsecond(999).nanosecond(999).build()?
            }
            Of::Microsecond => zdt.with().nanosecond(999).build()?,
        };
        Ok(zdt.into())
    }
}

impl std::str::FromStr for Of {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Of> {
        use self::Of::*;

        // This is what's recognized by the friendly duration format.
        let of = match &*s.to_lowercase() {
            "years" | "year" | "yrs" | "yr" | "y" => Year,
            "months" | "month" | "mos" | "mo" => Month,
            "week-sunday" | "wk-sunday" | "w-sunday" => WeekSunday,
            "week-monday" | "wk-monday" | "w-monday" => WeekMonday,
            "days" | "day" | "d" => Day,
            "hours" | "hour" | "hrs" | "hr" | "h" => Hour,
            "minutes" | "minute" | "mins" | "min" | "m" => Minute,
            "seconds" | "second" | "secs" | "sec" | "s" => Second,
            "milliseconds" | "millisecond" | "millis" | "milli" | "msecs"
            | "msec" | "ms" => Millisecond,
            "microseconds" | "microsecond" | "micros" | "micro" | "usecs"
            | "µsecs" | "usec" | "µsec" | "us" | "µs" => Microsecond,
            unk => anyhow::bail!("unrecognized \"of\" unit: `{unk}`"),
        };
        Ok(of)
    }
}

/// Provides parsing for Jiff's `RoundMode` type.
#[derive(Clone, Debug)]
pub struct RoundMode {
    mode: jiff::RoundMode,
}

impl RoundMode {
    pub const USAGE: Usage = Usage::flag(
        "-m/--mode <rounding-mode>",
        "Specifies how to perform rounding, e.g., `trunc` or `half-expand`.",
        r#"
This flag specifies how to perform rounding. That is, this flag specifies how
to treat the remainder when rounding either datetimes or time spans.

The default for this flag is `half-expand`, which does rounding like how you
were probably taught in school. The legal values are:

`ceil`: rounds toward positive infinity. For negative time spans and datetimes,
this option will make the value smaller, which could be unexpected. To round
away from zero, use `expand`.

`floor`: rounds toward negative infinity. This mode acts like `trunc` for
positive time spans and datetimes, but for negative values it will make the
value larger, which could be unexpected. To round towards zero, use `trunc`.

`expand`: rounds away from zero like `ceil` for positive time spans and
datetimes, and like `floor` for negative spans and datetimes.

`trunc`: rounds toward zero, chopping off any fractional part of a unit.

`half-ceil`: rounds to the nearest allowed value like `half-expand`, but when
there is a tie, round towards positive infinity like `ceil`.

`half-floor`: rounds to the nearest allowed value like `half-expand`, but when
there is a tie, round towards negative infinity like `floor`.

`half-expand`: rounds to the nearest value allowed by the rounding increment
and the smallest unit. When there is a tie, round away from zero like `ceil`
for positive time spans and datetimes and like `floor` for negative time spans
and datetimes. This corresponds to how rounding is often taught in school.

`half-trunc`: rounds to the nearest allowed value like `half-expand`, but when
there is a tie, round towards zero like `trunc`.

`half-even`: rounds to the nearest allowed value like `half-expand`, but when
there is a tie, round towards the value that is an even multiple of the
rounding increment. For example, with a rounding increment of 3, the number 10
would round up to 12 instead of down to 9, because 12 is an even multiple of 3,
where as 9 is is an odd multiple.
"#,
    );

    /// Return the parsed unit.
    pub fn get(&self) -> jiff::RoundMode {
        self.mode
    }
}

impl From<jiff::RoundMode> for RoundMode {
    fn from(mode: jiff::RoundMode) -> RoundMode {
        RoundMode { mode }
    }
}

impl std::str::FromStr for RoundMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<RoundMode> {
        use jiff::RoundMode::*;

        // This is what's recognized by the friendly duration format.
        let mode = match &*s.to_lowercase() {
            "ceil" => Ceil,
            "floor" => Floor,
            "expand" => Expand,
            "trunc" => Trunc,
            "half-ceil" => HalfCeil,
            "half-floor" => HalfFloor,
            "half-expand" => HalfExpand,
            "half-trunc" => HalfTrunc,
            "half-even" => HalfEven,
            unk => anyhow::bail!("unrecognized rounding mode: `{unk}`"),
        };
        Ok(RoundMode { mode })
    }
}

/// A simple abstraction over "one file path or stdin."
#[derive(Clone, Debug)]
pub struct FileOrStdin {
    path: Option<PathBuf>,
}

impl FileOrStdin {
    /// Create a `FileOrStdin` that reads from `stdin`.
    ///
    /// This doesn't actually read or touch `stdin` until `FileOrStdin::reader`
    /// is called and something tries to read from *that*. This means that
    /// this is a useful default value for CLI parsing.
    ///
    /// This corresponds to `FileOrStdin::default()`.
    pub fn stdin() -> FileOrStdin {
        FileOrStdin { path: None }
    }

    /// Sets the path to the one provided, but only if no path has already been
    /// set.
    ///
    /// If a path has already been set, then an error is returned.
    ///
    /// This is useful in contexts where a CLI wants to accept only 0 or 1
    /// file paths, with 0 corresponding to stdin.
    pub fn set(&mut self, path: impl Into<PathBuf>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.path.is_none(),
            "command only accepts a single path",
        );
        self.path = Some(path.into());
        Ok(())
    }

    /// Return a `std::fmt::Display` impl for the underlying file or stdin.
    ///
    /// When a file, this is its file path. When stdin, it's the literal
    /// string `<stdin>`.
    pub fn display(&self) -> impl std::fmt::Display + '_ {
        self.path.as_deref().unwrap_or_else(|| Path::new("<stdin>")).display()
    }

    /// Return a buffered reader for the underlying file or stdin.
    pub fn reader(&self) -> anyhow::Result<Box<dyn io::BufRead>> {
        Ok(if let Some(ref path) = self.path {
            let file = std::fs::File::open(path)
                .with_context(|| format!("{}", path.display()))?;
            Box::new(std::io::BufReader::new(file))
        } else {
            Box::new(std::io::stdin().lock())
        })
    }
}

impl Default for FileOrStdin {
    fn default() -> FileOrStdin {
        FileOrStdin::stdin()
    }
}

impl From<OsString> for FileOrStdin {
    fn from(os_str: OsString) -> FileOrStdin {
        FileOrStdin::from(PathBuf::from(os_str))
    }
}

impl From<PathBuf> for FileOrStdin {
    fn from(path: PathBuf) -> FileOrStdin {
        if path == Path::new("-") {
            FileOrStdin { path: None }
        } else {
            FileOrStdin { path: Some(path) }
        }
    }
}

/// A helper type for parsing a flag indicating the "number of threads" to use.
///
/// This should be used in commands that support parallelism, so that users
/// can control their resource usage.
#[derive(Clone, Debug, Default)]
pub struct Threads {
    count: Option<NonZero<usize>>,
}

impl Threads {
    pub const USAGE: Usage = Usage::flag(
        "-j/--threads <number>",
        "Control the number of threads used by this command.",
        r#"
Control the number of threads used by this command.

When not set, this command will query your system to determine the number of
available cores to use.
"#,
    );

    /// Return the number of threads this command should use.
    ///
    /// If no flag was given, then the number of cores is queried here. If that
    /// fails, then a DEBUG-level log message is emitted and `1` is returned.
    pub fn get(&self) -> NonZero<usize> {
        if let Some(threads) = self.count {
            return threads;
        }
        let available = match std::thread::available_parallelism() {
            Ok(available) => available,
            Err(err) => {
                log::warn!(
                    "failed to query available parallelism, \
                     falling back to single threaded mode: {err}",
                );
                return NonZero::<usize>::MIN;
            }
        };
        available
    }
}

impl std::str::FromStr for Threads {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Threads> {
        let count: usize = s
            .parse()
            .with_context(|| format!("failed to parse `{s}` as an integer"))?;
        // I suppose we could make `0` mean "not given." But I was a little
        // unsure and so decided to be conservative.
        let count = NonZero::new(count).ok_or_else(|| {
            anyhow::anyhow!("number of threads must be greater than 0")
        })?;
        Ok(Threads { count: Some(count) })
    }
}

/// A simple wrapper for a pair of `--no-mmap` and `--mmap` flags.
///
/// This is useful for opening files in the "quickest" manner possible.
///
/// When neither `--no-mmap` nor `--mmap` are given, then a default is selected
/// based on heuristics/platform.
#[derive(Clone, Debug, Default)]
pub struct MemoryMapper {
    enabled: Option<bool>,
}

impl MemoryMapper {
    /// Returns a value corresponding to the contents of the given file.
    ///
    /// This attempts to use memory maps when enabled.
    ///
    /// # Safety
    ///
    /// Callers should ensure or must assume that the file at the given path
    /// is not mutated while in use by bttf. The "caller" in this context may
    /// be an end user. In which case, end users should use `--no-mmap` if
    /// they cannot make this assurance. Otherwise, this risks undefined
    /// behavior.
    ///
    /// When memory maps are disabled, this is safe to call for all inputs.
    pub unsafe fn open(&self, path: &Path) -> anyhow::Result<MemoryMapOrHeap> {
        if !self.enabled() {
            return self.open_heap(path);
        }
        let file =
            File::open(path).with_context(|| format!("{}", path.display()))?;
        // When the file is very small, don't both with file backed memory
        // maps. This is actually overall an optimizaton on Linux at least,
        // where opening a bunch of memory maps in parallel ends up being
        // quite slow.
        let md =
            file.metadata().with_context(|| format!("{}", path.display()))?;
        if md.len() <= 10 * (1 << 20) {
            return self.open_heap(path);
        }
        // SAFETY: Safety obligations are forwarded to caller.
        let mmap = unsafe {
            memmap2::Mmap::map(&file)
                .with_context(|| format!("{}", path.display()))?
        };
        #[cfg(unix)]
        {
            mmap.advise(memmap2::Advice::Sequential)
                .with_context(|| format!("{}", path.display()))?;
        }
        Ok(MemoryMapOrHeap::MemoryMap(mmap))
    }

    fn open_heap(&self, path: &Path) -> anyhow::Result<MemoryMapOrHeap> {
        std::fs::read(path)
            .with_context(|| format!("{}", path.display()))
            .map(MemoryMapOrHeap::Heap)
    }

    fn enabled(&self) -> bool {
        // Memory maps on macOS suck. ripgrep does the same.
        self.enabled.unwrap_or(!cfg!(target_os = "macos"))
    }
}

impl Configurable for MemoryMapper {
    fn configure(
        &mut self,
        _: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Long("no-mmap") => {
                self.enabled = Some(false);
            }
            lexopt::Arg::Long("mmap") => {
                self.enabled = Some(true);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        const MMAP: Usage = Usage::flag(
            "--mmap",
            "Enable reading files via memory maps (the default).",
            r#"
Enable reading files via memory maps (the default).

Reading via memory maps can be advantageous for performance reasons and to
avoid reading large files on to the heap. The latter would otherwise be
required in order to run regexes on file contents, since the regex engine does
not support stream searching.

Note that even when enabled, memory maps are not guaranteed to be used. bttf
will still use heuristics to determine when or if they ought to be enabled.

Use the `--no-mmap` flag to forcefully disable memory maps. This is useful if
bttf's heuristics aren't good enough, or if you don't want to be susceptible to
bugs instigated by the use of file backed memory maps.
"#,
        );

        const NO_MMAP: Usage = Usage::flag(
            "--no-mmap",
            "Disable reading files via memory maps.",
            r#"
Disable reading files via memory maps.

This is useful for cases where using memory maps might be slower. bttf already
uses heuristics to detect when it thinks memory maps will be slower, but an
end user might know better.

This is also useful for avoiding bugs instigated by using memory maps, such as
files being mutated or truncated while searching. This could cause a `SIGBUS`
and unceremoniously terminate the bttf process.
"#,
        );

        &[MMAP, NO_MMAP]
    }
}

/// A type that abstracts over the contents of files.
///
/// Either the contents come from a file backed memory map or from the heap.
pub enum MemoryMapOrHeap {
    MemoryMap(memmap2::Mmap),
    Heap(Vec<u8>),
}

impl MemoryMapOrHeap {
    /// Return the contents of a file as a slice of bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match *self {
            MemoryMapOrHeap::MemoryMap(ref mmap) => mmap,
            MemoryMapOrHeap::Heap(ref bytes) => bytes,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl Op {
    pub const ARG: Usage = Usage::arg(
        "<op>",
        "A comparison operator: eq, ne, lt, gt, le, or ge.",
        r#"
A comparison operator.

Legal values are eq (equals), ne (not equals), lt (less than),
gt (greater than), le (less than or equal), or ge (greater than or equal).
"#,
    );
}

impl FromBytes for Op {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<Op> {
        match s {
            b"eq" => Ok(Op::Eq),
            b"ne" => Ok(Op::Ne),
            b"lt" => Ok(Op::Lt),
            b"gt" => Ok(Op::Gt),
            b"le" => Ok(Op::Le),
            b"ge" => Ok(Op::Ge),
            unk => anyhow::bail!(
                "unknown comparison operator `{unk}`",
                unk = unk.as_bstr(),
            ),
        }
    }
}
