//! Load/validate XDG config, Theme struct, defaults.

use std::path::PathBuf;

use ratatui::style::Color;

/// Theme colors, mapped to ratatui `Color::Rgb` (24-bit truecolor;
/// docs.rs/ratatui/0.30.2/ratatui/style/enum.Color.html — `Color::Rgb(u8, u8, u8)`,
/// degraded terminals fall back to the emulator's closest palette match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub topbar: Color,
    pub body: Color,
    pub foreground: Color,
    pub normal: Color,
    pub insert: Color,
    pub hl: Color,
    pub cmd: Color,
    pub error: Color,
    /// Local echo (`:echo on`): successfully sent TX lines shown in the
    /// scrollback, rendered with a render-only `> ` prefix (ui.rs).
    pub tx: Color,
}

impl Default for Theme {
    /// Palette derived from the Zed "Shades Of Purple (Super Dark)" theme
    /// (`~/.local/share/zed/extensions/installed/shades-of-purple-theme/
    /// themes/shades-of-purple-theme.json`). Contrast notes below are WCAG
    /// relative-luminance ratios on `body` `#15152b` / `topbar` `#1c1b36`
    /// (AA threshold 4.5:1).
    fn default() -> Self {
        Self {
            // SoP `background`.
            topbar: rgb(0x1c, 0x1b, 0x36),
            // SoP `surface.background` — darker than the top bar, keeping
            // ncom's body-darker-than-topbar rule.
            body: rgb(0x15, 0x15, 0x2b),
            // White: ~17.9:1 on body, ~16.7:1 on topbar.
            foreground: rgb(0xff, 0xff, 0xff),
            // SoP muted lavender (`text.muted`) → COMMAND MODE chip:
            // ~7.1:1 on body, ~6.6:1 on topbar.
            normal: rgb(0xa5, 0x99, 0xe9),
            // SoP operator orange → SERIAL MODE chip: ~8.6:1 on body,
            // ~8.0:1 on topbar.
            insert: rgb(0xff, 0x9d, 0x00),
            // SoP signature yellow → `:` command line: ~12.0:1 on body,
            // ~11.2:1 on topbar.
            cmd: rgb(0xfa, 0xd0, 0x00),
            // SoP comment purple → `:hl` whole-line highlight: ~5.2:1 on
            // body, ~4.9:1 on topbar — above AA on both surfaces.
            hl: rgb(0xb3, 0x62, 0xff),
            // SoP pink-red: ~6.3:1 on body, ~5.9:1 on topbar.
            error: rgb(0xff, 0x62, 0x8c),
            // Light pink kept from ncom's tx family (distinct from `error`)
            // for `:echo on` TX lines: ~9.4:1 on body, ~8.8:1 on topbar.
            tx: rgb(0xff, 0x9e, 0xd2),
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Loaded application config. `warnings` carries per-value problems for the
/// status line; each warned value falls back to its default.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub theme: Theme,
    pub warnings: Vec<String>,
}

/// Resolve the config path: `${XDG_CONFIG_HOME}/ncom/config.toml` with fallback
/// `~/.config/ncom/config.toml` — `dirs::config_dir` reads `$XDG_CONFIG_HOME`
/// on Linux and falls back to `$HOME/.config`
/// (docs.rs/dirs/7.0.0/dirs/fn.config_dir.html).
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ncom").join("config.toml"))
}

/// Load config from the resolved XDG path. Missing file → defaults, no warning.
pub fn load() -> Config {
    match config_path() {
        Some(path) => load_from_path(&path),
        None => Config::default(),
    }
}

/// Parse and validate a config file. Never writes the file; malformed values
/// produce a warning entry and fall back to the default. A missing file is
/// the normal first-run case: defaults, no warning. Any OTHER read error
/// (permission denied, a directory at the path, ...) also falls back to
/// defaults, but warns — silently ignoring an unreadable config the user
/// explicitly installed would hide real problems.
pub fn load_from_path(path: &std::path::Path) -> Config {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            let mut cfg = Config::default();
            cfg.warnings
                .push(format!("config: unreadable ({e}); using defaults"));
            return cfg;
        }
    };
    parse(&text)
}

/// Parse TOML text into a validated [`Config`].
fn parse(text: &str) -> Config {
    let mut warnings = Vec::new();

    // Parse via toml::Table instead of a strict serde struct so that unknown
    // keys produce a per-key warning instead of failing the whole document
    // (docs.rs/toml/1.1.7/toml/struct.Table.html — Table is a Value::Table
    // payload parseable via FromStr/from_str).
    let table = match text.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("config: parse error ({e}); using defaults"));
            return Config {
                theme: Theme::default(),
                warnings,
            };
        }
    };

    let mut theme = Theme::default();
    if let Some(theme_table) = table.get("theme") {
        match theme_table.as_table() {
            Some(t) => apply_theme(t, &mut theme, &mut warnings),
            None => warnings.push("config: [theme] is not a table; using default theme".into()),
        }
    } else if !table.is_empty() {
        warnings.push("config: no [theme] section; using default theme".into());
    }

    Config { theme, warnings }
}

const THEME_KEYS: [&str; 9] = [
    "topbar",
    "body",
    "foreground",
    "normal",
    "insert",
    "hl",
    "cmd",
    "error",
    "tx",
];

fn apply_theme(table: &toml::Table, theme: &mut Theme, warnings: &mut Vec<String>) {
    for key in table.keys() {
        if !THEME_KEYS.contains(&key.as_str()) {
            warnings.push(format!("config: unknown key `theme.{key}` ignored"));
        }
    }
    for key in THEME_KEYS {
        let Some(value) = table.get(key) else {
            continue;
        };
        apply_value(theme, key, value, warnings);
    }
}

fn apply_value(theme: &mut Theme, key: &str, value: &toml::Value, warnings: &mut Vec<String>) {
    match value.as_str() {
        Some(s) => match parse_hex(s) {
            Some(c) => apply_color(theme, key, c),
            None => warnings.push(format!(
                "config: invalid hex for `theme.{key}` = {s:?}; using default"
            )),
        },
        None => warnings.push(format!(
            "config: `theme.{key}` must be a string like \"#rrggbb\"; using default"
        )),
    }
}

fn apply_color(theme: &mut Theme, key: &str, c: Color) {
    match key {
        "topbar" => theme.topbar = c,
        "body" => theme.body = c,
        "foreground" => theme.foreground = c,
        "normal" => theme.normal = c,
        "insert" => theme.insert = c,
        "hl" => theme.hl = c,
        "cmd" => theme.cmd = c,
        "error" => theme.error = c,
        "tx" => theme.tx = c,
        _ => unreachable!("THEME_KEYS is exhaustive"),
    }
}

/// Parse `#rrggbb` (also accepts `rrggbb`) into `Color::Rgb`.
fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes env-var mutation: tests that set XDG_CONFIG_HOME must not run
    /// concurrently with each other (std::env is process-global).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Shared guard for tests (any module) that mutate process-global env
    /// vars, e.g. setting `TZ` around `format_ts` assertions.
    pub(crate) fn lock_env_for_test() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    #[test]
    fn body_defaults_to_theme_surface_and_overrides_and_falls_back() {
        // Default: the SoP surface (#15152b) — strictly darker than the
        // top-bar background (#1c1b36).
        let d = Theme::default();
        assert_eq!(d.body, Color::Rgb(0x15, 0x15, 0x2b));
        assert_ne!(d.body, d.topbar);
        // Missing key → default; explicit override applies.
        let cfg = parse("[theme]\ntopbar = \"#1c1b36\"\n");
        assert_eq!(cfg.theme.body, Theme::default().body);
        assert!(cfg.warnings.is_empty());
        let cfg = parse("[theme]\nbody = \"#123456\"\n");
        assert_eq!(cfg.theme.body, Color::Rgb(0x12, 0x34, 0x56));
        assert!(cfg.warnings.is_empty());
        // Malformed hex: warning + default fallback.
        let cfg = parse("[theme]\nbody = \"nope\"\n");
        assert_eq!(cfg.theme.body, Theme::default().body);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.body")),
            "warnings: {:?}",
            cfg.warnings
        );
        // Non-string value: warning + default fallback.
        let cfg = parse("[theme]\nbody = 7\n");
        assert_eq!(cfg.theme.body, Theme::default().body);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.body")),
            "warnings: {:?}",
            cfg.warnings
        );
    }

    #[test]
    fn missing_file_yields_defaults_no_warnings() {
        let cfg = load_from_path(std::path::Path::new("/nonexistent/ncom/config.toml"));
        assert_eq!(cfg.theme, Theme::default());
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn unreadable_config_warns_and_falls_back_to_defaults() {
        // A DIRECTORY at the config path: read_to_string fails with a
        // non-NotFound error (EISDIR) — must warn, not silently load defaults.
        let dir = std::env::temp_dir().join(format!("ncom-test-cfgdir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = load_from_path(&dir);
        assert_eq!(cfg.theme, Theme::default(), "defaults still apply");
        assert_eq!(cfg.warnings.len(), 1, "warnings: {:?}", cfg.warnings);
        assert!(
            cfg.warnings[0].contains("config: unreadable"),
            "warnings: {:?}",
            cfg.warnings
        );
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn valid_overrides_apply() {
        let text = r##"
            [theme]
            topbar = "#000000"
            foreground = "#ffffff"
            normal     = "#010203"
            insert     = "#040506"
            hl         = "#0d0e0f"
            cmd        = "#070809"
            error      = "#0a0b0c"
        "##;
        let cfg = parse(text);
        assert!(cfg.warnings.is_empty());
        assert_eq!(cfg.theme.topbar, Color::Rgb(0, 0, 0));
        assert_eq!(cfg.theme.foreground, Color::Rgb(255, 255, 255));
        assert_eq!(cfg.theme.normal, Color::Rgb(1, 2, 3));
        assert_eq!(cfg.theme.insert, Color::Rgb(4, 5, 6));
        assert_eq!(cfg.theme.hl, Color::Rgb(13, 14, 15));
        assert_eq!(cfg.theme.cmd, Color::Rgb(7, 8, 9));
        assert_eq!(cfg.theme.error, Color::Rgb(10, 11, 12));
    }

    #[test]
    fn defaults_when_only_some_keys_set() {
        let cfg = parse("[theme]\ncmd = \"#123456\"\n");
        assert_eq!(cfg.theme.cmd, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(cfg.theme.topbar, Theme::default().topbar);
        assert_eq!(cfg.theme.error, Theme::default().error);
    }

    #[test]
    fn malformed_hex_falls_back_with_warning() {
        let cfg = parse("[theme]\ncmd = \"#zzz\"\nnormal = \"#12345\"\ninsert = \"notahex\"\n");
        assert_eq!(cfg.theme.cmd, Theme::default().cmd);
        assert_eq!(cfg.theme.normal, Theme::default().normal);
        assert_eq!(cfg.theme.insert, Theme::default().insert);
        assert_eq!(cfg.warnings.len(), 3, "warnings: {:?}", cfg.warnings);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.cmd")),
            "warnings: {:?}",
            cfg.warnings
        );
    }

    #[test]
    fn non_string_value_falls_back_with_warning() {
        let cfg = parse("[theme]\nerror = 17\n");
        assert_eq!(cfg.theme.error, Theme::default().error);
        assert!(cfg.warnings.iter().any(|w| w.contains("theme.error")));
    }

    #[test]
    fn hl_defaults_to_comment_purple_and_overrides_and_falls_back() {
        // Default: the SoP comment purple, independent of the insert accent.
        assert_eq!(Theme::default().hl, Color::Rgb(0xb3, 0x62, 0xff));
        // Existing config files without `hl` keep the default for that key.
        let cfg = parse("[theme]\ninsert = \"#010203\"\n");
        assert_eq!(cfg.theme.hl, Theme::default().hl);
        assert!(cfg.warnings.is_empty());
        // Override applies.
        let cfg = parse("[theme]\nhl = \"#123456\"\n");
        assert_eq!(cfg.theme.hl, Color::Rgb(0x12, 0x34, 0x56));
        assert!(cfg.warnings.is_empty());
        // Malformed hex: warning + default fallback, other keys untouched.
        let cfg = parse("[theme]\nhl = \"nope\"\ninsert = \"#0a0b0c\"\n");
        assert_eq!(cfg.theme.hl, Theme::default().hl);
        assert_eq!(cfg.theme.insert, Color::Rgb(0x0a, 0x0b, 0x0c));
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.hl")),
            "warnings: {:?}",
            cfg.warnings
        );
        // Non-string value: warning + default fallback.
        let cfg = parse("[theme]\nhl = 7\n");
        assert_eq!(cfg.theme.hl, Theme::default().hl);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.hl")),
            "warnings: {:?}",
            cfg.warnings
        );
    }

    #[test]
    fn tx_defaults_to_light_pink_and_overrides_and_falls_back() {
        // Default: the light pink local-echo accent.
        assert_eq!(Theme::default().tx, Color::Rgb(0xff, 0x9e, 0xd2));
        // Missing key → default (older configs without `tx` keep working).
        let cfg = parse("[theme]\ninsert = \"#010203\"\n");
        assert_eq!(cfg.theme.tx, Theme::default().tx);
        assert!(cfg.warnings.is_empty());
        // Override applies.
        let cfg = parse("[theme]\ntx = \"#123456\"\n");
        assert_eq!(cfg.theme.tx, Color::Rgb(0x12, 0x34, 0x56));
        assert!(cfg.warnings.is_empty());
        // Malformed hex: warning + default fallback, other keys untouched.
        let cfg = parse("[theme]\ntx = \"nope\"\ninsert = \"#0a0b0c\"\n");
        assert_eq!(cfg.theme.tx, Theme::default().tx);
        assert_eq!(cfg.theme.insert, Color::Rgb(0x0a, 0x0b, 0x0c));
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.tx")),
            "warnings: {:?}",
            cfg.warnings
        );
        // Non-string value: warning + default fallback.
        let cfg = parse("[theme]\ntx = 7\n");
        assert_eq!(cfg.theme.tx, Theme::default().tx);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("theme.tx")),
            "warnings: {:?}",
            cfg.warnings
        );
    }

    #[test]
    fn unknown_key_warns_and_keeps_rest() {
        let cfg = parse("[theme]\ncolor = \"#112233\"\ntopbar = \"#445566\"\n");
        assert_eq!(cfg.theme.topbar, Color::Rgb(0x44, 0x55, 0x66));
        assert!(cfg.warnings.iter().any(|w| w.contains("theme.color")));
    }

    #[test]
    fn topbar_applies_without_warnings() {
        // The old `background` alias is gone: it now warns as an unknown key
        // and the value is ignored.
        let cfg = parse("[theme]\ntopbar = \"#333333\"\n");
        assert_eq!(cfg.theme.topbar, Color::Rgb(0x33, 0x33, 0x33));
        assert!(cfg.warnings.is_empty());
        let cfg = parse("[theme]\nbackground = \"#123456\"\n");
        assert_eq!(cfg.theme.topbar, Theme::default().topbar);
        assert!(
            cfg.warnings
                .iter()
                .any(|w| w.contains("unknown key `theme.background` ignored"))
        );
    }

    #[test]
    fn malformed_document_warns_and_uses_defaults() {
        let cfg = parse("[theme\nbackground = \"#000000\"");
        assert_eq!(cfg.theme, Theme::default());
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("parse error"));
    }

    #[test]
    fn xdg_config_home_override_is_respected() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("ncom-test-{}", std::process::id()));
        let cfg_dir = tmp.join("ncom");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[theme]\ntopbar = \"#abcdef\"\n",
        )
        .unwrap();

        // SAFETY: single-threaded w.r.t. env mutation (guarded by ENV_LOCK);
        // no other thread reads the environment during this test.
        let old = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
        }
        let path = config_path();
        let cfg = load();
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        std::fs::remove_dir_all(&tmp).unwrap();

        assert_eq!(
            path,
            Some(cfg_dir.join("config.toml")),
            "config_path must honor XDG_CONFIG_HOME"
        );
        assert_eq!(cfg.theme.topbar, Color::Rgb(0xab, 0xcd, 0xef));
        assert!(cfg.warnings.is_empty());
    }
}
