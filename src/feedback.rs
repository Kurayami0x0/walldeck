//! Desktop notifications and the completion sound.
//!
//! Both are best-effort and entirely optional: a missing `notify-send`, a
//! missing player, or a missing sound file are all silently ignored rather
//! than treated as errors. Changing the wallpaper should never fail because
//! the chime didn't play.

use crate::config::Config;
use std::process::{Command, Stdio};

pub fn notify(cfg: &Config, summary: &str, body: &str) {
    if !cfg.notifications {
        return;
    }
    let _ = Command::new("notify-send")
        .args(["-u", "low", "-a", "walldeck", summary, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn play_sound(cfg: &Config) {
    let Some(sound) = cfg.sound_path() else {
        return;
    };
    let _ = Command::new(&cfg.sound_command)
        .arg(sound)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
