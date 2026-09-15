//! Key bindings, as **data**.
//!
//! Twenty-odd bindings, a list of them that has to be shown to the user, and a
//! plausible future of rebinding. Any one of those alone would justify a table; all
//! three make `if i.key_pressed(...)` arms scattered through `ui()` the wrong shape.
//! It is a small refactor now and a large one later.
//!
//! Having them in one place also makes two whole classes of bug checkable rather
//! than discoverable, and both are pinned by tests below:
//!
//! - **Two actions on one chord.** With arms spread through a frame, the second one
//!   silently never fires — and which is second depends on the order somebody happened
//!   to write them in.
//! - **A bare letter eaten by a text field.** Eleven of these are unmodified letters,
//!   and the Settings menu introduces the app's first text inputs. Typing `p` into a
//!   filename must not toggle preview-original. The guard belongs to the dispatcher,
//!   not to each call site that might remember it.
//!
//! # The table can represent an unbuilt action
//!
//! The table was introduced before the view actions existed, so [`Binding::built`]
//! remains part of the model: a future specified chord can report itself as pending
//! rather than fail silently. The view actions that motivated it are now built.

/// Everything a key can ask for.
///
/// One variant per specified action. Keeping specification and dispatch in the same
/// enum makes a future unbuilt feature visible as pending rather than as a dead key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // -- built
    Undo,
    Redo,
    OpenFile,
    Export,
    /// Export Proof — the second of the two buttons in the Info panel's EXPORT block,
    /// and the one whose format, size and space are preferences rather than a target
    /// set per tab. `⌘⇧E` to `⌘E`'s master, which is the pairing every other shifted
    /// chord here uses: same key, same errand, the shifted one being the variant.
    ExportProof,
    DuplicateTab,
    CloseTab,
    SaveDuplicate,
    CycleTabs,
    CycleTabsBack,
    FlickTab,
    ZoomToggle,
    ZoomFit,
    ZoomIn,
    ZoomOut,
    Settings,
    /// Near-full-screen reference drawn over either Develop or Lightbox.
    HotkeyHud,
    // -- View overlays and reference previews
    PreviewOriginal,
    UnderexposedOverlay,
    OverexposedOverlay,
    SensorClipping,
    Surround,
    FalseColour,
    CyclePreviewSource,
    TogglePanels,
    CompareViewer,
    /// How many cells the compare grid shows. **`1` is the single view**, so the key
    /// that says "one picture" is the key that leaves the grid — which makes the run
    /// 1–4 one continuous control rather than a close key and three layout keys.
    ///
    /// A payload rather than four variants, following `Rating(u8)`.
    CompareUp(u8),
    CaptureSnapshot,
    Lightbox,
    Develop,
    ValuePinMode,
    ToggleValuePins,
    Crop,
    Loupe,
    LoupeBeforeAfter,
    /// Copy and paste the complete Develop parameter set between Lightbox images.
    /// Metadata is deliberately excluded; it belongs to the destination file.
    CopySettings,
    PasteSettings,
    /// Rename the selected Lightbox file or batch in visible contact-sheet order.
    RenameFiles,
    /// Open the Contact Sheet PDF dialog for the current Lightbox selection or view.
    ContactSheet,
    Rating(u8),
    ColourLabel(u8),
    // -- Composition
    RotateLeft,
    RotateRight,
    // -- Dodge & Burn
    /// Paint dodges, and open the brush if it is closed.
    Dodge,
    /// Paint burns, and open the brush if it is closed.
    Burn,
    /// Start a *new* dodge or burn instance rather than adding to the current one.
    NewDodge,
    NewBurn,
    /// Hold to see the accumulated dodge or burn map instead of the picture.
    ShowDodgeMap,
    ShowBurnMap,
    /// The four brush controls, `true` for the `]` end of each pair.
    ///
    /// A payload rather than eight variants, following `Rating(u8)`: the pairs are
    /// the same action in two directions, and writing them as eight names makes the
    /// table longer without making it clearer.
    BrushRadius(bool),
    BrushFeather(bool),
    BrushIntensity(bool),
    BrushOpacity(bool),
    /// Leave whatever interaction mode is open. **One key for all of them**, which
    /// is the affordance that makes modes survivable: the honest cost of a mode is
    /// that it is invisible until it surprises you, and the answer is that there is
    /// exactly one way out and it is the one every application uses.
    ///
    /// Not in `TABLE`. Escape is not a binding a user configures or looks up, it is
    /// a property of being in a mode, and putting it in the table would list it once
    /// under Composition as though it belonged to crop alone. It is dispatched
    /// directly, and only when a mode is open — so it stays available to egui for
    /// closing the Settings window the rest of the time.
    ExitMode,
    /// Apply and leave. `↵`, and not in `TABLE` for the same reason `ExitMode` is
    /// not: it is a property of being in a mode rather than a binding.
    CommitMode,
}

/// Which modifiers a binding wants, matched **exactly**.
///
/// Exactly, because `⌘E` (export) and `E` (develop) are different bindings, as are
/// `⌘O` and `O`. A subset match would fire both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mods {
    None,
    Cmd,
    Shift,
    CmdShift,
    /// Option. Added for the brush's opacity pair, which the
    /// prototype puts on `⌥[` / `⌥]`.
    ///
    /// Until then `matches` refused *any* binding while Alt was down, on the
    /// reasoning that no binding wanted it — which was true and is the thing that
    /// made adding one a change to the matcher rather than a row in the table.
    Alt,
    /// Control, and on macOS that is genuinely Control rather than Command.
    ///
    /// Added for `⌃Tab` / `⌃⇧Tab`, the tab strip. **`⌘\`` was the specified chord and
    /// it cannot work**: macOS reserves it as "Move focus to next window" — symbolic
    /// hotkey 27, on by default — and the system takes the event before the app or its
    /// menu bar ever sees it. Nothing in `TABLE`, `menu.rs` or `Tabs::step` was wrong;
    /// the key simply never arrived. the maintainer chose `⌃Tab`, which is the other
    /// cross-platform convention for walking a tab strip and is not reserved.
    ///
    /// Bare backtick still flicks A/B — see `docs/decisions.md`, "Both tab keys stay".
    /// That half of the decision is untouched and is the half that was working.
    Ctrl,
    CtrlShift,
}

impl Mods {
    fn matches(self, m: &egui::Modifiers) -> bool {
        let (cmd, shift, alt) = (m.command, m.shift, m.alt);
        // **`ctrl` here has to exclude Command, and `mac_cmd` is what does it.** egui
        // sets `command` to Cmd on macOS and to Ctrl everywhere else, which is what
        // makes `Mods::Cmd` one row for both platforms — but it also means `m.ctrl` is
        // true on Windows for the chord `Mods::Cmd` already claims. `mac_cmd` is Cmd on
        // macOS and always false elsewhere, so `ctrl && !mac_cmd` is real Control on
        // macOS and plain Ctrl on Windows, which is what `⌃Tab` should be on each.
        let ctrl = m.ctrl && !m.mac_cmd;
        // Exact on all of them, so `⌥]` and `]` are different bindings rather than
        // one binding that also fires with Option held.
        match self {
            Self::None => !cmd && !shift && !alt && !ctrl,
            Self::Cmd => cmd && !shift && !alt,
            Self::Shift => !cmd && shift && !alt && !ctrl,
            Self::CmdShift => cmd && shift && !alt,
            Self::Alt => !cmd && !shift && alt,
            Self::Ctrl => ctrl && !shift && !alt,
            Self::CtrlShift => ctrl && shift && !alt,
        }
    }

    /// Whether a binding is something a user could be *typing*.
    ///
    /// Bare and shifted keys are; anything with command or control is not. This is what
    /// decides which bindings a focused text field suppresses.
    fn is_typing(self) -> bool {
        matches!(self, Self::None | Self::Shift)
    }

    pub fn glyphs(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Cmd => "⌘",
            Self::Shift => "⇧",
            Self::CmdShift => "⌘⇧",
            Self::Alt => "⌥",
            Self::Ctrl => "⌃",
            Self::CtrlShift => "⌃⇧",
        }
    }
}

/// How the reference groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    View,
    Navigation,
    Comparison,
    ValuePins,
    Composition,
    DodgeBurn,
    Lightbox,
    Files,
    Edit,
}

impl Group {
    pub const ORDER: [Self; 9] = [
        Self::View,
        Self::Navigation,
        Self::Comparison,
        Self::ValuePins,
        Self::Composition,
        Self::DodgeBurn,
        Self::Lightbox,
        Self::Files,
        Self::Edit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::View => "VIEW",
            Self::Navigation => "NAVIGATION",
            Self::Comparison => "COMPARISON",
            // The panel is the INSPECTOR and the marks it places are pins — the maintainer's
            // rename, and the distinction is kept: the group is named for the module
            // so the reference reads the way the panel does, while `i` and `⇧I` keep
            // saying "pin" because that is what they place.
            Self::ValuePins => "INSPECTOR",
            Self::Composition => "COMPOSITION",
            Self::DodgeBurn => "DODGE / BURN",
            Self::Lightbox => "LIGHTBOX",
            Self::Files => "FILES",
            Self::Edit => "EDIT",
        }
    }
}

/// A reference entry that is a pointer or mode gesture rather than a dispatchable
/// key binding.
///
/// These belong beside [`TABLE`] because Settings and the HUD must agree about them,
/// but not *inside* it: Shift+Click cannot be represented by `egui::Key`, and Space
/// quick look is consumed by Lightbox's raw-input hook so it cannot press a focused
/// egui control first.
pub struct ReferenceGesture {
    pub chord: &'static str,
    pub what: &'static str,
    pub group: Group,
    /// Draw before the group's key bindings. Quick Look is Lightbox's primary viewing
    /// gesture; pin deletion is a follow-up to placing and hiding pins.
    pub before_bindings: bool,
}

pub const REFERENCE_GESTURES: &[ReferenceGesture] = &[
    ReferenceGesture {
        chord: "space",
        what: "Quick Look selected image",
        group: Group::Lightbox,
        before_bindings: true,
    },
    ReferenceGesture {
        chord: "⇧Click",
        what: "Delete Inspector pin",
        group: Group::ValuePins,
        before_bindings: false,
    },
];

pub fn reference_gestures(
    group: Group,
    before_bindings: bool,
) -> impl Iterator<Item = &'static ReferenceGesture> {
    REFERENCE_GESTURES
        .iter()
        .filter(move |gesture| gesture.group == group && gesture.before_bindings == before_bindings)
}

pub struct Binding {
    pub action: Action,
    pub key: egui::Key,
    pub mods: Mods,
    /// The key as a user reads it, without modifiers — those come from `mods`.
    pub key_label: &'static str,
    pub what: &'static str,
    pub group: Group,
    /// False while nothing implements the action. Shown as pending rather than
    /// hidden; see the module note.
    pub built: bool,
    /// This binding fires **only while an interaction mode is open**, and takes its
    /// chord back from whatever holds it the rest of the time.
    ///
    /// Milestone 10 is what made this a concept rather than a special case. The
    /// prototype's brush wants `⌘[` and `⌘]` for intensity, and 9a had already
    /// given them to Rotate left and right; it wants `⌘D` for a new dodge instance,
    /// and `⌘D` duplicates a tab. Three chords, all genuinely wanted by both sides,
    /// and none of them a collision in practice — you do not rotate the picture or
    /// duplicate the tab in the middle of a brush stroke.
    ///
    /// So the resolution is stated once, here, rather than discovered when a rotate
    /// fires mid-stroke: **a modal binding shadows a global one while its mode is
    /// open.** `pressed` implements it, `the_shadowed_chords_are_exactly_these`
    /// pins which chords it applies to, and the mode's own footer line is what tells
    /// the user — because a key whose meaning depends on a mode has to be
    /// documented by the mode.
    pub modal: bool,
}

impl Binding {
    /// `⌘⇧Z`, `` ` ``, `1`.
    pub fn chord(&self) -> String {
        format!("{}{}", self.mods.glyphs(), self.key_label)
    }
}

use Group::*;
use Mods::*;
use egui::Key;

/// The whole binding set. Specified by the maintainer, 2026-07-26; see `docs/hotkeys.md`.
pub const TABLE: &[Binding] = &[
    // -- View
    b(
        Action::PreviewOriginal,
        Key::P,
        None,
        "p",
        "Preview original — no edits",
        View,
        true,
    ),
    b(
        Action::UnderexposedOverlay,
        Key::U,
        None,
        "u",
        "Underexposed overlay",
        View,
        true,
    ),
    b(
        Action::OverexposedOverlay,
        Key::O,
        None,
        "o",
        "Overexposed overlay",
        View,
        true,
    ),
    b(
        Action::SensorClipping,
        Key::Q,
        None,
        "q",
        "Sensor clipping — green has measured support, yellow is fully censored",
        View,
        true,
    ),
    b(
        Action::Surround,
        Key::B,
        None,
        "b",
        "Surround — border around the image",
        View,
        true,
    ),
    b(
        Action::FalseColour,
        Key::Y,
        None,
        "y",
        "False-color exposure map",
        View,
        true,
    ),
    b(
        Action::CyclePreviewSource,
        Key::J,
        None,
        "j",
        "Cycle JPEG-color and raw-linear previews",
        View,
        true,
    ),
    b(
        Action::TogglePanels,
        Key::Tab,
        None,
        "tab",
        "Hide / show panels",
        View,
        true,
    ),
    b(
        Action::ZoomToggle,
        Key::Z,
        None,
        "z",
        "100% / fit",
        View,
        true,
    ),
    b(
        Action::ZoomFit,
        Key::Num0,
        Cmd,
        "0",
        "Fit to window",
        View,
        true,
    ),
    b(Action::ZoomIn, Key::Equals, Cmd, "+", "Zoom in", View, true),
    // `+` and `=` are one key. egui names it `Plus` when shift is down and `Equals`
    // when it is not, so zooming in needs both or it works only without shift.
    b(
        Action::ZoomIn,
        Key::Plus,
        CmdShift,
        "+",
        "Zoom in",
        View,
        true,
    ),
    b(
        Action::ZoomOut,
        Key::Minus,
        Cmd,
        "−",
        "Zoom out",
        View,
        true,
    ),
    // -- Navigation
    // **`⌃Tab`, not `⌘\``.** The chord the spec named is one macOS keeps for itself —
    // "Move focus to next window", symbolic hotkey 27 — so it never reached the app and
    // cycling appeared unimplemented while every part of it was in fact wired. See
    // `Mods::Ctrl`. Tab is safe to overload here: `raw_input_hook` steals only the
    // *bare* key, and egui's focus navigation claims Tab only with no modifiers or with
    // shift alone, so neither of these two is spent before the table sees it.
    b(
        Action::CycleTabs,
        Key::Tab,
        Ctrl,
        "tab",
        "Cycle open image tabs",
        Navigation,
        true,
    ),
    b(
        Action::CycleTabsBack,
        Key::Tab,
        CtrlShift,
        "tab",
        "Cycle open image tabs, backwards",
        Navigation,
        true,
    ),
    b(
        Action::FlickTab,
        Key::Backtick,
        None,
        "`",
        "Flick to the previous tab — A/B comparison",
        Navigation,
        true,
    ),
    b(
        Action::Lightbox,
        Key::L,
        None,
        "l",
        "Lightbox",
        Navigation,
        true,
    ),
    b(
        Action::Develop,
        Key::E,
        None,
        "e",
        "Bring the Develop panel forward",
        Navigation,
        true,
    ),
    b(
        Action::Settings,
        Key::Comma,
        None,
        ",",
        "Settings",
        Navigation,
        true,
    ),
    b(
        Action::HotkeyHud,
        Key::Period,
        None,
        ".",
        "Hotkey HUD",
        Navigation,
        true,
    ),
    // -- Comparison
    b(
        Action::CompareViewer,
        Key::K,
        None,
        "k",
        "Compare viewer",
        Comparison,
        true,
    ),
    // **Bare digits, and they were free.** Ratings took `⌘1`–`⌘5` and colour labels
    // `⇧1`–`⇧5`, both deliberately leaving the unmodified run alone. `1` returns to
    // the ordinary viewer; `2`–`4` select and, when needed, reopen the corresponding
    // snapshot layout, so the four keys behave as one continuous control.
    b(
        Action::CompareUp(1),
        Key::Num1,
        None,
        "1",
        "Single view",
        Comparison,
        true,
    ),
    b(
        Action::CompareUp(2),
        Key::Num2,
        None,
        "2",
        "Compare 2-up",
        Comparison,
        true,
    ),
    b(
        Action::CompareUp(3),
        Key::Num3,
        None,
        "3",
        "Compare 3-up",
        Comparison,
        true,
    ),
    b(
        Action::CompareUp(4),
        Key::Num4,
        None,
        "4",
        "Compare 4-up",
        Comparison,
        true,
    ),
    b(
        Action::CaptureSnapshot,
        Key::K,
        Cmd,
        "k",
        "Capture snapshot",
        Comparison,
        true,
    ),
    // A loupe compares the print-scale rendering to the picture around it. It is a
    // viewing comparison, not a composition edit, even though its reticle is dragged.
    b(
        Action::Loupe,
        Key::V,
        None,
        "v",
        "Print loupe — Esc to leave",
        Comparison,
        true,
    ),
    b(
        Action::LoupeBeforeAfter,
        Key::V,
        Shift,
        "v",
        "Toggle loupe Before / After",
        Comparison,
        true,
    ),
    // -- Value pins
    b(
        Action::ValuePinMode,
        Key::I,
        None,
        "i",
        "Place value pins; again to stop",
        ValuePins,
        true,
    ),
    b(
        Action::ToggleValuePins,
        Key::I,
        Shift,
        "i",
        "Hide / show value pins",
        ValuePins,
        true,
    ),
    // -- Composition
    b(
        Action::Crop,
        Key::C,
        None,
        "c",
        "Crop — Esc to leave",
        Composition,
        true,
    ),
    // `⌘[` and `⌘]`, the convention every editor uses for this.
    //
    // Not the bare bracket pair, though bare letters are this app's dominant idiom:
    // `[` and `]` are the brush-radius keys while painting, and Dodge & Burn is the
    // next one along. A binding that has to be taken back is worse than one that
    // was never the obvious choice. `⌘⇧[` / `⌘⇧]` already walk the tab strip, and
    // these two are their unshifted neighbours, which is how the pair reads.
    b(
        Action::RotateLeft,
        Key::OpenBracket,
        Cmd,
        "[",
        "Rotate left",
        Composition,
        true,
    ),
    b(
        Action::RotateRight,
        Key::CloseBracket,
        Cmd,
        "]",
        "Rotate right",
        Composition,
        true,
    ),
    // -- Dodge & burn
    //
    // **The bracket pairs are NOT here, and that is Dodge & Burn's decision.**
    // The prototype wants `[`/`]` for radius, `⌘[`/`⌘]` for intensity, `⇧[`/`⇧]` for
    // feather and `⌥[`/`⌥]` for opacity. Two of those four could not be global: `⌘[`
    // and `⌘]` are Rotate left and right, and `Mods` has no Alt variant. Binding two
    // pairs here and two modally would put one tool's controls in two places under
    // two rules.
    //
    // So all four are **modal**, live only while the brush is open, and are handled
    // by `paint::Brush::adjust`. The mode's footer line documents them, which is
    // where a modal key belongs — and `the_rotate_pair_leaves_the_bare_brackets_free`
    // fails if one is ever added back.
    b(
        Action::Dodge,
        Key::D,
        None,
        "d",
        "Dodge — Esc to leave",
        DodgeBurn,
        true,
    ),
    b(
        Action::Burn,
        Key::X,
        None,
        "x",
        "Burn — Esc to leave",
        DodgeBurn,
        true,
    ),
    b(
        Action::ShowDodgeMap,
        Key::D,
        Shift,
        "d",
        "Hold to see the dodge map",
        DodgeBurn,
        true,
    ),
    b(
        Action::ShowBurnMap,
        Key::X,
        Shift,
        "x",
        "Hold to see the burn map",
        DodgeBurn,
        true,
    ),
    // Modal: these six live only while the brush is open. Three of them are chords
    // something else holds the rest of the time — see `Binding::modal`.
    m(
        Action::NewDodge,
        Key::D,
        Cmd,
        "d",
        "New dodge instance",
        DodgeBurn,
    ),
    m(
        Action::NewBurn,
        Key::X,
        Cmd,
        "x",
        "New burn instance",
        DodgeBurn,
    ),
    m(
        Action::BrushRadius(false),
        Key::OpenBracket,
        None,
        "[",
        "Smaller brush",
        DodgeBurn,
    ),
    m(
        Action::BrushRadius(true),
        Key::CloseBracket,
        None,
        "]",
        "Bigger brush",
        DodgeBurn,
    ),
    m(
        Action::BrushFeather(false),
        Key::OpenBracket,
        Shift,
        "[",
        "Harder edge",
        DodgeBurn,
    ),
    m(
        Action::BrushFeather(true),
        Key::CloseBracket,
        Shift,
        "]",
        "Softer edge",
        DodgeBurn,
    ),
    m(
        Action::BrushIntensity(false),
        Key::OpenBracket,
        Cmd,
        "[",
        "Less EV per pass",
        DodgeBurn,
    ),
    m(
        Action::BrushIntensity(true),
        Key::CloseBracket,
        Cmd,
        "]",
        "More EV per pass",
        DodgeBurn,
    ),
    m(
        Action::BrushOpacity(false),
        Key::OpenBracket,
        Alt,
        "[",
        "Lower brush opacity",
        DodgeBurn,
    ),
    m(
        Action::BrushOpacity(true),
        Key::CloseBracket,
        Alt,
        "]",
        "Higher brush opacity",
        DodgeBurn,
    ),
    // -- Lightbox
    // Ratings and color labels operate on the Lightbox selection, never on an open
    // Develop tab. Keeping them with file commands made the reference imply otherwise.
    b(
        Action::CopySettings,
        Key::C,
        CmdShift,
        "c",
        "Copy Develop settings",
        Lightbox,
        true,
    ),
    b(
        Action::PasteSettings,
        Key::V,
        CmdShift,
        "v",
        "Paste Develop settings",
        Lightbox,
        true,
    ),
    b(
        Action::RenameFiles,
        Key::R,
        CmdShift,
        "r",
        "Rename selected file(s)",
        Lightbox,
        true,
    ),
    b(
        Action::ContactSheet,
        Key::P,
        CmdShift,
        "p",
        "Contact Sheet",
        Lightbox,
        true,
    ),
    b(
        Action::Rating(1),
        Key::Num1,
        Cmd,
        "1",
        "One star",
        Lightbox,
        true,
    ),
    b(
        Action::Rating(2),
        Key::Num2,
        Cmd,
        "2",
        "Two stars",
        Lightbox,
        true,
    ),
    b(
        Action::Rating(3),
        Key::Num3,
        Cmd,
        "3",
        "Three stars",
        Lightbox,
        true,
    ),
    b(
        Action::Rating(4),
        Key::Num4,
        Cmd,
        "4",
        "Four stars",
        Lightbox,
        true,
    ),
    b(
        Action::Rating(5),
        Key::Num5,
        Cmd,
        "5",
        "Five stars",
        Lightbox,
        true,
    ),
    b(
        Action::ColourLabel(1),
        Key::Num1,
        Shift,
        "1",
        "Magenta color label",
        Lightbox,
        true,
    ),
    b(
        Action::ColourLabel(2),
        Key::Num2,
        Shift,
        "2",
        "Blue color label",
        Lightbox,
        true,
    ),
    b(
        Action::ColourLabel(3),
        Key::Num3,
        Shift,
        "3",
        "Green color label",
        Lightbox,
        true,
    ),
    b(
        Action::ColourLabel(4),
        Key::Num4,
        Shift,
        "4",
        "Yellow color label",
        Lightbox,
        true,
    ),
    b(
        Action::ColourLabel(5),
        Key::Num5,
        Shift,
        "5",
        "Red color label",
        Lightbox,
        true,
    ),
    // -- Files
    b(
        Action::OpenFile,
        Key::O,
        Cmd,
        "o",
        "Open a raw",
        Files,
        true,
    ),
    b(Action::Export, Key::E, Cmd, "e", "Export", Files, true),
    b(
        Action::ExportProof,
        Key::E,
        CmdShift,
        "e",
        "Export proof",
        Files,
        true,
    ),
    b(
        Action::SaveDuplicate,
        Key::S,
        Cmd,
        "s",
        "Save duplicate file",
        Files,
        true,
    ),
    b(
        Action::DuplicateTab,
        Key::D,
        Cmd,
        "d",
        "Duplicate this tab",
        Files,
        true,
    ),
    b(Action::CloseTab, Key::W, Cmd, "w", "Close tab", Files, true),
    // -- Edit
    b(Action::Undo, Key::Z, Cmd, "z", "Undo", Edit, true),
    b(Action::Redo, Key::Z, CmdShift, "z", "Redo", Edit, true),
];

/// Whether `action`'s chord is **held down** right now.
///
/// `pressed` reports the frame a key goes down, which is the right event for
/// everything that toggles or fires. Two bindings are holds instead — `⇧D` and `⇧X`
/// show the dodge and burn maps for as long as you keep them down — and a hold is a
/// state rather than an event.
///
/// It reads the chord out of `TABLE` rather than naming a key here, so the row
/// stays the single description of the binding and the hotkey reference lists it
/// like any other.
/// The twin is counted here too — see [`shifted_twin`]. Today's two holds are `⇧D` and
/// `⇧X`, whose shifted form is still `D` and `X`, so this changes nothing yet. It is
/// written anyway because the alternative is a hold on a punctuation key silently not
/// working, which is the exact failure `shifted_twin` exists to end rather than to
/// relocate.
pub fn held(ctx: &egui::Context, action: Action) -> bool {
    let Some(bind) = TABLE.iter().find(|b| b.action == action) else {
        return false;
    };
    ctx.input(|i| {
        let down = i.key_down(bind.key) || shifted_twin(bind.key).is_some_and(|t| i.key_down(t));
        down && bind.mods.matches(&i.modifiers)
    })
}

/// Whether some modal binding claims this global binding's chord.
///
/// Derived from `TABLE` rather than listed, so a modal binding added later cannot
/// forget to suppress the global one it collides with.
fn shadowed(global: &Binding) -> bool {
    TABLE
        .iter()
        .any(|b| b.modal && b.key == global.key && b.mods == global.mods)
}

/// The key egui reports when this one is pressed **with Shift down**, where that is a
/// different key rather than the same key with a modifier.
///
/// # This is a whole bug class, not a special case
///
/// `egui_winit` resolves a keypress as `logical_key.or(physical_key)` — the *logical*
/// key wins. On a US layout `⇧[` produces the character `{`, and egui has a distinct
/// `OpenCurlyBracket` for it, so a binding written as `OpenBracket` + `Shift` is
/// checking a key that is never pressed. It does not fire, and nothing anywhere
/// reports that it cannot.
///
/// **The table was already carrying one hand-patched instance of this.** `ZoomIn` is
/// bound twice — `Equals` + `Cmd` and `Plus` + `CmdShift` — with a comment explaining
/// that egui renames the key when shift is down. That fix was correct and did not
/// generalise, so `⇧[` / `⇧]` (the brush's feather) and `⇧1` (the first colour label)
/// shipped broken.
///
/// # Why `⇧1` is broken and `⇧2`–`⇧5` are not
///
/// Purely because of which characters egui happens to have variants for. `!` is in its
/// list, so `⇧1` resolves to `Exclamationmark` and misses `Num1`. `@ # $ %` are
/// *not*, so for those the logical lookup fails, the physical fallback runs, and
/// `Digit2`–`Digit5` arrive as `Num2`–`Num5` exactly as the table expects. Four
/// working keys and one dead one, from one rule nobody wrote down.
///
/// Fixing it here rather than by adding twin rows keeps one row per chord — the
/// reference sheet and the menu both read `TABLE` directly, and a duplicate row would
/// print the same shortcut twice — and it means the *next* Shift binding is correct
/// when it is written rather than after someone notices it doing nothing.
///
/// Only the pairs egui actually distinguishes are listed. `⇧-`, `⇧,`, `⇧.` and `` ⇧` ``
/// produce characters egui has no variant for, so they already fall through to the
/// physical key and need nothing.
fn shifted_twin(key: egui::Key) -> Option<egui::Key> {
    use egui::Key as K;
    Some(match key {
        K::Num1 => K::Exclamationmark,
        K::OpenBracket => K::OpenCurlyBracket,
        K::CloseBracket => K::CloseCurlyBracket,
        K::Equals => K::Plus,
        K::Semicolon => K::Colon,
        K::Slash => K::Questionmark,
        K::Backslash => K::Pipe,
        // Qualified: `Mods::None` is in scope here and shadows `Option::None`.
        _ => return Option::None,
    })
}

/// Whether this binding's key went down this frame, counting the shifted twin.
///
/// The twin is checked unconditionally rather than only for `Shift` bindings, and that
/// is safe because the modifier test is separate and exact: a bare `[` binding is
/// `Mods::None`, which requires shift to be *up*, so pressing `{` cannot reach it.
fn key_pressed(i: &egui::InputState, key: egui::Key) -> bool {
    i.key_pressed(key) || shifted_twin(key).is_some_and(|t| i.key_pressed(t))
}

/// `const fn` so `TABLE` can be a `const` and the duplicate-chord test runs against
/// the real thing rather than a copy.
const fn b(
    action: Action,
    key: egui::Key,
    mods: Mods,
    key_label: &'static str,
    what: &'static str,
    group: Group,
    built: bool,
) -> Binding {
    Binding {
        action,
        key,
        mods,
        key_label,
        what,
        group,
        built,
        modal: false,
    }
}

/// A binding that fires only while an interaction mode is open. Always `built`:
/// a modal binding that did nothing would be worse than a global one that did
/// nothing, because there would be no way to tell it apart from the chord it
/// shadowed. See [`Binding::modal`].
const fn m(
    action: Action,
    key: egui::Key,
    mods: Mods,
    key_label: &'static str,
    what: &'static str,
    group: Group,
) -> Binding {
    Binding {
        action,
        key,
        mods,
        key_label,
        what,
        group,
        built: true,
        modal: true,
    }
}

/// Every action requested this frame.
///
/// A `Vec` rather than an `Option`: nothing stops two chords arriving in one frame,
/// and dropping one because it was not first is the kind of loss nobody reports as a
/// bug, they just press the key again.
///
/// **Typing suppresses the untyped-modifier bindings.** With a text field focused,
/// `p` is a letter, not preview-original. Bindings with command are unaffected,
/// because no field consumes those.
///
/// `text_edit_focused`, **not** `egui_wants_keyboard_input`, which despite its name is
/// `focused().is_some()` — true of a slider or a button you merely clicked. With that
/// as the test, one click on any control silently killed every bare key in the app
/// until focus happened to move, which is not a bug anyone reports: they press the key
/// again, and then stop trusting it. Same trap, and the same fix, as `raw_input_hook`.
/// **A mode's bindings shadow the global ones.** `modal` says whether an
/// interaction mode is open. When it is, the modal rows fire and any global row
/// sharing a chord with one does not; when it is not, the modal rows are inert. One
/// filter, in the one place that decides which binding fires — the same mechanism
/// and the same argument as the typing guard above.
/// `enabled` is the Settings switch. **`,` fires whether it is on or off**, and that
/// exemption is not a courtesy: it is the key that reaches the window where the switch
/// lives, and until an OS menu bar exists it is the only one. Turning every key off
/// and losing the way back would leave `settings.toml` and a text editor as the
/// remedy. Same rule the PANELS HIDDEN footer exists for — *a key that can stop
/// working must not be the sole inverse of a gesture* — applied to the switch that can
/// stop them all.
///
/// Escape and Enter are exempt too. They are not in `TABLE`, they close and commit
/// modes, and a mode you cannot leave is worse than a hotkey you did not want.
pub fn pressed(ctx: &egui::Context, modal: bool, enabled: bool) -> Vec<Action> {
    let typing = ctx.text_edit_focused();
    ctx.input(|i| {
        let mut out: Vec<Action> = TABLE
            .iter()
            .filter(|bind| enabled || bind.action == Action::Settings)
            .filter(|bind| !(typing && bind.mods.is_typing()))
            .filter(|bind| {
                if bind.modal {
                    modal
                } else {
                    !(modal && shadowed(bind))
                }
            })
            .filter(|bind| key_pressed(i, bind.key) && bind.mods.matches(&i.modifiers))
            .map(|bind| bind.action)
            .collect();
        // Escape, from outside the table. See [`Action::ExitMode`] for why it is not
        // in it, and note that reporting it here does not *consume* it: egui reads
        // the same input independently, so a window that closes on Escape still
        // does. The handler is a no-op when no mode is open.
        if !typing && i.key_pressed(egui::Key::Escape) {
            out.push(Action::ExitMode);
        }
        if !typing && i.key_pressed(egui::Key::Enter) {
            out.push(Action::CommitMode);
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Draw a widget, give it focus, press `key`, and report what `pressed` made of
    /// it. Two passes because egui applies a focus request on the frame after it.
    fn with_focus_on(text_field: bool, key: egui::Key) -> Vec<Action> {
        let ctx = egui::Context::default();
        let (mut text, mut number) = (String::new(), 0.5f32);
        let mut got = Vec::new();
        for pass in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(400.0, 300.0),
                )),
                events: if pass == 1 {
                    vec![egui::Event::Key {
                        key,
                        // Qualified: `Mods::None` is glob-imported here and shadows
                        // `Option::None`.
                        physical_key: Option::<egui::Key>::None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::default(),
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                let id = if text_field {
                    ui.add(egui::TextEdit::singleline(&mut text)).id
                } else {
                    ui.add(egui::Slider::new(&mut number, 0.0..=1.0)).id
                };
                ui.memory_mut(|m| m.request_focus(id));
                if pass == 1 {
                    got = pressed(ui.ctx(), false, true);
                }
            });
        }
        got
    }

    /// Press one chord with nothing focused and report what `pressed` made of it.
    ///
    /// `key` is what **egui** reports, not what is printed on the keycap — which is the
    /// entire point of the tests below. `egui_winit` resolves a press as
    /// `logical_key.or(physical_key)`, so holding shift and hitting the `[` key
    /// delivers `OpenCurlyBracket`, and these pass that in exactly as it would arrive.
    fn press(key: egui::Key, modifiers: egui::Modifiers, modal: bool) -> Vec<Action> {
        let ctx = egui::Context::default();
        let mut got = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(400.0, 300.0),
            )),
            events: vec![egui::Event::Key {
                key,
                physical_key: Option::<egui::Key>::None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            modifiers,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| got = pressed(ui.ctx(), modal, true));
        got
    }

    #[test]
    fn a_shifted_punctuation_key_reaches_the_binding_written_for_its_unshifted_name() {
        // `⇧[` and `⇧]` are the brush's feather and they did nothing, because the table
        // says `OpenBracket` + `Shift` and egui delivers `OpenCurlyBracket`. See
        // `shifted_twin`. The keycap is `[`; the character is `{`; the binding is
        // written the way a person would write it and the matcher closes the gap.
        let shift = egui::Modifiers::SHIFT;
        assert!(
            press(egui::Key::OpenCurlyBracket, shift, true).contains(&Action::BrushFeather(false)),
            "`⇧[` did not reach the feather"
        );
        assert!(
            press(egui::Key::CloseCurlyBracket, shift, true).contains(&Action::BrushFeather(true)),
            "`⇧]` did not reach the feather"
        );
        // The one broken colour label, and the reason this is a matcher fix rather than
        // two extra rows: `!` is a key egui knows, so `⇧1` missed `Num1`, while `@`
        // through `%` are not and `⇧2`-`⇧5` worked by falling through to the physical
        // key. Nothing in the table distinguished the six.
        assert!(
            press(egui::Key::Exclamationmark, shift, false).contains(&Action::ColourLabel(1)),
            "`⇧1` did not reach the first colour label"
        );
    }

    #[test]
    fn the_twin_does_not_leak_a_shifted_press_onto_the_bare_binding() {
        // The twin is checked for every binding, so the *modifier* test is what keeps
        // `{` off the bare-`[` brush size. If `Mods::matches` ever stopped being exact,
        // one keypress would resize the brush and feather it at the same time.
        let got = press(egui::Key::OpenCurlyBracket, egui::Modifiers::SHIFT, true);
        assert!(
            !got.contains(&Action::BrushRadius(false)),
            "`⇧[` also fired the bare `[` binding"
        );
        assert!(
            got.contains(&Action::BrushFeather(false)),
            "and it should still feather"
        );
    }

    #[test]
    fn only_a_text_field_suppresses_the_bare_keys() {
        // **The guard has to ask whether a *text field* has focus, not whether
        // anything does.** It asked the latter — via `egui_wants_keyboard_input`,
        // which despite the name is `focused().is_some()` — and a slider takes focus
        // when you touch it. So one drag on any control silently killed every bare
        // key in the app until focus happened to move: not a bug anyone reports, they
        // press the key again and then stop trusting it. The same mistake in
        // `raw_input_hook` latched `tab` into cycling focus forever and left the
        // panels unreachable, because `tab` was the only way back to them.
        assert!(
            with_focus_on(false, egui::Key::P).contains(&Action::PreviewOriginal),
            "a focused slider suppressed a bare key"
        );
        assert!(
            with_focus_on(false, egui::Key::Num4).contains(&Action::CompareUp(4)),
            "a focused control suppressed the 4-up comparison key"
        );
        assert!(
            !with_focus_on(true, egui::Key::P).contains(&Action::PreviewOriginal),
            "`p` reached the table while a text field had focus — it is a letter there"
        );
    }

    #[test]
    fn no_chord_is_bound_twice() {
        // The bug this table exists to make impossible. Two arms on one chord means
        // the second silently never fires, and which one is second depends on the
        // order somebody happened to write them in.
        //
        // Scoped **within** each modality rather than across the whole table:
        // Dodge & Burn introduced modal bindings, and a modal binding shadowing a
        // global one is the mechanism, not a bug. Which chords that applies to is
        // pinned separately by `the_shadowed_chords_are_exactly_these`, so the
        // shadowing cannot grow by accident either.
        for modal in [false, true] {
            let mut seen: HashMap<(egui::Key, Mods), Action> = HashMap::new();
            for bind in TABLE.iter().filter(|b| b.modal == modal) {
                if let Some(prev) = seen.insert((bind.key, bind.mods), bind.action) {
                    panic!(
                        "{} is bound to both {:?} and {:?}",
                        bind.chord(),
                        prev,
                        bind.action
                    );
                }
            }
        }
    }

    #[test]
    fn the_shadowed_chords_are_exactly_these() {
        // Every chord a mode takes back from the rest of the app, listed. Three of
        // them, all wanted by both sides and none of them a collision in use: you
        // do not rotate the picture or duplicate the tab mid-stroke.
        //
        // A list rather than a rule, because the cost of one more is that a key
        // silently changes meaning — which is exactly the thing worth having to
        // write down. See `Binding::modal`.
        let mut shadowing: Vec<String> = TABLE
            .iter()
            .filter(|b| !b.modal && shadowed(b))
            .map(|b| format!("{} ({:?})", b.chord(), b.action))
            .collect();
        shadowing.sort();
        assert_eq!(
            shadowing,
            ["⌘[ (RotateLeft)", "⌘] (RotateRight)", "⌘d (DuplicateTab)",]
        );
    }

    #[test]
    fn a_modal_binding_is_inert_until_its_mode_is_open() {
        // The other half of the mechanism. Nothing here presses a key — this is
        // about which rows are eligible at all, which is what `pressed` filters on.
        let eligible = |modal: bool| -> Vec<Action> {
            TABLE
                .iter()
                .filter(|b| {
                    if b.modal {
                        modal
                    } else {
                        !(modal && shadowed(b))
                    }
                })
                .map(|b| b.action)
                .collect()
        };
        let closed = eligible(false);
        let open = eligible(true);

        assert!(closed.contains(&Action::DuplicateTab) && closed.contains(&Action::RotateLeft));
        assert!(
            !closed.contains(&Action::NewDodge),
            "a modal key fired with no mode open"
        );

        assert!(open.contains(&Action::NewDodge) && open.contains(&Action::BrushRadius(true)));
        assert!(
            !open.contains(&Action::DuplicateTab),
            "⌘D duplicated a tab mid-stroke"
        );
        assert!(
            !open.contains(&Action::RotateLeft),
            "a rotate fired mid-stroke"
        );
        // And a global chord the brush does NOT claim still works while painting,
        // which is what stops "modal" from meaning "the app stops responding".
        assert!(open.contains(&Action::Undo) && open.contains(&Action::ZoomFit));
    }

    #[test]
    fn no_action_is_bound_twice_except_where_the_keyboard_forces_it() {
        // Two keys quietly doing the same thing is usually a half-finished rename.
        // The one real alias is zoom-in: `+` and `=` are a single physical key that
        // egui names differently depending on shift, so binding one of them would
        // make the action work only in one shift state.
        const ALIASED: &[Action] = &[Action::ZoomIn];
        let mut seen: HashMap<Action, &'static str> = HashMap::new();
        for bind in TABLE {
            if ALIASED.contains(&bind.action) {
                continue;
            }
            if let Some(prev) = seen.insert(bind.action, bind.what) {
                panic!(
                    "{:?} is bound twice: {prev:?} and {:?}",
                    bind.action, bind.what
                );
            }
        }
        for a in ALIASED {
            assert!(
                TABLE.iter().filter(|b| b.action == *a).count() > 1,
                "{a:?} is not aliased"
            );
        }
    }

    #[test]
    fn backtick_is_the_flick_alone_and_tab_carries_the_strip() {
        // This was the "backtick trio" — flick on the bare key, cycle on `⌘\``, cycle
        // back on `⌘⇧\``. The two `⌘` rows are gone: macOS reserves `⌘\`` for "Move
        // focus to next window" and takes the event before the app or its menu bar sees
        // it, so both were dead keys that read as implemented. See `Mods::Ctrl`.
        //
        // **Bare backtick keeps the flick**, which is the half of `docs/decisions.md`
        // "Both tab keys stay" that was always working, and the A/B gesture the
        // `WARM = 2` decision rests on.
        let ticks: Vec<&Binding> = TABLE
            .iter()
            .filter(|b| b.key == egui::Key::Backtick)
            .collect();
        assert_eq!(
            ticks.len(),
            1,
            "backtick should be the flick and nothing else"
        );
        assert_eq!(ticks[0].action, Action::FlickTab);
        assert_eq!(ticks[0].mods, None);

        // Tab now carries three bindings distinguished only by modifiers, which is the
        // case exact matching exists for — and one of them is the panel toggle, so a
        // subset match would hide the panels every time you walked the strip.
        let tabs: Vec<&Binding> = TABLE.iter().filter(|b| b.key == egui::Key::Tab).collect();
        assert_eq!(tabs.len(), 3);
        let mods: Vec<Mods> = tabs.iter().map(|b| b.mods).collect();
        assert!(mods.contains(&None) && mods.contains(&Ctrl) && mods.contains(&CtrlShift));
    }

    #[test]
    fn control_tab_walks_the_strip_without_also_toggling_the_panels() {
        // The trap this guards: `Mods::None` used to test only cmd, shift and alt, so
        // `⌃Tab` satisfied it and would have fired `TogglePanels` alongside the cycle —
        // one keypress walking the strip *and* hiding the panels. `raw_input_hook`
        // would not have caught it either; it steals Tab only when no modifier is down.
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let ctrl_shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };

        let got = press(egui::Key::Tab, ctrl, false);
        assert!(got.contains(&Action::CycleTabs), "`⌃Tab` did not cycle");
        assert!(
            !got.contains(&Action::TogglePanels),
            "`⌃Tab` also toggled the panels"
        );

        let back = press(egui::Key::Tab, ctrl_shift, false);
        assert!(
            back.contains(&Action::CycleTabsBack),
            "`⌃⇧Tab` did not cycle back"
        );
        assert!(
            !back.contains(&Action::CycleTabs),
            "`⌃⇧Tab` also cycled forwards"
        );
        assert!(
            !back.contains(&Action::TogglePanels),
            "`⌃⇧Tab` also toggled the panels"
        );
    }

    #[test]
    fn control_is_not_command_so_the_two_cannot_be_confused() {
        // On macOS egui reports Cmd as `command` *and* `mac_cmd`, and Control as
        // `ctrl`. Elsewhere `command` is an alias for `ctrl` — which is why `Mods::Ctrl`
        // tests `ctrl && !mac_cmd` rather than `ctrl` alone.
        let mac_cmd = egui::Modifiers {
            command: true,
            mac_cmd: true,
            ..Default::default()
        };
        let real_ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert!(Mods::Ctrl.matches(&real_ctrl));
        assert!(
            !Mods::Ctrl.matches(&mac_cmd),
            "Command matched a Control binding"
        );
        assert!(Mods::Cmd.matches(&mac_cmd));
        assert!(
            !Mods::Cmd.matches(&real_ctrl),
            "Control matched a Command binding"
        );
    }

    #[test]
    fn exact_modifier_matching_keeps_e_and_cmd_e_apart() {
        // `e` is Develop and `⌘E` is Export. The same trap catches `o`/`⌘O`,
        // `k`/`⌘K`, `i`/`⇧I` and `z`/`⌘Z`/`⌘⇧Z`.
        let plain = egui::Modifiers::default();
        let cmd = egui::Modifiers {
            command: true,
            ..Default::default()
        };
        let cmd_shift = egui::Modifiers {
            command: true,
            shift: true,
            ..Default::default()
        };

        assert!(Mods::None.matches(&plain) && !Mods::None.matches(&cmd));
        assert!(Mods::Cmd.matches(&cmd) && !Mods::Cmd.matches(&plain));
        assert!(!Mods::Cmd.matches(&cmd_shift), "⌘E would fire on ⌘⇧E");
        assert!(Mods::CmdShift.matches(&cmd_shift));
    }

    #[test]
    fn alt_is_exact_in_both_directions() {
        // Alt used to mean "no binding at all", and the brush's opacity
        // pair made it a real modifier. Both halves have to hold: `⌥]` must not
        // fire on a bare `]`, and — the half that was true before and must stay
        // true — a bare binding must not fire while Option is held down.
        let alt = egui::Modifiers {
            alt: true,
            ..Default::default()
        };
        let cmd_alt = egui::Modifiers {
            alt: true,
            command: true,
            ..Default::default()
        };
        let plain = egui::Modifiers::default();
        for m in [Mods::None, Mods::Cmd, Mods::Shift, Mods::CmdShift] {
            assert!(!m.matches(&alt), "{m:?} fired on bare alt");
            assert!(!m.matches(&cmd_alt), "{m:?} fired on ⌥⌘");
        }
        assert!(Mods::Alt.matches(&alt));
        assert!(!Mods::Alt.matches(&plain), "⌥] would fire on a bare ]");
        assert!(!Mods::Alt.matches(&cmd_alt));
    }

    #[test]
    fn typing_suppresses_the_bare_and_shifted_keys_only() {
        // Eleven bare letters, and the Settings menu is about to add the first text
        // fields. Typing "p" in a filename must not toggle preview-original; ⌘S
        // must still save.
        for bind in TABLE {
            match bind.mods {
                Mods::None | Mods::Shift => {
                    assert!(
                        bind.mods.is_typing(),
                        "{} would fire while typing",
                        bind.chord()
                    )
                }
                Mods::Cmd | Mods::CmdShift | Mods::Alt | Mods::Ctrl | Mods::CtrlShift => {
                    assert!(
                        !bind.mods.is_typing(),
                        "{} would be swallowed by a text field",
                        bind.chord()
                    )
                }
            }
        }
    }

    #[test]
    fn every_binding_says_whether_it_is_built() {
        // The rule: an unbuilt key is bound and reports itself as pending, never
        // silently absent. This checks the ones that are claimed built are actually
        // the ones the app handles today.
        let built: Vec<Action> = TABLE.iter().filter(|b| b.built).map(|b| b.action).collect();
        for a in [
            Action::Undo,
            Action::Redo,
            Action::OpenFile,
            Action::Export,
            Action::SaveDuplicate,
            Action::CycleTabs,
            Action::FlickTab,
            Action::Settings,
        ] {
            assert!(
                built.contains(&a),
                "{a:?} is implemented but marked unbuilt"
            );
        }
        for a in [
            Action::PreviewOriginal,
            Action::UnderexposedOverlay,
            Action::OverexposedOverlay,
            Action::SensorClipping,
            Action::FalseColour,
            Action::Surround,
            Action::CyclePreviewSource,
        ] {
            assert!(
                built.contains(&a),
                "{a:?} is implemented but marked unbuilt"
            );
        }
        for a in [Action::Crop, Action::RotateLeft, Action::RotateRight] {
            assert!(
                built.contains(&a),
                "{a:?} is implemented but marked unbuilt"
            );
        }
        assert!(
            built.contains(&Action::CaptureSnapshot) && built.contains(&Action::CompareViewer),
            "snapshots and the compare grid are built and must not still say pending"
        );
        assert!(
            built.contains(&Action::Lightbox),
            "the mode switch is built and must not still say pending"
        );
        // Ratings and labels arrived with the grid that gives them something to act
        // on. They are modal in the plainest sense — there is no selected tile
        // outside Lightbox — which is why they were kept off the tab strip.
        for a in [Action::Rating(3), Action::ColourLabel(2)] {
            assert!(
                built.contains(&a),
                "{a:?} is implemented but marked unbuilt"
            );
        }
        let labels: Vec<u8> = TABLE
            .iter()
            .filter_map(|binding| match binding.action {
                Action::ColourLabel(number) => Some(number),
                _ => Option::None,
            })
            .collect();
        assert_eq!(
            labels,
            [1, 2, 3, 4, 5],
            "the retired sixth label is still bound"
        );
    }

    #[test]
    fn the_rotate_pair_leaves_the_bare_brackets_free() {
        // The brush claims the bare brackets, modally. So the claim here is that no
        // GLOBAL binding holds one — the rotate pair pays a modifier on a key nobody
        // presses in a hurry rather than taking them and giving them back.
        for bind in TABLE.iter().filter(|b| !b.modal) {
            let bracket = matches!(bind.key, Key::OpenBracket | Key::CloseBracket);
            assert!(
                !(bracket && bind.mods == Mods::None),
                "{} claims a bare bracket, which the brush needs",
                bind.chord()
            );
        }
        assert!(
            TABLE
                .iter()
                .any(|b| b.modal && b.key == Key::CloseBracket && b.mods == Mods::None),
            "and the brush must actually be the one holding them"
        );
    }

    #[test]
    fn escape_is_not_in_the_table() {
        // It is not a binding; it is a property of being in a mode. Listing it under
        // Composition would claim it belongs to crop, when every mode this app grows
        // will exit the same way — and it must stay available to egui for closing a
        // window when no mode is open. See `Action::ExitMode`.
        assert!(!TABLE.iter().any(|b| b.key == Key::Escape));
        assert!(!TABLE.iter().any(|b| b.key == Key::Enter));
        assert!(!TABLE.iter().any(|b| b.action == Action::ExitMode));
        assert!(!TABLE.iter().any(|b| b.action == Action::CommitMode));
    }

    #[test]
    fn period_is_the_single_hotkey_hud_binding() {
        let bindings: Vec<&Binding> = TABLE
            .iter()
            .filter(|binding| binding.key == Key::Period)
            .collect();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].action, Action::HotkeyHud);
        assert_eq!(bindings[0].mods, Mods::None);
        assert_eq!(bindings[0].key_label, ".");
        assert!(bindings[0].built);
    }

    #[test]
    fn every_group_has_at_least_one_binding() {
        // A group with nothing in it draws an empty heading in the reference.
        for g in Group::ORDER {
            assert!(TABLE.iter().any(|b| b.group == g), "{} is empty", g.label());
        }
    }

    #[test]
    fn the_reference_groups_match_the_tools_the_actions_belong_to() {
        let loupe = TABLE.iter().find(|b| b.action == Action::Loupe).unwrap();
        assert_eq!(loupe.group, Group::Comparison);
        let before_after = TABLE
            .iter()
            .find(|b| b.action == Action::LoupeBeforeAfter)
            .unwrap();
        assert_eq!(before_after.group, Group::Comparison);
        assert_eq!(before_after.mods, Mods::Shift);
        assert_eq!(before_after.key, Key::V);

        for binding in TABLE
            .iter()
            .filter(|binding| matches!(binding.action, Action::Rating(_) | Action::ColourLabel(_)))
        {
            assert_eq!(binding.group, Group::Lightbox);
        }

        let contact_sheet = TABLE
            .iter()
            .find(|binding| binding.action == Action::ContactSheet)
            .unwrap();
        assert_eq!(contact_sheet.group, Group::Lightbox);
        assert_eq!(contact_sheet.mods, Mods::CmdShift);
        assert_eq!(contact_sheet.key, Key::P);
        assert!(contact_sheet.built);
    }

    #[test]
    fn mode_gestures_are_in_both_reference_sources_without_becoming_key_bindings() {
        let quick_look = REFERENCE_GESTURES
            .iter()
            .find(|gesture| gesture.what.contains("Quick Look"))
            .unwrap();
        assert_eq!(quick_look.chord, "space");
        assert_eq!(quick_look.group, Group::Lightbox);
        assert!(quick_look.before_bindings);

        let delete_pin = REFERENCE_GESTURES
            .iter()
            .find(|gesture| gesture.what == "Delete Inspector pin")
            .unwrap();
        assert_eq!(delete_pin.chord, "⇧Click");
        assert_eq!(delete_pin.group, Group::ValuePins);
        assert!(!delete_pin.before_bindings);
    }
}

#[cfg(test)]
mod disable_tests {
    use super::*;

    /// Every binding the table would fire for a key, with the switch in a given state.
    fn table_fires(enabled: bool, key: egui::Key, mods: egui::Modifiers) -> Vec<Action> {
        TABLE
            .iter()
            .filter(|bind| enabled || bind.action == Action::Settings)
            .filter(|bind| bind.key == key && bind.mods.matches(&mods))
            .map(|bind| bind.action)
            .collect()
    }

    #[test]
    fn bare_e_brings_develop_forward_and_says_it_is_built() {
        // `l` and `e` were reserved for the Lightbox ↔ Develop mode switch and for a
        // long time only the Develop half meant anything, which this test pinned so
        // the gap did not read as one of the pair having been missed.
        //
        // **The gap is closed.** `l` switches modes, so both halves of the pair now
        // report themselves built and the asymmetry this was written to record no
        // longer exists. What is left worth asserting is that the pair is a pair.
        let none = egui::Modifiers::default();
        assert_eq!(table_fires(true, egui::Key::E, none), vec![Action::Develop]);
        assert_eq!(
            table_fires(true, egui::Key::L, none),
            vec![Action::Lightbox]
        );
        let bind = |a: Action| TABLE.iter().find(|b| b.action == a).expect("bound");
        assert!(
            bind(Action::Develop).built,
            "e does something and must say so"
        );
        assert!(bind(Action::Lightbox).built, "l switches modes now");
        // And `⌘E` is still export: the two are different bindings, which is the whole
        // reason `Mods` matches exactly.
        let cmd = egui::Modifiers {
            command: true,
            ..Default::default()
        };
        assert_eq!(table_fires(true, egui::Key::E, cmd), vec![Action::Export]);
    }

    #[test]
    fn the_e_key_carries_three_different_errands() {
        // `e` develops, `⌘E` exports the master, `⌘⇧E` exports the proof. Three
        // bindings on one letter is exactly the arrangement `Mods` matching *exactly*
        // exists for, and exactly the one that goes wrong quietly: a subset match fires
        // the master export as well as the proof, and you get two file dialogs and one
        // of them writes a 40 MP TIFF you did not ask for.
        let none = egui::Modifiers::default();
        let cmd = egui::Modifiers {
            command: true,
            ..Default::default()
        };
        let cmd_shift = egui::Modifiers {
            command: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(table_fires(true, egui::Key::E, none), vec![Action::Develop]);
        assert_eq!(table_fires(true, egui::Key::E, cmd), vec![Action::Export]);
        assert_eq!(
            table_fires(true, egui::Key::E, cmd_shift),
            vec![Action::ExportProof]
        );
    }

    #[test]
    fn the_switch_silences_the_table() {
        // The point of the setting. A handful of representative bindings across the
        // modifier classes, so a filter that only caught bare keys would show up.
        let none = egui::Modifiers::default();
        let cmd = egui::Modifiers::COMMAND;
        assert!(
            !table_fires(true, egui::Key::P, none).is_empty(),
            "p fires when enabled"
        );
        assert!(
            table_fires(false, egui::Key::P, none).is_empty(),
            "p must be silent"
        );
        assert!(
            !table_fires(true, egui::Key::E, cmd).is_empty(),
            "⌘E fires when enabled"
        );
        assert!(
            table_fires(false, egui::Key::E, cmd).is_empty(),
            "⌘E must be silent"
        );
    }

    #[test]
    fn the_comma_survives_being_switched_off() {
        // **The exemption, and it is not a courtesy.** `,` is the key that reaches the
        // window where this switch lives, and until an OS menu bar exists it is the
        // only one — so a switch that silenced it would leave `settings.toml` and a
        // text editor as the way back. Same rule the PANELS HIDDEN footer exists for:
        // a key that can stop working must not be the sole inverse of a gesture.
        let none = egui::Modifiers::default();
        assert_eq!(
            table_fires(false, egui::Key::Comma, none),
            vec![Action::Settings]
        );
        assert_eq!(
            table_fires(true, egui::Key::Comma, none),
            vec![Action::Settings]
        );
    }

    #[test]
    fn nothing_else_claims_the_comma() {
        // The exemption is written as "this action survives", so a second binding on
        // the same key would ride out with it. There is only one, and this is what
        // says so.
        let all: Vec<Action> = TABLE
            .iter()
            .filter(|b| b.key == egui::Key::Comma)
            .map(|b| b.action)
            .collect();
        assert_eq!(all, vec![Action::Settings]);
    }
}
