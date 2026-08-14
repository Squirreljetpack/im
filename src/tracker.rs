use anyhow::{Context, Result};
use crossterm::style::{Color as CtColor, Stylize};
use sqlx::SqlitePool;
use std::io::Write;

use crate::global;
use crate::badge::completion_badge;
use crate::cli::{CliOpts, TrackerItem, TrackerPeriod};
use crate::config::{Config, TrackerKind};
use crate::date;
use crate::db::TrackerValue;

/// Read a tracker score as f64. The `score` column is stored as
/// BLOB but SQLite's dynamic typing means values can be INTEGER, REAL, or
/// TEXT. `sql::fetch_tracker_entries` selects `CAST(score AS TEXT)` so
/// every storage type decodes as a String; parse that.
pub(crate) fn score_f64(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(0.0)
}

/// Interpret a raw tracker value according to its configured kind, shared
/// by the CLI entry path and the today view's Update modal. Values are
/// parsed number-first, then as a humantime duration (e.g. `6.5`, `1h`,
/// `45s` — see [`crate::date::parse_num_or_duration`], the same parser
/// `min`/`max` config bounds use); `number` trackers additionally require
/// the parsed value to be a whole number. Text accepts the value as-is
/// (min/max ignored). Null trackers never reach this parser.
pub(crate) fn parse_tracker_value(
    tracker_type: &str,
    kind: TrackerKind,
    raw: &str,
) -> Result<TrackerValue> {
    match kind {
        TrackerKind::Text => Ok(TrackerValue::Text(raw.to_string())),
        TrackerKind::Number => {
            let f = crate::date::parse_num_or_duration(raw).map_err(|_| {
                anyhow::anyhow!(
                    "Cannot parse '{}' as an integer for tracker '{}'",
                    raw,
                    tracker_type
                )
            })?;
            if f.fract() != 0.0 || !(i64::MIN as f64..=i64::MAX as f64).contains(&f) {
                anyhow::bail!(
                    "Value '{}' for tracker '{}' is not a whole number (kind = number)",
                    raw,
                    tracker_type
                );
            }
            Ok(TrackerValue::Number(f as i64))
        }
        TrackerKind::Float => {
            let f = crate::date::parse_num_or_duration(raw).map_err(|_| {
                anyhow::anyhow!(
                    "Cannot parse '{}' as a number for tracker '{}'",
                    raw,
                    tracker_type
                )
            })?;
            Ok(TrackerValue::Float(f))
        }
        TrackerKind::Null => unreachable!("null trackers are handled by the caller"),
    }
}

/// Effective endpoints for dot binning (grid semantics). Configured
/// endpoints win; missing ones are derived from the nonzero data range.
/// With a single bound configured, that bound becomes the bad-end threshold
/// and the best observed score anchors the success end:
/// - neither configured → `(data_min, data_max)` — linear binning against the observed range;
/// - only `max` configured → `(cfg_max, data_min.min(cfg_max))` — inverted range: scores at/above `cfg_max` hit the first color, the best observed score anchors the last; collapses onto `cfg_max` when all data is at/above it;
/// - only `min` configured → `(cfg_min, data_max.max(cfg_min))` — normal range: scores at/below `cfg_min` hit the first color, the best observed score anchors the last; collapses onto `cfg_min` when all data is at/below it;
/// - both configured → used as-is `(cfg_min, cfg_max)`.
fn effective_range(
    cfg_min: Option<f64>,
    cfg_max: Option<f64>,
    nonzero: &[f64],
) -> (Option<f64>, Option<f64>) {
    let data_min = nonzero.iter().copied().reduce(f64::min);
    let data_max = nonzero.iter().copied().reduce(f64::max);

    match (cfg_min, cfg_max) {
        (Some(min), Some(max)) => (Some(min), Some(max)),
        (Some(min), None) => {
            // Floor-only: scores below cfg_min are bad; the best observed
            // score anchors the success end.
            let eff_max = data_max.map_or(min, |mx| mx.max(min));
            (Some(min), Some(eff_max))
        }
        (None, Some(max)) => {
            // Ceiling-only: scores above cfg_max are bad; the best observed
            // score anchors the success end.
            let eff_max = data_min.map_or(max, |mn| mn.min(max));
            (Some(max), Some(eff_max))
        }
        (None, None) => (data_min, data_max),
    }
}

/// Handle tracker view (`: [week|month|year] [ids]`): display
/// dot-sequence history.
pub async fn write_tracker_grid<W: Write>(
    pool: &SqlitePool,
    config: &Config,
    axes: &crate::color::ColorAxes,
    opts: &CliOpts,
    period: TrackerPeriod,
    items: Vec<TrackerItem>,
    out: &mut W,
) -> Result<()> {
    // Grid ranges follow config.grid. Non-rolling grids anchor the start
    // to the calendar period (week_start / month start) and end at today;
    // rolling grids use a fixed-size window — the full week (always 7 dots) or
    // the "last 4 weeks" window from the mood/ subrepo.
    let gv = &config.grid;
    let (start_epoch, end_epoch) = match period {
        TrackerPeriod::Week => {
            if gv.week_rolling {
                // Rolling 7-day window ending today.
                let start = date::today_start() - 6 * 86400;
                (start, date::today_end())
            } else {
                // Calendar week so far: from week_start through today.
                let ws = date::week_start(gv.week_start.into());
                (ws, date::today_end())
            }
        }
        TrackerPeriod::Month => {
            if gv.month_rolling {
                // Rolling 4-week window ending today, aligned to week_start.
                (
                    date::rolling_month_start(gv.week_start.into()),
                    date::today_end(),
                )
            } else {
                // Month so far: from the month start through today.
                (date::month_start(), date::today_end())
            }
        }
        TrackerPeriod::Year => {
            if gv.year_rolling {
                // Calendar year aligned to week_start: start from the week_start on
                // or before Jan 1, through today. The grid never opens with blank
                // cells in the first column.
                (
                    date::aligned_year_start(gv.week_start.into()),
                    date::today_end(),
                )
            } else {
                // Calendar year (January 1 through today). First column may have
                // blank rows if Jan 1 doesn't fall on week_start.
                (date::year_start(), date::today_end())
            }
        }
    };

    for (i, item) in items.iter().enumerate() {
        // Section header: at -v a title line (the ({period:?}) suffix only
        // from -vv); otherwise a blank-line separator — skipped before the
        // first item so there's no double leading newline.
        if i > 0 && !opts.verbose() {
            writeln!(out)?;
        }
        match item {
            TrackerItem::Mood => {
                // Positional mood-grid marker: render the mood dots grid here.
                if opts.verbose() {
                    writeln!(out, "{}", grid_title("Moods", period, opts.verbose_level()))?;
                }
                display_mood_tracker(pool, config, axes, start_epoch, end_epoch, period, out)
                    .await?;
            }
            TrackerItem::Tracker(id_str) => {
                if let Some(name) = id_str.strip_prefix('@') {
                    // Recurring task: display completion dots
                    if opts.verbose() {
                        writeln!(
                            out,
                            "{}",
                            grid_title(&format!("@{name}"), period, opts.verbose_level())
                        )?;
                    }
                    display_recurring_tracker(
                        pool,
                        config,
                        name,
                        start_epoch,
                        end_epoch,
                        period,
                        out,
                        None,
                    )
                    .await?;
                } else {
                    // Tracker: display score dots
                    if opts.verbose() {
                        writeln!(out, "{}", grid_title(id_str, period, opts.verbose_level()))?;
                    }
                    display_tracker(
                        pool,
                        config,
                        id_str,
                        start_epoch,
                        end_epoch,
                        period,
                        opts,
                        out,
                        None,
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

/// Grid section title: the bare label at `-v` (e.g. `Moods`, `idea`,
/// `@name`); the ` ({period:?})` suffix only at `-vv` and above.
fn grid_title(label: &str, period: TrackerPeriod, verbose_level: u8) -> String {
    if verbose_level >= 2 {
        format!("{label} ({period:?})")
    } else {
        label.to_string()
    }
}

async fn display_mood_tracker<W: Write>(
    pool: &SqlitePool,
    config: &Config,
    axes: &crate::color::ColorAxes,
    start_epoch: i64,
    end_epoch: i64,
    period: TrackerPeriod,
    out: &mut W,
) -> Result<()> {
    // Fetch mood entries in the period, grouped by day. Journal-only entries
    // (empty mood → no embedding) are excluded from the grid.
    let moods: Vec<crate::db::MoodRow> =
        crate::db::fetch_moods_between(pool, start_epoch, end_epoch)
            .await?
            .into_iter()
            .filter(|f| !f.mood.is_empty())
            .collect();

    if moods.is_empty() {
        writeln!(out, "No mood entries in this period.")?;
        return Ok(());
    }

    let embedder = global::embedder();

    let day_secs: i64 = 86400;
    let num_days = ((end_epoch - start_epoch) / day_secs + 1) as usize;
    let mut day_moods: Vec<Vec<&crate::db::MoodRow>> = vec![Vec::new(); num_days];
    let mut day_has_entry: Vec<bool> = vec![false; num_days];

    for f in &moods {
        let time = f.time;
        let day_idx = ((time - start_epoch) / day_secs) as usize;
        if day_idx >= num_days {
            continue;
        }
        day_has_entry[day_idx] = true;
        day_moods[day_idx].push(f);
    }

    let mut day_colors: Vec<Option<oklab::Oklab>> = vec![None; num_days];

    for (i, moods_in_day) in day_moods.iter().enumerate() {
        if moods_in_day.is_empty() {
            continue;
        }
        // Saliency-weighted day average, v̄ = Σ vᵢ·sᵢᵏ / Σ sᵢᵏ, with k =
        // `grid_blend_steepness` and s the mood's saliency. When every
        // saliency is zero this degrades to a plain average of the
        // embeddings. The day's saliency score passed to the regression
        // below stays a plain unweighted mean.
        let steepness = config.moods.axes.grid_blend_steepness;
        let mut emb_sum: Vec<f32> = Vec::new();
        let mut emb_plain: Vec<f32> = Vec::new();
        let mut score_sum: f32 = 0.0;
        // let mut score_wsum: f32 = 0.0; // s^k-weighted saliency sum (alternative)
        let mut weight_sum: f32 = 0.0;
        let mut count: usize = 0;

        for f in moods_in_day {
            let emb = match f
                .embedding
                .as_deref()
                .and_then(global::blob_to_embedding)
            {
                Some(e) => Some(e),
                None => embedder.embed(&f.mood, &axes.prefix_string).ok(),
            };
            let Some(emb) = emb else { continue };

            let score = match f.score {
                Some(s) => s,
                None => crate::color::predict_saliency(embedder, &f.mood),
            };

            let weight = score.powf(steepness);
            if emb_sum.is_empty() {
                emb_sum = emb.iter().map(|&e| e * weight).collect();
                emb_plain = emb;
            } else {
                for ((acc, plain), e) in emb_sum.iter_mut().zip(emb_plain.iter_mut()).zip(&emb) {
                    *acc += e * weight;
                    *plain += e;
                }
            }
            score_sum += score; // plain unweighted day-saliency sum (active)
            // score_wsum += score * weight; // s^k-weighted variant (alternative)
            weight_sum += weight;
            count += 1;
        }

        if count > 0 {
            let inv_total = if weight_sum > 0.0 {
                1.0 / weight_sum
            } else {
                // Every saliency was zero → plain average of the
                // embeddings (the day's saliency is zero as well).
                emb_sum = emb_plain;
                1.0 / count as f32
            };
            for e_elem in &mut emb_sum {
                *e_elem *= inv_total;
            }
            // Day saliency for the NNLS regression: plain unweighted mean.
            let avg_score = score_sum / count as f32;
            // Saliency-weighted alternative (Σ sᵢ·sᵢᵏ / Σ sᵢᵏ), matching
            // the embedding blend above; re-enable together with score_wsum:
            // let avg_score = score_wsum * inv_total;

            let reg = axes.regression_weights(&emb_sum, embedder, Ok(avg_score));
            let oklab = axes.weights_to_color(reg.as_ref());
            day_colors[i] = Some(oklab);
        }
    }

    // The grid body follows; the section title (if any) is printed by
    // write_tracker_grid.

    // Year grids always use the heatmap layout: one column per week, one
    // row per weekday (rows start at grid.week_start), dots from the
    // window start through today.
    if period == TrackerPeriod::Year {
        render_year_heatmap(out, &day_colors, &day_has_entry, start_epoch, config)?;
        return Ok(());
    }

    // Print dots: colored by the average mood color of each day when
    // available, otherwise a plain filled dot for days with an entry and ◯
    // for empty days. Dots are separated by two spaces and wrap at 7 per
    // row (the last row may be short, e.g. a month that ends mid-week).
    for (i, &oklab_opt) in day_colors.iter().enumerate() {
        let d = if !day_has_entry[i] {
            "◯".to_string()
        } else if let Some(oklab) = oklab_opt {
            "●"
                .with(crate::color::conversion::oklab_to_crossterm(oklab))
                .to_string()
        } else {
            "●".to_string()
        };
        write!(out, "{}", d)?;
        if (i + 1) % 7 == 0 || i == num_days - 1 {
            writeln!(out)?;
        } else {
            write!(out, "  ")?;
        }
    }

    Ok(())
}

/// Year heatmap: one column per week, one row per weekday (rows start at
/// `grid.week_start`, so Monday is the top row by default). The window
/// runs from `start_epoch` through today — the calendar year (Jan 1) when
/// `grid.year_rolling` is false, otherwise the calendar year aligned to
/// a full week start (the week_start on or before Jan 1, so the grid never
/// opens with blank cells in the first column). Days before Jan 1 (when
/// year_rolling is true) render as single spaces. Days after today in the
/// last partial week also render as spaces. There is no horizontal spacing
/// between columns.
fn render_year_heatmap<W: Write>(
    out: &mut W,
    day_colors: &[Option<oklab::Oklab>],
    day_has_entry: &[bool],
    start_epoch: i64,
    config: &Config,
) -> Result<()> {
    use jiff::civil::{Date, Weekday};

    let week_start: Weekday = config.grid.week_start.into();
    let start_date = crate::date::zoned_from_unix_secs(start_epoch)
        .map(|z| z.date())
        .context("year heatmap: start_epoch is not a valid local date")?;
    let today = jiff::Zoned::now().date();
    let jan1 = Date::new(today.year(), 1, 1).context("year heatmap: failed to build Jan 1")?; // Jan 1 of current year

    // Row = weekday offset from week_start; a week ends the day before
    // week_start (mirrors the subrepo's week_end_day).
    let weekday_row = |wd: Weekday| -> usize {
        let start_num = week_start.to_monday_zero_offset();
        let wd_num = wd.to_monday_zero_offset();
        ((wd_num + 7 - start_num) % 7) as usize
    };
    let week_end = match week_start {
        Weekday::Monday => Weekday::Sunday,
        Weekday::Sunday => Weekday::Saturday,
        other => other.previous(),
    };

    // One column per real week; None marks days outside Jan 1..=today.
    let mut weeks: Vec<[Option<usize>; 7]> = Vec::new();
    let mut week: [Option<usize>; 7] = [None; 7];
    let mut date = start_date;
    let mut day = 0usize;
    while date <= today {
        week[weekday_row(date.weekday())] = Some(day);
        if date.weekday() == week_end || date == today {
            weeks.push(week);
            week = [None; 7];
        }
        day += 1;
        date = date
            .checked_add(jiff::Span::new().days(1))
            .context("year heatmap: date overflow")?;
    }

    for row in 0..7 {
        for w in &weeks {
            match w[row] {
                Some(day) => {
                    let day_date = start_date
                        .checked_add(jiff::Span::new().days(day as i64))
                        .context("year heatmap: date overflow")?;
                    if day_date < jan1 {
                        // Day is before Jan 1 (previous year), render as space
                        write!(out, " ")?;
                    } else if !day_has_entry[day] {
                        write!(out, "·")?;
                    } else if let Some(oklab) = day_colors[day] {
                        write!(
                            out,
                            "{}",
                            "●".with(crate::color::conversion::oklab_to_crossterm(oklab))
                        )?;
                    } else {
                        write!(out, "●")?;
                    }
                }
                None => write!(out, " ")?,
            }
        }
        writeln!(out)?;
    }

    Ok(())
}

async fn display_tracker<W: Write>(
    pool: &SqlitePool,
    config: &Config,
    tracker_type: &str,
    start_epoch: i64,
    end_epoch: i64,
    period: TrackerPeriod,
    opts: &CliOpts,
    out: &mut W,
    wrap: Option<usize>,
) -> Result<()> {
    let tracker = config
        .tracker
        .get(tracker_type)
        .ok_or_else(|| anyhow::anyhow!("Unknown tracker '{}' not found in config", tracker_type))?;

    // Fetch all entries in the period
    let entries =
        crate::db::fetch_tracker_entries(pool, tracker_type, start_epoch, end_epoch).await?;

    if entries.is_empty() {
        writeln!(
            out,
            "No entries for tracker '{}' in this period.",
            tracker_type
        )?;
        return Ok(());
    }

    // Text trackers list their entries as indented lines instead of dots;
    // at -v each line gains the entry's own timestamp in Darkgray.
    if tracker.kind == TrackerKind::Text {
        for entry in &entries {
            write!(out, "{}", "> ".with(CtColor::DarkGrey))?;
            write!(out, "{}", entry.score)?;
            if opts.verbose() {
                writeln!(
                    out,
                    "{}",
                    format!(" [{}]", crate::date::format_datetime_short(entry.time))
                        .with(CtColor::DarkGrey)
                )?;
            } else {
                writeln!(out)?;
            }
        }
        return Ok(());
    }

    // If the tracker defines an interval, render one dot per calendar
    // interval slot (anchored at the tracker's configured anchor); otherwise
    // one dot per entry (newer entry wins the slot). Null trackers without
    // an interval are unsupported and skipped with an error.
    let colors = tracker.colors.as_ref().unwrap_or(&config.tasks.colors);
    if let Some(interval) = tracker.interval {
        let (Ok(anchor_z), Ok(start_z), Ok(end_z)) = (
            crate::date::zoned_from_unix_secs(interval.anchor),
            crate::date::zoned_from_unix_secs(start_epoch),
            crate::date::zoned_from_unix_secs(end_epoch),
        ) else {
            anyhow::bail!("Tracker '{}' has an invalid interval anchor.", tracker_type);
        };
        let start_idx =
            crate::date::interval_index(&anchor_z, &start_z, interval.span).unwrap_or(0);
        let end_idx =
            crate::date::interval_index(&anchor_z, &end_z, interval.span).unwrap_or(start_idx);
        let num_slots = (end_idx - start_idx + 1).max(0) as usize;
        let mut slot_sums: Vec<f64> = vec![0.0; num_slots];
        let mut slot_has_entry: Vec<bool> = vec![false; num_slots];
        // Null time-marker entries: remember the entry time per slot so the
        // color can be computed from the time-of-day position.
        let mut slot_time: Vec<Option<i64>> = vec![None; num_slots];

        for entry in &entries {
            let score = score_f64(&entry.score);
            let time = entry.time;
            let Ok(t_z) = crate::date::zoned_from_unix_secs(time) else {
                continue;
            };
            let Ok(idx) = crate::date::interval_index(&anchor_z, &t_z, interval.span) else {
                continue;
            };
            let idx = (idx - start_idx) as usize;
            if idx < num_slots {
                slot_sums[idx] += score;
                slot_has_entry[idx] = true;
                slot_time[idx] = Some(time);
            }
        }

        let nonzero_sums: Vec<f64> = slot_sums
            .iter()
            .zip(slot_has_entry.iter())
            .filter_map(|(&sum, &has)| {
                if has && sum.abs() > f64::EPSILON {
                    Some(sum)
                } else {
                    None
                }
            })
            .collect();

        let (eff_min, eff_max) = effective_range(tracker.min, tracker.max, &nonzero_sums);

        let use_circle = period != TrackerPeriod::Year;
        for (i, &has_entry) in slot_has_entry.iter().enumerate() {
            if !has_entry {
                if use_circle {
                    write!(out, "◯")?;
                } else {
                    write!(out, "·")?;
                }
            } else {
                let color = if tracker.kind == TrackerKind::Null {
                    crate::badge::null_tracker_color(
                        colors,
                        tracker,
                        slot_time[i].unwrap_or(0),
                        slot_sums[i],
                    )
                } else {
                    crate::badge::tracker_color(colors, slot_sums[i], eff_min, eff_max)
                };
                write!(out, "{}", "●".with(color))?;
            }
            if let Some(w) = wrap {
                if (i + 1) % w == 0 || i == num_slots - 1 {
                    writeln!(out)?;
                } else {
                    write!(out, "  ")?;
                }
            } else if i < num_slots - 1 {
                write!(out, "  ")?;
            }
        }
        writeln!(out)?;
    } else if tracker.kind == TrackerKind::Null {
        cba::ebog!(
            "tracker";
            "Null tracker '{}' has no interval: grid view is not supported (config an interval)",
            tracker_type
        );
        return Ok(());
    } else {
        // One dot per entry
        let scores: Vec<f64> = entries.iter().map(|e| score_f64(&e.score)).collect();

        let nonzero_scores: Vec<f64> = scores
            .iter()
            .filter(|&&s| s.abs() > f64::EPSILON)
            .cloned()
            .collect();

        let (eff_min, eff_max) = effective_range(tracker.min, tracker.max, &nonzero_scores);

        for &score in &scores {
            let color = crate::badge::tracker_color(colors, score, eff_min, eff_max);
            write!(out, "{}", "●".with(color))?;
        }
        writeln!(out)?;
    }
    Ok(())
}

async fn display_recurring_tracker<W: Write>(
    pool: &SqlitePool,
    config: &Config,
    name: &str,
    start_epoch: i64,
    end_epoch: i64,
    period: TrackerPeriod,
    out: &mut W,
    wrap: Option<usize>,
) -> Result<()> {
    // Find the recurring task: id can be numeric or the unique task name.
    let Some(task) = crate::db::fetch_recurring_task_meta(pool, name).await? else {
        writeln!(out, "Recurring task '{}' not found.", name)?;
        return Ok(());
    };

    let task_id = task.id;
    let start_time = task.start_time;
    let span = task.interval_secs.map(crate::date::db_to_span);
    let target_count = task.target_count;

    // Get completion events (time, count) for this task in the period
    let completions =
        crate::db::fetch_completions_between(pool, task_id, start_epoch, end_epoch).await?;

    if let (Some(st), Some(span)) = (start_time, span) {
        // For interval-based recurring tasks, show dots per interval
        // (calendar slots anchored at the task's start time), summing the
        // per-event counts into each interval.
        let (Ok(anchor_z), Ok(start_z), Ok(end_z)) = (
            crate::date::zoned_from_unix_secs(st),
            crate::date::zoned_from_unix_secs(start_epoch),
            crate::date::zoned_from_unix_secs(end_epoch),
        ) else {
            writeln!(out, "Recurring task '{}' has an invalid start time.", name)?;
            return Ok(());
        };
        let start_idx = crate::date::interval_index(&anchor_z, &start_z, span).unwrap_or(0);
        let end_idx = crate::date::interval_index(&anchor_z, &end_z, span).unwrap_or(start_idx);
        let num_intervals = (end_idx - start_idx + 1).max(0) as usize;
        let mut interval_sums: Vec<i64> = vec![0; num_intervals];

        for completion in &completions {
            let Ok(t_z) = crate::date::zoned_from_unix_secs(completion.time) else {
                continue;
            };
            let Ok(idx) = crate::date::interval_index(&anchor_z, &t_z, span) else {
                continue;
            };
            let idx = (idx - start_idx) as usize;
            if idx < num_intervals {
                interval_sums[idx] += i64::from(completion.count);
            }
        }

        for (i, sum) in interval_sums.iter().enumerate() {
            let (ch, color) = completion_badge(config, *sum, target_count);
            let d = if ch == '◯' && period == TrackerPeriod::Year {
                "·".to_string()
            } else if color == CtColor::Reset {
                ch.to_string()
            } else {
                ch.to_string().with(color).to_string()
            };
            write!(out, "{}", d)?;
            if let Some(w) = wrap {
                if (i + 1) % w == 0 || i == num_intervals - 1 {
                    writeln!(out)?;
                } else {
                    write!(out, "  ")?;
                }
            } else if i < num_intervals - 1 {
                write!(out, "  ")?;
            }
        }
        if wrap.is_none() {
            writeln!(out)?;
        }
    } else {
        // No interval: one dot per completion event, colored by its count
        if completions.is_empty() {
            writeln!(out, "No completions for '{}' in this period.", name)?;
            return Ok(());
        }

        for completion in &completions {
            let count = completion.count;
            let (ch, color) = completion_badge(config, i64::from(count), target_count);
            if color == CtColor::Reset {
                write!(out, "{}", ch)?;
            } else {
                write!(out, "{}", ch.to_string().with(color))?;
            }
        }
        writeln!(out)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_title() {
        // Bare label at -v; period suffix from -vv (and -vvv).
        assert_eq!(grid_title("Moods", TrackerPeriod::Week, 0), "Moods");
        assert_eq!(grid_title("Moods", TrackerPeriod::Week, 1), "Moods");
        assert_eq!(grid_title("Moods", TrackerPeriod::Week, 2), "Moods (Week)");
        assert_eq!(grid_title("idea", TrackerPeriod::Month, 2), "idea (Month)");
        assert_eq!(grid_title("@run", TrackerPeriod::Year, 3), "@run (Year)");
        assert_eq!(grid_title("idea", TrackerPeriod::Month, 1), "idea");
    }

    #[test]
    fn test_effective_range() {
        let data = [1.0, 2.0, 7.0, 9.0];
        // Neither configured → the data's own range.
        assert_eq!(effective_range(None, None, &data), (Some(1.0), Some(9.0)));
        // Both configured → as configured.
        assert_eq!(
            effective_range(Some(0.0), Some(10.0), &data),
            (Some(0.0), Some(10.0))
        );
        // Only max: inverted range (cfg_max, best observed) — scores at/above
        // cfg_max map to the first color, the best observed anchors the last.
        assert_eq!(
            effective_range(None, Some(8.0), &data),
            (Some(8.0), Some(1.0))
        );
        // Data entirely at/above cfg_max → the range collapses onto cfg_max
        // (degenerate → middle color at binning time).
        assert_eq!(
            effective_range(None, Some(0.5), &data),
            (Some(0.5), Some(0.5))
        );
        // Only min: normal range (cfg_min, best observed) — scores at/below
        // cfg_min map to the first color, the best observed anchors the last.
        assert_eq!(
            effective_range(Some(5.0), None, &data),
            (Some(5.0), Some(9.0))
        );
        // Data entirely at/below cfg_min → the range collapses onto cfg_min.
        assert_eq!(
            effective_range(Some(12.0), None, &data),
            (Some(12.0), Some(12.0))
        );
        // No data: a configured bound still yields the degenerate (bound, bound);
        // with no bound at all → (None, None).
        assert_eq!(effective_range(None, None, &[]), (None, None));
        assert_eq!(
            effective_range(None, Some(8.0), &[]),
            (Some(8.0), Some(8.0))
        );
        assert_eq!(
            effective_range(Some(2.0), None, &[]),
            (Some(2.0), Some(2.0))
        );
    }
}
