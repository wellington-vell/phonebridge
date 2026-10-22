//! The phone's IPv4 addresses, to show for Wi-Fi mode.

use std::ffi::CStr;
use std::net::Ipv4Addr;

/// Non-loopback IPv4 addresses as "interface address" pairs, Wi-Fi first.
pub fn addresses() -> Vec<(String, String)> {
    let mut list = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a linked list we free below; entries are only
    // read while the list is alive.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return list;
        }
        let mut cur = ifap;
        while let Some(ifa) = cur.as_ref() {
            cur = ifa.ifa_next;
            let addr = ifa.ifa_addr;
            if addr.is_null() || (*addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let sin = &*(addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if ip.is_loopback() {
                continue;
            }
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            list.push((name, ip.to_string()));
        }
        libc::freeifaddrs(ifap);
    }
    list.sort_by_key(|(name, _)| !name.starts_with("wlan"));
    list
}
