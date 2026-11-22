use jiff::fmt::{
    Write,
    strtime::{BrokenDownTime, Custom, Extension},
};
use jiff_icu::ConvertInto;
use writeable::Writeable;
use {
    icu_calendar::{Date, Iso},
    icu_datetime::{
        DateTimeFormatter as IcuDateTimeFormatter,
        DateTimeFormatterPreferences,
        fieldsets::{
            T, YMD, YMDET,
            enums::{
                CompositeFieldSet, DateAndTimeFieldSet, DateFieldSet,
                TimeFieldSet, ZoneFieldSet,
            },
            zone::SpecificShort,
        },
        preferences::HourCycle,
    },
    icu_locale::Locale as IcuLocale,
    icu_time::{Time, TimeZoneInfo, ZonedDateTime, zone::models::AtTime},
};

/// A wrapper around an ICU4X locale to create a locale formatter.
#[derive(Clone, Debug)]
pub struct Locale(IcuLocale);

impl Locale {
    /// Create a locale that is "unknown."
    pub fn unknown() -> Locale {
        Locale(IcuLocale::UNKNOWN)
    }

    /// Create a formatter that implements Jiff's `strtime::Custom` trait.
    pub fn to_formatter(&self) -> anyhow::Result<StrtimeLocaleFormatter> {
        let zone = ZoneFieldSet::SpecificShort(SpecificShort);
        let fset = if self.0.id.language.is_unknown() {
            // When using the `und` locale, just print the local time
            // without the time zone. This seems more faithful to the
            // POSIX C locale, and also (at present) matches what Jiff
            // will do by default.
            let fset = DateAndTimeFieldSet::YMDET(YMDET::medium());
            CompositeFieldSet::DateTime(fset)
        } else {
            let combo =
                DateAndTimeFieldSet::YMDET(YMDET::medium()).with_zone(zone);
            CompositeFieldSet::DateTimeZone(combo)
        };
        let datetime = IcuDateTimeFormatter::try_new((&self.0).into(), fset)?;

        let fset = DateFieldSet::YMD(YMD::medium());
        let date = IcuDateTimeFormatter::try_new((&self.0).into(), fset)?;

        let fset = TimeFieldSet::T(T::medium());
        let time = IcuDateTimeFormatter::try_new((&self.0).into(), fset)?;

        let fset = TimeFieldSet::T(T::medium());
        let mut prefs = DateTimeFormatterPreferences::from(&self.0);
        prefs.hour_cycle = Some(HourCycle::H12);
        let time12 = IcuDateTimeFormatter::try_new(prefs, fset)?;

        Ok(StrtimeLocaleFormatter { datetime, date, time, time12 })
    }
}

impl std::str::FromStr for Locale {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Locale> {
        Ok(Locale(s.parse::<IcuLocale>()?))
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A collection of ICU4X datetime formatters for `strftime` formatting.
#[derive(Debug)]
pub struct StrtimeLocaleFormatter {
    datetime: IcuDateTimeFormatter<CompositeFieldSet>,
    date: IcuDateTimeFormatter<DateFieldSet>,
    time: IcuDateTimeFormatter<TimeFieldSet>,
    time12: IcuDateTimeFormatter<TimeFieldSet>,
}

impl Custom for StrtimeLocaleFormatter {
    fn format_datetime(
        &self,
        _config: &jiff::fmt::strtime::Config<dyn Custom + '_>,
        _ext: &Extension,
        tm: &BrokenDownTime,
        wtr: &mut dyn Write,
    ) -> Result<(), jiff::Error> {
        let zdt = tm.to_zoned_datetime()?;
        let zdt: ZonedDateTime<Iso, TimeZoneInfo<AtTime>> =
            (&zdt).convert_into();
        self.datetime
            .format(&zdt)
            .write_to(&mut JiffFmtWriteToStdFmtWrite(wtr))
            .map_err(|_| {
                jiff::Error::from_args(format_args!(
                    "failed to format datetime using ICU4X locale",
                ))
            })?;
        Ok(())
    }

    fn format_date(
        &self,
        _config: &jiff::fmt::strtime::Config<dyn Custom + '_>,
        _ext: &Extension,
        tm: &BrokenDownTime,
        wtr: &mut dyn Write,
    ) -> Result<(), jiff::Error> {
        let date = tm.to_date()?;
        let date: Date<Iso> = date.convert_into();
        self.date
            .format(&date)
            .write_to(&mut JiffFmtWriteToStdFmtWrite(wtr))
            .map_err(|_| {
                jiff::Error::from_args(format_args!(
                    "failed to format date using ICU4X locale",
                ))
            })?;
        Ok(())
    }

    fn format_time(
        &self,
        _config: &jiff::fmt::strtime::Config<dyn Custom + '_>,
        _ext: &Extension,
        tm: &BrokenDownTime,
        wtr: &mut dyn Write,
    ) -> Result<(), jiff::Error> {
        let time = tm.to_time()?;
        let time: Time = time.convert_into();
        self.time
            .format(&time)
            .write_to(&mut JiffFmtWriteToStdFmtWrite(wtr))
            .map_err(|_| {
                jiff::Error::from_args(format_args!(
                    "failed to format time using ICU4X locale",
                ))
            })?;
        Ok(())
    }

    fn format_12hour_time(
        &self,
        _config: &jiff::fmt::strtime::Config<dyn Custom + '_>,
        _ext: &Extension,
        tm: &BrokenDownTime,
        wtr: &mut dyn Write,
    ) -> Result<(), jiff::Error> {
        let time = tm.to_time()?;
        let time: Time = time.convert_into();
        self.time12
            .format(&time)
            .write_to(&mut JiffFmtWriteToStdFmtWrite(wtr))
            .map_err(|_| {
                jiff::Error::from_args(format_args!(
                    "failed to format time using ICU4X locale",
                ))
            })?;
        Ok(())
    }
}

// This was the original more generic version that I came up with. Writing
// down the trait bounds was QUITE the effort, so I am keeping this around
// for now. But the above is a more concrete version that works for bttf's
// specific use case. In particular, when I wrote the below, I didn't know
// about the enums that provide a more dynamic API that is way more ergonomic.
/*
#[derive(Debug)]
pub struct DateTimeFormatter<FSet>(IcuDateTimeFormatter<FSet>)
where
    FSet: DateTimeNamesMarker;

impl<FSet> jiff::fmt::strtime::Locale for DateTimeFormatter<FSet>
where
    FSet::D: DateInputMarkers,
    FSet::T: TimeMarkers,
    FSet::Z: ZoneMarkers,
    FSet: DateTimeMarkers,
    ZonedDateTime<Iso, TimeZoneInfo<Full>>: ConvertCalendar,
    for<'a> ZonedDateTime<Ref<'a, AnyCalendar>, TimeZoneInfo<Full>>:
        AllInputMarkers<FSet>,
{
    fn format_datetime<W: jiff::fmt::Write>(
        &self,
        _config: &jiff::fmt::strtime::Config<Self>,
        tm: &BrokenDownTime,
        wtr: &mut W,
    ) -> Result<(), jiff::Error> {
        let zdt = tm.to_zoned_datetime()?;
        let zdt: ZonedDateTime<Iso, TimeZoneInfo<Full>> =
            (&zdt).convert_into();
        self.0
            .format(&zdt)
            .write_to(&mut JiffFmtWriteToStdFmtWrite(wtr))
            .map_err(|_| {
                jiff::Error::from_args(format_args!(
                    "failed to format datetime using ICU4X locale",
                ))
            })?;
        Ok(())
    }
}
*/

/// An adapter for providing a `std::fmt::Write` implementation given a
/// `jiff::fmt::Write` implementation.
///
/// (This is the inverse of `jiff::fmt::StdFmtWrite`.)
struct JiffFmtWriteToStdFmtWrite<W>(W);

impl<W: jiff::fmt::Write> std::fmt::Write for JiffFmtWriteToStdFmtWrite<W> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write_str(s).map_err(|_| std::fmt::Error)
    }
}
