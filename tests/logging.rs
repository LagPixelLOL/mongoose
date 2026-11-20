use chrono::{FixedOffset, TimeZone};
use log::LevelFilter;
use mongoose::logging::{format_timestamp, format_utc_offset, level_for};

#[test]
fn whole_hour_offsets() {
    assert_eq!(format_utc_offset(0), "+0");
    assert_eq!(format_utc_offset(3600), "+1");
    assert_eq!(format_utc_offset(2 * 3600), "+2");
    assert_eq!(format_utc_offset(14 * 3600), "+14");
    assert_eq!(format_utc_offset(-5 * 3600), "-5");
}

#[test]
fn fractional_offsets() {
    assert_eq!(format_utc_offset(6 * 3600 + 1800), "+6.5");
    assert_eq!(format_utc_offset(5 * 3600 + 45 * 60), "+5.75");
    assert_eq!(format_utc_offset(5 * 3600 + 30 * 60), "+5.5");
    assert_eq!(format_utc_offset(-(3 * 3600 + 30 * 60)), "-3.5");
    assert_eq!(format_utc_offset(-(9 * 3600 + 30 * 60)), "-9.5");
    assert_eq!(format_utc_offset(12 * 3600 + 45 * 60), "+12.75");
}

#[test]
fn odd_offsets_round_to_two_decimals() {
    // Amsterdam LMT: +0:19:32 = 0.32555... h
    assert_eq!(format_utc_offset(19 * 60 + 32), "+0.33");
    // +1:00:18 = 1.005 h rounds half up
    assert_eq!(format_utc_offset(3600 + 18), "+1.01");
    // +1:00:17 = 1.00472 h
    assert_eq!(format_utc_offset(3600 + 17), "+1");
    // -0:25:21 (Irish LMT) = -0.4225 h
    assert_eq!(format_utc_offset(-(25 * 60 + 21)), "-0.42");
    // Rounds up to the next whole hour.
    assert_eq!(format_utc_offset(2 * 3600 - 10), "+2");
    // Trailing zero is dropped: 1.10 -> 1.1
    assert_eq!(format_utc_offset(3600 + 6 * 60), "+1.1");
}

#[test]
fn timestamp_format() {
    let tz = FixedOffset::east_opt(5 * 3600 + 45 * 60).unwrap();
    let time = tz
        .with_ymd_and_hms(2026, 3, 7, 9, 5, 4)
        .unwrap()
        .checked_add_signed(chrono::TimeDelta::milliseconds(42))
        .unwrap();
    assert_eq!(format_timestamp(&time), "26/03/07 09:05:04:042+5.75");

    let utc = FixedOffset::east_opt(0).unwrap();
    let time = utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();
    assert_eq!(format_timestamp(&time), "26/12/31 23:59:59:000+0");

    let west = FixedOffset::west_opt(3 * 3600 + 1800).unwrap();
    let time = west.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
    assert_eq!(format_timestamp(&time), "30/01/02 03:04:05:000-3.5");
}

#[test]
fn verbosity_levels() {
    assert_eq!(level_for(false), LevelFilter::Info);
    assert_eq!(level_for(true), LevelFilter::Debug);
}
