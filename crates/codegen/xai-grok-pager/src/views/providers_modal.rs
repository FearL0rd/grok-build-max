//! `/providers` failover panel -- view-state, rendering, and input handling.
//!
//! No I/O lives here: key handling returns [`ProvidersOp`] values and the
//! modal controller (input wrapper + dispatch) performs the `config.toml`
//! writes via [`xai_grok_shell::util::config`] writers and the
//! `x.ai/providers/reload` extension request, then rebuilds `entries` from
//! disk. This struct only tracks selection, ordering edits, and inline
//! text entry.

use crate::app::app_view::InputOutcome;
use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::render::SafeBuf;
use crate::theme::Theme;
use crate::views::modal_window::{
    self, ModalContentArea, ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::util::providers::{PRESETS, ProviderPreset};

/// Build panel state from the effective on-disk config. `[failover].order`
/// entries render in order; unreadable config yields an empty panel
/// (add-mode still works).
pub fn build_providers_modal_state(active_model: &str) -> ProvidersModalState {
    let entries = effective_config()
        .map(|cfg| provider_rows(&cfg, active_model))
        .unwrap_or_default();
    ProvidersModalState {
        window: ModalWindowState::new(),
        entries,
        selected: 0,
        scroll_offset: 0,
        active_model: active_model.to_owned(),
        last_rollover: None,
        mode: ModalMode::Normal,
        input: LineEditor::default(),
        pending_base_url: None,
        pending_name: None,
        pending_api_key: None,
    }
}

fn effective_config() -> Option<AgentConfig> {
    let raw = xai_grok_shell::config::load_effective_config().ok()?;
    AgentConfig::new_from_toml_cfg(&raw).ok()
}

/// One row per `[failover].order` entry, in order. Key state mirrors what
/// `build_failover_chain` will see, without resolving credentials: built-in
/// models (not a preset, not in `[model.*]`) authenticate via the session.
fn provider_rows(cfg: &AgentConfig, active_model: &str) -> Vec<ProviderRow> {
    cfg.failover
        .order
        .iter()
        .map(|name| {
            let override_ = cfg.config_models.get(name);
            let preset = PRESETS
                .iter()
                .find(|p| name.eq_ignore_ascii_case(p.short_key));
            let base_url = override_
                .and_then(|o| o.base_url.clone())
                .or_else(|| preset.map(|p| p.base_url.to_string()))
                .unwrap_or_default();
            let has_key = override_.is_some_and(|o| o.api_key.is_some())
                || (override_.is_none() && preset.is_none())
                || name.eq_ignore_ascii_case("grok");
            let keyless = override_.is_some_and(|o| o.keyless) || preset.is_some_and(|p| p.keyless);
            let model = override_.and_then(|o| o.model.clone());
            ProviderRow {
                name: name.clone(),
                base_url,
                has_key,
                keyless,
                model,
                is_active: name == active_model,
            }
        })
        .collect()
}

/// One provider row in the failover order.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    /// `[model.<name>]` config name (e.g. `"openai"`).
    pub name: String,
    pub base_url: String,
    pub has_key: bool,
    /// Providers that need no key (Ollama-style local servers).
    pub keyless: bool,
    /// The entry's model id from `[model.<name>].model`, when set.
    pub model: Option<String>,
    /// Matches the session's currently-selected model.
    pub is_active: bool,
}

/// Which field an inline text entry is targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    BaseUrl,
    ApiKey,
    /// Add-flow only: the `[model.<name>]` config key.
    ModelName,
    /// The provider's model id (`model = "..."` in `[model.<name>]`).
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalMode {
    Normal,
    /// Text input focused. `add` = creating a new provider (name ->
    /// base_url -> api_key -> model); otherwise editing the selected row
    /// (base_url -> api_key -> model).
    Editing {
        add: bool,
        field: EditField,
    },
}

/// A mutation the user confirmed; the dispatcher turns this into a config
/// write + `x.ai/providers/reload` effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvidersOp {
    Upsert {
        name: String,
        base_url: String,
        api_key: Option<String>,
        model: String,
        keyless: bool,
    },
    /// Guarded upstream: the `"grok"` built-in cannot be removed.
    Remove { name: String },
    /// Full post-swap `[failover].order`.
    Reorder { order: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvidersOutcome {
    Close,
    Changed,
    Unchanged,
    Op(ProvidersOp),
}

#[derive(Debug, Clone)]
pub struct ProvidersModalState {
    pub window: ModalWindowState,
    pub entries: Vec<ProviderRow>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub active_model: String,
    pub last_rollover: Option<String>,
    pub mode: ModalMode,
    pub(crate) input: LineEditor,
    /// Fields captured earlier in the current add/edit flow.
    pub(crate) pending_base_url: Option<String>,
    pub(crate) pending_name: Option<String>,
    /// `None` means "keep existing" in the edit flow; add flow stores the
    /// typed key here between the api_key and model steps.
    pub(crate) pending_api_key: Option<String>,
}

impl ProvidersModalState {
    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len.max(1));
        self.selected = (next as usize).min(self.entries.len() - 1);
    }

    /// Swap the selected row with the one below; returns the swapped indices
    /// for `reorder_failover`, or None when at/past the last row.
    pub fn swap_with_next(&mut self) -> Option<(usize, usize)> {
        if self.selected + 1 >= self.entries.len() {
            return None;
        }
        self.entries.swap(self.selected, self.selected + 1);
        let pair = (self.selected, self.selected + 1);
        self.selected += 1;
        Some(pair)
    }

    fn begin_add(&mut self) {
        self.input.set_text("");
        self.pending_base_url = None;
        self.pending_name = None;
        self.pending_api_key = None;
        self.mode = ModalMode::Editing {
            add: true,
            field: EditField::ModelName,
        };
    }

    fn begin_edit(&mut self, field: EditField) {
        // Prefill the current value so editing shows what is saved rather
        // than a blank field. base_url is the only field with a visible
        // current value here; api_key stays blank (secret; empty = keep).
        let prefill = match field {
            EditField::BaseUrl => self
                .entries
                .get(self.selected)
                .map(|r| r.base_url.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        self.input.set_text(prefill);
        self.pending_base_url = None;
        self.pending_name = None;
        self.pending_api_key = None;
        self.mode = ModalMode::Editing { add: false, field };
    }

    pub fn cancel_edit(&mut self) {
        self.mode = ModalMode::Normal;
        self.input.set_text("");
        self.pending_base_url = None;
        self.pending_name = None;
        self.pending_api_key = None;
    }

    fn preset_for(&self, name: &str) -> Option<&'static ProviderPreset> {
        PRESETS
            .iter()
            .find(|p| name.eq_ignore_ascii_case(p.short_key))
    }
}

/// Keys in Normal mode: navigation, reorder, add/edit/remove.
fn handle_normal_key(state: &mut ProvidersModalState, key: &KeyEvent) -> ProvidersOutcome {
    match key.code {
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            state.move_selection(1);
            ProvidersOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            state.move_selection(-1);
            ProvidersOutcome::Changed
        }
        KeyCode::Char('x') if key.modifiers.is_empty() => {
            if state.swap_with_next().is_some() {
                let order = state.entries.iter().map(|r| r.name.clone()).collect();
                ProvidersOutcome::Op(ProvidersOp::Reorder { order })
            } else {
                ProvidersOutcome::Unchanged
            }
        }
        KeyCode::Char('a') if key.modifiers.is_empty() => {
            state.begin_add();
            ProvidersOutcome::Changed
        }
        KeyCode::Char('e') if key.modifiers.is_empty() => {
            let Some(row) = state.entries.get(state.selected) else {
                return ProvidersOutcome::Unchanged;
            };
            let add_base_url = !row.base_url.is_empty();
            state.begin_edit(if add_base_url {
                EditField::BaseUrl
            } else {
                EditField::ApiKey
            });
            ProvidersOutcome::Changed
        }
        KeyCode::Char('r') if key.modifiers.is_empty() => {
            let Some(row) = state.entries.get(state.selected) else {
                return ProvidersOutcome::Unchanged;
            };
            // The Grok built-in authenticates via the session; dropping it
            // would leave no fallback. Remove is for [model.*] entries only.
            if row.name.eq_ignore_ascii_case("grok") {
                return ProvidersOutcome::Unchanged;
            }
            ProvidersOutcome::Op(ProvidersOp::Remove {
                name: row.name.clone(),
            })
        }
        KeyCode::Esc => ProvidersOutcome::Close,
        _ => ProvidersOutcome::Unchanged,
    }
}

/// Confirm the current text-entry field; advances the flow or emits an op.
fn confirm_field(state: &mut ProvidersModalState) -> ProvidersOutcome {
    let ModalMode::Editing { add, field } = state.mode else {
        return ProvidersOutcome::Unchanged;
    };
    let text = state.input.text().trim().to_owned();
    match (add, field) {
        // Add step 1: config name. Validate, then prefill base_url from the
        // matching preset so Enter accepts the default.
        (true, EditField::ModelName) => {
            if text.is_empty() || text.contains('"') || text.contains('\n') {
                return ProvidersOutcome::Changed;
            }
            let preset = state.preset_for(&text);
            state.pending_name = Some(text);
            state
                .input
                .set_text(preset.map(|p| p.base_url).unwrap_or_default());
            state.mode = ModalMode::Editing {
                add: true,
                field: EditField::BaseUrl,
            };
            ProvidersOutcome::Changed
        }
        // Add step 2: base_url (preset default prefilled). Keyless presets
        // skip straight to the model step.
        (true, EditField::BaseUrl) => {
            if text.is_empty() {
                return ProvidersOutcome::Changed;
            }
            let keyless = state
                .pending_name
                .as_deref()
                .and_then(|n| state.preset_for(n))
                .is_some_and(|p| p.keyless);
            state.pending_base_url = Some(text);
            if keyless {
                prefill_model_step(state, true);
            } else {
                state.input.set_text("");
                state.mode = ModalMode::Editing {
                    add: true,
                    field: EditField::ApiKey,
                };
            }
            ProvidersOutcome::Changed
        }
        // Add step 3: api_key (empty = none; keyless presets skip this step).
        (true, EditField::ApiKey) => {
            let api_key = if text.is_empty() { None } else { Some(text) };
            state.pending_api_key = api_key;
            prefill_model_step(state, true);
            ProvidersOutcome::Changed
        }
        // Add step 4: model id (preset default prefilled; Enter accepts it).
        (true, EditField::Model) => {
            let Some(name) = state.pending_name.clone() else {
                state.cancel_edit();
                return ProvidersOutcome::Changed;
            };
            let preset = state.preset_for(&name);
            let keyless = preset.is_some_and(|p| p.keyless);
            let model = if text.is_empty() {
                default_model(preset, &name)
            } else {
                text
            };
            let base_url = state.pending_base_url.clone().unwrap_or_default();
            let api_key = state.pending_api_key.take();
            state.cancel_edit();
            ProvidersOutcome::Op(ProvidersOp::Upsert {
                name,
                base_url,
                api_key,
                model,
                keyless,
            })
        }
        // Edit: base_url, then api_key, then model.
        (false, EditField::BaseUrl) => {
            if text.is_empty() {
                return ProvidersOutcome::Changed;
            }
            state.pending_base_url = Some(text);
            state.input.set_text("");
            state.mode = ModalMode::Editing {
                add: false,
                field: EditField::ApiKey,
            };
            ProvidersOutcome::Changed
        }
        (false, EditField::ApiKey) => {
            // Empty input keeps the existing key; typing replaces it.
            let api_key = if text.is_empty() { None } else { Some(text) };
            state.pending_api_key = api_key;
            prefill_model_step(state, false);
            ProvidersOutcome::Changed
        }
        (false, EditField::Model) => {
            let Some(row) = state.entries.get(state.selected).cloned() else {
                state.cancel_edit();
                return ProvidersOutcome::Changed;
            };
            let preset = state.preset_for(&row.name);
            let keyless = row.keyless || preset.is_some_and(|p| p.keyless);
            let model = if text.is_empty() {
                row.model
                    .clone()
                    .unwrap_or_else(|| default_model(preset, &row.name))
            } else {
                text
            };
            let base_url = state
                .pending_base_url
                .clone()
                .unwrap_or_else(|| row.base_url.clone());
            // None means "keep current key" at the writer level.
            let api_key = state.pending_api_key.take();
            state.cancel_edit();
            ProvidersOutcome::Op(ProvidersOp::Upsert {
                name: row.name,
                base_url,
                api_key,
                model,
                keyless,
            })
        }
        // ModelName is add-flow only; edit never targets it.
        (false, EditField::ModelName) => ProvidersOutcome::Unchanged,
    }
}

/// Prefill for the model step: presets suggest their model, custom entries
/// default to the config name.
fn default_model(preset: Option<&'static ProviderPreset>, name: &str) -> String {
    preset
        .map(|p| p.suggested_model.to_string())
        .unwrap_or_else(|| name.to_owned())
}

/// Enter the model step, prefilled with the row's current model (edit) or the
/// preset/name default (add).
fn prefill_model_step(state: &mut ProvidersModalState, add: bool) {
    let current = if add {
        let name = state.pending_name.clone().unwrap_or_default();
        default_model(state.preset_for(&name), &name)
    } else {
        state
            .entries
            .get(state.selected)
            .map(|r| {
                r.model
                    .clone()
                    .unwrap_or_else(|| default_model(state.preset_for(&r.name), &r.name))
            })
            .unwrap_or_default()
    };
    state.input.set_text(&current);
    state.mode = ModalMode::Editing {
        add,
        field: EditField::Model,
    };
}

/// Keys while the text input is focused.
fn handle_editing_key(state: &mut ProvidersModalState, key: &KeyEvent) -> ProvidersOutcome {
    match key.code {
        KeyCode::Esc => {
            state.cancel_edit();
            ProvidersOutcome::Changed
        }
        KeyCode::Enter => confirm_field(state),
        _ => {
            let _ = state.input.handle_key(key);
            ProvidersOutcome::Changed
        }
    }
}

/// Handle a key event for the `/providers` panel.
pub fn handle_providers_key(state: &mut ProvidersModalState, key: &KeyEvent) -> ProvidersOutcome {
    if key.kind == KeyEventKind::Release {
        return ProvidersOutcome::Unchanged;
    }
    if matches!(state.mode, ModalMode::Editing { .. }) {
        // Ctrl shortcuts stay with the global registry; plain keys go to the
        // input line.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return ProvidersOutcome::Unchanged;
        }
        return handle_editing_key(state, key);
    }
    handle_normal_key(state, key)
}

/// Paste into the inline text entry (editing mode only).
pub fn handle_providers_paste(state: &mut ProvidersModalState, text: &str) -> InputOutcome {
    if matches!(state.mode, ModalMode::Editing { .. }) {
        let _ = state.input.insert_paste(text);
        InputOutcome::Changed
    } else {
        InputOutcome::Unchanged
    }
}

fn edit_prompt(state: &ProvidersModalState) -> String {
    let ModalMode::Editing { add, field } = state.mode else {
        return String::new();
    };
    match (add, field) {
        (true, EditField::ModelName) => "name (e.g. openai, ollama-local): ".to_owned(),
        (true, EditField::BaseUrl) => "base_url: ".to_owned(),
        (true, EditField::ApiKey) => "api_key (empty = none): ".to_owned(),
        (true, EditField::Model) => "model (Enter = default): ".to_owned(),
        (false, EditField::BaseUrl) => "base_url: ".to_owned(),
        (false, EditField::ApiKey) => {
            if state.entries.get(state.selected).is_some_and(|r| r.has_key) {
                "api_key (empty = keep current): ".to_owned()
            } else {
                "api_key: ".to_owned()
            }
        }
        (false, EditField::Model) => "model (empty = keep): ".to_owned(),
        (false, EditField::ModelName) => String::new(),
    }
}

/// Render the `/providers` panel: failover order rows with key state, the
/// active model marker, inline text entry, and a rollover notice line.
pub fn render_providers_modal(
    buf: &mut Buffer,
    full_area: Rect,
    state: &mut ProvidersModalState,
    compact: bool,
    theme: &Theme,
) {
    let title = format!("Providers \u{2014} active: {}", state.active_model);
    let shortcuts = build_shortcuts(&state.mode);
    let modal_config = ModalWindowConfig {
        title: &title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing {
            width_pct: 0.7,
            max_width: 110,
            min_width: 44,
            v_margin: 4,
            h_pad: 2,
            v_pad: 1,
            footer_lines: 2,
        }
        .with_compact(compact),
        fold_info: None,
    };

    let Some(ModalContentArea {
        content: content_area,
        ..
    }) = modal_window::render_modal_window(buf, full_area, &mut state.window, &modal_config, theme)
    else {
        return;
    };
    if content_area.height < 1 || content_area.width < 10 {
        return;
    }
    buf.set_style(content_area, Style::default().bg(theme.bg_base));

    let mut y = content_area.y;
    // Rollover notice, when one arrived while the panel is open.
    if let Some(ref note) = state.last_rollover
        && y < content_area.y + content_area.height
    {
        let line = Line::from(Span::styled(
            truncate_str(note, content_area.width as usize).to_owned(),
            Style::default().fg(theme.accent_running).bg(theme.bg_base),
        ));
        buf.set_line(content_area.x, y, &line, content_area.width);
        y += 1;
    }

    let editing = matches!(state.mode, ModalMode::Editing { .. });
    let rows_height = (content_area.y + content_area.height).saturating_sub(y);
    let available = rows_height.saturating_sub(if editing { 1 } else { 0 }) as usize;
    if state.selected < state.scroll_offset {
        state.scroll_offset = state.selected;
    }
    if state.selected >= state.scroll_offset.saturating_add(available) && available > 0 {
        state.scroll_offset = state.selected + 1 - available;
    }

    let end = state.entries.len().min(state.scroll_offset + available);
    for (i, row) in state.entries[state.scroll_offset..end].iter().enumerate() {
        let y = y + i as u16;
        let is_selected = state.scroll_offset + i == state.selected;
        let bg = if is_selected {
            theme.bg_visual
        } else {
            theme.bg_base
        };
        buf.set_style(
            Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: 1,
            },
            Style::default().bg(bg),
        );

        let marker = if row.is_active { "\u{25CF}" } else { " " };
        let name_style = Style::default()
            .fg(if row.is_active {
                theme.accent_user
            } else {
                theme.text_primary
            })
            .bg(bg)
            .add_modifier(if row.is_active {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        buf.set_span(
            content_area.x,
            y,
            &Span::styled(format!("{marker} {}", row.name), name_style),
            content_area.width,
        );

        let key_label = if row.keyless {
            "keyless"
        } else if row.has_key {
            "key set"
        } else {
            "no key"
        };
        let key_style = Style::default()
            .fg(if row.has_key || row.keyless {
                theme.gray
            } else {
                theme.accent_error
            })
            .bg(bg);
        let key_w = key_label.len() as u16;
        buf.set_span(
            content_area.x + content_area.width.saturating_sub(key_w),
            y,
            &Span::styled(key_label, key_style),
            key_w,
        );

        let url_x = content_area.x + row.name.len() as u16 + 3;
        let url_max = content_area
            .width
            .saturating_sub(key_w + (url_x - content_area.x) + 1) as usize;
        if url_max > 1 {
            let url = truncate_str(&row.base_url, url_max);
            buf.set_span(
                url_x,
                y,
                &Span::styled(url.to_owned(), Style::default().fg(theme.gray).bg(bg)),
                url_max as u16,
            );
        }
    }

    // Inline text entry line at the bottom.
    if editing {
        let y = content_area.y + content_area.height - 1;
        let prompt = edit_prompt(state);
        buf.set_span(
            content_area.x,
            y,
            &Span::styled(
                truncate_str(&prompt, content_area.width as usize).to_owned(),
                Style::default().fg(theme.accent_user).bg(theme.bg_base),
            ),
            content_area.width,
        );
        let prompt_w = prompt.len().min(content_area.width as usize);
        let input_w = content_area.width as usize - prompt_w;
        if input_w > 0 {
            let viewport = state.input.viewport(input_w);
            let visible = state.input.text()[viewport.visible_byte_range.clone()].to_owned();
            buf.set_span(
                content_area.x + prompt_w as u16,
                y,
                &Span::styled(
                    visible,
                    Style::default().fg(theme.text_primary).bg(theme.bg_base),
                ),
                input_w as u16,
            );
            let cursor_x = content_area.x + prompt_w as u16 + viewport.cursor_display_column as u16;
            if cursor_x < content_area.x + content_area.width
                && let Some(cell) = buf.cell_mut((cursor_x, y))
            {
                cell.set_style(Style::default().fg(theme.bg_base).bg(theme.text_primary));
            }
        }
    }
}

fn build_shortcuts(mode: &ModalMode) -> Vec<Shortcut<'static>> {
    if matches!(mode, ModalMode::Editing { .. }) {
        return vec![
            Shortcut {
                label: "Enter save",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc cancel",
                clickable: false,
                id: 0,
            },
        ];
    }
    vec![
        Shortcut {
            label: "\u{2191}/\u{2193} move",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "x swap down",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "a add",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "e edit",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "r remove",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 0,
        },
    ]
}

fn truncate_str(s: &str, max_width: usize) -> &str {
    let offset = crate::render::line_utils::byte_offset_at_width(s, max_width);
    &s[..offset]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_rows(n: usize) -> ProvidersModalState {
        ProvidersModalState {
            window: ModalWindowState::new(),
            entries: (0..n)
                .map(|i| ProviderRow {
                    name: format!("prov{i}"),
                    base_url: format!("http://p{i}.example"),
                    has_key: i % 2 == 1,
                    is_active: false,
                    keyless: false,
                    model: None,
                })
                .collect(),
            selected: 0,
            scroll_offset: 0,
            active_model: "grok".into(),
            last_rollover: None,
            mode: ModalMode::Normal,
            input: LineEditor::default(),
            pending_base_url: None,
            pending_name: None,
            pending_api_key: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selection_moves_and_wraps_backward() {
        let mut st = state_with_rows(3);
        st.move_selection(5);
        assert_eq!(st.selected, 2);
        st.move_selection(-9);
        assert_eq!(st.selected, 2);
        st.move_selection(-2);
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn selection_wraps_forward() {
        let mut st = state_with_rows(3);
        st.move_selection(-1);
        assert_eq!(st.selected, 2);
    }

    #[test]
    fn selection_on_empty_is_noop() {
        let mut st = state_with_rows(0);
        st.move_selection(1);
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn swap_updates_order_and_reports_indices() {
        let mut st = state_with_rows(2);
        let swapped = st.swap_with_next();
        assert_eq!(swapped, Some((0, 1)));
        assert_eq!(st.entries[0].name, "prov1");
        assert_eq!(st.entries[1].name, "prov0");
    }

    #[test]
    fn swap_at_last_row_is_noop() {
        let mut st = state_with_rows(2);
        st.selected = 1;
        assert_eq!(st.swap_with_next(), None);
    }

    #[test]
    fn add_and_edit_modes_transition() {
        let mut st = state_with_rows(1);
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Char('a'))),
            ProvidersOutcome::Changed
        );
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: true,
                field: EditField::ModelName
            }
        );
        handle_providers_key(&mut st, &key(KeyCode::Esc));
        assert_eq!(st.mode, ModalMode::Normal);
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Char('e'))),
            ProvidersOutcome::Changed
        );
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: false,
                field: EditField::BaseUrl
            }
        );
    }

    #[test]
    fn x_swap_emits_reorder_op() {
        let mut st = state_with_rows(2);
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Char('x')));
        assert_eq!(
            outcome,
            ProvidersOutcome::Op(ProvidersOp::Reorder {
                order: vec!["prov1".into(), "prov0".into()]
            })
        );
        assert_eq!(st.selected, 1);
    }

    #[test]
    fn remove_grok_is_refused() {
        let mut st = state_with_rows(2);
        st.entries[0].name = "grok".into();
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Char('r')));
        assert_eq!(outcome, ProvidersOutcome::Unchanged);
    }

    #[test]
    fn remove_custom_emits_op() {
        let mut st = state_with_rows(1);
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Char('r')));
        assert_eq!(
            outcome,
            ProvidersOutcome::Op(ProvidersOp::Remove {
                name: "prov0".into()
            })
        );
    }

    #[test]
    fn esc_in_normal_closes() {
        let mut st = state_with_rows(1);
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Esc)),
            ProvidersOutcome::Close
        );
    }

    #[test]
    fn add_flow_collects_name_url_key() {
        let mut st = state_with_rows(1);
        handle_providers_key(&mut st, &key(KeyCode::Char('a')));
        // Type the preset name; Enter prefills base_url from the preset.
        for ch in "openai".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Enter)),
            ProvidersOutcome::Changed
        );
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: true,
                field: EditField::BaseUrl
            }
        );
        assert_eq!(st.input.text(), "https://api.openai.com/v1");
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Enter)),
            ProvidersOutcome::Changed
        );
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: true,
                field: EditField::ApiKey
            }
        );
        for ch in "sk-test".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        // api_key confirm moves to the model step, prefilled with the
        // preset's suggested model.
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Enter)),
            ProvidersOutcome::Changed
        );
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: true,
                field: EditField::Model
            }
        );
        let default_model = PRESETS
            .iter()
            .find(|p| p.short_key == "openai")
            .map(|p| p.suggested_model.to_string())
            .unwrap();
        assert_eq!(st.input.text(), default_model);
        // Enter accepts the prefill.
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            ProvidersOutcome::Op(ProvidersOp::Upsert {
                name: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: Some("sk-test".into()),
                model: default_model,
                keyless: false,
            })
        );
        assert_eq!(st.mode, ModalMode::Normal);
    }

    #[test]
    fn add_flow_model_step_accepts_custom_model() {
        let mut st = state_with_rows(1);
        handle_providers_key(&mut st, &key(KeyCode::Char('a')));
        for ch in "openai".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // name
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // base_url
        for ch in "sk-x".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // api_key
        st.input.set_text(""); // replace the preset prefill
        for ch in "gpt-4o-mini".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Enter)); // model
        match outcome {
            ProvidersOutcome::Op(ProvidersOp::Upsert { model, api_key, .. }) => {
                assert_eq!(model, "gpt-4o-mini");
                assert_eq!(api_key, Some("sk-x".into()));
            }
            other => panic!("expected Upsert op, got {other:?}"),
        }
    }

    #[test]
    fn add_flow_keyless_skips_api_key_step() {
        let mut st = state_with_rows(1);
        handle_providers_key(&mut st, &key(KeyCode::Char('a')));
        for ch in "ollama-local".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // name
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // base_url
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: true,
                field: EditField::Model
            },
            "keyless preset must skip the api_key step"
        );
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Enter)); // model
        match outcome {
            ProvidersOutcome::Op(ProvidersOp::Upsert {
                api_key, keyless, ..
            }) => {
                assert_eq!(api_key, None);
                assert!(keyless);
            }
            other => panic!("expected Upsert op, got {other:?}"),
        }
    }

    /// The edit flow prefills base_url with the row's current value; change
    /// it by clearing the field first (backspaces).
    fn clear_prefill(st: &mut ProvidersModalState) {
        assert!(!st.input.text().is_empty(), "base_url should be prefilled");
        while !st.input.text().is_empty() {
            let _ = handle_providers_key(st, &key(KeyCode::Backspace));
        }
        assert!(st.input.text().is_empty());
    }

    #[test]
    fn edit_flow_prefills_current_base_url() {
        let mut st = state_with_rows(1);
        handle_providers_key(&mut st, &key(KeyCode::Char('e')));
        assert_eq!(st.input.text(), "http://p0.example");
        // Enter accepts the prefill unchanged: Upsert keeps the same url.
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // base_url
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // api_key keep
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Enter)); // model
        match outcome {
            ProvidersOutcome::Op(ProvidersOp::Upsert { base_url, .. }) => {
                assert_eq!(base_url, "http://p0.example");
            }
            other => panic!("expected Upsert op, got {other:?}"),
        }
    }

    #[test]
    fn edit_flow_preserves_existing_model() {
        let mut st = state_with_rows(1);
        st.entries[0].model = Some("my-model-v2".into());
        handle_providers_key(&mut st, &key(KeyCode::Char('e')));
        clear_prefill(&mut st);
        for ch in "https://proxy.example/v1".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // base_url
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter)); // api_key empty = keep
        // Model step prefilled with the existing model; empty Enter keeps it.
        assert_eq!(
            st.mode,
            ModalMode::Editing {
                add: false,
                field: EditField::Model
            }
        );
        assert_eq!(st.input.text(), "my-model-v2");
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Enter));
        match outcome {
            ProvidersOutcome::Op(ProvidersOp::Upsert {
                model, base_url, ..
            }) => {
                assert_eq!(model, "my-model-v2");
                assert_eq!(base_url, "https://proxy.example/v1");
            }
            other => panic!("expected Upsert op, got {other:?}"),
        }
    }

    #[test]
    fn edit_flow_keeps_key_when_left_empty() {
        let mut st = state_with_rows(1);
        st.entries[0].has_key = true;
        handle_providers_key(&mut st, &key(KeyCode::Char('e')));
        clear_prefill(&mut st);
        for ch in "https://proxy.example/v1".chars() {
            let _ = handle_providers_key(&mut st, &key(KeyCode::Char(ch)));
        }
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter));
        // Empty api_key input keeps the existing key (api_key: None means
        // "keep" at the op level; the writer preserves it upstream).
        let _ = handle_providers_key(&mut st, &key(KeyCode::Enter));
        // Empty model input keeps the row's model (prefill accepted).
        let outcome = handle_providers_key(&mut st, &key(KeyCode::Enter));
        match outcome {
            ProvidersOutcome::Op(ProvidersOp::Upsert {
                base_url, api_key, ..
            }) => {
                assert_eq!(base_url, "https://proxy.example/v1");
                assert_eq!(api_key, None);
            }
            other => panic!("expected Upsert op, got {other:?}"),
        }
    }

    #[test]
    fn esc_during_edit_cancels_not_closes() {
        let mut st = state_with_rows(1);
        handle_providers_key(&mut st, &key(KeyCode::Char('a')));
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Esc)),
            ProvidersOutcome::Changed
        );
        assert_eq!(st.mode, ModalMode::Normal);
        // Second Esc now closes.
        assert_eq!(
            handle_providers_key(&mut st, &key(KeyCode::Esc)),
            ProvidersOutcome::Close
        );
    }

    #[test]
    fn paste_only_in_editing() {
        let mut st = state_with_rows(1);
        assert!(matches!(
            handle_providers_paste(&mut st, "x"),
            InputOutcome::Unchanged
        ));
        handle_providers_key(&mut st, &key(KeyCode::Char('a')));
        assert!(matches!(
            handle_providers_paste(&mut st, "openai"),
            InputOutcome::Changed
        ));
        assert_eq!(st.input.text(), "openai");
    }

    fn cfg_from(toml_src: &str) -> AgentConfig {
        let raw: toml::Value = toml::from_str(toml_src).unwrap();
        AgentConfig::new_from_toml_cfg(&raw).unwrap()
    }

    #[test]
    fn rows_reflect_order_and_key_state() {
        let cfg = cfg_from(
            r#"
[failover]
order = ["grok", "openai", "ollama-local"]

[model.openai]
api_key = "sk-test"
base_url = "https://custom.openai/v1"
model = "gpt-x"

[model.ollama-local]
keyless = true
"#,
        );
        let rows = provider_rows(&cfg, "grok");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "grok");
        assert!(rows[0].has_key && rows[0].is_active);
        // User base_url wins over the preset default.
        assert_eq!(rows[1].base_url, "https://custom.openai/v1");
        assert_eq!(rows[1].model.as_deref(), Some("gpt-x"));
        assert!(rows[1].has_key && !rows[1].is_active);
        assert!(rows[2].keyless && !rows[2].has_key);
        assert_eq!(rows[2].base_url, "http://localhost:11434/v1");
    }
}
