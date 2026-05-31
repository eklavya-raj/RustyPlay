//! OBS AirPlay Plugin - Rust FFI Layer
//!
//! This library provides the Foreign Function Interface (FFI) for the OBS AirPlay plugin.
//! It exposes the AirPlay server functionality to OBS Studio via C-compatible functions.

use std::sync::{Arc, Mutex};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

// Re-export the main AirPlay library
use rusty_play::{AirPlayServer, ServerConfig};

/// Global server instance
static mut SERVER: Option<AirPlayServer> = None;

/// Initialize the AirPlay server
#[no_mangle]
pub extern "C" fn obs_airplay_server_init() -> c_int {
    unsafe {
        if SERVER.is_some() {
            return 0; // Already initialized
        }
        let config = ServerConfig::default();
        SERVER = Some(AirPlayServer::new(config));
        1 // Success
    }
}

/// Start the AirPlay server
#[no_mangle]
pub extern "C" fn obs_airplay_server_start(http_port: u16, rtsp_port: u16) -> c_int {
    unsafe {
        if let Some(ref mut server) = SERVER {
            // Update config with custom ports
            server.config.http_port = http_port;
            server.config.rtsp_port = rtsp_port;
            
            match server.start() {
                Ok(_) => 1,
                Err(_) => 0,
            }
        } else {
            0 // Not initialized
        }
    }
}

/// Stop the AirPlay server
#[no_mangle]
pub extern "C" fn obs_airplay_server_stop() {
    unsafe {
        if let Some(ref mut server) = SERVER {
            server.stop();
        }
    }
}

/// Check if the server is running
#[no_mangle]
pub extern "C" fn obs_airplay_server_is_running() -> c_int {
    unsafe {
        if let Some(ref server) = SERVER {
            if server.is_running() {
                1
            } else {
                0
            }
        } else {
            0
        }
    }
}

/// Cleanup the AirPlay server
#[no_mangle]
pub extern "C" fn obs_airplay_server_cleanup() {
    unsafe {
        if let Some(mut server) = SERVER.take() {
            server.stop();
        }
    }
}

/// Get the last error message
#[no_mangle]
pub extern "C" fn obs_airplay_get_last_error() -> *const c_char {
    // For now, return a static error message
    // In a real implementation, this would return the last error from the server
    CString::new("No error")
        .unwrap()
        .into_raw()
}

/// Free a string allocated by Rust
#[no_mangle]
pub extern "C" fn obs_airplay_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            let _ = CString::from_raw(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_init() {
        assert_eq!(obs_airplay_server_init(), 1);
        assert_eq!(obs_airplay_server_init(), 0); // Already initialized
        obs_airplay_server_cleanup();
    }

    #[test]
    fn test_server_lifecycle() {
        obs_airplay_server_init();
        assert_eq!(obs_airplay_server_is_running(), 0);
        obs_airplay_server_start(7000, 5000);
        // Note: The actual start might fail if ports are in use
        obs_airplay_server_stop();
        obs_airplay_server_cleanup();
    }
}
