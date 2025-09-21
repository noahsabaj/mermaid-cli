use serde::{Deserialize, Serialize};

/// Hardware statistics for monitoring
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareStats {
    pub gpu: Option<GpuInfo>,
    pub cpu_usage_percent: f32,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
    pub inference_speed: Option<f32>, // tokens/sec
    pub model_info: Option<ModelInfo>,
}

/// GPU information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub gpu_type: GpuType,
    pub usage_percent: f32,
    pub memory_used_gb: f32,
    pub memory_total_gb: f32,
    pub temperature_celsius: Option<f32>,
}

/// Model runtime information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub size_on_disk_gb: Option<f32>,
    pub loaded_memory_gb: Option<f32>,
    pub context_length: usize,
    pub context_used: usize,
}

/// Type of GPU detected
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum GpuType {
    Nvidia,
    Amd,
    Intel,
    AppleSilicon,
    None,
}

impl GpuType {
    pub fn display_name(&self) -> &str {
        match self {
            GpuType::Nvidia => "NVIDIA",
            GpuType::Amd => "AMD",
            GpuType::Intel => "Intel",
            GpuType::AppleSilicon => "Apple",
            GpuType::None => "None",
        }
    }
}

/// Diagnostic display mode
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum DiagnosticsMode {
    Hidden,
    Compact,   // Status line only
    Detailed,  // Full panel
}

impl Default for DiagnosticsMode {
    fn default() -> Self {
        DiagnosticsMode::Compact
    }
}

impl HardwareStats {
    /// Get a compact status line representation
    pub fn to_status_line(&self) -> String {
        let mut parts = Vec::new();

        // GPU info
        if let Some(gpu) = &self.gpu {
            let gpu_color = if gpu.usage_percent > 90.0 { "🔴" }
                           else if gpu.usage_percent > 70.0 { "🟡" }
                           else { "🟢" };

            parts.push(format!(
                "{} GPU: {:.0}% ({:.1}/{:.1}GB)",
                gpu_color,
                gpu.usage_percent,
                gpu.memory_used_gb,
                gpu.memory_total_gb
            ));
        }

        // CPU info
        let cpu_color = if self.cpu_usage_percent > 90.0 { "🔴" }
                       else if self.cpu_usage_percent > 70.0 { "🟡" }
                       else { "🟢" };
        parts.push(format!("{} CPU: {:.0}%", cpu_color, self.cpu_usage_percent));

        // RAM info
        let ram_percent = (self.ram_used_gb / self.ram_total_gb) * 100.0;
        let ram_color = if ram_percent > 90.0 { "🔴" }
                       else if ram_percent > 70.0 { "🟡" }
                       else { "🟢" };
        parts.push(format!(
            "{} RAM: {:.1}/{:.1}GB",
            ram_color,
            self.ram_used_gb,
            self.ram_total_gb
        ));

        // Inference speed
        if let Some(speed) = self.inference_speed {
            parts.push(format!("⚡ {:.1} tok/s", speed));
        }

        parts.join(" │ ")
    }

    /// Check if any resource is critically high (>90%)
    pub fn has_critical_usage(&self) -> bool {
        if self.cpu_usage_percent > 90.0 {
            return true;
        }

        if (self.ram_used_gb / self.ram_total_gb) * 100.0 > 90.0 {
            return true;
        }

        if let Some(gpu) = &self.gpu {
            if gpu.usage_percent > 90.0 || (gpu.memory_used_gb / gpu.memory_total_gb) * 100.0 > 90.0 {
                return true;
            }
        }

        false
    }
}