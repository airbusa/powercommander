//! The desktop's toolkit draws the controls into images the GUI shows: on
//! KDE the user's Qt style (System Settings' Application Style), through
//! `sc-qtstyle`; on Xfce, MATE and Cinnamon their GTK 3 theme, through
//! `sc-gtkstyle`, which also says the GTK theme's colours; both beside the app.

use std::collections::HashMap;
use std::io;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// An image in device pixels, straight RGBA.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct Drawn {
    /// The style's name: breeze, fusion, …
    pub style: String,
    /// Whether its colours are dark.
    pub dark: bool,
    /// By name, as `checkbox.on.hovered`, `button.default.pressed.left`.
    pub images: HashMap<String, Picture>,
    /// 0xRRGGBB by name: GTK's `theme_bg_color`, …, and `button`, a push button's fill.
    pub colors: HashMap<String, u32>,
}

/// This desktop's helper; none where the app draws the controls itself.
pub fn helper() -> Option<&'static str> {
    match () {
        _ if crate::theme::on_desktop("KDE") => Some("sc-qtstyle"),
        _ if ["XFCE", "MATE", "X-Cinnamon"].into_iter().any(crate::theme::on_desktop) => Some("sc-gtkstyle"),
        _ => None,
    }
}

/// The controls as the toolkit draws them, `scale` device pixels to a pixel,
/// buttons `button_height` pixels high. Blocks, some 70 ms.
pub fn draw(scale: f32, button_height: u32) -> io::Result<Drawn> {
    let drawn = run(&[&scale.to_string(), &button_height.to_string()])?;
    // Qt 6 without the user's style draws Fusion: Plasma 5's styles are Qt 5's.
    if helper() == Some("sc-qtstyle") {
        let chosen = crate::desktop::kde_setting("KDE", "widgetStyle").unwrap_or_else(|| "Breeze".into());
        if !drawn.style.eq_ignore_ascii_case(&chosen) {
            return Err(io::Error::other(format!("sc-qtstyle drew {}, not {chosen}", drawn.style)));
        }
    }
    Ok(drawn)
}

/// The GTK 3 theme's colours, without images, on Xfce, MATE and Cinnamon;
/// asked once, some 40 ms.
pub fn gtk_colors() -> Option<&'static Drawn> {
    static COLORS: OnceLock<Option<Drawn>> = OnceLock::new();
    let ask = || (helper() == Some("sc-gtkstyle")).then(|| run(&["colors"]).ok()).flatten();
    COLORS.get_or_init(ask).as_ref()
}

fn run(args: &[&str]) -> io::Result<Drawn> {
    let name = helper().ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no toolkit to draw the controls"))?;
    let helper = std::env::current_exe()?.with_file_name(name);
    let output = Command::new(&helper).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output();
    // Cargo builds it only where the toolkit's development files are.
    let output = output.map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => io::Error::new(
            err.kind(),
            format!("{} is missing, so the controls are the app's own: cargo builds it once its toolkit's development files are installed", helper.display()),
        ),
        _ => io::Error::new(err.kind(), format!("{name}: {err}")),
    })?;
    if !output.status.success() {
        return Err(io::Error::other(format!("{name}: {}", output.status)));
    }
    parse(&output.stdout)
}

/// Lines, an image's pixels after its line, "end" last.
fn parse(mut data: &[u8]) -> io::Result<Drawn> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("the toolkit's drawing: {what}"));
    let mut drawn = Drawn { style: String::new(), dark: false, images: HashMap::new(), colors: HashMap::new() };
    while let Some(end) = data.iter().position(|&byte| byte == b'\n') {
        let line = std::str::from_utf8(&data[..end]).map_err(|_| bad("not text"))?;
        data = &data[end + 1..];
        let words: Vec<&str> = line.split(' ').collect();
        match words[..] {
            ["style", name] => drawn.style = name.to_owned(),
            ["dark", dark] => drawn.dark = dark == "1",
            ["color", name, rgb] => {
                drawn.colors.insert(name.to_owned(), u32::from_str_radix(rgb, 16).map_err(|_| bad(line))?);
            }
            ["image", name, width, height] => {
                let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>()) else {
                    return Err(bad(line));
                };
                let size = width as usize * height as usize * 4;
                let rgba = data.get(..size).ok_or_else(|| bad("an image cut short"))?.to_vec();
                data = &data[size..];
                drawn.images.insert(name.to_owned(), Picture { width, height, rgba });
            }
            ["end"] => return Ok(drawn),
            _ => {}
        }
    }
    Err(bad("cut short"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_helpers_output() {
        let mut data = b"sc-qtstyle 1\nstyle fusion\ndark 1\ncolor borders 1d1f2a\nimage checkbox.on.normal 2 1\n".to_vec();
        data.extend_from_slice(&[1, 2, 3, 4, 10, 11, 12, 13]);
        data.extend_from_slice(b"end\n");
        let drawn = parse(&data).unwrap();
        assert_eq!((drawn.style.as_str(), drawn.dark), ("fusion", true));
        assert_eq!(drawn.colors["borders"], 0x1d1f2a);
        let picture = &drawn.images["checkbox.on.normal"];
        assert_eq!((picture.width, picture.height, picture.rgba.len()), (2, 1, 8));
        // Cut short: no "end", or fewer pixels than said.
        assert!(parse(&data[..data.len() - 4]).is_err());
        assert!(parse(b"image a 2 2\n\x00\x00end\n").is_err());
    }
}
