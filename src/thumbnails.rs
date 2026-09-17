//! Thumbnail cache: decode and resize in-process with `image` + `rayon`.
//!
//! Cache layout is one file per wallpaper, under `cache_dir`, keyed by the
//! wallpaper's own filename. Thumbnails are regenerated when the source is
//! newer, and pruned when the source disappears, so the cache stays in step
//! with the wallpaper directory without ever needing a manual clear.

use crate::config::Config;
use crate::feedback::{notify, play_sound};
use crate::images::{full_path, thumb_path};
use image::imageops::FilterType;
use rayon::prelude::*;
use std::fs;

/// If `path` (or its nearest existing ancestor) is owned by a different
/// user than the one running us, say so -- that's the single most common
/// cause of a cache directory refusing writes (typically left behind by an
/// earlier run under `sudo`), and the raw OS error alone doesn't point at
/// it. Empty string if nothing useful to add.
fn ownership_hint(path: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt;

    let mut probe = path;
    let existing = loop {
        if let Ok(meta) = fs::metadata(probe) {
            break Some((probe, meta));
        }
        match probe.parent() {
            Some(parent) if parent != probe => probe = parent,
            _ => break None,
        }
    };
    let Some((found, meta)) = existing else {
        return String::new();
    };
    let owner_uid = meta.uid();
    // SAFETY: getuid() has no documented failure mode and no preconditions.
    let our_uid = unsafe { libc_getuid() };
    if owner_uid == our_uid {
        return String::new();
    }
    format!(
        " ('{}' is owned by uid {owner_uid}, not you -- if an earlier run \
         used sudo by mistake, `sudo chown -R $USER: '{}'` or removing it \
         and letting walldeck recreate it should fix this)",
        found.display(),
        found.display(),
    )
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// True if `src` has no thumbnail yet, or the thumbnail predates the source.
fn needs_thumb(src: &std::path::Path, thumb: &std::path::Path) -> bool {
    let src_mtime = fs::metadata(src).and_then(|m| m.modified()).ok();
    let thumb_meta = fs::metadata(thumb).and_then(|m| m.modified());

    match (src_mtime, thumb_meta) {
        (Some(src_t), Ok(thumb_t)) => src_t > thumb_t,
        _ => true, // no thumb yet, or couldn't stat -> (re)generate
    }
}

/// Cover-crop a source image to `size`x`size` and write it into the cache.
/// `resize_to_fill` scales so the image *covers* the target box, then
/// center-crops the overflow -- so every thumbnail is the same square
/// regardless of the source's aspect ratio.
fn generate_one(src: &std::path::Path, thumb: &std::path::Path, size: u32) -> Result<(), String> {
    let img = image::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let cropped = img.resize_to_fill(size, size, FilterType::Lanczos3);
    cropped
        .save(thumb)
        .map_err(|e| format!("{}: {e}", thumb.display()))
}

/// Build or refresh the cache in parallel (rayon's global pool already caps
/// concurrency at the core count), then prune orphans.
pub fn ensure_thumbnails(cfg: &Config, images: &[String]) -> std::io::Result<()> {
    fs::create_dir_all(&cfg.cache_dir).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "{}: {e}{}",
                cfg.cache_dir.display(),
                ownership_hint(&cfg.cache_dir)
            ),
        )
    })?;

    let pending: Vec<&String> = images
        .iter()
        .filter(|img| needs_thumb(&full_path(cfg, img), &thumb_path(cfg, img)))
        .collect();

    // Only announce when there's actually work to do -- on the common path
    // the cache is warm and the picker should just appear.
    if !pending.is_empty() {
        notify(
            cfg,
            "walldeck",
            &format!("Generating {} thumbnail(s)…", pending.len()),
        );
        play_sound(cfg);
    }

    pending.par_iter().for_each(|img| {
        let src = full_path(cfg, img);
        let thumb = thumb_path(cfg, img);
        if let Err(e) = generate_one(&src, &thumb, cfg.thumbnail_size) {
            eprintln!("walldeck: thumbnail generation failed for {img}: {e}");
        }
    });

    prune_stale(cfg, images)?;
    Ok(())
}

/// Remove cached thumbnails whose wallpaper no longer exists.
fn prune_stale(cfg: &Config, images: &[String]) -> std::io::Result<()> {
    if !cfg.cache_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&cfg.cache_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !images.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}
