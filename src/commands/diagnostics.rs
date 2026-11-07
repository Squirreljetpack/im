use anyhow::{Context, Result};
use crossterm::style::Stylize;
use oklab::Oklab;
use std::io::{BufRead, Write};

use crate::cli::{CliOpts, DatetimeFormat};
use crate::config::Config;
use crate::global;

/// `:tool embed` — read one text line at a time from stdin, print the embedding
/// vector for each line as space-separated floats.
///
/// Diagnostic tool: uses raw text (no `im ` prefix) so users can probe
/// arbitrary strings independent of the runtime mood encoding.
pub fn print_embeddings<R: BufRead, W: Write>(reader: &mut R, out: &mut W) -> Result<()> {
    let embedder = global::embedder();

    for line in reader.lines() {
        let line = line.context("Failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let vector = embedder.embed(&line, "")?;
        writeln!(out, "{}", global::format_vector(&vector))?;
    }
    Ok(())
}

/// `:tool datetime [expr]` — resolve a `jiff-english` date/time expression
/// against the current instant and print the result in `format`. An empty
/// expression means `now`.
pub(super) fn print_datetime<W: Write>(
    expr: &[String],
    format: DatetimeFormat,
    quiet: bool,
    out: &mut W,
) -> Result<()> {
    let joined = expr.join(" ");
    let expr = joined.trim();
    let expr = if expr.is_empty() { "now" } else { expr };

    let zdt = jiff_english::parse_strict(expr, &jiff::Zoned::now(), crate::date::DATE_DIALECT)
        .with_context(|| format!("Failed to parse datetime: '{expr}'"))?;

    if !quiet && let DatetimeFormat::Zone(zone) = &format {
        eprintln!("{}", zone_summary(zone, &zdt));
    }
    writeln!(out, "{}", render_datetime(&zdt, &format)?)?;
    Ok(())
}

/// The resolved zone and its compact offset at the parsed instant, e.g.
/// `America/New_York, -5h`. A fixed offset has no name, so only the offset
/// is shown.
fn zone_summary(zone: &jiff::tz::TimeZone, zdt: &jiff::Zoned) -> String {
    let timestamp = zdt.timestamp();
    let offset = zone.to_offset(timestamp).seconds();
    let hint = crate::date::LogTime::new(timestamp.as_second(), offset).offset_hint();
    match zone.iana_name() {
        Some(name) => format!("{name}, {hint}"),
        None => hint,
    }
}

/// Render one parsed instant in the selected output format. Kept separate
/// from [`print_datetime`] so tests can drive fixed instants and zones.
fn render_datetime(zdt: &jiff::Zoned, format: &DatetimeFormat) -> Result<String> {
    let rendered = match format {
        DatetimeFormat::Utc => zdt.timestamp().to_string(),
        DatetimeFormat::Civil => strtime("%Y-%m-%d %H:%M:%S", zdt)?,
        DatetimeFormat::UnixSeconds => zdt.timestamp().as_second().to_string(),
        DatetimeFormat::Short => {
            crate::date::format::format_human_datetime(zdt.timestamp().as_second(), true)
        }
        DatetimeFormat::Iso => strtime("%Y-%m-%dT%H:%M:%S%:z", zdt)?,
        DatetimeFormat::Date => strtime("%Y-%m-%d", zdt)?,
        DatetimeFormat::Time => strtime("%H:%M:%S", zdt)?,
        DatetimeFormat::Dmy => strtime("%d/%m/%y", zdt)?,
        DatetimeFormat::Mdy => strtime("%m/%d/%y", zdt)?,
        DatetimeFormat::Zone(zone) => {
            let zoned = zdt.with_time_zone(zone.clone());
            if zone.iana_name().is_some() {
                strtime("%Y-%m-%d %H:%M:%S %Z", &zoned)?
            } else {
                strtime("%Y-%m-%d %H:%M:%S %:z", &zoned)?
            }
        }
        DatetimeFormat::Pattern(pattern) => jiff::fmt::strtime::format(pattern, zdt)
            .with_context(|| format!("Invalid datetime format '{pattern}'"))?,
    };
    Ok(rendered)
}

fn strtime(format: &str, zdt: &jiff::Zoned) -> Result<String> {
    jiff::fmt::strtime::format(format, zdt)
        .with_context(|| format!("Failed to format datetime as '{format}'"))
}

/// `:tool mood [mood]` — with a mood, the full pipeline diagnostic; without
/// one, the mood palette: every anchor mood of the moods file with a swatch
/// of its colour, one per line.
pub(super) fn diagnose_color<W: Write>(
    mood: Option<&str>,
    config: &Config,
    axes: &crate::color::ColorAxes,
    opts: &CliOpts,
    out: &mut W,
) -> Result<()> {
    match mood.map(str::trim).filter(|mood| !mood.is_empty()) {
        Some(mood) => diagnose_mood(mood, config, axes, opts, out),
        None => print_mood_swatches(axes, out),
    }
}

/// One `mood: <swatch>` line per anchor mood, in moods-file order.
fn print_mood_swatches<W: Write>(axes: &crate::color::ColorAxes, out: &mut W) -> Result<()> {
    for basis in &axes.basis_moods {
        writeln!(out, "{}: {}", basis.mood, swatch(basis.oklab))?;
    }
    Ok(())
}

/// Eight spaces on the colour's background: a terminal colour cell that
/// needs no glyph support (crossterm emits the SGR sequence itself).
fn swatch(color: Oklab) -> String {
    "        "
        .on(crate::color::conversion::oklab_to_crossterm(color))
        .to_string()
}

/// Embed the mood string with the configured `moods.prefix_string`, run it
/// through the full three-step mood-color pipeline, and print intermediate
/// values at each stage plus the final Oklab / sRGB colour (with a terminal
/// swatch of the final colour).
fn diagnose_mood<W: Write>(
    mood: &str,
    config: &Config,
    axes: &crate::color::ColorAxes,
    opts: &CliOpts,
    out: &mut W,
) -> Result<()> {
    let embedder = global::embedder();

    // Verbose: dump the full axes settings up front; the per-value lines that
    // used to follow are gone — the dump carries them.
    if opts.verbose() {
        dbg!(&config.moods.axes);
    }

    // Embed the mood with the same prefix as the production pipeline.
    let embedding = embedder
        .embed(mood, &config.moods.axes.prefix_string)
        .context("Failed to embed mood")?;

    // The diagnostic always runs the full pipeline (no cached score).
    let weights = axes.regression_weights(&embedding, embedder, Err(mood));
    let final_oklab = axes.weights_to_color(weights.as_ref());
    let rgb = final_oklab.to_srgb();

    let raw_emb = embedder.embed(mood, "").unwrap_or_default();
    let saliency = weights.as_ref().map(|w| w.saliency).unwrap_or(1.0);
    let s_eff = axes.effective_saliency(saliency);

    // Shift vector = prefixed embedding relative to the neutral base — the
    // vector the NNLS regression projects onto (see `regression_weights`).
    let shift: Vec<f32> = embedding
        .iter()
        .zip(&axes.base_vector)
        .map(|(e, b)| e - b)
        .collect();
    let cos_raw_shift = global::cosine_similarity(&raw_emb, &shift);

    // --- output ---
    writeln!(out, "mood              : {mood}")?;
    writeln!(
        out,
        "embedding         : {} floats (first 8: {:?}...)",
        embedding.len(),
        &embedding[..8.min(embedding.len())]
    )?;
    writeln!(
        out,
        "cos sim(raw,shift): {}",
        match cos_raw_shift {
            Some(c) => format!("{c:.4}"),
            None => "(undefined)".to_string(),
        }
    )?;
    writeln!(out, "saliency score    : {saliency:.4}",)?;
    writeln!(out, "effective saliency: {s_eff:.4}")?;

    // Regression weights: raw NNLS weights and the rescaled (power-weighted,
    // normalized) weights used for the Oklab blend, per contributing mood.
    match &weights {
        Some(reg) => {
            let raw = reg
                .raw
                .iter()
                .map(|(i, w)| format!("{}: {w:.4}", axes.basis_moods[*i].mood))
                .collect::<Vec<_>>()
                .join(", ");
            let rescaled = reg
                .rescaled
                .iter()
                .zip(&reg.raw)
                .map(|(w, (i, _))| format!("{}: {w:.4}", axes.basis_moods[*i].mood))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(out, "regression weights (NNLS)     : {raw}")?;
            writeln!(out, "regression weights (rescaled) : {rescaled}")?;
        }
        None => {
            writeln!(out, "regression weights (NNLS)     : (none)")?;
        }
    }
    writeln!(out)?;

    writeln!(
        out,
        "final Oklab: (L={l:.4}, a={a:.4}, b={b:.4})",
        l = final_oklab.l,
        a = final_oklab.a,
        b = final_oklab.b,
    )?;
    writeln!(
        out,
        "final sRGB : #{r:02X}{g:02X}{b:02X}",
        r = rgb.r,
        g = rgb.g,
        b = rgb.b,
    )?;
    // Real swatch rendered directly (works when stdout is a tty); the hex
    // line above is the capture-safe record. Debug builds additionally
    // print the copy-paste printf command for non-tty captures.
    writeln!(out, "swatch     : {}", swatch(final_oklab))?;
    #[cfg(debug_assertions)]
    writeln!(
        out,
        "to visualise (copy-paste): printf \"\\x1b[48;2;{r};{g};{b}m  \\x1b[0m\"",
        r = rgb.r,
        g = rgb.g,
        b = rgb.b,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(expr: &[&str], format: DatetimeFormat) -> String {
        let expr: Vec<String> = expr.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        print_datetime(&expr, format, true, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn test_print_datetime_formats() {
        let expr = &["2024-03-15", "14:30:00"];
        assert_eq!(render(expr, DatetimeFormat::Civil), "2024-03-15 14:30:00\n");

        let epoch = crate::date::parse_datetime("2024-03-15 14:30:00", crate::date::DATE_DIALECT)
            .unwrap();
        assert_eq!(
            render(expr, DatetimeFormat::UnixSeconds),
            format!("{epoch}\n")
        );
        assert_eq!(
            render(expr, DatetimeFormat::Short),
            format!("{}\n", crate::date::format::format_human_datetime(epoch, true))
        );

        // UTC is always RFC 3339 with a `Z` suffix; the civil clock may
        // differ from the local one depending on the system zone.
        let utc = render(expr, DatetimeFormat::Utc);
        assert!(utc.starts_with("2024-03-15T"), "{utc}");
        assert!(utc.ends_with("Z\n"), "{utc}");
    }

    #[test]
    fn test_print_datetime_defaults_to_now() {
        let rendered = render(&[], DatetimeFormat::UnixSeconds);
        let now = crate::date::now();
        let printed: i64 = rendered.trim().parse().unwrap();
        assert!((printed - now).abs() <= 2, "got {printed}, now {now}");
    }

    #[test]
    fn test_print_datetime_rejects_trailing_words() {
        let expr = vec!["10pm".to_string(), "meeting".to_string()];
        let mut out = Vec::new();
        assert!(print_datetime(&expr, DatetimeFormat::Civil, true, &mut out).is_err());
    }

    #[test]
    fn test_parse_datetime_format_values() {
        use crate::cli::parse_datetime_format;
        use jiff::Timestamp;
        use jiff::tz::TimeZone;

        assert_eq!(parse_datetime_format("civil").unwrap(), DatetimeFormat::Civil);
        assert_eq!(parse_datetime_format("UTC").unwrap(), DatetimeFormat::Utc);
        assert_eq!(parse_datetime_format("unix").unwrap(), DatetimeFormat::UnixSeconds);
        assert_eq!(parse_datetime_format("iso").unwrap(), DatetimeFormat::Iso);
        assert_eq!(parse_datetime_format("dmy").unwrap(), DatetimeFormat::Dmy);
        assert_eq!(parse_datetime_format("%d %m %y").unwrap(), DatetimeFormat::Pattern("%d %m %y".to_string()));

        match parse_datetime_format("+8").unwrap() {
            DatetimeFormat::Zone(zone) => {
                assert!(zone.iana_name().is_none());
                assert_eq!(zone.to_offset(Timestamp::UNIX_EPOCH).seconds(), 8 * 3600);
            }
            other => panic!("expected a fixed-offset zone, got {other:?}"),
        }
        match parse_datetime_format("-05:30").unwrap() {
            DatetimeFormat::Zone(zone) => {
                assert_eq!(zone.to_offset(Timestamp::UNIX_EPOCH).seconds(), -5 * 3600 - 1800);
            }
            other => panic!("expected a fixed-offset zone, got {other:?}"),
        }
        match parse_datetime_format("NewYork").unwrap() {
            DatetimeFormat::Zone(zone) => {
                assert_eq!(zone, TimeZone::get("America/New_York").unwrap());
            }
            other => panic!("expected a named zone, got {other:?}"),
        }
        match parse_datetime_format("singapore").unwrap() {
            DatetimeFormat::Zone(zone) => {
                let name = zone.iana_name().unwrap_or_default();
                assert_eq!(
                    name.rsplit('/').next(),
                    Some("Singapore"),
                    "unexpected zone {name}"
                );
            }
            other => panic!("expected a named zone, got {other:?}"),
        }
        assert!(
            parse_datetime_format("america").is_err(),
            "an ambiguous zone substring should be rejected"
        );
        assert!(parse_datetime_format("bogus").is_err());
    }

    #[test]
    fn test_render_datetime_formats_and_zones() {
        use jiff::tz::TimeZone;

        let zdt = jiff::civil::date(2024, 3, 15)
            .at(14, 30, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        assert_eq!(render_datetime(&zdt, &DatetimeFormat::Date).unwrap(), "2024-03-15");
        assert_eq!(render_datetime(&zdt, &DatetimeFormat::Time).unwrap(), "14:30:00");
        assert_eq!(render_datetime(&zdt, &DatetimeFormat::Dmy).unwrap(), "15/03/24");
        assert_eq!(render_datetime(&zdt, &DatetimeFormat::Mdy).unwrap(), "03/15/24");
        assert_eq!(render_datetime(&zdt, &DatetimeFormat::Iso).unwrap(), "2024-03-15T14:30:00+00:00");
        assert_eq!(
            render_datetime(&zdt, &DatetimeFormat::Pattern("%d %m %y".to_string())).unwrap(),
            "15 03 24"
        );

        let plus8 = crate::cli::parse_datetime_format("+08:00").unwrap();
        assert_eq!(
            render_datetime(&zdt, &plus8).unwrap(),
            "2024-03-15 22:30:00 +08:00"
        );
        let new_york = crate::cli::parse_datetime_format("NewYork").unwrap();
        assert!(
            render_datetime(&zdt, &new_york)
                .unwrap()
                .starts_with("2024-03-15 10:30:00 "),
            "New York rendering should shift the clock"
        );
        match &new_york {
            DatetimeFormat::Zone(zone) => {
                assert_eq!(zone_summary(zone, &zdt), "America/New_York, -4h");
            }
            other => panic!("expected a named zone, got {other:?}"),
        }
        match &plus8 {
            DatetimeFormat::Zone(zone) => assert_eq!(zone_summary(zone, &zdt), "+8h"),
            other => panic!("expected a fixed zone, got {other:?}"),
        }

        assert!(
            render_datetime(&zdt, &DatetimeFormat::Pattern("%".to_string())).is_err(),
            "an incomplete pattern should report an error"
        );
    }
}
