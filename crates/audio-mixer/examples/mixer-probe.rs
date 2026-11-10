// Proves the mixer's OS code without the app:
// `cargo run -p notch-feat-audio-mixer --example mixer-probe`. Lists the audio
// sessions on the default output and the rows the mixer makes of every
// session, then prints session and device changes for about 20 seconds.
// Observes only: it never sets a level, a mute or a default device.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use notch_feat_audio_mixer::apps::{self, Known};
use svc_audio::{Flow, Role};

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn rows(known: &mut Known) -> Vec<String> {
    let sessions = svc_audio::sessions(None).unwrap_or_default();
    apps::group(&sessions, known)
        .iter()
        .map(|a| {
            let icon = !a.exe.is_empty() && svc_shell::icon(&a.exe, 32).is_ok();
            let name = if a.system { "System sounds" } else { &a.name };
            format!(
                "{name:<28} id {:<24} {:>3}%{}{} icon {} pids {:?}",
                a.id,
                (a.volume * 100.0).round(),
                if a.mute { " muted" } else { "" },
                if a.playing { " playing" } else { "" },
                if icon { "found" } else { "none" },
                a.pids,
            )
        })
        .collect()
}

/// The automatic priority over a made-up device list (this PC has one output).
fn priority_proof() {
    let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let p = v(&["dock", "bt", "spk"]);
    let cases: [(&[&str], Option<&str>, Option<&str>, &str); 6] = [
        (&["spk", "bt"], Some("spk"), Some("bt"), "bt plugged in above speakers -> switch to bt"),
        (&["spk", "bt"], Some("bt"), None, "bt already the default -> stay"),
        (&["spk", "bt", "dock"], Some("bt"), Some("dock"), "dock above bt appears -> switch"),
        (&["spk", "bt"], None, Some("bt"), "no default -> highest present"),
        (&["spk"], Some("hdmi"), Some("spk"), "current (unlisted, gone) -> highest left"),
        (&["hdmi"], Some("hdmi"), None, "nothing listed present -> stay"),
    ];
    for (present, cur, want, what) in cases {
        let got = notch_feat_audio_mixer::auto_target(&p, &v(present), cur);
        println!("  {} {what}", if got.as_deref() == want { "ok  " } else { "FAIL" });
    }
}

fn main() {
    if std::env::args().any(|a| a == "--priority") {
        return priority_proof();
    }
    let outputs = svc_audio::devices(Flow::Render).unwrap_or_default();
    let default = outputs.iter().find(|d| d.default_for.contains(&Role::Console));
    println!("default output: {:?}", default.map(|d| &d.name));
    match svc_audio::sessions(default.map(|d| d.id.as_str())) {
        Ok(list) => {
            println!("{} sessions on it:", list.len());
            for s in &list {
                println!(
                    "  pid {:>6} {:<26} name {:<40} {:>3}%{}{} icon path {:?}",
                    s.pid,
                    if s.system { "(system sounds)" } else { file_name(&s.exe) },
                    format!("{:?}", s.name),
                    (s.volume * 100.0).round(),
                    if s.mute { " muted" } else { "" },
                    if s.active { " playing" } else { "" },
                    s.icon,
                );
            }
        }
        Err(err) => println!("sessions: {err}"),
    }
    let everywhere = svc_audio::sessions(None).map(|l| l.len());
    println!("sessions on all {} outputs: {everywhere:?}", outputs.len());

    // What the pickers and "mute all microphones" start from (read only).
    println!("can set the default device: {}", svc_audio::can_set_default());
    for mic in svc_audio::devices(Flow::Capture).unwrap_or_default() {
        println!("microphone {:?}: {:?}", mic.name, svc_audio::device_volume(&mic.id));
    }

    let known = Mutex::new(Known::default());
    println!("the mixer's rows:");
    rows(&mut known.lock().unwrap()).iter().for_each(|row| println!("  {row}"));

    let start = Instant::now();
    let sessions = svc_audio::on_sessions(svc_audio::Scope::All, move || {
        println!("{:5.1}s sessions changed:", start.elapsed().as_secs_f32());
        rows(&mut known.lock().unwrap()).iter().for_each(|row| println!("        {row}"));
    });
    let devices = svc_audio::on_endpoint_change(move |change| {
        println!("{:5.1}s {change:?}", start.elapsed().as_secs_f32());
    });
    println!("listening for 20 s: start or pause something that plays, move a fader in the Windows mixer, plug in headphones…");
    std::thread::sleep(Duration::from_secs(20));

    drop(sessions);
    drop(devices);
    println!("{:5.1}s unsubscribed: both audio threads are gone", start.elapsed().as_secs_f32());
}
