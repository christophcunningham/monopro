# Numeric control increments — review item 6

Implemented locally on 2026-09-20.

## Changes

The shared develop number field previously derived its increment only from the slider range. For narrow ranges, or fields displayed without decimals, an Up/Down step rounded straight back to the old value. Numeric fields now use increments of at least one displayed unit, with larger range-derived increments rounded to a whole displayed unit. The separate slider track keeps its existing behavior.

Examples: Contrast Mask contrast, luminance weights, grain density and gamma step by 0.01; AgX White EV by 0.1; crystal size, grain layers and perspective correction by 1. Exposure retains its 0.04 EV increment.

Contact Sheet grid dimensions, sequence numbers, digit counts and resolution, plus both Rename sequence forms, explicitly step by 1. Previously egui's integer default was 0.25, which rounded away on each key press. Dodge & Burn and curve opacity fields now step by one displayed percentage point instead of half a point.

The shared numeric constructor also clamps values when they are written. This prevents egui 0.35's keyboard path from exposing a value beyond the range for one frame. Typing, dragging and accessibility value changes in these fields pass through the same bounded setter.

## Verification

Automated tests send actual egui input events to the production numeric widgets. They cover the nine representative develop fields above, Up and Down, held-key repeats, integer increments, text replacement followed by nudging, change notifications, and upper/lower bounds. In this egui version, receiving keyboard focus immediately puts the field into text-edit mode, so there is no separate focused display mode.

The full workspace regression suite passed: **1,082 passed, 0 failed, 4 ignored**, including native Metal GPU tests. Workspace Clippy with warnings denied, formatting, and whitespace checks also passed. Windows hardware was not available; the changed numeric behavior is shared across platforms.
