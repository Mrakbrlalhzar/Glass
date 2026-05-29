//! About-dialog and annotations-pane state methods.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block — Rust allows
//! multiple `impl Shell` blocks across files in the same crate,
//! so the existing call sites continue to work without renames.
//!
//! Scope: open / close for the About dialog and the annotations
//! pane, horizontal scroll for the annotations pane, and the
//! per-key `navigate_to_annotation` dispatcher that opens the
//! right view (listing / hex / smali / op-line) for whichever
//! `AnnotationKey` the user clicked.

use gpui::{px, Context, Pixels};

use crate::Shell;

impl Shell {
    pub(crate) fn open_about(&mut self, cx: &mut Context<Self>) {
        if !self.about_open {
            self.about_open = true;
            cx.notify();
        }
    }

    pub(crate) fn close_about(&mut self, cx: &mut Context<Self>) {
        if self.about_open {
            self.about_open = false;
            cx.notify();
        }
    }

    pub(crate) fn close_annotations_pane(&mut self, cx: &mut Context<Self>) {
        if self.annotations_pane_open {
            self.annotations_pane_open = false;
            self.save_state();
            cx.notify();
        }
    }

    /// Scroll the annotations-pane horizontally by `dx` (positive
    /// = scroll right). Clamps to [0, max_offset].
    pub(crate) fn scroll_annotations_pane_h(
        &mut self,
        dx: Pixels,
        max_offset: Pixels,
        cx: &mut Context<Self>,
    ) {
        let new = (self.annotations_pane_h_offset + dx).clamp(px(0.), max_offset);
        if new != self.annotations_pane_h_offset {
            self.annotations_pane_h_offset = new;
            cx.notify();
        }
    }

    // Used by Phase 4 (edge-icon click + write auto-open). Kept
    // for that wiring even though no current caller exercises it.
    #[allow(dead_code)]
    pub(crate) fn open_annotations_pane(&mut self, cx: &mut Context<Self>) {
        if !self.annotations_pane_open {
            self.annotations_pane_open = true;
            self.save_state();
            cx.notify();
        }
    }

    /// Click handler for an annotations-pane entry. Opens the
    /// appropriate view for the key kind: address → listing, symbol
    /// → resolve through the artifact's symbol map then listing,
    /// class / method → smali tab.
    pub(crate) fn navigate_to_annotation(
        &mut self,
        artifact: glass_db::ArtifactId,
        key: glass_db::AnnotationKey,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle().cloned() else { return };
        match key {
            glass_db::AnnotationKey::Address(addr) => {
                if let Some(section) =
                    bundle.text_section_for_addr(&artifact, addr)
                {
                    let section = section.to_string();
                    self.open_listing_at(artifact, section, addr, cx);
                } else if let Some(section) =
                    bundle.data_section_for_addr(&artifact, addr)
                {
                    let section = section.to_string();
                    self.open_hex_in_new_tab(artifact, section, addr, cx);
                }
            }
            glass_db::AnnotationKey::Symbol(name) => {
                let Some(sm) = bundle.symbol_maps.get(&artifact) else { return };
                let Some(sym) = sm.iter().find(|s| {
                    s.display_name == name || s.name == name
                }) else {
                    return;
                };
                let addr = sym.address;
                if let Some(section) =
                    bundle.text_section_for_addr(&artifact, addr)
                {
                    let section = section.to_string();
                    self.open_listing_at(artifact, section, addr, cx);
                }
            }
            glass_db::AnnotationKey::Class(class_jni)
            | glass_db::AnnotationKey::Method(class_jni, _) => {
                let leaf = bundle.resolve(&glass_db::TabState::SmaliClass {
                    class_jni: class_jni.clone(),
                    scroll_line: 0,
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
            glass_db::AnnotationKey::MethodLine(class_jni, method_decl, line_offset) => {
                // Look up the `.method` line in the smali body
                // through the pre-built method-line index, then
                // scroll the smali tab to header + line_offset.
                let method_key = format!("{class_jni}->{method_decl}");
                let Some((leaf, header_line)) =
                    bundle.method_lines.get(&method_key).copied()
                else {
                    // Fall back to opening the class — method
                    // index may not have been built (e.g. native).
                    if let Some(leaf) = bundle.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                        scroll_line: 0,
                    }) {
                        self.open_leaf(leaf, cx);
                    }
                    return;
                };
                let target_line = header_line + line_offset as usize;
                self.goto_smali_method(leaf, target_line, cx);
            }
            glass_db::AnnotationKey::OpIndex {
                class_jni,
                method_decl,
                op_index,
            } => {
                // Resolve the class's leaf + the method header line,
                // then render the method and walk to find the line
                // offset where op `op_index` lands.
                let method_key = format!("{class_jni}->{method_decl}");
                let Some((leaf, header_line)) =
                    bundle.method_lines.get(&method_key).copied()
                else {
                    if let Some(leaf) = bundle.resolve(&glass_db::TabState::SmaliClass {
                        class_jni: class_jni.clone(),
                        scroll_line: 0,
                    }) {
                        self.open_leaf(leaf, cx);
                    }
                    return;
                };
                // Find the SmaliMethod so we can map op index back
                // to a line offset.
                let target_line = bundle.smali_classes.iter().find_map(
                    |((_aid, jni), c)| {
                        if jni != &class_jni {
                            return None;
                        }
                        let m = c.methods.iter().find(|m| {
                            format!("{}{}", m.name, m.signature.to_jni())
                                == method_decl
                        })?;
                        crate::annotations::op_index_to_line_offset(m, op_index)
                            .map(|off| header_line + off as usize)
                    },
                );
                match target_line {
                    Some(line) => self.goto_smali_method(leaf, line, cx),
                    None => self.open_leaf(leaf, cx),
                }
            }
        }
    }
}
