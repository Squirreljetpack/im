pub mod badge;
pub mod cli;
pub mod color;
pub mod commands;
pub mod config;
pub mod date;
pub mod db;
pub mod editor;
pub mod global;
pub mod logger;
pub mod notify;
pub mod ort_compat;
pub mod output;
pub mod paths;
pub mod prompts;
pub mod task;
pub mod task_tree;
pub mod task_view;
pub mod today;
pub mod tracker;
pub mod types;
pub mod ui;
pub mod utils;

pub async fn run_app() {
    use cba::{_dbg, bog};
    use cba::{bo::load_type_or_default, bog::BogOkExt};

    bog::init_bogger(true, false);

    let cli = cli::parse_args().__ebog();

    let [q, v] = cli.opts.qv;
    logger::init_logger([q, 1 + v], paths::log_path());

    let mut config: config::Config =
        load_type_or_default(paths::default_config_path(), |s| toml::from_str(s));
    config.init();

    let tui = atty::is(atty::Stream::Stdout);

    // Interactively, offer to delete it
    // and start fresh (default: no — deleting destroys all data);
    // non-interactive runs just fail with the error below.
    let db_path = paths::database_path();
    let pool = match db::init_database(db_path).await {
        Ok(pool) => pool,
        Err(source) => {
            let source = source.context(format!(
                "Database at {} could not be opened (corrupt, or from an older version); \
                 delete the file to start fresh",
                db_path.display()
            ));
            if tui
                && db_path.is_file()
                && crate::prompts::prompt_delete_invalid_db(db_path).__ebog()
            {
                db::delete_database(db_path).__ebog();
                let pool = db::init_database(db_path).await.__ebog();
                cba::ibog!("Reinitialized db");
                pool
            } else {
                Err(source).__ebog()
            }
        }
    };
    crate::global::set_pool(pool.clone());

    _dbg!(
        commands::execute_command(
            cli.cmd,
            &pool,
            &config,
            &cli.opts,
            &mut std::io::stdout(),
            tui
        )
        .await
    )
    .__ebog()
}
