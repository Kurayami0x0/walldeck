use crate::config::{Config, IMAGE_EXTS};
use std::fs;
use std::path::PathBuf;

/// List wallpaper filenames (not full paths) directly inside `wallpaper_dir`,
/// filtered to supported extensions, sorted. Non-recursive by design: a
/// wallpaper directory with subfolders usually means the subfolders are
/// collections the user organizes by hand, not more wallpapers to offer.
pub fn list_images(cfg: &Config) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(&cfg.wallpaper_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy().to_string();
        let ext_matches = name
            .rsplit_once('.')
            .map(|(_, ext)| IMAGE_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .unwrap_or(false);
        if ext_matches {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Drop secondary-target variants (files whose stem ends in `suffix`) from a
/// listing, so only primary wallpapers are offered in the picker. An empty
/// suffix filters nothing -- that's the "no secondary target" case.
pub fn filter_originals(images: Vec<String>, suffix: &str) -> Vec<String> {
    if suffix.is_empty() {
        return images;
    }
    images
        .into_iter()
        .filter(|img| {
            let stem = img.rsplit_once('.').map(|(s, _)| s).unwrap_or(img.as_str());
            !stem.ends_with(suffix)
        })
        .collect()
}

/// Derive the secondary-target filename for a selected image, by appending
/// `suffix` to the stem (`wall.jpg` + `-b` -> `wall-b.jpg`).
pub fn variant_name(image: &str, suffix: &str) -> String {
    match image.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}{suffix}.{ext}"),
        None => format!("{image}{suffix}"),
    }
}

pub fn full_path(cfg: &Config, filename: &str) -> PathBuf {
    cfg.wallpaper_dir.join(filename)
}

pub fn thumb_path(cfg: &Config, filename: &str) -> PathBuf {
    cfg.cache_dir.join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn variant_name_inserts_suffix_before_the_extension() {
        assert_eq!(variant_name("wall.jpg", "-b"), "wall-b.jpg");
        assert_eq!(variant_name("a.b.png", "-blur"), "a.b-blur.png");
        assert_eq!(variant_name("noext", "-b"), "noext-b");
    }

    #[test]
    fn filter_drops_only_variants() {
        let got = filter_originals(names(&["a.jpg", "a-b.jpg", "b-blue.png", "c.png"]), "-b");
        assert_eq!(got, names(&["a.jpg", "b-blue.png", "c.png"]),
                   "'-blue' ends with 'e', not the '-b' suffix on the stem");
    }

    #[test]
    fn empty_suffix_filters_nothing() {
        let all = names(&["a.jpg", "b.jpg"]);
        assert_eq!(filter_originals(all.clone(), ""), all);
    }

    #[test]
    fn round_trips_with_the_filter() {
        let originals = filter_originals(names(&["x.jpg", "x-b.jpg"]), "-b");
        assert_eq!(originals, names(&["x.jpg"]));
        assert_eq!(variant_name(&originals[0], "-b"), "x-b.jpg");
    }
}
