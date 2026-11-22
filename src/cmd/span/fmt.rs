use std::io::Write;

use {
    anyhow::Context,
    bstr::ByteSlice,
    jiff::{Unit, fmt::friendly},
};

use crate::{
    args::{self, Usage, flags, positional},
    parse::FromBytes,
    span::TimeSpan,
};

const USAGE: &'static str = r#"
Format spans into the "friendly" format.

This permits controlling a number of different settings that influence the
formatting of spans. This includes, but is not limited to, the verbosity of
unit designators, spacing and precision.

All spans printed by this command are valid instantiations of the "friendly"
format. That is, any output of this command can be parsed anywhere bttf
expects a span.

To format a span as an ISO 8601 duration, use `bttf span iso8601`.

Full details on the friendly format can be found here:
https://docs.rs/jiff/1/jiff/fmt/friendly/index.html

USAGE:
    bttf span fmt <span>...
    bttf span fmt < line delimited <span>

TIP:
    use -h for short docs and --help for long docs

EXAMPLES:
    To print a verbose span with nice spacing:

        $ bttf span fmt '75y5mo22d' -s units-and-designators -d verbose --comma
        75 years, 5 months, 22 days

    %snip-start%

    To use fractional seconds instead of writing out sub-second units:

        $ bttf span fmt '30s123ms456us789ns' -f secs
        30.123456789s

    One can also control precision:

        $ bttf span fmt '30s123ms456us789ns' -f secs --precision 3
        30.123s

    Or use an `HH:MM:SS` format for the time units:

        $ bttf span fmt '5d2h30m10s' --hms
        5d 02:30:10

    %snip-end%
REQUIRED ARGUMENTS:
%args%
OPTIONS:
%flags%
"#;

pub fn run(p: &mut lexopt::Parser) -> anyhow::Result<()> {
    let mut config = Config::default();
    let mut spans = positional::Spans::default();
    args::configure(p, USAGE, &mut [&mut config, &mut spans])?;

    let printer = config.printer();
    let mut wtr = std::io::stdout().lock();
    spans.try_map(|datum| {
        let formatted =
            datum.try_map(|span| Ok(printer.span_to_string(span.get())))?;
        formatted.write(&mut wtr)?;
        writeln!(wtr)?;
        Ok(true)
    })
}

#[derive(Debug, Default)]
struct Config {
    designator: Designator,
    spacing: Spacing,
    direction: Direction,
    fractional: FractionalUnit,
    comma: bool,
    hms: bool,
    padding: Padding,
    precision: Precision,
    zero_unit: Option<Unit>,
}

impl Config {
    fn printer(&self) -> friendly::SpanPrinter {
        let mut printer = friendly::SpanPrinter::new()
            .designator(self.designator.0)
            .spacing(self.spacing.0)
            .direction(self.direction.0)
            .fractional(self.fractional.0)
            .comma_after_designator(self.comma)
            .hours_minutes_seconds(self.hms)
            .precision(self.precision.0)
            .zero_unit(self.zero_unit.unwrap_or(Unit::Second));
        if let Some(pad) = self.padding.0 {
            printer = printer.padding(pad);
        }
        printer
    }
}

impl args::Configurable for Config {
    fn configure(
        &mut self,
        p: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Short('d') | lexopt::Arg::Long("designator") => {
                self.designator = args::parse_bytes(p, "-d/--designator")?;
            }
            lexopt::Arg::Short('s') | lexopt::Arg::Long("spacing") => {
                self.spacing = args::parse_bytes(p, "-s/--spacing")?;
            }
            lexopt::Arg::Long("sign") => {
                self.direction = args::parse_bytes(p, "--sign")?;
            }
            lexopt::Arg::Short('f') | lexopt::Arg::Long("fractional") => {
                self.fractional = args::parse_bytes(p, "-f/--fractional")?;
            }
            lexopt::Arg::Long("comma") => {
                self.comma = true;
            }
            lexopt::Arg::Long("hms") => {
                self.hms = true;
            }
            lexopt::Arg::Long("pad") => {
                self.padding = args::parse(p, "--pad")?;
            }
            lexopt::Arg::Long("precision") => {
                self.precision = args::parse(p, "--precision")?;
            }
            lexopt::Arg::Long("zero-unit") => {
                let unit: flags::Unit = args::parse(p, "--zero-unit")?;
                self.zero_unit = Some(unit.get());
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        const HMS: Usage = Usage::flag(
            "--hms",
            "Enable `HH:MM:SS` format for time units.",
            r#"
Enable `HH:MM:SS` format for time units.

Calendar units are still formatted as normal, before `HH:MM:SS`.

If there are non-zero milliseconds, microseconds or nanoseconds in the span,
then they are rendered as fractional seconds. For example, `123ms` would be
formatted as `00:00:00.123`.
"#,
        );

        const COMMA: Usage = Usage::flag(
            "--comma",
            "Add commas after unit designators.",
            r#"
Add commas after unit designators.

For example, instead of `5y 1d`, this will result in `5y, 1d`. This option
is often best combined with `--designator=verbose` and
`--spacing=units-and-designators`.
"#,
        );

        const ZERO_UNIT: Usage = Usage::flag(
            "--zero-unit <unit>",
            "Set the unit to use for spans of length zero.",
            r#"
Set the unit to use for spans of length zero.

When `-f/--fractional` is given, then this is ignored and the zero unit
corresponds to the fractional unit specified.

This defaults to `second`.
"#,
        );

        &[
            TimeSpan::ARG_OR_STDIN,
            Designator::USAGE,
            Spacing::USAGE,
            Direction::USAGE,
            FractionalUnit::USAGE,
            COMMA,
            HMS,
            Padding::USAGE,
            Precision::USAGE,
            ZERO_UNIT,
        ]
    }
}

#[derive(Clone, Debug)]
struct Designator(friendly::Designator);

impl Designator {
    const USAGE: Usage = Usage::flag(
        "-d/--designator <kind>",
        "Set the verbosity level of calendar/time units to use.",
        r#"
Set the verbosity level of calendar/time units to use.

The default value is `compact`. The possible values and their behavior are:

`verbose`: This writes out the full word of each unit designation. For example,
`year` and `nanoseconds`.

`short`: This writes out a short but not minimal label for each unit. For
example, `yr` for year and `yrs` for years.

`compact`: This writes out the shortest possible label for each unit that is
still generally recognizable. For example, `y` for both `years` and `year`.
Note that in the compact representation, and unlike the `verbose` and `short`
representations, there is no distinction between singular or plural.
"#,
    );
}

impl Default for Designator {
    fn default() -> Designator {
        Designator(friendly::Designator::Compact)
    }
}

impl FromBytes for Designator {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<Designator> {
        let d = match s {
            b"verbose" => friendly::Designator::Verbose,
            b"short" => friendly::Designator::Short,
            b"compact" => friendly::Designator::Compact,
            // N.B. We don't currently support using `humantime`
            // designators since I feel like that's kind of a hack
            // intended for the Rust library ecosystem that probably
            // should propagate out to CLI tools.
            unk => anyhow::bail!(
                "unknown designator `{unk}`",
                unk = unk.as_bstr()
            ),
        };
        Ok(Designator(d))
    }
}

#[derive(Clone, Debug)]
struct Spacing(friendly::Spacing);

impl Spacing {
    const USAGE: Usage = Usage::flag(
        "-s/--spacing <kind>",
        "Sets how to insert spaces into a formatted span.",
        r#",
Sets how to insert spaces into a formatted span.

The default value is `units`. The possible values and their behavior are:

`none`: Does not insert any ASCII whitespace. Except in the case that `--hms`
is given and one is formatting a span with non-zero calendar units, then an
ASCII whitespace is inserted between the calendar and non-calendar units.

`units`: Inserts one ASCII whitespace between the unit designator and the next
unit value.

`units-and-designators`: Inserts one ASCII whitespace between the unit value
and the unit designator, in addition to inserting one ASCII whitespace between
the unit designator and the next unit value.
"#,
    );
}

impl Default for Spacing {
    fn default() -> Spacing {
        Spacing(friendly::Spacing::BetweenUnits)
    }
}

impl FromBytes for Spacing {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<Spacing> {
        let d = match s {
            b"none" => friendly::Spacing::None,
            b"units" => friendly::Spacing::BetweenUnits,
            b"units-and-designators" => {
                friendly::Spacing::BetweenUnitsAndDesignators
            }
            unk => anyhow::bail!(
                "unknown spacing option `{unk}`",
                unk = unk.as_bstr()
            ),
        };
        Ok(Spacing(d))
    }
}

#[derive(Clone, Debug)]
struct Direction(friendly::Direction);

impl Direction {
    const USAGE: Usage = Usage::flag(
        "--sign <kind>",
        "Sets how to add a sign to a formatted span.",
        r#"
Sets how to add a sign to a formatted span.

The default value is `auto`. The possible values and their behavior are:

`auto`: When `-s/--spacing` is set to `none`, then this is equivalent to
`prefix`. When `--hms` given, then this is equivalent to `prefix` when all
calendar units (days and greater) are zero. Otherwise, this is equivalent to
`suffix`.

`prefix`: When set, a sign is only written when the span is negative. And when
it is written, it is written as a prefix of the formatted span.

`force-prefix`: When set, a prefix sign is always written, with `-` for
negative spans and `+` for all non-negative spans. The sign is always written
as a prefix of the formatted span.

`suffix`: When set, a sign is only written when the span is negative. And when
it is written, it is written as a suffix via a trailing `ago` string.
"#,
    );
}

impl Default for Direction {
    fn default() -> Direction {
        Direction(friendly::Direction::Auto)
    }
}

impl FromBytes for Direction {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<Direction> {
        let d = match s {
            b"auto" => friendly::Direction::Auto,
            b"prefix" => friendly::Direction::Sign,
            b"force-prefix" => friendly::Direction::ForceSign,
            b"suffix" => friendly::Direction::Suffix,
            unk => anyhow::bail!(
                "unknown direction/sign option `{unk}`",
                unk = unk.as_bstr()
            ),
        };
        Ok(Direction(d))
    }
}

#[derive(Clone, Debug, Default)]
struct FractionalUnit(Option<friendly::FractionalUnit>);

impl FractionalUnit {
    const USAGE: Usage = Usage::flag(
        "-f, --fractional <unit>",
        "Sets whether to write fractional time units.",
        r#"
Sets whether to write fractional time units.

The default value for this flag is `auto`. The possible values and their
behavior are:

`auto`: No fractional units are written unless `--hms` is provided. In which
case, bttf behaves as if `--fractional=second` was given.

Otherwise, the value must be a time unit greater than nanoseconds. Here are the
different ways that each time unit can be spelled:

hours, hour, hrs, hr, h

minutes, minute, mins, min, m

seconds, second, secs, sec, s

milliseconds, millisecond, millis, milli, msecs, msec, ms

microseconds, microsecond, micros, micro, usecs, µsecs, usec, µsec, us, µs

Be warned that, at present, the "friendly" duration format that bttf uses is
limited to 9 digits after the decimal point. This means that if you use
hours or minutes as your fractional unit, the resulting formatted span may
have precision loss.
"#,
    );
}

impl FromBytes for FractionalUnit {
    type Err = anyhow::Error;

    fn from_bytes(s: &[u8]) -> anyhow::Result<FractionalUnit> {
        use jiff::fmt::friendly::FractionalUnit::*;

        let unit = match s {
            b"auto" => return Ok(FractionalUnit(None)),
            b"hours" | b"hour" | b"hrs" | b"hr" | b"h" => Hour,
            b"minutes" | b"minute" | b"mins" | b"min" | b"m" => Minute,
            b"seconds" | b"second" | b"secs" | b"sec" | b"s" => Second,
            b"milliseconds" | b"millisecond" | b"millis" | b"milli"
            | b"msecs" | b"msec" | b"ms" => Millisecond,
            b"microseconds" | b"microsecond" | b"micros" | b"micro"
            | b"usecs" | b"\xC2\xB5secs" | b"usec" | b"\xC2\xB5sec"
            | b"us" | b"\xC2\xB5s" => Microsecond,
            unk => anyhow::bail!(
                "unknown fractional unit `{unk}`",
                unk = unk.as_bstr()
            ),
        };
        Ok(FractionalUnit(Some(unit)))
    }
}

#[derive(Clone, Debug, Default)]
struct Padding(Option<u8>);

impl Padding {
    const USAGE: Usage = Usage::flag(
        "--pad <amount>",
        "Sets the amount of padding, with zeroes, to apply to each unit.",
        r#"
Sets the amount of padding, with zeroes, to apply to each unit value.

The default value for this flag is `0`. Except with `--hms` is given, then
the hour, minute and second unit values are padded to two places, with leading
zeroes if necessary.
"#,
    );
}

impl std::str::FromStr for Padding {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Padding> {
        let pad: u8 = s.parse().with_context(|| {
            format!("failed to parse padding amount from `{s}`")
        })?;
        Ok(Padding(Some(pad)))
    }
}

#[derive(Clone, Debug, Default)]
struct Precision(Option<u8>);

impl Precision {
    const USAGE: Usage = Usage::flag(
        "--precision <amount>",
        "Sets the amount of precision to use for fractional units.",
        r#"
Sets the amount of precision to use for fractional units.

The default value for this flag is `auto`, which means that precision will
automatically be determined from the span's unit values. A value of `0` means
that any fractional component is truncated. The maximum value is `9`. If values
bigger than `9` are given, then they are clamped to `9`.
"#,
    );
}

impl std::str::FromStr for Precision {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Precision> {
        if s == "auto" {
            return Ok(Precision(None));
        }
        let precision: u8 = s.parse().with_context(|| {
            format!("failed to parse precision amount from `{s}`")
        })?;
        Ok(Precision(Some(precision)))
    }
}
