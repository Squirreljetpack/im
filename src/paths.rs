use cba::{birs, expr_as_path_fn};

pub const BINARY_FULL: &str = "im";
pub const BINARY_SHORT: &str = "im";

expr_as_path_fn!(state_dir, birs::state_dir(BINARY_FULL).unwrap_or_default());

expr_as_path_fn!(config_dir, birs::config_dir(BINARY_FULL).unwrap_or_default());

#[cfg(debug_assertions)]
expr_as_path_fn!(
    default_config_path,
    birs::config_dir(BINARY_FULL).unwrap_or_default().join("dev.toml")
);

#[cfg(not(debug_assertions))]
expr_as_path_fn!(
    default_config_path,
    birs::config_dir(BINARY_FULL).unwrap_or_default().join("config.toml")
);

#[cfg(debug_assertions)]
expr_as_path_fn!(database_path, state_dir().join("im.dev.db"));

#[cfg(not(debug_assertions))]
expr_as_path_fn!(database_path, state_dir().join("im.db"));

#[cfg(debug_assertions)]
expr_as_path_fn!(
    mm_config_path,
    birs::config_dir(BINARY_FULL).unwrap_or_default().join("mm.dev.toml")
);

#[cfg(not(debug_assertions))]
expr_as_path_fn!(
    mm_config_path,
    birs::config_dir(BINARY_FULL).unwrap_or_default().join("mm.toml")
);

#[cfg(debug_assertions)]
expr_as_path_fn!(
    colors_path,
    birs::config_dir(BINARY_FULL)
        .unwrap_or_default()
        .join("colors.dev.toml")
);

#[cfg(not(debug_assertions))]
expr_as_path_fn!(
    colors_path,
    birs::config_dir(BINARY_FULL).unwrap_or_default().join("colors.toml")
);

expr_as_path_fn!(log_path, state_dir().join(format!("{BINARY_SHORT}.log")));
