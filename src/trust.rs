#[cfg(target_os = "macos")]
mod platform {
    use core_foundation::base::TCFType;
    use core_foundation::data::CFData;
    use security_framework::os::macos::code_signing::{Flags, GuestAttributes, SecCode, SecRequirement};
    use std::os::unix::io::RawFd;

    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERTOKEN: libc::c_int = 6;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecCodeCopyDesignatedRequirement(
            code: core_foundation::base::CFTypeRef,
            flags: u32,
            requirement: *mut core_foundation::base::CFTypeRef,
        ) -> i32;
    }

    fn self_requirement() -> Option<SecRequirement> {
        let own = SecCode::for_self(Flags::NONE).ok()?;
        let mut requirement: core_foundation::base::CFTypeRef = std::ptr::null();
        let status = unsafe { SecCodeCopyDesignatedRequirement(own.as_CFTypeRef(), 0, &mut requirement) };
        if status != 0 || requirement.is_null() {
            return None;
        }
        Some(unsafe { SecRequirement::wrap_under_create_rule(requirement as _) })
    }

    fn peer_audit_token(fd: RawFd) -> Option<[u8; 32]> {
        let mut token = [0u8; 32];
        let mut len = std::mem::size_of::<[u8; 32]>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, token.as_mut_ptr().cast(), &mut len)
        };
        (rc == 0 && len as usize == token.len()).then_some(token)
    }

    fn peer_code(fd: RawFd) -> Option<SecCode> {
        let token = peer_audit_token(fd)?;
        let data = CFData::from_buffer(&token);
        let mut attrs = GuestAttributes::new();
        attrs.set_audit_token(data.as_concrete_TypeRef());
        SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE).ok()
    }

    fn no_requirement_ok() -> bool {
        #[cfg(feature = "strict-signature")]
        {
            eprintln!("isohypse: refusing peer — no code-signing identity and strict-signature is enabled");
            false
        }
        #[cfg(not(feature = "strict-signature"))]
        {
            use std::sync::Once;
            static WARN: Once = Once::new();
            WARN.call_once(|| {
                eprintln!("isohypse: WARNING — this binary has no code-signing identity; peer signature enforcement is DISABLED and uid is the only trust boundary. Ship a signed build (or rebuild with --features strict-signature) for same-uid impostor protection.");
            });
            true
        }
    }

    pub fn peer_signature_ok(fd: RawFd) -> bool {
        let Some(requirement) = self_requirement() else {
            return no_requirement_ok();
        };
        let Some(code) = peer_code(fd) else {
            return false;
        };
        code.check_validity(Flags::NONE, &requirement).is_ok()
    }
}

#[cfg(target_os = "macos")]
pub use platform::peer_signature_ok;

#[cfg(not(target_os = "macos"))]
pub fn peer_signature_ok(_fd: std::os::unix::io::RawFd) -> bool {
    #[cfg(feature = "strict-signature")]
    {
        eprintln!("isohypse: refusing peer — signature verification is unavailable on this platform and strict-signature is enabled");
        false
    }
    #[cfg(not(feature = "strict-signature"))]
    {
        use std::sync::Once;
        static WARN: Once = Once::new();
        WARN.call_once(|| {
            eprintln!("isohypse: WARNING — peer signature enforcement is unavailable on this platform; uid is the only trust boundary.");
        });
        true
    }
}
