//! Win32 helpers: Credential Manager, registry (Run key, app theme),
//! clipboard, shell open.

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HGLOBAL};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD,
    RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CF_UNICODETEXT: u32 = 13;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- credentials

/// Store `secret` under the generic credential `target`.
pub fn cred_write(target: &str, secret: &str) -> Result<(), String> {
    let mut t = wide(target);
    let mut user = wide("OpenView");
    let blob = secret.as_bytes();
    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(t.as_mut_ptr()),
        CredentialBlobSize: u32::try_from(blob.len()).map_err(|_| "secret too long")?,
        CredentialBlob: blob.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: every pointer in `cred` outlives the call.
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| e.message())
}

pub fn cred_read(target: &str) -> Option<String> {
    let t = HSTRING::from(target);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: on success `p` points at a CREDENTIALW we free with CredFree.
    unsafe {
        CredReadW(&t, CRED_TYPE_GENERIC, None, &mut p).ok()?;
        if p.is_null() {
            return None;
        }
        let c = &*p;
        let bytes = if c.CredentialBlob.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        };
        CredFree(p.cast());
        decode_blob(&bytes)
    }
}

/// We write UTF-8; Credential Manager's own UI and `cmdkey` write UTF-16LE.
fn decode_blob(bytes: &[u8]) -> Option<String> {
    if bytes.len() >= 2
        && bytes.len().is_multiple_of(2)
        && bytes.iter().skip(1).step_by(2).all(|&b| b == 0)
    {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        return String::from_utf16(&units).ok();
    }
    String::from_utf8(bytes.to_vec()).ok()
}

pub fn cred_delete(target: &str) {
    let t = HSTRING::from(target);
    // SAFETY: plain call with a valid string.
    let _ = unsafe { CredDeleteW(&t, CRED_TYPE_GENERIC, None) };
}

// ------------------------------------------------------------------- registry

pub fn reg_get_string(subkey: &str, value: &str) -> Option<String> {
    let (k, v) = (HSTRING::from(subkey), HSTRING::from(value));
    let mut buf = vec![0u16; 1024];
    let mut size = u32::try_from(buf.len() * 2).ok()?;
    // SAFETY: buffer and size describe the same allocation.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &k,
            &v,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let n = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}

pub fn reg_get_dword(subkey: &str, value: &str) -> Option<u32> {
    let (k, v) = (HSTRING::from(subkey), HSTRING::from(value));
    let mut out: u32 = 0;
    let mut size = 4u32;
    // SAFETY: `out` is 4 bytes, matching `size`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &k,
            &v,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut out).cast()),
            Some(&mut size),
        )
    };
    (rc == ERROR_SUCCESS).then_some(out)
}

pub fn reg_set_string(subkey: &str, value: &str, data: &str) -> Result<(), String> {
    let (k, v) = (HSTRING::from(subkey), HSTRING::from(value));
    let d = wide(data);
    // SAFETY: `d` is a NUL-terminated UTF-16 buffer of the given byte size.
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &k,
            &v,
            REG_SZ.0,
            Some(d.as_ptr().cast()),
            u32::try_from(d.len() * 2).unwrap_or(0),
        )
    };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("registry write failed ({})", rc.0))
    }
}

pub fn reg_delete_value(subkey: &str, value: &str) {
    let (k, v) = (HSTRING::from(subkey), HSTRING::from(value));
    // SAFETY: plain call with valid strings.
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &k, &v) };
}

/// Windows "app mode": true when apps should be dark.
pub fn system_dark_mode() -> bool {
    reg_get_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    )
    .is_some_and(|v| v == 0)
}

// ------------------------------------------------------------------ clipboard

pub fn set_clipboard(text: &str) -> bool {
    let data = wide(text);
    let bytes = data.len() * 2;
    // SAFETY: standard OpenClipboard / GlobalAlloc / SetClipboardData dance;
    // ownership of the HGLOBAL passes to the system on success.
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| -> Option<()> {
            EmptyClipboard().ok()?;
            let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let p = GlobalLock(h);
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u16>(), data.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).ok()?;
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

// ---------------------------------------------------------------------- shell

pub fn shell_open(path: &str) {
    let p = HSTRING::from(path);
    // SAFETY: plain ShellExecute "open".
    unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &p,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_utf8_and_utf16_blobs() {
        assert_eq!(super::decode_blob(b"openview").as_deref(), Some("openview"));
        let w: Vec<u8> = "pin123".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(super::decode_blob(&w).as_deref(), Some("pin123"));
    }
}
