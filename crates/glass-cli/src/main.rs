use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod output;

use output::Format;

#[derive(Parser)]
#[command(name = "glass", about = "Glass mobile interactive disassembler")]
struct Cli {
    /// Bundle / binary to open when no subcommand is given. With no
    /// path either, opens an empty Glass window.
    path: Option<PathBuf>,
    /// Ignore any previously-saved tabs / expansion state on this
    /// launch. Only meaningful when running the GUI (no subcommand).
    #[arg(long)]
    fresh: bool,
    /// Output format for automation-API verbs. Ignored by the GUI
    /// and the legacy subcommands (arm64, bundle, db-dump, cfg).
    #[arg(long, value_enum, global = true, default_value_t = Format::default())]
    format: Format,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Disassemble AArch64 code from an ELF or thin Mach-O.
    Arm64 {
        path: PathBuf,
        #[arg(short, long, default_value_t = 100)]
        limit: usize,
    },
    /// Inspect an APK / IPA bundle: list DEX files, native libs, etc.
    Bundle { path: PathBuf },
    /// Open the interactive GUI. Optional bundle/binary path.
    Gui {
        path: Option<PathBuf>,
        /// Ignore any previously-saved tabs / expansion state for this
        /// launch. Writes still happen, so subsequent (non-`--fresh`)
        /// launches will pick up where you leave off.
        #[arg(long)]
        fresh: bool,
    },
    /// Show what's stored in the persistence DB for a given bundle path.
    DbDump {
        path: PathBuf,
    },
    /// Inject a fake open-tab record into the DB. Used to test restore.
    DbInjectTab {
        path: PathBuf,
        /// JNI signature, e.g. "Lcom/example/Foo;"
        class_jni: String,
    },
    /// Run build_listing_rows on a native lib's .text section and print
    /// any rows that got a string-literal comment. Helps verify adrp+add
    /// pair detection without driving the GUI.
    StringComments {
        path: PathBuf,
        /// Section name (default: ".text" or "__text" — try both).
        #[arg(long)]
        section: Option<String>,
        /// Max comments to print.
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Dump PLT-related sections and relocations from a native lib.
    PltProbe { path: PathBuf },
    /// Benchmark how long ArtifactId hashing of a file takes.
    HashBench { path: PathBuf },
    /// Build a CFG for the function at `entry_hex` (e.g. 0x100051e8)
    /// and print block / edge counts plus the layout summary.
    Cfg {
        path: PathBuf,
        /// Function entry address in hex (with or without 0x prefix).
        entry_hex: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Some(c) => c,
        // No subcommand → fall back to GUI. Honour any positional
        // path + the top-level --fresh flag.
        None => Cmd::Gui { path: cli.path, fresh: cli.fresh },
    };
    match cmd {
        Cmd::Arm64 { path, limit } => dump_arm64(path, limit),
        Cmd::Bundle { path } => dump_bundle(path),
        Cmd::Gui { path, fresh } => run_gui(path, fresh),
        Cmd::DbDump { path } => db_dump(path),
        Cmd::DbInjectTab { path, class_jni } => db_inject_tab(path, class_jni),
        Cmd::StringComments { path, section, limit } => {
            dump_string_comments(path, section, limit)
        }
        Cmd::PltProbe { path } => plt_probe(path),
        Cmd::Cfg { path, entry_hex } => {
            let entry = u64::from_str_radix(entry_hex.trim_start_matches("0x"), 16)?;
            let bin = glass_arch_arm64::Arm64Binary::open(&path)?;
            let symbols = glass_arch_arm64::SymbolMap::build(&bin.container);
            let Some(cfg) =
                glass_arch_arm64::build_function_cfg(&bin.container, &symbols, entry)
            else {
                anyhow::bail!("no function at 0x{entry:x}");
            };
            println!(
                "function @ 0x{:x}-0x{:x}  ({} blocks, {} edges)",
                cfg.entry_addr,
                cfg.end_addr,
                cfg.blocks.len(),
                cfg.edges.len(),
            );
            let max_rank = cfg.layout.iter().map(|l| l.rank).max().unwrap_or(0);
            let total_calls: usize = cfg.blocks.iter().map(|b| b.calls.len()).sum();
            let exits = cfg.blocks.iter().filter(|b| b.exits_function).count();
            println!(
                "max_rank={max_rank}  total_calls={total_calls}  exit_blocks={exits}",
            );
            for (block, layout) in cfg.blocks.iter().zip(cfg.layout.iter()).take(10) {
                println!(
                    "  block {:>2} rank={} pos=({:.1},{:.1}) addr=0x{:x}..0x{:x} ({} insns, {} calls){}",
                    block.id.0,
                    layout.rank,
                    layout.x,
                    layout.y,
                    block.start_addr,
                    block.end_addr,
                    block.instructions.len(),
                    block.calls.len(),
                    if block.exits_function { " EXIT" } else { "" },
                );
            }
            if cfg.blocks.len() > 10 {
                println!("  … ({} more)", cfg.blocks.len() - 10);
            }
            Ok(())
        }
        Cmd::HashBench { path } => {
            use std::time::Instant;
            let t = Instant::now();
            let bytes = std::fs::read(&path)?;
            println!("read {} MB in {:?}", bytes.len() / 1_000_000, t.elapsed());
            let t = Instant::now();
            let id = glass_db::ArtifactId::from_bytes(&bytes);
            println!("hash {} MB in {:?} -> {}", bytes.len() / 1_000_000, t.elapsed(), id);
            Ok(())
        }
    }
}

fn plt_probe(path: PathBuf) -> Result<()> {
    let bin = glass_arch_arm64::Arm64Binary::open(&path)?;
    let c = &bin.container;
    println!("=== plt-like sections ===");
    for s in &c.sections {
        if s.name.contains("plt") || s.name.contains("got") {
            println!(
                "  {:30} addr=0x{:08x} size=0x{:08x} bytes={}",
                s.name,
                s.address,
                s.size,
                s.bytes.len(),
            );
        }
    }
    // Group relocations by section name.
    use std::collections::HashMap;
    let mut by_sec: HashMap<&str, Vec<&armv8_encode::container::Relocation>> =
        HashMap::new();
    for r in &c.relocations {
        let name = c.sections.get(r.section.0).map(|s| s.name.as_str()).unwrap_or("?");
        by_sec.entry(name).or_default().push(r);
    }
    println!("=== relocation counts by target section ===");
    let mut names: Vec<&&str> = by_sec.keys().collect();
    names.sort();
    for n in names {
        println!("  {} : {}", n, by_sec[n].len());
    }
    for target in [".got.plt", ".got", ".rela.plt"] {
        if let Some(relocs) = by_sec.get(target) {
            println!("=== first 10 relocations targeting {} ===", target);
            for r in relocs.iter().take(10) {
                let sym_name = r
                    .symbol
                    .and_then(|id| c.symbols.get(id.0).map(|s| s.name.as_str()))
                    .unwrap_or("(no sym)");
                println!(
                    "  offset=0x{:08x} kind={:?} addend={} sym={}",
                    r.offset, r.kind, r.addend, sym_name,
                );
            }
        }
    }
    Ok(())
}

fn dump_string_comments(
    path: PathBuf,
    section_arg: Option<String>,
    limit: usize,
) -> Result<()> {
    use std::sync::Arc;
    let bin = glass_arch_arm64::Arm64Binary::open(&path)?;
    let symbols = glass_arch_arm64::SymbolMap::build(&bin.container);

    // Find the requested text section.
    let pick = section_arg.as_deref();
    let text_sec = bin
        .container
        .sections
        .iter()
        .find(|s| {
            matches!(s.kind, armv8_encode::container::SectionKind::Text)
                && pick.map(|p| p == s.name).unwrap_or(true)
        })
        .ok_or_else(|| anyhow::anyhow!("no matching text section"))?;
    println!(
        "# Disassembling {} ({} bytes)",
        text_sec.name,
        text_sec.bytes.len()
    );

    let text = glass_ui::TextSectionBytes {
        base: text_sec.address,
        bytes: Arc::new(text_sec.bytes.clone()),
    };
    // Build a DataPeek from non-text non-debug non-zero-base sections.
    // See LoadedBundle::data_sections loader for matching filter.
    let mut data_sections = Vec::new();
    for s in &bin.container.sections {
        if matches!(s.kind, armv8_encode::container::SectionKind::Text)
            || matches!(s.kind, armv8_encode::container::SectionKind::Debug)
            || s.bytes.is_empty()
            || s.address == 0
        {
            continue;
        }
        data_sections.push((s.address, Arc::new(s.bytes.clone())));
    }
    let data = glass_ui::DataPeek { sections: data_sections };
    println!("# DataPeek has {} sections", data.sections.len());
    for (b, bytes) in &data.sections {
        println!("#   0x{:x}  ({} bytes)", b, bytes.len());
    }

    let rows = glass_ui::build_listing_rows(&text, &symbols, &data, None);
    let mut found = 0usize;
    let mut rows_with_arrows = 0usize;
    let mut total_segments = 0usize;
    let mut max_lane = 0u8;
    let mut solid = 0usize;
    let mut dotted = 0usize;
    for r in &rows {
        if let glass_ui::ListingRow::Instruction { address, comment, mnemonic, arrows, .. } = r {
            if comment.contains('"') {
                found += 1;
                if found <= limit {
                    println!("0x{:016x}  {}  {}", address, mnemonic, comment);
                }
            }
            if !arrows.is_empty() {
                rows_with_arrows += 1;
                total_segments += arrows.len();
                for s in arrows.iter() {
                    if s.lane > max_lane { max_lane = s.lane; }
                    match s.style {
                        glass_ui::ArrowStyle::Solid => solid += 1,
                        glass_ui::ArrowStyle::Dotted => dotted += 1,
                    }
                }
            }
        }
    }
    println!("# Total string comments: {found}");
    println!(
        "# Arrow rows: {rows_with_arrows}  segments: {total_segments}  max_lane: {max_lane}  solid_segs: {solid}  dotted_segs: {dotted}"
    );
    Ok(())
}

fn db_inject_tab(path: PathBuf, class_jni: String) -> Result<()> {
    let bytes = std::fs::read(&path)?;
    let id = glass_db::BundleId::from_bytes(&bytes);
    let db = glass_db::Database::open(false)?;
    let mut rec = db.load_bundle(&id)?.unwrap_or(glass_db::BundleRecord {
        schema_version: 1,
        label: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        last_opened_unix: 0,
        artifacts: vec![],
        open_tabs: vec![],
        active_tab: None,
        expanded_paths: vec![],
        source_path: None,
    });
    rec.open_tabs.push(glass_db::TabState::SmaliClass { class_jni });
    rec.active_tab = Some(rec.open_tabs.len() - 1);
    db.save_bundle(id, rec);
    db.flush()?;
    println!("injected tab; relaunch `glass gui {}` to restore", path.display());
    Ok(())
}

fn db_dump(path: PathBuf) -> Result<()> {
    let bytes = std::fs::read(&path)?;
    let id = glass_db::BundleId::from_bytes(&bytes);
    let db = glass_db::Database::open(false)?;
    println!("# {} (BundleId={})", path.display(), id);
    match db.load_bundle(&id)? {
        Some(rec) => {
            println!("  label       : {}", rec.label);
            println!("  schema      : v{}", rec.schema_version);
            println!("  last opened : unix {}", rec.last_opened_unix);
            println!("  artifacts   : {}", rec.artifacts.len());
            println!("  source_path : {:?}", rec.source_path);
            println!("  expanded    : {} paths", rec.expanded_paths.len());
            println!("  active_tab  : {:?}", rec.active_tab);
            println!("  open_tabs   : {}", rec.open_tabs.len());
            for (i, t) in rec.open_tabs.iter().enumerate() {
                println!("    [{i}] {t:?}");
            }
        }
        None => println!("  (no record for this bundle)"),
    }
    Ok(())
}

fn run_gui(path: Option<PathBuf>, fresh: bool) -> Result<()> {
    // The UI handles loading itself (background + progress bar). All we do
    // here is hand it the path.
    glass_ui::launch(path, fresh)
}

fn dump_arm64(path: PathBuf, limit: usize) -> Result<()> {
    let binary = glass_arch_arm64::Arm64Binary::open(&path)?;
    let rows = glass_arch_arm64::linear_sweep(&binary.container)?;
    println!("# {} ({} bytes) — {} rows", path.display(), binary.bytes.len(), rows.len());
    for row in rows.iter().take(limit) {
        println!(
            "0x{:016x}  {:02x} {:02x} {:02x} {:02x}  {}",
            row.address, row.bytes[0], row.bytes[1], row.bytes[2], row.bytes[3], row.text,
        );
    }
    Ok(())
}

fn dump_bundle(path: PathBuf) -> Result<()> {
    use glass_mobile::Bundle;
    match Bundle::open(&path)? {
        Bundle::Apk(apk) => {
            println!("APK: {}", apk.path.display());
            println!("  DEX files: {}", apk.dex_files.len());
            for d in &apk.dex_files {
                println!("    {}", d.name);
            }
            println!("  Native libs: {}", apk.native_libs.len());
            for lib in &apk.native_libs {
                let sm = glass_arch_arm64::SymbolMap::build(&lib.binary.container);
                println!("    {}/{}  ({} symbols)", lib.abi, lib.name, sm.len());
                let mut plt_examples: Vec<&glass_arch_arm64::Symbol> = sm
                    .iter()
                    .filter(|s| s.display_name.ends_with("@plt"))
                    .take(5)
                    .collect();
                for sym in sm.iter().take(5) {
                    println!(
                        "      {:016x}  size={:#x}  src={:?}  {}",
                        sym.address, sym.size, sym.sources, sym.display_name,
                    );
                }
                if sm.len() > 5 {
                    println!("      … ({} more)", sm.len() - 5);
                }
                if !plt_examples.is_empty() {
                    println!("      sample @plt:");
                    while let Some(sym) = plt_examples.pop() {
                        println!(
                            "        {:016x}  size={:#x}  {}",
                            sym.address, sym.size, sym.display_name,
                        );
                    }
                }
            }
        }
        Bundle::Ipa(ipa) => {
            println!("IPA: {}", ipa.path.display());
            println!("  app dir       : {}", ipa.app_dir);
            println!("  bundle id     : {:?}", ipa.info.bundle_id);
            println!("  display name  : {:?}", ipa.info.display_name);
            println!("  executable    : {:?}", ipa.info.executable);
            println!("  version       : {:?} (build {:?})", ipa.info.short_version, ipa.info.build_version);
            println!("  min iOS       : {:?}", ipa.info.min_os);
            println!("  platform      : {:?}", ipa.info.platform);
            match &ipa.main_executable {
                Some(bin) => {
                    let sm = glass_arch_arm64::SymbolMap::build(&bin.container);
                    println!("  main exec     : loaded ({} bytes, {} symbols)", bin.bytes.len(), sm.len());
                    for sym in sm.iter().take(5) {
                        println!("      {:016x}  {}", sym.address, sym.display_name);
                    }
                    if sm.len() > 5 {
                        println!("      … ({} more)", sm.len() - 5);
                    }
                    let stub_examples: Vec<&glass_arch_arm64::Symbol> = sm
                        .iter()
                        .filter(|s| s.display_name.ends_with("@stubs"))
                        .take(5)
                        .collect();
                    let total_stubs = sm
                        .iter()
                        .filter(|s| s.display_name.ends_with("@stubs"))
                        .count();
                    if !stub_examples.is_empty() {
                        println!("      sample @stubs ({} total):", total_stubs);
                        for sym in &stub_examples {
                            println!(
                                "        {:016x}  size={:#x}  {}",
                                sym.address, sym.size, sym.display_name,
                            );
                        }
                    }
                }
                None => println!("  main exec     : (not loaded)"),
            }
            println!("  frameworks    : {}", ipa.frameworks.len());
            for fw in &ipa.frameworks {
                println!("      {}  ({} bytes)  {}", fw.name, fw.bytes.len(), fw.archive_path);
            }
        }
    }
    Ok(())
}
