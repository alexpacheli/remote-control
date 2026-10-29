//! Sealing the key with the operating system, where it can: DPAPI on
//! Windows, so the file opens only for this Windows user (on this computer,
//! or wherever their roaming profile goes). A copy of the file alone, from
//! a backup or another account, is useless.
//!
//! Not the macOS Keychain yet: a Keychain item is tied to the app's code
//! signature, and until the app is signed with a Developer ID every update
//! would ask the user to allow access again. There, as on Linux, the file
//! stays readable only by the user (and FileVault encrypts the disk).

/// Whether keys are sealed here.
pub const AVAILABLE: bool = cfg!(windows);

#[cfg(windows)]
pub use dpapi::{seal, unseal};

#[cfg(not(windows))]
pub fn seal(_data: &[u8]) -> std::io::Result<Vec<u8>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(not(windows))]
pub fn unseal(_sealed: &[u8]) -> std::io::Result<zeroize::Zeroizing<Vec<u8>>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(windows)]
#[allow(unsafe_code)] // plain Win32 calls
mod dpapi {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;
    use zeroize::{Zeroize, Zeroizing};

    /// Keeps the sealed key from opening with a call that names nothing
    /// (it is no secret: a program running as the same user can still
    /// open it, as with any user-level protection).
    const ENTROPY: &[u8] = b"hext-remote device key";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr().cast_mut(),
        }
    }

    /// Copies what Windows returned, then wipes and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: Windows allocated `cbData` bytes at `pbData` for us.
        let bytes = unsafe { std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize) };
        let copy = bytes.to_vec();
        bytes.zeroize();
        // SAFETY: allocated with LocalAlloc by the call that returned it.
        unsafe { LocalFree(Some(HLOCAL(out.pbData.cast()))) };
        copy
    }

    pub fn seal(data: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        let entropy = blob(ENTROPY);
        // SAFETY: the input blobs point to live slices; Windows fills `out`.
        unsafe {
            CryptProtectData(
                &blob(data),
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
        .map_err(std::io::Error::other)?;
        Ok(take(out))
    }

    pub fn unseal(sealed: &[u8]) -> std::io::Result<Zeroizing<Vec<u8>>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        let entropy = blob(ENTROPY);
        // SAFETY: as in `seal`.
        unsafe {
            CryptUnprotectData(
                &blob(sealed),
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
        .map_err(std::io::Error::other)?;
        Ok(Zeroizing::new(take(out)))
    }
}
