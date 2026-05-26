# RLib (Rust Precompiled Library Manager)

**RLib** is a lightweight, zero-overhead development tool for Rust that precompiles crates and shares them dynamically across all your local projects. It eliminates the need for every single Rust project to compile the same massive dependencies from scratch, even in initial compilation.

---

## Key Benefits

### 1. Lightning-Fast Compilations & Universal Reuse

Once a library (such as `tokio` or `serde`) is compiled via RLib, it is stored in a centralized location (`~/.rlib`). The precompiled crate can be reused across any number of Rust projects in any directory. The initial and subsequent builds skip dependency compilation entirely, dropping build times significantly.

### 2. Massive Disk Space Savings

Standard Rust development creates a massive `target` folder for every single project, often duplicating gigabytes of compiled dependencies. RLib acts as a shared ledger, multiple projects reference the exact same precompiled `.rlib` binaries, saving massive amounts of SSD space.

### 3. Protection Against `cargo clean`

When running `cargo clean` on a project, Cargo wipes out the local `target` directory, forcing a complete re-compilation of every dependency on the next run. With RLib, the precompiled crates reside safely outside the project workspace. Running `cargo clean` will **never** delete the cached crates, they remain intact until they are explicitly removed manually using RLib commands.

### 4. Advanced Compiler & Linker speed Optimization Under the Hood

RLib automatically builds the crates using highly efficient defaults, including the `cranelift` codegen backend on nightly for near-instant compilation, and high-performance linkers like `mold` or `lld` to eliminate link-time bottlenecks. It also supports alternative memory allocators (`mimalloc` / `tcmalloc`) for the compiler.

---

## Comparison to sccache

While tools like `sccache` (Mozilla's compiler cache) solve a similar problem, RLib approaches dependency management differently:

| Feature | RLib | sccache |
| --- | --- | --- |
| **Overhead** | **Zero Overhead.** Direct path pass via compiler flags. Serverless, runs instantly as a direct CLI tool | **Compression and Client-Server Overhead.** Incurs time costs compressing and decompressing cache artifacts. Using Client-Server Model that requires background daemon process, introducing communication overhead over networking while the background server consumes resources |
| **Granularity** | **Fine-Grained Management.** Explicit control over which library versions and exact features are active or removed. | **Implicit Caching.** Automatic global matching based on hashes, hard to inspect or manually prune specific libraries. |
| **Storage Security** | Completely immune to local `cargo clean`. | Immune to local `cargo clean` but vulnerable to automatic cache size evictions. |

---

## Installation

```bash
cargo install rlib
```

---

## Command Reference

### Initialization

* `rlib init`
Creates both `rlib.config` and `rlib.list` templates in the current directory with optimal defaults.
* `rlib init list`
Generates only the `rlib.list` tracking file in the current directory.
* `rlib init config`
Generates only the `rlib.config` compiler/linker configuration file.

### Local Project Management (Current Directory)

* `rlib this add <key>`
Appends a specific precompiled library key to the local `rlib.list` file (automatically prevents duplicates).
* `rlib this remove <key>`
Removes a specific library key from the local `rlib.list` file.
* `rlib this print`
Prints all active, non-commented keys currently tracked in the local `rlib.list`.
* `rlib this print for cargo`
Clears the terminal and outputs exact `Cargo.toml` dependency lines ready for copy-pasting.
* `rlib this <cargo sub-command...> [nightly]`
A wrapper shortcut that executes any Cargo command (e.g., `check`, `run`, `build`) on the current project while passing all precompiled libraries.

### Global Cache Management

* `rlib <lib_name> [features=a,b,c]`
Compiles `<lib_name>` with specified features as a standalone release binary, moves it to the global storage, and updates the index.
* `rlib list`
Lists all available precompiled library keys stored globally in `~/.rlib/list.json`.
* `rlib list <file.list>`
Prints the active keys listed inside a specific custom `.list` file path.
* `rlib remove <key>`
Deletes the precompiled binaries from disk and purges the key entry from `~/.rlib/list.json`.

---

## Step-by-Step Usage Guide

Follow this walkthrough to initialize a project, precompile a heavy library, link it, and run a project with zero build latency.

### Step 1: Initialize the project folder

Navigate to your current Rust project workspace and generate the local configuration files:

```bash
rlib init
```

### Step 2: Check available precompiled libraries

Check if the needed library is already compiled globally on the machine:

```bash
rlib list
```

### Step 3: Compile a library (If not already cached)

If `list.json` is empty or doesn't have the library version/feature variant you need, compile it using RLib. Let's precompile `tokio` with all features enabled:

```bash
rlib tokio features=full

```

*This handles the build automatically and isolates the artifacts globally.*

### Step 4: Verify and copy the generated key

Run the list command again to see the newly generated lookup key:

```bash
rlib list
```

**Output Example:**

```text
[rlib] Entries in ~/.rlib/list.json (1):
  tokio_1_38_0_full
```

Copy that key string (`tokio_1_38_0_full`).

### Step 5: Add the key to the local workspace rlib.list

Register that specific cached binary to your current workspace directory's `rlib.list` file:

```bash
rlib this add tokio_1_38_0_full
```

### Step 6: Run cargo command

Run, check, or build your project instantly. RLib hooks into the compiler environment variables, bypassing standard dependency compilation pipelines:

```bash
rlib this cargo run
```