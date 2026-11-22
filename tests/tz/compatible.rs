use crate::{bttf, command::assert_cmd_snapshot};

#[test]
fn basic() {
    let has_legacy = jiff::tz::TimeZone::get("Australia/South").is_ok();
    if has_legacy {
        assert_cmd_snapshot!(
            bttf(["tz", "compatible", "2025-03-09T17:00+10:30"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        Australia/Adelaide
        Australia/Broken_Hill
        Australia/South
        Australia/Yancowinna

        ----- stderr -----
        ",
        );
    } else {
        assert_cmd_snapshot!(
            bttf(["tz", "compatible", "2025-03-09T17:00+10:30"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        Australia/Adelaide
        Australia/Broken_Hill
        Australia/Yancowinna

        ----- stderr -----
        ",
        );
    }
}

#[test]
fn unknown() {
    assert_cmd_snapshot!(
        bttf(["tz", "compatible", "2025-03-09T17:00Z"]),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Etc/Unknown

    ----- stderr -----
    ",
    );

    assert_cmd_snapshot!(
        bttf(["tz", "compatible", "2025-03-09T17:00-00:00"]),
        @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Etc/Unknown

    ----- stderr -----
    ",
    );
}

// This test is marked as ignored because its output tends to vary too much
// from system to system.
#[test]
#[ignore]
fn utc() {
    insta::with_settings!({
        filters => vec![(r"localtime\n", "")],
    }, {
        assert_cmd_snapshot!(
            bttf(["tz", "compatible", "2025-03-09T17:00+00:00"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        Africa/Abidjan
        Africa/Accra
        Africa/Bamako
        Africa/Banjul
        Africa/Bissau
        Africa/Casablanca
        Africa/Conakry
        Africa/Dakar
        Africa/El_Aaiun
        Africa/Freetown
        Africa/Lome
        Africa/Monrovia
        Africa/Nouakchott
        Africa/Ouagadougou
        Africa/Sao_Tome
        Africa/Timbuktu
        America/Danmarkshavn
        Antarctica/Troll
        Atlantic/Canary
        Atlantic/Faeroe
        Atlantic/Faroe
        Atlantic/Madeira
        Atlantic/Reykjavik
        Atlantic/St_Helena
        Eire
        Etc/GMT
        Etc/GMT+0
        Etc/GMT-0
        Etc/GMT0
        Etc/Greenwich
        Etc/UCT
        Etc/UTC
        Etc/Universal
        Etc/Zulu
        Europe/Belfast
        Europe/Dublin
        Europe/Guernsey
        Europe/Isle_of_Man
        Europe/Jersey
        Europe/Lisbon
        Europe/London
        Factory
        GB
        GB-Eire
        GMT
        GMT+0
        GMT-0
        GMT0
        Greenwich
        Iceland
        Portugal
        UCT
        UTC
        Universal
        WET
        Zulu

        ----- stderr -----
        ",
        );
    });

    // This one is somewhat unfortunate. Arguably this should just print
    // `UTC`, since an affirmative time zone annotation has been given...
    insta::with_settings!({
        filters => vec![(r"localtime\n", "")],
    }, {
        assert_cmd_snapshot!(
            bttf(["tz", "compatible", "2025-03-09T17:00[UTC]"]),
            @r"
        success: true
        exit_code: 0
        ----- stdout -----
        Africa/Abidjan
        Africa/Accra
        Africa/Bamako
        Africa/Banjul
        Africa/Bissau
        Africa/Casablanca
        Africa/Conakry
        Africa/Dakar
        Africa/El_Aaiun
        Africa/Freetown
        Africa/Lome
        Africa/Monrovia
        Africa/Nouakchott
        Africa/Ouagadougou
        Africa/Sao_Tome
        Africa/Timbuktu
        America/Danmarkshavn
        Antarctica/Troll
        Atlantic/Canary
        Atlantic/Faeroe
        Atlantic/Faroe
        Atlantic/Madeira
        Atlantic/Reykjavik
        Atlantic/St_Helena
        Eire
        Etc/GMT
        Etc/GMT+0
        Etc/GMT-0
        Etc/GMT0
        Etc/Greenwich
        Etc/UCT
        Etc/UTC
        Etc/Universal
        Etc/Zulu
        Europe/Belfast
        Europe/Dublin
        Europe/Guernsey
        Europe/Isle_of_Man
        Europe/Jersey
        Europe/Lisbon
        Europe/London
        Factory
        GB
        GB-Eire
        GMT
        GMT+0
        GMT-0
        GMT0
        Greenwich
        Iceland
        Portugal
        UCT
        UTC
        Universal
        WET
        Zulu

        ----- stderr -----
        ",
        );
    });
}
