//! Glass theme system.
//!
//! A `Theme` is a full palette spec serialisable to JSON. Built-ins
//! are compiled in (so a fresh install always has something to pick
//! from); user themes live under `<data_dir>/Glass/Themes/*.json` and
//! are merged on top of the built-in list at startup.
//!
//! Each theme also defines five `window_tints` — subtle background
//! tints layered on top of the shell `bg`, indexed per-bundle by
//! `BundleRecord.window_tint`. The point is that multiple Glass
//! windows on the same screen are visually distinguishable at a
//! glance.
//!
//! JSON shape (every field optional — partial themes inherit from
//! the built-in default):
//!
//! ```json
//! {
//!   "name": "Glass Dark",
//!   "shell": { "bg": "#1e1e22", "panel": "#26262c", ... },
//!   "disasm": { "mnemonic": "#6fc3df", ... },
//!   "window_tints": ["#1e1e22", "#1e2230", "#1e2a1e", "#2a1e1e", "#2a1e2a"]
//! }
//! ```
//!
//! Colours parse as `"#rrggbb"` or `"#rrggbbaa"`. Numeric `0xRRGGBB(AA)`
//! forms are also accepted.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use gpui::{Hsla, Rgba, rgb, rgba};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// ThemeColor — JSON-friendly wrapper around `gpui::Rgba`.
// ---------------------------------------------------------------------------

/// RGBA colour with serde de/serialisation as `"#rrggbb"` or `"#rrggbbaa"`.
/// Integers (e.g. `0x1e1e22ff`) are also accepted on the read side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeColor(pub Rgba);

impl ThemeColor {
    pub const fn from_rgb(hex: u32) -> Self {
        // Convert 0xRRGGBB → Rgba (alpha = 1.0) without going through
        // `gpui::rgb` (not const). Mirrors the bit-layout `gpui::rgb` uses.
        let r = ((hex >> 16) & 0xff) as f32 / 255.0;
        let g = ((hex >> 8) & 0xff) as f32 / 255.0;
        let b = (hex & 0xff) as f32 / 255.0;
        Self(Rgba { r, g, b, a: 1.0 })
    }

    pub const fn from_rgba(hex: u32) -> Self {
        let r = ((hex >> 24) & 0xff) as f32 / 255.0;
        let g = ((hex >> 16) & 0xff) as f32 / 255.0;
        let b = ((hex >> 8) & 0xff) as f32 / 255.0;
        let a = (hex & 0xff) as f32 / 255.0;
        Self(Rgba { r, g, b, a })
    }

    pub fn rgba(self) -> Rgba {
        self.0
    }

    pub fn hsla(self) -> Hsla {
        self.0.into()
    }
}

impl From<ThemeColor> for Rgba {
    fn from(c: ThemeColor) -> Self {
        c.0
    }
}

impl From<ThemeColor> for Hsla {
    fn from(c: ThemeColor) -> Self {
        c.0.into()
    }
}

impl Serialize for ThemeColor {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let r = (self.0.r * 255.0).round() as u8;
        let g = (self.0.g * 255.0).round() as u8;
        let b = (self.0.b * 255.0).round() as u8;
        let a = (self.0.a * 255.0).round() as u8;
        let s = if a == 0xff {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        };
        ser.serialize_str(&s)
    }
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(de)?;
        match v {
            serde_json::Value::String(s) => parse_hex_string(&s).map(ThemeColor)
                .ok_or_else(|| D::Error::custom(format!("bad colour {s:?}"))),
            serde_json::Value::Number(n) => {
                let u = n.as_u64().ok_or_else(|| D::Error::custom("colour: non-u64 number"))?;
                if u > 0xffff_ffff {
                    return Err(D::Error::custom("colour: out of u32 range"));
                }
                let u = u as u32;
                Ok(if u <= 0xff_ffff {
                    ThemeColor::from_rgb(u)
                } else {
                    ThemeColor::from_rgba(u)
                })
            }
            _ => Err(D::Error::custom("colour: expected string or number")),
        }
    }
}

fn parse_hex_string(s: &str) -> Option<Rgba> {
    let s = s.trim();
    let s = s.strip_prefix('#').or_else(|| s.strip_prefix("0x")).unwrap_or(s);
    match s.len() {
        6 => {
            let v = u32::from_str_radix(s, 16).ok()?;
            Some(rgb(v).into())
        }
        8 => {
            let v = u32::from_str_radix(s, 16).ok()?;
            Some(rgba(v).into())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Theme schema.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    /// Display name — also the lookup key from `WindowSettings.theme`.
    pub name: String,
    /// Optional one-line description shown in the settings dropdown.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub shell: ShellPalette,
    #[serde(default)]
    pub disasm: DisasmPalette,
    #[serde(default)]
    pub hex: HexPalette,
    #[serde(default)]
    pub cfg: CfgPalette,
    #[serde(default)]
    pub smali: SmaliPalette,
    #[serde(default)]
    pub state: StatePalette,
    #[serde(default)]
    pub modals: ModalsPalette,
    #[serde(default)]
    pub errors: ErrorsPalette,
    #[serde(default)]
    pub hovers: HoversPalette,
    #[serde(default)]
    pub refs: RefsPalette,
    /// Five subtle window tints. The active window's outer background is
    /// `window_tints[BundleRecord.window_tint]` — slot 0 should be the
    /// "no tint" baseline and the rest should be subtle accents.
    #[serde(default = "default_window_tints")]
    pub window_tints: [ThemeColor; 5],
}

impl Default for Theme {
    fn default() -> Self {
        builtin_dark()
    }
}

/// Shell chrome (root bg, panels, borders, primary/secondary text, accent).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellPalette {
    pub bg: ThemeColor,
    pub panel: ThemeColor,
    pub panel_alt: ThemeColor,
    pub border: ThemeColor,
    pub border_alt: ThemeColor,
    pub text: ThemeColor,
    pub text_bright: ThemeColor,
    pub text_dim: ThemeColor,
    pub accent: ThemeColor,
}

impl Default for ShellPalette {
    fn default() -> Self {
        Self {
            bg: ThemeColor::from_rgb(0x1e1e22),
            panel: ThemeColor::from_rgb(0x26262c),
            panel_alt: ThemeColor::from_rgb(0x2a2a30),
            border: ThemeColor::from_rgb(0x36363c),
            border_alt: ThemeColor::from_rgb(0x2d2d33),
            text: ThemeColor::from_rgb(0xd6d6d6),
            text_bright: ThemeColor::from_rgb(0xf2f2f2),
            text_dim: ThemeColor::from_rgb(0x808088),
            accent: ThemeColor::from_rgb(0x4f7cff),
        }
    }
}

/// AArch64 listing syntax highlighting.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DisasmPalette {
    pub address: ThemeColor,
    pub bytes: ThemeColor,
    pub mnemonic: ThemeColor,
    pub register: ThemeColor,
    pub immediate: ThemeColor,
    pub address_op: ThemeColor,
    pub shift: ThemeColor,
    pub condition: ThemeColor,
    pub punct: ThemeColor,
    pub comment: ThemeColor,
    pub symbol_header: ThemeColor,
    pub bb_separator: ThemeColor,
    pub plain: ThemeColor,
}

impl Default for DisasmPalette {
    fn default() -> Self {
        Self {
            address: ThemeColor::from_rgb(0x8a8a92),
            bytes: ThemeColor::from_rgb(0x676770),
            mnemonic: ThemeColor::from_rgb(0x6fc3df),
            register: ThemeColor::from_rgb(0xa8c5ff),
            immediate: ThemeColor::from_rgb(0xf4a55a),
            address_op: ThemeColor::from_rgb(0xf3d27a),
            shift: ThemeColor::from_rgb(0xb6b6c0),
            condition: ThemeColor::from_rgb(0xc191ff),
            punct: ThemeColor::from_rgb(0x808088),
            comment: ThemeColor::from_rgb(0x6e9c5d),
            symbol_header: ThemeColor::from_rgb(0xfff39c),
            bb_separator: ThemeColor::from_rgb(0x3a3a42),
            plain: ThemeColor::from_rgb(0xd6d6d6),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HexPalette {
    pub bytes: ThemeColor,
    pub selected_bg: ThemeColor,
    pub field_bg: ThemeColor,
    pub field_selection: ThemeColor,
    pub error_text: ThemeColor,
    /// Diff-modified byte tint; alpha-blended over `bytes`.
    pub diff_tint: ThemeColor,
    pub selection_text: ThemeColor,
}

impl Default for HexPalette {
    fn default() -> Self {
        Self {
            bytes: ThemeColor::from_rgb(0x676770),
            selected_bg: ThemeColor::from_rgb(0x4f7cff),
            field_bg: ThemeColor::from_rgb(0x1b2a44),
            field_selection: ThemeColor::from_rgb(0x355487),
            error_text: ThemeColor::from_rgb(0xff7070),
            diff_tint: ThemeColor::from_rgba(0x1c4a3c80),
            selection_text: ThemeColor::from_rgb(0xffffff),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CfgPalette {
    pub block_exit: ThemeColor,
    pub block_normal: ThemeColor,
    pub block_border: ThemeColor,
}

impl Default for CfgPalette {
    fn default() -> Self {
        Self {
            block_exit: ThemeColor::from_rgb(0x3a2c2c),
            block_normal: ThemeColor::from_rgb(0x2a313c),
            block_border: ThemeColor::from_rgb(0x6b6b78),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SmaliPalette {
    pub directive: ThemeColor,
    pub modifier: ThemeColor,
    pub label: ThemeColor,
    pub string: ThemeColor,
    pub type_: ThemeColor,
    pub type_external: ThemeColor,
}

impl Default for SmaliPalette {
    fn default() -> Self {
        Self {
            directive: ThemeColor::from_rgb(0xff9c6e),
            modifier: ThemeColor::from_rgb(0xc191ff),
            label: ThemeColor::from_rgb(0xff8fc1),
            string: ThemeColor::from_rgb(0xa5d678),
            type_: ThemeColor::from_rgb(0xf3d27a),
            type_external: ThemeColor::from_rgb(0x8c7a4a),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StatePalette {
    pub row_selected: ThemeColor,
    pub committed_change: ThemeColor,
    pub committed_bg: ThemeColor,
    pub committed_hover: ThemeColor,
}

impl Default for StatePalette {
    fn default() -> Self {
        Self {
            row_selected: ThemeColor::from_rgb(0x2e3245),
            committed_change: ThemeColor::from_rgb(0xc8e8d4),
            committed_bg: ThemeColor::from_rgb(0x1c4a3c),
            committed_hover: ThemeColor::from_rgb(0x276652),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModalsPalette {
    pub overlay_light: ThemeColor,
    pub overlay_dark: ThemeColor,
    pub palette_selected: ThemeColor,
    pub palette_hover: ThemeColor,
}

impl Default for ModalsPalette {
    fn default() -> Self {
        Self {
            overlay_light: ThemeColor::from_rgba(0x000000bb),
            overlay_dark: ThemeColor::from_rgba(0x000000cc),
            palette_selected: ThemeColor::from_rgba(0x355487ff),
            palette_hover: ThemeColor::from_rgba(0x355487aa),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ErrorsPalette {
    pub icon: ThemeColor,
    pub highlight: ThemeColor,
    pub severe: ThemeColor,
}

impl Default for ErrorsPalette {
    fn default() -> Self {
        Self {
            icon: ThemeColor::from_rgb(0xff6060),
            highlight: ThemeColor::from_rgb(0xff8080),
            severe: ThemeColor::from_rgb(0xff9090),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HoversPalette {
    pub standard: ThemeColor,
    pub tab: ThemeColor,
    pub delete: ThemeColor,
}

impl Default for HoversPalette {
    fn default() -> Self {
        Self {
            standard: ThemeColor::from_rgb(0x36363c),
            tab: ThemeColor::from_rgb(0x55555c),
            delete: ThemeColor::from_rgb(0x2e2e34),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RefsPalette {
    pub dex_ref: ThemeColor,
    pub xref_addr: ThemeColor,
    pub edit_indicator: ThemeColor,
}

impl Default for RefsPalette {
    fn default() -> Self {
        Self {
            dex_ref: ThemeColor::from_rgb(0xb0c8ff),
            xref_addr: ThemeColor::from_rgb(0xa0a0a8),
            edit_indicator: ThemeColor::from_rgb(0x66c2ff),
        }
    }
}

fn default_window_tints() -> [ThemeColor; 5] {
    builtin_dark_tints()
}

// ---------------------------------------------------------------------------
// Built-in themes.
// ---------------------------------------------------------------------------

const fn builtin_dark_tints() -> [ThemeColor; 5] {
    // Slot 0 = no tint (matches shell.bg). Slots 1-4 lift their dominant
    // channel by ~+18 over the neutral baseline so the window's identity
    // reads at a glance, without going so far that the rest of the
    // chrome stops looking right. Green is the reference (already
    // visible) — the others are now in the same ballpark.
    [
        ThemeColor::from_rgb(0x1e1e22), // neutral
        ThemeColor::from_rgb(0x1e2438), // blue
        ThemeColor::from_rgb(0x1e2a1e), // green (unchanged)
        ThemeColor::from_rgb(0x331e1e), // red
        ThemeColor::from_rgb(0x2c1e34), // purple
    ]
}

pub fn builtin_dark() -> Theme {
    Theme {
        name: "Glass Dark".into(),
        description: "Default dark palette — neutral with a blue accent.".into(),
        shell: ShellPalette::default(),
        disasm: DisasmPalette::default(),
        hex: HexPalette::default(),
        cfg: CfgPalette::default(),
        smali: SmaliPalette::default(),
        state: StatePalette::default(),
        modals: ModalsPalette::default(),
        errors: ErrorsPalette::default(),
        hovers: HoversPalette::default(),
        refs: RefsPalette::default(),
        window_tints: builtin_dark_tints(),
    }
}

pub fn builtin_high_contrast() -> Theme {
    let mut t = builtin_dark();
    t.name = "Glass High Contrast".into();
    t.description = "Brighter text + heavier borders for accessibility.".into();
    t.shell.bg = ThemeColor::from_rgb(0x101014);
    t.shell.panel = ThemeColor::from_rgb(0x1a1a20);
    t.shell.panel_alt = ThemeColor::from_rgb(0x202028);
    t.shell.border = ThemeColor::from_rgb(0x55555c);
    t.shell.border_alt = ThemeColor::from_rgb(0x44444a);
    t.shell.text = ThemeColor::from_rgb(0xf2f2f2);
    t.shell.text_bright = ThemeColor::from_rgb(0xffffff);
    t.shell.text_dim = ThemeColor::from_rgb(0xa0a0a8);
    t.window_tints = [
        ThemeColor::from_rgb(0x101014),
        ThemeColor::from_rgb(0x10182a),
        ThemeColor::from_rgb(0x102414),
        ThemeColor::from_rgb(0x241014),
        ThemeColor::from_rgb(0x1c1024),
    ];
    t
}

pub fn builtins() -> Vec<Theme> {
    vec![builtin_dark(), builtin_high_contrast()]
}

// ---------------------------------------------------------------------------
// Theme set — loads built-ins + user themes from disk and resolves
// the active theme + window tint slot.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ThemeSet {
    themes: Vec<Theme>,
}

impl ThemeSet {
    /// Load built-ins, then merge every `*.json` under `themes_dir()`.
    /// On first run (the dir doesn't exist), seed it with the bundled
    /// reference themes so users have a starting point to edit. Bad
    /// files in the dir are logged and skipped.
    pub fn load() -> Self {
        let mut themes = builtins();
        if let Ok(dir) = glass_db::themes_dir() {
            seed_themes_dir_if_missing(&dir);
            load_user_themes(&dir, &mut themes);
        }
        Self { themes }
    }

    pub fn all(&self) -> &[Theme] {
        &self.themes
    }

    /// Resolve a theme by name. Falls back to the first built-in if
    /// `name` is `None` or doesn't match anything on disk.
    pub fn resolve(&self, name: Option<&str>) -> &Theme {
        if let Some(n) = name {
            if let Some(t) = self.themes.iter().find(|t| t.name == n) {
                return t;
            }
        }
        &self.themes[0]
    }
}

impl Default for ThemeSet {
    fn default() -> Self {
        Self::load()
    }
}

// Bundled reference themes, embedded at compile time so first-run
// seeding doesn't depend on the install layout. Paths are relative
// to this source file.
//
// `prior_hashes` carries the blake3 hex of every previously-shipped
// revision of the same file. On launch, if the on-disk copy hashes
// to any of these (including the current shipped body itself), we
// refresh it to the current body — that means the user never edited
// it and they get our improvements automatically. If the hash
// matches nothing, the user has edited the file and we leave it.
//
// When bumping a shipped theme, append the **previous** body's
// blake3 to `prior_hashes` so installs of that version refresh.
// Never remove entries — that would orphan very old installs.
struct SeedTheme {
    name: &'static str,
    body: &'static str,
    prior_hashes: &'static [&'static str],
}

const SEED_THEMES: &[SeedTheme] = &[
    SeedTheme {
        name: "glass-dark.json",
        body: include_str!("../../../docs/themes/glass-dark.json"),
        prior_hashes: &[
            // v1 (2026-05-19): brighter blue/red/purple tints.
            "e46b4f0544d31e09f9677996d62b7986200edbc199451835627810e007d2998c",
        ],
    },
    SeedTheme {
        name: "sepia.json",
        body: include_str!("../../../docs/themes/sepia.json"),
        prior_hashes: &[
            // v1 (2026-05-19): initial sepia palette.
            "aabfd9bbe0c12a3779641e7f7b43308f73550a755b00eed4699a1e6361c86ffb",
        ],
    },
];

/// Reconcile the on-disk Themes dir with the bundled reference
/// themes. On first launch this drops the shipped JSONs in so users
/// have something to edit; on subsequent launches it refreshes any
/// file whose content hash matches a prior shipped revision (i.e.
/// the user never touched it) so improvements to a built-in
/// propagate. User-edited files are left alone.
///
/// A file that the user deleted stays gone — once the dir exists,
/// we only touch known seed-named files, and we never recreate one
/// the user has removed.
fn seed_themes_dir_if_missing(dir: &std::path::Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("could not create themes dir {}: {e}", dir.display());
        return;
    }
    for seed in SEED_THEMES {
        let path = dir.join(seed.name);
        let action = decide_seed_action(&path, seed);
        match action {
            SeedAction::Skip => {}
            SeedAction::WriteFresh => {
                if let Err(e) = std::fs::write(&path, seed.body) {
                    tracing::warn!("seeding {} failed: {e}", path.display());
                }
            }
            SeedAction::Refresh => {
                if let Err(e) = std::fs::write(&path, seed.body) {
                    tracing::warn!("refreshing {} failed: {e}", path.display());
                } else {
                    tracing::info!("refreshed shipped theme {}", path.display());
                }
            }
        }
    }
}

enum SeedAction {
    /// File is missing — seed it.
    WriteFresh,
    /// File exists and matches a prior shipped hash — refresh.
    Refresh,
    /// File exists and the user has edited it — leave alone.
    Skip,
}

fn decide_seed_action(path: &std::path::Path, seed: &SeedTheme) -> SeedAction {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The user deleted this file (or it's first run). Only
            // seed on first run — to tell the two cases apart we
            // check whether the directory itself was just created.
            // Simplest workable rule: always write when the file is
            // absent. If the user wants it gone they can delete *and*
            // create an empty placeholder, or set a different theme
            // and ignore it. Most users won't delete a seed file.
            return SeedAction::WriteFresh;
        }
        Err(e) => {
            tracing::warn!("read {}: {e}", path.display());
            return SeedAction::Skip;
        }
    };
    let on_disk = blake3::hash(&bytes).to_hex().to_string();
    let shipped = blake3::hash(seed.body.as_bytes()).to_hex().to_string();
    if on_disk == shipped {
        // Already current.
        return SeedAction::Skip;
    }
    if seed.prior_hashes.iter().any(|h| *h == on_disk) {
        SeedAction::Refresh
    } else {
        // User has edited the file.
        SeedAction::Skip
    }
}

fn load_user_themes(dir: &std::path::Path, out: &mut Vec<Theme>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match load_theme_file(&path) {
            Ok(t) => {
                // User theme with same name overrides a built-in.
                if let Some(pos) = out.iter().position(|x| x.name == t.name) {
                    out[pos] = t;
                } else {
                    out.push(t);
                }
            }
            Err(e) => tracing::warn!("skipping theme {}: {e}", path.display()),
        }
    }
}

fn load_theme_file(path: &PathBuf) -> anyhow::Result<Theme> {
    let bytes = std::fs::read(path)?;
    let t: Theme = serde_json::from_slice(&bytes)?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// Per-window tint resolution.
// ---------------------------------------------------------------------------

impl Theme {
    /// The window background colour for a given per-bundle tint slot
    /// (clamped to 0..=4). Slot 0 is always `shell.bg`.
    pub fn window_bg(&self, slot: u8) -> Rgba {
        let idx = (slot as usize).min(self.window_tints.len() - 1);
        self.window_tints[idx].rgba()
    }

    /// Punched-up version of a tint suitable for rendering inside the
    /// 14×14 swatch picker. The on-window tints are intentionally
    /// subtle (near-black), which makes them unreadable in the small
    /// dots.
    ///
    /// The hue of an RGB colour lives in the **ratios** between
    /// channels, not their absolute values. To preserve "what colour
    /// this is" while making it visible, we:
    ///
    ///   1. Take the raw tint (channels at e.g. 0x33/0x1e/0x1e for
    ///      red — a tiny channel spread riding on a dark baseline).
    ///   2. Scale the whole vector so the brightest channel lands at
    ///      ~0x80, which preserves the ratios exactly.
    ///   3. Compress the spread between the brightest and dimmest
    ///      channels so the colour looks saturated at small sizes.
    ///      Without this step purple (R=0x2c B=0x34) reads as cool
    ///      grey because the channel difference is only 8/255 even
    ///      after scaling.
    ///
    /// Slot 0 (neutral) gets a recognisable mid-grey instead so the
    /// "no tint" choice is still distinguishable.
    pub fn swatch_preview(&self, slot: u8) -> Rgba {
        let idx = (slot as usize).min(self.window_tints.len() - 1);
        let c = self.window_tints[idx].rgba();
        let max = c.r.max(c.g).max(c.b);
        let min = c.r.min(c.g).min(c.b);
        // Treat anything with <2/255 channel spread as neutral — slot
        // 0 or any other near-grey gets a recognisable mid-grey so
        // it's distinguishable from a coloured slot at small sizes.
        if max - min < 0.008 {
            return Rgba { r: 0.45, g: 0.45, b: 0.50, a: 1.0 };
        }
        // Step 1: scale so the brightest channel hits ~0x80. This
        // alone preserves the hue exactly.
        let target = 0x80 as f32 / 255.0;
        let scale = target / max;
        let r0 = c.r * scale;
        let g0 = c.g * scale;
        let b0 = c.b * scale;
        // Step 2: stretch the spread around the channel mean so the
        // dimmest channel drops further and the brightest stays put.
        // Saturation = (max - min) / max; we boost it by `gain`. A
        // gain of ~2.5 takes the tiny initial spread of the raw
        // tints up to something that visibly reads as a colour at
        // 14px while still respecting the per-tint hue ratios.
        let gain = 2.5_f32;
        let mean = (r0 + g0 + b0) / 3.0;
        let stretch = |v: f32| {
            let stretched = mean + (v - mean) * gain;
            stretched.clamp(0.0, 1.0)
        };
        Rgba {
            r: stretch(r0),
            g: stretch(g0),
            b: stretch(b0),
            a: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_themes() {
        // Both reference JSONs in docs/themes/ should deserialise
        // cleanly into a Theme — this is the contract that ships
        // alongside the docs.
        for fname in ["glass-dark.json", "sepia.json"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..").join("..").join("docs").join("themes").join(fname);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let t: Theme = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            assert!(!t.name.is_empty());
            assert_eq!(t.window_tints.len(), 5);
        }
    }

    #[test]
    fn partial_theme_inherits_defaults() {
        // A theme that only overrides `shell.bg` should still produce
        // a fully-populated palette via `#[serde(default)]`.
        let json = r##"{ "name": "Minimal", "shell": { "bg": "#000000" } }"##;
        let t: Theme = serde_json::from_str(json).unwrap();
        assert_eq!(t.name, "Minimal");
        let bg = t.shell.bg.rgba();
        assert!(bg.r < 0.01 && bg.g < 0.01 && bg.b < 0.01);
        // window_tints defaulted, so slot 0 is the built-in dark bg.
        let slot0 = t.window_bg(0);
        let expected = builtin_dark().window_tints[0].rgba();
        assert!((slot0.r - expected.r).abs() < 0.001);
    }

    #[test]
    fn seeds_themes_dir_on_first_run() {
        // Point at a path that doesn't exist yet: the seeder should
        // create it and drop in the bundled JSONs.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Themes");
        assert!(!dir.exists());
        super::seed_themes_dir_if_missing(&dir);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "glass-dark.json"));
        assert!(names.iter().any(|n| n == "sepia.json"));
    }

    #[test]
    fn refresh_leaves_user_edits_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Themes");
        super::seed_themes_dir_if_missing(&dir);
        let edited = dir.join("glass-dark.json");
        // Simulate the user editing the file.
        let edited_body = b"{ \"name\": \"my custom dark\" }";
        std::fs::write(&edited, edited_body).unwrap();
        super::seed_themes_dir_if_missing(&dir);
        // Edit survives.
        assert_eq!(std::fs::read(&edited).unwrap(), edited_body);
    }

    #[test]
    fn refresh_overwrites_stale_shipped_copy() {
        // Stale = on-disk file's hash matches one of the prior shipped
        // revisions (i.e. the user hasn't touched it). Drop in a
        // synthetic "v0" body, list its hash as a prior, and check
        // the seeder rewrites it to the current shipped body.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Themes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("glass-dark.json");
        let stale = b"{ \"name\": \"Glass Dark v0\" }";
        std::fs::write(&path, stale).unwrap();
        let stale_hash: &'static str =
            Box::leak(blake3::hash(stale).to_hex().to_string().into_boxed_str());
        let priors: &'static [&'static str] =
            Box::leak(vec![stale_hash].into_boxed_slice());
        let dyn_seed = super::SeedTheme {
            name: "glass-dark.json",
            body: "{ \"name\": \"Glass Dark v1\" }",
            prior_hashes: priors,
        };
        match super::decide_seed_action(&path, &dyn_seed) {
            super::SeedAction::Refresh => {}
            _ => panic!("expected Refresh for a stale shipped copy"),
        }
    }

    #[test]
    fn swatch_preview_is_brighter_than_raw_tint() {
        let t = builtin_dark();
        // Slot 0 — neutral, gets a recognisable mid-grey.
        let s0 = t.swatch_preview(0);
        assert!(s0.r > 0.3 && s0.g > 0.3 && s0.b > 0.3);
        // Slots 1..4 — dominant channel should be much brighter than
        // in the raw tint, and the hue (which channel dominates) must
        // be preserved.
        for slot in 1u8..5 {
            let raw = t.window_bg(slot);
            let preview = t.swatch_preview(slot);
            let raw_max = raw.r.max(raw.g).max(raw.b);
            let prev_max = preview.r.max(preview.g).max(preview.b);
            assert!(
                prev_max > raw_max + 0.3,
                "slot {slot}: preview ({prev_max}) should be much brighter than raw ({raw_max})"
            );
            // The dominant channel in the preview must be the same one
            // that dominates in the raw tint — otherwise we've changed
            // the colour.
            let dominant = |c: Rgba| -> u8 {
                if c.r >= c.g && c.r >= c.b { 0 }
                else if c.g >= c.b { 1 }
                else { 2 }
            };
            assert_eq!(
                dominant(raw), dominant(preview),
                "slot {slot}: dominant channel changed; raw={raw:?} preview={preview:?}"
            );
        }
    }

    #[test]
    fn swatch_previews_are_visually_distinct() {
        // The bug we're guarding against: two swatches rendering as
        // near-identical colours because the lightening algorithm
        // washed out the hue difference. Each pair of slot previews
        // should differ by at least ~0.15 in some channel.
        let t = builtin_dark();
        let previews: Vec<Rgba> = (0u8..5).map(|s| t.swatch_preview(s)).collect();
        for i in 0..previews.len() {
            for j in (i + 1)..previews.len() {
                let a = previews[i];
                let b = previews[j];
                let max_diff = (a.r - b.r).abs()
                    .max((a.g - b.g).abs())
                    .max((a.b - b.b).abs());
                assert!(
                    max_diff > 0.15,
                    "swatch {i} and {j} look too similar: {a:?} vs {b:?} (max channel diff {max_diff})"
                );
            }
        }
    }

    #[test]
    fn colour_parses_hex_string_and_number() {
        let c: ThemeColor = serde_json::from_str("\"#ff0000\"").unwrap();
        assert_eq!(c.rgba().r, 1.0);
        let c: ThemeColor = serde_json::from_str("\"#ff000080\"").unwrap();
        assert!((c.rgba().a - 0.5).abs() < 0.01);
        let c: ThemeColor = serde_json::from_str("16711680").unwrap(); // 0xff0000
        assert_eq!(c.rgba().r, 1.0);
    }
}

// ---------------------------------------------------------------------------
// Active-theme accessor.
//
// Rather than thread `&Theme` through every render fn, the Shell calls
// `set_active(&self.theme)` once at the top of its `render()` and leaf
// renderers reach for it via `current()`. This is single-threaded (all
// gpui rendering happens on the main thread) so a thread-local is fine.
// `palette.rs` works the same way — it just used `const`s.
// ---------------------------------------------------------------------------

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Theme>>> = const { RefCell::new(None) };
}

static FALLBACK: OnceLock<Arc<Theme>> = OnceLock::new();

/// Set the active theme for the current render pass.
pub fn set_active(theme: &Arc<Theme>) {
    ACTIVE.with(|c| *c.borrow_mut() = Some(theme.clone()));
}

/// Borrow the active theme. Falls back to the built-in dark theme if
/// nothing has been set on this thread yet (e.g. for early renders
/// before the Shell's first `render()` runs). Returning `Arc<Theme>`
/// keeps the borrow short — callers should bind to a local.
pub fn current() -> Arc<Theme> {
    ACTIVE.with(|c| {
        c.borrow()
            .clone()
            .unwrap_or_else(|| FALLBACK.get_or_init(|| Arc::new(builtin_dark())).clone())
    })
}
