//! `xcap` 0.9.10 screen capture backend implementing `FrameCapturer`.

use super::{
    resolve_monitor_crop, validate_region_dimensions, CaptureError, CapturedFrame, FrameCapturer,
    MonitorInfo,
};
use crate::config::CaptureRegion;

/// Default screen capturer backed by `xcap` 0.9.10.
#[derive(Debug, Default, Clone, Copy)]
pub struct XcapCapturer;

impl XcapCapturer {
    pub fn new() -> Self {
        Self
    }
}

fn monitor_bounds_info(monitor: &xcap::Monitor, id: String) -> Result<MonitorInfo, CaptureError> {
    let x = monitor
        .x()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let y = monitor
        .y()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let width = monitor
        .width()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let height = monitor
        .height()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;

    Ok(MonitorInfo {
        id: id.clone(),
        name: id,
        x,
        y,
        width,
        height,
        scale_factor: 1.0,
        is_primary: false,
    })
}

fn monitor_to_info(monitor: &xcap::Monitor) -> Result<MonitorInfo, CaptureError> {
    let id = monitor
        .name()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let name = monitor.friendly_name().unwrap_or_else(|_| id.clone());
    let mut info = monitor_bounds_info(monitor, id)?;
    info.name = name;
    info.scale_factor = monitor
        .scale_factor()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    info.is_primary = monitor
        .is_primary()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    Ok(info)
}

impl FrameCapturer for XcapCapturer {
    fn list_monitors(&self) -> Result<Vec<MonitorInfo>, CaptureError> {
        let monitors = xcap::Monitor::all().map_err(|e| CaptureError::Backend(e.to_string()))?;
        monitors.iter().map(monitor_to_info).collect()
    }

    fn capture_region(&self, region: &CaptureRegion) -> Result<CapturedFrame, CaptureError> {
        validate_region_dimensions(region.width, region.height)?;

        let monitors = xcap::Monitor::all().map_err(|e| CaptureError::Backend(e.to_string()))?;
        let mut matched = None;
        for monitor in &monitors {
            let id = monitor
                .name()
                .map_err(|e| CaptureError::Backend(e.to_string()))?;
            if id == region.monitor_id {
                matched = Some(monitor);
                break;
            }
        }
        let matched = matched.ok_or_else(|| CaptureError::MonitorUnavailable {
            monitor_id: region.monitor_id.clone(),
        })?;

        let info = monitor_bounds_info(matched, region.monitor_id.clone())?;
        let (_, crop) = resolve_monitor_crop(region, &[info])?;
        let rgba_image = matched
            .capture_region(crop.x, crop.y, crop.width, crop.height)
            .map_err(|e| CaptureError::Backend(e.to_string()))?;

        let width = rgba_image.width();
        let height = rgba_image.height();
        CapturedFrame::new(width, height, rgba_image.into_raw())
    }
}
