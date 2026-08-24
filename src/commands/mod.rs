use anyhow::Result;
use cba::_dbg;
use sqlx::SqlitePool;
use std::io::Write;

use crate::cli::{CliOpts, Command};
use crate::config::Config;

mod diagnostics;
mod edit_task;
mod entry;
mod maintenance;
mod task;
mod update;

pub use diagnostics::print_embeddings;

pub async fn execute_command<W: Write>(
    cmd: Command,
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    out: &mut W,
    tui: bool,
) -> Result<()> {
    match cmd {
        Command::Entry(entry) => entry::record_entry(pool, config, opts, _dbg!(entry)).await,

        Command::View { mode, show } => {
            if tui {
                crate::ui::tasks::TasksApp::new(mode, config.clone(), show, opts.fullscreen)
                    .await
                    .run()
                    .await
            } else {
                crate::task_view::write_task_view(pool, mode, config, show, out).await
            }
        }

        Command::Tracker { period, items } => {
            let axes = crate::color::ColorAxes::build(pool, &config.moods).await?;
            crate::tracker::write_tracker_grid(pool, config, &axes, opts, period, items, out).await
        }

        Command::Task(task) => task::create_task_command(pool, config, opts, task).await,

        Command::TaskEdit { task } => edit_task::handle_task_edit(pool, config, opts, task).await,

        Command::Embed => {
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            diagnostics::print_embeddings(&mut reader, out)
        }

        Command::Score { .. } => {
            todo!();
            // handle_score(&start, &end, &mut reader, out)
        }

        Command::Today {
            date,
            show,
            horizon,
        } => {
            // `im @<date>` anchors the view to that day; parse with
            // the fixed `DATE_DIALECT`.
            let day_epoch = match &date {
                Some(d) => crate::date::parse_date(d, crate::date::DATE_DIALECT)?,
                None => crate::date::today_start(),
            };
            if tui {
                crate::ui::today::TodayApp::new(
                    config.clone(),
                    day_epoch,
                    show,
                    horizon,
                    opts.fullscreen,
                )
                .await
                .run()
                .await
            } else {
                let axes = crate::color::ColorAxes::build(pool, &config.moods).await?;
                crate::today::write_today_view(
                    pool, config, &axes, day_epoch, show, horizon, opts, out,
                )
                .await
            }
        }

        Command::Help => {
            // assets/help.txt is bundled via `include_str!` so the compiled
            // binary always has the help text even when the working directory
            // does not contain the assets directory.
            const HELP: &str = include_str!("../../assets/help.txt");
            out.write_all(HELP.as_bytes())?;
            Ok(())
        }

        Command::Config { target } => match target {
            crate::cli::ConfigTarget::Main => maintenance::edit_config().await,
            crate::cli::ConfigTarget::Moods => maintenance::edit_moods(config).await,
            crate::cli::ConfigTarget::Colors => maintenance::edit_colors().await,
        },

        Command::Matchmaker => crate::ui::tracker_picker::run_trackers_app(config).await,

        Command::Db { sub } => match sub {
            crate::cli::DbSubcommand::Prune => maintenance::db_prune(pool, config).await,
            crate::cli::DbSubcommand::Backfill => maintenance::db_backfill(pool).await,
            crate::cli::DbSubcommand::Doctor => maintenance::db_doctor(pool, config, tui).await,
        },

        Command::Color { mood } => {
            let axes = crate::color::ColorAxes::build(pool, &config.moods).await?;
            diagnostics::diagnose_color(&mood, config, &axes, opts, out)
        }

        Command::Clear { date } => maintenance::clear_moods(pool, config, date, tui).await,
    }
}
