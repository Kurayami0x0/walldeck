//! On-the-fly blurred variant generation for the secondary target.
//!
//! When a wallpaper has no matching `-b` (or whatever `secondary_suffix` is
//! set to) variant on disk, and `secondary_auto_blur` is enabled, a blurred
//! copy is generated here instead of leaving that target unset. Generated
//! copies are cached under `cache_dir/blurred`, keyed by the variant
//! filename, and regenerated only when the source wallpaper changes -- the
//! same freshness policy `thumbnails.rs` uses for thumbnails. A real variant
//! file always wins: `apply_secondary` in `main.rs` only reaches this module
//! when one isn't on disk, and `prune_stale` below removes a generated copy
//! the moment a real one appears.

use crate::config::Config;
use std::fs;
use std::path::{Path, PathBuf};

fn blurred_dir(cfg: &Config) -> PathBuf {
    cfg.cache_dir.join("blurred")
}

fn needs_regen(src: &Path, cached: &Path) -> bool {
    let src_mtime = fs::metadata(src).and_then(|m| m.modified()).ok();
    let cached_mtime = fs::metadata(cached).and_then(|m| m.modified());
    match (src_mtime, cached_mtime) {
        (Some(s), Ok(c)) => s > c,
        _ => true, // no cached copy yet, or couldn't stat -> (re)generate
    }
}

/// Return the path to a blurred copy of `src`, generating (or regenerating,
/// if the source changed since) and caching it first if needed. `variant_name`
/// is the filename a real variant would have -- used only as the cache key,
/// so a later real file with that name and this generated one never collide
/// in confusing ways.
pub fn get_or_generate(cfg: &Config, src: &Path, variant_name: &str) -> Result<PathBuf, String> {
    let dir = blurred_dir(cfg);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let cached = dir.join(variant_name);
    if !needs_regen(src, &cached) {
        return Ok(cached);
    }

    let img = image::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let blurred = img.blur(cfg.secondary.blur_sigma);
    blurred
        .save(&cached)
        .map_err(|e| format!("{}: {e}", cached.display()))?;
    Ok(cached)
}

/// Remove cached generated copies that no longer make sense: the wallpaper
/// they were made from is gone, or a real variant file now exists and
/// should be used instead. Best-effort -- a cache directory that can't be
/// read or a file that can't be removed just gets left for next time.
pub fn prune_stale(cfg: &Config, originals: &[String]) {
    let dir = blurred_dir(cfg);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let source_still_exists = originals
            .iter()
            .any(|o| crate::images::variant_name(o, &cfg.secondary.suffix) == name);
        let real_variant_exists = crate::images::full_path(cfg, &name).is_file();
        if !source_still_exists || real_variant_exists {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh, isolated cache_dir per test, under the OS temp dir -- so
    /// these tests can run in parallel and never touch a real config.
    fn test_cfg() -> (Config, PathBuf) {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("walldeck-blur-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cfg = Config::default();
        cfg.wallpaper_dir = dir.clone();
        cfg.cache_dir = dir.join("cache");
        cfg.secondary.blur_sigma = 3.0; // small, so the test image blurs fast
        (cfg, dir)
    }

    fn write_test_image(path: &Path) {
        // A tiny solid image is enough to exercise open/blur/save; content
        // doesn't matter for these tests, only that a file lands at `cached`.
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([100, 150, 200]));
        img.save(path).unwrap();
    }

    #[test]
    fn generates_and_reuses_a_cached_copy() {
        let (cfg, dir) = test_cfg();
        let src = dir.join("wall.png");
        write_test_image(&src);

        let p1 = get_or_generate(&cfg, &src, "wall-b.png").expect("first generation");
        assert!(p1.is_file());
        let generated_at = fs::metadata(&p1).unwrap().modified().unwrap();

        // Second call with an unchanged source must not regenerate -- same
        // mtime on the cached file either way, but we can at least confirm
        // it returns the same path without erroring.
        let p2 = get_or_generate(&cfg, &src, "wall-b.png").expect("second call");
        assert_eq!(p1, p2);
        assert_eq!(fs::metadata(&p2).unwrap().modified().unwrap(), generated_at);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_removes_copies_with_no_surviving_source() {
        let (cfg, dir) = test_cfg();
        fs::create_dir_all(dir.join("cache/blurred")).unwrap();
        let orphan = dir.join("cache/blurred/gone-b.png");
        write_test_image(&orphan);

        prune_stale(&cfg, &["still-here.png".to_string()]);
        assert!(!orphan.is_file(), "a copy for a wallpaper no longer in the list should be pruned");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_copies_still_needed() {
        let (cfg, dir) = test_cfg();
        fs::create_dir_all(dir.join("cache/blurred")).unwrap();
        let keep = dir.join("cache/blurred/wall-b.png");
        write_test_image(&keep);

        prune_stale(&cfg, &["wall.png".to_string()]);
        assert!(keep.is_file(), "still-relevant generated copy must survive pruning");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_prefers_a_real_variant_over_a_generated_one() {
        let (cfg, dir) = test_cfg();
        fs::create_dir_all(dir.join("cache/blurred")).unwrap();
        let generated = dir.join("cache/blurred/wall-b.png");
        write_test_image(&generated);
        // A real variant has since shown up next to the wallpapers.
        write_test_image(&dir.join("wall-b.png"));

        prune_stale(&cfg, &["wall.png".to_string()]);
        assert!(!generated.is_file(), "a real variant should make the generated copy stale");

        let _ = fs::remove_dir_all(&dir);
    }
}
