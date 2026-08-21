use cba::bait::TransformExt;
use matchmaker::{
    action::Action as MMAction,
    bindmap,
    binds::{key, BindMap, BindMapExt},
    config::{OverlayConfig, RenderConfig, TerminalConfig},
};

use serde::{Deserialize, Serialize};

use crate::ui::action::ImAction;

/// Bundled matchmaker render config (`assets/config/mm.toml`); the
/// fallback when no user config exists.
pub const DEFAULT_MM_CONFIG: &str = include_str!("../../assets/config/mm.toml");

/// Bundled dev mirror (`assets/config/mm.dev.toml`): debug builds copy
/// this onto the user's `mm.dev.toml` path, so every debug run starts
/// from the bundled settings.
pub const DEFAULT_MM_DEV_CONFIG: &str = include_str!("../../assets/config/mm.dev.toml");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MMConfig {
    #[serde(default, flatten)]
    pub render: RenderConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
    #[serde(default)]
    pub tui: TerminalConfig,
    #[serde(default)]
    pub binds: BindMap<ImAction>,
}

impl Default for MMConfig {
    fn default() -> Self {
        toml::from_str(DEFAULT_MM_CONFIG).expect("bundled assets/config/mm.toml must parse")
    }
}

/// Matchmaker default binds extended with the view-specific ones. `enter`
/// maps to the custom `Action::Update` — the TUI's accept state machine,
/// which never quits the picker — while `alt-enter` runs the builtin
/// matchmaker `Accept` (runs the accept hook and finishes `pick`). Quit is
/// `ctrl-c` / `esc` only (no `q`).
pub fn default_binds() -> BindMap<ImAction> {
    let mut base = BindMap::default_binds().with_extras();
    let custom = bindmap!(
        key!(enter) => ImAction::Update,
        key!(alt-enter) => MMAction::Accept,
        key!(tab) => ImAction::CycleMode,
        key!(alt-s) => ImAction::ToggleSort,
        key!(ctrl-s) => ImAction::CycleFilter,
        key!(delete), key!(ctrl-h) => ImAction::Delete,
        key!(ctrl-e) => ImAction::Edit,
        key!(ctrl-l) => ImAction::Link,
        key!(ctrl-r) => ImAction::Refresh,
        // Results/Preview wrap (mirrors the matchmaker-cli assets binds).
        key!(alt-h) => MMAction::Help("".to_string()),
        key!(alt-'[') => MMAction::ToggleWrap,
        key!(alt-']') => MMAction::TogglePreviewWrap,
    );
    base.extend(custom);
    base
}

/// Load the matchmaker config from the user config dir (`mm.toml` next to
/// `config.toml`; debug builds use `mm.dev.toml`), falling back to the
/// bundled default. User binds in the file override the defaults.
///
/// In debug builds the on-disk file is always overwritten from the
/// bundled dev default first — the dev mm config is a throwaway mirror of
/// `assets/config/mm.dev.toml`, so every debug run starts from the
/// bundled settings regardless of what a previous run left behind.
pub fn get_mm_cfg() -> (
    RenderConfig,
    BindMap<ImAction>,
    TerminalConfig,
    OverlayConfig,
) {
    let path = crate::paths::mm_config_path();
    #[cfg(debug_assertions)]
    {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Best-effort: a read-only config dir falls back to whatever is
        // on disk, or the bundled default below.
        let _ = std::fs::write(path, DEFAULT_MM_DEV_CONFIG);
    }
    let mut mm_cfg: MMConfig = cba::bo::load_type_or_default(path, |s| toml::from_str(s));
    mm_cfg.binds = default_binds().modify(|b| b.extend(mm_cfg.binds));
    // Force the selection prefixes off after the file loads: the views
    // render no row prefixes, and the config file must not re-enable them.
    mm_cfg.render.results.multi_prefix = String::new();
    mm_cfg.render.results.default_prefix = String::new();
    mm_cfg.render.results.autoscroll.initial_preserved = 3;
    (mm_cfg.render, mm_cfg.binds, mm_cfg.tui, mm_cfg.overlay)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_mm_toml_parses() {
        let cfg = super::MMConfig::default();
        assert!(cfg.render.ui.mouse_events);
    }

    #[test]
    fn bundled_dev_mm_toml_parses() {
        let cfg: super::MMConfig =
            toml::from_str(super::DEFAULT_MM_DEV_CONFIG).expect("mm.dev.toml must parse");
        assert!(cfg.render.ui.mouse_events);
    }
}
