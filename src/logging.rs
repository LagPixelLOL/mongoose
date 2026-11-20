//! Logging setup built on `env_logger`.
//!
//! Timestamps look like `26/11/20 14:03:07:042+5.75`: `yy/mm/dd hh:mm:ss:mmm`
//! followed by the local UTC offset in hours, rounded to two decimals.

use std::io::Write;

use chrono::{DateTime, Local, Offset, TimeZone};
use log::LevelFilter;

/// Formats a UTC offset given in seconds as `+N`, `-N`, `+N.N` or `+N.NN`.
pub fn format_utc_offset(offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let seconds = i64::from(offset_seconds).abs();
    // Hundredths of an hour, rounded half up: 1/100 h = 36 s.
    let hundredths = (seconds + 18) / 36;
    let hours = hundredths / 100;
    let fraction = hundredths % 100;
    if fraction == 0 {
        format!("{sign}{hours}")
    } else if fraction % 10 == 0 {
        format!("{sign}{hours}.{}", fraction / 10)
    } else {
        format!("{sign}{hours}.{fraction:02}")
    }
}

/// Formats a timestamp as `yy/mm/dd hh:mm:ss:mmm+N`.
pub fn format_timestamp<Tz: TimeZone>(time: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let offset = time.offset().fix().local_minus_utc();
    format!(
        "{}{}",
        time.format("%y/%m/%d %H:%M:%S:%3f"),
        format_utc_offset(offset)
    )
}

/// Log level used for the given verbosity flag.
pub fn level_for(verbose: bool) -> LevelFilter {
    if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    }
}

/// Installs the global logger. Logs go to stderr.
pub fn init(verbose: bool) {
    env_logger::Builder::new()
        .filter_level(level_for(verbose))
        .format(|buf, record| {
            let style = buf.default_level_style(record.level());
            writeln!(
                buf,
                "[{} {style}{:<5}{style:#} {}] {}",
                format_timestamp(&Local::now()),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}
