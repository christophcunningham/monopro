# Keyboard shortcuts

Default bindings. `Cmd` means Command on macOS and Control on Windows and Linux.
`Ctrl` means Control on all platforms; `Alt` means Option on macOS.
Press `.` to open the in-app reference.
Letter shortcuts are suppressed while typing into text fields.

## View

| Key | Action |
|---|---|
| `p` | Preview original — no edits |
| `u` | Underexposed overlay |
| `o` | Overexposed overlay |
| `q` | Sensor clipping — green has measured support, yellow is fully censored |
| `b` | Surround — border around the image |
| `y` | False-color exposure map |
| `j` | Cycle JPEG-color and raw-linear previews |
| `Tab` | Hide / show panels |
| `z` | 100% / fit |
| `Cmd+0` | Fit to window |
| `Cmd++` | Zoom in |
| `Cmd+Shift++` | Zoom in |
| `Cmd+−` | Zoom out |

## Navigation

| Key | Action |
|---|---|
| `Ctrl+Tab` | Cycle open image tabs |
| `Ctrl+Shift+Tab` | Cycle open image tabs, backwards |
| `Backtick` | Flick to the previous tab — A/B comparison |
| `l` | Lightbox |
| `e` | Bring the Develop panel forward |
| `,` | Settings |
| `.` | Hotkey HUD |

## Comparison

| Key | Action |
|---|---|
| `k` | Compare viewer |
| `1` | Single view |
| `2` | Compare 2-up |
| `3` | Compare 3-up |
| `4` | Compare 4-up |
| `Cmd+K` | Capture snapshot |
| `v` | Print loupe — Esc to leave |
| `Shift+V` | Toggle loupe Before / After |

## Inspector

| Key | Action |
|---|---|
| `i` | Place value pins; again to stop |
| `Shift+I` | Hide / show value pins |
| `Shift+click` | Delete the pin under the pointer |

## Composition

| Key | Action |
|---|---|
| `c` | Crop — Esc to leave |
| `Cmd+[` | Rotate left |
| `Cmd+]` | Rotate right |

## Dodge / Burn

| Key | Action |
|---|---|
| `d` | Dodge — Esc to leave |
| `x` | Burn — Esc to leave |
| `Shift+D` | Hold to see the dodge map |
| `Shift+X` | Hold to see the burn map |
| `Cmd+D` | New dodge instance (brush open) |
| `Cmd+X` | New burn instance (brush open) |
| `[` | Smaller brush (brush open) |
| `]` | Bigger brush (brush open) |
| `Shift+[` | Harder edge (brush open) |
| `Shift+]` | Softer edge (brush open) |
| `Cmd+[` | Less EV per pass (brush open) |
| `Cmd+]` | More EV per pass (brush open) |
| `Alt+[` | Lower brush opacity (brush open) |
| `Alt+]` | Higher brush opacity (brush open) |

## Lightbox

| Key | Action |
|---|---|
| `Space` | Quick Look selected image |
| `Cmd+Shift+C` | Copy Develop settings |
| `Cmd+Shift+V` | Paste Develop settings |
| `Cmd+Shift+R` | Rename selected file(s) |
| `Cmd+Shift+P` | Contact Sheet |
| `Cmd+1` | One star |
| `Cmd+2` | Two stars |
| `Cmd+3` | Three stars |
| `Cmd+4` | Four stars |
| `Cmd+5` | Five stars |
| `Shift+1` | Magenta color label |
| `Shift+2` | Blue color label |
| `Shift+3` | Green color label |
| `Shift+4` | Yellow color label |
| `Shift+5` | Red color label |

## Files

| Key | Action |
|---|---|
| `Cmd+O` | Open a raw |
| `Cmd+E` | Export |
| `Cmd+Shift+E` | Export proof |
| `Cmd+S` | Save duplicate file |
| `Cmd+D` | Duplicate this tab |
| `Cmd+W` | Close tab |

## Edit

| Key | Action |
|---|---|
| `Cmd+Z` | Undo |
| `Cmd+Shift+Z` | Redo |

## Interaction modes

`Esc` leaves the active tool. In Crop it cancels the pending change; in Dodge / Burn
it restores the brush session to its starting state. `Enter` applies and leaves.
The print loupe closes with either key.

While the brush is open, its shortcuts take precedence over tab duplication and
rotation. `Alt+drag` erases; `Shift+click` draws a straight pass.

Source: [binding table](../crates/raw-app/src/hotkeys.rs).
