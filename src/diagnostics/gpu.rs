use super::types::{GpuInfo, GpuType};
use anyhow::Result;
use std::process::Command;

/// Detect which type of GPU is available
pub fn detect_gpu_type() -> GpuType {
    // Check for NVIDIA GPU
    if is_nvidia_available() {
        return GpuType::Nvidia;
    }

    // Check for AMD GPU
    if is_amd_available() {
        return GpuType::Amd;
    }

    // Check for Apple Silicon
    #[cfg(target_os = "macos")]
    {
        if is_apple_silicon() {
            return GpuType::AppleSilicon;
        }
    }

    // Check for Intel GPU
    if is_intel_gpu_available() {
        return GpuType::Intel;
    }

    GpuType::None
}

/// Get GPU information based on type
pub fn get_gpu_info(gpu_type: GpuType) -> Result<GpuInfo> {
    match gpu_type {
        GpuType::Nvidia => get_nvidia_info(),
        GpuType::Amd => get_amd_info(),
        GpuType::AppleSilicon => get_apple_silicon_info(),
        GpuType::Intel => get_intel_info(),
        GpuType::None => anyhow::bail!("No GPU detected"),
    }
}

/// Check if NVIDIA GPU is available
fn is_nvidia_available() -> bool {
    Command::new("nvidia-smi")
        .arg("--query-gpu=name")
        .arg("--format=csv,noheader")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Get NVIDIA GPU information
fn get_nvidia_info() -> Result<GpuInfo> {
    // Query GPU name
    let name_output = Command::new("nvidia-smi")
        .arg("--query-gpu=name")
        .arg("--format=csv,noheader")
        .output()?;
    let name = String::from_utf8_lossy(&name_output.stdout)
        .trim()
        .to_string();

    // Query GPU utilization
    let util_output = Command::new("nvidia-smi")
        .arg("--query-gpu=utilization.gpu")
        .arg("--format=csv,noheader,nounits")
        .output()?;
    let usage_percent: f32 = String::from_utf8_lossy(&util_output.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0);

    // Query memory usage
    let mem_output = Command::new("nvidia-smi")
        .arg("--query-gpu=memory.used,memory.total")
        .arg("--format=csv,noheader,nounits")
        .output()?;
    let mem_string = String::from_utf8_lossy(&mem_output.stdout);
    let mem_parts: Vec<&str> = mem_string.trim().split(',').map(|s| s.trim()).collect();

    let memory_used_mb: f32 = mem_parts.get(0).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let memory_total_mb: f32 = mem_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);

    // Query temperature
    let temp_output = Command::new("nvidia-smi")
        .arg("--query-gpu=temperature.gpu")
        .arg("--format=csv,noheader,nounits")
        .output()?;
    let temperature_celsius: Option<f32> = String::from_utf8_lossy(&temp_output.stdout)
        .trim()
        .parse()
        .ok();

    Ok(GpuInfo {
        name,
        gpu_type: GpuType::Nvidia,
        usage_percent,
        memory_used_gb: memory_used_mb / 1024.0,
        memory_total_gb: memory_total_mb / 1024.0,
        temperature_celsius,
    })
}

/// Check if AMD GPU is available
fn is_amd_available() -> bool {
    // Check for ROCm
    Command::new("rocm-smi")
        .arg("--showproductname")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Get AMD GPU information
fn get_amd_info() -> Result<GpuInfo> {
    // This is a simplified version - AMD GPU monitoring is more complex
    // and would need proper ROCm integration
    Ok(GpuInfo {
        name: "AMD GPU".to_string(),
        gpu_type: GpuType::Amd,
        usage_percent: 0.0,
        memory_used_gb: 0.0,
        memory_total_gb: 0.0,
        temperature_celsius: None,
    })
}

/// Check if running on Apple Silicon
#[cfg(target_os = "macos")]
fn is_apple_silicon() -> bool {
    Command::new("sysctl")
        .arg("-n")
        .arg("machdep.cpu.brand_string")
        .output()
        .map(|output| {
            let cpu_info = String::from_utf8_lossy(&output.stdout);
            cpu_info.contains("Apple M1")
                || cpu_info.contains("Apple M2")
                || cpu_info.contains("Apple M3")
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn is_apple_silicon() -> bool {
    false
}

/// Get Apple Silicon GPU information
fn get_apple_silicon_info() -> Result<GpuInfo> {
    #[cfg(target_os = "macos")]
    {
        // Get system info using sysctl
        let output = Command::new("sysctl")
            .arg("-n")
            .arg("machdep.cpu.brand_string")
            .output()?;
        let cpu_info = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Parse unified memory from system_profiler
        let mem_output = Command::new("system_profiler")
            .arg("SPHardwareDataType")
            .output()?;
        let mem_info = String::from_utf8_lossy(&mem_output.stdout);

        // Extract memory size (rough parsing)
        let memory_gb = if mem_info.contains("64 GB") {
            64.0
        } else if mem_info.contains("32 GB") {
            32.0
        } else if mem_info.contains("16 GB") {
            16.0
        } else if mem_info.contains("8 GB") {
            8.0
        } else {
            16.0
        }; // Default

        // For Apple Silicon, we can't easily get real-time GPU usage
        // This would require using Metal Performance Shaders
        Ok(GpuInfo {
            name: cpu_info,
            gpu_type: GpuType::AppleSilicon,
            usage_percent: 0.0,  // Would need Metal API
            memory_used_gb: 0.0, // Unified memory - hard to separate
            memory_total_gb: memory_gb,
            temperature_celsius: None,
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("Apple Silicon detection not available on this platform")
    }
}

/// Check if Intel GPU is available
fn is_intel_gpu_available() -> bool {
    // Check for Intel GPU tools
    Command::new("intel_gpu_top")
        .arg("-l")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Get Intel GPU information
fn get_intel_info() -> Result<GpuInfo> {
    // Simplified Intel GPU info
    Ok(GpuInfo {
        name: "Intel GPU".to_string(),
        gpu_type: GpuType::Intel,
        usage_percent: 0.0,
        memory_used_gb: 0.0,
        memory_total_gb: 0.0,
        temperature_celsius: None,
    })
}
