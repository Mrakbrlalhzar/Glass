//! Bundle loading: APK / IPA / standalone-binary snapshots.
//!
//! All snapshot functions return a `LoadedBundle` — a fully-prepared,
//! UI-friendly view of the input. Progress is reported through the
//! shared `Progress` struct in `lib.rs` so the loading-screen UI can
//! poll it at frame rate.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use glass_arch_arm64::Arm64Binary;
use glass_mobile::{ApkBundle, Bundle, IpaBundle};
use gpui::SharedString;

use crate::{
    flatten_info_plist, flatten_manifest, DataSectionBytes, LeafId, LeafKind, LoadedBundle,
    ManifestRow, NativeSectionKind, Node, Progress, SectionInfo, TextSectionBytes, Tree,
};

pub(crate) fn load_bundle_blocking(path: PathBuf, progress: Arc<Mutex<Progress>>) -> Result<LoadedBundle> {
    let result = load_inner(&path, &progress);
    // Make sure the foreground poll loop notices we're done even on error.
    if let Ok(mut p) = progress.lock() {
        p.done = true;
    }
    result
}

fn load_inner(path: &std::path::Path, progress: &Arc<Mutex<Progress>>) -> Result<LoadedBundle> {
    if let Ok(mut p) = progress.lock() {
        p.phase = SharedString::from("Reading archive…");
        p.current = 0;
        p.total = 0;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "apk" | "aab") {
        // Open the APK first so we have access to its DEX and native-
        // lib bytes. We used to also `fs::read` the whole APK file to
        // hash it — but that's a 350 MB+ read on big games. The
        // BundleId is derived from the concatenated ArtifactIds below
        // instead: same content-addressed guarantee, no extra I/O.
        if let Ok(mut p) = progress.lock() {
            p.phase = SharedString::from("Reading archive…");
        }
        let apk = match glass_mobile::Bundle::open(path)? {
            Bundle::Apk(a) => a,
            _ => anyhow::bail!("expected APK"),
        };
        let display_label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle")
            .to_string();
        snapshot_apk_with_progress(apk, progress.clone(), display_label)
    } else if matches!(ext, "ipa") {
        if let Ok(mut p) = progress.lock() {
            p.phase = SharedString::from("Reading archive…");
        }
        let ipa = match glass_mobile::Bundle::open(path)? {
            Bundle::Ipa(i) => i,
            _ => anyhow::bail!("expected IPA"),
        };
        let display_label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle")
            .to_string();
        snapshot_ipa_with_progress(ipa, progress.clone(), display_label)
    } else {
        // Standalone binary: ELF (`.so`, no-ext executables) or Mach-O
        // (`.dylib`, no-ext executables — possibly fat). Arm64Binary
        // transparently slices fat Mach-Os down to arm64/arm64e.
        let bin = Arm64Binary::open(path)?;
        snapshot_arm64(bin)
    }
}

fn snapshot_apk_with_progress(
    apk: ApkBundle,
    progress: Arc<Mutex<Progress>>,
    display_label: String,
) -> Result<LoadedBundle> {
    // Hash each artifact as we touch its bytes.
    let mut artifact_ids: Vec<glass_db::ArtifactId> = Vec::new();
    let mut native_sections: std::collections::HashMap<
        glass_db::ArtifactId,
        Vec<SectionInfo>,
    > = std::collections::HashMap::new();
    let mut symbol_maps: std::collections::HashMap<
        glass_db::ArtifactId,
        glass_arch_arm64::SymbolMap,
    > = std::collections::HashMap::new();
    let mut text_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        TextSectionBytes,
    > = std::collections::HashMap::new();
    let mut data_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        DataSectionBytes,
    > = std::collections::HashMap::new();
    for dex in &apk.dex_files {
        artifact_ids.push(glass_db::ArtifactId::from_bytes(&dex.bytes));
    }
    // Hash each native lib once; reuse the resulting id in the tree
    // loop further down. Hashing a 23 MB lib twice costs real time on
    // big APKs (coc-jigsaw has libg.so at 23 MB).
    let lib_artifact_ids: Vec<glass_db::ArtifactId> = apk
        .native_libs
        .iter()
        .map(|lib| glass_db::ArtifactId::from_bytes(&lib.binary.bytes))
        .collect();
    for (lib, aid) in apk.native_libs.iter().zip(lib_artifact_ids.iter()) {
        let aid = aid.clone();
        native_sections.insert(aid.clone(), build_section_info(&lib.binary.container));
        symbol_maps.insert(
            aid.clone(),
            glass_arch_arm64::SymbolMap::build(&lib.binary.container),
        );
        // armv8-encode parses the container regardless of architecture
        // but its decoder is AArch64-only. For non-AArch64 (x86_64,
        // armeabi-v7a, etc.) the listing would render meaningless
        // AArch64 reads of the bytes, so we register every section —
        // including text — as data so the UI routes clicks to the hex
        // view instead.
        let arch = lib.binary.container.architecture;
        let aarch64 =
            matches!(arch, armv8_encode::container::Architecture::Aarch64);
        for sec in &lib.binary.container.sections {
            let kind = NativeSectionKind::from_armv8(sec.kind);
            let is_text =
                matches!(sec.kind, armv8_encode::container::SectionKind::Text);
            if aarch64 && is_text {
                text_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    TextSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                    },
                );
            } else if !sec.bytes.is_empty() {
                data_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    DataSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                        kind,
                    },
                );
            }
        }
        artifact_ids.push(aid);
    }

    let mut bodies: Vec<SharedString> = Vec::new();
    let mut origins: Vec<SharedString> = Vec::new();
    let mut labels: Vec<SharedString> = Vec::new();
    let mut kinds: Vec<LeafKind> = Vec::new();
    let mut roots: Vec<Node> = Vec::new();

    // Manifest leaf at the very top — first thing a reverser usually
    // looks at. Only emit when we actually parsed a manifest.
    let manifest_rows: Vec<ManifestRow> = match apk.manifest.as_ref() {
        Some(m) => {
            let leaf_id = LeafId(bodies.len());
            bodies.push(SharedString::from(""));
            origins.push(SharedString::from("manifest"));
            labels.push(SharedString::from("AndroidManifest.xml"));
            kinds.push(LeafKind::Manifest);
            roots.push(Node::Leaf {
                label: SharedString::from("AndroidManifest.xml"),
                leaf_id,
            });
            flatten_manifest(m)
        }
        None => Vec::new(),
    };

    // Count total classes up-front for a determinate bar.
    let mut total_classes = 0usize;
    for dex in &apk.dex_files {
        total_classes += dex.classes()?.len();
    }
    if let Ok(mut p) = progress.lock() {
        p.phase = SharedString::from("Lifting smali…");
        p.current = 0;
        p.total = total_classes;
    }

    let mut processed = 0usize;
    for dex in &apk.dex_files {
        let classes = dex.classes()?;
        let dex_origin = SharedString::from(dex.name.clone());
        let mut pkg_root = PkgBuilder::default();
        for class in classes {
            let id = LeafId(bodies.len());
            bodies.push(SharedString::from(class.to_smali()));
            origins.push(dex_origin.clone());
            let jni = class.name.to_string();
            let parts = split_jni_class_name(&jni);
            labels.push(SharedString::from(
                parts.last().cloned().unwrap_or_else(|| jni.clone()),
            ));
            kinds.push(LeafKind::SmaliClass { class_jni: jni.clone() });
            pkg_root.insert(&parts, id);
            processed += 1;
            // Updating shared state every class would thrash the lock. The
            // UI polls at ~30fps so a coarser cadence here is plenty.
            if processed % 64 == 0 {
                if let Ok(mut p) = progress.lock() {
                    p.current = processed;
                }
            }
        }
        roots.push(Node::Group {
            label: dex_origin,
            children: pkg_root.finish(),
        });
    }
    if let Ok(mut p) = progress.lock() {
        p.current = processed;
    }

    if !apk.native_libs.is_empty() {
        if let Ok(mut p) = progress.lock() {
            p.phase = SharedString::from("Disassembling native…");
            p.current = 0;
            p.total = apk.native_libs.len();
        }
        use std::collections::BTreeMap;
        let mut by_abi: BTreeMap<String, Vec<Node>> = BTreeMap::new();
        for (i, lib) in apk.native_libs.iter().enumerate() {
            let lib_aid = lib_artifact_ids[i].clone();
            let arch = lib.binary.container.architecture;
            let aarch64 =
                matches!(arch, armv8_encode::container::Architecture::Aarch64);

            // Overview leaf (SectionMap), then one leaf per actual text
            // section. ELF uses `.text`, Mach-O uses `__text`, and some
            // ELF variants split text across `.text.startup` etc. —
            // we surface them all as siblings under the lib.
            //
            // For non-AArch64 libs (armeabi-v7a, x86_64, …) we can't
            // disassemble — armv8-encode is AArch64-only. Emit Hex
            // leaves so clicking takes the user to the raw byte view
            // instead of a fake disassembly.
            let overview_id = LeafId(bodies.len());
            bodies.push(SharedString::from("")); // SectionMap renders its own body
            origins.push(SharedString::from(format!("lib/{}", lib.abi)));
            labels.push(SharedString::from(format!("{} (overview)", lib.name)));
            kinds.push(LeafKind::SectionMap { artifact: lib_aid.clone() });

            let mut children: Vec<Node> = vec![Node::Leaf {
                label: SharedString::from("Overview"),
                leaf_id: overview_id,
            }];
            for sec in &lib.binary.container.sections {
                if !matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
                    continue;
                }
                let leaf_id = LeafId(bodies.len());
                bodies.push(SharedString::from(""));
                origins.push(SharedString::from(format!("lib/{}", lib.abi)));
                labels.push(SharedString::from(sec.name.clone()));
                if aarch64 {
                    kinds.push(LeafKind::Listing {
                        artifact: lib_aid.clone(),
                        section: sec.name.clone(),
                    });
                } else {
                    kinds.push(LeafKind::Hex {
                        artifact: lib_aid.clone(),
                        section: sec.name.clone(),
                    });
                }
                children.push(Node::Leaf {
                    label: SharedString::from(sec.name.clone()),
                    leaf_id,
                });
            }

            // Tag the lib group label with arch when we can't
            // disassemble — makes "why is this in hex?" self-evident.
            let lib_label = if aarch64 {
                lib.name.clone()
            } else {
                format!("{} ({})", lib.name, arch_label(arch))
            };
            by_abi
                .entry(lib.abi.clone())
                .or_default()
                .push(Node::Group {
                    label: SharedString::from(lib_label),
                    children,
                });
            if let Ok(mut p) = progress.lock() {
                p.current = i + 1;
            }
        }
        let mut lib_children = Vec::new();
        for (abi, libs) in by_abi {
            lib_children.push(Node::Group {
                label: SharedString::from(abi),
                children: libs,
            });
        }
        roots.push(Node::Group {
            label: SharedString::from("lib"),
            children: lib_children,
        });
    }

    let method_lines = build_method_line_index(&kinds, &bodies);
    let method_calls = build_method_call_index(&kinds, &bodies);

    // Derive BundleId from the artifact IDs themselves. Same content-
    // addressed guarantee (any DEX/lib changes ⇒ new bundle id) at a
    // tiny fraction of the cost of hashing the whole APK file.
    let mut bundle_hasher = blake3::Hasher::new();
    for aid in &artifact_ids {
        bundle_hasher.update(aid.as_bytes());
    }
    let bundle_id = glass_db::BundleId::from_raw(*bundle_hasher.finalize().as_bytes());

    Ok(LoadedBundle {
        title: format!("Glass — {}", apk.path.display()),
        tree: Arc::new(Tree { roots }),
        bodies: Arc::new(bodies),
        origins: Arc::new(origins),
        labels: Arc::new(labels),
        kinds: Arc::new(kinds),
        bundle_id: Some(bundle_id),
        artifact_ids: Arc::new(artifact_ids),
        display_label,
        native_sections: Arc::new(native_sections),
        symbol_maps: Arc::new(symbol_maps),
        text_sections: Arc::new(text_sections),
        data_sections: Arc::new(data_sections),
        method_lines: Arc::new(method_lines),
        method_calls: Arc::new(method_calls),
        manifest_rows: Arc::new(manifest_rows),
        xrefs: crate::xref::XrefStore::new(),
    })
}


/// IPA snapshot. Mirrors `snapshot_apk_with_progress` but for iOS:
/// Info.plist + main executable + frameworks/dylibs.
fn snapshot_ipa_with_progress(
    ipa: IpaBundle,
    progress: Arc<Mutex<Progress>>,
    display_label: String,
) -> Result<LoadedBundle> {
    let mut artifact_ids: Vec<glass_db::ArtifactId> = Vec::new();
    let mut native_sections: std::collections::HashMap<
        glass_db::ArtifactId,
        Vec<SectionInfo>,
    > = std::collections::HashMap::new();
    let mut symbol_maps: std::collections::HashMap<
        glass_db::ArtifactId,
        glass_arch_arm64::SymbolMap,
    > = std::collections::HashMap::new();
    let mut text_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        TextSectionBytes,
    > = std::collections::HashMap::new();
    let mut data_sections: std::collections::HashMap<
        (glass_db::ArtifactId, String),
        DataSectionBytes,
    > = std::collections::HashMap::new();

    let mut bodies: Vec<SharedString> = Vec::new();
    let mut origins: Vec<SharedString> = Vec::new();
    let mut labels: Vec<SharedString> = Vec::new();
    let mut kinds: Vec<LeafKind> = Vec::new();
    let mut roots: Vec<Node> = Vec::new();

    // Info.plist leaf at the top — first thing a reverser checks for
    // the bundle id, executable name, and entitlements clues.
    let info_rows = flatten_info_plist(&ipa.info);
    {
        let leaf_id = LeafId(bodies.len());
        bodies.push(SharedString::from(""));
        origins.push(SharedString::from("plist"));
        labels.push(SharedString::from("Info.plist"));
        kinds.push(LeafKind::Manifest);
        roots.push(Node::Leaf {
            label: SharedString::from("Info.plist"),
            leaf_id,
        });
    }

    // Helper to register one native artifact (main exec or framework
    // binary). Returns its ArtifactId and a Group node summarising it.
    let mut register_artifact = |bytes: &[u8],
                                 container: &armv8_encode::container::Container,
                                 display_name: String,
                                 origin: String|
     -> (glass_db::ArtifactId, Node) {
        let aid = glass_db::ArtifactId::from_bytes(bytes);
        native_sections.insert(aid.clone(), build_section_info(container));
        symbol_maps.insert(aid.clone(), glass_arch_arm64::SymbolMap::build(container));

        let arch = container.architecture;
        let aarch64 =
            matches!(arch, armv8_encode::container::Architecture::Aarch64);
        for sec in &container.sections {
            let kind = NativeSectionKind::from_armv8(sec.kind);
            let is_text =
                matches!(sec.kind, armv8_encode::container::SectionKind::Text);
            if aarch64 && is_text {
                text_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    TextSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                    },
                );
            } else if !sec.bytes.is_empty() {
                data_sections.insert(
                    (aid.clone(), sec.name.clone()),
                    DataSectionBytes {
                        base: sec.address,
                        bytes: Arc::new(sec.bytes.clone()),
                        kind,
                    },
                );
            }
        }

        let overview_id = LeafId(bodies.len());
        bodies.push(SharedString::from(""));
        origins.push(SharedString::from(origin.clone()));
        labels.push(SharedString::from(format!("{display_name} (overview)")));
        kinds.push(LeafKind::SectionMap { artifact: aid.clone() });

        let mut children: Vec<Node> = vec![Node::Leaf {
            label: SharedString::from("Overview"),
            leaf_id: overview_id,
        }];
        for sec in &container.sections {
            if !matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
                continue;
            }
            let leaf_id = LeafId(bodies.len());
            bodies.push(SharedString::from(""));
            origins.push(SharedString::from(origin.clone()));
            labels.push(SharedString::from(sec.name.clone()));
            if aarch64 {
                kinds.push(LeafKind::Listing {
                    artifact: aid.clone(),
                    section: sec.name.clone(),
                });
            } else {
                kinds.push(LeafKind::Hex {
                    artifact: aid.clone(),
                    section: sec.name.clone(),
                });
            }
            children.push(Node::Leaf {
                label: SharedString::from(sec.name.clone()),
                leaf_id,
            });
        }
        let group_label = if aarch64 {
            display_name
        } else {
            format!("{display_name} ({})", arch_label(arch))
        };
        let node = Node::Group {
            label: SharedString::from(group_label),
            children,
        };
        (aid, node)
    };

    if let Ok(mut p) = progress.lock() {
        p.phase = SharedString::from("Disassembling native…");
        p.current = 0;
        p.total = 1 + ipa.frameworks.len();
    }
    let mut progressed = 0usize;
    let bump = |progress: &Arc<Mutex<Progress>>, progressed: &mut usize| {
        *progressed += 1;
        if let Ok(mut p) = progress.lock() {
            p.current = *progressed;
        }
    };

    // Main executable.
    if let Some(bin) = &ipa.main_executable {
        let display_name = ipa
            .info
            .executable
            .clone()
            .unwrap_or_else(|| "main".to_string());
        let (aid, node) = register_artifact(
            &bin.bytes,
            &bin.container,
            display_name,
            "main".to_string(),
        );
        artifact_ids.push(aid);
        roots.push(node);
    }
    bump(&progress, &mut progressed);

    // Frameworks / dylibs. Each ships its own arm64-sliced bytes; parse
    // and register them just like the main exec.
    if !ipa.frameworks.is_empty() {
        let mut fw_children: Vec<Node> = Vec::new();
        for fw in &ipa.frameworks {
            let bin = match Arm64Binary::from_bytes(
                PathBuf::from(&fw.archive_path),
                fw.bytes.clone(),
            ) {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!("skipping {}: {e}", fw.name);
                    bump(&progress, &mut progressed);
                    continue;
                }
            };
            let (aid, node) = register_artifact(
                &bin.bytes,
                &bin.container,
                fw.name.clone(),
                "Frameworks".to_string(),
            );
            artifact_ids.push(aid);
            fw_children.push(node);
            bump(&progress, &mut progressed);
        }
        if !fw_children.is_empty() {
            roots.push(Node::Group {
                label: SharedString::from("Frameworks"),
                children: fw_children,
            });
        }
    }

    let method_lines = build_method_line_index(&kinds, &bodies);
    let method_calls = build_method_call_index(&kinds, &bodies);

    let mut bundle_hasher = blake3::Hasher::new();
    for aid in &artifact_ids {
        bundle_hasher.update(aid.as_bytes());
    }
    let bundle_id = glass_db::BundleId::from_raw(*bundle_hasher.finalize().as_bytes());

    Ok(LoadedBundle {
        title: format!("Glass — {}", ipa.path.display()),
        tree: Arc::new(Tree { roots }),
        bodies: Arc::new(bodies),
        origins: Arc::new(origins),
        labels: Arc::new(labels),
        kinds: Arc::new(kinds),
        bundle_id: Some(bundle_id),
        artifact_ids: Arc::new(artifact_ids),
        display_label,
        native_sections: Arc::new(native_sections),
        symbol_maps: Arc::new(symbol_maps),
        text_sections: Arc::new(text_sections),
        data_sections: Arc::new(data_sections),
        method_lines: Arc::new(method_lines),
        method_calls: Arc::new(method_calls),
        manifest_rows: Arc::new(info_rows),
        xrefs: crate::xref::XrefStore::new(),
    })
}

/// Walk every SmaliClass leaf in the bundle, scan its body, record the
/// line index of each `.method` declaration, and key it by the same
/// `Class;->name(sig)ret` form a smali method-ref takes. Single linear
/// pass per class, on the load thread.
///
/// Smali method-decl lines look like:
///   `.method public static foo(Lcom/Foo;I)V`
///
/// We pluck the trailing token (which is `name(sig)ret`) and pair it
/// with the class JNI to form the key.
fn build_method_line_index(
    kinds: &[LeafKind],
    bodies: &[SharedString],
) -> std::collections::HashMap<String, (LeafId, usize)> {
    let mut map = std::collections::HashMap::new();
    for (i, k) in kinds.iter().enumerate() {
        let LeafKind::SmaliClass { class_jni } = k else { continue };
        let Some(body) = bodies.get(i) else { continue };
        for (line_no, raw) in body.lines().enumerate() {
            let trimmed = raw.trim_start();
            let Some(after) = trimmed.strip_prefix(".method ") else { continue };
            // Last whitespace-separated token = name(sig)ret.
            let Some(method_decl) = after.split_whitespace().last() else { continue };
            let key = format!("{class_jni}->{method_decl}");
            map.entry(key).or_insert((LeafId(i), line_no));
        }
    }
    map
}

/// Build a per-method call index: for each `Class;->name(sig)ret`
/// key, the set of methods it calls (also as `Class;->name(sig)ret`
/// keys). Used by the DEX call-graph view.
///
/// Calls are extracted by scanning each SmaliClass body for
/// `invoke-*` directives. The target reference after the last comma
/// is the callee key; we deduplicate so a method that calls the
/// same callee 50 times only contributes one graph edge. Order of
/// first-occurrence within the method is preserved so the graph
/// reads top-to-bottom in source order.
fn build_method_call_index(
    kinds: &[LeafKind],
    bodies: &[SharedString],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (i, k) in kinds.iter().enumerate() {
        let LeafKind::SmaliClass { class_jni } = k else { continue };
        let Some(body) = bodies.get(i) else { continue };
        let mut current_method: Option<String> = None;
        let mut seen_in_method: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for raw in body.lines() {
            let trimmed = raw.trim_start();
            if let Some(after) = trimmed.strip_prefix(".method ") {
                let Some(method_decl) = after.split_whitespace().last() else { continue };
                let key = format!("{class_jni}->{method_decl}");
                current_method = Some(key);
                seen_in_method.clear();
                continue;
            }
            if trimmed.starts_with(".end method") {
                current_method = None;
                continue;
            }
            if !trimmed.starts_with("invoke-") {
                continue;
            }
            let Some(method_key) = current_method.as_ref() else { continue };
            // The callee ref is the last whitespace-separated
            // token on the line. Smali shapes:
            //   "invoke-virtual {v0, v1}, Lcom/Foo;->bar(I)V"
            //   "invoke-static {}, Lcom/Foo;->bar()V"
            //   "invoke-virtual/range {v0 .. v3}, Lcom/Foo;->bar(III)V"
            let Some(callee_ref) = trimmed.split_whitespace().last() else { continue };
            // Filter out anything that doesn't look like a JNI
            // method ref (must contain `->` and end with a return
            // type). Reflective / array-ish invokes occasionally
            // produce odd shapes; skip them.
            if !callee_ref.contains("->") {
                continue;
            }
            if seen_in_method.insert(callee_ref.to_string()) {
                map.entry(method_key.clone())
                    .or_default()
                    .push(callee_ref.to_string());
            }
        }
    }
    map
}

/// Short tag for non-AArch64 architectures, shown in the tree label.
fn arch_label(arch: armv8_encode::container::Architecture) -> &'static str {
    use armv8_encode::container::Architecture as A;
    match arch {
        A::Aarch64 => "arm64",
        A::Arm => "arm32",
        A::Other => "other",
    }
}

/// Snapshot section metadata for a native artifact into a UI-friendly form.
fn build_section_info(container: &armv8_encode::container::Container) -> Vec<SectionInfo> {
    // Total on-disk + bss size across all sections — we draw the bar
    // proportional to size rather than file offset because Mach-O segments
    // and ELF sections have very different on-disk vs. virtual extents.
    // Using `size` keeps the strip readable on a typical ARM64 .so where
    // .text dominates.
    let total: u64 = container.sections.iter().map(|s| s.size).sum();
    container
        .sections
        .iter()
        .map(|s| SectionInfo {
            name: SharedString::from(s.name.clone()),
            address: s.address,
            size: s.size,
            kind: NativeSectionKind::from_armv8(s.kind),
            fraction: if total == 0 {
                0.
            } else {
                s.size as f32 / total as f32
            },
        })
        .collect()
}

pub fn snapshot_arm64(bin: Arm64Binary) -> Result<LoadedBundle> {
    let body = format_arm64(&bin);
    let display_label = bin
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary")
        .to_string();
    let aid = glass_db::ArtifactId::from_bytes(&bin.bytes);
    let mut native_sections = std::collections::HashMap::new();
    native_sections.insert(aid.clone(), build_section_info(&bin.container));
    let mut symbol_maps = std::collections::HashMap::new();
    symbol_maps.insert(aid.clone(), glass_arch_arm64::SymbolMap::build(&bin.container));
    let mut text_sections = std::collections::HashMap::new();
    let mut data_sections = std::collections::HashMap::new();
    for sec in &bin.container.sections {
        let kind = NativeSectionKind::from_armv8(sec.kind);
        if matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
            text_sections.insert(
                (aid.clone(), sec.name.clone()),
                TextSectionBytes {
                    base: sec.address,
                    bytes: Arc::new(sec.bytes.clone()),
                },
            );
        } else if !sec.bytes.is_empty() {
            data_sections.insert(
                (aid.clone(), sec.name.clone()),
                DataSectionBytes {
                    base: sec.address,
                    bytes: Arc::new(sec.bytes.clone()),
                    kind,
                },
            );
        }
    }
    // Build leaves: Overview + one Listing per actual text section.
    let mut tree_roots: Vec<Node> = Vec::new();
    let mut bodies: Vec<SharedString> = Vec::new();
    let mut origins: Vec<SharedString> = Vec::new();
    let mut labels_v: Vec<SharedString> = Vec::new();
    let mut kinds_v: Vec<LeafKind> = Vec::new();

    bodies.push(SharedString::from(""));
    origins.push(SharedString::from("arm64"));
    labels_v.push(SharedString::from(format!("{display_label} (overview)")));
    kinds_v.push(LeafKind::SectionMap { artifact: aid.clone() });
    tree_roots.push(Node::Leaf {
        label: SharedString::from("Overview"),
        leaf_id: LeafId(0),
    });
    let _ = body; // legacy: built earlier; no longer used now that Listing reads from text_sections
    for sec in &bin.container.sections {
        if !matches!(sec.kind, armv8_encode::container::SectionKind::Text) {
            continue;
        }
        let leaf_id = LeafId(bodies.len());
        bodies.push(SharedString::from(""));
        origins.push(SharedString::from("arm64"));
        labels_v.push(SharedString::from(sec.name.clone()));
        kinds_v.push(LeafKind::Listing {
            artifact: aid.clone(),
            section: sec.name.clone(),
        });
        tree_roots.push(Node::Leaf {
            label: SharedString::from(sec.name.clone()),
            leaf_id,
        });
    }

    Ok(LoadedBundle {
        title: format!("Glass — {}", bin.path.display()),
        tree: Arc::new(Tree { roots: tree_roots }),
        bodies: Arc::new(bodies),
        origins: Arc::new(origins),
        labels: Arc::new(labels_v),
        kinds: Arc::new(kinds_v),
        bundle_id: None,
        artifact_ids: Arc::new(vec![aid]),
        display_label,
        native_sections: Arc::new(native_sections),
        symbol_maps: Arc::new(symbol_maps),
        text_sections: Arc::new(text_sections),
        data_sections: Arc::new(data_sections),
        method_lines: Arc::new(std::collections::HashMap::new()),
        method_calls: Arc::new(std::collections::HashMap::new()),
        manifest_rows: Arc::new(Vec::new()),
        xrefs: crate::xref::XrefStore::new(),
    })
}

fn format_arm64(bin: &Arm64Binary) -> String {
    let rows = match glass_arch_arm64::linear_sweep(&bin.container) {
        Ok(r) => r,
        Err(e) => return format!("(disassembly failed: {e})"),
    };
    let mut out = String::new();
    for row in rows.iter().take(5000) {
        out.push_str(&format!("0x{:016x}  {}\n", row.address, row.text));
    }
    if rows.len() > 5000 {
        out.push_str(&format!("... ({} more rows truncated)\n", rows.len() - 5000));
    }
    out
}

/// Split `Lcom/example/Foo$Bar;` -> `["com", "example", "Foo$Bar"]`.
fn split_jni_class_name(jni: &str) -> Vec<String> {
    let trimmed = jni
        .strip_prefix('L')
        .unwrap_or(jni)
        .strip_suffix(';')
        .unwrap_or(jni);
    trimmed.split('/').map(|s| s.to_string()).collect()
}

#[derive(Default)]
struct PkgBuilder {
    /// child name -> subtree (or leaf flagged via `leaf`).
    subpkgs: std::collections::BTreeMap<String, PkgBuilder>,
    leaf: Option<LeafId>,
    /// Direct class leaves at this package level (insertion preserved by
    /// pushing into a vec).
    classes: Vec<(String, LeafId)>,
}

impl PkgBuilder {
    fn insert(&mut self, parts: &[String], id: LeafId) {
        match parts {
            [] => self.leaf = Some(id),
            [name] => self.classes.push((name.clone(), id)),
            [head, tail @ ..] => self.subpkgs.entry(head.clone()).or_default().insert(tail, id),
        }
    }

    fn finish(self) -> Vec<Node> {
        let mut out = Vec::new();
        // Packages first (sorted by BTreeMap), then classes (insertion order).
        for (name, sub) in self.subpkgs {
            let children = sub.finish();
            if children.is_empty() {
                continue;
            }
            // Collapse single-child package chains for compactness:
            //   com -> example -> Foo  shown as  com.example
            //                                       Foo
            let (label, children) = collapse_chain(name, children);
            out.push(Node::Group {
                label: SharedString::from(label),
                children,
            });
        }
        for (name, id) in self.classes {
            out.push(Node::Leaf {
                label: SharedString::from(name),
                leaf_id: id,
            });
        }
        out
    }
}

fn collapse_chain(mut label: String, mut children: Vec<Node>) -> (String, Vec<Node>) {
    while children.len() == 1 {
        if let Node::Group { label: child_label, children: child_kids } = &children[0] {
            label = format!("{label}.{child_label}");
            let next = child_kids.clone_or_take();
            children = next;
        } else {
            break;
        }
    }
    (label, children)
}

// Small helper trait so collapse_chain can move out of a borrowed Vec.
trait CloneOrTake {
    fn clone_or_take(&self) -> Vec<Node>;
}
impl CloneOrTake for Vec<Node> {
    fn clone_or_take(&self) -> Vec<Node> {
        self.iter().map(clone_node).collect()
    }
}
fn clone_node(n: &Node) -> Node {
    match n {
        Node::Group { label, children } => Node::Group {
            label: label.clone(),
            children: children.iter().map(clone_node).collect(),
        },
        Node::Leaf { label, leaf_id } => Node::Leaf {
            label: label.clone(),
            leaf_id: *leaf_id,
        },
    }
}
