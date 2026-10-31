use super::MonitorError;
use jiff::{
    Timestamp,
    civil::Time,
    tz::{AmbiguousOffset, TimeZone},
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqliteRow};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorSchedule {
    #[default]
    Interval,
    Daily {
        time: String,
    },
    Weekly {
        time: String,
        weekday: i8,
    },
}

impl MonitorSchedule {
    pub(super) fn canonical_timezone(&self, timezone: &str) -> Result<String, MonitorError> {
        if matches!(self, Self::Interval) && timezone != "UTC" {
            return Err(MonitorError::Invalid);
        }
        TimeZone::get(timezone)
            .map_err(|_| MonitorError::Invalid)?
            .iana_name()
            .map(str::to_owned)
            .ok_or(MonitorError::Invalid)
    }

    pub(super) fn columns(&self) -> (&'static str, Option<&str>, Option<i8>) {
        match self {
            Self::Interval => ("interval", None, None),
            Self::Daily { time } => ("daily", Some(time), None),
            Self::Weekly { time, weekday } => ("weekly", Some(time), Some(*weekday)),
        }
    }

    pub(super) fn decode(row: &SqliteRow) -> Result<Self, MonitorError> {
        Ok(match row.try_get::<&str, _>("schedule_kind")? {
            "interval" => Self::Interval,
            "daily" => Self::Daily {
                time: row.try_get("schedule_time")?,
            },
            "weekly" => Self::Weekly {
                time: row.try_get("schedule_time")?,
                weekday: row.try_get("schedule_weekday")?,
            },
            _ => return Err(MonitorError::Database),
        })
    }

    pub(super) fn next_run(
        &self,
        after: i64,
        interval: i64,
        timezone: &str,
    ) -> Result<i64, MonitorError> {
        if !(900..=31536000).contains(&interval) {
            return Err(MonitorError::Invalid);
        }
        let (time, weekday) = match self {
            Self::Interval => {
                return if timezone == "UTC" {
                    Ok(super::next_boundary(after, interval))
                } else {
                    Err(MonitorError::Invalid)
                };
            }
            Self::Daily { time } => (time, None),
            Self::Weekly { time, weekday } if (1..=7).contains(weekday) => (time, Some(*weekday)),
            Self::Weekly { .. } => return Err(MonitorError::Invalid),
        };
        if time.len() != 5
            || !time.as_bytes().iter().enumerate().all(|(i, b)| {
                if i == 2 {
                    *b == b':'
                } else {
                    b.is_ascii_digit()
                }
            })
            || timezone.len() > 255
            || timezone.is_empty()
        {
            return Err(MonitorError::Invalid);
        }
        let time: Time = time.parse().map_err(|_| MonitorError::Invalid)?;
        let zone = TimeZone::get(timezone).map_err(|_| MonitorError::Invalid)?;
        let mut date = Timestamp::from_second(after)
            .map_err(|_| MonitorError::Invalid)?
            .to_zoned(zone.clone())
            .date();
        for _ in 0..15 {
            if weekday.is_none_or(|day| date.weekday().to_monday_one_offset() == day) {
                let local = date.to_datetime(time);
                let ambiguous = zone.to_ambiguous_timestamp(local);
                let timestamp = match ambiguous.offset() {
                    // Shifting by the gap would run 02:30 at 03:30. Use the transition's
                    // first valid instant instead, and skip dates with no valid instant.
                    AmbiguousOffset::Gap { .. } => zone
                        .following(ambiguous.earlier().map_err(|_| MonitorError::Invalid)?)
                        .next()
                        .map(|transition| transition.timestamp()),
                    _ => Some(ambiguous.earlier().map_err(|_| MonitorError::Invalid)?),
                };
                if let Some(timestamp) = timestamp
                    && timestamp.as_second() > after
                    && timestamp.to_zoned(zone.clone()).date() == date
                {
                    return Ok(timestamp.as_second());
                }
            }
            date = date.tomorrow().map_err(|_| MonitorError::Invalid)?;
        }
        Err(MonitorError::Invalid)
    }
}

pub(super) fn default_timezone() -> String {
    "UTC".into()
}
