use anyhow::Result;
use tracing::info;
use std::process::Command;

#[cfg(target_os = "macos")]
use bonjour_sys::DNSServiceRegister;

#[cfg(not(target_os = "macos"))]
use libmdns::Responder;

/// Get the system hostname
fn get_hostname() -> String {
    #[cfg(target_os = "macos")]
    {
        Command::new("hostname")
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_else(|_| "localhost".to_string())
    }

    #[cfg(not(target_os = "macos"))]
    {
        Command::new("hostname")
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_else(|_| "localhost".to_string())
    }
}

/// Get the system MAC address for the first network interface (lowercase with colons for AirPlay)
pub fn get_mac_address() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("ifconfig").arg("-a").output() {
            let output_str = String::from_utf8_lossy(&output.stdout);
            
            // Parse blocks
            let mut blocks = Vec::new();
            let mut current_block = Vec::new();
            
            for line in output_str.lines() {
                if !line.starts_with('\t') && !line.starts_with(' ') && !line.is_empty() {
                    if !current_block.is_empty() {
                        blocks.push(current_block);
                        current_block = Vec::new();
                    }
                }
                current_block.push(line);
            }
            if !current_block.is_empty() {
                blocks.push(current_block);
            }
            
            let mut candidates = Vec::new();
            
            for block in blocks {
                if block.is_empty() {
                    continue;
                }
                let first_line = block[0];
                let name = first_line.split(':').next().unwrap_or("").trim().to_string();
                
                let mut mac = None;
                let mut is_active = false;
                let mut has_ip = false;
                
                for line in block.iter().skip(1) {
                    let line_trimmed = line.trim();
                    if line_trimmed.starts_with("ether ") {
                        if let Some(m) = line_trimmed.split_whitespace().nth(1) {
                            mac = Some(m.to_lowercase());
                        }
                    } else if line_trimmed.contains("status: active") {
                        is_active = true;
                    } else if line_trimmed.starts_with("inet ") {
                        has_ip = true;
                    }
                }
                
                if let Some(m) = mac {
                    if m != "00:00:00:00:00:00" {
                        let is_ignored = name.starts_with("anpi") 
                            || name.starts_with("lo") 
                            || name.starts_with("utun") 
                            || name.starts_with("awdl") 
                            || name.starts_with("llw") 
                            || name.starts_with("bridge") 
                            || name.starts_with("gif") 
                            || name.starts_with("stf")
                            || name.starts_with("ap");
                        
                        candidates.push((name, m, is_active, has_ip, is_ignored));
                    }
                }
            }
            
            // Find the best candidate
            // Rank 1: not ignored, active or has IP
            if let Some((_, m, _, _, _)) = candidates.iter().find(|(_, _, active, ip, ignored)| !*ignored && (*active || *ip)) {
                return m.clone();
            }
            
            // Rank 2: not ignored, regardless of active/IP status
            if let Some((_, m, _, _, _)) = candidates.iter().find(|(_, _, _, _, ignored)| !*ignored) {
                return m.clone();
            }
            
            // Rank 3: any candidate (even if ignored like awdl)
            if let Some((_, m, _, _, _)) = candidates.first() {
                return m.clone();
            }
        }
        "00:11:22:33:44:55".to_string()
    }

    #[cfg(not(target_os = "macos"))]
    {
        // On Linux, parse active network interface
        if let Ok(output) = Command::new("ip").args(&["addr", "show"]).output() {
            let output_str = String::from_utf8_lossy(&output.stdout);
            
            let mut blocks = Vec::new();
            let mut current_block = Vec::new();
            
            for line in output_str.lines() {
                if !line.starts_with(' ') && !line.is_empty() {
                    if !current_block.is_empty() {
                        blocks.push(current_block);
                        current_block = Vec::new();
                    }
                }
                current_block.push(line);
            }
            if !current_block.is_empty() {
                blocks.push(current_block);
            }
            
            let mut candidates = Vec::new();
            for block in blocks {
                if block.is_empty() {
                    continue;
                }
                let first_line = block[0];
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                if parts.len() < 2 { continue; }
                let name = parts[1].trim_end_matches(':').to_string();
                
                let mut mac = None;
                let mut has_ip = false;
                let mut is_up = first_line.contains("state UP") || first_line.contains("LOWER_UP");
                
                for line in block.iter().skip(1) {
                    let line_trimmed = line.trim();
                    if line_trimmed.starts_with("link/ether ") {
                        if let Some(m) = line_trimmed.split_whitespace().nth(1) {
                            mac = Some(m.to_lowercase());
                        }
                    } else if line_trimmed.starts_with("inet ") {
                        has_ip = true;
                    }
                }
                
                if let Some(m) = mac {
                    if m != "00:00:00:00:00:00" {
                        let is_ignored = name.starts_with("lo") 
                            || name.starts_with("docker") 
                            || name.starts_with("veth") 
                            || name.starts_with("br-") 
                            || name.starts_with("virbr")
                            || name.starts_with("tun")
                            || name.starts_with("tap");
                            
                        candidates.push((name, m, is_up, has_ip, is_ignored));
                    }
                }
            }
            
            // Find the best candidate
            if let Some((_, m, _, _, _)) = candidates.iter().find(|(_, _, up, ip, ignored)| !*ignored && (*up || *ip)) {
                return m.clone();
            }
            if let Some((_, m, _, _, _)) = candidates.iter().find(|(_, _, _, _, ignored)| !*ignored) {
                return m.clone();
            }
            if let Some((_, m, _, _, _)) = candidates.first() {
                return m.clone();
            }
        }
        "00:11:22:33:44:55".to_string()
    }
}

pub fn get_airplay_name() -> String {
    "RustyPlay".to_string()
}


/// Get the system MAC address for RAOP service name (uppercase hex, no colons)
fn get_mac_address_raop() -> String {
    let mac = get_mac_address();
    mac.replace(":", "").to_uppercase()
}

/// Build DNS-SD TXT record bytes from key=value pairs.
///
/// TXT records are length-prefixed: each entry is [len][key=value]
#[cfg(target_os = "macos")]
fn build_txt_record(entries: &[&str]) -> Vec<u8> {
    let mut txt = Vec::new();
    for entry in entries {
        let bytes = entry.as_bytes();
        if bytes.len() <= 255 {
            txt.push(bytes.len() as u8);
            txt.extend_from_slice(bytes);
        }
    }
    txt
}

#[cfg(target_os = "macos")]
pub async fn start_mdns(http_running: bool, rtsp_running: bool, http_port: u16, rtsp_port: u16) -> Result<()> {
    info!("Starting mDNS service discovery using native Bonjour (macOS)");

    // Get system hostname and MAC address
    let _hostname = get_hostname();
    let mac_address = get_mac_address();
    let mac_address_raop = get_mac_address_raop();
    let raop_name = format!("{}@RustyPlay", mac_address_raop);
    let airplay_name = "RustyPlay".to_string();

    info!(raop_name = %raop_name, airplay_name = %airplay_name, mac_address = %mac_address, "Using system identity");

    let raop_name_cstr = std::ffi::CString::new(raop_name.clone()).unwrap();
    let airplay_name_cstr = std::ffi::CString::new(airplay_name.clone()).unwrap();
    let mut airplay_ref = std::ptr::null_mut();
    let mut raop_ref = std::ptr::null_mut();

    // Only register _airplay._tcp if HTTP server is running
    if http_running {
        let airplay_txt = build_txt_record(&[
            &format!("deviceid={}", mac_address),
            "features=0x527FFEE6,0x0",
            "model=AppleTV3,2",
            "flags=0x84",
            "pw=false",
            "pk=b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7",
            "pi=2e388006-13ba-4041-9a67-25dd4a43d536",
            "srcvers=220.68",
            "vv=2",
        ]);

        let airplay_result = unsafe {
            DNSServiceRegister(
                &mut airplay_ref,
                0,
                0,
                airplay_name_cstr.as_ptr(),
                c"_airplay._tcp".as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                http_port.to_be(),
                airplay_txt.len() as u16,
                airplay_txt.as_ptr() as *const std::ffi::c_void,
                None,
                std::ptr::null_mut(),
            )
        };

        if airplay_result != 0 {
            tracing::error!(error = airplay_result, "Failed to register _airplay._tcp service");
        } else {
            info!(name = %airplay_name, port = http_port, "Registered _airplay._tcp service via Bonjour");
        }
    }

    // Only register _raop._tcp if RTSP server is running
    if rtsp_running {
        let raop_txt = build_txt_record(&[
            "ch=2",
            "cn=0,1,2,3",
            "da=true",
            "et=0,3,5",
            "vv=2",
            "ft=0x527FFEE6,0x0",
            "am=AppleTV3,2",
            "md=0,1,2",
            "rhd=5.6.0.0",
            "pw=false",
            "sf=0x4",
            "sr=44100",
            "ss=16",
            "sv=false",
            "tp=UDP",
            "txtvers=1",
            "vs=220.68",
            "vn=65537",
            "pk=b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7",
        ]);

        let raop_result = unsafe {
            DNSServiceRegister(
                &mut raop_ref,
                0,
                0,
                raop_name_cstr.as_ptr(),
                c"_raop._tcp".as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                rtsp_port.to_be(),
                raop_txt.len() as u16,
                raop_txt.as_ptr() as *const std::ffi::c_void,
                None,
                std::ptr::null_mut(),
            )
        };

        if raop_result != 0 {
            tracing::error!(error = raop_result, "Failed to register _raop._tcp service");
        } else {
            info!(name = %raop_name, port = rtsp_port, "Registered _raop._tcp service via Bonjour");
        }
    }

    if http_running || rtsp_running {
        info!("mDNS services registered via native Bonjour");
    }

    // Box the raw pointers so we can properly prevent deallocation.
    // The service refs must stay alive for the registration to persist.
    // We intentionally leak them since mDNS must stay registered for
    // the lifetime of the process.
    if http_running {
        let _airplay_guard = Box::new(airplay_ref);
        std::mem::forget(_airplay_guard);
    }
    if rtsp_running {
        let _raop_guard = Box::new(raop_ref);
        std::mem::forget(_raop_guard);
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub async fn start_mdns(http_running: bool, rtsp_running: bool, http_port: u16, rtsp_port: u16) -> Result<()> {
    info!("Starting mDNS service discovery using libmdns");

    // Get system hostname and MAC address
    let _hostname = get_hostname();
    let mac_address = get_mac_address();
    let mac_address_raop = get_mac_address_raop();
    let raop_name = format!("{}@RustyPlay", mac_address_raop);
    let airplay_name = "RustyPlay".to_string();

    info!(raop_name = %raop_name, airplay_name = %airplay_name, mac_address = %mac_address, "Using system identity");

    let handle = tokio::runtime::Handle::current();
    let responder = Responder::spawn(&handle)?;

    let mut _airplay_svc = None;
    let mut _raop_svc = None;

    // Only register _airplay._tcp if HTTP server is running
    if http_running {
        let txt_airplay: Vec<String> = vec![
            "txtvers=1".to_string(),
            format!("deviceid={}", mac_address),
            "features=0x527FFEE6,0x0".to_string(),
            "model=AppleTV3,2".to_string(),
            "flags=0x84".to_string(),
            "pw=false".to_string(),
            "pk=b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7".to_string(),
            "pi=2e388006-13ba-4041-9a67-25dd4a43d536".to_string(),
            "srcvers=220.68".to_string(),
            "vv=2".to_string(),
        ];

        _airplay_svc = Some(responder.register(
            "_airplay._tcp".to_string(),
            airplay_name.clone(),
            http_port,
            &txt_airplay.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ));
    }

    // Only register _raop._tcp if RTSP server is running
    if rtsp_running {
        let txt_raop: Vec<String> = vec![
            "txtvers=1".to_string(),
            "ch=2".to_string(),
            "cn=0,1,2,3".to_string(),
            "et=0,3,5".to_string(),
            "vv=2".to_string(),
            "ft=0x527FFEE6,0x0".to_string(),
            "am=AppleTV3,2".to_string(),
            "md=0,1,2".to_string(),
            "rhd=5.6.0.0".to_string(),
            "pw=false".to_string(),
            "sf=0x4".to_string(),
            "sr=44100".to_string(),
            "ss=16".to_string(),
            "sv=false".to_string(),
            "tp=UDP".to_string(),
            "txtvers=1".to_string(),
            "sf=0x4".to_string(),
            "vs=220.68".to_string(),
            "vn=65537".to_string(),
            "pk=b07727d6f6cd6e08b58ede525ec3cdeaa252ad9f683feb212ef8a205246554e7".to_string(),
        ];

        _raop_svc = Some(responder.register(
            "_raop._tcp".to_string(),
            raop_name.clone(),
            rtsp_port,
            &txt_raop.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ));
    }

    if http_running || rtsp_running {
        info!("mDNS services registered");
    }

    // Keep the responder and service handles alive for the process lifetime
    std::mem::forget(responder);
    if http_running {
        std::mem::forget(_airplay_svc);
    }
    if rtsp_running {
        std::mem::forget(_raop_svc);
    }

    Ok(())
}
