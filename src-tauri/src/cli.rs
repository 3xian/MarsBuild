//! Startup flags for window chrome that must be applied before the webview exists.
//!
//! Acrylic + Overlay CSD is shaped for macOS/Windows. On Wayland, WebKitGTK
//! cannot composite an alpha surface (no protocol support), and GTK CSD shows
//! as a black bar. These flags rewrite `tauri.conf.json` window fields in
//! memory so the window is created opaque and undecorated.

use tauri::utils::config::{Color, WindowConfig};
use tauri::TitleBarStyle;

/// Cream fill used by the acrylic tint, with a fully opaque alpha.
const OPAQUE_BACKGROUND: Color = Color(250, 244, 237, 255);

pub const USAGE: &str = "\
PinkCode - desktop GUI for Grok Build

Usage:
  PinkCode [options]

Options:
  --disable-csd             Hide client-side decorations (fixes the black
                            title bar on Wayland compositors such as niri)
  --disable-transparency    Opaque window (WebKitGTK has no Wayland
                            protocol for alpha compositing)
  -h, --help                Show this help
";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub disable_csd: bool,
    pub disable_transparency: bool,
}

pub enum Action {
    Run(Flags),
    Help,
}

impl Flags {
    /// GTK reads `GTK_CSD` at init; `set_decorations(false)` after the window
    /// exists is not enough to stop the black CSD bar.
    pub fn prepare_env(self) {
        if self.disable_csd {
            #[cfg(target_os = "linux")]
            {
                std::env::set_var("GTK_CSD", "0");
            }
        }
    }

    pub fn apply(self, config: &mut tauri::Config) {
        if !self.disable_csd && !self.disable_transparency {
            return;
        }
        for win in &mut config.app.windows {
            self.apply_window(win);
        }
    }

    fn apply_window(self, win: &mut WindowConfig) {
        if self.disable_csd {
            win.decorations = false;
            win.hidden_title = false;
            win.title_bar_style = TitleBarStyle::Visible;
        }
        if self.disable_transparency {
            win.transparent = false;
            win.window_effects = None;
            win.background_color = Some(OPAQUE_BACKGROUND);
        }
    }
}

/// Unknown args are ignored so WebKit / `tauri dev` extra flags still work.
pub fn parse<I, S>(args: I) -> Action
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut flags = Flags::default();
    for arg in args.into_iter().skip(1) {
        match arg.as_ref() {
            "-h" | "--help" => return Action::Help,
            "--disable-csd" => flags.disable_csd = true,
            "--disable-transparency" => flags.disable_transparency = true,
            _ => {}
        }
    }
    Action::Run(flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags_of(args: &[&str]) -> Flags {
        match parse(args.iter().copied()) {
            Action::Run(flags) => flags,
            Action::Help => panic!("expected Run"),
        }
    }

    #[test]
    fn no_args_leaves_defaults() {
        assert_eq!(flags_of(&["PinkCode"]), Flags::default());
    }

    #[test]
    fn each_flag_and_both() {
        assert_eq!(
            flags_of(&["PinkCode", "--disable-csd"]),
            Flags {
                disable_csd: true,
                disable_transparency: false,
            }
        );
        assert_eq!(
            flags_of(&["PinkCode", "--disable-transparency"]),
            Flags {
                disable_csd: false,
                disable_transparency: true,
            }
        );
        assert_eq!(
            flags_of(&["PinkCode", "--disable-csd", "--disable-transparency"]),
            Flags {
                disable_csd: true,
                disable_transparency: true,
            }
        );
    }

    #[test]
    fn help_and_unknown_args() {
        assert!(matches!(parse(["PinkCode", "--help"]), Action::Help));
        assert_eq!(
            flags_of(&["PinkCode", "--something-webkit-passes"]),
            Flags::default()
        );
    }

    #[test]
    fn apply_rewrites_overlay_acrylic_window() {
        let mut win = WindowConfig {
            transparent: true,
            decorations: true,
            hidden_title: true,
            title_bar_style: TitleBarStyle::Overlay,
            window_effects: Some(Default::default()),
            background_color: Some(Color(0, 0, 0, 0)),
            ..Default::default()
        };
        Flags {
            disable_csd: true,
            disable_transparency: true,
        }
        .apply_window(&mut win);
        assert!(!win.decorations);
        assert!(!win.hidden_title);
        assert_eq!(win.title_bar_style, TitleBarStyle::Visible);
        assert!(!win.transparent);
        assert!(win.window_effects.is_none());
        assert_eq!(win.background_color, Some(OPAQUE_BACKGROUND));
    }

    #[test]
    fn apply_is_a_no_op_without_flags() {
        let mut win = WindowConfig {
            transparent: true,
            decorations: true,
            ..Default::default()
        };
        Flags::default().apply_window(&mut win);
        assert!(win.transparent);
        assert!(win.decorations);
    }
}
