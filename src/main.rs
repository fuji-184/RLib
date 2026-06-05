
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use serde::{Serialize, Deserialize};
use std::fs::{File, write};
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

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

// f_comptime
const CACHE_FILE: &str = "target/.comptime_last_test";

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
            if args.len() < 3 {
                eprintln!("[rlib] Usage: rlib this [add|remove] <key>, rlib this print, rlib this print for cargo, or rlib this cargo <args>");
                exit(1);
            }
            
            if args[2] == "comptime" {
                let mut fake_args = vec![args[0].clone(), "rlib.list".to_string()];
                fake_args.extend(args[3..].iter().cloned());
                handle_comptime_subcommand(&fake_args, "rlib.list");
                exit(0);
            }
            
            let sub_cmd = &args[2];
            match sub_cmd.as_str() {
                "add" => {
                    if args.len() < 4 {
                        eprintln!("[rlib] Usage: rlib this add <key>");
                        exit(1);
                    }
                    cmd_list_add("rlib.list", &args[3]);
                }
                "remove" => {
                    if args.len() < 4 {
                        eprintln!("[rlib] Usage: rlib this remove <key>");
                        exit(1);
                    }
                    cmd_list_remove("rlib.list", &args[3]);
                }
                "print" => {
                    if args.len() == 5 && args[3] == "for" && args[4] == "cargo" {
                        cmd_print_for_cargo("rlib.list");
                    } else if args.len() == 3 {
                        cmd_list_file("rlib.list");
                    } else {
                        eprintln!("[rlib] Unknown command. Did you mean 'rlib this print' or 'rlib this print for cargo'?");
                        exit(1);
                    }
                }
                _ => {
                    let mut run_args = vec![args[0].clone(), "rlib.list".to_string()];
                    run_args.extend(args[2..].iter().cloned());
                    cmd_run(run_args);
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
                if args.len() >= 3 && args[2] == "comptime" {
                    let mut fake_args = vec![args[0].clone(), first.to_string()];
                    fake_args.extend(args[3..].iter().cloned());
                    handle_comptime_subcommand(&fake_args, first);
                    exit(0);
                }
                
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
      settings from rlib.config in the current directory, pass them
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
      
  rlib this print
      Print the active (non-comment) keys in 'rlib.list' in the current directory.
      
  rlib this print for cargo
      Clear the terminal and print all libraries in 'rlib.list' as
      Cargo.toml [dependencies] entries ready to copy-paste.
      
  rlib this <cargo sub-command...> [nightly]
      Shortcut to run cargo commands directly using 'rlib.list' in the
      current directory.

      Examples:
        rlib this cargo run
        rlib this cargo run nightly
        rlib this cargo check

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

    println!("[rlib] Passing RUSTFLAGS for: {}", keys.join(", "));
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
    let mut git_url: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut git_tag: Option<String> = None;
    let mut git_rev: Option<String> = None;

    for arg in &args[2..] {
        if let Some(feat_str) = arg.strip_prefix("features=") {
            features = feat_str
                .split(',')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect();
        } else if let Some(url) = arg.strip_prefix("git=") {
            git_url = Some(url.to_string());
        } else if let Some(branch) = arg.strip_prefix("branch=") {
            git_branch = Some(branch.to_string());
        } else if let Some(tag) = arg.strip_prefix("tag=") {
            git_tag = Some(tag.to_string());
        } else if let Some(rev) = arg.strip_prefix("rev=") {
            git_rev = Some(rev.to_string());
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

    let mut cargo_add_args = vec!["add".to_string()];
    
    if let Some(url) = git_url {
        cargo_add_args.push("--git".to_string());
        cargo_add_args.push(url);
        
        if let Some(branch) = git_branch {
            cargo_add_args.push("--branch".to_string());
            cargo_add_args.push(branch);
        } else if let Some(tag) = git_tag {
            cargo_add_args.push("--tag".to_string());
            cargo_add_args.push(tag);
        } else if let Some(rev) = git_rev {
            cargo_add_args.push("--rev".to_string());
            cargo_add_args.push(rev);
        }
        
        cargo_add_args.push(lib_name.clone());
    } else {
        cargo_add_args.push(lib_name.clone());
    }

    if !features.is_empty() {
        cargo_add_args.push("--features".to_string());
        cargo_add_args.push(features.join(","));
    }

    println!("[rlib] Adding dependency via cargo...");
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
    
    let mut folder_name = format!("{}_{}", lib_name.replace('-', "_"), version_safe);
    if !features_safe.is_empty() {
        folder_name.push('_');
        folder_name.push_str(&features_safe);
    }

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

// f_comptime start

fn latest_src_mtime() -> u64 {
    let mut latest = 0u64;
    let mut stack = vec!["src".to_string()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.to_string_lossy().to_string());
            } else if path.extension().map_or(false, |e| e == "rs") {
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(mtime) = meta.modified() {
                        let secs = mtime.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                        if secs > latest {
                            latest = secs;
                        }
                    }
                }
            }
        }
    }
    latest
}

fn last_test_timestamp() -> u64 {
    fs::read_to_string(CACHE_FILE)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn save_test_timestamp() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = fs::create_dir_all("target");
    let _ = fs::write(CACHE_FILE, now.to_string());
}

fn needs_retest() -> bool {
    if latest_src_mtime() > last_test_timestamp() {
        return true;
    }
    !comptime_files_exist()
}

fn comptime_files_exist() -> bool {
    Path::new("comptime").exists()
        && fs::read_dir("comptime")
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
}

fn run_cargo_test() {
    let output = Command::new("cargo")
        .args(&["test", "--features=comptime", "--no-run", "--message-format=json", "--profile=dev", "--", "--no-capture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("Failed to compile tests");

    if !output.status.success() {
        let _ = Command::new("cargo")
            .args(&["test", "--features=comptime", "--no-run", "--profile=dev", "--", "--no-capture"])
            .status();
        exit(1);
    }

    let mut test_binary = None;
    let reader = BufReader::new(&output.stdout[..]);
    for line_res in reader.lines() {
        if let Ok(line) = line_res {
            if line.starts_with('{') {
                if let Some(start_idx) = line.find("\"executable\":\"") {
                    let rem = &line[start_idx + 14..];
                    if let Some(end_idx) = rem.find('"') {
                        let path_str = &rem[..end_idx];
                        if !path_str.is_empty() {
                            test_binary = Some(path_str.replace("\\\\", "\\"));
                        }
                    }
                }
            }
        }
    }

    let Some(bin_path) = test_binary else {
        let _ = Command::new("cargo")
            .args(&["test", "--features=comptime", "--no-run"])
            .status();
        exit(1);
    };

    loop {
        let run_output = Command::new(&bin_path)
            .output()
            .expect("Failed to execute test binary");

        if run_output.status.success() {
            break;
        }

        let stderr_str = String::from_utf8_lossy(&run_output.stderr);
        let stdout_str = String::from_utf8_lossy(&run_output.stdout);

        if stdout_str.contains("comptime error: output not found yet")
            || stderr_str.contains("comptime error: output not found yet")
            || stdout_str.contains("ParseIntError")
            || stderr_str.contains("ParseIntError")
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }

        eprint!("{}", stdout_str);
        eprint!("{}", stderr_str);
        exit(1);
    }

    save_test_timestamp();
}

fn run_cargo_test_nested_raw() {
    let output = Command::new("cargo")
        .args(&["test", "--features=comptime", "--no-run", "--message-format=json", "--profile=dev", "--", "--no-capture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("Failed to compile tests");

    if !output.status.success() {
        let _ = Command::new("cargo")
            .args(&["test", "--features=comptime", "--no-run", "--profile=dev", "--", "--no-capture"])
            .status();
        exit(1);
    }

    let mut test_binary = None;
    let reader = BufReader::new(&output.stdout[..]);
    for line_res in reader.lines() {
        if let Ok(line) = line_res {
            if line.starts_with('{') {
                if let Some(start_idx) = line.find("\"executable\":\"") {
                    let rem = &line[start_idx + 14..];
                    if let Some(end_idx) = rem.find('"') {
                        let path_str = &rem[..end_idx];
                        if !path_str.is_empty() {
                            test_binary = Some(path_str.replace("\\\\", "\\"));
                        }
                    }
                }
            }
        }
    }

    let Some(bin_path) = test_binary else {
        let _ = Command::new("cargo")
            .args(&["test", "--features=comptime", "--no-run"])
            .status();
        exit(1);
    };

    loop {
        let run_output = Command::new(&bin_path)
            .output()
            .expect("Failed to execute test binary");

        let stderr_str = String::from_utf8_lossy(&run_output.stderr);
        let stdout_str = String::from_utf8_lossy(&run_output.stdout);

        if stdout_str.contains("comptime error: raw output not found yet")
            || stderr_str.contains("comptime error: raw output not found yet")
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        } else {
            break;
        }
    }

    let output = Command::new("cargo")
        .env("RUSTFLAGS", "--cfg comptime_ready")
        .args(&["test", "--features=comptime", "--no-run", "--message-format=json", "--profile=dev", "--", "--no-capture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("Failed to compile tests");

    if !output.status.success() {
        let _ = Command::new("cargo")
            .env("RUSTFLAGS", "--cfg comptime_ready")
            .args(&["test", "--features=comptime", "--no-run", "--profile=dev", "--", "--no-capture"])
            .status();
        exit(1);
    }

    let mut test_binary = None;
    let reader = BufReader::new(&output.stdout[..]);
    for line_res in reader.lines() {
        if let Ok(line) = line_res {
            if line.starts_with('{') {
                if let Some(start_idx) = line.find("\"executable\":\"") {
                    let rem = &line[start_idx + 14..];
                    if let Some(end_idx) = rem.find('"') {
                        let path_str = &rem[..end_idx];
                        if !path_str.is_empty() {
                            test_binary = Some(path_str.replace("\\\\", "\\"));
                        }
                    }
                }
            }
        }
    }

    let Some(bin_path) = test_binary else {
        let _ = Command::new("cargo")
            .env("RUSTFLAGS", "--cfg comptime_ready")
            .args(&["test", "--features=comptime", "--no-run"])
            .status();
        exit(1);
    };

    loop {
        let run_output = Command::new(&bin_path)
            .output()
            .expect("Failed to execute test binary");

        let stderr_str = String::from_utf8_lossy(&run_output.stderr);
        let stdout_str = String::from_utf8_lossy(&run_output.stdout);

        if stdout_str.contains("comptime error: output not found yet")
            || stderr_str.contains("comptime error: output not found yet")
            || stdout_str.contains("comptime error: raw output not found yet")
            || stderr_str.contains("comptime error: raw output not found yet")
            || stdout_str.contains("ParseIntError")
            || stderr_str.contains("ParseIntError")
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        } else {
            break;
        }
    }

    save_test_timestamp();
}

fn handle_custom_comptime(file_path: &str) {
    let path = Path::new(file_path);
    if !path.exists() {
        eprintln!("Error: Configuration file '{}' not found.", file_path);
        exit(1);
    }
    let file = File::open(path).expect("Failed to open configuration file");
    let reader = BufReader::new(file);
    for line_result in reader.lines() {
        let line = line_result.expect("Failed to read line");
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        let status = Command::new(parts[0]).args(&parts[1..]).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => exit(s.code().unwrap_or(1)),
            Err(_) => {
                eprintln!("Failed to run command: {}", trimmed);
                exit(1);
            }
        }
    }
}

fn handle_comptime_subcommand(args: &[String], list_path: &str) {
    if args.len() < 3 {
        print_help();
        exit(1);
    }
    
    let arg2 = args[2].as_str();
    if arg2 == "-h" || arg2 == "--help" {
        print_help();
        exit(0);
    }
    let nightly = args.iter().any(|a| a == "nightly");
    let use_cargo_backend = args.iter().any(|a| a == "cargo");
    if arg2 == "init" {
        if args.len() >= 4 && args[3] == "config" {
            let template = "# Add your custom commands below\ncargo build --release\n";
            let _ = write("comptime.config", template);
            exit(0);
        } else {
            exit(1);
        }
    }

    let rlib_flags = get_rlib_rustc_base_args(list_path);
    let rustflags_str = rlib_flags.join(" ");

    match arg2 {
        "check" | "run" | "build" if args.len() >= 5 && args[3] == "nested" && args[4] == "raw" => {
            if needs_retest() {
                if use_cargo_backend {
                    run_cargo_test_nested_raw();
                } else {
                    run_rustc_comptime_nested_raw(list_path, nightly);
                }
            }
            let remaining: Vec<&str> = args.iter().skip(5).filter(|&a| a != "nightly" && a != "cargo").map(|s| s.as_str()).collect();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            let mut cmd = Command::new("cargo");
            cmd.env("COMPTIME_NONCE", now.to_string())
               .env("RUSTFLAGS", &rustflags_str)
               .env("CARGO_PROFILE_DEV_BUILD_OVERRIDE_OPT_LEVEL", "3")
               .arg(arg2)
               .args(&remaining);
            let status = cmd.status();
            std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
        }
        "check" | "run" | "build" => {
            if needs_retest() {
                if use_cargo_backend {
                    run_cargo_test();
                } else {
                    run_rustc_comptime(list_path, nightly);
                }
            }
            let remaining: Vec<&str> = args.iter().skip(3).filter(|&a| a != "nightly" && a != "cargo").map(|s| s.as_str()).collect();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            let mut cmd = Command::new("cargo");
            cmd.env("COMPTIME_NONCE", now.to_string())
               .env("RUSTFLAGS", &rustflags_str)
               .env("CARGO_PROFILE_DEV_BUILD_OVERRIDE_OPT_LEVEL", "3")
               .arg(arg2)
               .args(&remaining);
            let status = cmd.status();
            std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
        }
        _ => {
            if needs_retest() {
                run_cargo_test();
            }
            handle_custom_comptime(arg2);
        }
    }
}

fn get_rlib_rustc_base_args(list_path: &str) -> Vec<String> {
    let mut args = Vec::new();
    let content = match fs::read_to_string(list_path) {
        Ok(c) => c,
        Err(_) => return args,
    };

    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let list_json = PathBuf::from(&home).join(".rlib").join("list.json");
    let all_entries = load_list(&list_json);

    for line in content.lines() {
        let key = line.trim();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        if let Some(entry) = all_entries.get(key) {
            for flag in entry.flags.split_whitespace() {
                if !flag.is_empty() {
                    args.push(flag.to_string());
                }
            }
        }
    }
    args
}

fn run_rustc_comptime(list_path: &str, nightly: bool) {
    let mut rustc_args = get_rlib_rustc_base_args(list_path);
    
    let test_src = if Path::new("src/lib.rs").exists() { "src/lib.rs" } else { "src/main.rs" };
    let out_exe = if cfg!(windows) { "target/debug/deps/comptime_test.exe" } else { "target/debug/deps/comptime_test" };
    
    let _ = std::fs::create_dir_all("target/debug/deps");

    let mut args = vec![
        test_src.to_string(),
        "--test".to_string(),
        "--cfg".to_string(), "feature=\"comptime\"".to_string(),
        "-o".to_string(), out_exe.to_string(),
    ];
    args.extend(rustc_args);

    let status = Command::new("rustc")
        .args(&args)
        .status()
        .expect("Failed to compile comptime with rustc");

    if !status.success() {
        std::process::exit(1);
    }
    
    let rustc_sysroot = Command::new("rustc")
        .args(&["--print", "sysroot"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let rust_lib_path = format!("{}/lib", rustc_sysroot);

    loop {
        let run_output = Command::new(out_exe)
            .env("LD_LIBRARY_PATH", &rust_lib_path)
            .output()
            .expect("Failed to execute test binary");

        let stderr_str = String::from_utf8_lossy(&run_output.stderr);
        let stdout_str = String::from_utf8_lossy(&run_output.stdout);

        if stdout_str.contains("comptime error: output not found yet")
            || stderr_str.contains("comptime error: output not found yet")
            || stdout_str.contains("ParseIntError")
            || stderr_str.contains("ParseIntError")
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        } else {
          break;
        }

        std::io::Write::write_all(&mut std::io::stderr(), run_output.stdout.as_slice()).unwrap();
        std::io::Write::write_all(&mut std::io::stderr(), run_output.stderr.as_slice()).unwrap();
        std::process::exit(1);
    }
    save_test_timestamp();
}

fn run_rustc_comptime_nested_raw(list_path: &str, nightly: bool) {
    let rlib_flags = get_rlib_rustc_base_args(list_path);
    let test_src = if Path::new("src/lib.rs").exists() { "src/lib.rs" } else { "src/main.rs" };
    let out_exe = if cfg!(windows) { "target/debug/deps/comptime_test.exe" } else { "target/debug/deps/comptime_test" };
    let _ = std::fs::create_dir_all("target/debug/deps");

    let mut args = vec![
        test_src.to_string(),
        "--test".to_string(),
        "--cfg".to_string(), "feature=\"comptime\"".to_string(),
        "-o".to_string(), out_exe.to_string(),
    ];
    args.extend(rlib_flags.clone());

    let status = Command::new("rustc").args(&args).status().expect("Failed to compile raw binary");
    if !status.success() { std::process::exit(1); }
    
    let rustc_sysroot = Command::new("rustc")
        .args(&["--print", "sysroot"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let rust_lib_path = format!("{}/lib", rustc_sysroot);

    loop {
        let run_output = Command::new(out_exe)
            .env("LD_LIBRARY_PATH", &rust_lib_path)
            .output()
            .expect("Failed to execute test binary");

        let stderr_str = String::from_utf8_lossy(&run_output.stderr);
        let stdout_str = String::from_utf8_lossy(&run_output.stdout);

        if stdout_str.contains("comptime error: raw output not found yet")
            || stderr_str.contains("comptime error: raw output not found yet")
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        break;
    }

    let mut ready_args = vec![
        test_src.to_string(),
        "--test".to_string(),
        "-C".to_string(), "debuginfo=2".to_string(),
        "--cfg".to_string(), "feature=\"comptime\"".to_string(),
        "--cfg".to_string(), "comptime_ready".to_string(),
        "-o".to_string(), out_exe.to_string(),
    ];
    ready_args.extend(rlib_flags);

    let status = Command::new("rustc").args(&ready_args).status().expect("Failed to compile ready binary");
    if !status.success() { std::process::exit(1); }

    loop {
        let run_output = Command::new(out_exe).output().expect("Failed to execute ready test binary");
        let stderr_str = String::from_utf8_lossy(&run_output.stderr);
        let stdout_str = String::from_utf8_lossy(&run_output.stdout);

        if stdout_str.contains("comptime error: output not found yet")
            || stderr_str.contains("comptime error: output not found yet")
            || stdout_str.contains("comptime error: raw output not found yet")
            || stderr_str.contains("comptime error: raw output not found yet")
            || stdout_str.contains("ParseIntError")
            || stderr_str.contains("ParseIntError")
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        } else {
            break;
        }
    }
    save_test_timestamp();
}