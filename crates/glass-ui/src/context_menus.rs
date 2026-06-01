//! Right-click context-menu builders and dispatcher.
//!
//! Lifted out of `shell_actions.rs` so the file stays under the
//! module-size discipline. The methods are still defined on
//! `Shell` via a sibling `impl Shell` block — Rust allows
//! multiple `impl Shell` blocks across files in the same crate,
//! so the existing call sites continue to work without renames.
//!
//! Scope: the per-surface `open_*_context_menu` constructors
//! (listing rows, smali lines, smali class headers, address
//! links, fields, method headers, smali links), plus the small
//! `close_context_menu` / `activate_context_menu_item` glue that
//! dismisses the menu and dispatches the chosen `ContextMenuItem`
//! to its action handler. The action handlers themselves
//! (`activate_follow`, `show_cfg`, `open_xrefs_to_address`,
//! `begin_annotation_edit`, …) still live in `shell_actions.rs`.

use gpui::{Context, Pixels, SharedString};

use crate::context_menu::{ContextMenuItem, ContextMenuState};
use crate::shell_actions::preview_string_at;
use crate::{LeafId, Shell, TabKind};

impl Shell {
    /// Right-click handler invoked from a Listing row. Offers Show
    /// CFG + Callers of function when the row is inside a known
    /// symbol; a generic References to address otherwise.
    pub(crate) fn open_listing_context_menu(
        &mut self,
        artifact: glass_db::ArtifactId,
        addr: u64,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = self.bundle() else { return };
        let covering = bundle
            .symbol_maps
            .get(&artifact)
            .and_then(|sm| sm.covering(addr));
        let mut items = Vec::new();
        // Copy the formatted listing row, if we have rendered rows
        // for the active tab and the row at `addr` exists. Lives
        // at the top so it's the first item users see.
        if let Some(copy_text) = self.copy_text_for_listing_addr(&artifact, addr) {
            items.push(ContextMenuItem::CopyText {
                text: copy_text,
                label: SharedString::from(format!("0x{addr:x}")),
            });
        }
        // 1) Top items depend on what kind of thing the click
        //    landed on:
        //    - Function symbol → Show CFG + Callers of function
        //    - Object (data) symbol → References to <name>
        //    - No covering symbol → References to 0x<addr>
        match covering {
            Some(sym) if matches!(sym.kind, glass_arch_arm::SymbolKind::Function) => {
                let label = SharedString::from(sym.display_name.clone());
                let entry_addr = sym.address;
                items.push(ContextMenuItem::ShowCfg {
                    artifact: artifact.clone(),
                    entry_addr,
                    label: label.clone(),
                });
                items.push(ContextMenuItem::CallersOfFunction {
                    artifact: artifact.clone(),
                    entry_addr,
                    label,
                });
            }
            Some(sym) => {
                // Data symbol — xrefs scoped to the symbol's
                // entry address so e.g. ADRP+ADD pairs pointing
                // at this string show up.
                items.push(ContextMenuItem::XrefsToAddress {
                    artifact: artifact.clone(),
                    addr: sym.address,
                    label: SharedString::from(sym.display_name.clone()),
                });
            }
            None => {
                // No covering symbol — but if the click landed
                // inside a recognisable data item (e.g. a C string
                // in `__cstring` with no symtab entry), use the
                // item's start address so the xref query matches
                // the address recorded by ADRP+ADD resolution.
                let (query_addr, label) = match crate::listing_render::item_extent_for(
                    bundle,
                    &artifact,
                    addr,
                ) {
                    Some((start, _end)) if start != addr => {
                        // Show a short string preview when it's a
                        // strings-section item the user clicked
                        // into the middle of.
                        let preview = preview_string_at(bundle, &artifact, start);
                        let label_text = match preview {
                            Some(s) => format!("\"{s}\""),
                            None => format!("0x{start:x}"),
                        };
                        (start, SharedString::from(label_text))
                    }
                    Some((start, _end)) => {
                        let preview = preview_string_at(bundle, &artifact, start);
                        let label_text = match preview {
                            Some(s) => format!("\"{s}\""),
                            None => format!("0x{start:x}"),
                        };
                        (start, SharedString::from(label_text))
                    }
                    None => (addr, SharedString::from(format!("0x{addr:x}"))),
                };
                items.push(ContextMenuItem::XrefsToAddress {
                    artifact: artifact.clone(),
                    addr: query_addr,
                    label,
                });
            }
        }
        // 1b) Open the same address as a hex view. Useful when the
        // typed-assembly editor can't express what the user wants
        // (custom byte sequences, padding NOPs, encodings the
        // grammar doesn't cover yet). The hex tab dedupes by
        // section, so a section's hex view + listing view can
        // coexist as two tabs.
        if let Some(section) = bundle.text_section_for_addr(&artifact, addr) {
            items.push(ContextMenuItem::OpenHexHere {
                artifact: artifact.clone(),
                section: section.to_string(),
                addr,
                label: SharedString::from(format!("0x{addr:x}")),
            });
        }
        // 2) Annotation items. Always address-keyed: the user
        //    right-clicked a specific row, so that row is the
        //    intent. Function-level tagging is still possible —
        //    just right-click the function's entry row (its
        //    address is the same one the SymbolHeader covers).
        let (annot_key, annot_label) =
            (glass_db::AnnotationKey::Address(addr), format!("0x{addr:x}"));
        let existing = bundle
            .annotations
            .get(&artifact)
            .and_then(|idx| match &annot_key {
                glass_db::AnnotationKey::Address(a) => idx.at_address(*a),
                glass_db::AnnotationKey::Symbol(s) => idx.at_symbol(s),
                glass_db::AnnotationKey::Class(c) => idx.at_class(c),
                glass_db::AnnotationKey::Method(c, m) => {
                    idx.at_method(&format!("{c}->{m}"))
                }
                glass_db::AnnotationKey::MethodLine(c, m, line) => {
                    idx.at_method_line(&format!("{c}->{m}"), *line)
                }
                glass_db::AnnotationKey::OpIndex {
                    class_jni, method_decl, op_index,
                } => idx.at_op_index(
                    &format!("{class_jni}->{method_decl}"),
                    *op_index,
                ),
            })
            .cloned()
            .unwrap_or_default();
        let comment_label = if existing.comment.is_some() {
            "Edit comment…"
        } else {
            "Add comment…"
        };
        items.push(ContextMenuItem::EditComment {
            artifact: artifact.clone(),
            key: annot_key.clone(),
            current: existing.comment.clone().unwrap_or_default(),
            label: SharedString::from(comment_label),
        });
        items.push(ContextMenuItem::PickColour {
            artifact: artifact.clone(),
            key: annot_key.clone(),
            current: existing.colour,
            label: SharedString::from("Set colour…"),
        });
        // 3) Revert staged disasm edit, if any.
        let has_edit = bundle.edits.get(&artifact, addr).is_some();
        if has_edit {
            items.push(ContextMenuItem::RevertDisasmEdit {
                artifact: artifact.clone(),
                vaddr: addr,
                label: SharedString::from(format!("Revert change ({annot_label})")),
            });
        }
        if !existing.is_empty() {
            items.push(ContextMenuItem::ClearAnnotation {
                artifact,
                key: annot_key,
                label: SharedString::from(format!("Clear annotation ({annot_label})")),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a line in a SmaliClass listing → context menu
    /// offering "Show call graph" for the method that contains the
    /// line. The caller determined the method by scanning upward.
    pub(crate) fn open_smali_context_menu(
        &mut self,
        class_jni: String,
        method_decl: String,
        line_offset: u32,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Display name: just the method name (no signature) to keep
        // the menu line readable.
        let display = method_decl
            .split('(')
            .next()
            .unwrap_or(&method_decl)
            .to_string();
        let label = SharedString::from(display.clone());
        let method_key = format!("{class_jni}->{method_decl}");
        // For annotation lookup we need an artifact id. DEX
        // artifacts share the bundle's first DEX artifact id; pick
        // the first one in the bundle's artifact list as the
        // canonical DEX target.
        let dex_artifact = self
            .bundle()
            .and_then(|b| b.artifact_ids.first().cloned());
        let mut items = vec![
            ContextMenuItem::CopyText {
                text: method_key.clone(),
                label: label.clone(),
            },
            ContextMenuItem::ShowDexCallGraph {
                class_jni: class_jni.clone(),
                method_decl: method_decl.clone(),
                label: label.clone(),
            },
            ContextMenuItem::CallersOfMethod {
                method_key: method_key.clone(),
                label: label.clone(),
            },
        ];
        if let Some(artifact) = dex_artifact {
            // Translate the row's line offset into an op index
            // through the parsed SmaliMethod. Line offset 0 is
            // the `.method` header — keep that as a Method key
            // (no op). Anything else maps to an op via the
            // shared `line_offset_to_op_index` helper.
            //
            // Falls back to `MethodLine` only if we couldn't
            // find the SmaliMethod (e.g. a class that lifted
            // raw but didn't parse). In practice that's rare
            // and the fallback at least preserves the original
            // semantics for the duration of this session.
            let (key, existing) = if line_offset == 0 {
                let k = glass_db::AnnotationKey::Method(
                    class_jni.clone(),
                    method_decl.clone(),
                );
                let e = self
                    .bundle()
                    .and_then(|b| b.annotations.get(&artifact))
                    .and_then(|idx| idx.at_method(&method_key))
                    .cloned()
                    .unwrap_or_default();
                (k, e)
            } else {
                let op_index = self
                    .bundle()
                    .and_then(|b| {
                        b.smali_classes.iter().find_map(|((_aid, jni), c)| {
                            if jni == &class_jni {
                                c.methods.iter().find(|m| {
                                    format!(
                                        "{}{}",
                                        m.name,
                                        m.signature.to_jni()
                                    ) == method_decl
                                })
                            } else {
                                None
                            }
                        })
                    })
                    .and_then(|m| {
                        crate::annotations::line_offset_to_op_index(m, line_offset)
                    });
                match op_index {
                    Some(op_index) => {
                        let k = glass_db::AnnotationKey::OpIndex {
                            class_jni: class_jni.clone(),
                            method_decl: method_decl.clone(),
                            op_index,
                        };
                        let e = self
                            .bundle()
                            .and_then(|b| b.annotations.get(&artifact))
                            .and_then(|idx| {
                                idx.at_op_index(&method_key, op_index)
                            })
                            .cloned()
                            .unwrap_or_default();
                        (k, e)
                    }
                    None => {
                        let k = glass_db::AnnotationKey::MethodLine(
                            class_jni.clone(),
                            method_decl.clone(),
                            line_offset,
                        );
                        let e = self
                            .bundle()
                            .and_then(|b| b.annotations.get(&artifact))
                            .and_then(|idx| {
                                idx.at_method_line(&method_key, line_offset)
                            })
                            .cloned()
                            .unwrap_or_default();
                        (k, e)
                    }
                }
            };
            let comment_label = if existing.comment.is_some() {
                "Edit comment…"
            } else {
                "Add comment…"
            };
            // Disambiguate the menu label so a user with several
            // annotations in the same method can see which line
            // they're editing.
            let line_chip = if line_offset == 0 {
                String::new()
            } else {
                format!(" (line {line_offset})")
            };
            items.push(ContextMenuItem::EditComment {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.comment.clone().unwrap_or_default(),
                label: SharedString::from(format!("{comment_label}{line_chip}")),
            });
            items.push(ContextMenuItem::PickColour {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.colour,
                label: SharedString::from(format!("Set colour…{line_chip}")),
            });
            if !existing.is_empty() {
                let clear_label = if line_offset == 0 {
                    format!("Clear annotation ({display})")
                } else {
                    format!("Clear annotation ({display} line {line_offset})")
                };
                items.push(ContextMenuItem::ClearAnnotation {
                    artifact,
                    key,
                    label: SharedString::from(clear_label),
                });
            }
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a `.class` header in the smali viewer. Same
    /// annotation surface as `open_smali_context_menu`, keyed on
    /// the class JNI rather than a method.
    pub(crate) fn open_smali_class_context_menu(
        &mut self,
        class_jni: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let dex_artifact = self
            .bundle()
            .and_then(|b| b.artifact_ids.first().cloned());
        let Some(artifact) = dex_artifact else {
            return;
        };
        // Display name: dotted Java form for menu chip
        // ("com.example.Foo") rather than the JNI form
        // ("Lcom/example/Foo;").
        let display = class_jni
            .trim_start_matches('L')
            .trim_end_matches(';')
            .replace('/', ".");
        let label = SharedString::from(display);
        let key = glass_db::AnnotationKey::Class(class_jni.clone());
        let existing = self
            .bundle()
            .and_then(|b| b.annotations.get(&artifact))
            .and_then(|idx| idx.at_class(&class_jni))
            .cloned()
            .unwrap_or_default();
        let comment_label = if existing.comment.is_some() {
            "Edit comment…"
        } else {
            "Add comment…"
        };
        let mut items = vec![
            ContextMenuItem::CopyText {
                text: class_jni.clone(),
                label: label.clone(),
            },
            ContextMenuItem::EditComment {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.comment.clone().unwrap_or_default(),
                label: SharedString::from(comment_label),
            },
            ContextMenuItem::PickColour {
                artifact: artifact.clone(),
                key: key.clone(),
                current: existing.colour,
                label: SharedString::from("Set colour…"),
            },
        ];
        if !existing.is_empty() {
            items.push(ContextMenuItem::ClearAnnotation {
                artifact: artifact.clone(),
                key,
                label: SharedString::from(format!("Clear annotation ({label})")),
            });
        }
        // If the active class has a staged structural edit, offer
        // a Revert. Walk smali_classes to find the matching artifact
        // — there's typically just one entry per jni, but APKs can
        // legally ship the same class in multiple DEX files.
        if let Some(bundle) = self.bundle() {
            let revert_targets: Vec<glass_db::ArtifactId> = bundle
                .smali_classes
                .iter()
                .filter_map(|((aid, jni), _)| {
                    if jni == &class_jni && bundle.smali_edits.get(aid, jni).is_some() {
                        Some(aid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for aid in revert_targets {
                items.push(ContextMenuItem::RevertSmaliClassEdit {
                    artifact: aid,
                    class_jni: class_jni.clone(),
                    label: SharedString::from(format!("Revert class edit ({label})")),
                });
            }
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on an address link inside a Listing row. Offers
    /// Follow / Follow in new tab (matching left-click + shift-click
    /// behaviour), plus Show CFG when the target lands in a text
    /// section with a known covering function.
    pub(crate) fn open_link_context_menu(
        &mut self,
        artifact: glass_db::ArtifactId,
        section: String,
        addr: u64,
        is_data: bool,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        use crate::context_menu::FollowTarget;
        let label = SharedString::from(display);
        let target = if is_data {
            FollowTarget::Hex {
                artifact: artifact.clone(),
                section: section.clone(),
                addr,
            }
        } else {
            FollowTarget::Listing {
                artifact: artifact.clone(),
                section: section.clone(),
                addr,
            }
        };
        let mut items = vec![
            ContextMenuItem::CopyText {
                text: label.to_string(),
                label: label.clone(),
            },
            ContextMenuItem::Follow { target: target.clone(), label: label.clone() },
            ContextMenuItem::FollowInNewTab { target, label: label.clone() },
        ];
        // Add Show CFG + Callers of function when the address has a
        // covering function in a text section; otherwise add a
        // generic References to address item.
        if !is_data {
            if let Some(bundle) = self.bundle() {
                if let Some(sym) = bundle
                    .symbol_maps
                    .get(&artifact)
                    .and_then(|sm| sm.covering(addr))
                {
                    items.push(ContextMenuItem::ShowCfg {
                        artifact: artifact.clone(),
                        entry_addr: sym.address,
                        label: SharedString::from(sym.display_name.clone()),
                    });
                    items.push(ContextMenuItem::CallersOfFunction {
                        artifact: artifact.clone(),
                        entry_addr: sym.address,
                        label: SharedString::from(sym.display_name.clone()),
                    });
                    // If the covering symbol is an ObjC method IMP
                    // (synthesised by the symbol_map pass-6), offer
                    // a jump to the class viewer for that class.
                    // The persistence-stable class_name key on the
                    // leaf uses the raw (mangled) form, so look up
                    // the leaf by `(artifact, raw_class_from_name)`
                    // — derived from the symbol's `name` field.
                    if let Some(raw_class) = parse_objc_class_from_symbol(&sym.name) {
                        let pretty =
                            glass_arch_arm::objc_format::pretty_class_name(raw_class);
                        items.push(ContextMenuItem::OpenObjCClass {
                            artifact: artifact.clone(),
                            class_name: raw_class.to_string(),
                            label: SharedString::from(pretty),
                        });
                    }
                    // Swift equivalent: if the covering symbol was
                    // synthesised by the Swift pass — either a
                    // metadata accessor or a vtable slot — extract
                    // the mangled type name and offer a jump to
                    // the type viewer.
                    if let Some(raw_type) = parse_swift_type_from_symbol(&sym.name) {
                        let pretty =
                            glass_arch_arm::swift_format::pretty_swift_type_name(raw_type);
                        items.push(ContextMenuItem::OpenSwiftType {
                            artifact: artifact.clone(),
                            mangled_name: raw_type.to_string(),
                            label: SharedString::from(pretty),
                        });
                    }
                } else {
                    items.push(ContextMenuItem::XrefsToAddress {
                        artifact: artifact.clone(),
                        addr,
                        label: label.clone(),
                    });
                }
            }
        } else {
            // Hex target — references to that byte (often a string
            // literal or data pointer).
            items.push(ContextMenuItem::XrefsToAddress {
                artifact: artifact.clone(),
                addr,
                label: label.clone(),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a DEX call-graph node. Shows Follow / Follow
    /// in new tab; both navigate to the method's smali. (Smali tabs
    /// dedupe by class so "new tab" reuses an existing class tab —
    /// see the comment in `activate_follow`.)
    /// Right-click on a `.field` line in a smali listing.
    /// Always shows "References to field"; when the active class
    /// has a staged edit that touches this specific field, adds
    /// "Revert field edit" too.
    pub(crate) fn open_field_context_menu(
        &mut self,
        field_ref: String,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let label = SharedString::from(display.clone());
        let mut items = vec![
            ContextMenuItem::CopyText {
                text: field_ref.clone(),
                label: label.clone(),
            },
            ContextMenuItem::RefsToField { field_ref: field_ref.clone(), label },
        ];
        // Field is edited if it appears in `edited_fields` for
        // the artifact that owns the active class. We need the
        // artifact id, the field's (name, sig), and a way to
        // know that the class is staged at all.
        if let Some((artifact, class_jni, name, sig)) =
            self.resolve_edited_field(&field_ref)
        {
            items.push(ContextMenuItem::RevertSmaliFieldEdit {
                artifact,
                class_jni,
                field_name: name,
                field_signature_jni: sig,
                label: SharedString::from(format!("Revert field edit ({display})")),
            });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Right-click on a `.method` header in a smali listing.
    /// Shows the existing method options (callers + call-graph)
    /// plus, when the active class has a staged edit that
    /// touches this method, "Revert method edit".
    pub(crate) fn open_method_header_context_menu(
        &mut self,
        method_name: String,
        method_signature_jni: String,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let label = SharedString::from(display.clone());
        let Some(active) = self.active_tab else { return };
        let class_jni = match self.tabs.get(active).map(|t| &t.kind) {
            Some(TabKind::SmaliClass { class_jni }) => class_jni.clone(),
            _ => return,
        };
        // Pre-fetch the artifact so we can decide whether to
        // offer Revert. The other menu items don't need it.
        let artifact = self.bundle().and_then(|b| {
            b.smali_classes.keys().find_map(|(aid, jni)| {
                if jni == &class_jni { Some(aid.clone()) } else { None }
            })
        });
        let mut items: Vec<ContextMenuItem> = Vec::new();
        // Reuse the existing dex-callgraph / callers-of-method
        // entry points so the "Show call graph" menu item stays
        // available.
        let method_decl =
            format!("{method_name}{method_signature_jni}");
        items.push(ContextMenuItem::CopyText {
            text: format!("{class_jni}->{method_decl}"),
            label: label.clone(),
        });
        items.push(ContextMenuItem::ShowDexCallGraph {
            class_jni: class_jni.clone(),
            method_decl: method_decl.clone(),
            label: label.clone(),
        });
        items.push(ContextMenuItem::CallersOfMethod {
            method_key: format!("{class_jni}->{method_decl}"),
            label: label.clone(),
        });
        if let Some(artifact) = artifact {
            if self
                .bundle()
                .and_then(|b| {
                    b.smali_classes
                        .get(&(artifact.clone(), class_jni.clone()))
                        .map(|c| {
                            b.smali_edits
                                .edited_methods(&artifact, &class_jni, c)
                                .into_iter()
                                .any(|(n, s)| {
                                    n == method_name && s == method_signature_jni
                                })
                        })
                })
                .unwrap_or(false)
            {
                items.push(ContextMenuItem::RevertSmaliMethodEdit {
                    artifact: artifact.clone(),
                    class_jni: class_jni.clone(),
                    method_name: method_name.clone(),
                    method_signature_jni: method_signature_jni.clone(),
                    label: SharedString::from(format!(
                        "Revert method edit ({display})"
                    )),
                });
            }
            // Trace items — only show when the debug dock is
            // attached. Toggle between Start / Stop based on
            // current registry state. <clinit> is excluded
            // because Frida's Java.use can't hook static
            // initialisers.
            let dock_attached = self
                .debug_dock
                .as_ref()
                .map(|d| d.session.is_some())
                .unwrap_or(false);
            if dock_attached && method_name != "<clinit>" {
                let is_traced = self
                    .bundle()
                    .map(|b| {
                        b.traces.is_traced(
                            &artifact,
                            &class_jni,
                            &method_name,
                            &method_signature_jni,
                        )
                    })
                    .unwrap_or(false);
                if is_traced {
                    items.push(ContextMenuItem::StopTrace {
                        artifact: artifact.clone(),
                        class_jni: class_jni.clone(),
                        method_name: method_name.clone(),
                        method_signature_jni: method_signature_jni.clone(),
                        label: SharedString::from(display.clone()),
                    });
                } else {
                    items.push(ContextMenuItem::StartTrace {
                        artifact: artifact.clone(),
                        class_jni: class_jni.clone(),
                        method_name: method_name.clone(),
                        method_signature_jni: method_signature_jni.clone(),
                        label: SharedString::from(display.clone()),
                    });
                }
                // Hook items — same gating as traces.
                let is_hooked = self
                    .bundle()
                    .map(|b| {
                        b.hooks.is_hooked(
                            &artifact,
                            &class_jni,
                            &method_name,
                            &method_signature_jni,
                        )
                    })
                    .unwrap_or(false);
                if is_hooked {
                    items.push(ContextMenuItem::StopHook {
                        artifact,
                        class_jni,
                        method_name,
                        method_signature_jni,
                        label: SharedString::from(display),
                    });
                } else {
                    items.push(ContextMenuItem::StartHook {
                        artifact,
                        class_jni,
                        method_name,
                        method_signature_jni,
                        label: SharedString::from(display),
                    });
                }
            }
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    /// Given a `field_ref` like `Lcom/Foo;->count:I`, find the
    /// owning artifact and return `(artifact, class_jni, name, sig)`
    /// when that field is currently edited. Returns `None` if
    /// the class isn't loaded, the ref doesn't parse, or the
    /// field isn't in the edited set.
    fn resolve_edited_field(
        &self,
        field_ref: &str,
    ) -> Option<(glass_db::ArtifactId, String, String, String)> {
        let (class_jni, rest) = field_ref.split_once("->")?;
        let (name, sig) = rest.split_once(':')?;
        let bundle = self.bundle()?;
        let (artifact, original) =
            bundle.smali_classes.iter().find_map(|((aid, jni), c)| {
                if jni == class_jni { Some((aid.clone(), c.clone())) } else { None }
            })?;
        let edited = bundle
            .smali_edits
            .edited_fields(&artifact, class_jni, &original);
        if edited
            .into_iter()
            .any(|(n, s)| n == name && s == sig)
        {
            Some((artifact, class_jni.to_string(), name.to_string(), sig.to_string()))
        } else {
            None
        }
    }

    pub(crate) fn open_smali_link_context_menu(
        &mut self,
        leaf: LeafId,
        line: usize,
        method_key: Option<String>,
        display: String,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        use crate::context_menu::FollowTarget;
        let label = SharedString::from(display);
        let target = FollowTarget::SmaliMethod { leaf, line };
        let mut items = vec![
            ContextMenuItem::CopyText {
                text: label.to_string(),
                label: label.clone(),
            },
            ContextMenuItem::Follow { target: target.clone(), label: label.clone() },
            ContextMenuItem::FollowInNewTab { target, label: label.clone() },
        ];
        if let Some(key) = method_key {
            items.push(ContextMenuItem::CallersOfMethod { method_key: key, label });
        }
        self.context_menu = Some(ContextMenuState { position, items });
        cx.notify();
    }

    pub(crate) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.is_some() {
            self.context_menu = None;
            cx.notify();
        }
    }

    pub(crate) fn activate_context_menu_item(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.context_menu.as_ref() else { return };
        let Some(item) = menu.items.get(index).cloned() else { return };
        self.context_menu = None;
        match item {
            ContextMenuItem::CopyText { text, .. } => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
            ContextMenuItem::Follow { target, .. } => {
                self.activate_follow(target, false, cx);
            }
            ContextMenuItem::FollowInNewTab { target, .. } => {
                self.activate_follow(target, true, cx);
            }
            ContextMenuItem::ShowCfg {
                artifact,
                entry_addr,
                label,
            } => {
                self.show_cfg(artifact, entry_addr, label, cx);
            }
            ContextMenuItem::ShowDexCallGraph {
                class_jni,
                method_decl,
                label,
            } => {
                self.show_dex_callgraph(class_jni, method_decl, label, cx);
            }
            ContextMenuItem::XrefsToAddress { artifact, addr, label } => {
                self.open_xrefs_to_address(artifact, addr, label, cx);
            }
            ContextMenuItem::CallersOfFunction { artifact, entry_addr, label } => {
                self.open_xrefs_to_address(artifact, entry_addr, label, cx);
            }
            ContextMenuItem::CallersOfMethod { method_key, label } => {
                self.open_callers_of_method(method_key, label, cx);
            }
            ContextMenuItem::RefsToField { field_ref, label } => {
                self.open_refs_to_field(field_ref, label, cx);
            }
            ContextMenuItem::EditRename { artifact, key, current, .. } => {
                self.begin_annotation_edit(
                    artifact,
                    key,
                    crate::AnnotationFacet::Rename,
                    current,
                    cx,
                );
            }
            ContextMenuItem::EditComment { artifact, key, current, .. } => {
                self.begin_annotation_edit(
                    artifact,
                    key,
                    crate::AnnotationFacet::Comment,
                    current,
                    cx,
                );
            }
            ContextMenuItem::PickColour { artifact, key, current, .. } => {
                self.open_colour_picker(artifact, key, current, cx);
            }
            ContextMenuItem::ClearAnnotation { artifact, key, .. } => {
                self.clear_annotation_at(artifact, key, cx);
            }
            ContextMenuItem::RevertDisasmEdit { artifact, vaddr, .. } => {
                self.revert_disasm_edit(artifact, vaddr, cx);
            }
            ContextMenuItem::OpenHexHere { artifact, section, addr, .. } => {
                // Open in a fresh tab rather than reusing an
                // existing Hex tab on the same section — the
                // user explicitly asked to see this address,
                // and the existing tab might be scrolled
                // elsewhere. (open_hex_in_new_tab dedupes by
                // section so we'd lose the scroll target on
                // reuse anyway.)
                self.open_hex_in_new_tab(artifact, section, addr, cx);
            }
            ContextMenuItem::OpenSwiftType { artifact, mangled_name, .. } => {
                let leaf = self.bundle().and_then(|b| {
                    b.kinds.iter().enumerate().find_map(|(i, k)| match k {
                        crate::LeafKind::SwiftType { artifact: a, mangled_name: n }
                            if a == &artifact && n == &mangled_name =>
                        {
                            Some(crate::LeafId(i))
                        }
                        _ => None,
                    })
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
            ContextMenuItem::OpenObjCClass { artifact, class_name, .. } => {
                // Locate the existing ObjC class leaf and open it.
                // The loader populates these whenever a Mach-O
                // artifact has parseable __objc_classlist
                // metadata — if the leaf isn't there, the user
                // is on a binary that didn't yield ObjC metadata
                // and we silently no-op.
                let leaf = self.bundle().and_then(|b| {
                    b.kinds.iter().enumerate().find_map(|(i, k)| match k {
                        crate::LeafKind::ObjCClass { artifact: a, class_name: c }
                            if a == &artifact && c == &class_name =>
                        {
                            Some(crate::LeafId(i))
                        }
                        _ => None,
                    })
                });
                if let Some(leaf) = leaf {
                    self.open_leaf(leaf, cx);
                }
            }
            ContextMenuItem::RevertSmaliClassEdit { artifact, class_jni, .. } => {
                self.revert_smali_class_edit(artifact, class_jni, cx);
            }
            ContextMenuItem::RevertSmaliFieldEdit {
                artifact,
                class_jni,
                field_name,
                field_signature_jni,
                ..
            } => {
                self.revert_smali_field_edit(
                    artifact,
                    class_jni,
                    field_name,
                    field_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::RevertSmaliMethodEdit {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.revert_smali_method_edit(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::StartTrace {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.start_trace(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::StopTrace {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.stop_trace(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::StartHook {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                // Default action: LogOnly. User flips via the
                // Hooks dialog's Cycle button.
                self.start_hook(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    crate::hooks::HookAction::LogOnly,
                    cx,
                );
            }
            ContextMenuItem::StopHook {
                artifact,
                class_jni,
                method_name,
                method_signature_jni,
                ..
            } => {
                self.stop_hook(
                    artifact,
                    class_jni,
                    method_name,
                    method_signature_jni,
                    cx,
                );
            }
            ContextMenuItem::ToggleScriptEnabled {
                name,
                currently_enabled,
                ..
            } => {
                self.set_script_enabled_for_bundle(
                    &name,
                    !currently_enabled,
                    cx,
                );
            }
            ContextMenuItem::DeleteScript { name, .. } => {
                self.delete_script_and_close_tab(&name, cx);
            }
        }
    }
}

/// Extract the class name from an ObjC method symbol name
/// (`-[Class selector:]` / `+[Class selector:]`). For category
/// methods (`-[Base(Cat) sel]`) the returned slice includes the
/// parens — that's the form the loader uses as the persistence
/// key for the category leaf, so the lookup matches.
///
/// Returns `None` for names that don't look like ObjC IMP
/// symbols. The class name is returned as a slice of the input
/// (raw / possibly mangled), since the caller decides whether
/// to demangle for display.
fn parse_objc_class_from_symbol(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("-[").or_else(|| name.strip_prefix("+["))?;
    // Class part runs to the space before the selector. (Selectors
    // can't contain spaces; categories' parens come before the
    // space, so they're included in the class part.)
    let space = rest.find(' ')?;
    Some(&rest[..space])
}

/// Extract the raw Swift mangled-name from a symbol synthesised by
/// the Swift pass in `glass_arch_arm::symbol_map`. Recognises both
/// shapes the synthesis emits:
///
///   * `type metadata accessor for <raw>` — produced for each
///     type's metadata-accessor function.
///   * `<raw>.vtable[<n>]` — produced for each vtable slot.
///
/// Returns the raw mangled-name slice (the same string used as the
/// persistence key on `LeafKind::SwiftType`), or `None` for symbols
/// that don't match either shape.
fn parse_swift_type_from_symbol(name: &str) -> Option<&str> {
    const META_PREFIX: &str = "type metadata accessor for ";
    if let Some(rest) = name.strip_prefix(META_PREFIX) {
        return Some(rest);
    }
    // `<raw>.vtable[<n>]` — `<raw>` may itself contain dots
    // (`module.Type`), so split on the last `.vtable[` literal.
    if let Some(idx) = name.rfind(".vtable[") {
        if name.ends_with(']') {
            return Some(&name[..idx]);
        }
    }
    None
}

#[cfg(test)]
mod swift_symbol_parse_tests {
    use super::parse_swift_type_from_symbol;

    #[test]
    fn parse_metadata_accessor() {
        assert_eq!(
            parse_swift_type_from_symbol("type metadata accessor for blackjack.ContentView"),
            Some("blackjack.ContentView"),
        );
    }

    #[test]
    fn parse_vtable_slot() {
        assert_eq!(
            parse_swift_type_from_symbol("blackjack.ContentView.vtable[3]"),
            Some("blackjack.ContentView"),
        );
    }

    #[test]
    fn non_swift_symbol_returns_none() {
        assert_eq!(parse_swift_type_from_symbol("-[NSString length]"), None);
        assert_eq!(parse_swift_type_from_symbol("sub_1000"), None);
    }
}
