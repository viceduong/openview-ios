// Shared Windows resource generation for the OpenView executables
// (`include!`d by the build.rs of ovhost, ovview and ovgui).
//
// Writes `<OUT_DIR>/openview.manifest` and `<OUT_DIR>/openview.rc` and
// compiles them into the exe:
//   * the app icon (resource id 1 = APP_ICON_ID in ovview window.rs / the
//     GUI tray);
//   * an application manifest: asInvoker, supportedOS Windows 10/11,
//     PerMonitorV2 DPI awareness (with the `true/pm` fallback), and for the
//     GUI the Common Controls v6 dependency;
//   * a VERSIONINFO block (CompanyName "OpenView contributors", ProductName
//     "OpenView", FileVersion = the crate's CARGO_PKG_VERSION).

/// What one executable embeds.
struct WinRes {
    /// File name of the exe, e.g. "ovhost.exe".
    exe: &'static str,
    /// Explorer's "File description".
    description: &'static str,
    /// GUI apps also depend on Common Controls v6.
    common_controls: bool,
}

fn embed_windows_resources(res: &WinRes) {
    use std::path::PathBuf;
    println!("cargo:rerun-if-changed=../../assets/winres.rs");
    println!("cargo:rerun-if-changed=../../assets/openview.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let icon = manifest_dir.join("../../assets/openview.ico");
    let icon = icon.canonicalize().unwrap_or(icon);
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let nums = version_quad(&version);

    let common_controls = if res.common_controls {
        r#"
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
                        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>"#
    } else {
        ""
    };
    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 and Windows 11 -->
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>{common_controls}
</assembly>
"#
    );
    let manifest_path = out.join("openview.manifest");
    std::fs::write(&manifest_path, manifest).expect("write manifest");

    let rc_str = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let (a, b, c, d) = nums;
    let rc = format!(
        r#"1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},{d}
PRODUCTVERSION {a},{b},{c},{d}
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "OpenView contributors"
      VALUE "FileDescription", "{desc}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{exe}"
      VALUE "LegalCopyright", "Copyright (c) OpenView contributors. MIT License."
      VALUE "OriginalFilename", "{exe}"
      VALUE "ProductName", "OpenView"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = rc_str(&icon),
        manifest = rc_str(&manifest_path),
        desc = res.description,
        exe = res.exe,
    );
    let rc_path = out.join("openview.rc");
    std::fs::write(&rc_path, rc).expect("write .rc");
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_required()
        .expect("embed Windows resources");
}

/// "1.2.3-beta.1" -> (1, 2, 3, 0).
fn version_quad(v: &str) -> (u16, u16, u16, u16) {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}
