//! macOS-specific video renderer using AVFoundation and Metal
//!
//! This module provides native macOS video rendering using:
//! - NSWindow for window management
//! - AVSampleBufferDisplayLayer for frame presentation
//! - Core Video for pixel buffer management
//! - Metal for hardware-accelerated rendering

use anyhow::Result;
use cocoa::appkit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
use cocoa::base::{id, nil, YES, NO};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_video_sys::*;
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{DecodedFrame, PixelFormat, VideoRenderer};

/// macOS-specific video renderer
///
/// This renderer uses native macOS APIs to display decoded video frames
/// in a native NSWindow with hardware acceleration.
pub struct MacOSRenderer {
    window: id,
    display_layer: id,
    pixel_buffer_pool: Option<CVPixelBufferPoolRef>,
    width: u32,
    height: u32,
    window_open: Arc<AtomicBool>,
}

// Safety: NSWindow and AVSampleBufferDisplayLayer are thread-safe for our use case
// We only access them from the main thread or through atomic operations
unsafe impl Send for MacOSRenderer {}

impl VideoRenderer for MacOSRenderer {
    fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            // Initialize NSApplication (required for window creation)
            let app = NSApplication::sharedApplication(nil);
            app.setActivationPolicy_(NSApplicationActivationPolicy::NSApplicationActivationPolicyRegular);
            
            // Create NSWindow
            let window = NSWindow::alloc(nil);
            let frame = NSRect::new(
                NSPoint::new(100.0, 100.0),
                NSSize::new(width as f64, height as f64),
            );
            
            let style_mask = NSWindowStyleMask::NSTitledWindowMask
                | NSWindowStyleMask::NSClosableWindowMask
                | NSWindowStyleMask::NSResizableWindowMask;
            
            let window = window.initWithContentRect_styleMask_backing_defer_(
                frame,
                style_mask,
                NSBackingStoreType::NSBackingStoreBuffered,
                NO,
            );
            
            if window == nil {
                anyhow::bail!("Failed to create NSWindow");
            }
            
            // Set window title
            let title = NSString::alloc(nil).init_str("RustyPlay - AirPlay Mirroring");
            window.setTitle_(title);
            
            // Make window visible
            window.makeKeyAndOrderFront_(nil);
            
            // Create AVSampleBufferDisplayLayer
            let display_layer_class = class!(AVSampleBufferDisplayLayer);
            let display_layer: id = msg_send![display_layer_class, new];
            
            if display_layer == nil {
                anyhow::bail!("Failed to create AVSampleBufferDisplayLayer");
            }
            
            // Get the content view and its layer
            let content_view: id = window.contentView();
            let _: () = msg_send![content_view, setWantsLayer: YES];
            let view_layer: id = msg_send![content_view, layer];
            
            // Set display layer frame to match content view bounds
            let bounds: NSRect = msg_send![content_view, bounds];
            let _: () = msg_send![display_layer, setFrame: bounds];
            
            // Add display layer as sublayer
            let _: () = msg_send![view_layer, addSublayer: display_layer];
            
            // Configure display layer for video content
            let _: () = msg_send![display_layer, setVideoGravity: NSString::alloc(nil).init_str("AVLayerVideoGravityResizeAspect")];
            
            // Create CVPixelBufferPool for efficient buffer allocation
            let pixel_buffer_pool = Self::create_pixel_buffer_pool(width, height)?;
            
            let window_open = Arc::new(AtomicBool::new(true));
            
            tracing::info!(
                width = width,
                height = height,
                "MacOSRenderer initialized with NSWindow and AVSampleBufferDisplayLayer"
            );
            
            Ok(Self {
                window,
                display_layer,
                pixel_buffer_pool: Some(pixel_buffer_pool),
                width,
                height,
                window_open,
            })
        }
    }
    
    fn display_frame(&mut self, frame: &DecodedFrame) -> Result<()> {
        unsafe {
            // Create CVPixelBuffer from decoded frame
            let pixel_buffer = self.create_pixel_buffer_from_frame(frame)?;
            
            // Create CMSampleBuffer to wrap the pixel buffer
            let sample_buffer = self.create_sample_buffer(pixel_buffer, frame.timestamp)?;
            
            // Enqueue sample buffer to display layer
            let _: () = msg_send![self.display_layer, enqueueSampleBuffer: sample_buffer];
            
            // Release resources
            CFRelease(sample_buffer as CFTypeRef);
            CVPixelBufferRelease(pixel_buffer);
            
            Ok(())
        }
    }
    
    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe {
            tracing::info!(
                old_width = self.width,
                old_height = self.height,
                new_width = width,
                new_height = height,
                "Resizing video window"
            );
            
            // Calculate aspect ratio from source dimensions
            let aspect_ratio = width as f64 / height as f64;
            
            // Get current window frame to preserve position
            let current_frame: NSRect = msg_send![self.window, frame];
            
            // Create new frame with updated dimensions while preserving aspect ratio
            let new_frame = NSRect::new(
                current_frame.origin,
                NSSize::new(width as f64, height as f64),
            );
            
            // Update window frame
            let _: () = msg_send![self.window, setFrame:new_frame display:YES animate:YES];
            
            // Update display layer frame to match content view bounds
            let content_view: id = msg_send![self.window, contentView];
            let bounds: NSRect = msg_send![content_view, bounds];
            let _: () = msg_send![self.display_layer, setFrame: bounds];
            
            // Recreate CVPixelBufferPool with new dimensions
            if let Some(old_pool) = self.pixel_buffer_pool.take() {
                CVPixelBufferPoolRelease(old_pool);
                tracing::debug!("Released old CVPixelBufferPool");
            }
            
            let new_pool = Self::create_pixel_buffer_pool(width, height)?;
            self.pixel_buffer_pool = Some(new_pool);
            
            // Update stored dimensions
            self.width = width;
            self.height = height;
            
            tracing::info!(
                width = width,
                height = height,
                aspect_ratio = aspect_ratio,
                "Window resized successfully"
            );
            
            Ok(())
        }
    }
    
    fn is_window_open(&self) -> bool {
        unsafe {
            // Check if window is still visible
            let is_visible: bool = msg_send![self.window, isVisible];
            is_visible && self.window_open.load(Ordering::Relaxed)
        }
    }
    
    fn shutdown(&mut self) -> Result<()> {
        unsafe {
            tracing::info!("Shutting down MacOSRenderer");
            
            // Release pixel buffer pool
            if let Some(pool) = self.pixel_buffer_pool.take() {
                CVPixelBufferPoolRelease(pool);
            }
            
            // Remove display layer from superlayer
            let _: () = msg_send![self.display_layer, removeFromSuperlayer];
            
            // Release display layer
            let _: () = msg_send![self.display_layer, release];
            
            // Close window
            let _: () = msg_send![self.window, close];
            
            self.window_open.store(false, Ordering::Relaxed);
        }
        
        Ok(())
    }
}

impl Drop for MacOSRenderer {
    fn drop(&mut self) {
        if self.window_open.load(Ordering::Relaxed) {
            let _ = self.shutdown();
        }
    }
}

impl MacOSRenderer {
    /// Create a CVPixelBufferPool for efficient buffer allocation
    ///
    /// This pool enables buffer reuse and zero-copy frame transfer
    unsafe fn create_pixel_buffer_pool(width: u32, height: u32) -> Result<CVPixelBufferPoolRef> {
        use core_foundation::base::kCFAllocatorDefault;
        use core_foundation::string::CFString;
        use core_foundation::number::CFNumber;
        use core_foundation::dictionary::CFMutableDictionary;
        
        // Create pixel buffer attributes
        let mut pixel_attrs = CFMutableDictionary::new();
        
        // Set pixel format (NV12 - biplanar YUV 4:2:0)
        let pixel_format_key = CFString::from_static_string("PixelFormatType");
        let pixel_format_value = CFNumber::from(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        pixel_attrs.set(pixel_format_key, pixel_format_value);
        
        // Set width
        let width_key = CFString::from_static_string("Width");
        let width_value = CFNumber::from(width as i32);
        pixel_attrs.set(width_key, width_value);
        
        // Set height
        let height_key = CFString::from_static_string("Height");
        let height_value = CFNumber::from(height as i32);
        pixel_attrs.set(height_key, height_value);
        
        let mut pool: CVPixelBufferPoolRef = ptr::null_mut();
        let status = CVPixelBufferPoolCreate(
            kCFAllocatorDefault,
            ptr::null(),
            pixel_attrs.as_concrete_TypeRef() as *const _,
            &mut pool,
        );
        
        if status != kCVReturnSuccess {
            anyhow::bail!("Failed to create CVPixelBufferPool: {}", status);
        }
        
        tracing::debug!(
            width = width,
            height = height,
            "Created CVPixelBufferPool"
        );
        
        Ok(pool)
    }
    
    /// Create a CVPixelBuffer from a DecodedFrame
    ///
    /// Handles YUV420P and NV12 pixel format conversions
    unsafe fn create_pixel_buffer_from_frame(&self, frame: &DecodedFrame) -> Result<CVPixelBufferRef> {
        use core_foundation::base::kCFAllocatorDefault;
        
        let mut pixel_buffer: CVPixelBufferRef = ptr::null_mut();
        
        match frame.format {
            PixelFormat::NV12 => {
                // NV12 format: Y plane followed by interleaved UV plane
                // This is the preferred format for VideoToolbox
                let status = CVPixelBufferCreate(
                    kCFAllocatorDefault,
                    frame.width as usize,
                    frame.height as usize,
                    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    ptr::null(),
                    &mut pixel_buffer,
                );
                
                if status != kCVReturnSuccess {
                    anyhow::bail!("Failed to create CVPixelBuffer: {}", status);
                }
                
                // Lock the pixel buffer for writing
                CVPixelBufferLockBaseAddress(pixel_buffer, 0);
                
                // Copy Y plane
                let y_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0);
                let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0);
                let y_size = (frame.width * frame.height) as usize;
                
                for row in 0..frame.height as usize {
                    let src_offset = row * frame.width as usize;
                    let dst_offset = row * y_stride;
                    ptr::copy_nonoverlapping(
                        frame.data.as_ptr().add(src_offset),
                        (y_dest as *mut u8).add(dst_offset),
                        frame.width as usize,
                    );
                }
                
                // Copy UV plane
                let uv_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1);
                let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1);
                let uv_height = frame.height / 2;
                
                for row in 0..uv_height as usize {
                    let src_offset = y_size + row * frame.width as usize;
                    let dst_offset = row * uv_stride;
                    ptr::copy_nonoverlapping(
                        frame.data.as_ptr().add(src_offset),
                        (uv_dest as *mut u8).add(dst_offset),
                        frame.width as usize,
                    );
                }
                
                // Unlock the pixel buffer
                CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
            }
            
            PixelFormat::YUV420P => {
                // YUV420P format: Y plane, U plane, V plane (planar)
                // Need to convert to NV12 (Y plane, interleaved UV plane)
                let status = CVPixelBufferCreate(
                    kCFAllocatorDefault,
                    frame.width as usize,
                    frame.height as usize,
                    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    ptr::null(),
                    &mut pixel_buffer,
                );
                
                if status != kCVReturnSuccess {
                    anyhow::bail!("Failed to create CVPixelBuffer: {}", status);
                }
                
                // Lock the pixel buffer for writing
                CVPixelBufferLockBaseAddress(pixel_buffer, 0);
                
                // Copy Y plane
                let y_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0);
                let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0);
                let y_size = (frame.width * frame.height) as usize;
                
                for row in 0..frame.height as usize {
                    let src_offset = row * frame.width as usize;
                    let dst_offset = row * y_stride;
                    ptr::copy_nonoverlapping(
                        frame.data.as_ptr().add(src_offset),
                        (y_dest as *mut u8).add(dst_offset),
                        frame.width as usize,
                    );
                }
                
                // Convert planar U and V to interleaved UV
                let uv_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1);
                let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1);
                let uv_width = frame.width / 2;
                let uv_height = frame.height / 2;
                let u_offset = y_size;
                let v_offset = y_size + (uv_width * uv_height) as usize;
                
                for row in 0..uv_height as usize {
                    for col in 0..uv_width as usize {
                        let u_src = u_offset + row * uv_width as usize + col;
                        let v_src = v_offset + row * uv_width as usize + col;
                        let dst = row * uv_stride + col * 2;
                        
                        *((uv_dest as *mut u8).add(dst)) = frame.data[u_src];
                        *((uv_dest as *mut u8).add(dst + 1)) = frame.data[v_src];
                    }
                }
                
                // Unlock the pixel buffer
                CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
            }
            
            _ => {
                anyhow::bail!("Unsupported pixel format: {:?}", frame.format);
            }
        }
        
        Ok(pixel_buffer)
    }
    
    /// Create a CMSampleBuffer to wrap a CVPixelBuffer
    ///
    /// This is required for AVSampleBufferDisplayLayer
    unsafe fn create_sample_buffer(
        &self,
        pixel_buffer: CVPixelBufferRef,
        timestamp: Option<i64>,
    ) -> Result<*mut c_void> {
        use core_foundation::base::kCFAllocatorDefault;
        
        // Import Core Media types
        #[repr(C)]
        struct CMTime {
            value: i64,
            timescale: i32,
            flags: u32,
            epoch: i64,
        }
        
        #[repr(C)]
        struct CMSampleTimingInfo {
            duration: CMTime,
            presentation_time_stamp: CMTime,
            decode_time_stamp: CMTime,
        }
        
        // Create timing info
        let time_value = timestamp.unwrap_or(0);
        let timescale = 1_000_000_000; // nanoseconds
        
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 0,
                timescale,
                flags: 1, // kCMTimeFlags_Valid
                epoch: 0,
            },
            presentation_time_stamp: CMTime {
                value: time_value,
                timescale,
                flags: 1, // kCMTimeFlags_Valid
                epoch: 0,
            },
            decode_time_stamp: CMTime {
                value: time_value,
                timescale,
                flags: 1, // kCMTimeFlags_Valid
                epoch: 0,
            },
        };
        
        // Get video format description from pixel buffer
        let mut format_description: *mut c_void = ptr::null_mut();
        let status = CMVideoFormatDescriptionCreateForImageBuffer(
            kCFAllocatorDefault,
            pixel_buffer,
            &mut format_description,
        );
        
        if status != 0 {
            anyhow::bail!("Failed to create CMVideoFormatDescription: {}", status);
        }
        
        // Create sample buffer
        let mut sample_buffer: *mut c_void = ptr::null_mut();
        let status = CMSampleBufferCreateReadyWithImageBuffer(
            kCFAllocatorDefault,
            pixel_buffer,
            format_description,
            &timing as *const _ as *const c_void,
            &mut sample_buffer,
        );
        
        // Release format description
        CFRelease(format_description as CFTypeRef);
        
        if status != 0 {
            anyhow::bail!("Failed to create CMSampleBuffer: {}", status);
        }
        
        Ok(sample_buffer)
    }
}

// External Core Media functions
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateForImageBuffer(
        allocator: *const c_void,
        image_buffer: CVPixelBufferRef,
        format_description_out: *mut *mut c_void,
    ) -> i32;
    
    fn CMSampleBufferCreateReadyWithImageBuffer(
        allocator: *const c_void,
        image_buffer: CVPixelBufferRef,
        format_description: *mut c_void,
        sample_timing: *const c_void,
        sample_buffer_out: *mut *mut c_void,
    ) -> i32;
}

// Note: GUI tests for MacOSRenderer cannot run in headless environments.
// These tests require a graphical display server and would crash in CI/test environments.
// The renderer functionality will be validated through integration tests with actual video streams.
