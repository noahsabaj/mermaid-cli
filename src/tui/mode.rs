use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// The four operation modes for Mermaid, inspired by Claude Code's Shift+Tab cycling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationMode {
    /// Default mode - asks for confirmation on all operations
    Normal,
    /// Auto-accepts file edits only, confirms other operations
    AcceptEdits,
    /// Research & plan mode - shows what would happen without executing
    PlanMode,
    /// YOLO mode - accepts everything automatically (use with caution)
    BypassAll,
}

impl Default for OperationMode {
    fn default() -> Self {
        Self::Normal
    }
}

impl OperationMode {
    /// Cycle to the next mode in the sequence
    pub fn cycle(&self) -> Self {
        match self {
            Self::Normal => Self::AcceptEdits,
            Self::AcceptEdits => Self::PlanMode,
            Self::PlanMode => Self::BypassAll,
            Self::BypassAll => Self::Normal,
        }
    }

    /// Cycle to the previous mode in the sequence
    pub fn cycle_reverse(&self) -> Self {
        match self {
            Self::Normal => Self::BypassAll,
            Self::BypassAll => Self::PlanMode,
            Self::PlanMode => Self::AcceptEdits,
            Self::AcceptEdits => Self::Normal,
        }
    }

    /// Get the display name for the mode with icon
    pub fn display_name(&self) -> &str {
        match self {
            Self::Normal => "Normal",
            Self::AcceptEdits => "Accept Edits",
            Self::PlanMode => "Plan Mode",
            Self::BypassAll => "Bypass All",
        }
    }

    /// Get a short name for compact display
    pub fn short_name(&self) -> &str {
        match self {
            Self::Normal => "N",
            Self::AcceptEdits => "A",
            Self::PlanMode => "P",
            Self::BypassAll => "B",
        }
    }

    /// Get the color associated with this mode for visual indicators
    pub fn color(&self) -> Color {
        match self {
            Self::Normal => Color::Green,
            Self::AcceptEdits => Color::Yellow,
            Self::PlanMode => Color::Blue,
            Self::BypassAll => Color::Red,
        }
    }

    /// Get a description of what this mode does
    pub fn description(&self) -> &str {
        match self {
            Self::Normal => "Asks for confirmation on all operations",
            Self::AcceptEdits => "Auto-accepts file edits, confirms other operations",
            Self::PlanMode => "Shows what would happen without executing anything",
            Self::BypassAll => "Automatically accepts all operations (use with caution)",
        }
    }

    /// Check if this mode should auto-accept file operations
    pub fn auto_accept_files(&self) -> bool {
        matches!(self, Self::AcceptEdits | Self::BypassAll)
    }

    /// Check if this mode should auto-accept shell commands
    pub fn auto_accept_commands(&self) -> bool {
        matches!(self, Self::BypassAll)
    }

    /// Check if this mode should auto-accept git operations
    pub fn auto_accept_git(&self) -> bool {
        matches!(self, Self::BypassAll)
    }

    /// Check if this mode is in planning-only mode (no execution)
    pub fn is_planning_only(&self) -> bool {
        matches!(self, Self::PlanMode)
    }

    /// Check if this mode requires extra safety checks
    pub fn needs_safety_confirmation(&self) -> bool {
        matches!(self, Self::BypassAll)
    }

    /// Get the keyboard hint for this mode
    pub fn keyboard_hint(&self) -> &str {
        match self {
            Self::Normal => "Shift+Tab to cycle modes",
            Self::AcceptEdits => "Ctrl+E for Accept Edits mode",
            Self::PlanMode => "Ctrl+P for Plan mode",
            Self::BypassAll => "Ctrl+Y to toggle Bypass All",
        }
    }

    /// Parse mode from string (for config files)
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "normal" => Some(Self::Normal),
            "accept_edits" | "accept-edits" | "acceptedits" => Some(Self::AcceptEdits),
            "plan_mode" | "plan-mode" | "planmode" | "plan" => Some(Self::PlanMode),
            "bypass_all" | "bypass-all" | "bypassall" | "bypass" | "yolo" => Some(Self::BypassAll),
            _ => None,
        }
    }

    /// Convert mode to string (for config files)
    pub fn to_str(&self) -> &str {
        match self {
            Self::Normal => "normal",
            Self::AcceptEdits => "accept_edits",
            Self::PlanMode => "plan_mode",
            Self::BypassAll => "bypass_all",
        }
    }

    /// Get the warning level for this mode
    pub fn warning_level(&self) -> WarningLevel {
        match self {
            Self::Normal => WarningLevel::None,
            Self::AcceptEdits => WarningLevel::Low,
            Self::PlanMode => WarningLevel::None,
            Self::BypassAll => WarningLevel::High,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningLevel {
    None,
    Low,
    High,
}

impl WarningLevel {
    pub fn color(&self) -> Color {
        match self {
            Self::None => Color::Green,
            Self::Low => Color::Yellow,
            Self::High => Color::Red,
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Low => Some("Auto-accept files"),
            Self::High => Some("WARNING: BYPASS ALL"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_cycling() {
        let mut mode = OperationMode::Normal;

        mode = mode.cycle();
        assert_eq!(mode, OperationMode::AcceptEdits);

        mode = mode.cycle();
        assert_eq!(mode, OperationMode::PlanMode);

        mode = mode.cycle();
        assert_eq!(mode, OperationMode::BypassAll);

        mode = mode.cycle();
        assert_eq!(mode, OperationMode::Normal);
    }

    #[test]
    fn test_mode_cycling_reverse() {
        let mut mode = OperationMode::Normal;

        mode = mode.cycle_reverse();
        assert_eq!(mode, OperationMode::BypassAll);

        mode = mode.cycle_reverse();
        assert_eq!(mode, OperationMode::PlanMode);

        mode = mode.cycle_reverse();
        assert_eq!(mode, OperationMode::AcceptEdits);

        mode = mode.cycle_reverse();
        assert_eq!(mode, OperationMode::Normal);
    }

    #[test]
    fn test_mode_permissions() {
        assert!(!OperationMode::Normal.auto_accept_files());
        assert!(!OperationMode::Normal.auto_accept_commands());

        assert!(OperationMode::AcceptEdits.auto_accept_files());
        assert!(!OperationMode::AcceptEdits.auto_accept_commands());

        assert!(!OperationMode::PlanMode.auto_accept_files());
        assert!(OperationMode::PlanMode.is_planning_only());

        assert!(OperationMode::BypassAll.auto_accept_files());
        assert!(OperationMode::BypassAll.auto_accept_commands());
        assert!(OperationMode::BypassAll.auto_accept_git());
    }

    #[test]
    fn test_mode_from_str() {
        assert_eq!(
            OperationMode::from_str("normal"),
            Some(OperationMode::Normal)
        );
        assert_eq!(
            OperationMode::from_str("accept_edits"),
            Some(OperationMode::AcceptEdits)
        );
        assert_eq!(
            OperationMode::from_str("plan-mode"),
            Some(OperationMode::PlanMode)
        );
        assert_eq!(
            OperationMode::from_str("yolo"),
            Some(OperationMode::BypassAll)
        );
        assert_eq!(OperationMode::from_str("invalid"), None);
    }
}
