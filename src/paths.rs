use std::path::PathBuf;

use cba::expr_as_path_fn;

pub const BINARY_FULL: &str = "im";
pub const BINARY_SHORT: &str = "im";

fn config_dir_impl() -> Option<PathBuf> {
    if let Some(env_val) = std::env::var_os("IM_CONFIG_DIR") {
        let env_path = PathBuf::from(env_val);
        if env_path.exists() {
            return Some(env_path);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let config = home.join(".config").join(BINARY_FULL);
        if config.exists() {
            return Some(config);
        }
    }

    dirs::config_dir().map(|x| x.join(BINARY_FULL))
}

fn state_dir_impl() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")))
        .map(|x| x.join(BINARY_FULL))
}

expr_as_path_fn!(state_dir, state_dir_impl().unwrap_or_default());

expr_as_path_fn!(config_dir, config_dir_impl().unwrap_or_default());

#[cfg(debug_assertions)]
expr_as_path_fn!(
    default_config_path,
    config_dir_impl().unwrap_or_default().join("dev.toml")
);

#[cfg(not(debug_assertions))]
expr_as_path_fn!(
    default_config_path,
    config_dir_impl().unwrap_or_default().join("config.toml")
);

#[cfg(debug_assertions)]
expr_as_path_fn!(database_path, state_dir().join("im.dev.db"));

#[cfg(not(debug_assertions))]
expr_as_path_fn!(database_path, state_dir().join("im.db"));

#[cfg(debug_assertions)]
expr_as_path_fn!(
    mm_config_path,
    config_dir_impl().unwrap_or_default().join("mm.dev.toml")
);

#[cfg(not(debug_assertions))]
expr_as_path_fn!(
    mm_config_path,
    config_dir_impl().unwrap_or_default().join("mm.toml")
);

expr_as_path_fn!(log_path, state_dir().join(format!("{BINARY_SHORT}.log")));
