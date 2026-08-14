use std::{fs::OpenOptions, path::Path};

use cba::{
    bait::ResultExt,
    bog::{self, BogOkExt},
};

pub fn init_logger([q, v]: [u8; 2], log_path: &Path) {
    bog::init_bogger(true, true);
    bog::init_filter((4 + v).saturating_sub(q));

    let rust_log = std::env::var("RUST_LOG").ok().map(|val| val.to_lowercase());

    let mut builder = env_logger::Builder::from_default_env();

    if rust_log.is_none() {
        #[cfg(debug_assertions)]
        {
            builder
                .filter(None, log::LevelFilter::Info)
                .filter(Some("im"), log::LevelFilter::Trace)
                .filter(Some("cba"), log::LevelFilter::Trace)
                .format(|buf, record| {
                    use std::io::Write;

                    writeln!(
                        buf,
                        "{} {}:{} [{}] {}",
                        buf.timestamp_micros(),
                        record.file().unwrap_or("<unknown>"),
                        record.line().unwrap_or(0),
                        record.level(),
                        record.args()
                    )
                });
        }
        #[cfg(not(debug_assertions))]
        {
            builder
                .format_module_path(false)
                .format_target(false)
                .format_timestamp(None);

            let level = if q > v {
                log::LevelFilter::Warn
            } else if v > q {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            };

            builder.filter(Some("im"), level).filter(Some("cba"), level);
        }
    }

    if let Some(p) = log_path.parent() {
        std::fs::create_dir_all(p).ok();
    }

    if let Some(log_file) = OpenOptions::new()
        .truncate(true)
        .write(true)
        .create(true)
        .open(log_path)
        .prefix(format!(
            "Failed to open log file @ {}.",
            log_path.to_string_lossy()
        ))
        ._wbog()
    {
        builder.target(env_logger::Target::Pipe(Box::new(log_file)));
    }

    builder.init();
}
