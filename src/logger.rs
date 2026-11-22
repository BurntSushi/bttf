// This module defines a super simple logger that works with the `log` crate.
// We don't need anything fancy; just basic log levels and the ability to
// print to stderr. We therefore avoid bringing in extra dependencies just
// for this functionality.

use std::{
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use {jiff::tz::TimeZone, log::Log};

use crate::style::Theme;

/// The simplest possible logger that logs to stderr.
///
/// This logger does no filtering. Instead, it relies on the `log` crates
/// filtering via its global max_level setting.
#[derive(Debug)]
pub struct Logger {
    tz: Mutex<Option<TimeZone>>,
}

impl Logger {
    /// Create a new logger that logs to stderr and initialize it as the
    /// global logger. If there was a problem setting the logger, then an
    /// error is returned.
    pub fn init() -> Result<&'static Logger, log::SetLoggerError> {
        let logger = Box::leak(Box::new(Logger { tz: Mutex::new(None) }));
        log::set_logger(logger)?;
        Ok(logger)
    }

    pub fn set_time_zone(&self, tz: TimeZone) {
        let mut logger_tz = self.tz.lock().unwrap();
        *logger_tz = Some(tz);
    }
}

impl Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        // We set the log level via log::set_max_level, so we don't need to
        // implement filtering here.
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        // We avoid calling `ZonedDateTime::now()` here, because that might try
        // to read the system time zone from disk, and that in turn can emit
        // log messages. But this is the log implementation, so side-step
        // that to avoid endless recursion.
        //
        // This does technically rely on Jiff not emitting log statements
        // for `Timestamp::now()`, `Timestamp::to_zoned_datetime` and for
        // formatting. We should probably make that a semver guarantee...
        let ts = jiff::Timestamp::now();
        let now = self
            .tz
            .lock()
            .unwrap()
            .clone()
            .map(|tz| ts.to_zoned_datetime(tz).to_string())
            .unwrap_or_else(|| ts.to_string());
        match (record.file(), record.line()) {
            (Some(file), Some(line)) => {
                eprintln!(
                    "{}|{}|{}:{}: {}",
                    Theme::stderr().highlight(now),
                    record.level(),
                    relative(file),
                    line,
                    record.args()
                );
            }
            (Some(file), None) => {
                eprintln!(
                    "{}|{}|{}: {}",
                    now,
                    record.level(),
                    relative(file),
                    record.args()
                );
            }
            _ => {
                eprintln!("{}|{}: {}", now, record.level(), record.args());
            }
        }
    }

    fn flush(&self) {
        // We use eprintln! which is flushed on every call.
    }
}

fn relative<'p>(path: &'p str) -> &'p str {
    let Some(cwd) = cwd() else { return path };
    let Ok(relative) = Path::new(path).strip_prefix(cwd) else { return path };
    let Some(relative) = relative.to_str() else { return path };
    relative
}

fn cwd() -> Option<&'static Path> {
    static CWD: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::current_dir().ok());
    CWD.as_deref()
}
