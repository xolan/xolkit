//! xolkit's own light and dark themes, plus Base16 schemes: a bundled set and any
//! `*.yaml` files in `~/.config/xolkit/themes/` (`%APPDATA%\xolkit\themes\` on Windows).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;
use serde_json::json;

use crate::settings;

pub const DARK: &str = "xolkit Dark";
pub const LIGHT: &str = "xolkit Light";

const OWN: &str = include_str!("../themes/xolkit.json");

const BASE16: &[&str] = &[
    include_str!("../themes/base16/catppuccin-latte.yaml"),
    include_str!("../themes/base16/catppuccin-mocha.yaml"),
    include_str!("../themes/base16/dracula.yaml"),
    include_str!("../themes/base16/gruvbox-dark-medium.yaml"),
    include_str!("../themes/base16/gruvbox-light-medium.yaml"),
    include_str!("../themes/base16/nord.yaml"),
    include_str!("../themes/base16/one-light.yaml"),
    include_str!("../themes/base16/onedark.yaml"),
    include_str!("../themes/base16/rose-pine-dawn.yaml"),
    include_str!("../themes/base16/rose-pine.yaml"),
    include_str!("../themes/base16/solarized-dark.yaml"),
    include_str!("../themes/base16/solarized-light.yaml"),
    include_str!("../themes/base16/tokyo-night-dark.yaml"),
    include_str!("../themes/base16/tokyo-night-light.yaml"),
];

/// Registers every theme and applies the saved one, or the one matching the system appearance.
pub fn init(window: &Window, cx: &mut App) {
    let themes = settings::store(cx).env.themes_dir();
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(OWN)
        .expect("bundled theme is valid");
    for yaml in BASE16 {
        let set = base16(yaml).expect("bundled Base16 scheme is valid");
        registry
            .load_themes_from_str(&set)
            .expect("Base16 theme is valid");
    }
    for (path, result) in user_base16(&themes) {
        if let Err(e) = result.and_then(|set| registry.load_themes_from_str(&set)) {
            eprintln!("xolkit: skipping theme {}: {e:#}", path.display());
        }
    }

    let saved = settings::load(cx).get("theme").map(str::to_owned);
    let system = match window.appearance() {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => DARK,
        WindowAppearance::Light | WindowAppearance::VibrantLight => LIGHT,
    };
    if !saved.is_some_and(|name| apply(&name, cx)) {
        apply(system, cx);
    }
}

/// Themes for the picker: xolkit's own first, then the rest by name.
pub fn names(cx: &App) -> Vec<(SharedString, ThemeMode)> {
    let mut names: Vec<_> = ThemeRegistry::global(cx)
        .themes()
        .values()
        .filter(|t| !t.name.starts_with("Default "))
        .map(|t| (t.name.clone(), t.mode))
        .collect();
    names.sort_by_key(|(name, _)| (!name.starts_with("xolkit"), name.to_lowercase()));
    names
}

/// Applies a theme and remembers it for the next start.
pub fn select(name: &str, cx: &mut App) {
    if apply(name, cx) {
        let mut saved = settings::load(cx);
        saved.set("theme", name);
        settings::save_in_background(saved, cx);
    }
}

fn apply(name: &str, cx: &mut App) -> bool {
    let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() else {
        return false;
    };
    Theme::update(cx, |theme| theme.apply_config(&config));
    true
}

fn user_base16(dir: &Path) -> Vec<(PathBuf, Result<String>)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .map(|p| {
            let set = fs::read_to_string(&p)
                .map_err(Into::into)
                .and_then(|t| base16(&t));
            (p, set)
        })
        .collect()
}

/// Turns a Base16 scheme (current `palette:` format or the older flat one) into a theme set.
fn base16(yaml: &str) -> Result<String> {
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml)?;
    let name = doc
        .get("name")
        .or_else(|| doc.get("scheme"))
        .and_then(|v| v.as_str())
        .context("scheme has no name")?;
    let palette = doc.get("palette").unwrap_or(&doc);
    let mut b = Vec::with_capacity(16);
    for i in 0..16 {
        let key = format!("base{i:02X}");
        let raw = palette
            .get(&key)
            .and_then(|v| v.as_str())
            .with_context(|| format!("scheme has no {key}"))?;
        let hex = raw.trim_start_matches('#');
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{key} is not a #RRGGBB color: {raw}");
        }
        b.push(format!("#{hex}"));
    }
    let dark = match doc.get("variant").and_then(|v| v.as_str()) {
        Some("light") => false,
        Some("dark") => true,
        _ => luminance(&b[0]) < 0.5,
    };
    let a = |i: usize, alpha: &str| format!("{}{alpha}", b[i]);
    let colors: serde_json::Map<String, serde_json::Value> = [
        ("background", b[0].clone()),
        ("foreground", b[5].clone()),
        ("border", b[2].clone()),
        ("window.border", b[2].clone()),
        ("muted.background", b[1].clone()),
        ("muted.foreground", b[4].clone()),
        ("accent.background", b[2].clone()),
        ("accent.foreground", b[6].clone()),
        ("primary.background", b[0xD].clone()),
        ("primary.hover.background", a(0xD, "E6")),
        ("primary.active.background", a(0xD, "CC")),
        ("primary.foreground", b[0].clone()),
        ("secondary.background", b[1].clone()),
        ("secondary.hover.background", b[2].clone()),
        ("secondary.active.background", b[3].clone()),
        ("secondary.foreground", b[5].clone()),
        ("popover.background", b[0].clone()),
        ("popover.foreground", b[5].clone()),
        ("group_box.background", b[1].clone()),
        ("group_box.foreground", b[5].clone()),
        ("input.border", b[3].clone()),
        ("ring", b[0xD].clone()),
        ("caret", b[0xD].clone()),
        ("selection.background", a(0xD, "4D")),
        ("link.foreground", b[0xD].clone()),
        ("list.background", b[0].clone()),
        ("list.even.background", b[1].clone()),
        ("list.head.background", b[1].clone()),
        ("list.active.background", a(0xD, "26")),
        ("list.active.border", b[0xD].clone()),
        ("sidebar.background", b[1].clone()),
        ("sidebar.foreground", b[5].clone()),
        ("sidebar.border", b[2].clone()),
        ("sidebar.accent.background", b[2].clone()),
        ("sidebar.accent.foreground", b[6].clone()),
        ("sidebar.primary.background", b[0xD].clone()),
        ("sidebar.primary.foreground", b[0].clone()),
        ("title_bar.background", b[1].clone()),
        ("title_bar.border", b[2].clone()),
        ("status_bar.background", b[1].clone()),
        ("status_bar.border", b[2].clone()),
        ("switch.background", b[3].clone()),
        ("scrollbar.background", a(0, "00")),
        ("scrollbar.thumb.background", b[3].clone()),
        ("scrollbar.thumb.hover.background", b[4].clone()),
        ("skeleton.background", b[1].clone()),
        ("danger.background", b[8].clone()),
        ("warning.background", b[0xA].clone()),
        ("success.background", b[0xB].clone()),
        ("info.background", b[0xC].clone()),
        ("base.red", b[8].clone()),
        ("base.yellow", b[0xA].clone()),
        ("base.green", b[0xB].clone()),
        ("base.cyan", b[0xC].clone()),
        ("base.blue", b[0xD].clone()),
        ("base.magenta", b[0xE].clone()),
    ]
    .into_iter()
    .map(|(key, color)| (key.to_owned(), color.into()))
    .collect();
    let theme = json!({
        "name": name,
        "mode": if dark { "dark" } else { "light" },
        "font.family": "Geist",
        "mono_font.family": "Geist Mono",
        "colors": colors,
    });
    Ok(json!({ "name": name, "themes": [theme] }).to_string())
}

/// Relative luminance of a `#RRGGBB` color, 0 (black) to 1 (white).
fn luminance(hex: &str) -> f32 {
    let channel =
        |i: usize| u8::from_str_radix(&hex[1 + i * 2..3 + i * 2], 16).unwrap_or(0) as f32 / 255.0;
    0.2126 * channel(0) + 0.7152 * channel(1) + 0.0722 * channel(2)
}

#[cfg(test)]
mod tests {
    use super::{BASE16, base16};

    #[test]
    fn bundled_schemes_convert() {
        for yaml in BASE16 {
            let set: serde_json::Value = serde_json::from_str(&base16(yaml).unwrap()).unwrap();
            assert_eq!(
                set["themes"][0]["colors"]["background"]
                    .as_str()
                    .unwrap()
                    .len(),
                7
            );
        }
    }

    #[test]
    fn old_flat_format_and_variant_guess() {
        let mut yaml = String::from("scheme: \"Old\"\nauthor: \"x\"\n");
        for i in 0..16 {
            yaml += &format!(
                "base{i:02X}: \"{}\"\n",
                if i == 0 { "fafafa" } else { "101010" }
            );
        }
        let set: serde_json::Value = serde_json::from_str(&base16(&yaml).unwrap()).unwrap();
        assert_eq!(set["themes"][0]["name"], "Old");
        assert_eq!(set["themes"][0]["mode"], "light");
        assert!(base16("name: x\npalette:\n  base00: \"#zzzzzz\"\n").is_err());
    }
}
