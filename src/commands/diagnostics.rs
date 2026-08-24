use anyhow::{Context, Result};
use crossterm::style::Stylize;
use std::io::{BufRead, Write};

use crate::cli::CliOpts;
use crate::config::Config;
use crate::global;

/// `:embed` — read one text line at a time from stdin, print the embedding
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

/// `:color <mood>` — diagnostic: embed the mood string with the
/// configured `moods.prefix_string`, run it through the full three-step mood-color
/// pipeline, and print intermediate values at each stage plus the final
/// Oklab / sRGB colour (with a terminal swatch of the final colour).
pub(super) fn diagnose_color<W: Write>(
    mood: &str,
    config: &Config,
    axes: &crate::color::ColorAxes,
    opts: &CliOpts,
    out: &mut W,
) -> Result<()> {
    let embedder = global::embedder();
    let mood = mood.trim();

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
    writeln!(
        out,
        "swatch     : {}",
        "        ".on(crossterm::style::Color::Rgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }),
    )?;
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
