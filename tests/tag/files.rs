use crate::command::assert_cmd_snapshot;

use crate::TempDir;

#[test]
fn positional() {
    let tmp = TempDir::new();
    tmp.create("foo", "2025-03-15T00-04");

    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files", "foo"]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"2025-03-15T00-04"}],"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn stdin() {
    let tmp = TempDir::new();
    tmp.create("foo", "2025-03-15T00-04");

    // Notice that the line terminator is missing in the input and, therefore,
    // also the output!
    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files"]).stdin("foo"),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"2025-03-15T00-04"}],"data":{"text":"foo"}}

    ----- stderr -----
    "#,
    );

    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files"]).stdin("foo\n"),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"2025-03-15T00-04"}],"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn not_utf8() {
    let tmp = TempDir::new();
    tmp.create("foo", b"\xFF2025-03-15T00-04\xFF");

    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files", "foo"]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"2025-03-15T00-04"}],"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn nothing() {
    let tmp = TempDir::new();
    tmp.create("foo", "2025-03-15T00-04");

    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files", "--auto=none", "foo"]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn explicit_regex_disables_auto() {
    let tmp = TempDir::new();
    tmp.create("foo", "2025-03-15T00-04 03/15/2025");

    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files", "-e", r"[0-9]{2}/[0-9]{2}/[0-9]{4}", "foo"]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"03/15/2025"}],"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn explicit_regex_with_tag() {
    let tmp = TempDir::new();
    tmp.create("foo", "date: 03/15/2025\nSome text about 12/01/2024.\n");

    assert_cmd_snapshot!(
        tmp.bttf([
            "tag",
            "files",
            "-e", r"date: (?<tag>[0-9]{2}/[0-9]{2}/[0-9]{4})",
            "foo",
        ]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"03/15/2025"}],"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn explicit_regex_with_auto() {
    let tmp = TempDir::new();
    tmp.create("foo", "2025-03-15T00-04");
    tmp.create("bar", "03/15/2025");

    assert_cmd_snapshot!(
        tmp.bttf([
            "tag",
            "files",
            "--auto", "datetime",
            "-e", r"[0-9]{2}/[0-9]{2}/[0-9]{4}",
            "foo",
            "bar",
        ]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"2025-03-15T00-04"}],"data":{"text":"foo\n"}}
    {"tags":[{"value":"03/15/2025"}],"data":{"text":"bar\n"}}

    ----- stderr -----
    "#,
    );
}

#[test]
fn auto_time_zone() {
    let tmp = TempDir::new();
    tmp.create(
        "foo",
        "America/New_York Pacific/Honolulu Antarctica/Troll Asia/Jerusalem",
    );

    assert_cmd_snapshot!(
        tmp.bttf(["tag", "files", "--all", "--auto", "timezone", "foo"]),
        @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    {"tags":[{"value":"America/New_York"},{"value":"Pacific/Honolulu"},{"value":"Antarctica/Troll"},{"value":"Asia/Jerusalem"}],"data":{"text":"foo\n"}}

    ----- stderr -----
    "#,
    );
}
