//! Persistent Secure Enclave P-256 signing, backed by CryptoKit.
//!
//! The private key is generated inside the Secure Enclave and never leaves it;
//! `key_path` holds only its opaque, device-bound data representation.

use std::path::Path;

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::path::Path;

    extern "C" {
        fn iso_enclave_available() -> i32;
        fn iso_enclave_sign(
            key_path: *const c_char,
            data: *const u8,
            data_len: isize,
            out: *mut u8,
            out_cap: isize,
        ) -> i32;
        fn iso_enclave_verify(
            key_path: *const c_char,
            data: *const u8,
            data_len: isize,
            sig: *const u8,
            sig_len: isize,
        ) -> i32;
    }

    fn key_c(key_path: &Path) -> Option<CString> {
        CString::new(key_path.to_string_lossy().as_bytes().to_vec()).ok()
    }

    pub fn available() -> bool {
        unsafe { iso_enclave_available() == 1 }
    }

    pub fn sign(key_path: &Path, body: &[u8]) -> Option<Vec<u8>> {
        let key = key_c(key_path)?;
        let mut out = vec![0u8; 256];
        let written = unsafe {
            iso_enclave_sign(
                key.as_ptr(),
                body.as_ptr(),
                body.len() as isize,
                out.as_mut_ptr(),
                out.len() as isize,
            )
        };
        if written <= 0 {
            return None;
        }
        out.truncate(written as usize);
        Some(out)
    }

    pub fn verify(key_path: &Path, body: &[u8], signature: &[u8]) -> bool {
        let Some(key) = key_c(key_path) else { return false };
        unsafe {
            iso_enclave_verify(
                key.as_ptr(),
                body.as_ptr(),
                body.len() as isize,
                signature.as_ptr(),
                signature.len() as isize,
            ) == 1
        }
    }
}

/// True when this machine has a usable Secure Enclave.
pub fn available() -> bool {
    #[cfg(target_os = "macos")]
    {
        native::available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Sign `body` with the enclave key at `key_path`, creating the key on first use.
/// Returns the raw P-256 signature, or `None` if the enclave is unavailable.
pub fn sign(key_path: &Path, body: &[u8]) -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        native::sign(key_path, body)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (key_path, body);
        None
    }
}

/// Verify `signature` over `body` with the enclave key at `key_path`.
pub fn verify(key_path: &Path, body: &[u8], signature: &[u8]) -> bool {
    #[cfg(target_os = "macos")]
    {
        native::verify(key_path, body, signature)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (key_path, body, signature);
        false
    }
}
