use super::*;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ovfx-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&d);
    d
}

#[test]
fn hide_and_restore_roundtrip() {
    let marker = tmp("roundtrip");
    let be = DryRunFx::default();
    let log = be.log.clone();
    let mut fx = DesktopFx::new(Box::new(be), marker.clone(), Some(0));
    fx.set_wallpaper_hidden(true).unwrap();
    fx.set_animations_off(true).unwrap();
    // Originals are on disk while effects are active.
    let saved = Original::from_text(&std::fs::read_to_string(&marker).unwrap()).unwrap();
    assert_eq!(
        saved.wallpaper.as_deref(),
        Some(r"C:\Users\me\Pictures\wall.jpg")
    );
    assert_eq!(saved.desktop_color, Some(0x0080_4000));
    assert_eq!(saved.client_animation, Some(true));
    assert_eq!(saved.window_animation, Some(true));
    drop(fx);
    assert!(!marker.exists(), "marker removed after restore");
    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "set desktop color 0x000000".to_string(),
            "set wallpaper ``".into(),
            "set client-area animation false".into(),
            "set window animation false".into(),
            r"set wallpaper `C:\Users\me\Pictures\wall.jpg`".into(),
            "set desktop color 0x804000".into(),
            "set client-area animation true".into(),
            "set window animation true".into(),
        ]
    );
}

#[test]
fn toggles_are_idempotent_and_partial() {
    let marker = tmp("partial");
    let be = DryRunFx {
        client_anim: false,
        ..Default::default()
    };
    let log = be.log.clone();
    let mut fx = DesktopFx::new(Box::new(be), marker.clone(), None);
    fx.set_animations_off(true).unwrap();
    fx.set_animations_off(true).unwrap();
    fx.set_animations_off(false).unwrap();
    assert!(!marker.exists());
    fx.set_wallpaper_hidden(true).unwrap();
    fx.set_wallpaper_hidden(false).unwrap();
    // The client animation was already off: it is restored to off.
    let log = log.lock().unwrap().clone();
    assert!(log.contains(&"set client-area animation false".to_string()));
    assert!(!log.contains(&"set client-area animation true".to_string()));
    assert!(
        !log.iter().any(|l| l.contains("desktop color")),
        "no flat colour requested"
    );
}

#[test]
fn stale_marker_is_restored_on_next_start() {
    let marker = tmp("stale");
    let orig = Original {
        wallpaper: Some(r"D:\w p\a=b.png".into()),
        desktop_color: None,
        client_animation: Some(true),
        window_animation: Some(false),
    };
    std::fs::write(&marker, orig.to_text()).unwrap();
    let mut be = DryRunFx {
        wallpaper: String::new(),
        client_anim: false,
        ..Default::default()
    };
    assert!(DesktopFx::recover_stale(&mut be, &marker));
    assert_eq!(be.wallpaper, r"D:\w p\a=b.png");
    assert!(be.client_anim);
    assert!(!be.window_anim);
    assert!(!marker.exists());
    assert!(!DesktopFx::recover_stale(&mut be, &marker));
}

#[test]
fn rgb_parse() {
    assert_eq!(parse_rgb("102030"), Some(0x0030_2010));
    assert_eq!(parse_rgb("#000000"), Some(0));
    assert_eq!(parse_rgb("xyz"), None);
}
