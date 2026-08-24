use std::str::FromStr;
use strum_macros::Display;

/// Custom actions emitted by keybindings or dispatched by async tasks and
/// consumed by the TUI handlers.
#[derive(Debug, Clone, PartialEq, Display)]
pub enum ImAction {
    /// Primary action: context-dependent (complete/reset/prompt).
    Update,
    /// Edit the selected item.
    Edit,
    /// Open the link prompt (today view).
    Link,
    /// Delete the selected item (opens the confirm overlay).
    Delete,
    /// Cycle view mode: Pending ↔ Done (tasks) / horizon (today).
    CycleMode,
    /// Cycle view filter: All → A → B → All.
    CycleFilter,
    /// Toggle the list sort direction.
    ToggleSort,
    /// Reload data from the database.
    Refresh,
    /// Exit the TUI.
    Quit,
    /// Internal: repopulate the worker from the shared view state.
    /// Dispatched by async tasks after mutating data; not user-bindable.
    Repopulate,
    /// Internal: an editor payload is staged; raise the editor interrupt.
    /// Dispatched by async tasks that had to fetch payload data first;
    /// not user-bindable.
    EditExecute,
    /// Shift the today view to the previous day.
    Yesterday,
    /// Shift the today view to the next day.
    Tomorrow,
}

impl FromStr for ImAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Update" => Ok(ImAction::Update),
            "Edit" => Ok(ImAction::Edit),
            "Link" => Ok(ImAction::Link),
            "Delete" => Ok(ImAction::Delete),
            "CycleMode" => Ok(ImAction::CycleMode),
            "CycleFilter" => Ok(ImAction::CycleFilter),
            "ToggleSort" => Ok(ImAction::ToggleSort),
            "Refresh" => Ok(ImAction::Refresh),
            "Quit" => Ok(ImAction::Quit),
            "Repopulate" => Ok(ImAction::Repopulate),
            "EditExecute" => Ok(ImAction::EditExecute),
            "Yesterday" => Ok(ImAction::Yesterday),
            "Tomorrow" => Ok(ImAction::Tomorrow),
            // Unrecognized names fall through to matchmaker's builtin
            // action grammar (e.g. `NextColumn`, `Help`) — an empty error
            // signals the builtin parser to take over.
            _ => Err(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_names_fall_through_to_builtin_actions() {
        use super::ImAction;
        use matchmaker::Action;
        use std::str::FromStr;

        assert!(matches!(
            Action::<ImAction>::from_str("NextColumn").unwrap(),
            Action::NextColumn
        ));
        assert!(matches!(
            Action::<ImAction>::from_str("Help").unwrap(),
            Action::Help(_)
        ));
        assert!(matches!(
            Action::<ImAction>::from_str("Update").unwrap(),
            Action::Custom(ImAction::Update)
        ));
    }
}
