use anyhow::{Context, Result};
use cba::{ebog, ibog};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};

use crate::global;
use crate::config::{Config, TrackerKind, TrackerSetting, DEFAULT_CONFIG, DEFAULT_MOODS};
use crate::date;
use crate::db::{TrackerPruneRule, TrackerScoreKindRow};
use crate::editor::open_editor_at;
use crate::paths::default_config_path;

/// `im :clear [@date]` — clear/delete all mood entries from that day.
/// If interactive, confirm first, showing the computed date.
pub(super) async fn clear_moods(
    pool: &SqlitePool,
    _config: &Config,
    date_param: Option<String>,
    tui: bool,
) -> Result<()> {
    let target_ts = match date_param {
        Some(ref d_str) => crate::date::parse_datetime(d_str, crate::date::DATE_DIALECT)?,
        None => crate::date::now(),
    };

    let start = crate::date::day_start(target_ts);
    let end = crate::date::day_end(target_ts);
    let formatted_date = crate::date::format_date(start);

    // Count how many mood entries exist for this day
    let count = crate::db::clear_moods(pool, start, end, false).await?;

    if count == 0 {
        ebog!("No mood entries found for {formatted_date}");
        return Ok(());
    }

    let interactive = tui;
    if interactive {
        let confirmed = crate::prompts::prompt_clear_confirm(count as i64, &formatted_date)?;

        if !confirmed {
            cliclack::outro("Cancelled.")?;
            return Ok(());
        }
    }

    let deleted_count = crate::db::clear_moods(pool, start, end, true).await?;

    if interactive {
        cliclack::outro(format!(
            "Cleared {deleted_count} mood entry/entries for {formatted_date}"
        ))?;
    } else {
        ibog!("Cleared {deleted_count} mood entry/entries for {formatted_date}")
    }

    Ok(())
}

/// `im :db prune` — deletes completed oneshot tasks (their `short_id`
/// was cleared on completion, so they are no longer addressable) and
/// recurring tasks whose `end_time` has passed.
///
/// Both categories are collected in a single SQL `RETURNING` statement so
/// the per-row log lines below happen against the rows actually deleted,
/// not a SELECT-then-DELETE that could log a row that races with another
/// writer. Foreign-key cascades (see `db.rs`: `todo_completions.todo_id`
/// has `ON DELETE CASCADE`) drop the matching completion rows
/// automatically.
pub(super) async fn db_prune(pool: &SqlitePool, _config: &Config) -> Result<()> {
    let now = date::now();
    let pruned = crate::db::prune_tasks(pool, now).await?;

    for task in &pruned {
        match task.short_id {
            Some(short_id) => cba::ibog!(
                "prune";
                "deleted {} task #{} '{}'",
                task.reason,
                short_id,
                task.name
            ),
            None => cba::ibog!("prune"; "deleted {} task '{}'", task.reason, task.name),
        }
    }

    let pruned_cache_count = crate::db::prune_embedding_cache(pool).await?;
    if pruned_cache_count > 0 {
        cba::ibog!("prune"; "pruned {} stale cached embedding(s)", pruned_cache_count);
    }

    if pruned.is_empty() {
        cba::ibog!("prune"; "nothing to prune");
    } else {
        cba::ibog!("prune"; "pruned {} task(s)", pruned.len());
    }

    Ok(())
}

/// `im :db backfill` — compute and persist the mood embeddings and
/// saliency scores that rendering no longer writes inline (see
/// `color::ColorAxes::mood_color_cached`). Journal-only rows (empty mood)
/// never embed and are skipped, matching the old inline backfill behavior.
pub(super) async fn db_backfill(pool: &SqlitePool) -> Result<()> {
    let rows = crate::db::fetch_moods_between(pool, i64::MIN, i64::MAX).await?;
    let embedder = global::embedder();
    let mut backfilled = 0usize;
    let mut failed = 0usize;

    for mood in rows {
        if mood.mood.is_empty() {
            continue;
        }
        // Embed (skipping rows that already carry a stored embedding)…
        let embedding = match &mood.embedding {
            Some(blob) => global::blob_to_embedding(blob),
            None => embedder.embed(&mood.mood, "").ok(),
        };
        let Some(embedding) = embedding else {
            failed += 1;
            continue;
        };
        // …and persist the embedding and the saliency score.
        let mut changed = false;
        if mood.embedding.is_none() {
            let blob = global::embedding_to_blob(&embedding);
            if crate::db::update_mood_embedding(pool, mood.id, &blob)
                .await
                .is_ok()
            {
                changed = true;
            }
        }
        if mood.score.is_none() {
            let score = crate::color::predict_saliency(embedder, &mood.mood);
            if crate::db::update_mood_score(pool, mood.id, score)
                .await
                .is_ok()
            {
                changed = true;
            }
        }
        if changed {
            backfilled += 1;
        }
    }

    if backfilled == 0 {
        cba::ibog!("db"; "backfill: nothing to backfill");
    } else {
        cba::ibog!("db"; "backfilled {} mood row(s)", backfilled);
    }
    if failed > 0 {
        cba::ebog!("db"; "backfill failed for {} row(s)", failed);
    }
    Ok(())
}

/// One `:db doctor` report line: entries to prune for a single tracker
/// type, with the per-storage-class breakdown ("how many of each kind").
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TrackerPruneLine {
    pub tracker_type: String,
    /// Human-readable reason: `kind <name>` (with a time-marker note for
    /// marker-mode null) or the orphan note.
    pub reason: String,
    /// Pruned count per storage class, e.g. `[("real", 2), ("text", 1)]`;
    /// the marker-mode integer bucket is labelled `integer ≠ 0`. Empty for
    /// orphan types (everything is pruned).
    pub per_storage: Vec<(String, i64)>,
    /// Total entries this line prunes.
    pub total: i64,
}

/// The full `:db doctor` plan: report lines plus the equivalent prune rules.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TrackerPrunePlan {
    pub lines: Vec<TrackerPruneLine>,
    pub rules: Vec<TrackerPruneRule>,
}

fn kind_label(kind: TrackerKind) -> &'static str {
    match kind {
        TrackerKind::Text => "text",
        TrackerKind::Integer => "integer",
        TrackerKind::Float => "float",
        TrackerKind::Duration => "duration",
        TrackerKind::Null => "null",
    }
}

/// Classify tracker entries into prune lines/rules for `:db doctor`.
///
/// Each configured kind fixes the storage class every writer binds (`text`
/// → TEXT, `number` → INTEGER, `float` → REAL, `null` → INTEGER — see
/// `create_entry` and `update_tracker_score`), so an entry whose
/// `typeof(score)` differs no longer matches the tracker's current kind,
/// e.g. after the tracker's `kind` changed in the config. A `null` tracker
/// always writes score 0 (replace markers and cumulative count rows alike),
/// so any nonzero null row is a stale leftover and is pruned too. Types
/// with no `[tracker.<type>]` section (renamed/removed trackers) are
/// orphans: everything is pruned (the today view hard-errors on such rows).
fn plan_tracker_prunes(
    rows: &[TrackerScoreKindRow],
    trackers: &HashMap<String, TrackerSetting>,
) -> TrackerPrunePlan {
    let mut by_type: BTreeMap<&str, Vec<&TrackerScoreKindRow>> = BTreeMap::new();
    for row in rows {
        by_type
            .entry(row.tracker_type.as_str())
            .or_default()
            .push(row);
    }

    let mut lines = Vec::new();
    let mut rules = Vec::new();
    for (tracker_type, group) in by_type {
        let Some(setting) = trackers.get(tracker_type) else {
            let total = group.iter().map(|r| r.count).sum();
            lines.push(TrackerPruneLine {
                tracker_type: tracker_type.to_string(),
                reason: format!("no [tracker.{tracker_type}] section in config"),
                per_storage: Vec::new(),
                total,
            });
            rules.push(TrackerPruneRule::All {
                tracker_type: tracker_type.to_string(),
            });
            continue;
        };

        let keep = match setting.kind {
            TrackerKind::Text => "text",
            TrackerKind::Integer => "integer",
            TrackerKind::Float => "real",
            TrackerKind::Duration => "real",
            TrackerKind::Null => "integer",
        };
        // Null rows store score 0 by construction (replace inserts write 0,
        // the TUI timestamp update never touches score, cumulative inserts
        // 0), so any nonzero row is a stale leftover (incl. pre-rework
        // count rows) — prune it regardless of the interval mode.
        let marker_mode = setting.kind == TrackerKind::Null;

        let mut per_storage = Vec::new();
        let mut total = 0i64;
        let mut nonzero = false;
        for row in group {
            if row.storage != keep {
                per_storage.push((row.storage.clone(), row.count));
                total += row.count;
            } else if marker_mode && row.nonzero > 0 {
                per_storage.push(("integer ≠ 0".to_string(), row.nonzero));
                total += row.nonzero;
                nonzero = true;
            }
        }
        if total == 0 {
            continue;
        }

        rules.push(TrackerPruneRule::Storage {
            tracker_type: tracker_type.to_string(),
            keep,
        });
        if nonzero {
            rules.push(TrackerPruneRule::NonzeroScore {
                tracker_type: tracker_type.to_string(),
            });
        }

        let reason = if marker_mode {
            "kind null (null rows store score 0)".to_string()
        } else {
            format!("kind {}", kind_label(setting.kind))
        };
        lines.push(TrackerPruneLine {
            tracker_type: tracker_type.to_string(),
            reason,
            per_storage,
            total,
        });
    }
    TrackerPrunePlan { lines, rules }
}

/// `im :db doctor` — check every tracker entry's storage class against
/// the tracker's current configured kind and prune the mismatches, after an
/// interactive confirm. Non-interactive runs only surface the breakdown
/// (deletion requires the confirm).
pub(super) async fn db_doctor(pool: &SqlitePool, config: &Config, tui: bool) -> Result<()> {
    let rows = crate::db::fetch_tracker_score_kinds(pool).await?;
    let plan = plan_tracker_prunes(&rows, &config.tracker);

    if plan.lines.is_empty() {
        cba::ibog!("doctor"; "all tracker entries match their configured kinds");
        return Ok(());
    }

    let total: i64 = plan.lines.iter().map(|l| l.total).sum();
    for line in &plan.lines {
        let breakdown = line
            .per_storage
            .iter()
            .map(|(storage, n)| format!("{n} {storage}"))
            .collect::<Vec<_>>()
            .join(", ");
        let detail = if breakdown.is_empty() {
            String::new()
        } else {
            format!(" ({breakdown})")
        };
        cba::ibog!(
            "doctor";
            "{}: {} entry/entries — {}{}",
            line.tracker_type,
            line.total,
            line.reason,
            detail
        );
    }
    cba::ibog!("doctor"; "total: {total} entry/entries to prune");

    if !tui {
        cba::wbog!("doctor"; ":db doctor must be run interactively to confirm and delete");
        return Ok(());
    }

    if !crate::prompts::prompt_db_doctor_confirm(total)? {
        cliclack::outro("Cancelled.")?;
        return Ok(());
    }

    let deleted = crate::db::prune_tracker_rules(pool, &plan.rules).await?;
    cliclack::outro(format!("Pruned {deleted} tracker entry/entries"))?;
    Ok(())
}

/// `im :config` — open the active config in $VISUAL/$EDITOR.
///
/// If the on-disk config doesn't exist yet (common on first run with the
/// release profile), copy the bundled `assets/config.toml` straight to the
/// destination path verbatim — no TOML round-trip through `Config::default` —
/// so the user sees the exact source-of-truth defaults and we save a
/// serialization step. The copy is announced with `ibog!` so a non-interactive
/// invocation (e.g. a piped run, the editor still launches anyway) leaves a
/// legible trail in the log.
pub(super) async fn edit_config() -> Result<()> {
    let path = default_config_path();
    let mut created = false;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config dir {:?}", parent))?;
        }
        std::fs::write(path, DEFAULT_CONFIG.as_bytes())
            .with_context(|| format!("Failed to write default config to {:?}", path))?;
        created = true;
    }
    if created {
        cba::ibog!(
            "config";
            "Created file at {}",
            path.display()
        );
    }
    open_editor_at(path)
}

/// `im :moods` — open the moods file (`[moods] source`, relative to
/// the config directory) in $VISUAL/$EDITOR.
///
/// Like [`handle_config`], a missing file is created from the bundled moods
/// defaults first, announced with `ibog!`. When `[moods] source` is empty
/// (the default) there is no moods file to open: warn that `source` must be
/// set in the config, and do nothing else.
pub(super) async fn edit_moods(config: &Config) -> Result<()> {
    if config.moods.source.as_os_str().is_empty() {
        cba::wbog!(
            "im :moods needs a moods file, but [moods] source is unset: add \
             source = \"moods.toml\" to the [moods] section of your config"
        );
        return Ok(());
    }
    let path = crate::paths::config_dir().join(&config.moods.source);
    let mut created = false;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config dir {:?}", parent))?;
        }
        std::fs::write(&path, DEFAULT_MOODS.as_bytes())
            .with_context(|| format!("Failed to write default moods file to {:?}", path))?;
        created = true;
    }
    if created {
        cba::ibog!(
            "moods";
            "Created file at {}",
            path.display()
        );
    }
    open_editor_at(&path)
}

/// `im -` (bare) — tasks-edit entry point. Stub for now: interactive
/// task editing is future work (see TODO.md).
pub(super) async fn edit_tasks() -> Result<()> {
    anyhow::bail!("Task editing is not yet implemented");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrackerSetting;

    fn row(tracker_type: &str, storage: &str, count: i64, nonzero: i64) -> TrackerScoreKindRow {
        TrackerScoreKindRow {
            tracker_type: tracker_type.to_string(),
            storage: storage.to_string(),
            count,
            nonzero,
        }
    }

    fn make_trackers(entries: &[(&str, TrackerSetting)]) -> HashMap<String, TrackerSetting> {
        entries
            .iter()
            .map(|(name, setting)| (name.to_string(), setting.clone()))
            .collect()
    }

    #[test]
    fn plan_prunes_storage_mismatches_per_kind() {
        // sleep is float (real kept, text pruned); runs is integer
        // (integer kept, real pruned). BTreeMap orders lines by type name.
        let rows = vec![
            row("runs", "integer", 3, 0),
            row("runs", "real", 2, 0),
            row("sleep", "real", 2, 0),
            row("sleep", "text", 1, 0),
        ];
        let trackers = make_trackers(&[
            ("sleep", TrackerSetting::new(TrackerKind::Float)),
            ("runs", TrackerSetting::new(TrackerKind::Integer)),
        ]);
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert_eq!(
            plan.lines,
            vec![
                TrackerPruneLine {
                    tracker_type: "runs".to_string(),
                    reason: "kind integer".to_string(),
                    per_storage: vec![("real".to_string(), 2)],
                    total: 2,
                },
                TrackerPruneLine {
                    tracker_type: "sleep".to_string(),
                    reason: "kind float".to_string(),
                    per_storage: vec![("text".to_string(), 1)],
                    total: 1,
                },
            ]
        );
        assert_eq!(
            plan.rules,
            vec![
                TrackerPruneRule::Storage {
                    tracker_type: "runs".to_string(),
                    keep: "integer",
                },
                TrackerPruneRule::Storage {
                    tracker_type: "sleep".to_string(),
                    keep: "real",
                },
            ]
        );
    }

    #[test]
    fn plan_text_kind_keeps_only_text() {
        let rows = vec![
            row("affirmation", "text", 2, 0),
            row("affirmation", "integer", 1, 0),
        ];
        let trackers = make_trackers(&[("affirmation", TrackerSetting::new(TrackerKind::Text))]);
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert_eq!(plan.lines[0].reason, "kind text");
        assert_eq!(plan.lines[0].per_storage, vec![("integer".to_string(), 1)]);
        assert_eq!(plan.lines[0].total, 1);
        assert_eq!(
            plan.rules,
            vec![TrackerPruneRule::Storage {
                tracker_type: "affirmation".to_string(),
                keep: "text",
            }]
        );
    }

    #[test]
    fn plan_null_tracker_modes() {
        // No bounds: null rows carry score 0 in every mode, so nonzero
        // integers (pre-rework count rows) are stale leftovers; text
        // entries are pruned.
        let rows = vec![row("pills", "integer", 3, 3), row("pills", "text", 1, 0)];
        let trackers = make_trackers(&[("pills", TrackerSetting::new(TrackerKind::Null))]);
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert_eq!(
            plan.lines,
            vec![TrackerPruneLine {
                tracker_type: "pills".to_string(),
                reason: "kind null (null rows store score 0)".to_string(),
                per_storage: vec![
                    ("integer ≠ 0".to_string(), 3),
                    ("text".to_string(), 1),
                ],
                total: 4,
            }]
        );
        assert_eq!(
            plan.rules,
            vec![
                TrackerPruneRule::Storage {
                    tracker_type: "pills".to_string(),
                    keep: "integer",
                },
                TrackerPruneRule::NonzeroScore {
                    tracker_type: "pills".to_string(),
                },
            ]
        );

        // Same rule with both bounds: the interval mode does not matter.
        // leftovers and are pruned too.
        let rows = vec![
            row("sleep", "integer", 5, 3),
            row("sleep", "real", 1, 0),
            row("sleep", "text", 1, 0),
        ];
        let trackers = make_trackers(&[(
            "sleep",
            TrackerSetting::new(TrackerKind::Null)
                .with_low(82800.0)
                .with_high(7200.0),
        )]);
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert_eq!(
            plan.lines,
            vec![TrackerPruneLine {
                tracker_type: "sleep".to_string(),
                reason: "kind null (null rows store score 0)".to_string(),
                per_storage: vec![
                    ("integer ≠ 0".to_string(), 3),
                    ("real".to_string(), 1),
                    ("text".to_string(), 1),
                ],
                total: 5,
            }]
        );
        assert_eq!(
            plan.rules,
            vec![
                TrackerPruneRule::Storage {
                    tracker_type: "sleep".to_string(),
                    keep: "integer",
                },
                TrackerPruneRule::NonzeroScore {
                    tracker_type: "sleep".to_string(),
                },
            ]
        );

        // Marker mode with a clean integer bucket: no NonzeroScore rule.
        let rows = vec![row("sleep", "integer", 5, 0), row("sleep", "text", 1, 0)];
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert_eq!(plan.lines[0].total, 1);
        assert_eq!(
            plan.rules,
            vec![TrackerPruneRule::Storage {
                tracker_type: "sleep".to_string(),
                keep: "integer",
            }]
        );
    }

    #[test]
    fn plan_orphan_types_prune_everything() {
        let rows = vec![row("oldname", "real", 2, 0), row("oldname", "text", 1, 0)];
        let trackers = HashMap::new();
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert_eq!(
            plan.lines,
            vec![TrackerPruneLine {
                tracker_type: "oldname".to_string(),
                reason: "no [tracker.oldname] section in config".to_string(),
                per_storage: Vec::new(),
                total: 3,
            }]
        );
        assert_eq!(
            plan.rules,
            vec![TrackerPruneRule::All {
                tracker_type: "oldname".to_string(),
            }]
        );
    }

    #[test]
    fn plan_skips_clean_trackers_and_empty_input() {
        // sleep float with only real entries and pills null with only
        // zero-score integer entries (null rows store score 0 — clean).
        let rows = vec![row("sleep", "real", 2, 0), row("pills", "integer", 3, 0)];
        let trackers = make_trackers(&[
            ("sleep", TrackerSetting::new(TrackerKind::Float)),
            ("pills", TrackerSetting::new(TrackerKind::Null)),
        ]);
        let plan = plan_tracker_prunes(&rows, &trackers);
        assert!(plan.lines.is_empty());
        assert!(plan.rules.is_empty());

        let plan = plan_tracker_prunes(&[], &trackers);
        assert!(plan.lines.is_empty());
        assert!(plan.rules.is_empty());
    }
}
