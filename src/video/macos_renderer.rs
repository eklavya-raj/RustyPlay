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
use dispatch;
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use core_foundation::base::kCFAllocatorDefault;

use super::{DEFAULT_MIRROR_FPS, VIDEO_PTS_TIMESCALE};

use super::{DecodedFrame, PixelFormat, VideoRenderer};



// Global flag to ensure NSApplication event loop is started only once
static START_EVENT_LOOP: Once = Once::new();

/// macOS-specific video renderer
///
/// This renderer uses native macOS APIs to display decoded video frames
/// in a native NSWindow with hardware acceleration.
/// Max frames awaiting main-thread enqueue (drop only under heavy backlog).
const MAX_DISPLAY_IN_FLIGHT: usize = 8;

pub struct MacOSRenderer {
    window: id,
    display_layer: id,
    pixel_buffer_pool: Option<CVPixelBufferPoolRef>,
    width: u32,
    height: u32,
    window_open: Arc<AtomicBool>,
    display_fps: u32,
    in_flight: Arc<AtomicUsize>,
    timebase_started: Arc<AtomicBool>,
    resize_in_progress: Arc<AtomicBool>,
}

// Safety: NSWindow and AVSampleBufferDisplayLayer are thread-safe for our use case
// We only access them from the main thread or through atomic operations
unsafe impl Send for MacOSRenderer {}

impl VideoRenderer for MacOSRenderer {
    fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            // NSWindow and AppKit operations MUST run on the main thread
            // Use GCD to dispatch window creation to main thread synchronously
            // We send raw pointers as usize to make them Send-safe
            use std::sync::mpsc;
            
            let (tx, rx) = mpsc::channel::<Result<(usize, usize)>>();
            
            // Capture width and height explicitly for the closure
            let w = width;
            let h = height;
            
            // The closure will be executed on the main thread
            // We send raw pointers as usize which are Send
            dispatch::Queue::main().exec_async(move || {
                unsafe {
                    // Initialize NSApplication (required for window creation)
                    let app = NSApplication::sharedApplication(nil);
                    app.setActivationPolicy_(NSApplicationActivationPolicy::NSApplicationActivationPolicyRegular);
                    
                    // Create NSWindow
                    let window = NSWindow::alloc(nil);
                    
                    // Check if allocation succeeded
                    if window == nil {
                        tracing::error!("NSWindow::alloc() returned nil - window allocation failed");
                        let _ = tx.send(Err(anyhow::anyhow!("Failed to allocate NSWindow")));
                        return;
                    }
                    
                    let frame = NSRect::new(
                        NSPoint::new(100.0, 100.0),
                        NSSize::new(w as f64, h as f64),
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
                    
                    // Check if initialization succeeded
                    if window == nil {
                        tracing::error!("NSWindow initialization failed - initWithContentRect returned nil");
                        let _ = tx.send(Err(anyhow::anyhow!("Failed to initialize NSWindow")));
                        return;
                    }
                    
                    // Set window title
                    let title = NSString::alloc(nil).init_str("RustyPlay - AirPlay Mirroring");
                    window.setTitle_(title);
                    
                    // Activate the application to bring window to foreground
                    app.activateIgnoringOtherApps_(YES);
                    
                    // Make window visible and key
                    window.makeKeyAndOrderFront_(nil);
                    
                    // Force window to display immediately
                    let _: () = msg_send![window, display];
                    
                    tracing::info!("NSWindow created and made visible");
                    
                    // Create AVSampleBufferDisplayLayer
                    let display_layer_class = class!(AVSampleBufferDisplayLayer);
                    let display_layer: id = msg_send![display_layer_class, new];
                    
                    if display_layer == nil {
                        tracing::error!("AVSampleBufferDisplayLayer creation failed");
                        let _ = tx.send(Err(anyhow::anyhow!("Failed to create AVSampleBufferDisplayLayer")));
                        return;
                    }
                    
                    // Get the content view and its layer
                    let content_view: id = window.contentView();
                    let _: () = msg_send![content_view, setWantsLayer: YES];
                    let view_layer: id = msg_send![content_view, layer];
                    
                    // Set display layer frame to match content view bounds
                    let bounds: NSRect = msg_send![content_view, bounds];
                    let _: () = msg_send![display_layer, setFrame: bounds];
                    
                    // Set autoresizing mask so layer resizes with view
                    let _: () = msg_send![display_layer, setAutoresizingMask: 31]; // 31 = all edges flexible
                    
                    // Add display layer as sublayer
                    let _: () = msg_send![view_layer, addSublayer: display_layer];
                    
                    // Configure display layer for video content
                    // Use ResizeAspect to maintain aspect ratio without cropping (allows letterboxing)
                    let _: () = msg_send![display_layer, setVideoGravity: NSString::alloc(nil).init_str("AVLayerVideoGravityResizeAspect")];
                    tracing::debug!("Display layer gravity set to ResizeAspect");

                    // Send the window and display_layer back as raw pointers (usize)
                    let _ = tx.send(Ok((window as usize, display_layer as usize)));
                }
            });
            
            // Wait for the window creation to complete on the main thread
            let (window_ptr, display_layer_ptr) = rx.recv()
                .map_err(|e| anyhow::anyhow!("Failed to receive window from main thread: {}", e))??;
            
            // Convert back to id pointers
            let window = window_ptr as id;
            let display_layer = display_layer_ptr as id;
            
            // Create CVPixelBufferPool for efficient buffer allocation
            let pixel_buffer_pool = Self::create_pixel_buffer_pool(width, height)?;
            
            let window_open = Arc::new(AtomicBool::new(true));
            
            tracing::info!(
                width = width,
                height = height,
                "MacOSRenderer initialized with NSWindow and AVSampleBufferDisplayLayer on main thread"
            );
            
            Ok(Self {
                window,
                display_layer,
                pixel_buffer_pool: Some(pixel_buffer_pool),
                width,
                height,
                window_open,
                display_fps: DEFAULT_MIRROR_FPS,
                in_flight: Arc::new(AtomicUsize::new(0)),
                timebase_started: Arc::new(AtomicBool::new(false)),
                resize_in_progress: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    fn set_display_fps(&mut self, fps: u32) {
        self.display_fps = fps.max(1);
        tracing::info!(display_fps = self.display_fps, "Video display frame rate updated");
    }

    fn display_frame(&mut self, frame: DecodedFrame) -> Result<()> {
        // Skip frame enqueueing during resize to avoid dimension mismatches
        if self.resize_in_progress.load(Ordering::Acquire) {
            tracing::debug!("Skipping frame display during resize operation");
            return Ok(());
        }

        if self.in_flight.load(Ordering::Acquire) >= MAX_DISPLAY_IN_FLIGHT {
            tracing::trace!("Dropping video frame — main-thread backlog");
            return Ok(());
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);

        unsafe {
            let display_layer_ptr = self.display_layer as usize;
            let display_fps = self.display_fps;
            let in_flight = self.in_flight.clone();
            let timebase_started = self.timebase_started.clone();

            dispatch::Queue::main().exec_async(move || {
                unsafe {
                    let display_layer = display_layer_ptr as id;
                    if !timebase_started.swap(true, Ordering::AcqRel) {
                        ensure_host_timebase(display_layer);
                    }

                    let result = (|| -> Result<()> {
                        let pixel_buffer = create_pixel_buffer_from_frame_on_main(&frame)?;
                        let sample_buffer =
                            create_sample_buffer_on_main(pixel_buffer, frame.timestamp, display_fps)?;
                        let _: () = msg_send![display_layer, enqueueSampleBuffer: sample_buffer];
                        unsafe { CFRelease(sample_buffer as CFTypeRef) };
                        unsafe { CVPixelBufferRelease(pixel_buffer) };
                        Ok(())
                    })();

                    if let Err(e) = result {
                        tracing::warn!("Failed to display frame on main thread: {:?}", e);
                    }
                }
                in_flight.fetch_sub(1, Ordering::Release);
            });
        }

        Ok(())
    }

    
    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        // Set flag to pause frame enqueueing during resize
        self.resize_in_progress.store(true, Ordering::Release);

        unsafe {
            tracing::info!(
                old_width = self.width,
                old_height = self.height,
                new_width = width,
                new_height = height,
                "Resizing video window"
            );
            
            // All AppKit/CoreAnimation operations MUST run on the main thread.
            // We use exec_sync to block the calling (tokio) thread until the
            // main thread completes the window/layer resize.
            let window_ptr = self.window as usize;
            let display_layer_ptr = self.display_layer as usize;
            let w = width as f64;
            let h = height as f64;
            
            use std::sync::mpsc;
            let (tx, rx) = mpsc::channel();
            
            dispatch::Queue::main().exec_async(move || {
                unsafe {
                    let window = window_ptr as id;
                    let display_layer = display_layer_ptr as id;
                    
                    // CRITICAL: Flush the display layer to clear any buffered frames
                    // that may have dimensions mismatched with the new resolution.
                    // This prevents video from getting stuck during orientation changes.
                    let _: () = msg_send![display_layer, flushAndRemoveImage];
                    tracing::debug!("Flushed AVSampleBufferDisplayLayer");
                    
                    // Get current window frame to preserve position
                    let current_frame: NSRect = msg_send![window, frame];
                    
                    // Create new frame with updated dimensions
                    let new_frame = NSRect::new(
                        current_frame.origin,
                        NSSize::new(w, h),
                    );
                    
                    // Update window frame (no animation to avoid thread issues)
                    let _: () = msg_send![window, setFrame:new_frame display:YES animate:NO];
                    let _: () = msg_send![window, display];
                    
                    // Update display layer frame to match content view bounds
                    let content_view: id = msg_send![window, contentView];
                    let bounds: NSRect = msg_send![content_view, bounds];
                    let _: () = msg_send![display_layer, setFrame: bounds];
                    
                    tracing::info!(
                        layer_x = bounds.origin.x,
                        layer_y = bounds.origin.y,
                        layer_width = bounds.size.width,
                        layer_height = bounds.size.height,
                        "Updated display layer frame during resize"
                    );
                }
                let _ = tx.send(());
            });
            
            // Wait for the main thread to finish the resize
            rx.recv().ok();
            
            // Recreate CVPixelBufferPool with new dimensions (safe off main thread)
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
                "Window resized synchronously via main thread"
            );
        }

        // Clear flag to resume frame enqueueing after resize
        self.resize_in_progress.store(false, Ordering::Release);

        Ok(())
    }
    
    fn is_window_open(&self) -> bool {
        // Use atomic flag only to avoid calling AppKit from non-main threads
        self.window_open.load(Ordering::Relaxed)
    }
    
    fn shutdown(&mut self) -> Result<()> {
        unsafe {
            tracing::info!("Shutting down MacOSRenderer");
            
            // Release pixel buffer pool (safe off main thread)
            if let Some(pool) = self.pixel_buffer_pool.take() {
                CVPixelBufferPoolRelease(pool);
            }
            
            // AppKit operations must go through the main thread
            let window_ptr = self.window as usize;
            let display_layer_ptr = self.display_layer as usize;
            
            dispatch::Queue::main().exec_async(move || {
                unsafe {
                    let window = window_ptr as id;
                    let display_layer = display_layer_ptr as id;
                    
                    // Remove display layer from superlayer
                    let _: () = msg_send![display_layer, removeFromSuperlayer];
                    
                    // Release display layer
                    let _: () = msg_send![display_layer, release];
                    
                    // Close window
                    let _: () = msg_send![window, close];
                }
            });
            
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
        let status = unsafe {
            CVPixelBufferPoolCreate(
                kCFAllocatorDefault,
                ptr::null(),
                pixel_attrs.as_concrete_TypeRef() as *const _,
                &mut pool,
            )
        };
        
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
                let status = unsafe {
                    CVPixelBufferCreate(
                        kCFAllocatorDefault,
                        frame.width as usize,
                        frame.height as usize,
                        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                        ptr::null(),
                        &mut pixel_buffer,
                    )
                };
                
                if status != kCVReturnSuccess {
                    anyhow::bail!("Failed to create CVPixelBuffer: {}", status);
                }
                
                // Lock the pixel buffer for writing
                unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
                
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
                let status = unsafe {
                    CVPixelBufferCreate(
                        kCFAllocatorDefault,
                        frame.width as usize,
                        frame.height as usize,
                        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                        ptr::null(),
                        &mut pixel_buffer,
                    )
                };
                
                if status != kCVReturnSuccess {
                    anyhow::bail!("Failed to create CVPixelBuffer: {}", status);
                }
                
                // Lock the pixel buffer for writing
                unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
                
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
        let timescale = 30i32; // nanoseconds
        
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
        let status = unsafe {
            CMVideoFormatDescriptionCreateForImageBuffer(
                kCFAllocatorDefault,
                pixel_buffer,
                &mut format_description,
            )
        };
        
        if status != 0 {
            anyhow::bail!("Failed to create CMVideoFormatDescription: {}", status);
        }
        
        // Create sample buffer
        let mut sample_buffer: *mut c_void = ptr::null_mut();
        let status = unsafe {
            CMSampleBufferCreateReadyWithImageBuffer(
                kCFAllocatorDefault,
                pixel_buffer,
                format_description,
                &timing as *const _ as *const c_void,
                &mut sample_buffer,
            )
        };
        
        // Release format description
        unsafe { CFRelease(format_description as CFTypeRef) };
        
        if status != 0 {
            anyhow::bail!("Failed to create CMSampleBuffer: {}", status);
        }
        
        Ok(sample_buffer)
    }
}

/// Create a CVPixelBuffer from a DecodedFrame (designed to run on main thread)
unsafe fn create_pixel_buffer_from_frame_on_main(frame: &DecodedFrame) -> Result<CVPixelBufferRef> {
    use core_foundation::base::kCFAllocatorDefault;
    
    let mut pixel_buffer: CVPixelBufferRef = ptr::null_mut();
    let width = frame.width as usize;
    let height = frame.height as usize;
    
    match frame.format {
        PixelFormat::NV12 => {
            let status = unsafe {
                CVPixelBufferCreate(
                    kCFAllocatorDefault,
                    width, height,
                    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    ptr::null(),
                    &mut pixel_buffer,
                )
            };
            
            if status != kCVReturnSuccess {
                anyhow::bail!("Failed to create CVPixelBuffer: {}", status);
            }
            
            unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
            
            // Copy Y plane
            let y_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0);
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0) as usize;
            for row in 0..height {
                let src_off = row * width;
                let dst_off = row * y_stride;
                ptr::copy_nonoverlapping(
                    frame.data.as_ptr().add(src_off),
                    (y_dest as *mut u8).add(dst_off),
                    width,
                );
            }
            
            // Copy UV plane
            let uv_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1);
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1) as usize;
            let uv_height = height / 2;
            let y_size = width * height;
            for row in 0..uv_height {
                let src_off = y_size + row * width;
                let dst_off = row * uv_stride;
                ptr::copy_nonoverlapping(
                    frame.data.as_ptr().add(src_off),
                    (uv_dest as *mut u8).add(dst_off),
                    width,
                );
            }
            
            CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
        }
        
        PixelFormat::YUV420P => {
            // Create NV12 pixel buffer (preferred by CoreVideo)
            let status = unsafe {
                CVPixelBufferCreate(
                    kCFAllocatorDefault,
                    width, height,
                    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    ptr::null(),
                    &mut pixel_buffer,
                )
            };
            
            if status != kCVReturnSuccess {
                anyhow::bail!("Failed to create CVPixelBuffer: {}", status);
            }
            
            unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
            
            // Copy Y plane (same for both formats)
            let y_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0);
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0) as usize;
            for row in 0..height {
                let src_off = row * width;
                let dst_off = row * y_stride;
                ptr::copy_nonoverlapping(
                    frame.data.as_ptr().add(src_off),
                    (y_dest as *mut u8).add(dst_off),
                    width,
                );
            }
            
            // YUV420P has separate U and V planes -> convert to interleaved NV12 UV plane
            let uv_dest = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1);
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1) as usize;
            let uv_height = height / 2;
            let uv_width = width / 2;
            let y_size = width * height;
            let u_size = uv_width * uv_height;
            
            for row in 0..uv_height {
                for col in 0..uv_width {
                    let u_src = y_size + row * uv_width + col;
                    let v_src = y_size + u_size + row * uv_width + col;
                    let dst = row * uv_stride + col * 2;
                    *((uv_dest as *mut u8).add(dst)) = frame.data[u_src];
                    *((uv_dest as *mut u8).add(dst + 1)) = frame.data[v_src];
                }
            }
            
            CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
        }
        
        _ => {
            anyhow::bail!("Unsupported pixel format: {:?}", frame.format);
        }
    }
    
    Ok(pixel_buffer)
}

/// Create a CMSampleBuffer to wrap a CVPixelBuffer (designed to run on main thread)
unsafe fn create_sample_buffer_on_main(
    pixel_buffer: CVPixelBufferRef,
    timestamp: Option<i64>,
    display_fps: u32,
) -> Result<*mut c_void> {
    use core_foundation::base::kCFAllocatorDefault;
    
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
    
    let time_value = timestamp.unwrap_or(0);
    let timescale = VIDEO_PTS_TIMESCALE;
    let frame_duration_ns = 1_000_000_000i64 / display_fps.max(1) as i64;

    let timing = CMSampleTimingInfo {
        duration: CMTime {
            value: frame_duration_ns,
            timescale,
            flags: 1,
            epoch: 0,
        },
        presentation_time_stamp: CMTime {
            value: time_value,
            timescale,
            flags: 1,
            epoch: 0,
        },
        decode_time_stamp: CMTime {
            value: time_value,
            timescale,
            flags: 1,
            epoch: 0,
        },
    };
    
    let mut format_description: *mut c_void = ptr::null_mut();
    let status = unsafe {
        CMVideoFormatDescriptionCreateForImageBuffer(
            kCFAllocatorDefault,
            pixel_buffer,
            &mut format_description,
        )
    };
    
    if status != 0 {
        anyhow::bail!("Failed to create CMVideoFormatDescription: {}", status);
    }
    
    let mut sample_buffer: *mut c_void = ptr::null_mut();
    let status = unsafe {
        CMSampleBufferCreateReadyWithImageBuffer(
            kCFAllocatorDefault,
            pixel_buffer,
            format_description,
            &timing as *const _ as *const c_void,
            &mut sample_buffer,
        )
    };
    
    unsafe { CFRelease(format_description as CFTypeRef) };
    
    if status != 0 {
        anyhow::bail!("Failed to create CMSampleBuffer: {}", status);
    }
    
    Ok(sample_buffer)
}

#[repr(C)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// Bind ASBDL to the host clock once (live mirroring).
unsafe fn ensure_host_timebase(display_layer: id) {
    let mut timebase: *mut c_void = ptr::null_mut();
    let host_clock = unsafe { CMClockGetHostTimeClock() };
    if host_clock.is_null() {
        return;
    }
    if unsafe { CMTimebaseCreateWithMasterClock(kCFAllocatorDefault, host_clock, &mut timebase) } != 0 {
        return;
    }
    let now = unsafe { CMClockGetTime(host_clock) };
    unsafe { CMTimebaseSetTime(timebase, now) };
    unsafe { CMTimebaseSetRate(timebase, 1.0) };
    let _: () = msg_send![display_layer, setControlTimebase: timebase];
}

// External Core Media functions
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMClockGetHostTimeClock() -> *mut c_void;
    fn CMClockGetTime(clock: *mut c_void) -> CMTime;
    fn CMTimebaseCreateWithMasterClock(
        allocator: *const c_void,
        master_clock: *mut c_void,
        timebase_out: *mut *mut c_void,
    ) -> i32;
    fn CMTimebaseSetTime(timebase: *mut c_void, time: CMTime) -> i32;
    fn CMTimebaseSetRate(timebase: *mut c_void, rate: f64) -> i32;

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
