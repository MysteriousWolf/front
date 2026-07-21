//! Pure settings field model for the in-TUI config editor.
//!
//! Represents the editable field set (`eumetnet.api_key` secret,
//! `location.ip_fallback` bool) and an in-progress editing session: staged
//! edits, focus navigation, and a masked secret display that reveals only the
//! focused field. Zero ratatui, zero `App` coupling, zero I/O — `app.rs`
//! drives the interaction and `ui.rs` renders this model; a confirmed edit is
//! written to disk, verified, and applied live by the caller.

use crate::config::{Config, ConfigEdit, ConfigEditValue};

/// The kind of value a settings field holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// A secret string, masked unless its field is focused.
    Secret,
    /// A boolean preference.
    Bool,
}

/// The staged/current value of a field.
#[derive(Clone, PartialEq, Eq)]
pub enum FieldValue {
    Secret(String),
    Bool(bool),
}

/// Manual `Debug` impl (F-3): the derived one would print a `Secret`'s raw
/// value, and `App` — which holds a `SettingsState` once the modal is open —
/// derives `Debug`, so any `{:?}`/panic interpolation of `App` would leak
/// the key straight into `front.log`. Reuse the same mask the modal renders.
impl std::fmt::Debug for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldValue::Secret(s) => write!(f, "Secret({})", mask_secret(s)),
            FieldValue::Bool(b) => write!(f, "Bool({b})"),
        }
    }
}

/// One editable settings field: identity, kind, an optional provider URL (for
/// secrets, so the user knows where to get the key), the current (persisted)
/// value, and the staged (in-progress) value.
#[derive(Debug, Clone)]
pub struct Field {
    /// Dotted config key, e.g. `"eumetnet.api_key"`. Matches the key format
    /// `ConfigEdit` and `apply_config_edits` expect.
    pub key: &'static str,
    /// Human-readable label shown in the modal.
    pub label: &'static str,
    pub kind: FieldKind,
    /// Where to obtain this value, shown as a minimal hint under a secret.
    pub help_url: Option<&'static str>,
    pub current: FieldValue,
    pub staged: FieldValue,
}

impl Field {
    fn new(
        key: &'static str,
        label: &'static str,
        help_url: Option<&'static str>,
        value: FieldValue,
    ) -> Self {
        Self {
            key,
            label,
            kind: match value {
                FieldValue::Secret(_) => FieldKind::Secret,
                FieldValue::Bool(_) => FieldKind::Bool,
            },
            help_url,
            current: value.clone(),
            staged: value,
        }
    }

    /// Whether the staged value differs from the current (persisted) one.
    pub fn is_dirty(&self) -> bool {
        self.staged != self.current
    }

    /// True when this is a secret field whose staged value is empty.
    pub fn is_secret_empty(&self) -> bool {
        matches!(&self.staged, FieldValue::Secret(s) if s.is_empty())
    }

    /// Display string for the field's staged value. `revealed` shows a
    /// secret's raw value (the focused field); otherwise a secret is masked.
    /// Bool fields render as their on/off state regardless.
    pub fn display(&self, revealed: bool) -> String {
        match &self.staged {
            FieldValue::Bool(b) => (if *b { "on" } else { "off" }).to_string(),
            FieldValue::Secret(s) => {
                if revealed {
                    s.clone()
                } else {
                    mask_secret(s)
                }
            }
        }
    }
}

/// Mask a secret for display: `unset` when empty, `set ••••` + last 4 chars
/// when long enough to have 4 trailing chars to show, otherwise fully masked
/// (`set ••••`) so a short secret never leaks more than it has.
fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        return "unset".to_string();
    }
    let char_count = s.chars().count();
    if char_count <= 4 {
        return "set ••••".to_string();
    }
    let tail: String = s.chars().skip(char_count - 4).collect();
    format!("set ••••{tail}")
}

/// Editable field keys, in focus-navigation order.
const EUMETNET_API_KEY: &str = "eumetnet.api_key";
const LOCATION_IP_FALLBACK: &str = "location.ip_fallback";

/// Where to obtain the EUMETNET / MeteoGate API key.
const EUMETNET_KEY_URL: &str = "https://devportal.meteogate.eu/";

/// The settings editor's in-progress editing session.
#[derive(Debug, Clone)]
pub struct SettingsModel {
    pub fields: Vec<Field>,
    /// Index into `fields` of the currently focused field.
    pub focus: usize,
}

impl SettingsModel {
    /// Build the model from the current persisted config: current and
    /// staged both start at the config's present values.
    pub fn from_config(config: &Config) -> Self {
        Self {
            fields: vec![
                Field::new(
                    EUMETNET_API_KEY,
                    "EUMETNET API key",
                    Some(EUMETNET_KEY_URL),
                    FieldValue::Secret(config.eumetnet.api_key.clone()),
                ),
                Field::new(
                    LOCATION_IP_FALLBACK,
                    "IP location fallback",
                    None,
                    FieldValue::Bool(config.location.ip_fallback),
                ),
            ],
            focus: 0,
        }
    }

    /// The currently focused field.
    pub fn focused(&self) -> &Field {
        &self.fields[self.focus]
    }

    fn focused_mut(&mut self) -> &mut Field {
        &mut self.fields[self.focus]
    }

    /// Move focus to the next field, wrapping around.
    pub fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
    }

    /// Move focus to the previous field, wrapping around.
    pub fn focus_prev(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
    }

    /// Append a character to the focused secret's staged value. No-op on a
    /// `Bool` field.
    pub fn push_char(&mut self, c: char) {
        if let FieldValue::Secret(s) = &mut self.focused_mut().staged {
            s.push(c);
        }
    }

    /// Remove the last character from the focused secret's staged value.
    /// No-op on a `Bool` field or an already-empty value.
    pub fn backspace(&mut self) {
        if let FieldValue::Secret(s) = &mut self.focused_mut().staged {
            s.pop();
        }
    }

    /// Flip the focused field's staged bool. No-op on a `Secret` field.
    pub fn toggle_bool(&mut self) {
        if let FieldValue::Bool(b) = &mut self.focused_mut().staged {
            *b = !*b;
        }
    }

    /// The `ConfigEdit` the focused field would produce if committed now, or
    /// `None` when it is unchanged. Pure — does not mutate.
    pub fn focused_pending_edit(&self) -> Option<ConfigEdit> {
        let field = self.focused();
        if !field.is_dirty() {
            return None;
        }
        let value = match &field.staged {
            FieldValue::Secret(s) => ConfigEditValue::Str(s.clone()),
            FieldValue::Bool(b) => ConfigEditValue::Bool(*b),
        };
        Some(ConfigEdit {
            key: field.key.to_string(),
            value,
        })
    }

    /// Commit the focused field's staged value into its current value. Call
    /// only after the edit has been persisted successfully.
    pub fn commit_focused(&mut self) {
        let field = self.focused_mut();
        field.current = field.staged.clone();
    }

    /// Revert the focused field's staged value to its current value —
    /// cancelling an edit in progress.
    pub fn revert_focused(&mut self) {
        let field = self.focused_mut();
        field.staged = field.current.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(api_key: &str, ip_fallback: bool) -> SettingsModel {
        let mut config = Config::default();
        config.eumetnet.api_key = api_key.to_string();
        config.location.ip_fallback = ip_fallback;
        SettingsModel::from_config(&config)
    }

    // -- Masking --------------------------------------------------------

    #[test]
    fn mask_normal_secret_shows_last_four() {
        assert_eq!(mask_secret("abcd1234"), "set ••••1234");
    }

    #[test]
    fn mask_empty_secret_is_unset() {
        assert_eq!(mask_secret(""), "unset");
    }

    #[test]
    fn mask_short_secret_never_leaks_full_value() {
        // <=4 chars: fully masked, no tail leaked.
        assert_eq!(mask_secret("abc"), "set ••••");
        assert_eq!(mask_secret("abcd"), "set ••••");
        assert!(!mask_secret("abc").contains("abc"));
    }

    #[test]
    fn mask_five_char_secret_shows_last_four_not_first() {
        assert_eq!(mask_secret("abcde"), "set ••••bcde");
    }

    // -- Display / reveal-by-focus --------------------------------------

    #[test]
    fn display_masks_when_not_revealed_and_shows_raw_when_revealed() {
        let model = model_with("supersecretkey", true);
        assert_eq!(model.fields[0].display(false), "set ••••tkey");
        assert_eq!(model.fields[0].display(true), "supersecretkey");
    }

    #[test]
    fn bool_display_is_on_off_regardless_of_reveal() {
        let model = model_with("key", true);
        assert_eq!(model.fields[1].display(false), "on");
        assert_eq!(model.fields[1].display(true), "on");
    }

    // -- Provider link --------------------------------------------------

    #[test]
    fn secret_field_carries_a_help_url_bool_does_not() {
        let model = model_with("key", true);
        assert_eq!(model.fields[0].help_url, Some(EUMETNET_KEY_URL));
        assert_eq!(model.fields[1].help_url, None);
    }

    // -- Focus navigation ---------------------------------------------------

    #[test]
    fn focus_next_and_prev_wrap_around() {
        let mut model = model_with("key", true);
        assert_eq!(model.focus, 0);
        model.focus_next();
        assert_eq!(model.focus, 1);
        model.focus_next();
        assert_eq!(model.focus, 0, "focus_next must wrap past the last field");
        model.focus_prev();
        assert_eq!(
            model.focus, 1,
            "focus_prev must wrap before the first field"
        );
    }

    // -- Editing ------------------------------------------------------------

    #[test]
    fn push_char_and_backspace_mutate_staged_not_current() {
        let mut model = model_with("abc", true);
        model.push_char('d');
        assert_eq!(model.fields[0].staged, FieldValue::Secret("abcd".into()));
        assert_eq!(model.fields[0].current, FieldValue::Secret("abc".into()));
        model.backspace();
        model.backspace();
        assert_eq!(model.fields[0].staged, FieldValue::Secret("ab".into()));
        assert_eq!(model.fields[0].current, FieldValue::Secret("abc".into()));
    }

    #[test]
    fn backspace_on_empty_secret_does_not_panic() {
        let mut model = model_with("", true);
        model.backspace();
        assert_eq!(model.fields[0].staged, FieldValue::Secret(String::new()));
    }

    #[test]
    fn toggle_bool_flips_staged_not_current() {
        let mut model = model_with("key", true);
        model.focus_next();
        model.toggle_bool();
        assert_eq!(model.fields[1].staged, FieldValue::Bool(false));
        assert_eq!(model.fields[1].current, FieldValue::Bool(true));
    }

    #[test]
    fn push_char_on_bool_field_is_noop() {
        let mut model = model_with("key", true);
        model.focus_next();
        model.push_char('x');
        assert_eq!(model.fields[1].staged, FieldValue::Bool(true));
    }

    // -- Commit / revert (per-field apply) --------------------------------

    #[test]
    fn focused_pending_edit_is_none_when_unchanged() {
        let model = model_with("key", true);
        assert!(model.focused_pending_edit().is_none());
    }

    #[test]
    fn focused_pending_edit_reflects_the_staged_secret_without_committing() {
        let mut model = model_with("old", true);
        model.push_char('!');
        let edit = model
            .focused_pending_edit()
            .expect("dirty field yields edit");
        assert_eq!(edit.key, "eumetnet.api_key");
        match edit.value {
            ConfigEditValue::Str(s) => assert_eq!(s, "old!"),
            _ => panic!("expected Str value"),
        }
        // Not committed: current is still the old value.
        assert_eq!(model.fields[0].current, FieldValue::Secret("old".into()));
    }

    #[test]
    fn focused_pending_edit_for_bool_yields_bool_value() {
        let mut model = model_with("key", true);
        model.focus_next();
        model.toggle_bool();
        let edit = model
            .focused_pending_edit()
            .expect("dirty bool yields edit");
        assert_eq!(edit.key, "location.ip_fallback");
        match edit.value {
            ConfigEditValue::Bool(b) => assert!(!b),
            _ => panic!("expected Bool value"),
        }
    }

    #[test]
    fn commit_focused_moves_staged_into_current() {
        let mut model = model_with("old", true);
        model.push_char('!');
        assert!(model.focused().is_dirty());
        model.commit_focused();
        assert!(!model.focused().is_dirty());
        assert_eq!(model.fields[0].current, FieldValue::Secret("old!".into()));
    }

    #[test]
    fn revert_focused_drops_the_staged_edit() {
        let mut model = model_with("old", true);
        model.push_char('!');
        assert!(model.focused().is_dirty());
        model.revert_focused();
        assert!(!model.focused().is_dirty());
        assert_eq!(model.fields[0].staged, FieldValue::Secret("old".into()));
    }

    // -- Debug hardening (F-3) -------------------------------------------

    #[test]
    fn debug_of_secret_field_value_masks_the_raw_value() {
        let fv = FieldValue::Secret("supersecretkey".to_string());
        let out = format!("{fv:?}");
        assert!(!out.contains("supersecretkey"), "raw secret leaked: {out}");
        assert!(out.contains("tkey"), "masked tail missing: {out}");
    }

    #[test]
    fn debug_of_settings_model_does_not_leak_the_raw_secret() {
        let model = model_with("supersecretkey", true);
        let out = format!("{model:?}");
        assert!(!out.contains("supersecretkey"), "raw secret leaked: {out}");
    }
}
