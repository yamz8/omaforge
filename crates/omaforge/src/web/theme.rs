//! Colors for the UI, taken from an Omarchy `colors.toml` and exposed as CSS custom
//! properties (`accent` becomes `--accent`, `dark_background` becomes `--dark-background`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const BUILTIN_DARK: &str = include_str!("../../assets/themes/tokyo-night.toml");
const BUILTIN_LIGHT: &str = include_str!("../../assets/themes/catppuccin-latte.toml");

#[derive(Debug, Clone)]
pub enum Theme {
    /// Tokyo Night, or Catppuccin Latte when the browser prefers light.
    Builtin,
    /// A `colors.toml`, re-read on every request so `omarchy theme set` shows up on reload.
    File(PathBuf),
}

impl Theme {
    /// `builtin`, `omarchy` (the active Omarchy theme of the user running omaforge), or a
    /// path to a `colors.toml`. Without a choice, follow Omarchy when it's installed.
    pub fn from_arg(arg: Option<&str>) -> Result<Self> {
        match arg {
            Some("builtin") => Ok(Self::Builtin),
            Some("omarchy") => omarchy_colors()
                .map(Self::File)
                .context("no active Omarchy theme found"),
            Some(path) => Ok(Self::File(path.into())),
            None => Ok(omarchy_colors().map_or(Self::Builtin, Self::File)),
        }
    }

    pub fn css(&self) -> String {
        let builtin = || {
            format!(
                ":root {{\n{}}}\n@media (prefers-color-scheme: light) {{\n  :root {{\n{}  }}\n}}\n",
                variables(BUILTIN_DARK).unwrap_or_default(),
                variables(BUILTIN_LIGHT).unwrap_or_default(),
            )
        };
        match self {
            Self::Builtin => builtin(),
            Self::File(path) => match std::fs::read_to_string(path)
                .map_err(anyhow::Error::from)
                .and_then(|toml| variables(&toml))
            {
                Ok(vars) => format!(":root {{\n{vars}}}\n"),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "can't read theme, using built-in colors");
                    builtin()
                }
            },
        }
    }
}

fn omarchy_colors() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join(".local/state/omarchy/current/theme/colors.toml");
    path.is_file().then_some(path)
}

/// CSS declarations for every color in a `colors.toml`, plus `color-scheme` from `mode`.
fn variables(toml: &str) -> Result<String> {
    let table: toml::Table = toml.parse().context("parsing theme")?;
    let mut css = String::new();
    for (key, value) in &table {
        let Some(value) = value.as_str() else {
            continue;
        };
        if key == "mode" && matches!(value, "dark" | "light") {
            css.push_str(&format!("  color-scheme: {value};\n"));
        } else if is_color(value) && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            css.push_str(&format!("  --{}: {value};\n", key.replace('_', "-")));
        }
    }
    Ok(css)
}

/// Only plain hex colors make it into the stylesheet.
fn is_color(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_variables() {
        let css = variables(
            "mode = \"dark\"\naccent = \"#7aa2f7\"\ndark_background = \"#13141c\"\nevil = \"red;} body{display:none\"\n",
        )
        .unwrap();
        assert!(css.contains("color-scheme: dark;"));
        assert!(css.contains("--accent: #7aa2f7;"));
        assert!(css.contains("--dark-background: #13141c;"));
        assert!(!css.contains("evil"));
    }

    #[test]
    fn builtin_has_both_schemes() {
        let css = Theme::Builtin.css();
        assert!(css.contains("--accent: #7aa2f7;"));
        assert!(css.contains("prefers-color-scheme: light"));
        assert!(css.contains("--accent: #1e66f5;"));
    }
}
