# Themes

Glass colour themes are JSON documents loaded from a per-user
**Themes** directory. They define every palette colour the UI
uses, plus five **window-tint** colours that can vary per-window
so multiple Glass windows are visually distinguishable at a
glance.

## Where themes live

The Themes directory sits next to `settings.json` and
`glass.redb`:

- **macOS** — `~/Library/Application Support/Glass/Themes/`
- **Linux** — `~/.local/share/Glass/Themes/`

Glass scans every `*.json` in this directory on startup and
merges them with the two built-in themes (`Glass Dark` and
`Glass High Contrast`). A user theme whose `name` matches a
built-in **overrides** the built-in.

On first launch Glass creates the Themes directory and drops the
bundled reference themes (`glass-dark.json`, `sepia.json`) inside
so you have something concrete to edit. After that, the
directory is yours — Glass never re-seeds, so deleting one of
the seeded files (or replacing it with an edited copy) is
permanent.

## Choosing a theme

Use **View → Theme** in the app menu and pick one. The choice
takes effect immediately in every open window and is persisted
for the next launch. The active theme is marked with a leading
"●" in the menu.

If you prefer to set it from the command line, edit
`~/Library/Application Support/Glass/settings.json` (or the
Linux equivalent) and add a `theme` field:

```json
{
  "theme": "Glass High Contrast",
  "bounds": { "x": 100, "y": 100, "width": 1400, "height": 900 }
}
```

Restart Glass to see the change.

## Per-window tint

Each Glass window can pick one of the theme's **five window
tints** as its background — useful when you have multiple
reverse-engineering projects open side by side. Click one of
the five swatches in the window header to set the tint for the
current bundle; the choice is persisted in `glass.redb`
against the bundle's content hash, so closing and re-opening
that bundle restores the same colour.

Slot 0 is always the theme's neutral baseline (matches
`shell.bg`). Slots 1-4 are subtle hue shifts — kept within
about 10/255 of the baseline so the rest of the chrome still
reads correctly.

## JSON shape

Every field is optional. Anything you omit inherits from the
built-in default, so a minimal theme can just override the
shell:

```json
{
  "name": "Sepia",
  "description": "Warm low-contrast palette for long sessions.",
  "shell": {
    "bg":   "#2a221a",
    "panel": "#332a20",
    "text": "#e6d8c2",
    "accent": "#c98a3e"
  },
  "window_tints": [
    "#2a221a", "#2a261a", "#2a221e", "#26221a", "#2a1a22"
  ]
}
```

Colours accept either `"#rrggbb"`, `"#rrggbbaa"`, or a numeric
`0xRRGGBB` / `0xRRGGBBAA`.

The full set of fields, with their built-in defaults, lives in
`crates/glass-ui/src/theme.rs`. The top-level groups are:

| Group | What it colours |
|---|---|
| `shell` | Root background, panels, borders, primary/secondary text, accent |
| `disasm` | AArch64 listing syntax: address, bytes, mnemonic, registers, etc. |
| `hex` | Hex view: bytes, selection, error text, input fields |
| `cfg` | CFG block fills + borders |
| `smali` | Smali syntax: directives, labels, strings, types |
| `state` | Selected row, staged-edit indicators |
| `modals` | Dialog overlays, command-palette row hover/selection |
| `errors` | Error icons, warning highlights, severe states |
| `hovers` | Hover backgrounds for chrome elements |
| `refs` | DEX/xref link colours, edit-mode indicator |
| `window_tints` | Five entries — see "Per-window tint" above |

## Authoring tips

- Start from one of the built-ins. The fastest way is to copy
  `docs/themes/glass-dark.json` (shipped in this repo as a
  full reference) into your Themes dir and tweak from there.
- Keep `window_tints[0]` equal to `shell.bg` so slot 0 is the
  baseline "no tint" choice.
- Avoid making the four accent tints too saturated. The goal
  is "this is window 3, not window 1," not "this is the green
  window" — subtlety reads better at a glance over many hours.
- Glass reloads themes on startup only. After editing the
  JSON, restart Glass to see the result.
