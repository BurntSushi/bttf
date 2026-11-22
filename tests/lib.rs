use std::{ffi::OsStr, path::Path, sync::LazyLock};

use jiff::{ZonedDateTime, civil};

mod command;
mod span;
mod tag;
mod time;
mod tz;
mod untag;

static NOW: LazyLock<ZonedDateTime> = LazyLock::new(|| {
    civil::date(2024, 7, 20)
        .at(16, 30, 55, 0)
        .in_tz("America/New_York")
        .unwrap()
});

/// A lightweight abstraction for managing temporary directories and the files
/// within it.
#[derive(Debug)]
struct TempDir(tempfile::TempDir);

impl TempDir {
    /// Create a new temporary directory.
    fn new() -> TempDir {
        TempDir(tempfile::tempdir().unwrap())
    }

    /// Create a new `bttf` command whose CWD is this directory.
    fn bttf_bare(&self) -> crate::command::Command {
        bttf_bare().current_dir(self.0.path())
    }

    /// Create a new `bttf` command whose CWD is this directory and the
    /// given arguments appended to it.
    fn bttf<T: AsRef<OsStr>>(
        &self,
        args: impl IntoIterator<Item = T>,
    ) -> crate::command::Command {
        self.bttf_bare().args(args)
    }

    /// Create a new file in this temporary directory with the given relative
    /// path and contents.
    fn create(
        &self,
        relative_path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) {
        let path = self.0.path().join(relative_path.as_ref());
        std::fs::write(&path, contents).unwrap();
    }
}

/// Return a command for the `bttf` binary and no argument.
fn bttf_bare() -> crate::command::Command {
    crate::command::bin("bttf")
        .env("TZ", "America/New_York")
        .env("BTTF_NOW", NOW.to_string())
        // So that when tests are run with locale support,
        // we still get consistent behavior as if bttf were
        // compiled without locale support.
        .env("BTTF_LOCALE", "und")
}

/// Return a command for the `bttf` binary with the given arguments appended
/// to it.
fn bttf<T: AsRef<OsStr>>(
    args: impl IntoIterator<Item = T>,
) -> crate::command::Command {
    bttf_bare().args(args)
}

/// Test that calling `bttf` with no arguments prints the current time.
#[test]
fn no_args() {
    crate::command::assert_cmd_snapshot!(
        bttf_bare(),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    2024 M07 20, Sat 16:30:55

    ----- stderr -----
    ",
    );
}

/// Test that `--help` and `-h` print usage to stdout and exit successfully,
/// both at the top level and on a command group.
#[test]
fn help() {
    crate::command::assert_cmd_snapshot!(
        bttf(["--help"]),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    A simple utility for doing datetime arithmetic, parsing and formatting.

    USAGE:
        bttf <command> ...

    COMMANDS:
        span   Tools for manipulating time spans/durations
        time   Tools for manipulating datetimes
        tag    Tag arbitrary data with datetimes or spans
        tz     Commands for working directly with time zones
        untag  Remove tags from previously tagged data

    ----- stderr -----
    ",
    );
    crate::command::assert_cmd_snapshot!(
        bttf(["-h"]),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    A simple utility for doing datetime arithmetic, parsing and formatting.

    USAGE:
        bttf <command> ...

    COMMANDS:
        span   Tools for manipulating time spans/durations
        time   Tools for manipulating datetimes
        tag    Tag arbitrary data with datetimes or spans
        tz     Commands for working directly with time zones
        untag  Remove tags from previously tagged data

    ----- stderr -----
    ",
    );
    crate::command::assert_cmd_snapshot!(
        bttf(["time", "--help"]),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Commands for working with datetimes.

    USAGE:
        bttf time <command> ...

    COMMANDS:
        add       Add a span to a datetime
        cmp       Compare datetimes
        end-of    Get the end of a year, month, week, etc
        fmt       Format a datetime
        in        Convert a datetime to a time zone
        parse     Parse a datetime
        relative  Parse a relative datetime
        round     Round a datetime
        seq       Generate a sequence of datetimes
        sort      Sort datetimes
        start-of  Get the start of a year, month, week, etc

    ----- stderr -----
    ",
    );
}

/// Test that `--version` prints the version to stdout and exits successfully
/// at the top level, on a command group, and on a leaf command. The version
/// line embeds the crate version and git hash, so it is filtered to a stable
/// placeholder.
#[test]
fn version() {
    insta::with_settings!({
        filters => vec![(r"bttf \d+\.\d+\.\d+.*", "bttf [VERSION]")],
    }, {
        crate::command::assert_cmd_snapshot!(
            bttf(["--version"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        bttf [VERSION]

        ----- stderr -----
        ",
        );
        crate::command::assert_cmd_snapshot!(
            bttf(["time", "--version"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        bttf [VERSION]

        ----- stderr -----
        ",
        );
        crate::command::assert_cmd_snapshot!(
            bttf(["time", "fmt", "--version"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        bttf [VERSION]

        ----- stderr -----
        ",
        );
        // We also test the case where `--version` is added after a command
        // argument. This previously failed because we weren't checking for
        // the `--version` flag in all cases (like we do for `--help`). I don't
        // think this was intentional.
        crate::command::assert_cmd_snapshot!(
            bttf(["time", "fmt", "now", "--version"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        bttf [VERSION]

        ----- stderr -----
        ",
        );
    });
}

/// Test that calling `bttf` when compiled with `locale` and when `BTTF_LOCALE`
/// is set does something sensible.
#[cfg(feature = "locale")]
#[test]
fn no_args_locale() {
    crate::command::assert_cmd_snapshot!(
        bttf_bare().env("BTTF_LOCALE", "en-US"),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Sat, Jul 20, 2024, 4:30:55 PM EDT

    ----- stderr -----
    ",
    );

    crate::command::assert_cmd_snapshot!(
        bttf_bare().env("BTTF_LOCALE", "en-GB"),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Sat, 20 Jul 2024, 16:30:55 GMT-4

    ----- stderr -----
    ",
    );

    crate::command::assert_cmd_snapshot!(
        bttf_bare().env("TZ", "Europe/London").env("BTTF_LOCALE", "en-GB"),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Sat, 20 Jul 2024, 21:30:55 BST

    ----- stderr -----
    ",
    );

    crate::command::assert_cmd_snapshot!(
        bttf_bare().env("TZ", "Europe/Paris").env("BTTF_LOCALE", "fr-LA"),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    sam. 20 juil. 2024, 22:30:55 UTC+2

    ----- stderr -----
    ",
    );

    crate::command::assert_cmd_snapshot!(
        bttf_bare()
            .env("TZ", "US/Eastern")
            .env("BTTF_LOCALE", "en-US-u-ca-buddhist"),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Sat, Jul 20, 2567 BE, 4:30:55 PM EDT

    ----- stderr -----
    ",
    );
}
