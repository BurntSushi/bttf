use std::{
    cmp::Ordering,
    ops::{Range, RangeInclusive},
    sync::Arc,
};

use {
    anyhow::Context,
    jiff::{
        Span, ToSpan, ZonedDateTime,
        civil::{DateTime, Weekday},
        tz::{AmbiguousOffset, TimeZone},
    },
};

use crate::{
    args::{Usage, flags},
    weekdate::{WeekDate, first_of_week, last_of_week},
};

// The representation for recurrence rules seems quite fat overall. I don't
// think it matters much for bttf specifically, but if this were to be moved
// into a library, it might be worth trying to slim this down. Maybe there's a
// way to combine each of the lists into one big one, since they're generally
// expected to be small (or mostly empty most of the time).
//
// Also, right now, the frequency, start and "until" properties are coupled
// with the recurrence rule itself. But these are fixed length data that could
// conceivably vary while the rest of the rule stays invariant. For example,
// perhaps we want to construct the rule and then use it with different
// starting points. The API below doesn't really support this, but if we move
// it to a library, we should probably make that possible.

/// The RFC 5545 recurrence rule implementation.
#[derive(Clone, Debug)]
pub struct RecurrenceRule {
    inner: Arc<RecurrenceRuleInner>,
}

#[derive(Debug)]
struct RecurrenceRuleInner {
    freq: Frequency,
    zoned_start: ZonedDateTime,
    civil_start: DateTime,
    zoned_until: Option<ZonedDateTime>,
    interval: Span,
    by_month: Box<[i8]>,
    // can be negative
    by_week: Box<[i8]>,
    // can be negative
    by_year_day: Box<[i16]>,
    // can be negative
    by_month_day: Box<[i8]>,
    // can be negative
    by_week_day: Box<[ByWeekday]>,
    by_hour: Box<[i8]>,
    by_minute: Box<[i8]>,
    by_second: Box<[i8]>,
    // can be negative
    by_set_pos: Box<[i32]>,
    week_start: Weekday,
}

impl RecurrenceRule {
    /// Returns a builder for constructing a `RecurrenceRule`.
    ///
    /// The frequency and the starting point are the only two things required
    /// to create a rule.
    pub fn builder(
        freq: Frequency,
        start: ZonedDateTime,
    ) -> RecurrenceRuleBuilder {
        RecurrenceRuleBuilder::new(freq, start)
    }

    /// Returns an iterator over all datetimes in this recurrence rule.
    ///
    /// Note that the iterator may be "infinite," in the sense that it returns
    /// datetimes all the way up to Jiff's supported maximum datetime. Callers
    /// should therefore either specify an `RecurrenceRuleBuilder::until` rule
    /// or call `take(N)` to limit the number of datetimes to `N`.
    pub fn iter(&self) -> RecurrenceIter<'_> {
        RecurrenceIter {
            rule: self,
            set: RecurrenceSet::new(),
            cur: Some((0, self.inner.civil_start)),
        }
    }

    /// Returns the time zone that datetimes emitted by this rule should be in.
    fn time_zone(&self) -> &TimeZone {
        self.inner.zoned_start.time_zone()
    }
}

impl<'r> IntoIterator for &'r RecurrenceRule {
    type IntoIter = RecurrenceIter<'r>;
    type Item = ZonedDateTime;

    fn into_iter(self) -> RecurrenceIter<'r> {
        self.iter()
    }
}

/// An expander for a single RFC 5545 recurrence rule and datetime.
#[derive(Clone, Debug)]
struct Expander<'a> {
    /// The rule we are expanding.
    rule: &'a RecurrenceRule,
    /// The "current" datetime we are expanding.
    ///
    /// How this datetime is used depends on the frequency we are expanding
    /// for.
    cur: DateTime,
}

impl<'a> Expander<'a> {
    /// Expand into the set provided.
    fn expand(&self, set: &mut RecurrenceSet) {
        match self.frequency() {
            Frequency::Yearly => self.yearly(set),
            Frequency::Monthly => self.monthly(set),
            Frequency::Weekly => self.weekly(set),
            Frequency::Daily => self.daily(set),
            Frequency::Hourly => self.hourly(set),
            Frequency::Minutely => self.minutely(set),
            Frequency::Secondly => self.secondly(set),
        }
        set.canonicalize();

        // When BYSETPOS is present, we need to convert all of our civil
        // datetimes to zoned datetimes, check them with our start/until rules
        // and put them into another set. Only when we do this can we process
        // the BYSETPOS rule. In particular, BYSETPOS may contain negative
        // values, which fundamentally requires knowing the length of the
        // recurrence set.
        //
        // We could in theory do better here in the case where BYSETPOS is only
        // positive values, but it seems like it's most commonly used with
        // negative values. Moreover, BYSETPOS is probably rare by itself,
        // so it's not clear that we need to care too much about this extra
        // shuffling.
        if self.has_by_set_pos() {
            while let Some(zdt) = set.pop_civil(self.rule) {
                set.zoned.push(zdt);
            }
            set.zoned.sort_by(|zdt1, zdt2| zdt1.cmp(zdt2));
            set.zoned.dedup();

            let mut position = 0;
            let len = set.zoned.len();
            set.zoned.retain_mut(|_| {
                let keep = self.satisfies_by_set_pos(position, len);
                position += 1;
                keep
            });
            set.zoned.reverse();
        }
    }

    /// Populate `set` with datetimes according to this rule at a YEARLY
    /// frequency.
    fn yearly(&self, set: &mut RecurrenceSet) {
        set.insert(self.cur);
        if self.has_by_week_day() {
            if self.has_by_week() {
                self.expand_by_week(set);
                self.expand_by_week_day_weekly(set);
                self.limit_by_month(set);
            } else if self.has_by_month() {
                self.expand_by_month(set);
                self.expand_by_week_day_monthly(set);
            } else {
                self.expand_by_week_day_yearly(set);
            }
            self.limit_by_year_day(set);
            self.limit_by_month_day(set);
        } else if self.has_by_week() {
            self.expand_by_week(set);
            // Note that we use slightly different expansionary
            // behavior here for `by_week` than for other things[1].
            // For example, at YEARLY frequency with BYMONTH={2,5},
            // you'll get two datetimes in February and May with the
            // day and time of the starting point. You don't get every
            // day in the specified months. But when `by_week` is
            // present, you get every day in each week specified.
            //
            // This is... somewhat odd. But I guess makes sense for
            // certain kinds of expressions. And it's also what
            // Python's `dateutil` does. So we do the same.
            //
            // [1]: https://stackoverflow.com/questions/48064349
            set.expand(|dt| {
                (0..=6).filter_map(move |n| dt.checked_add(n.days()).ok())
            });
            self.limit_by_month(set);
            self.limit_by_year_day(set);
            self.limit_by_month_day(set);
        } else if self.has_by_month() {
            self.expand_by_month(set);
            self.expand_by_month_day(set);
            self.limit_by_year_day(set);
        } else if self.has_by_month_day() {
            set.expand(|dt| {
                (1..=12).filter_map(move |month| {
                    dt.with().month(month).build().ok()
                })
            });
            self.expand_by_month_day(set);
            self.limit_by_year_day(set);
        } else if self.has_by_year_day() {
            self.expand_by_year_day(set);
        }
        self.expand_by_hour(set);
        self.expand_by_minute(set);
        self.expand_by_second(set);
    }

    /// Populate `set` with datetimes according to this rule at a MONTHLY
    /// frequency.
    fn monthly(&self, set: &mut RecurrenceSet) {
        // N.B. BYWEEKNO and BYYEARDAY are not allowed here.
        if !self.satisfies_by_month(self.cur) {
            return;
        }

        set.insert(self.cur);
        if self.has_by_week_day() {
            self.expand_by_week_day_monthly(set);
            self.limit_by_month_day(set);
        } else {
            self.expand_by_month_day(set);
        }
        self.expand_by_hour(set);
        self.expand_by_minute(set);
        self.expand_by_second(set);
    }

    /// Populate `set` with datetimes according to this rule at a WEEKLY
    /// frequency.
    fn weekly(&self, set: &mut RecurrenceSet) {
        // N.B. BYWEEKNO, BYYEARDAY and BYMONTHDAY are not allowed here. Also,
        // BYDAY cannot have a numeric weekday.
        if !self.satisfies_by_month(self.cur) {
            return;
        }
        set.insert(self.cur);
        self.expand_by_week_day_weekly(set);
        self.expand_by_hour(set);
        self.expand_by_minute(set);
        self.expand_by_second(set);
    }

    /// Populate `set` with datetimes according to this rule at a DAILY
    /// frequency.
    fn daily(&self, set: &mut RecurrenceSet) {
        // N.B. BYWEEKNO and BYYEARDAY are not allowed here. Also, BYDAY
        // cannot have a numeric weekday. I get the restriction on BYDAY, but
        // it seems like BYWEEKNO and BYYEARDAY could be trivially supported
        // in exactly the same way that BYMONTH and BYMONTHDAY are supported
        // below.
        //
        // This is just so deliciously easy compared to YEARLY...
        if !self.satisfies_by_month(self.cur) {
            return;
        }
        if !self.satisfies_by_month_day(self.cur) {
            return;
        }
        if !self.satisfies_by_week_day(self.cur) {
            return;
        }
        set.insert(self.cur);
        self.expand_by_hour(set);
        self.expand_by_minute(set);
        self.expand_by_second(set);
    }

    /// Populate `set` with datetimes according to this rule at a HOURLY
    /// frequency.
    fn hourly(&self, set: &mut RecurrenceSet) {
        // N.B. BYWEEKNOis not allowed here. Also, BYDAY cannot have a numeric
        // weekday.
        if !self.satisfies_by_month(self.cur) {
            return;
        }
        if !self.satisfies_by_year_day(self.cur) {
            return;
        }
        if !self.satisfies_by_month_day(self.cur) {
            return;
        }
        if !self.satisfies_by_week_day(self.cur) {
            return;
        }
        if !self.satisfies_by_hour(self.cur) {
            return;
        }
        set.insert(self.cur);
        self.expand_by_minute(set);
        self.expand_by_second(set);
    }

    /// Populate `set` with datetimes according to this rule at a MINUTELY
    /// frequency.
    fn minutely(&self, set: &mut RecurrenceSet) {
        // N.B. BYWEEKNOis not allowed here. Also, BYDAY cannot have a numeric
        // weekday.
        if !self.satisfies_by_month(self.cur) {
            return;
        }
        if !self.satisfies_by_year_day(self.cur) {
            return;
        }
        if !self.satisfies_by_month_day(self.cur) {
            return;
        }
        if !self.satisfies_by_week_day(self.cur) {
            return;
        }
        if !self.satisfies_by_hour(self.cur) {
            return;
        }
        if !self.satisfies_by_minute(self.cur) {
            return;
        }
        set.insert(self.cur);
        self.expand_by_second(set);
    }

    /// Populate `set` with datetimes according to this rule at a SECONDLY
    /// frequency.
    fn secondly(&self, set: &mut RecurrenceSet) {
        // N.B. BYWEEKNOis not allowed here. Also, BYDAY cannot have a numeric
        // weekday.
        if !self.satisfies_by_month(self.cur) {
            return;
        }
        if !self.satisfies_by_year_day(self.cur) {
            return;
        }
        if !self.satisfies_by_month_day(self.cur) {
            return;
        }
        if !self.satisfies_by_week_day(self.cur) {
            return;
        }
        if !self.satisfies_by_hour(self.cur) {
            return;
        }
        if !self.satisfies_by_minute(self.cur) {
            return;
        }
        if !self.satisfies_by_second(self.cur) {
            return;
        }
        set.insert(self.cur);
    }

    /// Returns true if there is at least one BYMONTH value.
    fn has_by_month(&self) -> bool {
        !self.rule().by_month.is_empty()
    }

    /// Returns true if there is at least one BYWEEKNO value.
    fn has_by_week(&self) -> bool {
        !self.rule().by_week.is_empty()
    }

    /// Returns true if there is at least one BYYEARDAY value.
    fn has_by_year_day(&self) -> bool {
        !self.rule().by_year_day.is_empty()
    }

    /// Returns true if there is at least one BYMONTHDAY value.
    fn has_by_month_day(&self) -> bool {
        !self.rule().by_month_day.is_empty()
    }

    /// Returns true if there is at least one BYDAY value.
    fn has_by_week_day(&self) -> bool {
        !self.rule().by_week_day.is_empty()
    }

    /// Returns true if there is at least one BYHOUR value.
    fn has_by_hour(&self) -> bool {
        !self.rule().by_hour.is_empty()
    }

    /// Returns true if there is at least one BYMINUTE value.
    fn has_by_minute(&self) -> bool {
        !self.rule().by_minute.is_empty()
    }

    /// Returns true if there is at least one BYSECOND value.
    fn has_by_second(&self) -> bool {
        !self.rule().by_second.is_empty()
    }

    /// Returns true if there is at least one BYSETPOS value.
    fn has_by_set_pos(&self) -> bool {
        !self.rule().by_set_pos.is_empty()
    }

    /// Returns true if and only if the given datetime satisfies the
    /// BYMONTH rule.
    fn satisfies_by_month(&self, dt: DateTime) -> bool {
        !self.has_by_month() || self.rule().by_month.contains(&dt.month())
    }

    /// Returns true if and only if the given datetime satisfies the
    /// BYYEARDAY rule.
    fn satisfies_by_year_day(&self, dt: DateTime) -> bool {
        if !self.has_by_year_day() {
            return true;
        }
        let positive = dt.day_of_year();
        // Minus 1 because -1 is the last day of the year, and the days of the
        // year are 1-indexed.
        let negative = positive - 1 - dt.days_in_year();
        self.rule().by_year_day.binary_search(&positive).is_ok()
            || self.rule().by_year_day.binary_search(&negative).is_ok()
    }

    /// Returns true if and only if the given datetime satisfies the
    /// BYMONTHDAY rule.
    fn satisfies_by_month_day(&self, dt: DateTime) -> bool {
        if !self.has_by_month_day() {
            return true;
        }
        let positive = dt.day();
        // Minus 1 because -1 is the last day of month, and the days of the
        // month are 1-indexed.
        let negative = positive - 1 - dt.days_in_month();
        self.rule().by_month_day.binary_search(&positive).is_ok()
            || self.rule().by_month_day.binary_search(&negative).is_ok()
    }

    /// Returns true only if the weekday for the given datetime is allowed by
    /// this recurrence rule.
    ///
    /// # Panics
    ///
    /// This panics when this is a numbered weekday. In effect, this shouldn't
    /// be used in contexts where BYDAY can contain numbered weekdays. That
    /// limits this to YEARLY and MONTHLY frequencies (and in the YEARLY case,
    /// only when BYWEEKNO is not used).
    fn satisfies_by_week_day(&self, dt: DateTime) -> bool {
        if !self.has_by_week_day() {
            return true;
        }
        let wd = dt.weekday();
        self.rule().by_week_day.iter().any(|bywd| bywd.is_match(wd))
    }

    /// Returns true if and only if the given datetime satisfies the
    /// BYHOUR rule.
    fn satisfies_by_hour(&self, dt: DateTime) -> bool {
        !self.has_by_hour() || self.rule().by_hour.contains(&dt.hour())
    }

    /// Returns true if and only if the given datetime satisfies the
    /// BYMINUTE rule.
    fn satisfies_by_minute(&self, dt: DateTime) -> bool {
        !self.has_by_minute() || self.rule().by_minute.contains(&dt.minute())
    }

    /// Returns true if and only if the given datetime satisfies the
    /// BYSECOND rule.
    fn satisfies_by_second(&self, dt: DateTime) -> bool {
        !self.has_by_second() || self.rule().by_second.contains(&dt.second())
    }

    /// Returns true if and only if the given `position` in an ordered set of
    /// length `len` satisfies the BYSETPOS rule.
    /// BYMONTHDAY rule.
    fn satisfies_by_set_pos(&self, position: usize, len: usize) -> bool {
        if !self.has_by_set_pos() {
            return true;
        }
        let Ok(position) = i32::try_from(position) else { return false };
        let Ok(len) = i32::try_from(len) else { return false };
        let positive = position + 1;
        // Minus 1 because -1 is the last datetime of a recurrence set, and the
        // positions of a set are 1-indexed according to RFC 5545.
        let negative = positive - 1 - len;
        self.rule().by_set_pos.binary_search(&positive).is_ok()
            || self.rule().by_set_pos.binary_search(&negative).is_ok()
    }

    /// Removes any element in the given set whose month is
    /// inconsistent with the BYMONTH rule.
    ///
    /// If the BYMONTH rule is empty, then this is a no-op.
    fn limit_by_month(&self, set: &mut RecurrenceSet) {
        if !self.has_by_month() {
            return;
        }
        set.retain(|dt| self.satisfies_by_month(*dt));
    }

    /// Removes any element in the given set whose day of the year is
    /// inconsistent with the BYYEARDAY rule.
    ///
    /// If the BYYEARDAY rule is empty, then this is a no-op.
    fn limit_by_year_day(&self, set: &mut RecurrenceSet) {
        if !self.has_by_year_day() {
            return;
        }
        set.retain(|dt| self.satisfies_by_year_day(*dt));
    }

    /// Removes any element in the given set whose day of the month is
    /// inconsistent with the BYMONTHDAY rule.
    ///
    /// If the BYMONTHDAY rule is empty, then this is a no-op.
    fn limit_by_month_day(&self, set: &mut RecurrenceSet) {
        if !self.has_by_month_day() {
            return;
        }
        set.retain(|dt| self.satisfies_by_month_day(*dt));
    }

    /// When the rule has a non-empty number of BYMONTH values, then this
    /// expands every datetime in the given set with the corresponding
    /// months.
    fn expand_by_month(&self, set: &mut RecurrenceSet) {
        if !self.has_by_month() {
            return;
        }
        set.expand(|dt| self.iter_by_month(dt))
    }

    /// When the rule has a non-empty number of BYWEEKNO values, then this
    /// expands every datetime in the given set with the corresponding
    /// week numbers.
    fn expand_by_week(&self, set: &mut RecurrenceSet) {
        if !self.has_by_week() {
            return;
        }
        set.expand(|dt| self.iter_by_week(dt))
    }

    /// When the rule has a non-empty number of BYYEARDAY values, then this
    /// expands every datetime in the given set with the corresponding
    /// year days.
    fn expand_by_year_day(&self, set: &mut RecurrenceSet) {
        if !self.has_by_year_day() {
            return;
        }
        set.expand(|dt| self.iter_by_year_day(dt))
    }

    /// When the rule has a non-empty number of BYMONTHDAY values, then this
    /// expands every datetime in the given set with the corresponding
    /// month days.
    fn expand_by_month_day(&self, set: &mut RecurrenceSet) {
        if !self.has_by_month_day() {
            return;
        }
        set.expand(|dt| self.iter_by_month_day(dt))
    }

    /// When the rule has a non-empty number of BYDAY values, then this
    /// expands every datetime in the given set with the corresponding
    /// BYDAY rules at a YEARLY frequency.
    fn expand_by_week_day_yearly(&self, set: &mut RecurrenceSet) {
        if !self.has_by_week_day() {
            return;
        }
        set.expand(|dt| self.iter_by_week_day_yearly(dt))
    }

    /// When the rule has a non-empty number of BYDAY values, then this
    /// expands every datetime in the given set with the corresponding
    /// BYDAY rules at a MONTHLY frequency.
    fn expand_by_week_day_monthly(&self, set: &mut RecurrenceSet) {
        if !self.has_by_week_day() {
            return;
        }
        set.expand(|dt| self.iter_by_week_day_monthly(dt))
    }

    /// When the rule has a non-empty number of BYDAY values, then this
    /// expands every datetime in the given set with the corresponding
    /// BYDAY rules at a WEEKLY frequency.
    ///
    /// # Panics
    ///
    /// When any `ByWeekday` is `Numbered`. RFC 5545 doesn't permit that
    /// construction at anything other than YEARLY and MONTHLY frequency.
    fn expand_by_week_day_weekly(&self, set: &mut RecurrenceSet) {
        if !self.has_by_week_day() {
            return;
        }
        set.expand(|dt| self.iter_by_week_day_weekly(dt))
    }

    /// When the rule has a non-empty number of BYHOUR values, then this
    /// expands every datetime in the given set with the corresponding
    /// hours.
    fn expand_by_hour(&self, set: &mut RecurrenceSet) {
        if !self.has_by_hour() {
            return;
        }
        set.expand(|dt| self.iter_by_hour(dt))
    }

    /// When the rule has a non-empty number of BYMINUTE values, then this
    /// expands every datetime in the given set with the corresponding
    /// minutes.
    fn expand_by_minute(&self, set: &mut RecurrenceSet) {
        if !self.has_by_minute() {
            return;
        }
        set.expand(|dt| self.iter_by_minute(dt))
    }

    /// When the rule has a non-empty number of BYSECOND values, then this
    /// expands every datetime in the given set with the corresponding
    /// seconds.
    fn expand_by_second(&self, set: &mut RecurrenceSet) {
        if !self.has_by_second() {
            return;
        }
        set.expand(|dt| self.iter_by_second(dt))
    }

    /// Returns an iterator over the BYMONTH values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// months. The other parts of the datetime are copied from `dt`.
    ///
    /// If there are no month values, then this iterator does not yield
    /// any items.
    fn iter_by_month(&self, dt: DateTime) -> impl Iterator<Item = DateTime> {
        let day = self.rule().civil_start.day();
        self.rule().by_month.iter().copied().filter_map(move |month| {
            // This is subtle, but in the case where BYMONTHDAY is set, the
            // starting point is Feb 29 and all other date modifying rules are
            // empty, then the day here could have been constrained to Feb 28.
            // In which case, we really should pull the day from the actual
            // starting point.
            //
            // (I kinda wonder if we should do this more broadly, but it
            // doesn't appear necessary.)
            dt.with().month(month).day(day).build().ok()
        })
    }

    /// Returns an iterator over the BYWEEKNO values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding to a
    /// date that is the start of a week. The other parts of the datetime are
    /// copied from `dt`.
    ///
    /// If there are no week number values, then this iterator does not yield
    /// any items.
    fn iter_by_week(&self, dt: DateTime) -> impl Iterator<Item = DateTime> {
        let weeks_in_year =
            WeekDate::from_date(self.rule().week_start, dt.date())
                .map(|wd| wd.weeks_in_year())
                .ok();
        self.rule().by_week.iter().copied().filter_map(move |mut week| {
            if week.is_negative() {
                // Add 1 because -1 is the last week of the year, and the weeks
                // of the year are 1-indexed.
                week = weeks_in_year?.checked_add(week + 1)?;
            }
            let start = WeekDate::new(
                self.rule().week_start,
                dt.year(),
                week,
                self.rule().week_start,
            )
            .ok()?;
            dt.with().date(start.date()).build().ok()
        })
    }

    /// Returns an iterator over the BYYEARDAY values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// days of the year. The other parts of the datetime are copied from `dt`.
    ///
    /// This handles any negative day of the year values according to the
    /// number of days of the year in `dt`.
    ///
    /// If there are no day of the year values, then this iterator does not
    /// yield any items.
    fn iter_by_year_day(
        &self,
        dt: DateTime,
    ) -> impl Iterator<Item = DateTime> {
        let days_in_year = dt.days_in_year();
        self.rule().by_year_day.iter().copied().filter_map(move |mut day| {
            if day.is_negative() {
                // Add 1 because -1 is the last day of the year, and the days
                // of the year are 1-indexed.
                day = days_in_year.checked_add(day + 1)?;
            }
            dt.with().day_of_year(day).build().ok()
        })
    }

    /// Returns an iterator over the BYMONTHDAY values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// month days. The other parts of the datetime are copied from `dt`.
    ///
    /// This handles any negative month day values according to the number of
    /// days of the month in `dt`.
    ///
    /// If there are no month day values, then this iterator does not yield
    /// any items.
    fn iter_by_month_day(
        &self,
        dt: DateTime,
    ) -> impl Iterator<Item = DateTime> {
        let days_in_month = dt.days_in_month();
        self.rule().by_month_day.iter().copied().filter_map(move |mut day| {
            if day.is_negative() {
                // Add 1 because -1 is the last day of month, and the days of
                // the month are 1-indexed.
                day = days_in_month.checked_add(day + 1)?;
            }
            dt.with().day(day).build().ok()
        })
    }

    /// Returns an iterator over the BYDAY values in this recurrence rule at
    /// a YEARLY frequency.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// weekdays. The other parts of the datetime are copied from `dt`.
    ///
    /// This handles any negative numbered weekday values. Negative values are
    /// interpreted with respect to the end of the year. Where as positive
    /// values are interpreted with respect to the beginning of the year.
    /// (Where the year is taken from `dt`.)
    ///
    /// If there are no week day values, then this iterator does not yield
    /// any items.
    fn iter_by_week_day_yearly(
        &self,
        dt: DateTime,
    ) -> impl Iterator<Item = DateTime> {
        let start = dt.first_of_year();
        let end = dt.last_of_year();
        self.rule()
            .by_week_day
            .iter()
            .copied()
            .flat_map(move |weekday| weekday.iter_yearly(start, end))
    }

    /// Returns an iterator over the BYDAY values in this recurrence rule at
    /// a MONTHLY frequency.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// weekdays. The other parts of the datetime are copied from `dt`.
    ///
    /// This handles any negative numbered weekday values. Negative values are
    /// interpreted with respect to the end of the month. Where as positive
    /// values are interpreted with respect to the beginning of the month.
    /// (Where the month is taken from `dt`.)
    ///
    /// If there are no week day values, then this iterator does not yield
    /// any items.
    fn iter_by_week_day_monthly(
        &self,
        dt: DateTime,
    ) -> impl Iterator<Item = DateTime> {
        let start = dt.first_of_month();
        let end = dt.last_of_month();
        self.rule()
            .by_week_day
            .iter()
            .copied()
            .flat_map(move |weekday| weekday.iter_monthly(start, end))
    }

    /// Returns an iterator over the BYDAY values in this recurrence rule at
    /// a WEEKLY frequency.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// weekdays. The other parts of the datetime are copied from `dt`.
    ///
    /// If there are no week day values, then this iterator does not yield
    /// any items.
    ///
    /// # Panics
    ///
    /// When any `ByWeekday` is `Numbered`. RFC 5545 doesn't permit that
    /// construction at anything other than YEARLY and MONTHLY frequency.
    fn iter_by_week_day_weekly(
        &self,
        dt: DateTime,
    ) -> impl Iterator<Item = DateTime> {
        // I tried writing this as one iterator chain, but it was a complex
        // mess. Since I already defined `Either` for other uses, I felt it
        // was simpler to just use it here.
        let Ok(start) = first_of_week(self.rule().week_start, dt.date())
        else {
            return Either::Left(std::iter::empty());
        };
        let Ok(end) = last_of_week(self.rule().week_start, dt.date()) else {
            return Either::Left(std::iter::empty());
        };
        let Ok(start) = dt.with().date(start).build() else {
            return Either::Left(std::iter::empty());
        };
        let Ok(end) = dt.with().date(end).build() else {
            return Either::Left(std::iter::empty());
        };

        Either::Right(
            self.rule()
                .by_week_day
                .iter()
                .copied()
                .flat_map(move |weekday| weekday.iter_weekly(start, end)),
        )
    }

    /// Returns an iterator over the BYHOUR values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// hours. The other parts of the datetime are copied from `dt`.
    ///
    /// If there are no hour values, then this iterator does not yield any
    /// items.
    fn iter_by_hour(&self, dt: DateTime) -> impl Iterator<Item = DateTime> {
        self.rule()
            .by_hour
            .iter()
            .copied()
            .filter_map(move |hour| dt.with().hour(hour).build().ok())
    }

    /// Returns an iterator over the BYMINUTE values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// minutes. The other parts of the datetime are copied from `dt`.
    ///
    /// If there are no minute values, then this iterator does not yield any
    /// items.
    fn iter_by_minute(&self, dt: DateTime) -> impl Iterator<Item = DateTime> {
        self.rule()
            .by_minute
            .iter()
            .copied()
            .filter_map(move |minute| dt.with().minute(minute).build().ok())
    }

    /// Returns an iterator over the BYSECOND values in this recurrence rule.
    ///
    /// The values returned are datetimes with each of the corresponding
    /// seconds. The other parts of the datetime are copied from `dt`.
    ///
    /// If there are no second values, then this iterator does not yield any
    /// items.
    fn iter_by_second(&self, dt: DateTime) -> impl Iterator<Item = DateTime> {
        self.rule()
            .by_second
            .iter()
            .copied()
            .filter_map(move |second| dt.with().second(second).build().ok())
    }

    /// Returns the frequency at which we are generating datetimes.
    ///
    /// The frequency determines how we interpret the `by_*` data in our rule
    /// to generate datetimes.
    fn frequency(&self) -> Frequency {
        self.rule().freq
    }

    /// Returns the "inner" rule associated with this expander.
    ///
    /// This is somewhat of a misnomer, since `self.rule` is the actual
    /// rule, but this returns the "inner" rule. But the "inner" rule is
    /// where all the goods are.
    fn rule(&self) -> &RecurrenceRuleInner {
        &self.rule.inner
    }
}

#[derive(Clone, Debug)]
pub struct RecurrenceIter<'r> {
    /// The recurrence rule that we're generating zoned datetimes for.
    rule: &'r RecurrenceRule,
    /// The set of datetimes that we should drain and emit before
    /// incrementing by our interval and refilling the set.
    set: RecurrenceSet,
    /// The interval index along with the current datetime.
    ///
    /// In order to get the next datetime, the interval index should be
    /// incremented by one and then multiplied by the frequency `Span`. This
    /// is done instead of just adding to the previous datetime to avoid
    /// cases where we go from 2025-03-31 -> 2025-04-30 -> 2025-05-30 instead
    /// of 2025-03-31 -> 2025-04-30 -> 2025-05-31.
    ///
    /// When this is `None`, iteration has ceased.
    cur: Option<(i64, DateTime)>,
}

impl<'r> RecurrenceIter<'r> {
    fn expand(&mut self) {
        let Some((_, cur)) = self.cur else { return };
        Expander { rule: self.rule, cur }.expand(&mut self.set);
    }

    fn is_done(&self) -> bool {
        self.cur.is_none() && self.set.is_empty()
    }

    fn increment(&self) -> Option<(i64, DateTime)> {
        let (mut attempt, orig) = self.cur?;
        let interval = self.rule.inner.interval;
        loop {
            attempt = attempt.checked_add(1)?;
            let interval = interval.checked_mul(attempt).ok()?;
            let next =
                self.rule.inner.civil_start.checked_add(interval).ok()?;
            // Subtle, but this is not technically required for correctness,
            // since we filter based on `until` when we pop from the
            // recurrence set. The reason we do this is for cases where the
            // recurrence set is always empty (or nearly always). Namely, this
            // lets us "bound" our work even if we aren't visibly producing
            // anything.
            //
            // It is somewhat unfortunate that in order to do this, we need
            // to convert our next datetime to a physical instant. Perhaps
            // we can avoid doing this when the previous size of the recurrence
            // set is non-empty. Why? Because if the recurrence set is getting
            // datetimes in it, then filtering can be applied when we pop from
            // it and this optimization ends up not being necessary.
            //
            // I think one tricky thing here is that just because a recurrence
            // set is empty after expansion for a single interval, that doesn't
            // mean iteration can stop. Because a subsequent interval might
            // generate a non-empty recurrence set.
            if let Some(ref until) = self.rule.inner.zoned_until {
                let zoned_next = self
                    .rule
                    .time_zone()
                    .to_ambiguous_zoned_datetime(next)
                    .later()
                    .ok()?;
                if &zoned_next > until {
                    return None;
                }
            }
            // In cases where we add N years or N months, the day number can
            // change. For example, in Jiff, 2024-02-29 + 1 year = 2025-02-28.
            // But RFC 5545 wants us to treat it like 2025-02-29, which
            // is invalid and thus ignore such things. So we assume the
            // "constraining" behavior of Jiff (as Temporal calls it) occurs
            // when the result's day number is not equal to the one we started
            // with.
            //
            // When constraining behavior happens, we just continue on and try
            // the next point by adding our interval again. We do this *not*
            // by just continually adding our interval to an ever increasing
            // datetime, which is the natural thing to do. The problem is that
            // constraining behavior is infectious. Once you get, for example,
            // 2025-02-28, adding 3 years will give you 2028-02-28 and so on.
            // You'll never get back to a leap day. So instead, we track our
            // number of attempts and multiply our `Span` as we go.
            //
            // This doesn't account for the other way to get invalid datetimes,
            // which is to end up at a time that is invalid for the given time
            // zone. We handle that when we pop datetimes from the set.
            //
            // This is pretty tricky, but there is an exception to skipping
            // invalid datetimes here. When expansion rules are set that choose
            // the date (instead of using it from the start date), then we DO
            // need to consider the interval instead of skipping over it. This
            // works because if a date-setting expansion rule is present, then
            // the constrained datetime should not remain as part of the set,
            // unless the rule would have otherwise selected it.
            let r = &self.rule.inner;
            match r.freq {
                Frequency::Yearly if next.day() != orig.day() => {
                    if r.by_month.is_empty()
                        && r.by_week.is_empty()
                        && r.by_year_day.is_empty()
                        && r.by_month_day.is_empty()
                        && r.by_week_day.is_empty()
                    {
                        continue;
                    }
                }
                Frequency::Monthly if next.day() != orig.day() => {
                    if r.by_week_day.is_empty() && r.by_month_day.is_empty() {
                        continue;
                    }
                }
                _ => {}
            }
            return Some((attempt, next));
        }
    }
}

impl<'r> Iterator for RecurrenceIter<'r> {
    type Item = ZonedDateTime;

    fn next(&mut self) -> Option<ZonedDateTime> {
        while !self.is_done() {
            while let Some(zdt) = self.set.pop(&self.rule) {
                return Some(zdt);
            }
            self.expand();
            self.cur = self.increment();
        }
        None
    }
}

impl<'r> std::iter::FusedIterator for RecurrenceIter<'r> {}

/// A builder for constructing a valid recurrence rule.
#[derive(Clone, Debug)]
pub struct RecurrenceRuleBuilder {
    freq: Frequency,
    start: ZonedDateTime,
    until: Option<ZonedDateTime>,
    interval: i32,
    by_month: Vec<i8>,
    by_week: Vec<i8>,
    by_year_day: Vec<i16>,
    by_month_day: Vec<i8>,
    by_week_day: Vec<ByWeekday>,
    by_hour: Vec<i8>,
    by_minute: Vec<i8>,
    by_second: Vec<i8>,
    by_set_pos: Vec<i32>,
    week_start: Weekday,
}

impl RecurrenceRuleBuilder {
    fn new(freq: Frequency, start: ZonedDateTime) -> RecurrenceRuleBuilder {
        RecurrenceRuleBuilder {
            freq,
            start,
            until: None,
            interval: 1,
            by_month: vec![],
            by_week: vec![],
            by_year_day: vec![],
            by_month_day: vec![],
            by_week_day: vec![],
            by_hour: vec![],
            by_minute: vec![],
            by_second: vec![],
            by_set_pos: vec![],
            week_start: Weekday::Monday,
        }
    }

    pub fn build(&self) -> anyhow::Result<RecurrenceRule> {
        fn sort_and_dedup<T: Clone + Ord>(slice: &[T]) -> Box<[T]> {
            let mut vec = slice.to_vec();
            vec.sort();
            vec.dedup();
            vec.into_boxed_slice()
        }

        anyhow::ensure!(
            self.interval >= 1,
            "interval value of `{}` is invalid \
             (interval must be greater than or equal to 1)",
            self.interval,
        );
        for &v in self.by_month.iter() {
            anyhow::ensure!(
                1 <= v && v <= 12,
                "invalid 'by month' value `{v}` \
                 (values must be in range 1..=12)",
            );
        }
        for &v in self.by_week.iter() {
            anyhow::ensure!(
                (-53 <= v && v <= -1) || (1 <= v && v <= 53),
                "invalid 'by week' value `{v}` \
                 (values must be in range 1..=53 or -53..=-1)",
            );
        }
        for &v in self.by_year_day.iter() {
            anyhow::ensure!(
                (-366 <= v && v <= -1) || (1 <= v && v <= 366),
                "invalid 'by day of the year' value `{v}` \
                 (values must be in range 1..=366 or -366..=-1)",
            );
        }
        for &v in self.by_month_day.iter() {
            anyhow::ensure!(
                (-31 <= v && v <= -1) || (1 <= v && v <= 31),
                "invalid 'by day of the month' value `{v}` \
                 (values must be in range 1..=31 or -31..=-1)",
            );
        }
        for &v in self.by_week_day.iter() {
            let nth = match v {
                ByWeekday::Any(_) => continue,
                ByWeekday::Numbered { nth, .. } => nth,
            };
            // Firstly, numbered weekdays are only allowed for YEARLY or
            // MONTHLY frequencies.
            anyhow::ensure!(
                matches!(self.freq, Frequency::Yearly | Frequency::Monthly),
                "numbered weekday `{v}` is only allowed at \
                 yearly or monthly frequencies",
            );
            // ... except when using YEARLY frequency with BYWEEKNO set.
            anyhow::ensure!(
                !matches!(self.freq, Frequency::Yearly)
                    || self.by_week.is_empty(),
                "numbered weekday `{v}` is only allowed at yearly \
                 frequency when 'by week' is not used",
            );
            // Secondly, check the bounds on `nth`. It's yearly when the
            // frequency is yearly and BYMONTH isn't set. Otherwise, it's
            // monthly.
            if matches!(self.freq, Frequency::Yearly)
                && self.by_month.is_empty()
            {
                anyhow::ensure!(
                    (-53 <= nth && nth <= -1) || (1 <= nth && nth <= 53),
                    "invalid numbered 'by week day' value `{v}` \
                     (values must be in range 1..=53 or -53..=-1)",
                );
            } else {
                anyhow::ensure!(
                    (-5 <= nth && nth <= -1) || (1 <= nth && nth <= 5),
                    "invalid numbered 'by week day' value `{v}` \
                     (values must be in range 1..=5 or -5..=-1)",
                );
            }
        }
        for &v in self.by_hour.iter() {
            anyhow::ensure!(
                0 <= v && v <= 23,
                "invalid 'by hour' value `{v}` \
                 (values must be in range 0..=23)",
            );
        }
        for &v in self.by_minute.iter() {
            anyhow::ensure!(
                0 <= v && v <= 59,
                "invalid 'by minute' value `{v}` \
                 (values must be in range 0..=59)",
            );
        }
        for &v in self.by_second.iter() {
            // RFC 5545 technically allows a value of `60`
            // here, presumably for leap seconds. Jiff doesn't
            // support leap seconds outside of parsing, in
            // which case, Jiff just clamps the value. Clamping
            // doesn't really make sense here, so just reject
            // it. This is also what `python-dateutil` does.
            anyhow::ensure!(
                0 <= v && v <= 59,
                "invalid 'by second' value `{v}` \
                 (values must be in range 0..=59)",
            );
        }
        for &v in self.by_set_pos.iter() {
            anyhow::ensure!(
                (-366 <= v && v <= -1) || (1 <= v && v <= 366),
                "invalid 'by set position' value `{v}` \
                 (values must be in range 1..=366 or -366..=-1)",
            );
        }

        // Some additional frequency-specific errors.
        if !self.by_week.is_empty() {
            anyhow::ensure!(
                matches!(self.freq, Frequency::Yearly),
                "'by week' cannot be used with any frequency except yearly",
            );
        }
        if !self.by_year_day.is_empty() {
            anyhow::ensure!(
                !matches!(
                    self.freq,
                    Frequency::Monthly | Frequency::Weekly | Frequency::Daily
                ),
                "'by day of the year' cannot be used \
                 with monthly, weekly or daily frequency",
            );
        }
        if !self.by_month_day.is_empty() {
            anyhow::ensure!(
                !matches!(self.freq, Frequency::Weekly),
                "'by day of the month' cannot be used with weekly frequency",
            );
        }

        // A BYSETPOS specific error is that, if it's given, then there MUST
        // be another BY* rule.
        if !self.by_set_pos.is_empty() {
            anyhow::ensure!(
                !self.by_month.is_empty()
                    || !self.by_week.is_empty()
                    || !self.by_year_day.is_empty()
                    || !self.by_month_day.is_empty()
                    || !self.by_week_day.is_empty()
                    || !self.by_hour.is_empty()
                    || !self.by_minute.is_empty()
                    || !self.by_second.is_empty(),
                "when 'by set position' is used, at least one other \
                 'by' rule must be specified, but all are empty",
            );
        }

        let interval =
            self.freq.to_span(self.interval).with_context(|| {
                format!(
                    "could not convert {freq} interval of `{interval}` to \
                     time span",
                    freq = self.freq.as_str(),
                    interval = self.interval,
                )
            })?;
        let inner = Arc::new(RecurrenceRuleInner {
            freq: self.freq,
            zoned_start: self.start.clone(),
            civil_start: self.start.datetime(),
            zoned_until: self.until.clone(),
            interval,
            by_month: sort_and_dedup(&self.by_month),
            by_week: sort_and_dedup(&self.by_week),
            by_year_day: sort_and_dedup(&self.by_year_day),
            by_month_day: sort_and_dedup(&self.by_month_day),
            by_week_day: sort_and_dedup(&self.by_week_day),
            by_hour: sort_and_dedup(&self.by_hour),
            by_minute: sort_and_dedup(&self.by_minute),
            by_second: sort_and_dedup(&self.by_second),
            by_set_pos: sort_and_dedup(&self.by_set_pos),
            week_start: self.week_start,
        });
        Ok(RecurrenceRule { inner })
    }

    pub fn until(
        &mut self,
        until: ZonedDateTime,
    ) -> &mut RecurrenceRuleBuilder {
        self.until = Some(until);
        self
    }

    pub fn interval(&mut self, increment: i32) -> &mut RecurrenceRuleBuilder {
        self.interval = increment;
        self
    }

    pub fn by_month<I: IntoI8Iter>(
        &mut self,
        months: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_month.extend(months.into_i8_iter());
        self
    }

    pub fn by_week<I: IntoI8Iter>(
        &mut self,
        weeks: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_week.extend(weeks.into_i8_iter());
        self
    }

    pub fn by_year_day<I: IntoI16Iter>(
        &mut self,
        days: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_year_day.extend(days.into_i16_iter());
        self
    }

    pub fn by_month_day<I: IntoI8Iter>(
        &mut self,
        days: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_month_day.extend(days.into_i8_iter());
        self
    }

    pub fn by_week_day<I: IntoByWeekdayIter>(
        &mut self,
        week_days: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_week_day.extend(week_days.into_by_weekday_iter());
        self
    }

    pub fn by_hour<I: IntoI8Iter>(
        &mut self,
        hours: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_hour.extend(hours.into_i8_iter());
        self
    }

    pub fn by_minute<I: IntoI8Iter>(
        &mut self,
        minutes: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_minute.extend(minutes.into_i8_iter());
        self
    }

    pub fn by_second<I: IntoI8Iter>(
        &mut self,
        seconds: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_second.extend(seconds.into_i8_iter());
        self
    }

    pub fn by_set_position<I: IntoI32Iter>(
        &mut self,
        positions: I,
    ) -> &mut RecurrenceRuleBuilder {
        self.by_set_pos.extend(positions.into_i32_iter());
        self
    }

    pub fn week_start(
        &mut self,
        weekday: Weekday,
    ) -> &mut RecurrenceRuleBuilder {
        self.week_start = weekday;
        self
    }
}

#[derive(Clone, Debug)]
struct RecurrenceSet {
    civil: Vec<DateTime>,
    /// A set of *zoned* datetimes that we use exactly like `set`, but only
    /// when BYSETPOS has a non-zero number of values.
    ///
    /// Basically, when BYSETPOS is used, `set` is drained into `zoned_set`,
    /// `zoned_set` is then filtered according to BYSETPOS and the iterator
    /// pops from `zoned_set` instead of `set`.
    ///
    /// Otherwise, this is totally ignored and no allocation is done.
    ///
    /// Generally speaking, this isn't really managed by the methods on
    /// `RecurrenceSet`. The abstraction somewhat sucks. If this needed to be
    /// a public API, we'd probably use more type machinery. But in practice,
    /// this is mostly treated as scratch space by `Expander`.
    zoned: Vec<ZonedDateTime>,
    /// A queued zoned datetime that should be emitted next. This can get used
    /// when, for example, a generated datetime falls into a fold. In that
    /// case, the datetime gets duplicated for each offset in the fold. The
    /// chronologically first one is returned and the one after it is pushed
    /// into this placeholder position to be emitted next.
    tmp: Option<ZonedDateTime>,
}

impl RecurrenceSet {
    fn new() -> RecurrenceSet {
        RecurrenceSet { civil: vec![], zoned: vec![], tmp: None }
    }

    fn is_empty(&self) -> bool {
        self.civil.is_empty() && self.zoned.is_empty() && self.tmp.is_none()
    }

    fn insert(&mut self, dt: DateTime) {
        self.civil.push(dt);
    }

    fn clear(&mut self) {
        self.civil.clear();
        self.zoned.clear();
        self.tmp = None;
    }

    fn retain(&mut self, predicate: impl FnMut(&mut DateTime) -> bool) {
        self.civil.retain_mut(predicate);
    }

    fn expand<E, I>(&mut self, expand: E)
    where
        E: Fn(DateTime) -> I,
        I: Iterator<Item = DateTime>,
    {
        // We're going to replace every datetime in the set at this point
        // with its expansion provided by the closure. So record how many
        // datetimes we have now. At the end, we'll drain them in one swoop.
        let len = self.civil.len();
        for i in 0..len {
            self.civil.extend(expand(self.civil[i]));
        }
        self.civil.drain(..len);
    }

    fn canonicalize(&mut self) {
        self.civil.sort_by(|dt1, dt2| dt1.cmp(dt2).reverse());
        self.civil.dedup();
    }

    fn pop(&mut self, rule: &RecurrenceRule) -> Option<ZonedDateTime> {
        loop {
            let next = self.zoned.pop().or_else(|| self.pop_civil(rule))?;
            // For simplicity of implementation, the recurrence rule
            // generator may create datetimes before our starting point. Since
            // this can generally only happen on the first "iteration," we
            // don't worry about it too much and just filter them here.
            //
            // Ideally we would do this even earlier, and that might make the
            // BYSETPOS implementation simpler. But the starting point is a
            // zoned datetime and this is the earliest that we convert a
            // datetime from our buffered set to a zoned datetime.
            if next < rule.inner.zoned_start {
                continue;
            }
            // For similar reasons, we handle our "until" constraint here too.
            if let Some(ref until) = rule.inner.zoned_until
                && &next > until
            {
                // Since we always move forward in time, if we reach
                // this point, we'll never be able to return any other
                // datetime. So we can drain the rest of the set and
                // bail.
                self.clear();
                return None;
            }
            return Some(next);
        }
    }

    fn pop_civil(&mut self, rule: &RecurrenceRule) -> Option<ZonedDateTime> {
        let tz = rule.time_zone();
        loop {
            if let Some(zdt) = self.tmp.take() {
                return Some(zdt);
            }
            let dt = self.civil.pop()?;
            let azdt = tz.to_ambiguous_zoned_datetime(dt);
            let next = match azdt.offset() {
                AmbiguousOffset::Unambiguous { offset } => {
                    let Ok(ts) = offset.to_timestamp(dt) else { continue };
                    ts.to_zoned_datetime(tz.clone())
                }
                // RFC 5545 explicitly says that if a time is invalid for a
                // particular time zone, then it should be skipped.
                AmbiguousOffset::Gap { .. } => continue,
                // RFC 5545 doesn't say what should happen in this case. It's
                // plausible this should be configurable. But since the time
                // gets repeated twice, we emit two events. It kinda seems like
                // we should only emit one (emitting two could be inconsistent
                // with the requested frequency), but how does one choose which
                // one to emit? It seems though that this is the conservative
                // option, since it emits "too much" data instead of too
                // little. With all that said, my guess is that whatever choice
                // we make, it's unlikely to matter much in practice.
                AmbiguousOffset::Fold { before, after } => {
                    if let Ok(ts) = after.to_timestamp(dt) {
                        self.tmp = Some(ts.to_zoned_datetime(tz.clone()));
                    }
                    // If this fails but the above succeeds, then we'll loop
                    // back around and pop from `self.tmp`.
                    if let Ok(ts) = before.to_timestamp(dt) {
                        ts.to_zoned_datetime(tz.clone())
                    } else {
                        continue;
                    }
                }
            };
            // N.B. We don't check if the civil datetime is before our starting
            // point or after our `until` here because BYSETPOS wants those
            // included in the recurrence set when resolving indices. They
            // will get filtered out later in `pop()`.
            return Some(next);
        }
    }
}

/// A trait that permits flexibly specifying a sequence of `i8` integers.
///
/// This trait is used for builder methods on `RecurrenceRuleBuilder`. It
/// permits callers to provide integers in a number of flexible ways:
///
/// * A single integer: `5`
/// * An array of integers: `[1, 3, 5]`.
/// * A single range of integers: `5..8` or `5..=8`.
/// * An array of ranges of integers: `[5..=10, 15..=20]`.
///
/// # Design
///
/// The reason this trait _and_ `IntoI16Iter` exists is to make specifying a
/// sequence more ergonomic. In particular, an alternative design is:
///
/// ```ignore
/// pub trait IntoIntegerIter {
///     type Integer;
///     fn into_integer_iter(self) -> impl Iterator<Item = Self::Integer>;
/// }
/// ```
///
/// But since this would be implemented for both `i8` and `i16`, this
/// means that `builder.by_month(5)` cannot have the type of `5` inferred
/// unambiguously.
// N.B. If we end up copying this design into Jiff, we should make this
// trait sealed so that people downstream can't add trait impls and fuck up
// inference.
pub trait IntoI8Iter {
    /// Creates an iterator over all integers in this sequence.
    fn into_i8_iter(self) -> impl Iterator<Item = i8>;
}

/// A trait that permits flexibly specifying a sequence of `i16` integers.
///
/// This trait is used for builder methods on `RecurrenceRuleBuilder`. It
/// permits callers to provide integers in a number of flexible ways:
///
/// * A single integer: `5`
/// * An array of integers: `[1, 3, 5]`.
/// * A single range of integers: `5..8` or `5..=8`.
/// * An array of ranges of integers: `[5..=10, 15..=20]`.
///
/// # Design
///
/// The reason this trait _and_ `IntoI8Iter` exists is to make specifying a
/// sequence more ergonomic. In particular, an alternative design is:
///
/// ```ignore
/// pub trait IntoIntegerIter {
///     type Integer;
///     fn into_integer_iter(self) -> impl Iterator<Item = Self::Integer>;
/// }
/// ```
///
/// But since this would be implemented for both `i8` and `i16`, this
/// means that `builder.by_month(5)` cannot have the type of `5` inferred
/// unambiguously.
pub trait IntoI16Iter {
    /// Creates an iterator over all integers in this sequence.
    fn into_i16_iter(self) -> impl Iterator<Item = i16>;
}

/// A trait that permits flexibly specifying a sequence of `i32` integers.
///
/// This trait is used for builder methods on `RecurrenceRuleBuilder`. It
/// permits callers to provide integers in a number of flexible ways:
///
/// * A single integer: `5`
/// * An array of integers: `[1, 3, 5]`.
/// * A single range of integers: `5..8` or `5..=8`.
/// * An array of ranges of integers: `[5..=10, 15..=20]`.
///
/// # Design
///
/// The reason this trait, `IntoI8Iter` _and_ `IntoI16Iter` exists is to make
/// specifying a sequence more ergonomic. In particular, an alternative design
/// is:
///
/// ```ignore
/// pub trait IntoIntegerIter {
///     type Integer;
///     fn into_integer_iter(self) -> impl Iterator<Item = Self::Integer>;
/// }
/// ```
///
/// But since this would be implemented for `i8`, `i16` and `i32`, this
/// means that `builder.by_month(5)` cannot have the type of `5` inferred
/// unambiguously.
pub trait IntoI32Iter {
    /// Creates an iterator over all integers in this sequence.
    fn into_i32_iter(self) -> impl Iterator<Item = i32>;
}

impl IntoI8Iter for i8 {
    fn into_i8_iter(self) -> impl Iterator<Item = i8> {
        std::iter::once(self)
    }
}

impl IntoI16Iter for i16 {
    fn into_i16_iter(self) -> impl Iterator<Item = i16> {
        std::iter::once(self)
    }
}

impl IntoI32Iter for i32 {
    fn into_i32_iter(self) -> impl Iterator<Item = i32> {
        std::iter::once(self)
    }
}

impl IntoI8Iter for Range<i8> {
    fn into_i8_iter(self) -> impl Iterator<Item = i8> {
        self.into_iter()
    }
}

impl IntoI16Iter for Range<i16> {
    fn into_i16_iter(self) -> impl Iterator<Item = i16> {
        self.into_iter()
    }
}

impl IntoI32Iter for Range<i32> {
    fn into_i32_iter(self) -> impl Iterator<Item = i32> {
        self.into_iter()
    }
}

impl IntoI8Iter for RangeInclusive<i8> {
    fn into_i8_iter(self) -> impl Iterator<Item = i8> {
        self.into_iter()
    }
}

impl IntoI16Iter for RangeInclusive<i16> {
    fn into_i16_iter(self) -> impl Iterator<Item = i16> {
        self.into_iter()
    }
}

impl IntoI32Iter for RangeInclusive<i32> {
    fn into_i32_iter(self) -> impl Iterator<Item = i32> {
        self.into_iter()
    }
}

impl<const N: usize> IntoI8Iter for [i8; N] {
    fn into_i8_iter(self) -> impl Iterator<Item = i8> {
        self.into_iter()
    }
}

impl<const N: usize> IntoI16Iter for [i16; N] {
    fn into_i16_iter(self) -> impl Iterator<Item = i16> {
        self.into_iter()
    }
}

impl<const N: usize> IntoI32Iter for [i32; N] {
    fn into_i32_iter(self) -> impl Iterator<Item = i32> {
        self.into_iter()
    }
}

impl<const N: usize> IntoI8Iter for [Range<i8>; N] {
    fn into_i8_iter(self) -> impl Iterator<Item = i8> {
        self.into_iter().flatten()
    }
}

impl<const N: usize> IntoI16Iter for [Range<i16>; N] {
    fn into_i16_iter(self) -> impl Iterator<Item = i16> {
        self.into_iter().flatten()
    }
}

impl<const N: usize> IntoI32Iter for [Range<i32>; N] {
    fn into_i32_iter(self) -> impl Iterator<Item = i32> {
        self.into_iter().flatten()
    }
}

impl<const N: usize> IntoI8Iter for [RangeInclusive<i8>; N] {
    fn into_i8_iter(self) -> impl Iterator<Item = i8> {
        self.into_iter().flatten()
    }
}

impl<const N: usize> IntoI16Iter for [RangeInclusive<i16>; N] {
    fn into_i16_iter(self) -> impl Iterator<Item = i16> {
        self.into_iter().flatten()
    }
}

impl<const N: usize> IntoI32Iter for [RangeInclusive<i32>; N] {
    fn into_i32_iter(self) -> impl Iterator<Item = i32> {
        self.into_iter().flatten()
    }
}

/// A trait that permits flexibly specifying a sequence of weekdays.
///
/// Each weekday can just mean "any" weekday (e.g., `Weekday::Saturday`), or
/// it can mean a numbered weekday. For example, when the frequency for a
/// recurrence rule is yearly, then `(3, Weekday::Saturday)` corresponds to the
/// third Saturday of the year.
///
/// This trait is primarily used for the `RecurrenceRuleBuilder::by_week_day`
/// builder methods. It permits callers to provide weekdays in a number of
/// flexible ways:
///
/// * Directly via `ByWeekday::Numbered { nth: 3, weekday: Weekday::Monday }`.
/// * As just any weekday via `Weekday::Monday`.
/// * As a range of weekdays via `Weekday::Monday..=Weekday::Wednesday`.
/// * As an array of weekdays via `[Weekday::Monday, Weekday::Friday]`.
/// * As an array of numbered weekdays via
/// `[(2, Weekday::Monday), (1, Weekday::Friday)]`.
pub trait IntoByWeekdayIter {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday>;
}

impl IntoByWeekdayIter for ByWeekday {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        std::iter::once(self)
    }
}

impl IntoByWeekdayIter for Weekday {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        std::iter::once(ByWeekday::Any(self))
    }
}

impl IntoByWeekdayIter for (i8, Weekday) {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        let (nth, weekday) = self;
        std::iter::once(ByWeekday::Numbered { nth, weekday })
    }
}

impl IntoByWeekdayIter for RangeInclusive<Weekday> {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        let (start, end) = (*self.start(), *self.end());
        // OK because `Weekday::until` guarantees `0..=6`.
        // And add `1` because this is an inclusive range.
        let count = 1 + usize::try_from(start.until(end)).unwrap();
        start.cycle_forward().take(count).map(ByWeekday::Any)
    }
}

impl<const N: usize> IntoByWeekdayIter for [ByWeekday; N] {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        self.into_iter()
    }
}

impl<const N: usize> IntoByWeekdayIter for [Weekday; N] {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        self.into_iter().flat_map(|any| any.into_by_weekday_iter())
    }
}

impl<const N: usize> IntoByWeekdayIter for [(i8, Weekday); N] {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        self.into_iter().flat_map(|numbered| numbered.into_by_weekday_iter())
    }
}

impl<const N: usize> IntoByWeekdayIter for [RangeInclusive<Weekday>; N] {
    fn into_by_weekday_iter(self) -> impl Iterator<Item = ByWeekday> {
        self.into_iter().flat_map(|any| any.into_by_weekday_iter())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Frequency {
    Yearly,
    Monthly,
    Weekly,
    Daily,
    Hourly,
    Minutely,
    Secondly,
}

impl Frequency {
    pub const USAGE: Usage = Usage::arg(
        "<frequency>",
        "The frequency at which to repeat a sequence.",
        r#"
The frequency at which to repeat a sequence.

Valid values are:

yearly, year, yr, y

monthly, month, mo

weekly, week, wk, w

daily, day, d

hourly, hour, hr, h

minutely, minute, min, m

secondly, second, sec, s
"#,
    );

    fn to_span(&self, interval: i32) -> anyhow::Result<Span> {
        let base = match *self {
            Frequency::Yearly => 1.year(),
            Frequency::Monthly => 1.month(),
            Frequency::Weekly => 1.week(),
            Frequency::Daily => 1.day(),
            Frequency::Hourly => 1.hour(),
            Frequency::Minutely => 1.minute(),
            Frequency::Secondly => 1.second(),
        };
        Ok(base.checked_mul(i64::try_from(interval)?)?)
    }

    fn as_str(&self) -> &'static str {
        match *self {
            Frequency::Yearly => "yearly",
            Frequency::Monthly => "monthly",
            Frequency::Weekly => "weekly",
            Frequency::Daily => "daily",
            Frequency::Hourly => "hourly",
            Frequency::Minutely => "minutely",
            Frequency::Secondly => "secondly",
        }
    }
}

impl std::str::FromStr for Frequency {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Frequency> {
        use self::Frequency::*;

        // This is what's recognized by the friendly duration format.
        let unit = match &*s.to_lowercase() {
            "yearly" | "year" | "yr" | "y" => Yearly,
            "monthly" | "month" | "mo" => Monthly,
            "weekly" | "week" | "wk" | "w" => Weekly,
            "daily" | "day" | "d" => Daily,
            "hourly" | "hour" | "hr" | "h" => Hourly,
            "minutely" | "minute" | "min" | "m" => Minutely,
            "secondly" | "second" | "sec" | "s" => Secondly,
            unk => anyhow::bail!("unrecognized frequency: `{unk}`"),
        };
        Ok(unit)
    }
}

/// A type describing "day of week" inputs.
///
/// This implements `Ord` even though the actual order of weekdays cannot be
/// determined unless the _start_ of the week is known (which is commonly
/// either or Sunday or Monday, but RFC 5545 lets any day be the start).
/// However, we implement `Ord` to make it easy to sort and de-duplicate
/// collections containing a `ByWeekday`. We never actually rely on its
/// ordering for generating datetimes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ByWeekday {
    Any(Weekday),
    Numbered { nth: i8, weekday: Weekday },
}

impl ByWeekday {
    /// Returns true if and only if the given weekday matches this one.
    ///
    /// # Panics
    ///
    /// This panics when this is a numbered weekday. In effect, this shouldn't
    /// be used in contexts where BYDAY can contain numbered weekdays. That
    /// limits this to YEARLY and MONTHLY frequencies (and in the YEARLY case,
    /// only when BYWEEKNO is not used).
    fn is_match(&self, wd: Weekday) -> bool {
        match *self {
            ByWeekday::Any(weekday) => weekday == wd,
            _ => unreachable!(),
        }
    }

    /// Return an iterator of weekdays, at yearly frequency, within the given
    /// range of datetimes.
    ///
    /// Generally speaking, `start` should be the first day of a year and `end`
    /// should be the last day of that same year.
    ///
    /// When this is a numbered weekday, then a positive number is interpreted
    /// relative to the start and a negative number is interpreted relative to
    /// the end. Either way, the iterator returned yields at most one element
    /// (but may yield zero).
    ///
    /// When this is "any" weekday, then every date with that weekday between
    /// `start` and `end` (inclusive) is returned.
    fn iter_yearly(
        &self,
        start: DateTime,
        end: DateTime,
    ) -> impl Iterator<Item = DateTime> + use<> {
        match *self {
            ByWeekday::Any(weekday) => {
                Either::Left(iter_weekdays_between(weekday, start, end))
            }
            ByWeekday::Numbered { nth, weekday } => {
                let nth = if start.weekday() != weekday {
                    nth
                } else if nth == 1 {
                    return Either::Right(Some(start).into_iter());
                } else if nth == -1 {
                    return Either::Right(Some(end).into_iter());
                } else {
                    nth - nth.signum()
                };
                let from = if nth < 0 { end } else { start };
                if let Ok(dt) = from.nth_weekday(i32::from(nth), weekday) {
                    return Either::Right(Some(dt).into_iter());
                }
                Either::Right(None.into_iter())
            }
        }
    }

    /// Return an iterator of weekdays, at monthly frequency, within the given
    /// range of datetimes.
    ///
    /// Generally speaking, `start` should be the first day of a month and
    /// `end` should be the last day of that same month.
    ///
    /// When this is a numbered weekday, then a positive number is interpreted
    /// relative to the start and a negative number is interpreted relative to
    /// the end. Either way, the iterator returned yields at most one element
    /// (but may yield zero).
    ///
    /// When this is "any" weekday, then every date with that weekday between
    /// `start` and `end` (inclusive) is returned.
    fn iter_monthly(
        &self,
        start: DateTime,
        end: DateTime,
    ) -> impl Iterator<Item = DateTime> + use<> {
        match *self {
            ByWeekday::Any(weekday) => {
                Either::Left(iter_weekdays_between(weekday, start, end))
            }
            ByWeekday::Numbered { nth, weekday } => Either::Right(
                start.nth_weekday_of_month(nth, weekday).ok().into_iter(),
            ),
        }
    }

    /// Return an iterator of weekdays, at weekly frequency, within the given
    /// range of datetimes.
    ///
    /// Generally speaking, `start` should be the first day of a week and
    /// `end` should be the last day of that same week.
    ///
    /// When this is "any" weekday, then every date with that weekday between
    /// `start` and `end` (inclusive) is returned.
    ///
    /// # Panics
    ///
    /// When this `ByWeekday` is `Numbered`. RFC 5545 doesn't permit that
    /// construction at anything other than YEARLY and MONTHLY frequency.
    fn iter_weekly(
        &self,
        start: DateTime,
        end: DateTime,
    ) -> impl Iterator<Item = DateTime> + use<> {
        match *self {
            ByWeekday::Any(weekday) => {
                iter_weekdays_between(weekday, start, end)
            }
            // Not allowed according to RFC 5545. This case should be prevented
            // by recurrence rule construction.
            ByWeekday::Numbered { .. } => unreachable!(),
        }
    }
}

impl Ord for ByWeekday {
    fn cmp(&self, rhs: &ByWeekday) -> Ordering {
        match (*self, *rhs) {
            (ByWeekday::Any(lhs), ByWeekday::Any(rhs)) => {
                lhs.to_monday_one_offset().cmp(&rhs.to_monday_one_offset())
            }
            (
                ByWeekday::Numbered { nth: lhs_nth, weekday: lhs_weekday },
                ByWeekday::Numbered { nth: rhs_nth, weekday: rhs_weekday },
            ) => {
                let lhs = (lhs_nth, lhs_weekday.to_monday_one_offset());
                let rhs = (rhs_nth, rhs_weekday.to_monday_one_offset());
                lhs.cmp(&rhs)
            }
            (ByWeekday::Any(_), ByWeekday::Numbered { .. }) => Ordering::Less,
            (ByWeekday::Numbered { .. }, ByWeekday::Any(_)) => {
                Ordering::Greater
            }
        }
    }
}

impl PartialOrd for ByWeekday {
    fn partial_cmp(&self, rhs: &ByWeekday) -> Option<Ordering> {
        Some(self.cmp(rhs))
    }
}

impl std::str::FromStr for ByWeekday {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<ByWeekday> {
        let Some((nth, weekday)) = s.rsplit_once("-") else {
            return Ok(ByWeekday::Any(s.parse::<flags::Weekday>()?.get()));
        };
        let nth = nth.parse().with_context(|| {
            format!("failed to parse `{nth}` as integer weekday number")
        })?;
        let weekday = weekday.parse::<flags::Weekday>()?.get();
        Ok(ByWeekday::Numbered { nth, weekday })
    }
}

impl std::fmt::Display for ByWeekday {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        fn fmt_weekday(
            wd: Weekday,
            f: &mut std::fmt::Formatter,
        ) -> std::fmt::Result {
            match wd {
                Weekday::Sunday => write!(f, "Sun"),
                Weekday::Monday => write!(f, "Mon"),
                Weekday::Tuesday => write!(f, "Tue"),
                Weekday::Wednesday => write!(f, "Wed"),
                Weekday::Thursday => write!(f, "Thu"),
                Weekday::Friday => write!(f, "Fri"),
                Weekday::Saturday => write!(f, "Sat"),
            }
        }

        match *self {
            ByWeekday::Any(weekday) => fmt_weekday(weekday, f),
            ByWeekday::Numbered { nth, weekday } => {
                write!(f, "{nth}-")?;
                fmt_weekday(weekday, f)
            }
        }
    }
}

/// A simple `Either` type for easy construction of `impl Iterator`.
///
/// Specifically, this is useful when it's supremely annoying to write a
/// single iterator chain when it would be more naturally written using case
/// analysis.
enum Either<L, R> {
    Left(L),
    Right(R),
}

impl<L, R, I> Iterator for Either<L, R>
where
    L: Iterator<Item = I>,
    R: Iterator<Item = I>,
{
    type Item = I;

    fn next(&mut self) -> Option<I> {
        match *self {
            Either::Left(ref mut it) => it.next(),
            Either::Right(ref mut it) => it.next(),
        }
    }
}

/// Returns an iterator for every weekday between `start` and `end` (inclusive).
fn iter_weekdays_between(
    weekday: Weekday,
    start: DateTime,
    end: DateTime,
) -> impl Iterator<Item = DateTime> {
    (start.weekday() == weekday).then_some(start).into_iter().chain({
        let mut cur = start.nth_weekday(1, weekday).ok();
        std::iter::from_fn(move || {
            let next = cur.take()?;
            if next > end {
                return None;
            }
            cur = next.nth_weekday(1, weekday).ok();
            Some(next)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests come directly from the RFC 5545 definition of the RRULE
    // property[1]. I tried to use inline snapshots where possible, but some
    // examples produce a lot of output. Those snapshots are in files. And
    // note that some examples specifically produce an infinite sequence, but
    // we put smaller bounds on such things to make them practically testable.
    //
    // Unlike the tests after the RFC 5545 tests, these tests are necessarily
    // grouped by FREQ. Instead, they are in the same order as listed in the
    // RFC, to make it easy to see what's missing and what isn't.
    //
    // [1]: https://icalendar.org/iCalendar-RFC-5545/3-8-5-3-recurrence-rule.html

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=DAILY;COUNT=10
    #[test]
    fn daily_for_ten_occurrences() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-03T09:00:00-04:00[America/New_York]
        1997-09-04T09:00:00-04:00[America/New_York]
        1997-09-05T09:00:00-04:00[America/New_York]
        1997-09-06T09:00:00-04:00[America/New_York]
        1997-09-07T09:00:00-04:00[America/New_York]
        1997-09-08T09:00:00-04:00[America/New_York]
        1997-09-09T09:00:00-04:00[America/New_York]
        1997-09-10T09:00:00-04:00[America/New_York]
        1997-09-11T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=DAILY;UNTIL=19971224T000000Z
    #[test]
    fn daily_until_dec24() {
        let start = zoned("19970902T090000[America/New_York]");
        let until = zoned("19971224T000000Z[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));

        // Do it again, but using the built-in "until" feature.
        let start = zoned("19970902T090000[America/New_York]");
        let until = zoned("19971224T000000Z[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .until(until)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(&rrule));
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=DAILY;INTERVAL=2
    #[test]
    fn daily_every_other_day_forever() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .interval(2)
            .build()
            .unwrap();
        // Supposed to be forever, but not practical to test that.
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-04T09:00:00-04:00[America/New_York]
        1997-09-06T09:00:00-04:00[America/New_York]
        1997-09-08T09:00:00-04:00[America/New_York]
        1997-09-10T09:00:00-04:00[America/New_York]
        1997-09-12T09:00:00-04:00[America/New_York]
        1997-09-14T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-18T09:00:00-04:00[America/New_York]
        1997-09-20T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=DAILY;INTERVAL=10;COUNT=5
    #[test]
    fn daily_every_ten_days_five_occurrences() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .interval(10)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-12T09:00:00-04:00[America/New_York]
        1997-09-22T09:00:00-04:00[America/New_York]
        1997-10-02T09:00:00-04:00[America/New_York]
        1997-10-12T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19980101T090000
    // RRULE:FREQ=YEARLY;UNTIL=20000131T140000Z;
    //  BYMONTH=1;BYDAY=SU,MO,TU,WE,TH,FR,SA
    // or
    // DTSTART;TZID=America/New_York:19980101T090000
    // RRULE:FREQ=DAILY;UNTIL=20000131T140000Z;BYMONTH=1
    #[test]
    fn daily_every_day_in_january_for_three_years() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("20000131T140000Z[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .by_month(1)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));

        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("20000131T140000Z[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month(1)
            .by_week_day(Weekday::Sunday..=Weekday::Saturday)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=WEEKLY;COUNT=10
    #[test]
    fn weekly_for_ten_occurrences() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Weekly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-09T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-23T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-07T09:00:00-04:00[America/New_York]
        1997-10-14T09:00:00-04:00[America/New_York]
        1997-10-21T09:00:00-04:00[America/New_York]
        1997-10-28T09:00:00-05:00[America/New_York]
        1997-11-04T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=WEEKLY;UNTIL=19971224T000000Z
    #[test]
    fn weekly_until_dec_24_1997() {
        let start = zoned("19970902T090000[America/New_York]");
        let until = zoned("19971224T000000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .until(until)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(&rrule),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-09T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-23T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-07T09:00:00-04:00[America/New_York]
        1997-10-14T09:00:00-04:00[America/New_York]
        1997-10-21T09:00:00-04:00[America/New_York]
        1997-10-28T09:00:00-05:00[America/New_York]
        1997-11-04T09:00:00-05:00[America/New_York]
        1997-11-11T09:00:00-05:00[America/New_York]
        1997-11-18T09:00:00-05:00[America/New_York]
        1997-11-25T09:00:00-05:00[America/New_York]
        1997-12-02T09:00:00-05:00[America/New_York]
        1997-12-09T09:00:00-05:00[America/New_York]
        1997-12-16T09:00:00-05:00[America/New_York]
        1997-12-23T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=WEEKLY;INTERVAL=2;WKST=SU
    #[test]
    fn weekly_every_other_week_forever() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .interval(2)
            .week_start(Weekday::Sunday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(13)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-14T09:00:00-04:00[America/New_York]
        1997-10-28T09:00:00-05:00[America/New_York]
        1997-11-11T09:00:00-05:00[America/New_York]
        1997-11-25T09:00:00-05:00[America/New_York]
        1997-12-09T09:00:00-05:00[America/New_York]
        1997-12-23T09:00:00-05:00[America/New_York]
        1998-01-06T09:00:00-05:00[America/New_York]
        1998-01-20T09:00:00-05:00[America/New_York]
        1998-02-03T09:00:00-05:00[America/New_York]
        1998-02-17T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=WEEKLY;UNTIL=19971007T000000Z;WKST=SU;BYDAY=TU,TH
    // or
    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=WEEKLY;COUNT=10;WKST=SU;BYDAY=TU,TH
    #[test]
    fn weekly_on_tues_and_thurs_for_five_weeks() {
        let start = zoned("19970902T090000[America/New_York]");
        let until = zoned("19971007T000000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .until(until)
            .by_week_day([Weekday::Tuesday, Weekday::Thursday])
            .week_start(Weekday::Sunday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(&rrule),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-04T09:00:00-04:00[America/New_York]
        1997-09-09T09:00:00-04:00[America/New_York]
        1997-09-11T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-18T09:00:00-04:00[America/New_York]
        1997-09-23T09:00:00-04:00[America/New_York]
        1997-09-25T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-02T09:00:00-04:00[America/New_York]
        ",
        );

        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .by_week_day([Weekday::Tuesday, Weekday::Thursday])
            .week_start(Weekday::Sunday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-04T09:00:00-04:00[America/New_York]
        1997-09-09T09:00:00-04:00[America/New_York]
        1997-09-11T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-18T09:00:00-04:00[America/New_York]
        1997-09-23T09:00:00-04:00[America/New_York]
        1997-09-25T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-02T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970901T090000
    // RRULE:FREQ=WEEKLY;INTERVAL=2;UNTIL=19971224T000000Z;WKST=SU;
    //  BYDAY=MO,WE,FR
    #[test]
    fn weekly_every_other_week_mon_wed_fri() {
        let start = zoned("19970901T090000[America/New_York]");
        let until = zoned("19971224T000000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .interval(2)
            .until(until)
            .week_start(Weekday::Sunday)
            .by_week_day([
                Weekday::Monday,
                Weekday::Wednesday,
                Weekday::Friday,
            ])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(&rrule),
            @r"
        1997-09-01T09:00:00-04:00[America/New_York]
        1997-09-03T09:00:00-04:00[America/New_York]
        1997-09-05T09:00:00-04:00[America/New_York]
        1997-09-15T09:00:00-04:00[America/New_York]
        1997-09-17T09:00:00-04:00[America/New_York]
        1997-09-19T09:00:00-04:00[America/New_York]
        1997-09-29T09:00:00-04:00[America/New_York]
        1997-10-01T09:00:00-04:00[America/New_York]
        1997-10-03T09:00:00-04:00[America/New_York]
        1997-10-13T09:00:00-04:00[America/New_York]
        1997-10-15T09:00:00-04:00[America/New_York]
        1997-10-17T09:00:00-04:00[America/New_York]
        1997-10-27T09:00:00-05:00[America/New_York]
        1997-10-29T09:00:00-05:00[America/New_York]
        1997-10-31T09:00:00-05:00[America/New_York]
        1997-11-10T09:00:00-05:00[America/New_York]
        1997-11-12T09:00:00-05:00[America/New_York]
        1997-11-14T09:00:00-05:00[America/New_York]
        1997-11-24T09:00:00-05:00[America/New_York]
        1997-11-26T09:00:00-05:00[America/New_York]
        1997-11-28T09:00:00-05:00[America/New_York]
        1997-12-08T09:00:00-05:00[America/New_York]
        1997-12-10T09:00:00-05:00[America/New_York]
        1997-12-12T09:00:00-05:00[America/New_York]
        1997-12-22T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=8;WKST=SU;BYDAY=TU,TH
    #[test]
    fn weekly_every_other_week_tues_thurs() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .interval(2)
            .week_start(Weekday::Sunday)
            .by_week_day([Weekday::Tuesday, Weekday::Thursday])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(8)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-04T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-18T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-02T09:00:00-04:00[America/New_York]
        1997-10-14T09:00:00-04:00[America/New_York]
        1997-10-16T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970905T090000
    // RRULE:FREQ=MONTHLY;COUNT=10;BYDAY=1FR
    #[test]
    fn monthly_first_friday_ten_occurrences() {
        let start = zoned("19970905T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day((1, Weekday::Friday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-05T09:00:00-04:00[America/New_York]
        1997-10-03T09:00:00-04:00[America/New_York]
        1997-11-07T09:00:00-05:00[America/New_York]
        1997-12-05T09:00:00-05:00[America/New_York]
        1998-01-02T09:00:00-05:00[America/New_York]
        1998-02-06T09:00:00-05:00[America/New_York]
        1998-03-06T09:00:00-05:00[America/New_York]
        1998-04-03T09:00:00-05:00[America/New_York]
        1998-05-01T09:00:00-04:00[America/New_York]
        1998-06-05T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970905T090000
    // RRULE:FREQ=MONTHLY;UNTIL=19971224T000000Z;BYDAY=1FR
    #[test]
    fn monthly_first_friday_until_dec_24_1997() {
        let start = zoned("19970905T090000[America/New_York]");
        let until = zoned("19971224T000000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day((1, Weekday::Friday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1997-09-05T09:00:00-04:00[America/New_York]
        1997-10-03T09:00:00-04:00[America/New_York]
        1997-11-07T09:00:00-05:00[America/New_York]
        1997-12-05T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970907T090000
    // RRULE:FREQ=MONTHLY;INTERVAL=2;COUNT=10;BYDAY=1SU,-1SU
    #[test]
    fn monthly_every_other_month_first_last_sunday_ten_occurrences() {
        let start = zoned("19970907T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .interval(2)
            .by_week_day([(1, Weekday::Sunday), (-1, Weekday::Sunday)])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-07T09:00:00-04:00[America/New_York]
        1997-09-28T09:00:00-04:00[America/New_York]
        1997-11-02T09:00:00-05:00[America/New_York]
        1997-11-30T09:00:00-05:00[America/New_York]
        1998-01-04T09:00:00-05:00[America/New_York]
        1998-01-25T09:00:00-05:00[America/New_York]
        1998-03-01T09:00:00-05:00[America/New_York]
        1998-03-29T09:00:00-05:00[America/New_York]
        1998-05-03T09:00:00-04:00[America/New_York]
        1998-05-31T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970922T090000
    // RRULE:FREQ=MONTHLY;COUNT=6;BYDAY=-2MO
    #[test]
    fn monthly_second_to_last_monday_for_six_months() {
        let start = zoned("19970922T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day((-2, Weekday::Monday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(6)),
            @r"
        1997-09-22T09:00:00-04:00[America/New_York]
        1997-10-20T09:00:00-04:00[America/New_York]
        1997-11-17T09:00:00-05:00[America/New_York]
        1997-12-22T09:00:00-05:00[America/New_York]
        1998-01-19T09:00:00-05:00[America/New_York]
        1998-02-16T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970928T090000
    // RRULE:FREQ=MONTHLY;BYMONTHDAY=-3
    #[test]
    fn monthly_third_to_last_day_month_forever() {
        let start = zoned("19970905T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_month_day(-3)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(6)),
            @r"
        1997-09-28T09:00:00-04:00[America/New_York]
        1997-10-29T09:00:00-05:00[America/New_York]
        1997-11-28T09:00:00-05:00[America/New_York]
        1997-12-29T09:00:00-05:00[America/New_York]
        1998-01-29T09:00:00-05:00[America/New_York]
        1998-02-26T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=MONTHLY;COUNT=10;BYMONTHDAY=2,15
    #[test]
    fn monthly_on_2nd_15th_of_month_ten_occurrences() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_month_day([2, 15])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-15T09:00:00-04:00[America/New_York]
        1997-10-02T09:00:00-04:00[America/New_York]
        1997-10-15T09:00:00-04:00[America/New_York]
        1997-11-02T09:00:00-05:00[America/New_York]
        1997-11-15T09:00:00-05:00[America/New_York]
        1997-12-02T09:00:00-05:00[America/New_York]
        1997-12-15T09:00:00-05:00[America/New_York]
        1998-01-02T09:00:00-05:00[America/New_York]
        1998-01-15T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970930T090000
    // RRULE:FREQ=MONTHLY;COUNT=10;BYMONTHDAY=1,-1
    #[test]
    fn monthly_first_last_of_month_ten_occurrences() {
        let start = zoned("19970930T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_month_day([1, -1])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-10-01T09:00:00-04:00[America/New_York]
        1997-10-31T09:00:00-05:00[America/New_York]
        1997-11-01T09:00:00-05:00[America/New_York]
        1997-11-30T09:00:00-05:00[America/New_York]
        1997-12-01T09:00:00-05:00[America/New_York]
        1997-12-31T09:00:00-05:00[America/New_York]
        1998-01-01T09:00:00-05:00[America/New_York]
        1998-01-31T09:00:00-05:00[America/New_York]
        1998-02-01T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970910T090000
    // RRULE:FREQ=MONTHLY;INTERVAL=18;COUNT=10;BYMONTHDAY=10,11,12,13,14,15
    #[test]
    fn monthly_every_18_months_10th_15th_ten_occurrences() {
        let start = zoned("19970910T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .interval(18)
            .by_month_day(10..=15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-10T09:00:00-04:00[America/New_York]
        1997-09-11T09:00:00-04:00[America/New_York]
        1997-09-12T09:00:00-04:00[America/New_York]
        1997-09-13T09:00:00-04:00[America/New_York]
        1997-09-14T09:00:00-04:00[America/New_York]
        1997-09-15T09:00:00-04:00[America/New_York]
        1999-03-10T09:00:00-05:00[America/New_York]
        1999-03-11T09:00:00-05:00[America/New_York]
        1999-03-12T09:00:00-05:00[America/New_York]
        1999-03-13T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=MONTHLY;INTERVAL=2;BYDAY=TU
    #[test]
    fn monthly_every_tuesday_every_other_month() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .interval(2)
            .by_week_day(Weekday::Tuesday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(18)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-09T09:00:00-04:00[America/New_York]
        1997-09-16T09:00:00-04:00[America/New_York]
        1997-09-23T09:00:00-04:00[America/New_York]
        1997-09-30T09:00:00-04:00[America/New_York]
        1997-11-04T09:00:00-05:00[America/New_York]
        1997-11-11T09:00:00-05:00[America/New_York]
        1997-11-18T09:00:00-05:00[America/New_York]
        1997-11-25T09:00:00-05:00[America/New_York]
        1998-01-06T09:00:00-05:00[America/New_York]
        1998-01-13T09:00:00-05:00[America/New_York]
        1998-01-20T09:00:00-05:00[America/New_York]
        1998-01-27T09:00:00-05:00[America/New_York]
        1998-03-03T09:00:00-05:00[America/New_York]
        1998-03-10T09:00:00-05:00[America/New_York]
        1998-03-17T09:00:00-05:00[America/New_York]
        1998-03-24T09:00:00-05:00[America/New_York]
        1998-03-31T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970610T090000
    // RRULE:FREQ=YEARLY;COUNT=10;BYMONTH=6,7
    #[test]
    fn yearly_june_and_july_ten_times() {
        let start = zoned("19970610T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month(6..=7)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-06-10T09:00:00-04:00[America/New_York]
        1997-07-10T09:00:00-04:00[America/New_York]
        1998-06-10T09:00:00-04:00[America/New_York]
        1998-07-10T09:00:00-04:00[America/New_York]
        1999-06-10T09:00:00-04:00[America/New_York]
        1999-07-10T09:00:00-04:00[America/New_York]
        2000-06-10T09:00:00-04:00[America/New_York]
        2000-07-10T09:00:00-04:00[America/New_York]
        2001-06-10T09:00:00-04:00[America/New_York]
        2001-07-10T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970310T090000
    // RRULE:FREQ=YEARLY;INTERVAL=2;COUNT=10;BYMONTH=1,2,3
    #[test]
    fn yearly_every_other_year_jan_feb_march_ten_times() {
        let start = zoned("19970310T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .interval(2)
            .by_month(1..=3)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-03-10T09:00:00-05:00[America/New_York]
        1999-01-10T09:00:00-05:00[America/New_York]
        1999-02-10T09:00:00-05:00[America/New_York]
        1999-03-10T09:00:00-05:00[America/New_York]
        2001-01-10T09:00:00-05:00[America/New_York]
        2001-02-10T09:00:00-05:00[America/New_York]
        2001-03-10T09:00:00-05:00[America/New_York]
        2003-01-10T09:00:00-05:00[America/New_York]
        2003-02-10T09:00:00-05:00[America/New_York]
        2003-03-10T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970101T090000
    // RRULE:FREQ=YEARLY;INTERVAL=3;COUNT=10;BYYEARDAY=1,100,200
    #[test]
    fn yearly_every_third_year_doy_ten_times() {
        let start = zoned("19970101T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .interval(3)
            .by_year_day(1)
            .by_year_day(100)
            .by_year_day(200)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-01-01T09:00:00-05:00[America/New_York]
        1997-04-10T09:00:00-04:00[America/New_York]
        1997-07-19T09:00:00-04:00[America/New_York]
        2000-01-01T09:00:00-05:00[America/New_York]
        2000-04-09T09:00:00-04:00[America/New_York]
        2000-07-18T09:00:00-04:00[America/New_York]
        2003-01-01T09:00:00-05:00[America/New_York]
        2003-04-10T09:00:00-04:00[America/New_York]
        2003-07-19T09:00:00-04:00[America/New_York]
        2006-01-01T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970519T090000
    // RRULE:FREQ=YEARLY;BYDAY=20MO
    #[test]
    fn yearly_every_20th_monday() {
        let start = zoned("19970519T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week_day((20, Weekday::Monday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-05-19T09:00:00-04:00[America/New_York]
        1998-05-18T09:00:00-04:00[America/New_York]
        1999-05-17T09:00:00-04:00[America/New_York]
        2000-05-15T09:00:00-04:00[America/New_York]
        2001-05-14T09:00:00-04:00[America/New_York]
        2002-05-20T09:00:00-04:00[America/New_York]
        2003-05-19T09:00:00-04:00[America/New_York]
        2004-05-17T09:00:00-04:00[America/New_York]
        2005-05-16T09:00:00-04:00[America/New_York]
        2006-05-15T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970512T090000
    // RRULE:FREQ=YEARLY;BYWEEKNO=20;BYDAY=MO
    #[test]
    fn yearly_monday_of_20th_week() {
        let start = zoned("19970512T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week(20)
            .by_week_day(Weekday::Monday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-05-12T09:00:00-04:00[America/New_York]
        1998-05-11T09:00:00-04:00[America/New_York]
        1999-05-17T09:00:00-04:00[America/New_York]
        2000-05-15T09:00:00-04:00[America/New_York]
        2001-05-14T09:00:00-04:00[America/New_York]
        2002-05-13T09:00:00-04:00[America/New_York]
        2003-05-12T09:00:00-04:00[America/New_York]
        2004-05-10T09:00:00-04:00[America/New_York]
        2005-05-16T09:00:00-04:00[America/New_York]
        2006-05-15T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970313T090000
    // RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=TH
    #[test]
    fn yearly_every_thursday_in_march() {
        let start = zoned("19970313T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month(3)
            .by_week_day(Weekday::Thursday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(11)),
            @r"
        1997-03-13T09:00:00-05:00[America/New_York]
        1997-03-20T09:00:00-05:00[America/New_York]
        1997-03-27T09:00:00-05:00[America/New_York]
        1998-03-05T09:00:00-05:00[America/New_York]
        1998-03-12T09:00:00-05:00[America/New_York]
        1998-03-19T09:00:00-05:00[America/New_York]
        1998-03-26T09:00:00-05:00[America/New_York]
        1999-03-04T09:00:00-05:00[America/New_York]
        1999-03-11T09:00:00-05:00[America/New_York]
        1999-03-18T09:00:00-05:00[America/New_York]
        1999-03-25T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970605T090000
    // RRULE:FREQ=YEARLY;BYDAY=TH;BYMONTH=6,7,8
    #[test]
    fn yearly_every_thursday_only_in_june_july_aug() {
        let start = zoned("19970605T090000[America/New_York]");
        let until = zoned("20000101T000000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month(6..=8)
            .by_week_day(Weekday::Thursday)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ),);
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // EXDATE;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13
    #[test]
    fn monthly_every_friday_the_13th_forever() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day(Weekday::Friday)
            .by_month_day(13)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        1998-02-13T09:00:00-05:00[America/New_York]
        1998-03-13T09:00:00-05:00[America/New_York]
        1998-11-13T09:00:00-05:00[America/New_York]
        1999-08-13T09:00:00-04:00[America/New_York]
        2000-10-13T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970913T090000
    // RRULE:FREQ=MONTHLY;BYDAY=SA;BYMONTHDAY=7,8,9,10,11,12,13
    #[test]
    fn monthly_first_saturday_after_first_sunday() {
        let start = zoned("19970913T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day(Weekday::Saturday)
            .by_month_day(7..=13)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-09-13T09:00:00-04:00[America/New_York]
        1997-10-11T09:00:00-04:00[America/New_York]
        1997-11-08T09:00:00-05:00[America/New_York]
        1997-12-13T09:00:00-05:00[America/New_York]
        1998-01-10T09:00:00-05:00[America/New_York]
        1998-02-07T09:00:00-05:00[America/New_York]
        1998-03-07T09:00:00-05:00[America/New_York]
        1998-04-11T09:00:00-04:00[America/New_York]
        1998-05-09T09:00:00-04:00[America/New_York]
        1998-06-13T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19961105T090000
    // RRULE:FREQ=YEARLY;INTERVAL=4;BYMONTH=11;BYDAY=TU;
    //  BYMONTHDAY=2,3,4,5,6,7,8
    #[test]
    fn yearly_every_us_presidential_election_day() {
        let start = zoned("19961105T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .interval(4)
            .by_month(11)
            .by_week_day(Weekday::Tuesday)
            .by_month_day(2..=8)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1996-11-05T09:00:00-05:00[America/New_York]
        2000-11-07T09:00:00-05:00[America/New_York]
        2004-11-02T09:00:00-05:00[America/New_York]
        2008-11-04T09:00:00-05:00[America/New_York]
        2012-11-06T09:00:00-05:00[America/New_York]
        2016-11-08T09:00:00-05:00[America/New_York]
        2020-11-03T09:00:00-05:00[America/New_York]
        2024-11-05T09:00:00-05:00[America/New_York]
        2028-11-07T09:00:00-05:00[America/New_York]
        2032-11-02T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970904T090000
    // RRULE:FREQ=MONTHLY;COUNT=3;BYDAY=TU,WE,TH;BYSETPOS=3
    #[test]
    fn monthly_third_tues_wed_thurs_for_three_months() {
        let start = zoned("19970904T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day(Weekday::Tuesday..=Weekday::Thursday)
            .by_set_position(3)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(3)),
            @r"
        1997-09-04T09:00:00-04:00[America/New_York]
        1997-10-07T09:00:00-04:00[America/New_York]
        1997-11-06T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970929T090000
    // RRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-2
    #[test]
    fn monthly_second_to_last_weekday_of_month() {
        let start = zoned("19970929T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day(Weekday::Monday..=Weekday::Friday)
            .by_set_position(-2)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(7)),
            @r"
        1997-09-29T09:00:00-04:00[America/New_York]
        1997-10-30T09:00:00-05:00[America/New_York]
        1997-11-27T09:00:00-05:00[America/New_York]
        1997-12-30T09:00:00-05:00[America/New_York]
        1998-01-29T09:00:00-05:00[America/New_York]
        1998-02-26T09:00:00-05:00[America/New_York]
        1998-03-30T09:00:00-05:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=HOURLY;INTERVAL=3;UNTIL=19970902T170000Z
    #[test]
    fn hourly_every_three_hours_9am_5pm_on_specific_day() {
        let start = zoned("19970902T090000[America/New_York]");
        // Odd that UNTIL above is written with Z, which seems to
        // be contrary to the prose description of this rule?
        let until = zoned("19970902T170000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Hourly, start)
            .interval(3)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-02T12:00:00-04:00[America/New_York]
        1997-09-02T15:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=MINUTELY;INTERVAL=15;COUNT=6
    #[test]
    fn minutely_every_fifteen_minutes_six_occurrences() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Minutely, start)
            .interval(15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(6)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-02T09:15:00-04:00[America/New_York]
        1997-09-02T09:30:00-04:00[America/New_York]
        1997-09-02T09:45:00-04:00[America/New_York]
        1997-09-02T10:00:00-04:00[America/New_York]
        1997-09-02T10:15:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=MINUTELY;INTERVAL=90;COUNT=4
    #[test]
    fn minutely_every_hour_and_half_four_occurrences() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Minutely, start)
            .interval(90)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(4)),
            @r"
        1997-09-02T09:00:00-04:00[America/New_York]
        1997-09-02T10:30:00-04:00[America/New_York]
        1997-09-02T12:00:00-04:00[America/New_York]
        1997-09-02T13:30:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:19970902T090000
    // RRULE:FREQ=DAILY;BYHOUR=9,10,11,12,13,14,15,16;BYMINUTE=0,20,40
    // or
    // RRULE:FREQ=MINUTELY;INTERVAL=20;BYHOUR=9,10,11,12,13,14,15,16
    #[test]
    fn daily_every_20_minutes_9am_to_440pm() {
        let start = zoned("19970902T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .by_hour(9)
            .by_hour(10)
            .by_hour(11)
            .by_hour(12)
            .by_hour(13)
            .by_hour(14)
            .by_hour(15)
            .by_hour(16)
            .by_minute(0)
            .by_minute(20)
            .by_minute(40)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(rrule.iter().take(100)));
    }

    // DTSTART;TZID=America/New_York:19970805T090000
    // RRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=MO
    // and
    // DTSTART;TZID=America/New_York:19970805T090000
    // RRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=SU
    #[test]
    fn weekly_difference_based_on_wkst() {
        let start = zoned("19970805T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .interval(2)
            .week_start(Weekday::Monday)
            .by_week_day([Weekday::Tuesday, Weekday::Sunday])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(4)),
            @r"
        1997-08-05T09:00:00-04:00[America/New_York]
        1997-08-10T09:00:00-04:00[America/New_York]
        1997-08-19T09:00:00-04:00[America/New_York]
        1997-08-24T09:00:00-04:00[America/New_York]
        ",
        );

        let start = zoned("19970805T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .interval(2)
            .week_start(Weekday::Sunday)
            .by_week_day([Weekday::Tuesday, Weekday::Sunday])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(4)),
            @r"
        1997-08-05T09:00:00-04:00[America/New_York]
        1997-08-17T09:00:00-04:00[America/New_York]
        1997-08-19T09:00:00-04:00[America/New_York]
        1997-08-31T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // DTSTART;TZID=America/New_York:20070115T090000
    // RRULE:FREQ=MONTHLY;BYMONTHDAY=15,30;COUNT=5
    #[test]
    fn monthly_feb_30_ignored() {
        let start = zoned("20070115T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_month_day([15, 30])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2007-01-15T09:00:00-05:00[America/New_York]
        2007-01-30T09:00:00-05:00[America/New_York]
        2007-02-15T09:00:00-05:00[America/New_York]
        2007-03-15T09:00:00-04:00[America/New_York]
        2007-03-30T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // From this point on, these cases aren't from RFC 5545, but just from my
    // own devising. In many cases, they are branched off of test cases from
    // RFC 5545. The main point is that I tried to test every branch of the
    // implementation as I wrote it. (I should probably add coverage testing.)

    // This first batch of tests is for the YEARLY frequency.

    #[test]
    fn yearly_every_week_day_1998_in_some_weeks() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week(6..=10)
            .by_week_day(Weekday::Monday..=Weekday::Friday)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));
    }

    #[test]
    fn yearly_every_week_day_jan_feb_1998_overlap_some_weeks() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_month(1..=2)
            .by_week(4..=6)
            .by_week_day(Weekday::Monday..=Weekday::Friday)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));
    }

    #[test]
    fn yearly_some_weeks_in_1998() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week(4..=6)
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));

        // Like the above, but test that BYYEARDAY intersects with the above.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week(4..=6)
            .by_year_day([33, 30])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1998-01-30T09:00:00-05:00[America/New_York]
        1998-02-02T09:00:00-05:00[America/New_York]
        ",
        );

        // Like the above, but test that BYMONTHDAY intersects with the above.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week(4..=6)
            .by_month_day([21, 29, 30, 31, 2, 5])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1998-01-21T09:00:00-05:00[America/New_York]
        1998-01-29T09:00:00-05:00[America/New_York]
        1998-01-30T09:00:00-05:00[America/New_York]
        1998-01-31T09:00:00-05:00[America/New_York]
        1998-02-02T09:00:00-05:00[America/New_York]
        1998-02-05T09:00:00-05:00[America/New_York]
        ",
        );

        // Like the above, but test that BYYEARDAY and BYMONTHDAY intersects.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week(4..=6)
            .by_year_day([33, 30])
            .by_month_day(2)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @"1998-02-02T09:00:00-05:00[America/New_York]",
        );
    }

    #[test]
    fn yearly_every_weekend_in_2025() {
        let start = zoned("20250101T090000[America/New_York]");
        let until = zoned("20251231T235959[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week_day([Weekday::Sunday, Weekday::Saturday])
            .build()
            .unwrap();
        insta::assert_snapshot!(snapshot(
            rrule.iter().take_while(|zdt| zdt <= until)
        ));

        // Like the above, but test that BYYEARDAY intersects with the above.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week_day([Weekday::Sunday, Weekday::Saturday])
            .by_year_day([4, 12])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-01-04T09:00:00-05:00[America/New_York]
        2025-01-12T09:00:00-05:00[America/New_York]
        ",
        );

        // And test that BYMONTHDAY also intersects.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week_day([Weekday::Sunday, Weekday::Saturday])
            .by_month_day([4, 19])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-01-04T09:00:00-05:00[America/New_York]
        2025-01-19T09:00:00-05:00[America/New_York]
        2025-04-19T09:00:00-04:00[America/New_York]
        2025-05-04T09:00:00-04:00[America/New_York]
        2025-07-19T09:00:00-04:00[America/New_York]
        2025-10-04T09:00:00-04:00[America/New_York]
        2025-10-19T09:00:00-04:00[America/New_York]
        ",
        );

        // And now both at the same time.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_week_day([Weekday::Sunday, Weekday::Saturday])
            .by_year_day([4, 200])
            .by_month_day([4, 19])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-01-04T09:00:00-05:00[America/New_York]
        2025-07-19T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_every_last_monday() {
        let start = zoned("19970519T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week_day((-1, Weekday::Monday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-12-29T09:00:00-05:00[America/New_York]
        1998-12-28T09:00:00-05:00[America/New_York]
        1999-12-27T09:00:00-05:00[America/New_York]
        2000-12-25T09:00:00-05:00[America/New_York]
        2001-12-31T09:00:00-05:00[America/New_York]
        2002-12-30T09:00:00-05:00[America/New_York]
        2003-12-29T09:00:00-05:00[America/New_York]
        2004-12-27T09:00:00-05:00[America/New_York]
        2005-12-26T09:00:00-05:00[America/New_York]
        2006-12-25T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_every_last_monday_of_month() {
        let start = zoned("19970519T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month(1..=12)
            .by_week_day((-1, Weekday::Monday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        1997-05-26T09:00:00-04:00[America/New_York]
        1997-06-30T09:00:00-04:00[America/New_York]
        1997-07-28T09:00:00-04:00[America/New_York]
        1997-08-25T09:00:00-04:00[America/New_York]
        1997-09-29T09:00:00-04:00[America/New_York]
        1997-10-27T09:00:00-05:00[America/New_York]
        1997-11-24T09:00:00-05:00[America/New_York]
        1997-12-29T09:00:00-05:00[America/New_York]
        1998-01-26T09:00:00-05:00[America/New_York]
        1998-02-23T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_for_five_years() {
        let start = zoned("20250715T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Yearly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-07-15T09:00:00-04:00[America/New_York]
        2026-07-15T09:00:00-04:00[America/New_York]
        2027-07-15T09:00:00-04:00[America/New_York]
        2028-07-15T09:00:00-04:00[America/New_York]
        2029-07-15T09:00:00-04:00[America/New_York]
        ",
        );

        // Tricky with leap years. This actually happens. I closed on my house
        // on Feb 29.
        let start = zoned("20240229T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Yearly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2024-02-29T09:00:00-05:00[America/New_York]
        2028-02-29T09:00:00-05:00[America/New_York]
        2032-02-29T09:00:00-05:00[America/New_York]
        2036-02-29T09:00:00-05:00[America/New_York]
        2040-02-29T09:00:00-05:00[America/New_York]
        ",
        );

        // Starting day before leap day.
        let start = zoned("20240228T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Yearly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2024-02-28T09:00:00-05:00[America/New_York]
        2025-02-28T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2027-02-28T09:00:00-05:00[America/New_York]
        2028-02-28T09:00:00-05:00[America/New_York]
        ",
        );

        // Starting day after leap day.
        let start = zoned("20240301T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Yearly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2024-03-01T09:00:00-05:00[America/New_York]
        2025-03-01T09:00:00-05:00[America/New_York]
        2026-03-01T09:00:00-05:00[America/New_York]
        2027-03-01T09:00:00-05:00[America/New_York]
        2028-03-01T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_leap() {
        // This is tested elsewhere, but in simple cases where we don't
        // have any expansion rules for selecting a different day than what
        // is in the starting point, we skip ahead 4 years at a time. This
        // is because invalid dates, like 2025-02-29, are meant to be ignored.
        let start = zoned("20240229T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Yearly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2024-02-29T09:00:00-05:00[America/New_York]
        2028-02-29T09:00:00-05:00[America/New_York]
        2032-02-29T09:00:00-05:00[America/New_York]
        2036-02-29T09:00:00-05:00[America/New_York]
        2040-02-29T09:00:00-05:00[America/New_York]
        ",
        );

        // But note that if we specify BYMONTH, BYWEEKNO, BYYEARDAY, BYMONTHDAY
        // or BYDAY, then each yearly interval should be considered, even if it
        // would initially have an invalid date (like Feb 29 2025). A similar
        // thing applies to MONTHLY frequency (where the MONTHLY case is more
        // likely to come up in real examples).

        let start = zoned("20240229T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month([2, 5, 11])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        2024-02-29T09:00:00-05:00[America/New_York]
        2024-05-29T09:00:00-04:00[America/New_York]
        2024-11-29T09:00:00-05:00[America/New_York]
        2025-05-29T09:00:00-04:00[America/New_York]
        2025-11-29T09:00:00-05:00[America/New_York]
        2026-05-29T09:00:00-04:00[America/New_York]
        2026-11-29T09:00:00-05:00[America/New_York]
        2027-05-29T09:00:00-04:00[America/New_York]
        2027-11-29T09:00:00-05:00[America/New_York]
        2028-02-29T09:00:00-05:00[America/New_York]
        ",
        );

        let start = zoned("20240229T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week([2, 20, 50])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(20)),
            @r"
        2024-05-13T09:00:00-04:00[America/New_York]
        2024-05-14T09:00:00-04:00[America/New_York]
        2024-05-15T09:00:00-04:00[America/New_York]
        2024-05-16T09:00:00-04:00[America/New_York]
        2024-05-17T09:00:00-04:00[America/New_York]
        2024-05-18T09:00:00-04:00[America/New_York]
        2024-05-19T09:00:00-04:00[America/New_York]
        2024-12-09T09:00:00-05:00[America/New_York]
        2024-12-10T09:00:00-05:00[America/New_York]
        2024-12-11T09:00:00-05:00[America/New_York]
        2024-12-12T09:00:00-05:00[America/New_York]
        2024-12-13T09:00:00-05:00[America/New_York]
        2024-12-14T09:00:00-05:00[America/New_York]
        2024-12-15T09:00:00-05:00[America/New_York]
        2025-01-06T09:00:00-05:00[America/New_York]
        2025-01-07T09:00:00-05:00[America/New_York]
        2025-01-08T09:00:00-05:00[America/New_York]
        2025-01-09T09:00:00-05:00[America/New_York]
        2025-01-10T09:00:00-05:00[America/New_York]
        2025-01-11T09:00:00-05:00[America/New_York]
        ",
        );

        let start = zoned("20240229T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_year_day([2, 59, 60, 61, 300])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(25)),
            @r"
        2024-02-29T09:00:00-05:00[America/New_York]
        2024-03-01T09:00:00-05:00[America/New_York]
        2024-10-26T09:00:00-04:00[America/New_York]
        2025-01-02T09:00:00-05:00[America/New_York]
        2025-02-28T09:00:00-05:00[America/New_York]
        2025-03-01T09:00:00-05:00[America/New_York]
        2025-03-02T09:00:00-05:00[America/New_York]
        2025-10-27T09:00:00-04:00[America/New_York]
        2026-01-02T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-01T09:00:00-05:00[America/New_York]
        2026-03-02T09:00:00-05:00[America/New_York]
        2026-10-27T09:00:00-04:00[America/New_York]
        2027-01-02T09:00:00-05:00[America/New_York]
        2027-02-28T09:00:00-05:00[America/New_York]
        2027-03-01T09:00:00-05:00[America/New_York]
        2027-03-02T09:00:00-05:00[America/New_York]
        2027-10-27T09:00:00-04:00[America/New_York]
        2028-01-02T09:00:00-05:00[America/New_York]
        2028-02-28T09:00:00-05:00[America/New_York]
        2028-02-29T09:00:00-05:00[America/New_York]
        2028-03-01T09:00:00-05:00[America/New_York]
        2028-10-26T09:00:00-04:00[America/New_York]
        2029-01-02T09:00:00-05:00[America/New_York]
        2029-02-28T09:00:00-05:00[America/New_York]
        ",
        );

        let start = zoned("20240229T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month_day(15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(20)),
            @r"
        2024-03-15T09:00:00-04:00[America/New_York]
        2024-04-15T09:00:00-04:00[America/New_York]
        2024-05-15T09:00:00-04:00[America/New_York]
        2024-06-15T09:00:00-04:00[America/New_York]
        2024-07-15T09:00:00-04:00[America/New_York]
        2024-08-15T09:00:00-04:00[America/New_York]
        2024-09-15T09:00:00-04:00[America/New_York]
        2024-10-15T09:00:00-04:00[America/New_York]
        2024-11-15T09:00:00-05:00[America/New_York]
        2024-12-15T09:00:00-05:00[America/New_York]
        2025-01-15T09:00:00-05:00[America/New_York]
        2025-02-15T09:00:00-05:00[America/New_York]
        2025-03-15T09:00:00-04:00[America/New_York]
        2025-04-15T09:00:00-04:00[America/New_York]
        2025-05-15T09:00:00-04:00[America/New_York]
        2025-06-15T09:00:00-04:00[America/New_York]
        2025-07-15T09:00:00-04:00[America/New_York]
        2025-08-15T09:00:00-04:00[America/New_York]
        2025-09-15T09:00:00-04:00[America/New_York]
        2025-10-15T09:00:00-04:00[America/New_York]
        ",
        );

        let start = zoned("20240229T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week_day((-1, Weekday::Friday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(10)),
            @r"
        2024-12-27T09:00:00-05:00[America/New_York]
        2025-12-26T09:00:00-05:00[America/New_York]
        2026-12-25T09:00:00-05:00[America/New_York]
        2027-12-31T09:00:00-05:00[America/New_York]
        2028-12-29T09:00:00-05:00[America/New_York]
        2029-12-28T09:00:00-05:00[America/New_York]
        2030-12-27T09:00:00-05:00[America/New_York]
        2031-12-26T09:00:00-05:00[America/New_York]
        2032-12-24T09:00:00-05:00[America/New_York]
        2033-12-30T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_for_two_years_in_some_months() {
        let start = zoned("20250715T090000[America/New_York]");
        let until = zoned("20271231T235959[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month([2, 8, 11])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-08-15T09:00:00-04:00[America/New_York]
        2025-11-15T09:00:00-05:00[America/New_York]
        2026-02-15T09:00:00-05:00[America/New_York]
        2026-08-15T09:00:00-04:00[America/New_York]
        2026-11-15T09:00:00-05:00[America/New_York]
        2027-02-15T09:00:00-05:00[America/New_York]
        2027-08-15T09:00:00-04:00[America/New_York]
        2027-11-15T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_in_1998_some_weeks_in_1998() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_week(4..=6)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1998-01-19T09:00:00-05:00[America/New_York]
        1998-01-20T09:00:00-05:00[America/New_York]
        1998-01-21T09:00:00-05:00[America/New_York]
        1998-01-22T09:00:00-05:00[America/New_York]
        1998-01-23T09:00:00-05:00[America/New_York]
        1998-01-24T09:00:00-05:00[America/New_York]
        1998-01-25T09:00:00-05:00[America/New_York]
        1998-01-26T09:00:00-05:00[America/New_York]
        1998-01-27T09:00:00-05:00[America/New_York]
        1998-01-28T09:00:00-05:00[America/New_York]
        1998-01-29T09:00:00-05:00[America/New_York]
        1998-01-30T09:00:00-05:00[America/New_York]
        1998-01-31T09:00:00-05:00[America/New_York]
        1998-02-01T09:00:00-05:00[America/New_York]
        1998-02-02T09:00:00-05:00[America/New_York]
        1998-02-03T09:00:00-05:00[America/New_York]
        1998-02-04T09:00:00-05:00[America/New_York]
        1998-02-05T09:00:00-05:00[America/New_York]
        1998-02-06T09:00:00-05:00[America/New_York]
        1998-02-07T09:00:00-05:00[America/New_York]
        1998-02-08T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_in_1998_some_weeks_in_some_months() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .by_month(2)
            .by_week(4..=6)
            .by_week([9, 20])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1998-02-01T09:00:00-05:00[America/New_York]
        1998-02-02T09:00:00-05:00[America/New_York]
        1998-02-03T09:00:00-05:00[America/New_York]
        1998-02-04T09:00:00-05:00[America/New_York]
        1998-02-05T09:00:00-05:00[America/New_York]
        1998-02-06T09:00:00-05:00[America/New_York]
        1998-02-07T09:00:00-05:00[America/New_York]
        1998-02-08T09:00:00-05:00[America/New_York]
        1998-02-23T09:00:00-05:00[America/New_York]
        1998-02-24T09:00:00-05:00[America/New_York]
        1998-02-25T09:00:00-05:00[America/New_York]
        1998-02-26T09:00:00-05:00[America/New_York]
        1998-02-27T09:00:00-05:00[America/New_York]
        1998-02-28T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn yearly_in_1998_months_days() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_month([2, 4, 10, 12])
            .by_month_day([1, 25, 31])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1998-02-01T09:00:00-05:00[America/New_York]
        1998-02-25T09:00:00-05:00[America/New_York]
        1998-04-01T09:00:00-05:00[America/New_York]
        1998-04-25T09:00:00-04:00[America/New_York]
        1998-10-01T09:00:00-04:00[America/New_York]
        1998-10-25T09:00:00-05:00[America/New_York]
        1998-10-31T09:00:00-05:00[America/New_York]
        1998-12-01T09:00:00-05:00[America/New_York]
        1998-12-25T09:00:00-05:00[America/New_York]
        1998-12-31T09:00:00-05:00[America/New_York]
        ",
        );

        // Like the above, but test that specifying `by_year_day` results in
        // an intersection.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_month([2, 4, 10, 12])
            .by_month_day([1, 25, 31])
            .by_year_day(359)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @"1998-12-25T09:00:00-05:00[America/New_York]",
        );
    }

    #[test]
    fn yearly_in_1998_every_25th_and_31st() {
        let start = zoned("19980101T090000[America/New_York]");
        let until = zoned("19990101T000000Z[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_month_day([25, 31])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        1998-01-25T09:00:00-05:00[America/New_York]
        1998-01-31T09:00:00-05:00[America/New_York]
        1998-02-25T09:00:00-05:00[America/New_York]
        1998-03-25T09:00:00-05:00[America/New_York]
        1998-03-31T09:00:00-05:00[America/New_York]
        1998-04-25T09:00:00-04:00[America/New_York]
        1998-05-25T09:00:00-04:00[America/New_York]
        1998-05-31T09:00:00-04:00[America/New_York]
        1998-06-25T09:00:00-04:00[America/New_York]
        1998-07-25T09:00:00-04:00[America/New_York]
        1998-07-31T09:00:00-04:00[America/New_York]
        1998-08-25T09:00:00-04:00[America/New_York]
        1998-08-31T09:00:00-04:00[America/New_York]
        1998-09-25T09:00:00-04:00[America/New_York]
        1998-10-25T09:00:00-05:00[America/New_York]
        1998-10-31T09:00:00-05:00[America/New_York]
        1998-11-25T09:00:00-05:00[America/New_York]
        1998-12-25T09:00:00-05:00[America/New_York]
        1998-12-31T09:00:00-05:00[America/New_York]
        ",
        );

        // Like the above, but test that specifying `by_year_day` results in
        // an intersection.
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_month_day([25, 31])
            .by_year_day(359)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @"1998-12-25T09:00:00-05:00[America/New_York]",
        );
    }

    #[test]
    fn yearly_times() {
        let start = zoned("20250101T000000[America/New_York]");
        let until = zoned("20260101T000000[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @"2025-07-19T00:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_hour(15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @"2025-07-19T15:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_hour([15, 8])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T08:00:00-04:00[America/New_York]
        2025-07-19T15:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_hour([15, 8])
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T08:15:00-04:00[America/New_York]
        2025-07-19T08:55:00-04:00[America/New_York]
        2025-07-19T15:15:00-04:00[America/New_York]
        2025-07-19T15:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_hour([15, 8])
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T08:15:10-04:00[America/New_York]
        2025-07-19T08:15:55-04:00[America/New_York]
        2025-07-19T08:55:10-04:00[America/New_York]
        2025-07-19T08:55:55-04:00[America/New_York]
        2025-07-19T15:15:10-04:00[America/New_York]
        2025-07-19T15:15:55-04:00[America/New_York]
        2025-07-19T15:55:10-04:00[America/New_York]
        2025-07-19T15:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T00:15:00-04:00[America/New_York]
        2025-07-19T00:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T00:15:10-04:00[America/New_York]
        2025-07-19T00:15:55-04:00[America/New_York]
        2025-07-19T00:55:10-04:00[America/New_York]
        2025-07-19T00:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_hour([15, 8])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T08:00:10-04:00[America/New_York]
        2025-07-19T08:00:55-04:00[America/New_York]
        2025-07-19T15:00:10-04:00[America/New_York]
        2025-07-19T15:00:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .by_year_day(200)
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-07-19T00:00:10-04:00[America/New_York]
        2025-07-19T00:00:55-04:00[America/New_York]
        ",
        );
    }

    // MONTHLY frequency tests.

    #[test]
    fn monthly_basic() {
        let start = zoned("20250401T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-04-01T09:00:00-04:00[America/New_York]
        2025-05-01T09:00:00-04:00[America/New_York]
        2025-06-01T09:00:00-04:00[America/New_York]
        2025-07-01T09:00:00-04:00[America/New_York]
        2025-08-01T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn monthly_31st() {
        let start = zoned("20250131T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(7)),
            @r"
        2025-01-31T09:00:00-05:00[America/New_York]
        2025-03-31T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        ",
        );

        // But note that if we specify BYDAY or BYMONTHDAY, then each
        // monthly interval should be considered, even if it would initially
        // have an invalid date (like Feb 31). A similar thing applies to
        // YEARLY frequency.

        let start = zoned("20250131T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_month_day(15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(7)),
            @r"
        2025-02-15T09:00:00-05:00[America/New_York]
        2025-03-15T09:00:00-04:00[America/New_York]
        2025-04-15T09:00:00-04:00[America/New_York]
        2025-05-15T09:00:00-04:00[America/New_York]
        2025-06-15T09:00:00-04:00[America/New_York]
        2025-07-15T09:00:00-04:00[America/New_York]
        2025-08-15T09:00:00-04:00[America/New_York]
        ",
        );

        let start = zoned("20250131T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Monthly, start)
            .by_week_day((1, Weekday::Wednesday))
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(7)),
            @r"
        2025-02-05T09:00:00-05:00[America/New_York]
        2025-03-05T09:00:00-05:00[America/New_York]
        2025-04-02T09:00:00-04:00[America/New_York]
        2025-05-07T09:00:00-04:00[America/New_York]
        2025-06-04T09:00:00-04:00[America/New_York]
        2025-07-02T09:00:00-04:00[America/New_York]
        2025-08-06T09:00:00-04:00[America/New_York]
        ",
        );
    }

    // WEEKLY frequency tests.

    #[test]
    fn weekly_basic() {
        let start = zoned("20250401T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Weekly, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-04-01T09:00:00-04:00[America/New_York]
        2025-04-08T09:00:00-04:00[America/New_York]
        2025-04-15T09:00:00-04:00[America/New_York]
        2025-04-22T09:00:00-04:00[America/New_York]
        2025-04-29T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn weekly_by_month() {
        let start = zoned("20250401T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Weekly, start)
            .interval(3)
            .by_month(6..=12)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(20)),
            @r"
        2025-06-03T09:00:00-04:00[America/New_York]
        2025-06-24T09:00:00-04:00[America/New_York]
        2025-07-15T09:00:00-04:00[America/New_York]
        2025-08-05T09:00:00-04:00[America/New_York]
        2025-08-26T09:00:00-04:00[America/New_York]
        2025-09-16T09:00:00-04:00[America/New_York]
        2025-10-07T09:00:00-04:00[America/New_York]
        2025-10-28T09:00:00-04:00[America/New_York]
        2025-11-18T09:00:00-05:00[America/New_York]
        2025-12-09T09:00:00-05:00[America/New_York]
        2025-12-30T09:00:00-05:00[America/New_York]
        2026-06-16T09:00:00-04:00[America/New_York]
        2026-07-07T09:00:00-04:00[America/New_York]
        2026-07-28T09:00:00-04:00[America/New_York]
        2026-08-18T09:00:00-04:00[America/New_York]
        2026-09-08T09:00:00-04:00[America/New_York]
        2026-09-29T09:00:00-04:00[America/New_York]
        2026-10-20T09:00:00-04:00[America/New_York]
        2026-11-10T09:00:00-05:00[America/New_York]
        2026-12-01T09:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn weekly_times() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20250408T000000[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @"2025-04-01T09:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_hour(15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @"2025-04-01T15:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_hour([15, 10])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T10:00:00-04:00[America/New_York]
        2025-04-01T15:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_hour([15, 10])
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T10:15:00-04:00[America/New_York]
        2025-04-01T10:55:00-04:00[America/New_York]
        2025-04-01T15:15:00-04:00[America/New_York]
        2025-04-01T15:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_hour([15, 9])
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:15:10-04:00[America/New_York]
        2025-04-01T09:15:55-04:00[America/New_York]
        2025-04-01T09:55:10-04:00[America/New_York]
        2025-04-01T09:55:55-04:00[America/New_York]
        2025-04-01T15:15:10-04:00[America/New_York]
        2025-04-01T15:15:55-04:00[America/New_York]
        2025-04-01T15:55:10-04:00[America/New_York]
        2025-04-01T15:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:15:00-04:00[America/New_York]
        2025-04-01T09:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:15:10-04:00[America/New_York]
        2025-04-01T09:15:55-04:00[America/New_York]
        2025-04-01T09:55:10-04:00[America/New_York]
        2025-04-01T09:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_hour([15, 9])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:10-04:00[America/New_York]
        2025-04-01T09:00:55-04:00[America/New_York]
        2025-04-01T15:00:10-04:00[America/New_York]
        2025-04-01T15:00:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Weekly, start.clone())
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:10-04:00[America/New_York]
        2025-04-01T09:00:55-04:00[America/New_York]
        ",
        );
    }

    // DAILY frequency tests.

    #[test]
    fn daily_basic() {
        let start = zoned("20250401T090000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-04-01T09:00:00-04:00[America/New_York]
        2025-04-02T09:00:00-04:00[America/New_York]
        2025-04-03T09:00:00-04:00[America/New_York]
        2025-04-04T09:00:00-04:00[America/New_York]
        2025-04-05T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn daily_by_month() {
        let start = zoned("20250430T090000[America/New_York]");
        let until = zoned("20250601T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .by_month([4, 6])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-06-01T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn daily_by_month_day() {
        let start = zoned("20250401T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .by_month_day([2, 4])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-04-02T09:00:00-04:00[America/New_York]
        2025-04-04T09:00:00-04:00[America/New_York]
        2025-05-02T09:00:00-04:00[America/New_York]
        2025-05-04T09:00:00-04:00[America/New_York]
        2025-06-02T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn daily_by_week_day() {
        let start = zoned("20250401T090000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Daily, start)
            .by_week_day([Weekday::Sunday, Weekday::Saturday])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-04-05T09:00:00-04:00[America/New_York]
        2025-04-06T09:00:00-04:00[America/New_York]
        2025-04-12T09:00:00-04:00[America/New_York]
        2025-04-13T09:00:00-04:00[America/New_York]
        2025-04-19T09:00:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn daily_times() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20250402T000000[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @"2025-04-01T09:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_hour(15)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @"2025-04-01T15:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_hour([15, 10])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T10:00:00-04:00[America/New_York]
        2025-04-01T15:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_hour([15, 10])
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T10:15:00-04:00[America/New_York]
        2025-04-01T10:55:00-04:00[America/New_York]
        2025-04-01T15:15:00-04:00[America/New_York]
        2025-04-01T15:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_hour([15, 9])
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T09:15:10-04:00[America/New_York]
        2025-04-01T09:15:55-04:00[America/New_York]
        2025-04-01T09:55:10-04:00[America/New_York]
        2025-04-01T09:55:55-04:00[America/New_York]
        2025-04-01T15:15:10-04:00[America/New_York]
        2025-04-01T15:15:55-04:00[America/New_York]
        2025-04-01T15:55:10-04:00[America/New_York]
        2025-04-01T15:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T09:15:00-04:00[America/New_York]
        2025-04-01T09:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T09:15:10-04:00[America/New_York]
        2025-04-01T09:15:55-04:00[America/New_York]
        2025-04-01T09:55:10-04:00[America/New_York]
        2025-04-01T09:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_hour([15, 9])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T09:00:10-04:00[America/New_York]
        2025-04-01T09:00:55-04:00[America/New_York]
        2025-04-01T15:00:10-04:00[America/New_York]
        2025-04-01T15:00:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Daily, start.clone())
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt < until)),
            @r"
        2025-04-01T09:00:10-04:00[America/New_York]
        2025-04-01T09:00:55-04:00[America/New_York]
        ",
        );
    }

    // HOURLY frequency tests.

    #[test]
    fn hourly_restrictions() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20260401T090000[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(24)
            .by_month(2)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2026-02-01T09:00:00-05:00[America/New_York]
        2026-02-02T09:00:00-05:00[America/New_York]
        2026-02-03T09:00:00-05:00[America/New_York]
        2026-02-04T09:00:00-05:00[America/New_York]
        2026-02-05T09:00:00-05:00[America/New_York]
        2026-02-06T09:00:00-05:00[America/New_York]
        2026-02-07T09:00:00-05:00[America/New_York]
        2026-02-08T09:00:00-05:00[America/New_York]
        2026-02-09T09:00:00-05:00[America/New_York]
        2026-02-10T09:00:00-05:00[America/New_York]
        2026-02-11T09:00:00-05:00[America/New_York]
        2026-02-12T09:00:00-05:00[America/New_York]
        2026-02-13T09:00:00-05:00[America/New_York]
        2026-02-14T09:00:00-05:00[America/New_York]
        2026-02-15T09:00:00-05:00[America/New_York]
        2026-02-16T09:00:00-05:00[America/New_York]
        2026-02-17T09:00:00-05:00[America/New_York]
        2026-02-18T09:00:00-05:00[America/New_York]
        2026-02-19T09:00:00-05:00[America/New_York]
        2026-02-20T09:00:00-05:00[America/New_York]
        2026-02-21T09:00:00-05:00[America/New_York]
        2026-02-22T09:00:00-05:00[America/New_York]
        2026-02-23T09:00:00-05:00[America/New_York]
        2026-02-24T09:00:00-05:00[America/New_York]
        2026-02-25T09:00:00-05:00[America/New_York]
        2026-02-26T09:00:00-05:00[America/New_York]
        2026-02-27T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(24)
            .by_year_day([40..=44, 60..=64])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2026-02-09T09:00:00-05:00[America/New_York]
        2026-02-10T09:00:00-05:00[America/New_York]
        2026-02-11T09:00:00-05:00[America/New_York]
        2026-02-12T09:00:00-05:00[America/New_York]
        2026-02-13T09:00:00-05:00[America/New_York]
        2026-03-01T09:00:00-05:00[America/New_York]
        2026-03-02T09:00:00-05:00[America/New_York]
        2026-03-03T09:00:00-05:00[America/New_York]
        2026-03-04T09:00:00-05:00[America/New_York]
        2026-03-05T09:00:00-05:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(24)
            .by_month_day(31)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(24)
            .by_month_day(-1)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-11-30T09:00:00-05:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        // This is a weird rule that tests what happens when we combine
        // positive and negative BYMONTHDAY.
        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(24)
            .by_month_day([31, -1])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-11-30T09:00:00-05:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(24)
            .by_month_day(-1)
            .by_week_day(Weekday::Monday..=Weekday::Wednesday)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .interval(23)
            .by_month_day(-1)
            .by_week_day(Weekday::Monday..=Weekday::Wednesday)
            .by_hour([3, 11])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T03:00:00-04:00[America/New_York]
        2025-06-30T11:00:00-04:00[America/New_York]
        2025-09-30T11:00:00-04:00[America/New_York]
        2025-12-31T11:00:00-05:00[America/New_York]
        ",
        );
    }

    #[test]
    fn hourly_times() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20250401T130000[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:00-04:00[America/New_York]
        2025-04-01T10:00:00-04:00[America/New_York]
        2025-04-01T11:00:00-04:00[America/New_York]
        2025-04-01T12:00:00-04:00[America/New_York]
        2025-04-01T13:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_hour(10)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @"2025-04-01T10:00:00-04:00[America/New_York]",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_hour([10, 12])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T10:00:00-04:00[America/New_York]
        2025-04-01T12:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_hour([12, 10])
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T10:15:00-04:00[America/New_York]
        2025-04-01T10:55:00-04:00[America/New_York]
        2025-04-01T12:15:00-04:00[America/New_York]
        2025-04-01T12:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_hour([12, 10])
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T10:15:10-04:00[America/New_York]
        2025-04-01T10:15:55-04:00[America/New_York]
        2025-04-01T10:55:10-04:00[America/New_York]
        2025-04-01T10:55:55-04:00[America/New_York]
        2025-04-01T12:15:10-04:00[America/New_York]
        2025-04-01T12:15:55-04:00[America/New_York]
        2025-04-01T12:55:10-04:00[America/New_York]
        2025-04-01T12:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_minute([15, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:15:00-04:00[America/New_York]
        2025-04-01T09:55:00-04:00[America/New_York]
        2025-04-01T10:15:00-04:00[America/New_York]
        2025-04-01T10:55:00-04:00[America/New_York]
        2025-04-01T11:15:00-04:00[America/New_York]
        2025-04-01T11:55:00-04:00[America/New_York]
        2025-04-01T12:15:00-04:00[America/New_York]
        2025-04-01T12:55:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_minute([15, 55])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:15:10-04:00[America/New_York]
        2025-04-01T09:15:55-04:00[America/New_York]
        2025-04-01T09:55:10-04:00[America/New_York]
        2025-04-01T09:55:55-04:00[America/New_York]
        2025-04-01T10:15:10-04:00[America/New_York]
        2025-04-01T10:15:55-04:00[America/New_York]
        2025-04-01T10:55:10-04:00[America/New_York]
        2025-04-01T10:55:55-04:00[America/New_York]
        2025-04-01T11:15:10-04:00[America/New_York]
        2025-04-01T11:15:55-04:00[America/New_York]
        2025-04-01T11:55:10-04:00[America/New_York]
        2025-04-01T11:55:55-04:00[America/New_York]
        2025-04-01T12:15:10-04:00[America/New_York]
        2025-04-01T12:15:55-04:00[America/New_York]
        2025-04-01T12:55:10-04:00[America/New_York]
        2025-04-01T12:55:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_hour([10, 12])
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T10:00:10-04:00[America/New_York]
        2025-04-01T10:00:55-04:00[America/New_York]
        2025-04-01T12:00:10-04:00[America/New_York]
        2025-04-01T12:00:55-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Hourly, start.clone())
            .by_second([10, 55])
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:10-04:00[America/New_York]
        2025-04-01T09:00:55-04:00[America/New_York]
        2025-04-01T10:00:10-04:00[America/New_York]
        2025-04-01T10:00:55-04:00[America/New_York]
        2025-04-01T11:00:10-04:00[America/New_York]
        2025-04-01T11:00:55-04:00[America/New_York]
        2025-04-01T12:00:10-04:00[America/New_York]
        2025-04-01T12:00:55-04:00[America/New_York]
        ",
        );
    }

    // MINUTELY frequency tests.

    #[test]
    fn minutely_restrictions() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20260401T090000[America/New_York]");

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1440)
                .by_month(2)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2026-02-01T09:00:00-05:00[America/New_York]
        2026-02-02T09:00:00-05:00[America/New_York]
        2026-02-03T09:00:00-05:00[America/New_York]
        2026-02-04T09:00:00-05:00[America/New_York]
        2026-02-05T09:00:00-05:00[America/New_York]
        2026-02-06T09:00:00-05:00[America/New_York]
        2026-02-07T09:00:00-05:00[America/New_York]
        2026-02-08T09:00:00-05:00[America/New_York]
        2026-02-09T09:00:00-05:00[America/New_York]
        2026-02-10T09:00:00-05:00[America/New_York]
        2026-02-11T09:00:00-05:00[America/New_York]
        2026-02-12T09:00:00-05:00[America/New_York]
        2026-02-13T09:00:00-05:00[America/New_York]
        2026-02-14T09:00:00-05:00[America/New_York]
        2026-02-15T09:00:00-05:00[America/New_York]
        2026-02-16T09:00:00-05:00[America/New_York]
        2026-02-17T09:00:00-05:00[America/New_York]
        2026-02-18T09:00:00-05:00[America/New_York]
        2026-02-19T09:00:00-05:00[America/New_York]
        2026-02-20T09:00:00-05:00[America/New_York]
        2026-02-21T09:00:00-05:00[America/New_York]
        2026-02-22T09:00:00-05:00[America/New_York]
        2026-02-23T09:00:00-05:00[America/New_York]
        2026-02-24T09:00:00-05:00[America/New_York]
        2026-02-25T09:00:00-05:00[America/New_York]
        2026-02-26T09:00:00-05:00[America/New_York]
        2026-02-27T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1440)
                .by_year_day([40..=44, 60..=64])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2026-02-09T09:00:00-05:00[America/New_York]
        2026-02-10T09:00:00-05:00[America/New_York]
        2026-02-11T09:00:00-05:00[America/New_York]
        2026-02-12T09:00:00-05:00[America/New_York]
        2026-02-13T09:00:00-05:00[America/New_York]
        2026-03-01T09:00:00-05:00[America/New_York]
        2026-03-02T09:00:00-05:00[America/New_York]
        2026-03-03T09:00:00-05:00[America/New_York]
        2026-03-04T09:00:00-05:00[America/New_York]
        2026-03-05T09:00:00-05:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1440)
                .by_month_day(31)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1440)
                .by_month_day(-1)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-11-30T09:00:00-05:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        // This is a weird rule that tests what happens when we combine
        // positive and negative BYMONTHDAY.
        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1440)
                .by_month_day([31, -1])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-11-30T09:00:00-05:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1440)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Wednesday)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1399)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Wednesday)
                .by_hour([8, 12])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T12:30:00-04:00[America/New_York]
        2025-12-31T08:18:00-05:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .interval(1399)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Wednesday)
                .by_minute([13, 30, 45])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T12:30:00-04:00[America/New_York]
        2025-09-30T01:13:00-04:00[America/New_York]
        2026-03-31T16:45:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn minutely_times() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20250401T090500[America/New_York]");

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:00-04:00[America/New_York]
        2025-04-01T09:01:00-04:00[America/New_York]
        2025-04-01T09:02:00-04:00[America/New_York]
        2025-04-01T09:03:00-04:00[America/New_York]
        2025-04-01T09:04:00-04:00[America/New_York]
        2025-04-01T09:05:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .by_minute([2, 4])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:02:00-04:00[America/New_York]
        2025-04-01T09:04:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .by_minute([2, 4])
                .by_second([10, 55])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:02:10-04:00[America/New_York]
        2025-04-01T09:02:55-04:00[America/New_York]
        2025-04-01T09:04:10-04:00[America/New_York]
        2025-04-01T09:04:55-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .by_second([10, 55])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:10-04:00[America/New_York]
        2025-04-01T09:00:55-04:00[America/New_York]
        2025-04-01T09:01:10-04:00[America/New_York]
        2025-04-01T09:01:55-04:00[America/New_York]
        2025-04-01T09:02:10-04:00[America/New_York]
        2025-04-01T09:02:55-04:00[America/New_York]
        2025-04-01T09:03:10-04:00[America/New_York]
        2025-04-01T09:03:55-04:00[America/New_York]
        2025-04-01T09:04:10-04:00[America/New_York]
        2025-04-01T09:04:55-04:00[America/New_York]
        ",
        );
    }

    // SECONDLY frequency tests.

    #[test]
    fn secondly_restrictions() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20260401T090000[America/New_York]");

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86400)
                .by_month(2)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2026-02-01T09:00:00-05:00[America/New_York]
        2026-02-02T09:00:00-05:00[America/New_York]
        2026-02-03T09:00:00-05:00[America/New_York]
        2026-02-04T09:00:00-05:00[America/New_York]
        2026-02-05T09:00:00-05:00[America/New_York]
        2026-02-06T09:00:00-05:00[America/New_York]
        2026-02-07T09:00:00-05:00[America/New_York]
        2026-02-08T09:00:00-05:00[America/New_York]
        2026-02-09T09:00:00-05:00[America/New_York]
        2026-02-10T09:00:00-05:00[America/New_York]
        2026-02-11T09:00:00-05:00[America/New_York]
        2026-02-12T09:00:00-05:00[America/New_York]
        2026-02-13T09:00:00-05:00[America/New_York]
        2026-02-14T09:00:00-05:00[America/New_York]
        2026-02-15T09:00:00-05:00[America/New_York]
        2026-02-16T09:00:00-05:00[America/New_York]
        2026-02-17T09:00:00-05:00[America/New_York]
        2026-02-18T09:00:00-05:00[America/New_York]
        2026-02-19T09:00:00-05:00[America/New_York]
        2026-02-20T09:00:00-05:00[America/New_York]
        2026-02-21T09:00:00-05:00[America/New_York]
        2026-02-22T09:00:00-05:00[America/New_York]
        2026-02-23T09:00:00-05:00[America/New_York]
        2026-02-24T09:00:00-05:00[America/New_York]
        2026-02-25T09:00:00-05:00[America/New_York]
        2026-02-26T09:00:00-05:00[America/New_York]
        2026-02-27T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86400)
                .by_year_day([40..=44, 60..=64])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2026-02-09T09:00:00-05:00[America/New_York]
        2026-02-10T09:00:00-05:00[America/New_York]
        2026-02-11T09:00:00-05:00[America/New_York]
        2026-02-12T09:00:00-05:00[America/New_York]
        2026-02-13T09:00:00-05:00[America/New_York]
        2026-03-01T09:00:00-05:00[America/New_York]
        2026-03-02T09:00:00-05:00[America/New_York]
        2026-03-03T09:00:00-05:00[America/New_York]
        2026-03-04T09:00:00-05:00[America/New_York]
        2026-03-05T09:00:00-05:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86400)
                .by_month_day(31)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86400)
                .by_month_day(-1)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-11-30T09:00:00-05:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        // This is a weird rule that tests what happens when we combine
        // positive and negative BYMONTHDAY.
        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86400)
                .by_month_day([31, -1])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-05-31T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-07-31T09:00:00-04:00[America/New_York]
        2025-08-31T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-10-31T09:00:00-04:00[America/New_York]
        2025-11-30T09:00:00-05:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-01-31T09:00:00-05:00[America/New_York]
        2026-02-28T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86400)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Wednesday)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T09:00:00-04:00[America/New_York]
        2025-06-30T09:00:00-04:00[America/New_York]
        2025-09-30T09:00:00-04:00[America/New_York]
        2025-12-31T09:00:00-05:00[America/New_York]
        2026-03-31T09:00:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86399)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Friday)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T08:59:31-04:00[America/New_York]
        2025-06-30T08:58:30-04:00[America/New_York]
        2025-07-31T08:57:59-04:00[America/New_York]
        2025-09-30T08:56:58-04:00[America/New_York]
        2025-10-31T08:56:27-04:00[America/New_York]
        2025-12-31T08:55:26-05:00[America/New_York]
        2026-03-31T08:53:56-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(1440)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Friday)
                .by_hour(8)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T08:12:00-04:00[America/New_York]
        2025-04-30T08:36:00-04:00[America/New_York]
        2025-06-30T08:12:00-04:00[America/New_York]
        2025-06-30T08:36:00-04:00[America/New_York]
        2025-07-31T08:12:00-04:00[America/New_York]
        2025-07-31T08:36:00-04:00[America/New_York]
        2025-09-30T08:12:00-04:00[America/New_York]
        2025-09-30T08:36:00-04:00[America/New_York]
        2025-10-31T08:12:00-04:00[America/New_York]
        2025-10-31T08:36:00-04:00[America/New_York]
        2025-12-31T08:12:00-05:00[America/New_York]
        2025-12-31T08:36:00-05:00[America/New_York]
        2026-03-31T08:12:00-04:00[America/New_York]
        2026-03-31T08:36:00-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86399)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Friday)
                .by_minute(57..=59)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T08:59:31-04:00[America/New_York]
        2025-06-30T08:58:30-04:00[America/New_York]
        2025-07-31T08:57:59-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .interval(86399)
                .by_month_day(-1)
                .by_week_day(Weekday::Monday..=Weekday::Friday)
                .by_minute(57..=59)
                .by_second(30..=31)
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-30T08:59:31-04:00[America/New_York]
        2025-06-30T08:58:30-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn secondly_times() {
        let start = zoned("20250401T090000[America/New_York]");
        let until = zoned("20250401T090005[America/New_York]");

        let rrule =
            RecurrenceRule::builder(Frequency::Secondly, start.clone())
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:00-04:00[America/New_York]
        2025-04-01T09:00:01-04:00[America/New_York]
        2025-04-01T09:00:02-04:00[America/New_York]
        2025-04-01T09:00:03-04:00[America/New_York]
        2025-04-01T09:00:04-04:00[America/New_York]
        2025-04-01T09:00:05-04:00[America/New_York]
        ",
        );

        let rrule =
            RecurrenceRule::builder(Frequency::Minutely, start.clone())
                .by_second([2, 4])
                .build()
                .unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take_while(|zdt| zdt <= until)),
            @r"
        2025-04-01T09:00:02-04:00[America/New_York]
        2025-04-01T09:00:04-04:00[America/New_York]
        ",
        );
    }

    // More of my own tests for dealing with time zone transitions.

    #[test]
    fn every_day_but_not_dst_gaps() {
        let start = zoned("20250307T023000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-03-07T02:30:00-05:00[America/New_York]
        2025-03-08T02:30:00-05:00[America/New_York]
        2025-03-10T02:30:00-04:00[America/New_York]
        2025-03-11T02:30:00-04:00[America/New_York]
        2025-03-12T02:30:00-04:00[America/New_York]
        ",
        );
    }

    #[test]
    fn every_day_but_duplicate_dst_folds() {
        let start = zoned("20251031T013000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(5)),
            @r"
        2025-10-31T01:30:00-04:00[America/New_York]
        2025-11-01T01:30:00-04:00[America/New_York]
        2025-11-02T01:30:00-04:00[America/New_York]
        2025-11-02T01:30:00-05:00[America/New_York]
        2025-11-03T01:30:00-05:00[America/New_York]
        ",
        );
    }

    /// This tests the case where the starting point is inside a fold. We
    /// ensure that datetimes less than the start aren't yielded and that
    /// duplicates are handled correctly.
    #[test]
    fn start_in_fold() {
        let start = zoned("20251102T013000[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(4)),
            @r"
        2025-11-02T01:30:00-04:00[America/New_York]
        2025-11-02T01:30:00-05:00[America/New_York]
        2025-11-03T01:30:00-05:00[America/New_York]
        2025-11-04T01:30:00-05:00[America/New_York]
        ",
        );

        // This is the same starting point as above, but is explicitly
        // spelled out. The "compatible" disambiguation strategy is used
        // above, which results in selecting the first 01:30.
        let start = zoned("20251102T013000-04[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(4)),
            @r"
        2025-11-02T01:30:00-04:00[America/New_York]
        2025-11-02T01:30:00-05:00[America/New_York]
        2025-11-03T01:30:00-05:00[America/New_York]
        2025-11-04T01:30:00-05:00[America/New_York]
        ",
        );

        // This starts one hour later than both times above!
        let start = zoned("20251102T013000-05[America/New_York]");
        let rrule =
            RecurrenceRule::builder(Frequency::Daily, start).build().unwrap();
        insta::assert_snapshot!(
            snapshot(rrule.iter().take(4)),
            @r"
        2025-11-02T01:30:00-05:00[America/New_York]
        2025-11-03T01:30:00-05:00[America/New_York]
        2025-11-04T01:30:00-05:00[America/New_York]
        2025-11-05T01:30:00-05:00[America/New_York]
        ",
        );
    }

    // Other miscellaneous tests.

    /// Ensures that our `until` functionality is working correctly.
    ///
    /// This was a regression test I stumbled over when, if I commented out
    /// one of the `until` checks, none of the other tests failed. So I set
    /// out to write a test to make that case fail.
    #[test]
    fn tricky_until() {
        let start = zoned("20250513T000000[America/New_York]");
        let until = zoned("20250514T000000[America/New_York]");
        let rrule = RecurrenceRule::builder(Frequency::Yearly, start)
            .until(until)
            .by_month_day(10)
            .build()
            .unwrap();
        // The regression here is that this would actually emit some datetimes
        // within the first interval, but then stop. But in reality, this
        // rule shouldn't emit any datetimes.
        insta::assert_snapshot!(
            snapshot(&rrule),
            @"",
        );
    }

    /// Some various BYSETPOS test cases. (There are very few in RFC 5545.)
    #[test]
    fn bysetpos() {
        let start = zoned("20250501T000000[America/New_York]");
        let until = zoned("20250531T235959[America/New_York]");

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .until(until.clone())
            .by_hour(9..=23)
            .by_set_position(3..=5)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(&rrule),
            @r"
        2025-05-01T11:00:00-04:00[America/New_York]
        2025-05-01T12:00:00-04:00[America/New_York]
        2025-05-01T13:00:00-04:00[America/New_York]
        ",
        );

        let rrule = RecurrenceRule::builder(Frequency::Yearly, start.clone())
            .until(until.clone())
            .by_hour(9..=23)
            .by_set_position(-5..=-3)
            .build()
            .unwrap();
        insta::assert_snapshot!(
            snapshot(&rrule),
            @r"
        2025-05-01T19:00:00-04:00[America/New_York]
        2025-05-01T20:00:00-04:00[America/New_York]
        2025-05-01T21:00:00-04:00[America/New_York]
        ",
        );
    }

    // The tests below check the error cases for rule construction.

    /// Checks that interval values are legal.
    #[test]
    fn interval_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).interval(0),
        );
        insta::assert_snapshot!(
            err,
            @"interval value of `0` is invalid (interval must be greater than or equal to 1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).interval(-1),
        );
        insta::assert_snapshot!(
            err,
            @"interval value of `-1` is invalid (interval must be greater than or equal to 1)",
        );
    }

    /// Checks that BYMONTH values are legal.
    #[test]
    fn by_month_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_month(0),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by month' value `0` (values must be in range 1..=12)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_month(-1),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by month' value `-1` (values must be in range 1..=12)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_month(13),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by month' value `13` (values must be in range 1..=12)",
        );
    }

    /// Checks that BYWEEKNO values are legal.
    #[test]
    fn by_week_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_week(0),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by week' value `0` (values must be in range 1..=53 or -53..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_week(54),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by week' value `54` (values must be in range 1..=53 or -53..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_week(-54),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by week' value `-54` (values must be in range 1..=53 or -53..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now()).by_week(1),
        );
        insta::assert_snapshot!(
            err,
            @"'by week' cannot be used with any frequency except yearly",
        );
    }

    /// Checks that BYYEARDAY values are legal.
    #[test]
    fn by_year_day_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_year_day(0),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by day of the year' value `0` (values must be in range 1..=366 or -366..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_year_day(-367),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by day of the year' value `-367` (values must be in range 1..=366 or -366..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_year_day(367),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by day of the year' value `367` (values must be in range 1..=366 or -366..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now()).by_year_day(1),
        );
        insta::assert_snapshot!(
            err,
            @"'by day of the year' cannot be used with monthly, weekly or daily frequency",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Weekly, now()).by_year_day(1),
        );
        insta::assert_snapshot!(
            err,
            @"'by day of the year' cannot be used with monthly, weekly or daily frequency",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Daily, now()).by_year_day(1),
        );
        insta::assert_snapshot!(
            err,
            @"'by day of the year' cannot be used with monthly, weekly or daily frequency",
        );
    }

    /// Checks that BYMONTHDAY values are legal.
    #[test]
    fn by_month_day_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_month_day(0),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by day of the month' value `0` (values must be in range 1..=31 or -31..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_month_day(-32),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by day of the month' value `-32` (values must be in range 1..=31 or -31..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_month_day(32),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by day of the month' value `32` (values must be in range 1..=31 or -31..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Weekly, now()).by_month_day(1),
        );
        insta::assert_snapshot!(
            err,
            @"'by day of the month' cannot be used with weekly frequency",
        );
    }

    /// Checks that BYDAY values are legal.
    #[test]
    fn by_week_day_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Daily, now())
                .by_week_day((1, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"numbered weekday `1-Mon` is only allowed at yearly or monthly frequencies",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_week_day((1, Weekday::Monday))
                .by_week(20),
        );
        insta::assert_snapshot!(
            err,
            @"numbered weekday `1-Mon` is only allowed at yearly frequency when 'by week' is not used",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_week_day((0, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `0-Mon` (values must be in range 1..=53 or -53..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_week_day((-54, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `-54-Mon` (values must be in range 1..=53 or -53..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_week_day((54, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `54-Mon` (values must be in range 1..=53 or -53..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_month(5)
                .by_week_day((0, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `0-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_month(5)
                .by_week_day((-54, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `-54-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_month(5)
                .by_week_day((54, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `54-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_month(5)
                .by_week_day((-6, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `-6-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_month(5)
                .by_week_day((6, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `6-Mon` (values must be in range 1..=5 or -5..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now())
                .by_week_day((0, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `0-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now())
                .by_week_day((-54, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `-54-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now())
                .by_week_day((54, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `54-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now())
                .by_week_day((-6, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `-6-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Monthly, now())
                .by_week_day((6, Weekday::Monday)),
        );
        insta::assert_snapshot!(
            err,
            @"invalid numbered 'by week day' value `6-Mon` (values must be in range 1..=5 or -5..=-1)",
        );
    }

    /// Checks that BYHOUR values are legal.
    #[test]
    fn by_hour_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_hour(-1),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by hour' value `-1` (values must be in range 0..=23)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_hour(24),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by hour' value `24` (values must be in range 0..=23)",
        );
    }

    /// Checks that BYMINUTE values are legal.
    #[test]
    fn by_minute_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_minute(-1),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by minute' value `-1` (values must be in range 0..=59)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_minute(60),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by minute' value `60` (values must be in range 0..=59)",
        );
    }

    /// Checks that BYSECOND values are legal.
    #[test]
    fn by_second_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_second(-1),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by second' value `-1` (values must be in range 0..=59)",
        );

        // We don't support leap seconds.
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_second(60),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by second' value `60` (values must be in range 0..=59)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now()).by_second(61),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by second' value `61` (values must be in range 0..=59)",
        );
    }

    /// Checks that BYSETPOS values are legal.
    #[test]
    fn by_set_position_errors() {
        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_hour(9)
                .by_set_position(0),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by set position' value `0` (values must be in range 1..=366 or -366..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_hour(9)
                .by_set_position(-367),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by set position' value `-367` (values must be in range 1..=366 or -366..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_hour(9)
                .by_set_position(367),
        );
        insta::assert_snapshot!(
            err,
            @"invalid 'by set position' value `367` (values must be in range 1..=366 or -366..=-1)",
        );

        let err = expect_err(
            RecurrenceRule::builder(Frequency::Yearly, now())
                .by_set_position(1),
        );
        insta::assert_snapshot!(
            err,
            @"when 'by set position' is used, at least one other 'by' rule must be specified, but all are empty",
        );
    }

    fn now() -> ZonedDateTime {
        crate::NOW.clone()
    }

    fn zoned(s: &str) -> ZonedDateTime {
        s.parse().unwrap()
    }

    fn expect_err(builder: &mut RecurrenceRuleBuilder) -> anyhow::Error {
        match builder.build() {
            Err(err) => err,
            Ok(ok) => {
                panic!("expected recurrence rule error, but got:\n{ok:?}",)
            }
        }
    }

    fn snapshot<T>(it: impl IntoIterator<Item = T>) -> String
    where
        T: ToString,
    {
        it.into_iter()
            .map(|item| item.to_string())
            .collect::<Vec<String>>()
            .join("\n")
    }
}
