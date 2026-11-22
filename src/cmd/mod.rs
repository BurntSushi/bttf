mod span;
mod tag;
mod time;
mod tz;
mod untag;

const USAGE: &'static str = "\
A simple utility for doing datetime arithmetic, parsing and formatting.

USAGE:
    bttf <command> ...

COMMANDS:
    span   Tools for manipulating time spans/durations
    time   Tools for manipulating datetimes
    tag    Tag arbitrary data with datetimes or spans
    tz     Commands for working directly with time zones
    untag  Remove tags from previously tagged data
";

pub fn run(p: &mut lexopt::Parser) -> anyhow::Result<()> {
    // For convenience, running `bttf` with no arguments prints the current
    // time in a somewhat nice format (roughly matches `date` on my system).
    if p.try_raw_args().map_or(false, |args| args.as_slice().is_empty()) {
        use crate::{args::flags::Format, datetime::DateTime};
        use std::io::Write;

        // Instead of just making a `BrokenDownTime` from `ZonedDateTime`
        // and calling the formatting API directly, we specifically
        // allow for a `String` allocation here and reuse our existing
        // strftime API to avoid creating multiple copies of the
        // (rather large) `strftime` formatting routine inside of
        // Jiff. (Inside of Jiff, it's generic over the destination
        // `jiff::fmt::Write` impl.)
        let fmt = Format::Strtime("%c".into());
        let config = crate::locale::jiff_strtime_config()?;
        let now = DateTime::from(crate::NOW.clone());
        writeln!(std::io::stdout(), "{}", fmt.format(&config, &now)?)?;

        return Ok(());
    }

    let cmd = crate::args::next_as_command(USAGE, p)?;
    match &*cmd {
        "span" => span::run(p),
        "time" => time::run(p),
        "tag" => tag::run(p),
        "tz" => tz::run(p),
        "untag" => untag::run(p),
        unk => anyhow::bail!("unrecognized command '{}'", unk),
    }
}
