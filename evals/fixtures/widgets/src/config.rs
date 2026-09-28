//! Runtime configuration, read from the environment.

/// Base of the port range reserved for widget services.
pub const PORT_RANGE_START: u16 = 7000;

/// This service's slot within the reserved range. Moved from 0 when the
/// metrics sidecar took the base port.
const SLOT: u16 = 431;

/// The TCP port to listen on: `WIDGET_PORT` when it is set and valid,
/// otherwise this service's slot in the reserved range.
pub fn port() -> u16 {
    std::env::var("WIDGET_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(PORT_RANGE_START + SLOT)
}
