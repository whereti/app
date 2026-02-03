//! The few requests whereti makes (PLAN.md §40, §55), through Windows' own
//! WinHTTP: its TLS, the user's proxy settings, no cookies. Nothing else.

pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    // SAFETY: buffers and lengths match; the pseudo-handle needs no opening.
    let status = unsafe {
        BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, data.as_ptr(), data.len() as u32, out.as_mut_ptr(), 32)
    };
    if status != 0 {
        return Err(format!("hashing failed ({status:#x})"));
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// HTTPS GET through WinHTTP (Windows' own TLS, proxy settings honoured,
/// cookies off). Follows redirects; never more than `max` bytes.
pub fn http_get(url: &str, agent: &str, max: u64, progress: &mut dyn FnMut(u64, u64)) -> Result<(u16, Vec<u8>), String> {
    use windows_sys::Win32::Networking::WinHttp::*;
    let Some(rest) = url.strip_prefix("https://") else { return Err("only https".into()) };
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
    let (agent, host_w, path_w, get) = (w(agent), w(host), w(&path), w("GET"));
    struct H(*mut core::ffi::c_void);
    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle WinHTTP gave us, closed once.
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }
    let err = |what: &str| format!("{what} failed ({})", std::io::Error::last_os_error());
    // SAFETY: NUL-terminated strings that outlive the handles; every handle
    // is closed by H; buffers match their lengths.
    unsafe {
        let session = H(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0));
        if session.0.is_null() {
            return Err(err("connecting"));
        }
        WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 60_000);
        let connect = H(WinHttpConnect(session.0, host_w.as_ptr(), 443, 0));
        if connect.0.is_null() {
            return Err(err("connecting"));
        }
        let request = H(WinHttpOpenRequest(
            connect.0,
            get.as_ptr(),
            path_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(err("the request"));
        }
        let no_cookies: u32 = WINHTTP_DISABLE_COOKIES;
        WinHttpSetOption(request.0, WINHTTP_OPTION_DISABLE_FEATURE, &no_cookies as *const u32 as *const _, 4);
        if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
        {
            return Err(err("the request"));
        }
        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut status as *mut u32 as *mut _,
            &mut len,
            std::ptr::null_mut(),
        );
        let mut total = 0u64;
        let mut tlen = 8u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER64,
            std::ptr::null(),
            &mut total as *mut u64 as *mut _,
            &mut tlen,
            std::ptr::null_mut(),
        );
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(request.0, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut read) == 0 {
                return Err(err("downloading"));
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() as u64 > max {
                return Err("the answer is too large".into());
            }
            progress(body.len() as u64, total);
        }
        Ok((status as u16, body))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hashes_like_sha256() {
        assert_eq!(super::sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn only_https() {
        assert!(super::http_get("http://example.com/", "test", 10, &mut |_, _| {}).is_err());
    }
}
