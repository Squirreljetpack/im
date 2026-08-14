# jiff-english

Parses informal English date and time expressions on top of
[jiff](https://docs.rs/jiff), in the spirit of the Linux `date` command. It is
a port of the pattern language of
[`chrono-english`](https://github.com/stevedonovan/chrono-english) (MIT) —
same grammar, same dialect semantics, same error strings — but resolves into
`jiff::Zoned` instead of `chrono` types.

## Example

```rust
use jiff::civil;
use jiff::tz::TimeZone;
use jiff_english::{parse_date_string, Dialect};

let base = civil::date(2024, 3, 14)
    .at(12, 34, 56, 0)
    .to_zoned(TimeZone::UTC)
    .unwrap();
let date_time = parse_date_string("next friday 8pm", &base, Dialect::Uk).unwrap();
assert_eq!(date_time.date(), civil::date(2024, 3, 22));
assert_eq!(date_time.time(), civil::time(20, 0, 0, 0));
```

## Extensions over chrono-english

- **`eod` / `end` / `start`** as time-part specifiers: `"tomorrow eod"`,
  `"next friday start"`, or a bare `"eod"` (today at the last moment of the
  day). `start` is 00:00:00.000000000, `eod`/`end` is 23:59:59.999999999.
  Case-insensitive.
- **`hence` / `later`** as the explicit-future counterparts of `ago`:
  `"3 days ago"` negates, `"3 days hence"` / `"3 days later"` keep the sign.
  All three markers are case-insensitive.
- **12-hour clock fix**: `12am` is midnight (00:00) and `12pm` is noon
  (12:00); chrono-english turns `12pm` into an invalid 24:00 and leaves
  `12am` as 12:00 noon.

## Grammar

- Absolute dates: `2018-04-01`, `30/06/17`, `30 June 2018`, `June 30, 2018`
  (comma required), bare year `2018`
- Keywords: `now`, `today`, `yesterday`, `tomorrow` (+ optional time)
- Weekdays: `friday`, `fri`, `next mon` (UK: weekday of next week),
  `last fri 9.30`
- Months: `april`, `next April`, `last April`; day-month forms `4 July`,
  `9/11` (day/month under `Dialect::Uk`), `next 10 Dec`
- Intervals: `2 days`, `3h`, `-3 month`, `6 months ago`, `3 days hence`,
  `2 days later 15:00`; single-letter shortcuts `s m h w d y` (m = minutes)
- Times: `18:03[:40[.25]]`, `6.03pm`, `8pm`, `±HH[:MM]` / `Z` offsets
- Day alignment: `eod`, `end`, `start` (with any date form)

## Entry points

- `parse_date_string` — lenient (chrono-english parity): trailing words after
a bare am/pm time are dropped (`"10pm meeting"` → 22:00 today).
- `parse_strict` — errors on any trailing input (`trailing characters after
date expression`; trailing whitespace is fine).
- `parse_and_remainder` — like `parse_date_string` (and chrono's
`NaiveDate::parse_and_remainder`), but also returns the leftover input as
`(Zoned, &str)`, trailing whitespace included (the core the two above
delegate to).
- `parse_duration` — the relative part of `"N unit [ago|hence|later]"` as an
`Interval`.

## License

MIT — ported from [chrono-english](https://github.com/stevedonovan/chrono-english)
(c) Steve Donovan. See `LICENSE`.
