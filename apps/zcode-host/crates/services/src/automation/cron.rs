//! croner 10 的移植（automation 只用它“解析 + 算下次 / 上次触发时间”，见 automationCron.ts）。
//!
//! 只覆盖 `new Cron(pattern)` 的默认选项：`mode: "auto"`（5、6、7 段）、本地时区、
//! `legacyMode`（日与星期都受限时取 OR）、不带 interval / startAt / stopAt。语义逐行对照
//! `node_modules/croner/dist/croner.js` 的 `CronPattern`、`CronDate` 与 `Cron`。含 `:` 的字符串
//! 与 croner 一样当作一次性时间（ISO 日期时间）。
use super::time::{LocalFields, Zone, parse_int_prefix, parse_iso_datetime, utc_fields, utc_make};

const DAYS_IN_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const NTH_BITS: [u8; 5] = [1, 2, 4, 8, 16];
const ANY_NTH: u8 = 63;
const LAST_NTH: u8 = 32;
const YEARS: usize = 10_000;
/// croner 在 `recurse(…, r - 1)` 于第 0 层时越界读 `f[-1][0]`，JS 抛出这条 TypeError；原样复现。
pub const CRONER_LEVEL_ERROR: &str = "Cannot read properties of undefined (reading '0')";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Second,
    Minute,
    Hour,
    Day,
    Month,
    DayOfWeek,
    Year,
    NearestWeekdays,
}

/// `CronPattern`。
#[derive(Debug, Clone)]
pub struct Pattern {
    second: [u8; 60],
    minute: [u8; 60],
    hour: [u8; 24],
    day: [u8; 31],
    month: [u8; 12],
    day_of_week: [u8; 7],
    year: Vec<u8>,
    last_day_of_month: bool,
    last_weekday: bool,
    nearest_weekdays: [u8; 31],
    star_dom: bool,
    star_dow: bool,
    star_year: bool,
    use_and_logic: bool,
}

type ParseResult<T> = Result<T, String>;

fn err<T>(message: impl Into<String>) -> ParseResult<T> {
    Err(message.into())
}

fn replace_ci(text: &str, from: &str, to: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(found) = lower[index..].find(from) {
        out.push_str(&text[index..index + found]);
        out.push_str(to);
        index += found + from.len();
    }
    out.push_str(&text[index..]);
    out
}

fn replace_alpha_months(text: &str) -> String {
    let names = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    names
        .iter()
        .enumerate()
        .fold(text.to_owned(), |text, (index, name)| {
            replace_ci(&text, name, &(index + 1).to_string())
        })
}

fn replace_alpha_days(text: &str) -> String {
    let text = replace_ci(text, "-sun", "-7");
    ["sun", "mon", "tue", "wed", "thu", "fri", "sat"]
        .iter()
        .enumerate()
        .fold(text, |text, (index, name)| {
            replace_ci(&text, name, &index.to_string())
        })
}

fn handle_nicknames(pattern: &str) -> ParseResult<String> {
    Ok(match pattern.trim().to_lowercase().as_str() {
        "@yearly" | "@annually" => "0 0 1 1 *".into(),
        "@monthly" => "0 0 1 * *".into(),
        "@weekly" => "0 0 * * 0".into(),
        "@daily" | "@midnight" => "0 0 * * *".into(),
        "@hourly" => "0 * * * *".into(),
        "@reboot" => return err("CronPattern: @reboot is not supported in this environment."),
        _ => pattern.to_owned(),
    })
}

/// JS `i[1] || n`：`#n` / `L` 的原始文本；缺省（含空串）时取字段默认值（星期 63，其余 1）。
#[derive(Debug, Clone, PartialEq)]
enum Nth {
    Default,
    Text(String),
}

impl Nth {
    fn or_default(self) -> Nth {
        match self {
            Nth::Text(text) if text.is_empty() => Nth::Default,
            other => other,
        }
    }
}

impl Pattern {
    pub fn parse(source: &str) -> ParseResult<Self> {
        let mut pattern = source.to_owned();
        if pattern.contains('@') {
            pattern = handle_nicknames(&pattern)?.trim().to_owned();
        }
        let mut parts: Vec<String> = pattern.split_whitespace().map(str::to_owned).collect();
        if parts.is_empty() {
            parts.push(String::new());
        }
        if parts.len() < 5 || parts.len() > 7 {
            return err("CronPattern: invalid configuration format");
        }
        let mut this = Self {
            second: [0; 60],
            minute: [0; 60],
            hour: [0; 24],
            day: [0; 31],
            month: [0; 12],
            day_of_week: [0; 7],
            year: vec![0; YEARS],
            last_day_of_month: false,
            last_weekday: false,
            nearest_weekdays: [0; 31],
            star_dom: false,
            star_dow: false,
            star_year: false,
            use_and_logic: false,
        };
        if parts.len() == 5 {
            parts.insert(0, "0".into());
        }
        if parts.len() == 6 {
            parts.push("*".into());
        }
        if parts[3].to_uppercase() == "LW" {
            this.last_weekday = true;
            parts[3] = String::new();
        } else if parts[3].to_uppercase().contains('L') {
            parts[3] = parts[3].replace(['L', 'l'], "");
            this.last_day_of_month = true;
        }
        if parts[3] == "*" {
            this.star_dom = true;
        }
        if parts[6] == "*" {
            this.star_year = true;
        }
        if parts[4].len() >= 3 {
            parts[4] = replace_alpha_months(&parts[4]);
        }
        if parts[5].len() >= 3 {
            parts[5] = replace_alpha_days(&parts[5]);
        }
        if let Some(rest) = parts[5].strip_prefix('+') {
            this.use_and_logic = true;
            parts[5] = rest.to_owned();
            if parts[5].is_empty() {
                return err("CronPattern: Day-of-week field cannot be empty after '+' modifier.");
            }
        }
        if parts[5] == "*" {
            this.star_dow = true;
        }
        if pattern.contains('?') {
            for part in &mut parts {
                *part = part.replace('?', "*");
            }
        }
        for (index, part) in parts.iter().enumerate() {
            let allowed = |byte: u8| {
                byte.is_ascii_digit()
                    || matches!(byte, b'/' | b'*' | b',' | b'-')
                    || (index == 3 && matches!(byte, b'W' | b'w' | b'L' | b'l'))
                    || (index == 5 && matches!(byte, b'#' | b'L' | b'l'))
            };
            if !part.bytes().all(allowed) {
                return err("CronPattern: configuration entry contains illegal characters.");
            }
        }
        this.part_to_array(Field::Second, &parts[0], 0, 1)?;
        this.part_to_array(Field::Minute, &parts[1], 0, 1)?;
        this.part_to_array(Field::Hour, &parts[2], 0, 1)?;
        this.part_to_array(Field::Day, &parts[3], -1, 1)?;
        this.part_to_array(Field::Month, &parts[4], -1, 1)?;
        this.part_to_array(Field::DayOfWeek, &parts[5], 0, ANY_NTH)?;
        this.part_to_array(Field::Year, &parts[6], 0, 1)?;
        Ok(this)
    }

    fn slots(&mut self, field: Field) -> &mut [u8] {
        match field {
            Field::Second => &mut self.second,
            Field::Minute => &mut self.minute,
            Field::Hour => &mut self.hour,
            Field::Day => &mut self.day,
            Field::Month => &mut self.month,
            Field::DayOfWeek => &mut self.day_of_week,
            Field::Year => &mut self.year,
            Field::NearestWeekdays => &mut self.nearest_weekdays,
        }
    }

    fn len(&mut self, field: Field) -> i64 {
        self.slots(field).len() as i64
    }

    fn part_to_array(
        &mut self,
        field: Field,
        text: &str,
        offset: i64,
        value: u8,
    ) -> ParseResult<()> {
        let day_l = field == Field::Day && (self.last_day_of_month || self.last_weekday);
        if text.is_empty() && !day_l {
            return err("CronPattern: configuration entry is empty, check for trailing spaces.");
        }
        if text == "*" {
            self.slots(field).fill(value);
            return Ok(());
        }
        let pieces: Vec<&str> = text.split(',').collect();
        if pieces.len() > 1 {
            for piece in pieces {
                self.part_to_array(field, piece, offset, value)?;
            }
            return Ok(());
        }
        match (text.contains('-'), text.contains('/')) {
            (true, true) => self.handle_range_with_stepping(text, field, offset, value),
            (true, false) => self.handle_range(text, field, offset, value),
            (false, true) => self.handle_stepping(text, field, offset, value),
            (false, false) if !text.is_empty() => self.handle_number(text, field, offset, value),
            _ => Ok(()),
        }
    }

    fn extract_nth(text: &str, field: Field) -> ParseResult<(String, Nth)> {
        if text.contains('#') {
            if field != Field::DayOfWeek {
                return err("CronPattern: nth (#) only allowed in day-of-week field");
            }
            let mut split = text.split('#');
            let base = split.next().unwrap_or_default().to_owned();
            let nth = split.next().unwrap_or_default().to_owned();
            return Ok((base, Nth::Text(nth)));
        }
        if text.to_uppercase().ends_with('L') {
            if field != Field::DayOfWeek {
                return err("CronPattern: L modifier only allowed in day-of-week field");
            }
            return Ok((text[..text.len() - 1].to_owned(), Nth::Text("L".into())));
        }
        Ok((text.to_owned(), Nth::Default))
    }

    fn handle_number(
        &mut self,
        text: &str,
        field: Field,
        offset: i64,
        value: u8,
    ) -> ParseResult<()> {
        let (base, nth) = Self::extract_nth(text, field)?;
        let nearest = text.to_uppercase().contains('W');
        if field != Field::Day && nearest {
            return err("CronPattern: Nearest weekday modifier (W) only allowed in day-of-month.");
        }
        let field = if nearest {
            Field::NearestWeekdays
        } else {
            field
        };
        let number = parse_int_prefix(&base)
            .ok_or_else(|| "CronPattern: value is not a number".to_owned())?
            + offset;
        let _ = value;
        self.set_part(field, number, &nth.or_default())
    }

    fn set_part(&mut self, field: Field, number: i64, nth: &Nth) -> ParseResult<()> {
        if field == Field::DayOfWeek {
            let number = if number == 7 { 0 } else { number };
            if !(0..=6).contains(&number) {
                return err("CronPattern: Invalid value for dayOfWeek");
            }
            return self.set_nth_weekday_of_month(number as usize, nth);
        }
        let valid = match field {
            Field::Second | Field::Minute => (0..60).contains(&number),
            Field::Hour => (0..24).contains(&number),
            Field::Day | Field::NearestWeekdays => (0..31).contains(&number),
            Field::Month => (0..12).contains(&number),
            Field::Year => (1..10_000).contains(&number),
            Field::DayOfWeek => true,
        };
        if !valid {
            return err("CronPattern: Invalid value");
        }
        // 非星期字段不可能带 `#` / `L`（extract_nth 已拒绝），取值恒为 1。
        self.slots(field)[number as usize] = 1;
        Ok(())
    }

    fn set_nth_weekday_of_month(&mut self, weekday: usize, nth: &Nth) -> ParseResult<()> {
        let text = match nth {
            Nth::Default => {
                self.day_of_week[weekday] = ANY_NTH;
                return Ok(());
            }
            Nth::Text(text) => text,
        };
        if text.eq_ignore_ascii_case("l") {
            self.day_of_week[weekday] |= LAST_NTH;
            return Ok(());
        }
        // JS 用 `t < 6 && t > 0` 隐式把字符串转成数字（"3"、"03"）；非法字符已在解析前拦下。
        match text.parse::<u32>() {
            Ok(number @ 1..=5) => {
                self.day_of_week[weekday] |= NTH_BITS[number as usize - 1];
                Ok(())
            }
            _ => err("CronPattern: nth weekday out of range, should be 1-5 or L."),
        }
    }

    fn validate_range(
        &mut self,
        from: i64,
        to: i64,
        step: Option<i64>,
        field: Field,
    ) -> ParseResult<()> {
        if from > to {
            return err("CronPattern: From value is larger than to value");
        }
        if let Some(step) = step {
            if step == 0 {
                return err("CronPattern: Syntax error, illegal stepping: 0");
            }
            if step > self.len(field) {
                return err(
                    "CronPattern: Syntax error, steps cannot be greater than maximum value of part",
                );
            }
        }
        Ok(())
    }

    fn handle_range_with_stepping(
        &mut self,
        text: &str,
        field: Field,
        offset: i64,
        value: u8,
    ) -> ParseResult<()> {
        if text.to_uppercase().contains('W') {
            return err("CronPattern: Syntax error, W is not allowed in ranges with stepping.");
        }
        let (base, nth) = Self::extract_nth(text, field)?;
        let (range, step) = base
            .split_once('/')
            .ok_or_else(|| "CronPattern: Syntax error, illegal range with stepping".to_owned())?;
        let (from, to) = range
            .split_once('-')
            .ok_or_else(|| "CronPattern: Syntax error, illegal range with stepping".to_owned())?;
        let digits =
            |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        if !digits(from) || !digits(to) || !digits(step) {
            return err("CronPattern: Syntax error, illegal range with stepping");
        }
        let from = parse_int_prefix(from).unwrap_or_default() + offset;
        let to = parse_int_prefix(to).unwrap_or_default() + offset;
        let step = parse_int_prefix(step).unwrap_or_default();
        self.validate_range(from, to, Some(step), field)?;
        let _ = value;
        let nth = nth.or_default();
        let mut current = from;
        while current <= to {
            self.set_part(field, current, &nth)?;
            current += step;
        }
        Ok(())
    }

    fn handle_range(
        &mut self,
        text: &str,
        field: Field,
        offset: i64,
        value: u8,
    ) -> ParseResult<()> {
        if text.to_uppercase().contains('W') {
            return err("CronPattern: Syntax error, W is not allowed in a range.");
        }
        let (base, nth) = Self::extract_nth(text, field)?;
        let bounds: Vec<&str> = base.split('-').collect();
        if bounds.len() != 2 {
            return err("CronPattern: Syntax error, illegal range");
        }
        let from = parse_int_prefix(bounds[0])
            .ok_or_else(|| "CronPattern: Syntax error, illegal lower range (NaN)".to_owned())?
            + offset;
        let to = parse_int_prefix(bounds[1])
            .ok_or_else(|| "CronPattern: Syntax error, illegal upper range (NaN)".to_owned())?
            + offset;
        self.validate_range(from, to, None, field)?;
        let _ = value;
        let nth = nth.or_default();
        for current in from..=to {
            self.set_part(field, current, &nth)?;
        }
        Ok(())
    }

    fn handle_stepping(
        &mut self,
        text: &str,
        field: Field,
        offset: i64,
        value: u8,
    ) -> ParseResult<()> {
        if text.to_uppercase().contains('W') {
            return err("CronPattern: Syntax error, W is not allowed in parts with stepping.");
        }
        let (base, nth) = Self::extract_nth(text, field)?;
        let pieces: Vec<&str> = base.split('/').collect();
        if pieces.len() != 2 {
            return err("CronPattern: Syntax error, illegal stepping");
        }
        if pieces[0] != "*" {
            return err("CronPattern: Syntax error, stepping must use a wildcard prefix");
        }
        let _ = offset;
        let step = parse_int_prefix(pieces[1])
            .ok_or_else(|| "CronPattern: Syntax error, illegal stepping: (NaN)".to_owned())?;
        let last = self.len(field) - 1;
        self.validate_range(0, last, Some(step), field)?;
        let _ = value;
        let nth = nth.or_default();
        let mut current = 0;
        while current < self.len(field) {
            self.set_part(field, current, &nth)?;
            current += step;
        }
        Ok(())
    }
}

/// `CronDate`（本地时区：字段即本地日历字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CronDate {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
}

/// `f` 表：(字段, 上一级字段, 偏移)；0 = month，1 = day，2 = hour，3 = minute，4 = second。
const LEVELS: [(Unit, Unit, i64); 5] = [
    (Unit::Month, Unit::Year, 0),
    (Unit::Day, Unit::Month, -1),
    (Unit::Hour, Unit::Day, 0),
    (Unit::Minute, Unit::Hour, 0),
    (Unit::Second, Unit::Minute, 0),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

enum Found {
    Same,
    Changed,
    None,
}

fn last_day_of_month(year: i64, month: i64) -> i64 {
    if month != 1 {
        DAYS_IN_MONTH[month.rem_euclid(12) as usize]
    } else {
        utc_fields(utc_make([year, month + 1, 0, 0, 0, 0, 0])).day
    }
}

fn weekday(year: i64, month: i64, day: i64) -> i64 {
    utc_fields(utc_make([year, month, day, 0, 0, 0, 0])).weekday
}

impl CronDate {
    fn from_fields(fields: LocalFields) -> Self {
        Self {
            year: fields.year,
            month: fields.month,
            day: fields.day,
            hour: fields.hour,
            minute: fields.minute,
            second: fields.second,
            ms: fields.ms,
        }
    }

    fn get(&self, unit: Unit) -> i64 {
        match unit {
            Unit::Year => self.year,
            Unit::Month => self.month,
            Unit::Day => self.day,
            Unit::Hour => self.hour,
            Unit::Minute => self.minute,
            Unit::Second => self.second,
        }
    }

    fn set(&mut self, unit: Unit, value: i64) {
        match unit {
            Unit::Year => self.year = value,
            Unit::Month => self.month = value,
            Unit::Day => self.day = value,
            Unit::Hour => self.hour = value,
            Unit::Minute => self.minute = value,
            Unit::Second => self.second = value,
        }
    }

    fn last_weekday(&self) -> i64 {
        let last = last_day_of_month(self.year, self.month);
        match weekday(self.year, self.month, last) {
            0 => last - 2,
            6 => last - 1,
            _ => last,
        }
    }

    fn nearest_weekday(&self, day: i64) -> i64 {
        let last = last_day_of_month(self.year, self.month);
        if day > last {
            return -1;
        }
        match weekday(self.year, self.month, day) {
            0 if day == last => day - 2,
            0 => day + 1,
            6 if day == 1 => day + 2,
            6 => day - 1,
            _ => day,
        }
    }

    fn is_nth_weekday_of_month(&self, day: i64, nth: u8) -> bool {
        let target = weekday(self.year, self.month, day);
        let count = (1..=day)
            .filter(|candidate| weekday(self.year, self.month, *candidate) == target)
            .count();
        if nth & ANY_NTH != 0
            && count >= 1
            && NTH_BITS.get(count - 1).is_some_and(|bit| bit & nth != 0)
        {
            return true;
        }
        if nth & LAST_NTH != 0 {
            let last = last_day_of_month(self.year, self.month);
            return ((day + 1)..=last)
                .all(|candidate| weekday(self.year, self.month, candidate) != target);
        }
        false
    }

    /// `apply()`：字段越界时经 `Date.UTC` 进位（与 croner 一样，小时只检查 > 59）。
    fn apply(&mut self) -> bool {
        let month_ok = (0..=11).contains(&self.month);
        if !month_ok
            || self.day > DAYS_IN_MONTH[self.month.clamp(0, 11) as usize]
            || self.day < 1
            || self.hour > 59
            || self.minute > 59
            || self.second > 59
            || self.hour < 0
            || self.minute < 0
            || self.second < 0
        {
            let fields = utc_fields(utc_make([
                self.year,
                self.month,
                self.day,
                self.hour,
                self.minute,
                self.second,
                self.ms,
            ]));
            *self = Self::from_fields(fields);
            return true;
        }
        false
    }

    fn slot(pattern: &Pattern, unit: Unit, index: i64) -> u8 {
        let slots: &[u8] = match unit {
            Unit::Year => &pattern.year,
            Unit::Month => &pattern.month,
            Unit::Day => &pattern.day,
            Unit::Hour => &pattern.hour,
            Unit::Minute => &pattern.minute,
            Unit::Second => &pattern.second,
        };
        usize::try_from(index)
            .ok()
            .and_then(|index| slots.get(index).copied())
            .unwrap_or(0)
    }

    fn slot_len(pattern: &Pattern, unit: Unit) -> i64 {
        match unit {
            Unit::Year => pattern.year.len() as i64,
            Unit::Month => 12,
            Unit::Day => 31,
            Unit::Hour => 24,
            Unit::Minute | Unit::Second => 60,
        }
    }

    /// `_findMatch`：`direction` 为 1 向后找、-1 向前找。
    fn find_match(&mut self, unit: Unit, pattern: &Pattern, offset: i64, direction: i64) -> Found {
        let original = self.get(unit);
        let last_dom = pattern
            .last_day_of_month
            .then(|| last_day_of_month(self.year, self.month));
        let first_weekday =
            (!pattern.star_dow && unit == Unit::Day).then(|| weekday(self.year, self.month, 1));
        let len = Self::slot_len(pattern, unit);
        let mut index = original + offset;
        while if direction == 1 {
            index < len
        } else {
            index >= 0
        } {
            let mut matched = Self::slot(pattern, unit, index) != 0;
            if unit == Unit::Day {
                let day = index - offset;
                if !matched {
                    for (candidate, flag) in pattern.nearest_weekdays.iter().enumerate() {
                        if *flag == 0 {
                            continue;
                        }
                        let nearest = self.nearest_weekday(candidate as i64 - offset);
                        if nearest != -1 && nearest == day {
                            matched = true;
                            break;
                        }
                    }
                }
                if pattern.last_weekday && day == self.last_weekday() {
                    matched = true;
                }
                if last_dom == Some(day) {
                    matched = true;
                }
                if let Some(first) = first_weekday {
                    let nth = pattern.day_of_week[((first + (day - 1)) % 7) as usize];
                    let dow = nth != 0 && self.is_nth_weekday_of_month(day, nth);
                    matched = if pattern.use_and_logic || pattern.star_dom {
                        matched && dow
                    } else {
                        matched || dow
                    };
                }
            }
            if matched {
                self.set(unit, index - offset);
                return if original != self.get(unit) {
                    Found::Changed
                } else {
                    Found::Same
                };
            }
            index += direction;
        }
        Found::None
    }

    fn year_allowed(pattern: &Pattern, year: i64) -> bool {
        usize::try_from(year)
            .ok()
            .and_then(|year| pattern.year.get(year))
            .is_some_and(|flag| *flag != 0)
    }

    /// `recurse`。
    fn recurse(mut self, pattern: &Pattern, level: usize) -> Result<Option<Self>, String> {
        let mut level = level;
        loop {
            if level == 0 && !pattern.star_year {
                if (0..YEARS as i64).contains(&self.year) && !Self::year_allowed(pattern, self.year)
                {
                    let Some(next) = (self.year + 1..YEARS as i64)
                        .find(|year| Self::year_allowed(pattern, *year))
                    else {
                        return Ok(None);
                    };
                    self = Self {
                        year: next,
                        month: 0,
                        day: 1,
                        hour: 0,
                        minute: 0,
                        second: 0,
                        ms: 0,
                    };
                }
                if self.year >= YEARS as i64 {
                    return Ok(None);
                }
            }
            let (unit, parent, offset) = LEVELS[level];
            let found = self.find_match(unit, pattern, offset, 1);
            if !matches!(found, Found::Same) {
                for (lower, _, lower_offset) in &LEVELS[level + 1..] {
                    self.set(*lower, -lower_offset);
                }
                if matches!(found, Found::None) {
                    self.set(parent, self.get(parent) + 1);
                    self.set(unit, -offset);
                    self.apply();
                    if level == 0 && !pattern.star_year {
                        while (0..YEARS as i64).contains(&self.year)
                            && !Self::year_allowed(pattern, self.year)
                        {
                            self.year += 1;
                        }
                        if self.year >= YEARS as i64 {
                            return Ok(None);
                        }
                    }
                    level = 0;
                    continue;
                } else if self.apply() {
                    if level == 0 {
                        return Err(CRONER_LEVEL_ERROR.into());
                    }
                    level -= 1;
                    continue;
                }
            }
            level += 1;
            if level >= LEVELS.len() {
                return Ok(Some(self));
            }
            let limit = if pattern.star_year {
                3_000
            } else {
                YEARS as i64
            };
            if self.year >= limit {
                return Ok(None);
            }
        }
    }

    fn max_pattern_value(&self, unit: Unit, pattern: &Pattern, offset: i64) -> i64 {
        if unit == Unit::Day && (pattern.last_day_of_month || !pattern.star_dow) {
            return last_day_of_month(self.year, self.month);
        }
        let len = Self::slot_len(pattern, unit);
        (0..len)
            .rev()
            .find(|index| Self::slot(pattern, unit, *index) != 0)
            .map_or(len - 1 - offset, |index| index - offset)
    }

    /// `recurseBackward`。
    fn recurse_backward(
        mut self,
        pattern: &Pattern,
        level: usize,
        depth: u32,
    ) -> Result<Option<Self>, String> {
        let (mut level, mut depth) = (level, depth);
        loop {
            if depth > 10_000 {
                return Ok(None);
            }
            if level == 0 && !pattern.star_year {
                if (0..YEARS as i64).contains(&self.year) && !Self::year_allowed(pattern, self.year)
                {
                    let Some(previous) = (0..self.year)
                        .rev()
                        .find(|year| Self::year_allowed(pattern, *year))
                    else {
                        return Ok(None);
                    };
                    self = Self {
                        year: previous,
                        month: 11,
                        day: 31,
                        hour: 23,
                        minute: 59,
                        second: 59,
                        ms: 0,
                    };
                }
                if self.year < 0 {
                    return Ok(None);
                }
            }
            let (unit, parent, offset) = LEVELS[level];
            let found = self.find_match(unit, pattern, offset, -1);
            if !matches!(found, Found::Same) {
                for (lower, _, lower_offset) in &LEVELS[level + 1..] {
                    let max = self.max_pattern_value(*lower, pattern, *lower_offset);
                    self.set(*lower, max);
                }
                if matches!(found, Found::None) {
                    self.set(parent, self.get(parent) - 1);
                    if level == 0 {
                        let last = last_day_of_month(self.year, self.month);
                        if self.day > last {
                            self.day = last;
                        }
                    }
                    if level == 1 {
                        if self.day <= 0 {
                            self.day = 1;
                        } else {
                            let (mut year, mut month) = (self.year, self.month);
                            while month < 0 {
                                month += 12;
                                year -= 1;
                            }
                            while month > 11 {
                                month -= 12;
                                year += 1;
                            }
                            let last = last_day_of_month(year, month);
                            if self.day > last {
                                self.day = last;
                            }
                        }
                    }
                    self.apply();
                    let max = self.max_pattern_value(unit, pattern, offset);
                    if unit == Unit::Day {
                        self.set(unit, max.min(last_day_of_month(self.year, self.month)));
                    } else {
                        self.set(unit, max);
                    }
                    self.apply();
                    if level == 0 {
                        let day_max = self.max_pattern_value(Unit::Day, pattern, LEVELS[1].2);
                        let candidate = day_max.min(last_day_of_month(self.year, self.month));
                        if candidate != self.day {
                            self.day = candidate;
                            self.hour = self.max_pattern_value(Unit::Hour, pattern, 0);
                            self.minute = self.max_pattern_value(Unit::Minute, pattern, 0);
                            self.second = self.max_pattern_value(Unit::Second, pattern, 0);
                        }
                    }
                    if level == 0 && !pattern.star_year {
                        while (0..YEARS as i64).contains(&self.year)
                            && !Self::year_allowed(pattern, self.year)
                        {
                            self.year -= 1;
                        }
                        if self.year < 0 {
                            return Ok(None);
                        }
                    }
                    level = 0;
                    depth += 1;
                    continue;
                } else if self.apply() {
                    if level == 0 {
                        return Err(CRONER_LEVEL_ERROR.into());
                    }
                    level -= 1;
                    depth += 1;
                    continue;
                }
            }
            level += 1;
            if level >= LEVELS.len() {
                return Ok(Some(self));
            }
            if self.year < 0 {
                return Ok(None);
            }
            depth += 1;
        }
    }

    fn time(&self, zone: &dyn Zone) -> i64 {
        zone.make([
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.ms,
        ])
    }
}

/// `new Cron(pattern)`：周期表达式，或含 `:` 的一次性时间。
#[derive(Debug, Clone)]
pub enum Cron {
    Pattern(Box<Pattern>),
    Once(i64),
}

impl Cron {
    /// 解析失败（`new Cron` 抛错）时返回 Err。一次性时间需要时区来解释不带偏移的写法。
    pub fn parse(expression: &str, zone: &dyn Zone) -> Result<Self, String> {
        if expression.find(':').is_some_and(|index| index > 0) {
            return parse_iso_datetime(zone, expression)
                .map(Cron::Once)
                .ok_or_else(|| "Invalid ISO8601 passed to timezone parser.".to_owned());
        }
        Pattern::parse(expression).map(|pattern| Cron::Pattern(Box::new(pattern)))
    }

    /// `nextRun(from)`：严格晚于 `from`（至少 +1 s，毫秒清零）的下一次；没有时为 None。
    pub fn next_run(&self, zone: &dyn Zone, from_ms: i64) -> Result<Option<i64>, String> {
        match self {
            Cron::Once(at) => Ok((*at > from_ms).then_some(*at)),
            Cron::Pattern(pattern) => {
                let mut date = CronDate::from_fields(zone.fields(from_ms));
                date.second += 1;
                date.ms = 0;
                date.apply();
                Ok(date.recurse(pattern, 0)?.map(|date| date.time(zone)))
            }
        }
    }

    /// `previousRuns(1, from)[0]`：早于 `from` 的上一次。
    pub fn previous_run(&self, zone: &dyn Zone, from_ms: i64) -> Result<Option<i64>, String> {
        match self {
            Cron::Once(at) => Ok((*at < from_ms).then_some(*at)),
            Cron::Pattern(pattern) => {
                let mut date = CronDate::from_fields(zone.fields(from_ms));
                date.second -= 1;
                date.ms = 0;
                date.apply();
                Ok(date
                    .recurse_backward(pattern, 0, 0)?
                    .map(|date| date.time(zone)))
            }
        }
    }
}

/// `isValidCronExpr`。
pub fn is_valid(expression: &str, zone: &dyn Zone) -> bool {
    Cron::parse(expression, zone).is_ok()
}
