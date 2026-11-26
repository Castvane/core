use std::{str::FromStr, time::Duration};

use croner::Cron;
use jiff::{Timestamp, ZonedDateTime, tz::TimeZone};

pub enum Schedule {
    Once(Timestamp),
    Repeating(Box<Cron>),
}

impl Schedule {
    pub fn parse(specification: &str) -> Result<Option<Self>, &'static str> {
        if specification.is_empty() {
            return Ok(None);
        }
        if let Ok(date) = Timestamp::from_str(specification) {
            return Ok(Some(Self::Once(date)));
        }
        Cron::from_str(specification)
            .map(|cron| Some(Self::Repeating(Box::new(cron))))
            .map_err(|_| "invalid scheduler expression")
    }

    pub fn next(&self, now: Timestamp) -> Option<Duration> {
        let date = match self {
            Self::Once(date) => *date,
            Self::Repeating(cron) => cron
                .find_next_occurrence(&utc(now), false)
                .ok()?
                .timestamp(),
        };
        date.duration_since(now).try_into().ok()
    }
}

pub fn utc(timestamp: Timestamp) -> ZonedDateTime {
    timestamp.to_zoned_datetime(TimeZone::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_utc_occurrences_and_skips_missed_one_shots() {
        let time = "2026-04-02T15:04:00Z".parse().unwrap();
        assert_eq!(
            Schedule::parse("*/5 * * * *").unwrap().unwrap().next(time),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            Schedule::parse("2026-04-02T17:04:03+02:00")
                .unwrap()
                .unwrap()
                .next(time),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            Schedule::parse("2026-04-02T15:03:59Z")
                .unwrap()
                .unwrap()
                .next(time),
            None
        );
        assert!(Schedule::parse("61 * * * *").is_err());
    }
}
