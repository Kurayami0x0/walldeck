//! Configuration.
//!
//! Everything tunable lives in one flat `key = value` file, parsed by hand --
//! no `serde`/`toml` dependency for what is ultimately a few dozen scalar
//! settings. `[section]` headers are cosmetic grouping for readability and
//! carry no meaning, so every key is globally unique. `#` starts a comment.
//!
//! Resolution order for the file:
//!   1. `$WALLDECK_CONFIG` (explicit override, any path)
//!   2. `$XDG_CONFIG_HOME/walldeck/config.conf`
//!   3. `~/.config/walldeck/config.conf`
//!
//! Every field has a built-in default, so a missing file, a missing key, or
//! an unparsable value all fall back rather than failing. A bad value warns
//! on stderr and keeps that one field's default -- the program never refuses
//! to start over a typo.

use crate::theme::{parse_hex_color, Rgba, Theme};
use std::fs;
use std::path::{Path, PathBuf};

pub const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp"];

/// Backend command-line options for one wallpaper target. Every field is
/// optional: `None`/empty means the corresponding flag is simply not passed,
/// leaving the backend's own default in effect. That keeps this forward
/// compatible with backend versions that add, rename, or drop flags -- and
/// `extra_args` is the escape hatch for anything not modelled here.
#[derive(Debug, Clone, Default)]
pub struct TransitionOpts {
    pub transition_type: Option<String>,
    pub fps: Option<String>,
    pub duration: Option<String>,
    pub step: Option<String>,
    pub angle: Option<String>,
    pub pos: Option<String>,
    pub bezier: Option<String>,
    pub wave: Option<String>,
    pub resize: Option<String>,
    pub fill_color: Option<String>,
    pub filter: Option<String>,
    pub extra_args: Vec<String>,
}

impl TransitionOpts {
    /// Flatten into CLI arguments, skipping anything unset.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let mut push = |flag: &str, value: &Option<String>| {
            if let Some(v) = value {
                if !v.is_empty() {
                    args.push(flag.to_string());
                    args.push(v.clone());
                }
            }
        };
        push("--transition-type", &self.transition_type);
        push("--transition-fps", &self.fps);
        push("--transition-duration", &self.duration);
        push("--transition-step", &self.step);
        push("--transition-angle", &self.angle);
        push("--transition-pos", &self.pos);
        push("--transition-bezier", &self.bezier);
        push("--transition-wave", &self.wave);
        push("--resize", &self.resize);
        push("--fill-color", &self.fill_color);
        push("--filter", &self.filter);
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

/// An optional second wallpaper target: the same selection applied to a
/// different backend namespace, using a differently-suffixed file. The
/// typical use is a pre-blurred copy (`wall.jpg` -> `wall-b.jpg`) for an
/// overview/exposé layer, but nothing here assumes that specific use.
#[derive(Debug, Clone)]
pub struct SecondaryTarget {
    pub enabled: bool,
    pub namespace: Option<String>,
    /// Appended to the filename stem to find this target's file.
    pub suffix: String,
    /// When false, a failure applying this target (most commonly: no daemon
    /// running for its namespace) is reported as a warning and the program
    /// still exits successfully.
    pub required: bool,
    /// When a wallpaper has no matching `suffix` variant on disk, generate a
    /// blurred copy on the fly instead of skipping this target. Generated
    /// copies are cached under `cache_dir` (regenerated only when the
    /// source changes) and never override a real variant file if one is
    /// later added.
    pub auto_blur: bool,
    /// Gaussian blur sigma for `auto_blur`. Larger = blurrier; the `image`
    /// crate's own units, roughly "pixels of blur radius".
    pub blur_sigma: f32,
    pub opts: TransitionOpts,
}

#[derive(Debug, Clone)]
pub struct Config {
    // -- paths --
    pub wallpaper_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub thumbnail_size: u32,
    /// `None` disables the sound entirely. A configured-but-missing file is
    /// also silently skipped rather than treated as an error.
    pub sound_file: Option<PathBuf>,
    pub sound_command: String,
    pub notifications: bool,

    // -- backend --
    pub command: String,
    pub namespace: Option<String>,
    pub opts: TransitionOpts,
    pub secondary: SecondaryTarget,

    // -- appearance --
    pub theme: Theme,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            wallpaper_dir: home_relative("Pictures/Wallpapers"),
            cache_dir: cache_relative("walldeck"),
            thumbnail_size: 350,
            // A freedesktop sound theme path, present on most desktop
            // installs. Missing file == no sound, so this is safe as a
            // default even where the theme isn't installed.
            sound_file: Some(PathBuf::from(
                "/usr/share/sounds/freedesktop/stereo/complete.oga",
            )),
            sound_command: "pw-play".to_string(),
            notifications: true,

            command: "awww".to_string(),
            namespace: None,
            opts: TransitionOpts {
                transition_type: Some("center".to_string()),
                fps: Some("60".to_string()),
                ..TransitionOpts::default()
            },
            secondary: SecondaryTarget {
                enabled: true,
                namespace: Some("overview".to_string()),
                suffix: "-b".to_string(),
                required: false,
                auto_blur: false,
                blur_sigma: 20.0,
                opts: TransitionOpts {
                    fps: Some("60".to_string()),
                    ..TransitionOpts::default()
                },
            },

            theme: Theme::default(),
        }
    }
}

impl Config {
    /// Path the config would be read from, whether or not it exists.
    pub fn path() -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var("WALLDECK_CONFIG") {
            if !explicit.is_empty() {
                return Some(expand_path(&explicit));
            }
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))?;
        Some(PathBuf::from(base).join("walldeck/config.conf"))
    }

    pub fn load() -> Self {
        let mut cfg = Config::default();
        let Some(path) = Self::path() else {
            return cfg.finish();
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return cfg.finish();
        };

        for (lineno, raw_line) in text.lines().enumerate() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() || (line.starts_with('[') && line.ends_with(']')) {
                continue; // blank, comment, or cosmetic section header
            }
            let Some((key, raw_value)) = line.split_once('=') else {
                eprintln!(
                    "walldeck: {}:{}: expected `key = value`, skipping",
                    path.display(),
                    lineno + 1
                );
                continue;
            };
            let key = key.trim();
            let loc = format!("{}:{}", path.display(), lineno + 1);
            let value = Value {
                raw: raw_value.trim(),
                key,
                loc: &loc,
            };

            if !cfg.set_key(key, &value) && !cfg.theme.set_key(key, &value) {
                eprintln!("walldeck: {loc}: unknown key '{key}', ignoring");
            }
        }

        cfg.finish()
    }

    /// Apply a single key. Returns false if this module doesn't own the key,
    /// so the caller can offer it to the theme instead.
    fn set_key(&mut self, key: &str, v: &Value) -> bool {
        match key {
            "wallpaper_dir" => self.wallpaper_dir = v.path(),
            "cache_dir" => self.cache_dir = v.path(),
            "thumbnail_size" => {
                if let Some(n) = v.int() {
                    self.thumbnail_size = n.max(1) as u32;
                }
            }
            "sound_file" => self.sound_file = v.opt_text().map(|s| expand_path(&s)),
            "sound_command" => self.sound_command = v.text(),
            "notifications" => {
                if let Some(b) = v.boolean() {
                    self.notifications = b;
                }
            }

            "command" => self.command = v.text(),
            "namespace" => self.namespace = v.opt_text(),

            "secondary_enabled" => {
                if let Some(b) = v.boolean() {
                    self.secondary.enabled = b;
                }
            }
            "secondary_namespace" => self.secondary.namespace = v.opt_text(),
            "secondary_suffix" => self.secondary.suffix = v.text(),
            "secondary_required" => {
                if let Some(b) = v.boolean() {
                    self.secondary.required = b;
                }
            }
            "secondary_auto_blur" => {
                if let Some(b) = v.boolean() {
                    self.secondary.auto_blur = b;
                }
            }
            "secondary_blur_sigma" => {
                if let Some(n) = v.float() {
                    self.secondary.blur_sigma = n.max(0.1) as f32;
                }
            }

            _ => {
                // Transition options for both targets share one table,
                // distinguished by the `secondary_` prefix.
                return match key.strip_prefix("secondary_") {
                    Some(rest) => set_transition_key(&mut self.secondary.opts, rest, v),
                    None => set_transition_key(&mut self.opts, key, v),
                };
            }
        }
        true
    }

    /// Post-parse clamping and normalization that depends on several fields
    /// at once, so it can't be done during the per-key pass.
    fn finish(mut self) -> Self {
        self.theme.clamp();
        if self.sound_command.is_empty() {
            self.sound_file = None;
        }
        // An empty suffix would make the secondary file identical to the
        // primary, which in turn would make every wallpaper look like its own
        // variant and filter the whole list away.
        if self.secondary.suffix.is_empty() {
            self.secondary.enabled = false;
        }
        self
    }

    pub fn sound_path(&self) -> Option<&Path> {
        self.sound_file.as_deref().filter(|p| p.is_file())
    }
}

fn set_transition_key(opts: &mut TransitionOpts, key: &str, v: &Value) -> bool {
    match key {
        "transition_type" => opts.transition_type = v.opt_text(),
        "transition_fps" => opts.fps = v.opt_text(),
        "transition_duration" => opts.duration = v.opt_text(),
        "transition_step" => opts.step = v.opt_text(),
        "transition_angle" => opts.angle = v.opt_text(),
        "transition_pos" => opts.pos = v.opt_text(),
        "transition_bezier" => opts.bezier = v.opt_text(),
        "transition_wave" => opts.wave = v.opt_text(),
        "resize" => opts.resize = v.opt_text(),
        "fill_color" => opts.fill_color = v.opt_text(),
        "filter" => opts.filter = v.opt_text(),
        "extra_args" => opts.extra_args = v.args(),
        _ => return false,
    }
    true
}

// ------------------------------------------------------------ value parsing

/// Cut a line at its first unquoted `#`. A naive `line.split('#').next()`
/// would also cut inside a quoted hex color like `"#1e1e2eeb"`, truncating
/// the value to a bare `"` -- so this tracks whether we're inside a
/// double-quoted span and only treats `#` as a comment starter outside one.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

/// One `key = value` right-hand side, carrying enough context to emit a
/// useful warning when it doesn't parse.
pub struct Value<'a> {
    pub raw: &'a str,
    pub key: &'a str,
    pub loc: &'a str,
}

impl Value<'_> {
    fn warn(&self, what: &str) {
        eprintln!(
            "walldeck: {}: '{}' is not {} for {}",
            self.loc, self.raw, what, self.key
        );
    }

    /// Trimmed, with surrounding double quotes removed if present -- quoting
    /// is optional everywhere, and only matters for values with leading or
    /// trailing spaces.
    pub fn text(&self) -> String {
        let s = self.raw.trim();
        match (s.starts_with('"'), s.ends_with('"'), s.len() >= 2) {
            (true, true, true) => s[1..s.len() - 1].to_string(),
            _ => s.to_string(),
        }
    }

    /// `None` for an empty value, which is how the config says "don't pass
    /// this flag at all" / "disable this".
    pub fn opt_text(&self) -> Option<String> {
        let s = self.text();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    pub fn int(&self) -> Option<i32> {
        self.text().parse().ok().or_else(|| {
            self.warn("a valid integer");
            None
        })
    }

    pub fn float(&self) -> Option<f64> {
        self.text().parse().ok().or_else(|| {
            self.warn("a valid number");
            None
        })
    }

    pub fn byte(&self) -> Option<u8> {
        self.text()
            .parse::<i32>()
            .ok()
            .map(|n| n.clamp(0, 255) as u8)
            .or_else(|| {
                self.warn("a valid 0-255 value");
                None
            })
    }

    pub fn boolean(&self) -> Option<bool> {
        match self.text().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => {
                self.warn("a boolean (true/false)");
                None
            }
        }
    }

    pub fn color(&self) -> Option<Rgba> {
        parse_hex_color(&self.text()).or_else(|| {
            self.warn("a valid color (expected \"#rrggbb\" or \"#rrggbbaa\")");
            None
        })
    }

    pub fn curve(&self) -> Option<(f64, f64, f64, f64)> {
        let text = self.text();
        let parts: Vec<&str> = text.split(',').map(|s| s.trim()).collect();
        if parts.len() != 4 {
            self.warn("4 comma-separated numbers");
            return None;
        }
        let nums: Option<Vec<f64>> = parts.iter().map(|p| p.parse::<f64>().ok()).collect();
        match nums {
            Some(n) => Some((n[0], n[1], n[2], n[3])),
            None => {
                self.warn("4 valid numbers");
                None
            }
        }
    }

    pub fn path(&self) -> PathBuf {
        expand_path(&self.text())
    }

    /// Whitespace-split raw arguments. Deliberately not a shell parser:
    /// there's no quoting or escaping here, so arguments containing spaces
    /// aren't expressible. Anything that complex belongs in a wrapper script.
    pub fn args(&self) -> Vec<String> {
        self.text()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect()
    }
}

// ------------------------------------------------------------ path handling

/// Look up an environment variable for path expansion. For the XDG Base
/// Directory variables, an empty/unset value doesn't mean "empty string" --
/// the spec defines a fallback (e.g. `$XDG_CACHE_HOME` unset means
/// `$HOME/.cache`), and *not* applying it is a real footgun: naively
/// substituting "" collapses `$XDG_CACHE_HOME/walldeck` into the absolute
/// path `/walldeck`, which then fails with a permission error when
/// something tries to create it -- silently, and pointing nowhere near the
/// actual cause. `XDG_CACHE_HOME` in particular is commonly left unset by
/// minimal compositor/systemd sessions even though `HOME` always is.
fn lookup_path_var(name: &str, home: &str) -> String {
    let fallback = match name {
        "XDG_CACHE_HOME" => Some(".cache"),
        "XDG_CONFIG_HOME" => Some(".config"),
        "XDG_DATA_HOME" => Some(".local/share"),
        "XDG_STATE_HOME" => Some(".local/state"),
        _ => None,
    };
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => match fallback {
            Some(rel) if !home.is_empty() => format!("{home}/{rel}"),
            _ => std::env::var(name).unwrap_or_default(),
        },
    }
}

/// Expand a leading `~` and any `$VAR` / `${VAR}` references. An undefined
/// variable expands to nothing (matching shell behavior), except for the
/// XDG Base Directory variables, which fall back to their spec-defined
/// default under `$HOME` instead -- see `lookup_path_var`.
pub fn expand_path(input: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();

    let input = if input == "~" {
        home.clone()
    } else if let Some(rest) = input.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        input.to_string()
    };

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            let ok = if braced {
                c != '}'
            } else {
                c.is_ascii_alphanumeric() || c == '_'
            };
            if !ok {
                break;
            }
            name.push(c);
            chars.next();
        }
        if braced {
            chars.next(); // consume '}'
        }
        if name.is_empty() {
            out.push('$'); // a bare '$' isn't a variable reference
        } else {
            out.push_str(&lookup_path_var(&name, &home));
        }
    }
    PathBuf::from(out)
}

fn home_relative(rest: &str) -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(rest),
        _ => PathBuf::from(rest),
    }
}

fn cache_relative(rest: &str) -> PathBuf {
    match std::env::var("XDG_CACHE_HOME") {
        Ok(base) if !base.is_empty() => PathBuf::from(base).join(rest),
        _ => home_relative(&format!(".cache/{rest}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v<'a>(raw: &'a str, key: &'a str) -> Value<'a> {
        Value { raw, key, loc: "test:1" }
    }

    #[test]
    fn text_strips_optional_quotes() {
        assert_eq!(v("  hello  ", "k").text(), "hello");
        assert_eq!(v("\" spaced \"", "k").text(), " spaced ");
        assert_eq!(v("\"", "k").text(), "\"", "a lone quote isn't a pair");
    }

    #[test]
    fn empty_value_means_unset() {
        assert_eq!(v("", "k").opt_text(), None);
        assert_eq!(v("  ", "k").opt_text(), None);
        assert_eq!(v("x", "k").opt_text(), Some("x".into()));
    }

    #[test]
    fn booleans_accept_common_spellings() {
        for s in ["true", "yes", "on", "1", "TRUE"] {
            assert_eq!(v(s, "k").boolean(), Some(true), "{s}");
        }
        for s in ["false", "no", "off", "0"] {
            assert_eq!(v(s, "k").boolean(), Some(false), "{s}");
        }
        assert_eq!(v("maybe", "k").boolean(), None);
    }

    #[test]
    fn unset_transition_options_emit_no_flags() {
        assert!(TransitionOpts::default().to_args().is_empty());
    }

    #[test]
    fn transition_options_map_to_flags_in_order() {
        let opts = TransitionOpts {
            transition_type: Some("wipe".into()),
            fps: Some("144".into()),
            angle: Some("30".into()),
            extra_args: vec!["--invert-y".into()],
            ..TransitionOpts::default()
        };
        assert_eq!(
            opts.to_args(),
            vec![
                "--transition-type", "wipe",
                "--transition-fps", "144",
                "--transition-angle", "30",
                "--invert-y",
            ]
        );
    }

    #[test]
    fn expands_tilde_and_variables() {
        std::env::set_var("HOME", "/home/test");
        std::env::set_var("WALLDECK_TEST_VAR", "xyz");
        assert_eq!(expand_path("~/pics"), PathBuf::from("/home/test/pics"));
        assert_eq!(expand_path("~"), PathBuf::from("/home/test"));
        assert_eq!(expand_path("$HOME/a"), PathBuf::from("/home/test/a"));
        assert_eq!(expand_path("${WALLDECK_TEST_VAR}/b"), PathBuf::from("xyz/b"));
        assert_eq!(expand_path("/plain/path"), PathBuf::from("/plain/path"));
        // Undefined variables vanish, matching shell behavior.
        assert_eq!(expand_path("$NOPE_NOT_SET/c"), PathBuf::from("/c"));
        // A bare '$' is not a variable reference.
        assert_eq!(expand_path("/a$/b"), PathBuf::from("/a$/b"));
        // '~' only expands at the start.
        assert_eq!(expand_path("/a/~/b"), PathBuf::from("/a/~/b"));
    }

    #[test]
    fn unset_xdg_cache_home_falls_back_to_home_cache_not_empty_string() {
        // Regression test: naively substituting "" for an unset
        // $XDG_CACHE_HOME turns "$XDG_CACHE_HOME/walldeck" into the
        // absolute path "/walldeck" -- filesystem root, which then fails
        // with a permission error that gives no hint why. XDG_CACHE_HOME
        // is routinely left unset by minimal compositor/systemd sessions
        // even though HOME always is, so this isn't a hypothetical.
        std::env::set_var("HOME", "/home/test");
        std::env::remove_var("XDG_CACHE_HOME");
        assert_eq!(
            expand_path("$XDG_CACHE_HOME/walldeck"),
            PathBuf::from("/home/test/.cache/walldeck"),
            "must fall back to $HOME/.cache per the XDG Base Directory spec, not collapse to /walldeck"
        );
    }

    #[test]
    fn set_xdg_cache_home_is_still_honored() {
        std::env::set_var("HOME", "/home/test");
        std::env::set_var("XDG_CACHE_HOME", "/custom/cache");
        assert_eq!(
            expand_path("$XDG_CACHE_HOME/walldeck"),
            PathBuf::from("/custom/cache/walldeck")
        );
        std::env::remove_var("XDG_CACHE_HOME");
    }

    #[test]
    fn other_xdg_vars_have_spec_fallbacks_too() {
        std::env::set_var("HOME", "/home/test");
        for var in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
            std::env::remove_var(var);
        }
        assert_eq!(expand_path("$XDG_CONFIG_HOME/x"), PathBuf::from("/home/test/.config/x"));
        assert_eq!(expand_path("$XDG_DATA_HOME/x"), PathBuf::from("/home/test/.local/share/x"));
        assert_eq!(expand_path("$XDG_STATE_HOME/x"), PathBuf::from("/home/test/.local/state/x"));
    }

    #[test]
    fn non_xdg_vars_are_unaffected_by_the_fallback_logic() {
        std::env::remove_var("WALLDECK_TOTALLY_UNSET_VAR");
        assert_eq!(expand_path("$WALLDECK_TOTALLY_UNSET_VAR/x"), PathBuf::from("/x"));
    }

    #[test]
    fn secondary_keys_route_to_the_secondary_target() {
        let mut cfg = Config::default();
        assert!(cfg.set_key("transition_type", &v("fade", "transition_type")));
        assert!(cfg.set_key("secondary_transition_type", &v("grow", "secondary_transition_type")));
        assert_eq!(cfg.opts.transition_type.as_deref(), Some("fade"));
        assert_eq!(cfg.secondary.opts.transition_type.as_deref(), Some("grow"));
    }

    #[test]
    fn unknown_keys_are_rejected_so_the_caller_can_warn() {
        let mut cfg = Config::default();
        assert!(!cfg.set_key("not_a_real_key", &v("1", "not_a_real_key")));
    }

    #[test]
    fn empty_secondary_suffix_disables_the_target() {
        let mut cfg = Config::default();
        cfg.set_key("secondary_suffix", &v("", "secondary_suffix"));
        let cfg = cfg.finish();
        assert!(!cfg.secondary.enabled, "an empty suffix would match every file");
    }

    #[test]
    fn clearing_a_transition_option_unsets_it() {
        let mut cfg = Config::default();
        assert_eq!(cfg.opts.transition_type.as_deref(), Some("center"));
        cfg.set_key("transition_type", &v("", "transition_type"));
        assert_eq!(cfg.opts.transition_type, None, "empty must mean 'omit the flag'");
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        assert_eq!(
            strip_comment(r##"background = "#1e1e2eeb""##),
            r##"background = "#1e1e2eeb""##
        );
        assert_eq!(
            strip_comment(r##"background = "#1e1e2eeb"  # icon panel color"##),
            r##"background = "#1e1e2eeb"  "##
        );
        assert_eq!(strip_comment("padding = 10  # inset"), "padding = 10  ");
        assert_eq!(strip_comment("# whole line comment"), "");
    }

    #[test]
    fn color_value_with_trailing_comment_parses() {
        // The exact shape that was broken: a quoted #rrggbbaa color followed
        // by an inline comment. Regression test for the naive split('#')
        // that cut the value down to a bare `"`.
        let line = strip_comment(r##"background = "#1e1e2eeb"  # icon panel color"##).trim();
        let (key, raw_value) = line.split_once('=').unwrap();
        let val = v(raw_value.trim(), key.trim());
        assert!(val.color().is_some(), "expected a valid color, got raw={:?}", val.raw);
    }
}
