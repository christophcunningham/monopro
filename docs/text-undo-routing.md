# Text Undo routing — review item 8

Implemented locally on 2026-09-20.

While a text editor has focus, keyboard Undo and Redo now remain with that editor and are excluded from photograph-history dispatch. The original guard suppressed ordinary typing keys but allowed Command/Ctrl+Z to reach the photograph before the editor was drawn.

Native-menu Undo/Redo commands are also routed to the focused editor. Because macOS can consume the keyboard accelerator before egui receives it, a menu-only command is translated into the editor's equivalent key event. If that event already arrived from the keyboard, it is not injected again. With no text editor focused, photograph Undo/Redo follows the existing path.

The guard uses actual text-edit focus, so merely focusing a button or slider does not disable photograph history. Other command shortcuts retain their existing behavior.

## Verification

Application suite: **524 passed, 0 failed, 1 ignored**. Workspace Clippy with warnings denied, formatting and whitespace checks passed.

Automated egui tests follow the application's dispatch-before-editor order. They type text, undo it, redo it, and verify that no photograph-history action escapes. They cover Command and Ctrl modifier conventions, menu-only commands, simultaneous menu/keyboard delivery, and Undo with an empty text history. Separate checks confirm photograph Undo and Redo remain available without text focus.

Native AppKit menu interaction and Windows hardware were not exercised manually; menu-command routing and both modifier conventions were tested through the shared event path.
