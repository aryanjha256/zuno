//! The certificates panel.
//!
//! A plain state struct rendered by a function rather than an `Entity`, the shape `close_panel`
//! uses and for its reason: this owns no text input, so its whole state is which row is
//! selected. The certificates themselves are read from `app_state` at render time rather than
//! copied in — the convention that a derived view cannot disagree with what it describes.
//!
//! **Two sections, because the two halves are not the same shape.** A client identity is a
//! radio: TLS presents one certificate per handshake, so exactly one can be active. Trusted
//! issuers are a set with every member active, because `add_root_certificate` is repeatable and
//! trust is additive. A single flat list would hide that difference, which is the one thing
//! somebody opening this panel needs to understand.

use gpui::{
    App, FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, div, px,
};
use std::path::{Path, PathBuf};
use zuno_core::TlsFiles;

use crate::actions::{CertsConfirm, CertsRemove};
use crate::theme::Theme;
use crate::workspace::Workspace;

const WIDTH: f32 = 520.;

/// What a row does when confirmed. Derived from the certificates on every render, so it cannot
/// describe a file that is no longer there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Present this identity. Carries the path rather than an index, so a list that changed
    /// under the selection cannot activate the wrong file.
    UseIdentity(PathBuf),
    /// Present none.
    UseNoIdentity,
    ChooseIdentity,
    /// A trusted issuer. Confirm does nothing; `delete` removes it.
    RootCa(PathBuf),
    ChooseRootCa,
}

pub struct CertPanel {
    pub selected: usize,
    pub restore_focus: Option<FocusHandle>,
    pub focus_handle: FocusHandle,
}

impl CertPanel {
    pub fn new(restore_focus: Option<FocusHandle>, cx: &mut App) -> Self {
        Self {
            selected: 0,
            restore_focus,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Every row, in drawn order.
    pub fn rows(files: &TlsFiles) -> Vec<Row> {
        let mut rows = vec![Row::UseNoIdentity];
        rows.extend(files.identities.iter().cloned().map(Row::UseIdentity));
        rows.push(Row::ChooseIdentity);
        rows.extend(files.root_cas.iter().cloned().map(Row::RootCa));
        rows.push(Row::ChooseRootCa);
        rows
    }

    pub fn step(&mut self, delta: isize, files: &TlsFiles) {
        let len = Self::rows(files).len() as isize;
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn row(&self, files: &TlsFiles) -> Option<Row> {
        Self::rows(files).get(self.selected).cloned()
    }
}

/// A path's filename. The whole path is in the tooltip's place — the row title — but the
/// leading directories are the same on every row and carry nothing.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub fn render(
    state: &CertPanel,
    files: &TlsFiles,
    theme: &Theme,
    cx: &mut gpui::Context<Workspace>,
) -> impl IntoElement {
    let rows = CertPanel::rows(files);
    let selected = state.selected;
    let active = files.identity.clone();

    div()
        .id("cert-panel-scrim")
        // A scrim that catches clicks does not stop the wheel: scroll handlers gate on the hit
        // test, not on propagation. Every overlay in this app needs this.
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .track_focus(&state.focus_handle)
                .key_context("CertPanel")
                .w(px(WIDTH))
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg_elevated)
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_sm()
                        .text_color(theme.text)
                        .child("Certificates"),
                )
                .child(heading("Client identity", "one at a time", theme))
                .children(
                    rows.iter()
                        .enumerate()
                        .filter(|(_, row)| is_identity(row))
                        .map(|(ix, row)| {
                            entry(ix, row.clone(), ix == selected, &active, theme, cx)
                        }),
                )
                .child(heading("Trusted issuers", "all active", theme))
                .children(
                    rows.iter()
                        .enumerate()
                        .filter(|(_, row)| !is_identity(row))
                        .map(|(ix, row)| {
                            entry(ix, row.clone(), ix == selected, &active, theme, cx)
                        }),
                )
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_size(px(10.))
                        .text_color(theme.text_faint)
                        .child("enter chooses · delete removes · escape closes"),
                ),
        )
}

fn is_identity(row: &Row) -> bool {
    matches!(
        row,
        Row::UseIdentity(_) | Row::UseNoIdentity | Row::ChooseIdentity
    )
}

fn heading(label: &'static str, note: &'static str, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_2()
        .px_3()
        .pt_2()
        .pb_1()
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .child(label),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .child(note),
        )
}

fn entry(
    ix: usize,
    row: Row,
    selected: bool,
    active: &Option<PathBuf>,
    theme: &Theme,
    cx: &mut gpui::Context<Workspace>,
) -> impl IntoElement + use<> {
    // The marker column is what makes the two shapes legible: a radio for the identity, nothing
    // for an issuer, since every issuer in the list is in force.
    let (marker, label, removable) = match &row {
        Row::UseNoIdentity => (
            if active.is_none() { "●" } else { "○" }.to_string(),
            "None".to_string(),
            false,
        ),
        Row::UseIdentity(path) => (
            if active.as_deref() == Some(path.as_path()) {
                "●"
            } else {
                "○"
            }
            .to_string(),
            name_of(path),
            true,
        ),
        Row::ChooseIdentity => (String::new(), "Choose a file…".to_string(), false),
        Row::RootCa(path) => (String::new(), name_of(path), true),
        Row::ChooseRootCa => (String::new(), "Choose a file…".to_string(), false),
    };

    let id = ("cert-row", ix);

    div()
        .id(id)
        .debug_selector(move || format!("cert-row-{ix}"))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .text_xs()
        .cursor_pointer()
        .bg(if selected { theme.bg_hover } else { theme.bg_elevated })
        .text_color(if selected { theme.text } else { theme.text_muted })
        .hover(|style| style.bg(theme.bg_hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                workspace.select_cert_row(ix, cx);
                window.dispatch_action(Box::new(CertsConfirm), cx);
            }),
        )
        .child(
            div()
                .flex_none()
                .w(px(12.))
                .text_color(theme.accent)
                .child(marker),
        )
        .child(div().flex_1().min_w(px(0.)).whitespace_nowrap().child(label))
        .children(removable.then(|| {
            div()
                .id(("cert-remove", ix))
                .debug_selector(move || format!("cert-remove-{ix}"))
                .flex_none()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .hover(|style| style.text_color(theme.status_server_error))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |workspace, _: &MouseDownEvent, window, cx| {
                        // A clickable inside a clickable: without this the row's own handler
                        // would also fire and confirm the row being removed.
                        cx.stop_propagation();
                        workspace.select_cert_row(ix, cx);
                        window.dispatch_action(Box::new(CertsRemove), cx);
                    }),
                )
                .child("remove")
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_describe_both_sections_and_their_shapes() {
        let files = TlsFiles {
            identity: Some(PathBuf::from("/k/a.pem")),
            identities: vec![PathBuf::from("/k/a.pem"), PathBuf::from("/k/b.pem")],
            root_cas: vec![PathBuf::from("/k/corp.pem")],
        };
        let rows = CertPanel::rows(&files);

        assert_eq!(
            rows,
            vec![
                Row::UseNoIdentity,
                Row::UseIdentity(PathBuf::from("/k/a.pem")),
                Row::UseIdentity(PathBuf::from("/k/b.pem")),
                Row::ChooseIdentity,
                Row::RootCa(PathBuf::from("/k/corp.pem")),
                Row::ChooseRootCa,
            ]
        );
        // Identity rows carry a path rather than an index, so a list that changed under the
        // selection cannot activate the wrong file.
        assert!(matches!(rows[1], Row::UseIdentity(_)));
    }

    #[test]
    fn an_empty_panel_still_offers_both_choosers() {
        // The cold-start state, and the reason the titlebar icon is permanent: with nothing
        // configured there must still be a way in.
        assert_eq!(
            CertPanel::rows(&TlsFiles::default()),
            vec![Row::UseNoIdentity, Row::ChooseIdentity, Row::ChooseRootCa]
        );
    }
}
