use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::System;
use tokio::sync::Mutex;

use super::gpu::{detect_gpu_type, get_gpu_info};
use super::types::{GpuType, HardwareStats, ModelInfo};

/// Hardware monitoring service
pub struct HardwareMonitor {
    system: System,
    gpu_type: GpuType,
    last_update: Instant,
    update_interval: Duration,
    cached_stats: Option<HardwareStats>,
}

impl HardwareMonitor {
    /// Create a new hardware monitor
    pub fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_all();

        Self {
            system,
            gpu_type: detect_gpu_type(),
            last_update: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or(Instant::now()),
            update_interval: Duration::from_secs(2),
            cached_stats: None,
        }
    }

    /// Get current hardware statistics
    pub fn get_stats(&mut self) -> Result<HardwareStats> {
        // Return cached stats if still fresh
        if let Some(cached) = &self.cached_stats {
            if self.last_update.elapsed() < self.update_interval {
                return Ok(cached.clone());
            }
        }

        // Refresh system info
        self.system
            .refresh_cpu_specifics(sysinfo::CpuRefreshKind::everything());
        self.system.refresh_memory();

        // Get CPU usage
        let cpu_usage_percent = self.system.global_cpu_usage();

        // Get RAM usage
        let ram_used_bytes = self.system.used_memory();
        let ram_total_bytes = self.system.total_memory();
        let ram_used_gb = (ram_used_bytes as f32) / (1024.0 * 1024.0 * 1024.0);
        let ram_total_gb = (ram_total_bytes as f32) / (1024.0 * 1024.0 * 1024.0);

        // Get GPU info if available
        let gpu = if self.gpu_type != GpuType::None {
            get_gpu_info(self.gpu_type).ok()
        } else {
            None
        };

        let stats = HardwareStats {
            gpu,
            cpu_usage_percent,
            ram_used_gb,
            ram_total_gb,
            inference_speed: None, // Will be updated by the model during inference
            model_info: None,      // Will be updated when model is loaded
        };

        // Cache the stats
        self.cached_stats = Some(stats.clone());
        self.last_update = Instant::now();

        Ok(stats)
    }

    /// Update model information
    pub fn set_model_info(&mut self, model_info: ModelInfo) {
        if let Some(stats) = &mut self.cached_stats {
            stats.model_info = Some(model_info);
        }
    }

    /// Update inference speed
    pub fn set_inference_speed(&mut self, tokens_per_sec: f32) {
        if let Some(stats) = &mut self.cached_stats {
            stats.inference_speed = Some(tokens_per_sec);
        }
    }

    /// Get GPU type
    pub fn gpu_type(&self) -> GpuType {
        self.gpu_type
    }

    /// Check if GPU is available
    pub fn has_gpu(&self) -> bool {
        self.gpu_type != GpuType::None
    }
}

/// Shared hardware monitor for the application
pub type SharedHardwareMonitor = Arc<Mutex<HardwareMonitor>>;

/// Create a background monitoring task
pub fn create_monitoring_task(
    monitor: SharedHardwareMonitor,
    update_callback: impl Fn(HardwareStats) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;

            let stats = {
                let mut monitor = monitor.lock().await;
                monitor.get_stats()
            };

            if let Ok(stats) = stats {
                update_callback(stats);
            }
        }
    })
}

/// Estimate model memory requirements based on model name
pub fn estimate_model_memory(model_name: &str) -> Option<f32> {
    // Rough estimates based on common model sizes
    let lower = model_name.to_lowercase();

    if lower.contains("70b") || lower.contains("llama-2-70b") {
        Some(40.0) // 70B models typically need ~40GB in 4-bit
    } else if lower.contains("34b") || lower.contains("yi-34b") {
        Some(20.0)
    } else if lower.contains("13b") || lower.contains("llama-2-13b") {
        Some(8.0)
    } else if lower.contains("7b") || lower.contains("mistral") {
        Some(4.5)
    } else if lower.contains("3b") || lower.contains("phi") {
        Some(2.0)
    } else if lower.contains("1b") {
        Some(1.0)
    } else if lower.contains("gpt-4") {
        None // API model
    } else if lower.contains("gpt-3.5") {
        None // API model
    } else if lower.contains("claude") {
        None // API model
    } else {
        Some(4.0) // Default estimate
    }
}
