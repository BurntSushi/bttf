use std::{env, io::Write, process::ExitCode, sync::LazyLock};

use {
    anyhow::Context,
    jiff::{Timestamp, ZonedDateTime, tz::TimeZone},
};

use crate::locale::Locale;

mod args;
mod cmd;
mod datetime;
mod extract;
mod ical;
mod locale;
mod logger;
mod parallel;
mod parse;
mod round;
mod span;
mod style;
mod tag;
mod timezone;
mod weekdate;

static TZ: LazyLock<TimeZone> = LazyLock::new(|| TimeZone::system());

static NOW: LazyLock<ZonedDateTime> = LazyLock::new(|| {
    let ts = match read_env_bttf_now() {
        Ok(Some(ts)) => {
            log::trace!(
                "setting current time to `{ts}` from `BTTF_NOW` \
                 environment variable",
            );
            ts
        }
        Ok(None) => {
            let now = Timestamp::now();
            log::trace!(
                "`BTTF_NOW` environment variable not set, using \
                 current time `{now}`",
            );
            now
        }
        Err(err) => {
            let now = Timestamp::now();
            log::warn!(
                "reading `BTTF_NOW` failed, using current time \
                 `{now}`: {err:#}",
            );
            now
        }
    };
    ts.to_zoned_datetime(TZ.clone())
});

static LOCALE: LazyLock<Locale> = LazyLock::new(|| {
    let locale = match read_env_bttf_locale() {
        Ok(Some(locale)) => {
            log::trace!(
                "setting locale to `{locale}` from `BTTF_LOCALE` \
                 environment variable",
            );
            locale
        }
        Ok(None) => {
            let locale = Locale::unknown();
            log::trace!(
                "`BTTF_LOCALE` environment variable not set, using \
                 `unknown` locale",
            );
            locale
        }
        Err(err) => {
            let locale = Locale::unknown();
            log::warn!(
                "reading `BTTF_LOCALE` failed, using unknown locale \
                 `{locale}`: {err:#}",
            );
            locale
        }
    };
    locale
});

/// Then, as it was, then again it will be.
fn main() -> ExitCode {
    let err = match run() {
        Ok(code) => return code,
        Err(err) => err,
    };
    if let Some(help) = err.root_cause().downcast_ref::<args::Help>() {
        writeln!(&mut std::io::stdout(), "{help}").unwrap();
        return ExitCode::SUCCESS;
    }
    if let Some(version) = err.root_cause().downcast_ref::<args::Version>() {
        writeln!(&mut std::io::stdout(), "{version}").unwrap();
        return ExitCode::SUCCESS;
    }
    // Look for a broken pipe error. In this case, we generally want
    // to exit "gracefully" with a success exit code. This matches
    // existing Unix convention. We need to handle this explicitly
    // since the Rust runtime doesn't ask for PIPE signals, and thus
    // we get an I/O error instead. Traditional C Unix applications
    // quit by getting a PIPE signal that they don't handle, and thus
    // the unhandled signal causes the process to unceremoniously
    // terminate.
    for cause in err.chain() {
        if let Some(err) = cause.downcast_ref::<std::io::Error>()
            && err.kind() == std::io::ErrorKind::BrokenPipe
        {
            return ExitCode::from(0);
        }
        // `serde_json` for whatever reason swallows any
        // `std::io::Error` it may hit when serializing JSON
        // via `to_writer`. So to deal with broken pipe errors,
        // we need to explicitly check it.
        if let Some(err) = cause.downcast_ref::<serde_json::Error>()
            && let Some(kind) = err.io_error_kind()
            && kind == std::io::ErrorKind::BrokenPipe
        {
            return ExitCode::from(0);
        }
    }
    if std::env::var("RUST_BACKTRACE").map_or(false, |v| v == "1")
        && std::env::var("RUST_LIB_BACKTRACE").map_or(true, |v| v == "1")
    {
        writeln!(&mut std::io::stderr(), "{:?}", err).unwrap();
    } else {
        writeln!(&mut std::io::stderr(), "{:#}", err).unwrap();
    }
    ExitCode::from(1)
}

fn run() -> anyhow::Result<ExitCode> {
    let rustlog = env::var("BTTF_LOG").unwrap_or_else(|_| String::new());
    let level = match &*rustlog {
        "" | "off" => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        unk => anyhow::bail!("unrecognized log level '{}'", unk),
    };
    log::set_max_level(level);
    // We do this little dance here because we want `TimeZone::system()`
    // (run in the `TZ` lazy lock above) to emit log messages. But we
    // also want to use the time zone to emit localized datetimes in our
    // logger implementation! So we initialize the logger without a time
    // zone, which will then cause early log messages to be emitted in UTC.
    // But after that, we can set the time zone and things become local.
    let logger = logger::Logger::init()?;
    logger.set_time_zone(TZ.clone());
    cmd::run(&mut lexopt::Parser::from_env())?;
    Ok(ExitCode::SUCCESS)
}

fn read_env_bttf_now() -> anyhow::Result<Option<Timestamp>> {
    let Some(val) = std::env::var_os("BTTF_NOW") else { return Ok(None) };
    let Some(val) = val.to_str() else {
        anyhow::bail!(
            "`BTTF_NOW` environment variable is not valid UTF-8: {val:?}"
        )
    };
    val.parse::<Timestamp>()
        .context(
            "`BTTF_NOW` environment variable is not a valid RFC 3339 timestamp",
        )
        .map(Some)
}

fn read_env_bttf_locale() -> anyhow::Result<Option<Locale>> {
    let Some(val) = std::env::var_os("BTTF_LOCALE") else { return Ok(None) };
    let Some(val) = val.to_str() else {
        anyhow::bail!(
            "`BTTF_LOCALE` environment variable is not valid UTF-8: {val:?}"
        )
    };
    let locale = val.parse().with_context(|| {
        format!("failed to parse `BTTF_LOCALE` environment variable")
    })?;
    Ok(Some(locale))
}
