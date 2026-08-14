use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Open the user's preferred editor on `path`. Returns an error if neither
/// `VISUAL` nor `EDITOR` is set, if the editor fails to launch, or if it
/// exits with a non-zero status.
///
/// Used by both the mood/body editor (passing a tempfile path) and the
/// `im :config` command (passing the live config path). The body editor
/// reads back the file after save; `:config` just hands control to the editor.
pub fn open_editor_at(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .map_err(|_| anyhow::anyhow!("Neither VISUAL nor EDITOR environment variable is set. Set one to use the editor (body delimiter) feature."))?;

    let status = Command::new(&editor)
        .arg(path)
        .status()
        .with_context(|| format!("Failed to open editor: {}", editor))?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }
    Ok(())
}

/// Open the user's preferred editor on a temporary file pre-filled with
/// `initial`, and return the edited content (trimmed).
///
/// Unlike `open_editor_for_body`, there is no template and no `%%` comment
/// stripping — the temp file starts with the existing content so the user
/// edits in place. Used by the TUI Edit action for task/mood bodies and
/// tracker values.
pub fn open_editor_on_text(initial: &str) -> Result<String> {
    let mut temp_file =
        tempfile::NamedTempFile::new().context("Failed to create temporary file")?;

    write!(temp_file, "{}", initial).context("Failed to write to temporary file")?;
    temp_file.flush()?;

    open_editor_at(temp_file.path())?;

    let mut content = String::new();
    std::fs::File::open(temp_file.path())
        .context("Failed to reopen temporary file")?
        .read_to_string(&mut content)
        .context("Failed to read from temporary file")?;

    Ok(content.trim().to_string())
}

/// Strip `%%` comment blocks from editor content: a line that is exactly
/// `%%` opens (or closes) a block; the marker lines themselves and every
/// line between them are removed. An unpaired opening `%%` strips through
/// the end of the file. Lets templates carry instructions that never reach
/// the saved body.
fn strip_percent_blocks(content: &str) -> String {
    let mut in_block = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in content.lines() {
        if line == "%%" {
            in_block = !in_block;
        } else if !in_block {
            kept.push(line);
        }
    }
    kept.join("\n")
}

/// Open the user's preferred editor on a temporary file and return the
/// body content. `dots` is the bare body delimiter's dot count: `n` dots
/// open the `n`th of `templates` (1-based); the caller never passes 0.
/// Out-of-range counts write the legacy `# additional notes below` hint
/// line (its first line is dropped on read-back), an empty template path
/// starts blank, and a non-empty path that can't be read warns and falls
/// back to blank. On read-back, `%%` comment blocks are always stripped
/// and the result is trimmed. Returns an empty string if the editor
/// produced no meaningful content.
pub fn open_editor_for_body(templates: &[PathBuf], dots: usize) -> Result<String> {
    let mut temp_file =
        tempfile::NamedTempFile::new().context("Failed to create temporary file")?;

    // `n` dots seed the `n`th template (index `n - 1`); out of range,
    // write the legacy hint line instead.
    let mut hint = false;
    match dots.checked_sub(1).and_then(|i| templates.get(i)) {
        None => hint = true,
        Some(path) if path.as_os_str().is_empty() => {}
        Some(path) => {
            // Copy the template straight into the temp file; a missing or
            // unreadable one warns and falls back to blank.
            if let Err(err) = std::fs::copy(path, temp_file.path()) {
                cba::wbog!(
                    "editor template";
                    "could not copy template '{}' ({err}); opening a blank document",
                    path.display()
                );
            }
        }
    }
    if hint {
        writeln!(temp_file, "# additional notes below")
            .context("Failed to write to temporary file")?;
    }
    temp_file.flush()?;

    open_editor_at(temp_file.path())?;

    let mut content = String::new();
    std::fs::File::open(temp_file.path())
        .context("Failed to reopen temporary file")?
        .read_to_string(&mut content)
        .context("Failed to read from temporary file")?;

    // `%%` comment blocks are always stripped; a hint-seeded file also
    // drops the hint line itself.
    let body = strip_percent_blocks(&content);
    let body = if hint {
        body.lines().skip(1).collect::<Vec<_>>().join("\n")
    } else {
        body
    };
    Ok(body.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::strip_percent_blocks;

    #[test]
    fn strips_whole_block_including_markers() {
        assert_eq!(
            strip_percent_blocks("%%\ninstructions for the user\n%%\nbody text"),
            "body text"
        );
    }

    #[test]
    fn strips_block_in_the_middle() {
        assert_eq!(
            strip_percent_blocks("intro\n%%\nhidden\n%%\nrest"),
            "intro\nrest"
        );
    }

    #[test]
    fn unpaired_marker_strips_to_end_of_file() {
        assert_eq!(
            strip_percent_blocks("kept\n%%\ndropped\ndropped too"),
            "kept"
        );
    }

    #[test]
    fn keeps_content_without_markers() {
        assert_eq!(strip_percent_blocks("plain\ntext"), "plain\ntext");
    }
}
