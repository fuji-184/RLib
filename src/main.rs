
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use serde::{Serialize, Deserialize};

const BASE_FLAGS_STABLE: &str = concat!(
    "-C debuginfo=0 ",
    "-C prefer-dynamic ",
    "-C metadata=dev ",
    "-C embed-bitcode=no ",
    "-C debug-assertions=no",
);

const BASE_FLAGS_NIGHTLY: &str = concat!(
    "-Zthreads=0 ",
    "-Zshare-generics=y ",
    "-Zinline-mir=off ",
    "-Zproc-macro-backtrace=off ",
    "-Zvalidate-mir=off ",
    "-Zcache-proc-macros ",
    "-Zmacro-backtrace=off ",
    "-Zspan-debug=no ",
    "-Znext-solver ",
    "-Zrelax-elf-relocations=y ",
    "-Zprint-mono-items=off ",
    "-Zalways-encode-mir=no ",
    "-Zmeta-stats=no ",
    "-Zbinary-dep-depinfo=off ",
    "-Zno-implied-bounds-compat=y ",
    "-Zlayout-seed=0 ",
    "-Zno-leak-check ",
    "-Zub-checks=off ",
    "-Zincremental-info=off ",
    "-Zflatten-format-args=yes ",
    "-Zincremental-verify-ich=no ",
    "-Zdual-proc-macros",

);

const LLVM_FLAGS: &str = "-Cno-prepopulate-passes";
const CRANELIFT_FLAGS: &str = "-Zcodegen-backend=cranelift";

struct RlibConfig {
    backend: String,
    linker: String,
    allocator: String,
    nightly: bool,
}

impl Default for RlibConfig {
    fn default() -> Self {
        Self {
            backend: "cranelift".to_string(),
            linker: "mold".to_string(),
            allocator: "jemalloc".to_string(),
            nightly: false,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct LibEntry {
    flags: String,
    name: String,
    version: String,
    features: Vec<String>,
}

fn load_rlib_config(cwd: &Path) -> RlibConfig {
    let path = cwd.join("rlib.config");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return RlibConfig::default(),
    };

    let mut cfg = RlibConfig::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            match k {
                "backend" if matches!(v, "llvm" | "cranelift") => cfg.backend = v.to_string(),
                "linker" if matches!(v, "lld" | "mold" | "wild") => cfg.linker = v.to_string(),
                "allocator" if matches!(v, "jemalloc" | "mimalloc" | "tcmalloc") => cfg.allocator = v.to_string(),
                _ => eprintln!("[rlib] Unknown or invalid config entry ignored: {}", line),
            }
        }
    }
    cfg
}

fn backend_flags(backend: &str) -> &'static str {
    match backend {
        "cranelift" => CRANELIFT_FLAGS,
        _ => LLVM_FLAGS,
    }
}

fn resolve_linker_flags(name: &str) -> Option<&'static str> {
    let (check_bin, flag) = match name {
        "wild" => ("wild",   "-Clinker=clang -Clink-args=--ld-path=wild"),
        "mold" => ("mold",   "-Clink-arg=-fuse-ld=mold"),
        "lld"  => ("ld.lld", "-Clink-arg=-fuse-ld=lld"),
        other  => { eprintln!("[rlib] Unknown linker: {}", other); return None; }
    };
    if Command::new(check_bin).arg("--version").status().map(|s| s.success()).unwrap_or(false) {
        Some(flag)
    } else {
        move_linker_warning(name);
        None
    }
}

#[inline(never)]
fn move_linker_warning(name: &str) {
    eprintln!("[rlib] Linker '{}' not found, skipping linker flag.", name);
}

fn find_shared_lib(name: &str) -> Option<String> {
    let output = Command::new("ldconfig").arg("-p").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains(name) {
            if let Some(idx) = line.rfind("=>") {
                let path = line[idx + 2..].trim().to_string();
                if Path::new(&path).exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn build_config_flags(cfg: &RlibConfig) -> String {
    let mut parts: Vec<&str> = vec![BASE_FLAGS_STABLE];

    if cfg.nightly {
        parts.push(BASE_FLAGS_NIGHTLY);
        parts.push(backend_flags(&cfg.backend));
    }

    let mut flags = parts.join(" ");

    if let Some(lf) = resolve_linker_flags(&cfg.linker) {
        flags.push(' ');
        flags.push_str(lf);
    }

    flags
}

fn load_list(json_path: &Path) -> HashMap<String, LibEntry> {
    if !json_path.exists() {
        return HashMap::new();
    }
    let file = match fs::File::open(json_path) {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_reader(file).unwrap_or_default()
}

fn save_to_list(json_path: &Path, key: &str, entry: LibEntry) {
    let mut map = load_list(json_path);
    map.insert(key.to_string(), entry);
    if let Ok(file) = fs::File::create(json_path) {
        let _ = serde_json::to_writer_pretty(file, &map);
    } else {
        eprintln!("[rlib] Failed to write list.json");
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_help();
        exit(0);
    }

    match args[1].as_str() {
        "--help" | "-h" | "help" => print_help(),

        "init" => {
            match args.get(2).map(|s| s.as_str()) {
                Some("list") => cmd_init_list(),
                Some("config") => cmd_init_config(),
                Some("") | None => {
                    cmd_init_config();
                    cmd_init_list();
                }
                _ => {
                    eprintln!("[rlib] Usage: rlib init | rlib init list | rlib init config");
                    exit(1);
                }
            }
        }

        "list" => {
            if args.len() >= 3 {
                cmd_list_file(&args[2]);
            } else {
                cmd_list_json();
            }
        }
        
        "this" => {
            if args.len() < 4 {
                eprintln!("[rlib] Usage: rlib this add <key> | rlib this remove <key>");
                exit(1);
            }
            let sub_cmd = &args[2];
            let key = &args[3];
            match sub_cmd.as_str() {
                "add" => cmd_list_add("rlib.list", key),
                "remove" => cmd_list_remove("rlib.list", key),
                "print" => {
                    if args.len() == 5 && args[3] == "for" && args[4] == "cargo" {
                        cmd_print_for_cargo("rlib.list");
                    } else {
                        eprintln!("[rlib] Unknown command. Did you mean 'rlib this print for cargo'?");
                        exit(1);
                    }
                },
                _ => {
                    eprintln!("[rlib] Unknown sub-command for 'this'. Use 'add' or 'remove'.");
                    exit(1);
                }
            }
        }

        "remove" => {
            if args.len() < 3 {
                eprintln!("[rlib] Usage: rlib remove <key>");
                exit(1);
            }
            cmd_remove_key(&args[2]);
        }

        first => {
            let looks_like_list = first.contains('/')
                || first.contains('\\')
                || first.ends_with(".list")
                || Path::new(first).is_file();

            if looks_like_list {
                if args.len() >= 4 && args[2] == "add" {
                    cmd_list_add(first, &args[3]);
                } else if args.len() >= 4 && args[2] == "remove" {
                    cmd_list_remove(first, &args[3]);
                } else if args.len() >= 5 && args[2] == "print" && args[3] == "for" && args[4] == "cargo" {
                    cmd_print_for_cargo(first);
                } else {
                    cmd_run(args);
                }
            } else {
                cmd_build(args);
            }
        }
    }
}

fn print_help() {
    println!(
        r#"rlib — Rust prebuilt-library manager

BUILDING
  rlib <lib_name> [features=a,b,c]
      Build <lib_name> as a release .rlib, copy all deps to
      ~/.rlib/<lib>_<version>_<features>/, and save the rustc
      flags to ~/.rlib/list.json.

      Examples:
        rlib tokio features=full
        rlib serde features=derive,std
        rlib anyhow

RUNNING CARGO WITH RLIB FLAGS
  rlib <rlib.list> <cargo sub-command...> [nightly]
      Read library keys from <rlib.list> (one per line), look them
      up in ~/.rlib/list.json, combine their rustc flags plus the
      settings from rlib.config in the current directory, inject them
      as RUSTFLAGS, then execute the cargo command.
      Lines starting with # are treated as comments and ignored.
      Append 'nightly' to enable nightly-only flags (-Z flags, cranelift).

      Examples:
        rlib mylibs.list cargo run
        rlib mylibs.list cargo check
        rlib mylibs.list cargo build
        rlib mylibs.list cargo run nightly
        rlib mylibs.list cargo build nightly

INITIALISING
  rlib init
      Create both rlib.config and rlib.list in the current directory
      with default values and usage instructions.
      
  rlib init list
      Create rlib.list in the current directory with usage instructions
      as comments inside the file.

  rlib init config
      Create rlib.config in the current directory with default values
      and usage instructions as comments.

INSPECTING
  rlib list
      Print all keys currently stored in ~/.rlib/list.json.

  rlib list <rlib.list>
      Print the active (non-comment) keys in <rlib.list>.

MANAGING list.json
  rlib remove <key>
      Remove <key> from ~/.rlib/list.json AND delete its folder
      ~/.rlib/<key>/ from disk.
      
MANAGING CURRENT DIRECTORY LIST
  rlib this add <key>
      Append <key> to 'rlib.list' in the current directory (no duplicates).

  rlib this remove <key>
      Remove <key> from 'rlib.list' in the current directory.
      
  rlib this print for cargo
      Clear the terminal and print all libraries in 'rlib.list' as
      Cargo.toml [dependencies] entries ready to copy-paste.

MANAGING A SPECIFIC .list FILE
  rlib <rlib.list> add <key>
      Append <key> to <rlib.list> (no duplicates).

  rlib <rlib.list> remove <key>
      Remove <key> from <rlib.list>.

  rlib <rlib.list> print for cargo
      Clear the terminal and print all libraries in <rlib.list> as
      Cargo.toml [dependencies] entries ready to copy-paste.

OTHER
  rlib --help
      Show this help message."#
    );
}

fn cmd_init_list() {
    let dest = Path::new("rlib.list");
    if dest.exists() {
        eprintln!("[rlib] rlib.list already exists in the current directory.");
        exit(1);
    }

    let template = "\
#   Show active keys in this file:
#      rlib list rlib.list
#
#   Add a key to this file (after building it with rlib):
#      rlib rlib.list add tokio_1_52_3_full
#
#   Remove a key from this file:
#      rlib rlib.list remove tokio_1_52_3_full
#
# Add your library keys below:


";

    fs::write(dest, template).unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to create rlib.list: {}", e);
        exit(1);
    });

    println!("[rlib] Created rlib.list in the current directory.");
}

fn cmd_init_config() {
    let dest = Path::new("rlib.config");
    if dest.exists() {
        eprintln!("[rlib] rlib.config already exists in the current directory.");
        exit(1);
    }

    let template = "\
# backend — codegen backend to use.
# Values : llvm | cranelift
# Default: cranelift
# Note   : cranelift is only active when running with 'nightly' keyword.
#          On stable, llvm is always used regardless of this setting.

backend=cranelift

# linker — linker to use for faster linking.
# Values : lld | mold | wild
# Default: mold

linker=mold

# allocator — global memory allocator for the compiler.
# Values : jemalloc | mimalloc | tcmalloc
# Default: jemalloc
# Note   : jemalloc is the compiler default allocator on Linux
#          mimalloc and tcmalloc require the shared
#          library to be installed on the system.

allocator=jemalloc
";

    fs::write(dest, template).unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to create rlib.config: {}", e);
        exit(1);
    });

    println!("[rlib] Created rlib.config in the current directory.");
}

fn cmd_list_json() {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let list_json = PathBuf::from(&home).join(".rlib").join("list.json");
    let map = load_list(&list_json);

    if map.is_empty() {
        println!("[rlib] list.json is empty or does not exist.");
        return;
    }

    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    println!("[rlib] Entries in ~/.rlib/list.json ({}):", keys.len());
    for k in keys {
        println!("  {}", k);
    }
}

fn cmd_list_file(list_path: &str) {
    let content = match fs::read_to_string(list_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rlib] Failed to read '{}': {}", list_path, e);
            exit(1);
        }
    };

    let keys = active_lines(&content);

    if keys.is_empty() {
        println!("[rlib] '{}' has no active entries.", list_path);
        return;
    }

    println!("[rlib] Active entries in '{}' ({}):", list_path, keys.len());
    for k in keys {
        println!("  {}", k);
    }
}

fn cmd_remove_key(key: &str) {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let rlib_base = PathBuf::from(&home).join(".rlib");
    let list_json = rlib_base.join("list.json");

    let mut map = load_list(&list_json);
    if map.remove(key).is_none() {
        eprintln!("[rlib] Key '{}' not found in list.json.", key);
    } else {
        if let Ok(file) = fs::File::create(&list_json) {
            let _ = serde_json::to_writer_pretty(file, &map);
        }
        println!("[rlib] Removed '{}' from list.json.", key);
    }

    let folder = rlib_base.join(key);
    if folder.exists() {
        fs::remove_dir_all(&folder).unwrap_or_else(|e| {
            eprintln!("[rlib] Failed to delete folder '{}': {}", folder.display(), e);
            exit(1);
        });
        println!("[rlib] Deleted folder '{}'.", folder.display());
    } else {
        println!("[rlib] Note: folder '{}' did not exist on disk.", folder.display());
    }
}

fn cmd_list_add(list_path: &str, key: &str) {
    let existing = fs::read_to_string(list_path).unwrap_or_default();

    if active_lines(&existing).contains(&key) {
        eprintln!("[rlib] Error: Key '{}' already exists in '{}'. Connection rejected.", key, list_path);
        exit(1);
    }

    let mut new_content = existing;
    if !new_content.ends_with('\n') && !new_content.is_empty() {
        new_content.push('\n');
    }
    new_content.push_str(key);
    new_content.push('\n');

    fs::write(list_path, new_content).unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to write '{}': {}", list_path, e);
        exit(1);
    });
    println!("[rlib] Added '{}' to '{}'.", key, list_path);
}

fn cmd_list_remove(list_path: &str, key: &str) {
    let existing = match fs::read_to_string(list_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rlib] Failed to read '{}': {}", list_path, e);
            exit(1);
        }
    };

    let had_key = active_lines(&existing).contains(&key);
    if !had_key {
        println!("[rlib] '{}' was not found in '{}'.", key, list_path);
        return;
    }

    let new_content: String = existing
        .lines()
        .filter(|l| l.trim() != key)
        .map(|l| format!("{}\n", l))
        .collect();

    fs::write(list_path, new_content).unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to write '{}': {}", list_path, e);
        exit(1);
    });
    println!("[rlib] Removed '{}' from '{}'.", key, list_path);
}

fn cmd_run(args: Vec<String>) {
    if args.len() < 3 {
        eprintln!("[rlib] Usage: rlib <rlib.list> <cargo sub-command...> [nightly]");
        exit(1);
    }

    let list_path = &args[1];

    let nightly = args.iter().any(|a| a == "nightly");
    let cargo_args: Vec<&str> = args[2..]
        .iter()
        .filter(|a| a.as_str() != "nightly")
        .map(String::as_str)
        .collect();

    let list_content = match fs::read_to_string(list_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rlib] Failed to read list file '{}': {}", list_path, e);
            exit(1);
        }
    };

    let keys = active_lines(&list_content);
    if keys.is_empty() {
        eprintln!("[rlib] The list file has no active entries.");
        exit(1);
    }

    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let list_json = PathBuf::from(&home).join(".rlib").join("list.json");
    let all_entries = load_list(&list_json);

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut cfg = load_rlib_config(&cwd);
    cfg.nightly = nightly;

    if !nightly && cfg.backend == "cranelift" {
        cfg.backend = "llvm".to_string();
    }

    let mut combined_flags: Vec<&str> = Vec::new();
    let config_flags = build_config_flags(&cfg);
    combined_flags.extend(config_flags.split_whitespace());

    for key in &keys {
        match all_entries.get(*key) {
            Some(entry) => combined_flags.extend(entry.flags.split_whitespace()),
            None => eprintln!("[rlib] Warning: '{}' not found in list.json — skipping.", key),
        }
    }

    let rustflags = combined_flags.join(" ");
    if rustflags.is_empty() {
        eprintln!("[rlib] No valid flags found for the requested libraries.");
        exit(1);
    }

    println!("[rlib] Injecting RUSTFLAGS for: {}", keys.join(", "));
    println!(
        "[rlib] Backend: {}, Linker: {}, Allocator: {}, Channel: {}",
        cfg.backend,
        cfg.linker,
        cfg.allocator,
        if nightly { "nightly" } else { "stable" }
    );

    let (bin, bin_args) = if cargo_args[0] == "cargo" {
        ("cargo", &cargo_args[1..])
    } else {
        (cargo_args[0], &cargo_args[1..])
    };

    let mut cmd = Command::new(bin);
    cmd.args(bin_args);
    cmd.env("RUSTFLAGS", &rustflags);
    cmd.env("CARGO_PROFILE_DEV_BUILD_OVERRIDE_OPT_LEVEL", "3");

    if cfg.nightly && cfg.backend == "cranelift" {
        cmd.env("CARGO_CACHE_RUSTC_INFO", "1");
    }

    if cfg.allocator != "jemalloc" {
        let lib_name = match cfg.allocator.as_str() {
            "mimalloc" => "libmimalloc.so",
            "tcmalloc" => "libtcmalloc.so",
            _ => "",
        };
        if !lib_name.is_empty() {
            match find_shared_lib(lib_name) {
                Some(path) => { cmd.env("LD_PRELOAD", path); }
                None => eprintln!("[rlib] {} not found, using default allocator.", lib_name),
            }
        }
    }

    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to run '{}': {}", bin, e);
        exit(1);
    });

    exit(status.code().unwrap_or(1));
}

fn cmd_build(args: Vec<String>) {
    let lib_name = &args[1];
    let mut features: Vec<String> = Vec::new();

    for arg in &args[2..] {
        if let Some(feat_str) = arg.strip_prefix("features=") {
            features = feat_str
                .split(',')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect();
        }
    }

    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let rlib_base = PathBuf::from(&home).join(".rlib");
    let gen_project = rlib_base.join("rlib_gen");

    fs::create_dir_all(&rlib_base).unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to create ~/.rlib: {}", e);
        exit(1);
    });

    if !gen_project.exists() {
        println!("[rlib] Creating rlib_gen project...");
        run_command("cargo", &["new", "--lib", "rlib_gen"], &rlib_base, "cargo new");
    }

    println!("[rlib] Clearing old dependencies from Cargo.toml...");
    clean_cargo_toml(&gen_project);

    let target_dir = gen_project.join("target");
    if target_dir.exists() {
        println!("[rlib] Removing target directory...");
        fs::remove_dir_all(&target_dir).unwrap_or_else(|e| {
            eprintln!("[rlib] Failed to remove target directory: {}", e);
            exit(1);
        });
    }

    println!("[rlib] Adding dependency: {} {:?}", lib_name, features);
    let mut cargo_add_args = vec!["add".to_string(), lib_name.clone()];
    if !features.is_empty() {
        cargo_add_args.push("--features".to_string());
        cargo_add_args.push(features.join(","));
    }
    run_command(
        "cargo",
        &cargo_add_args.iter().map(String::as_str).collect::<Vec<_>>(),
        &gen_project,
        "cargo add",
    );

    println!("[rlib] Building release...");
    run_command("cargo", &["build", "--release"], &gen_project, "cargo build --release");

    let version = get_lib_version(&gen_project, lib_name);
    let version_safe = version.replace('.', "_");
    let features_safe = features.join("_");
    let folder_name = if features_safe.is_empty() {
        format!("{}_{}", lib_name.replace('-', "_"), version_safe)
    } else {
        format!("{}_{}_{}", lib_name.replace('-', "_"), version_safe, features_safe)
    };

    let output_dir = rlib_base.join(&folder_name);
    fs::create_dir_all(&output_dir).unwrap_or_else(|e| {
        eprintln!("[rlib] Failed to create output directory: {}", e);
        exit(1);
    });

    let deps_dir = gen_project.join("target").join("release").join("deps");
    println!("[rlib] Copying deps to {}...", output_dir.display());
    copy_dir_contents(&deps_dir, &output_dir);

    let flags = build_flags(&output_dir);
    let list_json = rlib_base.join("list.json");
    save_to_list(&list_json, &folder_name, LibEntry {
        flags,
        name: lib_name.to_string(),
        version,
        features,
    });

    Command::new("clear").status().ok();
    print!("\x1B[2J\x1B[1;1H");
    println!("[rlib] Done. Saved '{}' to ~/.rlib/list.json", folder_name);
}

fn cmd_print_for_cargo(list_path: &str) {
    let content = match fs::read_to_string(list_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rlib] Failed to read '{}': {}", list_path, e);
            exit(1);
        }
    };

    let keys = active_lines(&content);
    if keys.is_empty() {
        eprintln!("[rlib] '{}' has no active entries.", list_path);
        exit(1);
    }

    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let list_json = PathBuf::from(&home).join(".rlib").join("list.json");
    let all_entries = load_list(&list_json);

    Command::new("clear").status().ok();
    print!("\x1B[2J\x1B[1;1H");

    println!("[dependencies]");
    for key in keys {
        match all_entries.get(key) {
            Some(entry) => {
                if entry.features.is_empty() {
                    println!("{} = \"{}\"", entry.name, entry.version);
                } else {
                    let feats: Vec<String> = entry.features.iter()
                        .map(|f| format!("\"{}\"", f))
                        .collect();
                    println!(
                        "{} = {{ version = \"{}\", features = [{}] }}",
                        entry.name,
                        entry.version,
                        feats.join(", ")
                    );
                }
            }
            None => eprintln!("[rlib] Warning: '{}' not found in list.json — skipping.", key),
        }
    }
}

fn active_lines<'a>(content: &'a str) -> Vec<&'a str> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn build_flags(output_dir: &Path) -> String {
    let mut rlib_files: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(output_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "rlib").unwrap_or(false) {
                let fname = path.file_name().unwrap().to_string_lossy().to_string();
                if let Some(crate_name) = extract_crate_name(&fname) {
                    rlib_files.push((crate_name, fname));
                }
            }
        }
    }
    rlib_files.sort_by(|a, b| a.0.cmp(&b.0));

    let dir_path = output_dir.to_string_lossy();
    let mut parts = vec![format!("-L {}", dir_path)];
    for (crate_name, fname) in &rlib_files {
        parts.push(format!("--extern {}={}/{}", crate_name, dir_path, fname));
    }
    parts.join(" ")
}

fn clean_cargo_toml(project_dir: &Path) {
    let toml_path = project_dir.join("Cargo.toml");
    let content = match fs::read_to_string(&toml_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rlib] Failed to read Cargo.toml: {}", e);
            return;
        }
    };

    let mut out: Vec<&str> = Vec::new();
    let mut in_deps = false;

    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = matches!(t, "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]");
            out.push(line);
            continue;
        }
        if in_deps {
            if t.is_empty() {
                out.push(line);
            }
        } else {
            out.push(line);
        }
    }

    if let Err(e) = fs::write(&toml_path, out.join("\n")) {
        eprintln!("[rlib] Failed to write Cargo.toml: {}", e);
    }
}

fn get_lib_version(project_dir: &Path, lib_name: &str) -> String {
    let lockfile = project_dir.join("Cargo.lock");
    if let Ok(content) = fs::read_to_string(&lockfile) {
        let lib_hyphen = lib_name.replace('_', "-");
        let lib_under = lib_name.replace('-', "_");
        let mut in_block = false;
        let mut found_name = false;

        for line in content.lines() {
            if line == "[[package]]" {
                in_block = true;
                found_name = false;
            } else if in_block {
                if line.starts_with("name = ") {
                    let name_val = line.trim_start_matches("name = ").trim_matches('"');
                    found_name = name_val == lib_name || name_val == lib_hyphen || name_val == lib_under;
                } else if line.starts_with("version = ") && found_name {
                    return line.trim_start_matches("version = ").trim_matches('"').to_string();
                } else if line.is_empty() {
                    in_block = false;
                }
            }
        }
    }
    "0.0.0".to_string()
}

fn copy_dir_contents(src: &Path, dst: &Path) {
    if !src.exists() {
        eprintln!("[rlib] deps directory not found: {}", src.display());
        return;
    }
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[rlib] Failed to read deps directory: {}", e);
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let dest = dst.join(path.file_name().unwrap());
            if let Err(e) = fs::copy(&path, &dest) {
                eprintln!("[rlib] Failed to copy {}: {}", path.display(), e);
            }
        }
    }
}

fn run_command(bin: &str, args: &[&str], cwd: &Path, label: &str) {
    let status = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("[rlib] Failed to run {}: {}", label, e);
            exit(1);
        });
    if !status.success() {
        eprintln!("[rlib] {} failed", label);
        exit(1);
    }
}

fn extract_crate_name(fname: &str) -> Option<String> {
    let without_prefix = fname.strip_prefix("lib")?;
    let without_ext = without_prefix.strip_suffix(".rlib")?;
    let parts: Vec<&str> = without_ext.rsplitn(2, '-').collect();
    if parts.len() == 2 {
        Some(parts[1].to_string())
    } else {
        Some(without_ext.to_string())
    }
}