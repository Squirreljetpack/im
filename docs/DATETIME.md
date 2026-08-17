# date.md — Date & time expressions (`Dialect::Uk`)

Reference for every expression accepted by the shared datetime parser
(`crate::date::parse_datetime`), i.e. any place a `@<time>` / `@<date>` argument
appears in the CLI and TUIs. The parser is the **`jiff-english`** subcrate
(`crates/jiff-english` — a port of chrono-english 0.1.8 onto jiff) with the
fixed dialect `DATE_DIALECT = Dialect::Uk` — **UK English, day-first**, not
Ukrainian. It is intentionally a small pattern language, not natural-language
parsing. All examples below were verified against the actual parser with a
fixed base of **2024-03-14 12:34:56 UTC (a Thursday)**; month-clamp and
same-day examples use the stated base. The port reproduces chrono-english's
grammar and error strings exactly, with three additions: `eod`/`end`/`start`
time specifiers, `hence`/`later` interval markers, and a normalized 12-hour
clock (`12am` = midnight, `12pm` = noon).

## Grammar in one line

```text
[date-part] [time-part]          e.g. "next fri 8pm", "3 days ago 15:00"
date-part  := now|today|yesterday|tomorrow|y|yesterweek
            | [next|last] (weekday | month | day month | day/month)
            | [next|last] month-name day | day month-name [year]
            | year | year-month-day | day/month[/year] | month-name day[, year]
            | [+|-]N unit ["ago"|"hence"|"later"] | -N
time-part  := H[:MM[:SS[.ffff]]] [am|pm|Z|±HH[:MM]] | H.MM [am|pm] | H [am|pm]
            | eod | end | start
```

## Lexical rules

- Weekdays, months, time units, `ago`/`hence`/`later` and `eod`/`end`/`start`
  are case-insensitive (`friday` = `FRIDAY`, `EOD` = `eod`). Weekday and
  month names match as **initial substrings** of the full name, minimum
  three letters (`mon`, `mond`, `monday`, `tues`, `decemb`, `sept` all
  work; `junk`, `freday`, `sunny` are errors). `next`/`last` must be
  lowercase.
- Whitespace between tokens is free (`June   30,    2018` works).
- Numbers are plain integers: no floats (`1.5 hours` → error), no spelled-out
  numbers (`two days` → error), no ordinals (`5th of may`, `1st April` → error).
- No connective words: `at`, `in`, `of`, `next week friday`, `friday next week`,
  `a week ago`, `midnight`, `noon` are **all errors**.

## Absolute dates

| expression | result (base 2024-03-14) | notes |
| --- | --- | --- |
| `2018-04-01` | 2018-04-01 00:00 | ISO; `T` separator also OK (`2024-03-15T14:30`) |
| `30/06/17` | 2017-06-30 00:00 | day/month/year; 2-digit years: `00`–`40` → 2000–2040, `41`–`99` → 1941–1999 |
| `30/06/2017` | 2017-06-30 00:00 | 4-digit year also accepted |
| `30 June 2018` | 2018-06-30 00:00 | day + month name + year |
| `1 April 2018` | 2018-04-01 00:00 | same form — day must come before the month name |
| `June 30, 2018` | 2018-06-30 00:00 | month name + day + **comma required**; `June 30 2018` → error |
| `2018` | 2018-01-01 00:00 | bare year = Jan 1 |
| `jan 2025` / `January 2025` | error | month name + year alone unsupported (the number is read as a day) |

An absolute date with no time part resolves to **midnight** (00:00) of that day.

## Keywords

| expression | result | notes |
| --- | --- | --- |
| `now` / `today` | base instant unchanged | `today` keeps base time-of-day |
| `yesterday` / `y` | 2024-03-13 12:34:56 | keeps base time-of-day; `y` is an alias |
| `tomorrow` | 2024-03-15 12:34:56 | keeps base time-of-day |
| `yesterweek` | 2024-03-07 12:34:56 | one week back, same weekday |

All six keywords may be followed by a time part: `now 8pm` → today 20:00,
`today 8pm` → today 20:00, `yesterday 3pm` → yesterday 15:00,
`tomorrow 9am` → tomorrow 09:00.

## Weekdays

Bare weekday = the **next occurrence** (strictly after the base instant).

| expression | result | notes |
| --- | --- | --- |
| `friday` / `fri` | 2024-03-15 00:00 | coming Friday |
| `tues` | 2024-03-19 00:00 | initial-substring rule |
| `next mon` | 2024-03-25 00:00 | **UK: +1 week** on top of the coming Monday (18th) |
| `last fri 9.30` | 2024-03-08 09:30 | previous Friday, with time |
| `next fri 8pm` | 2024-03-22 20:00 | UK: Friday *of next week* |

UK `next` semantics: explicit `next <weekday>` means **the weekday of next week**
(+7 days), unlike `Dialect::Us` where it means the same as the bare weekday
(see Dialect table below). On the base's own weekday the time part decides:
`thursday 13:00` from a Thursday 12:34 base → same day 13:00, while
`thursday 12:00` → next week's Thursday. With no time part (00:00), a bare
weekday on its own day rolls to next week (`friday` from a Friday base →
+7 days). `eod` counts as "after the base time" in this comparison, `start`
as "before": `thursday eod` from a Thursday base → today, `thursday start` →
next week.

## Months

Bare month name = the 1st of that month, relative to the base year.

| expression | result | notes |
| --- | --- | --- |
| `april` / `apr` | 2024-04-01 00:00 | month name → 1st of month |
| `next April` | 2024-04-01 00:00 | already ahead of base → same year |
| `last April` | 2023-04-01 00:00 | previous year |
| `next 1 jan` | 2025-01-01 00:00 | Jan 1 2024 has passed → next year |
| `next 31 dec` | 2024-12-31 00:00 | still ahead → this year |

`next`/`last` on a month or day-month only shifts the **year** (when the date has
passed / is still ahead); the day itself is never shifted.

## Day-month (month-day) forms

| expression | result | notes |
| --- | --- | --- |
| `4 July` | 2024-07-04 00:00 | day + month name, current year |
| `next 4 July` | 2024-07-04 00:00 | future → same year |
| `last 4 July` | 2023-07-04 00:00 | past → previous year |
| `April 1` | 2024-04-01 00:00 | month name + day (current year) |
| `last April 1` | 2023-04-01 00:00 | |
| `9/11` | 2024-11-09 00:00 | **UK: day/month** — 9 November |
| `last 9/11` | 2023-11-09 00:00 | |
| `december 25` | 2024-12-25 00:00 | month name + day, any month name |
| `next 10 Dec` | 2024-12-10 00:00 | `next` + day + month |

Without a year these are relative: they default to the current year and
`next`/`last` only adjust the year (see above).

## Relative intervals (`N <unit>`)

| expression | result | notes |
| --- | --- | --- |
| `3h` / `3 hours` | 2024-03-14 15:34:56 | seconds/minutes/hours: **keep base time-of-day** |
| `3 hours ago` | 2024-03-14 09:34:56 | `ago` negates (case-insensitive) |
| `3 hours later` | 2024-03-14 15:34:56 | `later`/`hence` = explicit future, keep sign |
| `-3h` | 2024-03-14 09:34:56 | leading `-` negates |
| `2d` / `2 days` | 2024-03-16 12:34:56 | days/weeks: **keep base time-of-day** when no time given |
| `2 days 03:00` | 2024-03-16 03:00 | day/weeks may take an explicit time part |
| `2 days ago 15:00` | 2024-03-12 15:00 | |
| `2 days later 15:00` | 2024-03-16 15:00 | marker + time part |
| `3 days hence` | 2024-03-17 12:34:56 | `hence` = `later` |
| `-1 week` | 2024-03-07 12:34:56 | |
| `-3` | 2024-03-11 12:34:56 | bare negative number = that many **days** ago (`-3` = 3 days ago, never a year) |
| `3 weeks` | 2024-04-04 12:34:56 | |
| `6 months` | 2024-09-14 00:00 | months/years: **snap to midnight** |
| `6 months ago` | 2023-09-14 00:00 | |
| `6 months hence` | 2024-09-14 00:00 | |
| `8 years` | 2032-03-14 00:00 | 1 year = 12 months |
| `15m` | 2024-03-14 12:49:56 | single-letter `m` = **minutes** |
| `3mo` | error | two-letter forms are rejected: neither a shortcut nor a clear unit |
| `1s` / `1w` / `1y` | +1 sec / +7 days / +12 months | single-letter shortcuts |

Units and their accepted spellings (single-letter shortcut, or an initial
substring of the full name — minimum 3 letters, a trailing `s` is ignored):

| unit | spelled forms | shortcut | class |
| --- | --- | --- | --- |
| second | `sec`, `secs`, `second(s)` | `s` | second (keeps base time; trailing time ignored) |
| minute | `min`, `mins`, `minute(s)` | `m` | second |
| hour | `hou`, `hour(s)` | `h` | second |
| day | `day(s)` | `d` | day (keeps base time; explicit time allowed) |
| week | `wee`, `week(s)` | `w` | day |
| month | `mon`, `month(s)` | — (`m` = minutes!) | month (snaps to midnight) |
| year | `yea`, `year(s)` | `y` | month |

Calendar clamping: month arithmetic keeps the same day-of-month when possible,
else backs off to the last valid day — `2024-01-31 + 1 month` → **2024-02-29**
(leap) / `2023-01-31 + 1 month` → 2023-02-28; `2024-03-31 + 1 month` → 2024-04-30;
`2024-02-29 + 1 year` → 2025-02-28.

Trailing time after second-class units is **ignored**: `3h 15:00` → 15:34:56.
The sign marker is the sole sign control: `-2 days ago` and `-2 days hence`
are **errors** (the leading `-` already consumed the sign). A leading `-`
must always be consumed: `-3pm`, `-march`, `-yesterday`, `-eod` are errors.
Compound intervals (`3 months 2 days`, `1 hour 30 minutes`) are **errors**.

## Times

| expression | result (same day) | notes |
| --- | --- | --- |
| `18:03` | 18:03:00 | formal |
| `18:03:40` | 18:03:40 | optional seconds |
| `18:03:40.25` | 18:03:40.250000 | fractional seconds, up to 6 digits (micros) |
| `6.03pm` | 18:03:00 | informal dot form |
| `8pm` / `2am` | 20:00 / 02:00 | bare hour + am/pm |
| `9.05am` / `9:05` | 09:05:00 | informal may carry am/pm |
| `4pm` / `12am` | 16:00 / 00:00 | `12am` = **midnight** (chrono-english left it as 12:00 noon) |
| `12pm` / `12:00pm` | 12:00 | noon (chrono-english errored: 12+12 = 24h) |
| `24:00` / `25:00` | error | hours must be 0–23 |
| `2017-06-30 08:20:30 +02:00` | 2017-06-30 06:20:30 UTC | timezone offset `±HH[:MM]` or `±HHMM`; `Z` = UTC |
| `2017-06-30 08:20:30 +0200` | 2017-06-30 06:20:30 UTC | no colon form |
| `2017-06-30T08:20:30Z` | 2017-06-30 08:20:30 UTC | ISO `T` + `Z` |

A bare time (`8pm`, `18:03`) applies to **today**. An absolute date without a
time part is midnight; a relative day/month interval without a time part uses
the rules in the interval table above.

## Day alignment (`eod` / `end` / `start`)

`start` resolves to **00:00:00.000000000** of the computed date; `eod` and
`end` to the **last moment of the day, 23:59:59.999999999** (the `Epoch`
boundary in im truncates to whole seconds, i.e. 23:59:59 — same as
`date::day_end`). All three are case-insensitive and usable as the time part
of any date form, or bare (today).

| expression | result (base 2024-03-14) | notes |
| --- | --- | --- |
| `eod` / `end` | 2024-03-14 23:59:59.999999999 | bare → today |
| `start` | 2024-03-14 00:00:00.000000000 | bare → today |
| `tomorrow eod` | 2024-03-15 23:59:59.999999999 | after keywords |
| `yesterday start` | 2024-03-13 00:00:00 | |
| `next friday eod` | 2024-03-22 23:59:59.999999999 | after weekdays |
| `friday eod` | 2024-03-15 23:59:59.999999999 | |
| `2 days eod` | 2024-03-16 23:59:59.999999999 | after day-class intervals |
| `3 days hence eod` | 2024-03-17 23:59:59.999999999 | marker + alignment |
| `6 months start` | 2024-09-14 00:00:00 | after month-class intervals |
| `9/11 eod` | 2024-11-09 23:59:59.999999999 | after day-month forms |
| `2024-03-15 eod` | 2024-03-15 23:59:59.999999999 | after absolute dates |
| `8pm eod` | 20:00 — specifier **ignored** | trailing junk after bare am/pm is dropped (chrono-english parity) |
| `18:03 eod` | error `expected am or pm` | a formal time rejects a trailing specifier; use one or the other |

(The `8pm eod` row describes the lenient `parse_date_string`; through
im's strict entry point it errors like every other trailing word.)

## Dialect: `Uk` vs `Us`

The dialect changes exactly two things; everything else parses identically:

| form | `Dialect::Uk` (used by im) | `Dialect::Us` |
| --- | --- | --- |
| `3/5/2024` (slash dates) | 2024-**05-03** (day/month) | 2024-03-**05** (month/day) |
| `explicit next <weekday>` | weekday of **next week** (+7d): `next mon` from Thu Mar 14 → Mar 25 | just the next occurrence: → Mar 18 |
| bare `<weekday>` | next occurrence | same as Uk |
| `9/11` | 9 November | September 11 |

## Not supported (all verified errors)

- Spelled-out numbers (`two days`), ordinals (`5th of may`, `2nd monday`), floats (`1.5 hours`)
- Connectors / articles: `at`, `in`, `of`, `a/an` (`tomorrow at 9am`, `in 3 days`, `a week ago`)
- Compound intervals (`3 months 2 days`), `month day year` without comma, `jan 2025`
- Keywords `next week`, `noon`/`midnight`/`midday` (`last month` and
  `next month` are **errors** — "month" is not an initial substring of a
  weekday or month name; the old first-three-letters rule parsed them as
  last/next Monday)
- Hours > 23 (`24:00`), `2023-02-29` (invalid calendar dates → "bad date"),
  `-2 days ago` / `-2 days hence` (marker after a consumed sign)
- `5 may 8pm` (ambiguous: `8` is consumed as the day) — but `April 1 8.30pm` works

## Strict vs lenient entry points

`jiff-english` exposes three entry points; im uses **`parse_strict`**:

| entry point | trailing input after a valid expression |
| --- | --- |
| `parse_date_string` | **ignored** (chrono-english parity: `"10pm meeting"` → 22:00 today) |
| `parse_strict` | **error** (`trailing characters after date expression`; trailing whitespace is fine) |
| `parse_and_remainder` | ignored, and returns the leftover input as `(Zoned, &str)` — chrono-style, the core both above delegate to |

Consequence for the CLI: a `@<time>` field must be **one complete date
expression** — `im @10pm meeting` and `! @10pm meeting` are errors
(use `:name` markers: `! @10pm :meeting`), while `@10pm` (with or without
trailing whitespace) and multi-word forms like `@tomorrow eod` all work.

## Wiring in im

- `crates/jiff-english` is a workspace member (path dependency of the root
  crate). Its tests pin the exact behavior of every row above, including
  DST transitions (jiff's compatible disambiguation) and month clamping.
- `src/date/parse.rs`: `parse_datetime` (→ epoch seconds), `parse_date` (aligns
  to day start), `parse_datetime_end` (aligns to day end) — all delegate to
  `jiff_english::parse_strict` with `crate::date::DATE_DIALECT`
  (`Dialect::Uk`) and `jiff::Zoned::now()` as the anchor. Strict means the
  whole field must be one complete expression (see the table above); the
  lenient `parse_date_string` stays available inside jiff-english. The
  result is a `Zoned`; epoch seconds are taken directly
  (`timestamp().as_second()`), no chrono bridge. `chrono` remains only as a
  dev-dependency for the integration tests.
- Every `@<time>` / `@<date>` CLI/TUI argument accepts these same expressions,
  so `im @eod`-style arguments work everywhere a time is accepted.
- Interactive dates (e.g. due-date prompts) parse through the same entry point.
