mod blur;
mod config;
mod feedback;
mod font;
mod images;
mod picker;
mod theme;
mod thumbnails;
mod wallpaper;

use config::Config;
use feedback::{notify, play_sound};
use std::process::exit;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
walldeck — a Wayland layer-shell wallpaper picker

USAGE:
    walldeck [OPTIONS]

OPTIONS:
    -h, --help         Print this help and exit
    -V, --version      Print version and exit
        --config-path  Print the config file path being used and exit

ENVIRONMENT:
    WALLDECK_CONFIG    Override the config file path
";

fn main() {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                exit(0);
            }
            "-V" | "--version" => {
                println!("walldeck {VERSION}");
                exit(0);
            }
            "--config-path" => {
                match Config::path() {
                    Some(p) => println!("{}", p.display()),
                    None => println!("(could not determine a config path)"),
                }
                exit(0);
            }
            other => {
                eprintln!("walldeck: unrecognized argument '{other}'\n");
                print!("{USAGE}");
                exit(2);
            }
        }
    }

    let cfg = Config::load();

    if !wallpaper::is_on_path(&cfg.command) {
        eprintln!(
            "walldeck: backend '{}' not found on PATH (set `command` in the config \
             to point at your wallpaper daemon's CLI)",
            cfg.command
        );
        exit(1);
    }

    if !cfg.wallpaper_dir.is_dir() {
        eprintln!(
            "walldeck: wallpaper directory '{}' does not exist (set `wallpaper_dir` in the config)",
            cfg.wallpaper_dir.display()
        );
        exit(1);
    }

    let all_images = images::list_images(&cfg).unwrap_or_else(|e| {
        eprintln!(
            "walldeck: could not read '{}': {e}",
            cfg.wallpaper_dir.display()
        );
        exit(1);
    });
    if all_images.is_empty() {
        eprintln!(
            "walldeck: no images in {} (supported: {})",
            cfg.wallpaper_dir.display(),
            config::IMAGE_EXTS.join(", ")
        );
        exit(1);
    }

    let originals = images::filter_originals(all_images, &cfg.secondary.suffix);
    if originals.is_empty() {
        let msg = format!(
            "Every image in {} looks like a '{}' variant",
            cfg.wallpaper_dir.display(),
            cfg.secondary.suffix
        );
        eprintln!("walldeck: {msg}");
        notify(&cfg, "walldeck: no wallpapers", &msg);
        exit(1);
    }

    if let Err(e) = thumbnails::ensure_thumbnails(&cfg, &originals) {
        eprintln!("walldeck: could not build the thumbnail cache: {e}");
        exit(1);
    }
    if cfg.secondary.enabled && cfg.secondary.auto_blur {
        blur::prune_stale(&cfg, &originals);
    }

    // Cancelling (Escape / close) exits quietly without touching anything.
    let Some(selected) = picker::pick(&cfg, &originals) else {
        exit(0);
    };

    apply_selection(&cfg, &selected);
}

/// Apply the picked wallpaper to the primary target, then to the optional
/// secondary one. The primary is fatal on failure; the secondary is only
/// fatal when explicitly marked `secondary_required`, so running a single
/// daemon (with no second namespace) is a supported setup rather than an
/// error on every pick.
fn apply_selection(cfg: &Config, selected: &str) {
    let primary_path = images::full_path(cfg, selected);
    if let Err(e) = wallpaper::apply(cfg, cfg.namespace.as_deref(), &cfg.opts, &primary_path) {
        let msg = format!("Could not apply {selected}: {e}");
        eprintln!("walldeck: {msg}");
        notify(cfg, "walldeck: wallpaper failed", &msg);
        exit(1);
    }

    let mut applied = vec![selected.to_string()];

    if cfg.secondary.enabled {
        match apply_secondary(cfg, selected) {
            Ok(Some(name)) => applied.push(name),
            Ok(None) => {}
            Err(msg) => {
                eprintln!("walldeck: {msg}");
                if cfg.secondary.required {
                    notify(cfg, "walldeck: wallpaper failed", &msg);
                    exit(1);
                }
                notify(cfg, "walldeck: secondary target skipped", &msg);
            }
        }
    }

    notify(cfg, "Wallpaper changed", &applied.join("\n"));
    play_sound(cfg);
}

/// `Ok(Some(name))` applied, `Ok(None)` nothing to do, `Err(msg)` failed.
fn apply_secondary(cfg: &Config, selected: &str) -> Result<Option<String>, String> {
    let name = images::variant_name(selected, &cfg.secondary.suffix);
    let real_path = images::full_path(cfg, &name);

    let path = if real_path.is_file() {
        real_path
    } else if cfg.secondary.auto_blur {
        let primary_path = images::full_path(cfg, selected);
        match blur::get_or_generate(cfg, &primary_path, &name) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!(
                    "could not auto-generate a blurred '{}' variant for {selected}: {e}",
                    cfg.secondary.suffix
                );
                return if cfg.secondary.required {
                    Err(msg)
                } else {
                    eprintln!("walldeck: {msg}, leaving that target unchanged");
                    Ok(None)
                };
            }
        }
    } else {
        let msg = format!("no '{}' variant for {selected} ({name} not found)", cfg.secondary.suffix);
        // Not having a variant for every wallpaper is a normal, quiet state
        // unless the user has declared the secondary target mandatory.
        return if cfg.secondary.required {
            Err(msg)
        } else {
            eprintln!("walldeck: {msg}, leaving that target unchanged");
            Ok(None)
        };
    };

    let ns = cfg.secondary.namespace.as_deref();
    match wallpaper::apply(cfg, ns, &cfg.secondary.opts, &path) {
        Ok(()) => Ok(Some(name)),
        Err(e) => {
            let where_ = match ns {
                Some(ns) => format!("namespace '{ns}'"),
                None => "the default namespace".to_string(),
            };
            Err(format!(
                "could not apply {name} to {where_}: {e} \
                 (is a daemon running for it? set `secondary_enabled = false` if you only run one)"
            ))
        }
    }
}
