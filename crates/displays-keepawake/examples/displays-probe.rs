// Proves the feature's own OS code without the app:
// `cargo run -p notch-feat-displays-keepawake --example displays-probe`.
// Lists the displays with their kind and brightness method, reads the current
// brightness where it can be read, reads the Bluetooth and Wi-Fi radios, and
// holds a keep-awake request for two seconds before releasing it.
// Nothing else is changed: no brightness is set, no display is turned off, no
// wallpaper is set and no radio is switched.

use std::time::Duration;

use notch_feat_displays_keepawake::{display, power, radio, wall};

fn main() {
    let displays = display::list();
    println!("displays: {}", displays.len());
    for d in &displays {
        let (method, level) = display::probe(d);
        println!(
            "  {} | {} | {} | {}x{} at {},{} | method {} | brightness {} | contrast {}",
            d.id,
            d.name,
            if d.internal { "built-in" } else { "external" },
            d.w,
            d.h,
            d.x,
            d.y,
            method.name(),
            level.map_or("not readable".to_string(), |p| format!("{p}%")),
            display::read_contrast(d).map_or("not readable".to_string(), |p| format!("{p}%")),
        );
    }

    // The display under the pointer, and the keys a held shortcut would watch.
    let mut at = windows::Win32::Foundation::POINT::default();
    if unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut at) }.is_ok() {
        println!("pointer at {},{} -> display {:?}", at.x, at.y, display::at_point(at.x, at.y).map(|d| d.name));
    }
    for keys in ["Ctrl+Alt+PageUp", "Ctrl+Shift+F5", "Win+B", "Ctrl+Alt+Equal", "PageUp"] {
        println!("hold keys {keys}: {:?}", notch_feat_displays_keepawake::vks_of(keys));
    }

    for (name, kind) in [("bluetooth", radio::Kind::Bluetooth), ("wifi", radio::Kind::Wifi)] {
        match radio::states(kind) {
            Ok(on) if on.is_empty() => println!("{name}: no radio"),
            Ok(on) => println!("{name}: {} radio(s), on = {on:?}", on.len()),
            Err(e) => println!("{name}: cannot read ({e})"),
        }
    }

    let (sys, own) = wall::list(&wall::own_dir(""));
    println!("wallpapers: {} from Windows, {} from {}", sys.len(), own.len(), wall::own_dir("").display());

    match power::Request::new(false) {
        Ok(request) => {
            println!("keep awake: request held (system + display + execution)");
            let seen = std::process::Command::new("powercfg").arg("/requests").output();
            match seen {
                Ok(o) if o.status.success() => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    println!("  powercfg lists it: {}", text.contains("Keep awake"));
                }
                _ => println!("  powercfg /requests needs administrator rights here; not checked"),
            }
            std::thread::sleep(Duration::from_secs(2));
            drop(request);
            println!("keep awake: request released");
            if let Ok(o) = std::process::Command::new("powercfg").arg("/requests").output() {
                if o.status.success() {
                    println!("  powercfg lists it after release: {}", String::from_utf8_lossy(&o.stdout).contains("Keep awake"));
                }
            }
        }
        Err(e) => println!("keep awake: failed ({e})"),
    }
}
