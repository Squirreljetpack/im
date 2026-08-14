# Date and time expressions

Use a date or time expression after `@`. Dates use **UK format**: day before
month (`9/11` means 9 November).

## Common examples

```text
@today                 today
@tomorrow              tomorrow
@yesterday             yesterday
@friday                the next Friday
@next mon              Monday of next week
@4 july                4 July
@9/11/2025             9 November 2025
@2025-11-09            9 November 2025
@tomorrow 9am          tomorrow at 09:00
@friday eod            the end of the next Friday
@3 days                three days from now
@2 weeks ago           two weeks ago
```

## Accepted forms

### Dates

- `today`, `tomorrow`, `yesterday`, or `now`
- A weekday: `monday`, `mon`, `next friday`, `last tue`
- A month: `april`, `next april`, `last april`
- A day and month: `4 july`, `july 4`, `next 4 july`
- A numeric date: `9/11`, `9/11/2025`, or `2025-11-09`
- A date with a year: `4 july 2025` or `july 4, 2025`
- A bare year: `2025` (January 1)

Weekday and month names can be shortened to their first three letters. Names
are case-insensitive. `next` and `last` must be lowercase.

### Relative dates

Write a number followed by one of these units:

```text
3 seconds       15 minutes        2 hours
3 days          2 weeks           6 months
1 year
```

You can use the short forms `s`, `m`, `h`, `d`, `w`, and `y`:

```text
3h              2d ago            1w hence          -6 months
```

`ago` means backwards; `hence` and `later` mean forwards. Use only one
interval at a time: `3 days 2 hours` is not valid.

> `m` means **minutes**. Use `mon` or `months` for months.

### Times

A time by itself applies to today:

```text
8pm             09:30              18:03:40
6.30pm          12:00pm            08:20 +02:00
```

Times may follow a date or relative expression, for example:

```text
@tomorrow 9am
@2 days ago 15:00
@2025-11-09T08:20:30Z
```

Use `start` for midnight and `eod` or `end` for the end of the day:

```text
@today start
@tomorrow eod
```

## Important rules

- Separate words with spaces, but do not add connecting words:
  `tomorrow 9am` is valid; `tomorrow at 9am` is not.
- Use numbers, not words or ordinal suffixes: `2 days`, not `two days` or
  `2nd day`.
- Use day/month order for slash dates: `3/5/2025` means 3 May 2025.
- A month followed only by a year is not valid: use `1 january 2025`.
- Compound intervals are not supported: `1 hour 30 minutes` is invalid.
- The complete expression must be provided; extra words are not allowed.