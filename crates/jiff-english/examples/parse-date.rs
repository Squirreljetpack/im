//! Little command-line program to play with jiff-english expressions:
//!
//! ```text
//! $ cargo run -p jiff-english --example parse-date -- 'next friday 8pm'
//! $ cargo run -p jiff-english --example parse-date -- --utc '3 days hence eod'
//! $ cargo run -p jiff-english --example parse-date -- 'yesterday' '2024-03-14 12:00'
//! ```
//!
//! Optional `--utc` evaluates in UTC instead of the local time zone; a second
//! positional argument sets the base datetime (default: now).

use jiff::Zoned;
use jiff::tz::TimeZone;
use jiff_english::{Dialect, parse_date_string};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let utc = args.iter().any(|a| a == "--utc");
    let positionals: Vec<&String> = args.iter().filter(|a| a.as_str() != "--utc").collect();

    let (datestr, basestr) = match positionals.as_slice() {
        [d] => (d.as_str(), None),
        [d, b] => (d.as_str(), Some(b.as_str())),
        _ => {
            eprintln!("usage: parse-date [--utc] <date> [<base>]");
            std::process::exit(1);
        }
    };

    let now: Zoned = if utc {
        Zoned::now().timestamp().to_zoned(TimeZone::UTC)
    } else {
        Zoned::now()
    };
    let base = match basestr {
        Some(b) => match parse_date_string(b, &now, Dialect::Uk) {
            Ok(zdt) => zdt,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        None => now,
    };

    match parse_date_string(datestr, &base, Dialect::Uk) {
        Ok(zdt) => {
            println!("base {}", base);
            println!("calc {}", zdt);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
