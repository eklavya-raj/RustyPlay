//! Bug Condition Exploration Test for GUI Not Appearing After iPhone Connection
//!
//! **CRITICAL**: This test is EXPECTED TO FAIL on unfixed code.
//! Failure confirms the bug exists. Success after fix confirms the bug is resolved.
//!
//! **Validates: Requirements 1.1, 1.2, 1.3, 1.4**
//!
//! This integration test simulates an iPhone connecting to the AirPlay receiver
//! and verifies that the GUI window appears and audio plays.
//!
//! **NOTE**: This test requires the full application to be running, which has
//! dependencies on audio libraries (fdk-aac) and playfair decryption. For now,
//! we'll create a simplified test that focuses on the video pipeline initialization.

use std::time::Duration;
use tokio::time::sleep;

// Import necessary types from the main crate
use rusty_play::codec::{derive_video_keys, AudioCodec, SessionInfo, StreamType};
use rusty_play::video::VideoPipeline;


/// Check if a window with the specified title exists on macOS
#[cfg(target_os = "macos")]
fn check_window_exists(title: &str) -> bool {
    use cocoa::appkit::NSApplication;
    use cocoa::base::{id, nil};
    use objc::{msg_send, sel, sel_impl};
    
    unsafe {
        let app = NSApplication::sharedApplication(nil);
        let windows: id = msg_send![app, windows];
        let count: usize = msg_send![windows, count];
        
        for i in 0..count {
            let window: id = msg_send![windows, objectAtIndex: i];
            let window_title: id = msg_send![window, title];
            let title_str: *const i8 = msg_send![window_title, UTF8String];
            
            if !title_str.is_null() {
                let c_str = std::ffi::CStr::from_ptr(title_str);
                if let Ok(rust_str) = c_str.to_str() {
                    if rust_str == title {
                        // Check if window is visible
                        let is_visible: bool = msg_send![window, isVisible];
                        return is_visible;
                    }
                }
            }
        }
        
        false
    }
}

#[cfg(not(target_os = "macos"))]
fn check_window_exists(_title: &str) -> bool {
    // On non-macOS platforms, we can't check window existence
    // This test is macOS-specific
    false
}

#[tokio::test]
#[ignore] // Ignore by default as this requires a GUI environment
async fn test_gui_appears_after_iphone_connection() {
    // **Property 1: Bug Condition - GUI Appears After Connection**
    // **Validates: Requirements 1.1, 1.2, 1.3, 1.4**
    //
    // This is a simplified test that focuses on the core bug:
    // When VideoPipeline is initialized, does a window appear?
    
    // Initialize tracing for debugging
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    
    println!("=== Bug Condition Exploration Test ===");
    println!("Testing if GUI window appears when VideoPipeline is initialized");
    println!();
    
    // 1. Setup: Simulate the conditions when an iPhone connects
    println!("1. Simulating iPhone connection conditions:");
    println!("   ✓ Connection established (simulated)");
    println!("   ✓ Video keys available (simulated)");
    println!("   ✓ Codec config received (simulated)");
    
    // 2. Initialize VideoPipeline (this is where the bug manifests)
    println!("\n2. Initializing VideoPipeline...");
    
    let pipeline_result = VideoPipeline::new(true);
    
    match pipeline_result {
        Ok(mut pipeline) => {
            println!("   ✓ VideoPipeline created successfully");
            
            // Wait a moment for window to appear
            sleep(Duration::from_millis(1000)).await;
            
            // 3. Check if window is visible
            println!("\n3. Checking if window is visible...");
            
            let window_visible = check_window_exists("RustyPlay - AirPlay Mirroring");
            
            if window_visible {
                println!("   ✓ Window is visible!");
            } else {
                println!("   ✗ Window is NOT visible");
            }
            
            // **CRITICAL ASSERTION**: This will FAIL on unfixed code
            assert!(
                window_visible,
                "\n\n❌ BUG CONFIRMED: Window 'RustyPlay - AirPlay Mirroring' should be visible but is not.\n\
                 \n\
                 This confirms the bug exists. Counterexample found:\n\
                 - VideoPipeline was initialized successfully\n\
                 - MacOSRenderer was created (no error)\n\
                 - But window is not visible on screen\n\
                 \n\
                 Possible root causes:\n\
                 1. Window created on wrong thread (not main thread)\n\
                 2. NSApplication not activated properly\n\
                 3. Event loop not running\n\
                 4. Window created but makeKeyAndOrderFront not working from background thread\n\
                 \n\
                 Expected behavior: Window should appear immediately after VideoPipeline initialization.\n\
                 Actual behavior: Window does not appear.\n"
            );
            
            println!("\n✅ TEST PASSED: Window appeared as expected!");
            
            // Cleanup
            let _ = pipeline.shutdown();
        }
        Err(e) => {
            println!("   ✗ Failed to create VideoPipeline: {:?}", e);
            panic!(
                "\n\n❌ BUG CONFIRMED: VideoPipeline initialization failed.\n\
                 \n\
                 Error: {:?}\n\
                 \n\
                 This may indicate:\n\
                 1. NSWindow creation failed\n\
                 2. AVSampleBufferDisplayLayer creation failed\n\
                 3. CVPixelBufferPool creation failed\n\
                 \n\
                 The bug prevents the video pipeline from even initializing.\n",
                e
            );
        }
    }
}
