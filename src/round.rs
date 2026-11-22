use anyhow::Context;

use crate::{
    args::{self, Configurable, Usage, flags},
    datetime::{DateTime, DateTimeFlexible},
    span::TimeSpan,
};

const INCREMENT: Usage = Usage::flag(
    "-i/--increment <number>",
    "Set the rounding increment for the smallest unit.",
    r#"
Set the rounding increment for the smallest unit.

The default value is 1. Other values permit rounding the smallest unit to the
nearest integer increment specified. For example, if the smallest unit is
set to `minute`, then a rounding increment of 30 would result in rounding in
increments of a half hour. That is, the only minute value that could result
would be 0 or 30.
"#,
);

/// Provides the options necessary to configure datetime rounding in Jiff.
#[derive(Clone, Debug)]
pub struct DateTimeRound {
    smallest: flags::Unit,
    mode: flags::RoundMode,
    increment: i64,
}

impl DateTimeRound {
    pub fn round(&self, dt: &DateTime) -> anyhow::Result<DateTime> {
        Ok(dt.get().round(self.options())?.into())
    }

    fn options(&self) -> jiff::ZonedDateTimeRound {
        jiff::ZonedDateTimeRound::new()
            .smallest(self.smallest.get())
            .mode(self.mode.get())
            .increment(self.increment)
    }
}

impl Default for DateTimeRound {
    fn default() -> DateTimeRound {
        DateTimeRound {
            smallest: jiff::Unit::Nanosecond.into(),
            mode: jiff::RoundMode::HalfExpand.into(),
            increment: 1,
        }
    }
}

impl Configurable for DateTimeRound {
    fn configure(
        &mut self,
        p: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Short('s') | lexopt::Arg::Long("smallest") => {
                self.smallest = args::parse(p, "-s/--smallest")?;
            }
            lexopt::Arg::Short('m') | lexopt::Arg::Long("mode") => {
                self.mode = args::parse(p, "-m/--mode")?;
            }
            lexopt::Arg::Short('i') | lexopt::Arg::Long("increment") => {
                self.increment = args::parse(p, "-i/--increment")?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        &[flags::Unit::SMALLEST, flags::RoundMode::USAGE, INCREMENT]
    }
}

/// Provides the options necessary to configure span rounding in Jiff.
#[derive(Clone, Debug)]
pub struct TimeSpanRound {
    smallest: flags::Unit,
    largest: Option<flags::Unit>,
    mode: flags::RoundMode,
    increment: i64,
    relative: DateTime,
}

impl TimeSpanRound {
    pub fn round(&self, span: &TimeSpan) -> anyhow::Result<TimeSpan> {
        let rounded = span.get().round(self.options()).with_context(|| {
            format!(
                "failed to round span relative to `{relative}`",
                relative = self.relative
            )
        })?;
        Ok(rounded.into())
    }

    fn options(&self) -> jiff::SpanRound<'_> {
        let mut options = jiff::SpanRound::new()
            .smallest(self.smallest.get())
            .mode(self.mode.get())
            .increment(self.increment)
            .relative(self.relative.get());
        if let Some(ref largest) = self.largest {
            options = options.largest(largest.get());
        }
        options
    }
}

impl Default for TimeSpanRound {
    fn default() -> TimeSpanRound {
        TimeSpanRound {
            smallest: jiff::Unit::Nanosecond.into(),
            largest: None,
            mode: jiff::RoundMode::HalfExpand.into(),
            increment: 1,
            relative: DateTime::now(),
        }
    }
}

impl Configurable for TimeSpanRound {
    fn configure(
        &mut self,
        p: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Short('s') | lexopt::Arg::Long("smallest") => {
                self.smallest = args::parse(p, "-s/--smallest")?;
            }
            lexopt::Arg::Short('l') | lexopt::Arg::Long("largest") => {
                self.largest = Some(args::parse(p, "-l/--largest")?);
            }
            lexopt::Arg::Short('m') | lexopt::Arg::Long("mode") => {
                self.mode = args::parse(p, "-m/--mode")?;
            }
            lexopt::Arg::Short('i') | lexopt::Arg::Long("increment") => {
                self.increment = args::parse(p, "-i/--increment")?;
            }
            lexopt::Arg::Short('r') | lexopt::Arg::Long("relative") => {
                let relative: DateTimeFlexible =
                    args::parse(p, "-r/--relative")?;
                self.relative = relative.into();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        &[
            flags::Unit::SMALLEST,
            flags::Unit::LARGEST,
            flags::RoundMode::USAGE,
            INCREMENT,
            DateTime::RELATIVE_FLAG,
        ]
    }
}

/// Provides the options necessary to configure span balancing in Jiff.
///
/// Balancing uses Jiff's rounding API, but in a way that never does rounding
/// and only balancing.
#[derive(Clone, Debug)]
pub struct TimeSpanBalance {
    largest: flags::Unit,
    relative: DateTime,
}

impl TimeSpanBalance {
    pub fn balance(&self, span: &TimeSpan) -> anyhow::Result<TimeSpan> {
        let balanced =
            span.get().round(self.options()).with_context(|| {
                format!(
                    "failed to balance span relative to `{relative}`",
                    relative = self.relative
                )
            })?;
        Ok(balanced.into())
    }

    fn options(&self) -> jiff::SpanRound<'_> {
        jiff::SpanRound::new()
            .largest(self.largest.get())
            .relative(self.relative.get())
    }
}

impl Default for TimeSpanBalance {
    fn default() -> TimeSpanBalance {
        TimeSpanBalance {
            relative: DateTime::now(),
            largest: jiff::Unit::Year.into(),
        }
    }
}

impl args::Configurable for TimeSpanBalance {
    fn configure(
        &mut self,
        p: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Short('r') | lexopt::Arg::Long("relative") => {
                let relative: DateTimeFlexible =
                    args::parse(p, "-r/--relative")?;
                self.relative = relative.into();
            }
            lexopt::Arg::Short('l') | lexopt::Arg::Long("largest") => {
                self.largest = args::parse(p, "-l/--largest")?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        &[flags::Unit::LARGEST, DateTime::RELATIVE_FLAG]
    }
}
