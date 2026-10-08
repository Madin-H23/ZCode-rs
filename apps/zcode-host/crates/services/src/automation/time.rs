//! JS `Date` 的本地时间语义（automation 的 cron 与调度规则都按它计算，见 automationCron.ts）。
//!
//! - [`utc_make`] 等价 `Date.UTC(y, m, d, h, mi, s, ms)`：字段可以越界，按 MakeDay / MakeTime 进位。
//! - [`Zone::make`] 等价 `new Date(y, m, d, h, mi, s, ms)`：先按同样规则得到本地时刻，再换算 UTC；
//!   夏令时重叠取较早的时刻，空档按转换前（前一天）的偏移解释（ECMA-262 `UTC(t)`）。
//! - [`Zone::fields`] 等价 `getFullYear()`、`getMonth()` … `getDay()`。
//!
//! 生产用 [`ChronoZone`] 包 `chrono::Local`（读 `TZ` 与系统时区，与 Node 相同）；测试可换成固定时区。
use chrono::{DateTime, Datelike, LocalResult, NaiveDateTime, Offset, TimeZone, Timelike};

pub const MS_PER_DAY: i64 = 86_400_000;

/// 本地日历字段；`month` 从 0 开始，`weekday` 0 = 周日（与 JS 相同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalFields {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub ms: i64,
    pub weekday: i64,
}

pub trait Zone: Send + Sync {
    /// UTC 毫秒 → 本地字段。
    fn fields(&self, utc_ms: i64) -> LocalFields;
    /// `new Date(year, month, day, hour, minute, second, ms).getTime()`。
    fn make(&self, parts: [i64; 7]) -> i64;
}

/// 公历日期 → 自 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
pub fn days_from_civil(year: i64, month1: i64, day: i64) -> i64 {
    let y = if month1 <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month1;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 天数 → (年, 月 1-12, 日)。
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `Date.UTC(year, month, day, hour, minute, second, ms)`（month 从 0 开始，可越界）。
pub fn utc_make(parts: [i64; 7]) -> i64 {
    let [year, month, day, hour, minute, second, ms] = parts;
    let year = year + month.div_euclid(12);
    let month = month.rem_euclid(12);
    let days = days_from_civil(year, month + 1, 1) + day - 1;
    days * MS_PER_DAY + hour * 3_600_000 + minute * 60_000 + second * 1_000 + ms
}

/// UTC 毫秒按 UTC 拆成字段（`getUTC*`）。
pub fn utc_fields(utc_ms: i64) -> LocalFields {
    let days = utc_ms.div_euclid(MS_PER_DAY);
    let rest = utc_ms.rem_euclid(MS_PER_DAY);
    let (year, month1, day) = civil_from_days(days);
    LocalFields {
        year,
        month: month1 - 1,
        day,
        hour: rest / 3_600_000,
        minute: rest / 60_000 % 60,
        second: rest / 1_000 % 60,
        ms: rest % 1_000,
        // 1970-01-01 是周四。
        weekday: (days + 4).rem_euclid(7),
    }
}

/// `new Date(Date.UTC(y, m, d)).getUTCDay()`。
pub fn utc_weekday(year: i64, month: i64, day: i64) -> i64 {
    utc_fields(utc_make([year, month, day, 0, 0, 0, 0])).weekday
}

/// 以 chrono 时区实现 JS 本地时间语义。
pub struct ChronoZone<Tz>(pub Tz);

impl<Tz> ChronoZone<Tz>
where
    Tz: TimeZone,
{
    fn offset_ms(datetime: &DateTime<Tz>) -> i64 {
        i64::from(datetime.offset().fix().local_minus_utc()) * 1_000
    }

    fn naive(local_ms: i64) -> Option<NaiveDateTime> {
        DateTime::from_timestamp_millis(local_ms).map(|datetime| datetime.naive_utc())
    }
}

impl<Tz> Zone for ChronoZone<Tz>
where
    Tz: TimeZone + Send + Sync,
{
    fn fields(&self, utc_ms: i64) -> LocalFields {
        match self.0.timestamp_millis_opt(utc_ms) {
            LocalResult::Single(datetime) | LocalResult::Ambiguous(datetime, _) => {
                let local = datetime.naive_local();
                LocalFields {
                    year: i64::from(local.year()),
                    month: i64::from(local.month0()),
                    day: i64::from(local.day()),
                    hour: i64::from(local.hour()),
                    minute: i64::from(local.minute()),
                    second: i64::from(local.second()),
                    ms: i64::from(local.and_utc().timestamp_subsec_millis()),
                    weekday: i64::from(local.weekday().num_days_from_sunday()),
                }
            }
            // 超出 chrono 范围：按 UTC 退化（JS 的有效范围内不会出现）。
            LocalResult::None => utc_fields(utc_ms),
        }
    }

    fn make(&self, parts: [i64; 7]) -> i64 {
        let local_ms = utc_make(parts);
        let Some(naive) = Self::naive(local_ms) else {
            return local_ms;
        };
        match self.0.from_local_datetime(&naive) {
            LocalResult::Single(datetime) => local_ms - Self::offset_ms(&datetime),
            // 重叠：取较早的时刻（转换前的偏移）。
            LocalResult::Ambiguous(earlier, later) => {
                (local_ms - Self::offset_ms(&earlier)).min(local_ms - Self::offset_ms(&later))
            }
            // 空档：取前一天同一本地时刻的偏移（转换前的偏移）。
            LocalResult::None => {
                let before = Self::naive(local_ms - MS_PER_DAY)
                    .and_then(|naive| self.0.from_local_datetime(&naive).latest());
                match before {
                    Some(datetime) => local_ms - Self::offset_ms(&datetime),
                    None => local_ms,
                }
            }
        }
    }
}

/// 生产用的系统本地时区。
pub fn local_zone() -> ChronoZone<chrono::Local> {
    ChronoZone(chrono::Local)
}

/// `new Date(ms).toLocaleString()` 在 en-US 默认区域下的格式：`10/8/2026, 3:04:05 PM`。
pub fn locale_string(zone: &dyn Zone, utc_ms: i64) -> String {
    let fields = zone.fields(utc_ms);
    let hour12 = match fields.hour % 12 {
        0 => 12,
        hour => hour,
    };
    let meridiem = if fields.hour < 12 { "AM" } else { "PM" };
    format!(
        "{}/{}/{}, {}:{:02}:{:02} {}",
        fields.month + 1,
        fields.day,
        fields.year,
        hour12,
        fields.minute,
        fields.second,
        meridiem
    )
}

/// JS `parseInt(text, 10)` 的数字前缀解析（可选符号 + 前导数字）；没有数字时为 None（NaN）。
pub fn parse_int_prefix(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1, &trimmed[1..]),
        Some(b'+') => (1, &trimmed[1..]),
        _ => (1, trimmed),
    };
    let end = digits
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    // 超长数字只会让后续范围校验失败；饱和即可。
    let value = digits[..end].parse::<i64>().unwrap_or(i64::MAX);
    Some(sign * value)
}

/// `Date.parse` 对 ISO 日期时间字符串（`YYYY-MM-DDTHH:mm[:ss[.sss]][Z|±HH:mm]`，`T` 也可以是空格）
/// 的解析（V8 语义：日期在短月里进位）；不带偏移按本地时间。其他格式返回 None。
pub fn parse_iso_datetime(zone: &dyn Zone, text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        slice
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| slice.parse().ok())?
    };
    if bytes.len() < 16 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ' {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute) = (number(11..13)?, number(14..16)?);
    let mut index = 16;
    let (mut second, mut ms) = (0, 0);
    if bytes.get(index) == Some(&b':') {
        second = number(index + 1..index + 3)?;
        index += 3;
        if bytes.get(index) == Some(&b'.') {
            let start = index + 1;
            let end = text[start..]
                .bytes()
                .position(|byte| !byte.is_ascii_digit())
                .map_or(text.len(), |offset| start + offset);
            if end == start {
                return None;
            }
            let digits = &text[start..end];
            let padded = format!("{:0<3}", &digits[..digits.len().min(3)]);
            ms = padded.parse().ok()?;
            index = end;
        }
    }
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 24
        || minute > 59
        || second > 59
        || (hour == 24 && (minute, second, ms) != (0, 0, 0))
    {
        return None;
    }
    // V8 宽松：1-31 日在短月里向后进位（2026-02-30 → 3 月 2 日）。
    let parts = [year, month - 1, day, hour, minute, second, ms];
    let rest = &text[index..];
    if rest.is_empty() {
        return Some(zone.make(parts));
    }
    let utc = utc_make(parts);
    if rest == "Z" || rest == "z" {
        return Some(utc);
    }
    let sign = match rest.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let offset = &rest[1..];
    let (hours, minutes) = match offset.len() {
        5 if offset.as_bytes()[2] == b':' => (offset.get(0..2)?, offset.get(3..5)?),
        4 => (offset.get(0..2)?, offset.get(2..4)?),
        _ => return None,
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(utc - sign * (hours * 3_600_000 + minutes * 60_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_york() -> ChronoZone<chrono_tz::Tz> {
        ChronoZone(chrono_tz::America::New_York)
    }

    #[test]
    fn civil_round_trip_and_utc_overflow() {
        for days in [-800_000, -1, 0, 1, 19_000, 2_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        // Date.UTC(2026, 13, 0) = 2027-01-31。
        assert_eq!(
            utc_fields(utc_make([2026, 13, 0, 0, 0, 0, 0])),
            utc_fields(utc_make([2027, 1, 0, 0, 0, 0, 0]))
        );
        assert_eq!(utc_fields(utc_make([2027, 1, 0, 0, 0, 0, 0])).day, 31);
        assert_eq!(utc_weekday(2026, 9, 8), 4);
    }

    #[test]
    fn local_make_follows_ecmascript_for_gaps_and_folds() {
        let zone = new_york();
        // 2026-03-08 02:30 不存在：按转换前偏移（-5）解释，即 03:30 EDT。
        let gap = zone.make([2026, 2, 8, 2, 30, 0, 0]);
        assert_eq!(gap, utc_make([2026, 2, 8, 7, 30, 0, 0]));
        assert_eq!(zone.fields(gap).hour, 3);
        // 2026-11-01 01:30 出现两次：取较早的 EDT（-4）。
        let fold = zone.make([2026, 10, 1, 1, 30, 0, 0]);
        assert_eq!(fold, utc_make([2026, 10, 1, 5, 30, 0, 0]));
    }

    #[test]
    fn parses_like_javascript() {
        assert_eq!(parse_int_prefix("5*"), Some(5));
        assert_eq!(parse_int_prefix("W"), None);
        assert_eq!(parse_int_prefix(""), None);
        let zone = new_york();
        assert_eq!(
            parse_iso_datetime(&zone, "2026-10-10T10:00:00Z"),
            Some(utc_make([2026, 9, 10, 10, 0, 0, 0]))
        );
        assert_eq!(
            parse_iso_datetime(&zone, "2026-10-10T10:00"),
            Some(utc_make([2026, 9, 10, 14, 0, 0, 0]))
        );
        assert_eq!(parse_iso_datetime(&zone, "12:30 * * * *"), None);
        assert_eq!(
            locale_string(&zone, utc_make([2026, 9, 8, 19, 4, 5, 0])),
            "10/8/2026, 3:04:05 PM"
        );
    }
}
