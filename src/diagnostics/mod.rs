// Gateway module for diagnostics - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod gpu;
mod monitor;
mod panel;
mod types;

// Public re-exports - the ONLY way to access diagnostics functionality
pub use monitor::{
    create_monitoring_task, estimate_model_memory, HardwareMonitor, SharedHardwareMonitor,
};
pub use panel::render_diagnostics_panel;
pub use types::{DiagnosticsMode, GpuInfo, GpuType, HardwareStats, ModelInfo};
