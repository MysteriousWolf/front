//! Central keybinding registry.
//!
//! Every command the UI understands is declared exactly once in [`BINDINGS`],
//! together with everything needed to present it: a name, a category, a
//! description, the chords that trigger it, and the labels used by the help
//! modal and the footer.  The event loop resolves keys through [`resolve`],
//! and both `render_help` and `render_footer` are generated from the same
//! table — so a new binding shows up everywhere it should without any other
//! file being touched.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A command the UI can perform.
///
/// Documentation-only rows (mouse gestures) carry no action; see
/// [`Binding::action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    ToggleHelp,
    PanLeft,
    PanRight,
    PanUp,
    PanDown,
    ZoomIn,
    ZoomOut,
    FrameBack,
    FrameForward,
    TogglePlayback,
    JumpToLive,
    SpeedFaster,
    SpeedSlower,
    CycleHistory,
    ToggleLayer,
    ModeBraille,
    ModeColor,
    ModeText,
    SelectPrevious,
    SelectNext,
    EnterGroup,
    ExitGroup,
    RefetchMap,
    OpenSearch,
    OpenSettings,
}

/// The help modal's sections, rendered in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Navigation,
    Timeline,
    Layers,
    General,
}

impl Category {
    pub const ORDER: [Category; 4] = [
        Category::Navigation,
        Category::Timeline,
        Category::Layers,
        Category::General,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Navigation => "Navigation",
            Category::Timeline => "Timeline",
            Category::Layers => "Layers",
            Category::General => "General",
        }
    }
}

/// A single key press: a code plus the modifiers that must accompany it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    pub const fn plain(code: KeyCode) -> Self {
        Chord {
            code,
            mods: KeyModifiers::NONE,
        }
    }

    pub const fn shift(code: KeyCode) -> Self {
        Chord {
            code,
            mods: KeyModifiers::SHIFT,
        }
    }

    pub const fn alt(code: KeyCode) -> Self {
        Chord {
            code,
            mods: KeyModifiers::ALT,
        }
    }
}

/// A compact hint shown in the footer strip.
#[derive(Debug, Clone, Copy)]
pub struct FooterHint {
    /// Key label, kept short enough for a one-line strip (e.g. `"+/-"`).
    pub keys: &'static str,
    /// Lower-case verb (e.g. `"zoom"`).
    pub label: &'static str,
    /// Lower sorts earlier.  The strip is dropped from the right when the
    /// terminal is too narrow for all of it, so this ranks hints by how badly
    /// a new user needs them — the way out comes before the niceties.
    pub rank: u8,
}

/// One row of the registry.
pub struct Binding {
    /// The command to run, or `None` for a documentation-only row such as a
    /// mouse gesture, which the help lists but no key resolves to.
    pub action: Option<Action>,
    /// Short imperative name, used as the help row's label.
    pub name: &'static str,
    pub category: Category,
    /// One clause of detail, shown after the name when the help has room.
    pub description: &'static str,
    pub chords: &'static [Chord],
    /// Key label for this binding's own help row.  `None` folds the binding
    /// into a sibling row that already documents the same key group — the four
    /// pan arrows are one row, not four.
    pub help_keys: Option<&'static str>,
    pub footer: Option<FooterHint>,
}

const fn hint(keys: &'static str, label: &'static str, rank: u8) -> Option<FooterHint> {
    Some(FooterHint { keys, label, rank })
}

pub static BINDINGS: &[Binding] = &[
    // ── Navigation ──────────────────────────────────────────────────────
    Binding {
        action: Some(Action::PanLeft),
        name: "Pan the map",
        category: Category::Navigation,
        description: "move the viewport",
        chords: &[Chord::plain(KeyCode::Left)],
        help_keys: Some("← ↑ ↓ →"),
        footer: hint("arrows", "pan", 3),
    },
    Binding {
        action: Some(Action::PanRight),
        name: "Pan right",
        category: Category::Navigation,
        description: "move the viewport east",
        chords: &[Chord::plain(KeyCode::Right)],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::PanUp),
        name: "Pan up",
        category: Category::Navigation,
        description: "move the viewport north",
        chords: &[Chord::plain(KeyCode::Up)],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::PanDown),
        name: "Pan down",
        category: Category::Navigation,
        description: "move the viewport south",
        chords: &[Chord::plain(KeyCode::Down)],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::ZoomIn),
        name: "Zoom in / out",
        category: Category::Navigation,
        description: "one quarter step per press",
        chords: &[
            Chord::plain(KeyCode::Char('+')),
            Chord::plain(KeyCode::Char('=')),
        ],
        help_keys: Some("+ / -"),
        footer: hint("+/-", "zoom", 4),
    },
    Binding {
        action: Some(Action::ZoomOut),
        name: "Zoom out",
        category: Category::Navigation,
        description: "one quarter step per press",
        chords: &[Chord::plain(KeyCode::Char('-'))],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: None,
        name: "Drag to pan",
        category: Category::Navigation,
        description: "hold the left button and move",
        chords: &[],
        help_keys: Some("drag"),
        footer: None,
    },
    Binding {
        action: None,
        name: "Scroll to zoom",
        category: Category::Navigation,
        description: "zooms about the cursor; Shift for fine steps",
        chords: &[],
        help_keys: Some("scroll"),
        footer: None,
    },
    // ── Timeline ────────────────────────────────────────────────────────
    Binding {
        action: Some(Action::FrameBack),
        name: "Step frame",
        category: Category::Timeline,
        description: "back / forward one radar frame",
        chords: &[Chord::plain(KeyCode::Char(']'))],
        help_keys: Some("] / ["),
        footer: hint("[/]", "frame", 5),
    },
    Binding {
        action: Some(Action::FrameForward),
        name: "Step frame forward",
        category: Category::Timeline,
        description: "advance one radar frame",
        chords: &[Chord::plain(KeyCode::Char('['))],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::TogglePlayback),
        name: "Play / pause",
        category: Category::Timeline,
        description: "animate the loaded frames",
        chords: &[Chord::plain(KeyCode::Char(' '))],
        help_keys: Some("space"),
        footer: hint("space", "play", 6),
    },
    Binding {
        action: Some(Action::JumpToLive),
        name: "Jump to live",
        category: Category::Timeline,
        description: "return to the newest frame",
        chords: &[Chord::plain(KeyCode::Char('0'))],
        help_keys: Some("0"),
        footer: hint("0", "live", 7),
    },
    Binding {
        action: Some(Action::SpeedFaster),
        name: "Playback speed",
        category: Category::Timeline,
        description: "faster / slower",
        chords: &[Chord::plain(KeyCode::Char('.'))],
        help_keys: Some(". / ,"),
        footer: None,
    },
    Binding {
        action: Some(Action::SpeedSlower),
        name: "Playback slower",
        category: Category::Timeline,
        description: "lengthen the frame interval",
        chords: &[Chord::plain(KeyCode::Char(','))],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::CycleHistory),
        name: "History depth",
        category: Category::Timeline,
        description: "cycle 3 / 6 / 12 / 24 hours",
        chords: &[Chord::plain(KeyCode::Char('i'))],
        help_keys: Some("i"),
        footer: hint("i", "history", 8),
    },
    // ── Layers ──────────────────────────────────────────────────────────
    Binding {
        action: Some(Action::ToggleLayer),
        name: "Toggle layer",
        category: Category::Layers,
        description: "enable or disable the selection",
        chords: &[Chord::plain(KeyCode::Enter)],
        help_keys: Some("enter"),
        footer: hint("enter", "toggle", 9),
    },
    Binding {
        action: Some(Action::SelectPrevious),
        name: "Select layer",
        category: Category::Layers,
        description: "move up / down the panel",
        chords: &[Chord::alt(KeyCode::Up)],
        help_keys: Some("alt+↑↓"),
        footer: None,
    },
    Binding {
        action: Some(Action::SelectNext),
        name: "Select next layer",
        category: Category::Layers,
        description: "move down the panel",
        chords: &[Chord::alt(KeyCode::Down)],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::EnterGroup),
        name: "Layer options",
        category: Category::Layers,
        description: "enter / leave the selected layer's options",
        // Enhanced terminals send Alt+arrow; Terminal.app sends ESC+f/b.
        chords: &[Chord::alt(KeyCode::Right), Chord::alt(KeyCode::Char('f'))],
        help_keys: Some("alt+←→"),
        footer: None,
    },
    Binding {
        action: Some(Action::ExitGroup),
        name: "Leave layer options",
        category: Category::Layers,
        description: "step back out to the layer list",
        chords: &[Chord::alt(KeyCode::Left), Chord::alt(KeyCode::Char('b'))],
        help_keys: None,
        footer: None,
    },
    Binding {
        action: Some(Action::ModeBraille),
        name: "Braille mode",
        category: Category::Layers,
        description: "assign the braille renderer to this layer",
        chords: &[Chord::plain(KeyCode::Char('b'))],
        help_keys: Some("b"),
        footer: None,
    },
    Binding {
        action: Some(Action::ModeColor),
        name: "Colour mode",
        category: Category::Layers,
        description: "assign the colour renderer to this layer",
        chords: &[Chord::plain(KeyCode::Char('c'))],
        help_keys: Some("c"),
        footer: None,
    },
    Binding {
        action: Some(Action::ModeText),
        name: "Text mode",
        category: Category::Layers,
        description: "assign the text renderer to this layer",
        chords: &[Chord::plain(KeyCode::Char('l'))],
        help_keys: Some("l"),
        footer: None,
    },
    // ── General ─────────────────────────────────────────────────────────
    Binding {
        action: Some(Action::ToggleHelp),
        name: "Toggle this help",
        category: Category::General,
        description: "esc also closes it",
        chords: &[Chord::plain(KeyCode::Char('?'))],
        help_keys: Some("?"),
        footer: hint("?", "help", 1),
    },
    Binding {
        action: Some(Action::OpenSearch),
        name: "Search for a place",
        category: Category::General,
        description: "type a place name, enter to pin it",
        chords: &[Chord::plain(KeyCode::Char('/'))],
        help_keys: Some("/"),
        footer: hint("/", "search", 10),
    },
    Binding {
        action: Some(Action::OpenSettings),
        name: "Settings",
        category: Category::General,
        description: "edit the EUMETNET API key / IP fallback",
        chords: &[Chord::plain(KeyCode::Char('s'))],
        help_keys: Some("s"),
        footer: hint("s", "settings", 2),
    },
    Binding {
        action: Some(Action::RefetchMap),
        name: "Refetch map data",
        category: Category::General,
        description: "re-download the border geometry",
        chords: &[Chord::plain(KeyCode::Char('m'))],
        help_keys: Some("m"),
        footer: None,
    },
    Binding {
        action: Some(Action::Quit),
        name: "Quit",
        category: Category::General,
        description: "saves the viewport and layer state",
        chords: &[Chord::plain(KeyCode::Char('q')), Chord::plain(KeyCode::Esc)],
        help_keys: Some("q / esc"),
        footer: hint("q", "quit", 0),
    },
];

/// Reduce a key event to the form the registry matches against.
///
/// Only Shift/Alt/Ctrl distinguish a chord; terminals also report state bits
/// (KEYPAD, NUM_LOCK, …) that would otherwise defeat an exact comparison.  For
/// character keys Shift is dropped as well, because the terminal has already
/// folded it into the character itself — `+` arrives as Shift+`+` on many
/// layouts and must still match a plain `+` chord.
fn normalize(key: KeyEvent) -> (KeyCode, KeyModifiers) {
    let mut mods =
        key.modifiers & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);
    if matches!(key.code, KeyCode::Char(_)) {
        mods.remove(KeyModifiers::SHIFT);
    }
    (key.code, mods)
}

/// An action the settings modal's keyboard takeover derives from a raw key
/// press. Kept pure and separate from `ui.rs` so the mapping is
/// unit-testable without a terminal. The focused field is edited in place, so
/// printables and Backspace only bite once a field is being edited — see
/// `docs/spec/tui-config-editor.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsKeyAction {
    /// Ctrl-C — quits the whole app, even mid-edit.
    Quit,
    FocusPrev,
    FocusNext,
    /// Left or Right — flips the focused bool field while editing it.
    ToggleBool,
    /// Enter — start editing the focused field, or save the edit in progress
    /// (a changed key is verified automatically on save).
    Confirm,
    /// Esc — cancel the edit in progress, or close the modal when not editing.
    Back,
    PushChar(char),
    Backspace,
}

/// Map a raw key press to a [`SettingsKeyAction`] while the settings modal
/// owns the keyboard. `None` for keys the modal ignores.
pub fn settings_key_action(key: KeyEvent) -> Option<SettingsKeyAction> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(SettingsKeyAction::Quit),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Up => Some(SettingsKeyAction::FocusPrev),
        KeyCode::Down => Some(SettingsKeyAction::FocusNext),
        KeyCode::Left | KeyCode::Right => Some(SettingsKeyAction::ToggleBool),
        KeyCode::Enter => Some(SettingsKeyAction::Confirm),
        KeyCode::Esc => Some(SettingsKeyAction::Back),
        KeyCode::Backspace => Some(SettingsKeyAction::Backspace),
        KeyCode::Char(c) => Some(SettingsKeyAction::PushChar(c)),
        _ => None,
    }
}

/// The action bound to `key`, if any.
pub fn resolve(key: KeyEvent) -> Option<Action> {
    let (code, mods) = normalize(key);
    BINDINGS
        .iter()
        .find(|b| b.chords.iter().any(|c| c.code == code && c.mods == mods))
        .and_then(|b| b.action)
}

/// The footer hints, most-needed first.
pub fn footer_hints() -> Vec<&'static FooterHint> {
    let mut hints: Vec<_> = BINDINGS.iter().filter_map(|b| b.footer.as_ref()).collect();
    hints.sort_by_key(|h| h.rank);
    hints
}

/// The help rows for `category`, in registry order: `(keys, name, description)`.
pub fn help_rows(
    category: Category,
) -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    BINDINGS.iter().filter_map(move |b| {
        if b.category != category {
            return None;
        }
        b.help_keys.map(|k| (k, b.name, b.description))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Footer hints are ordered by rank, so a duplicate makes which of the two
    /// survives a narrow terminal depend on declaration order.
    #[test]
    fn footer_hint_ranks_are_unique() {
        let mut ranks: Vec<u8> = BINDINGS
            .iter()
            .filter_map(|b| b.footer.map(|f| f.rank))
            .collect();
        ranks.sort_unstable();
        let mut deduped = ranks.clone();
        deduped.dedup();
        assert_eq!(ranks, deduped, "duplicate footer hint rank");
    }

    #[test]
    fn slash_opens_the_search_prompt() {
        assert_eq!(
            resolve(press(KeyCode::Char('/'), KeyModifiers::NONE)),
            Some(Action::OpenSearch)
        );
    }

    #[test]
    fn space_plays_and_enter_toggles_a_layer() {
        assert_eq!(
            resolve(press(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(Action::TogglePlayback)
        );
        assert_eq!(
            resolve(press(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::ToggleLayer)
        );
        // 'p' is no longer bound to anything.
        assert_eq!(resolve(press(KeyCode::Char('p'), KeyModifiers::NONE)), None);
    }

    #[test]
    fn shift_distinguishes_arrow_chords_but_not_characters() {
        assert_eq!(
            resolve(press(KeyCode::Up, KeyModifiers::NONE)),
            Some(Action::PanUp)
        );
        // Alt is the only layer-selection modifier; Shift+arrow was a second,
        // partial binding for the same thing and only muddied the model.
        assert_eq!(
            resolve(press(KeyCode::Up, KeyModifiers::ALT)),
            Some(Action::SelectPrevious)
        );
        assert_eq!(
            resolve(press(KeyCode::Down, KeyModifiers::ALT)),
            Some(Action::SelectNext)
        );
        assert_eq!(
            resolve(press(KeyCode::Up, KeyModifiers::SHIFT)),
            None,
            "shift+arrow no longer selects layers"
        );
        // A layout that reports '+' as Shift+'+' still zooms in.
        assert_eq!(
            resolve(press(KeyCode::Char('+'), KeyModifiers::SHIFT)),
            Some(Action::ZoomIn)
        );
    }

    #[test]
    fn terminal_state_bits_do_not_block_a_match() {
        assert_eq!(
            resolve(press(KeyCode::Char('0'), KeyModifiers::empty())),
            Some(Action::JumpToLive)
        );
    }

    #[test]
    fn every_chord_resolves_to_exactly_one_action() {
        for b in BINDINGS {
            for c in b.chords {
                let hits = BINDINGS
                    .iter()
                    .filter(|o| o.chords.iter().any(|x| x == c))
                    .count();
                assert_eq!(hits, 1, "chord {c:?} is bound more than once");
            }
        }
    }

    #[test]
    fn every_category_has_help_rows() {
        for cat in Category::ORDER {
            assert!(
                help_rows(cat).next().is_some(),
                "{} has no help rows",
                cat.title()
            );
        }
    }

    #[test]
    fn documentation_rows_carry_no_chords() {
        for b in BINDINGS {
            if b.action.is_none() {
                assert!(b.chords.is_empty(), "{} is unreachable", b.name);
            }
        }
    }

    // -- Settings modal key mapping ------------------------------------

    #[test]
    fn ctrl_c_quits_the_settings_modal() {
        assert_eq!(
            settings_key_action(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(SettingsKeyAction::Quit)
        );
    }

    #[test]
    fn ctrl_r_and_ctrl_v_are_unbound_in_settings() {
        // Reveal is automatic on focus and verify is automatic on save, so
        // neither has a manual chord anymore — both fall through to None.
        assert_eq!(
            settings_key_action(press(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            settings_key_action(press(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn arrows_move_focus_and_left_right_toggle_bool() {
        assert_eq!(
            settings_key_action(press(KeyCode::Up, KeyModifiers::NONE)),
            Some(SettingsKeyAction::FocusPrev)
        );
        assert_eq!(
            settings_key_action(press(KeyCode::Down, KeyModifiers::NONE)),
            Some(SettingsKeyAction::FocusNext)
        );
        assert_eq!(
            settings_key_action(press(KeyCode::Left, KeyModifiers::NONE)),
            Some(SettingsKeyAction::ToggleBool)
        );
        assert_eq!(
            settings_key_action(press(KeyCode::Right, KeyModifiers::NONE)),
            Some(SettingsKeyAction::ToggleBool)
        );
    }

    #[test]
    fn enter_confirms_and_esc_backs_out() {
        assert_eq!(
            settings_key_action(press(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SettingsKeyAction::Confirm)
        );
        assert_eq!(
            settings_key_action(press(KeyCode::Esc, KeyModifiers::NONE)),
            Some(SettingsKeyAction::Back)
        );
    }

    #[test]
    fn printable_chars_push_and_backspace_deletes() {
        assert_eq!(
            settings_key_action(press(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(SettingsKeyAction::PushChar('a'))
        );
        // A space must still edit the field — bool toggling is Left/Right
        // only, so a secret can contain a literal space.
        assert_eq!(
            settings_key_action(press(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(SettingsKeyAction::PushChar(' '))
        );
        assert_eq!(
            settings_key_action(press(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(SettingsKeyAction::Backspace)
        );
    }

    #[test]
    fn unhandled_keys_resolve_to_none() {
        assert_eq!(
            settings_key_action(press(KeyCode::F(1), KeyModifiers::NONE)),
            None
        );
    }
}
