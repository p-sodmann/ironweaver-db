# Modern Rust Feature Reference (1.85 → 1.99)

Reference for upgrading and modernizing Rust codebases. It is meant to be referenced from other prompts, for example "use `rust_features.md` as the feature catalogue".

- **Current stable:** 1.99.0 (2026-10-01). Next release: 1.100 (expected 2026-11-12).
- **Baseline assumed:** 1.85 (Edition 2024 released).
- **Source:** official Rust release notes (`doc.rust-lang.org/stable/releases.html`).
- **Always verify** an API against the actual toolchain in use (`rustup show`, docs for that version) before relying on it. Signatures in snippets are illustrative. Check the docs when exactness matters.
- the new msrv will be 1.99.0

## How an agent should use this file

1. This is a **catalogue, not a checklist**. Never introduce a feature just because it is listed here.
2. Adopt a feature only if it makes the code **safer, simpler, faster, easier to maintain, or removes a dependency**. Otherwise leave existing code alone.
3. Respect the project's MSRV. A feature marked `(1.99)` raises the MSRV to at least 1.99.
4. Each entry lists **Use for** and **Avoid** guidance. Follow the "Avoid" part as strictly as the "Use for" part.
5. Read section 16 (upgrade gotchas) before bumping a toolchain.

---

## 0. Version map

| Version | Date | Headline items |
|---|---|---|
| 1.85 | 2025-02-20 | Edition 2024, async closures, rust-version-aware resolver |
| 1.86 | 2025-04-03 | Trait upcasting, `get_disjoint_mut`, safe `#[target_feature]` fns, `Vec::pop_if`, `OnceLock::wait` |
| 1.87 | 2025-05-15 | `Vec::extract_if`, safe arch intrinsics, `io::pipe`, `split_off*` on slices, `is_multiple_of`, `unbounded_shl/shr`, `cast_signed/unsigned`, `use<..>` in traits, `asm_goto` |
| 1.88 | 2025-06-26 | Let chains (2024), naked functions, `cfg(true/false)`, `HashMap::extract_if`, `as_chunks`, `Cell::update`, `hint::select_unpredictable`, Cargo auto cache GC |
| 1.89 | 2025-08-07 | `File::lock*`, AVX-512 target features + intrinsics, `_` for inferred const args, `repr(u128)`, `Result::flatten`, `NonNull::from_ref/from_mut`, `mismatched_lifetime_syntaxes` lint |
| 1.90 | 2025-09-18 | LLD default on x86_64 Linux, `cargo publish --workspace`, `u{n}::*_sub_signed` |
| 1.91 | 2025-10-30 | `strict_*` integer ops, `carrying_add/mul`, `BTreeMap::extract_if`, `AtomicPtr::fetch_*`, `floor/ceil_char_boundary`, `core::array::repeat`, Cargo `build.build-dir`, aarch64 Windows Tier 1 |
| 1.92 | 2025-12-11 | `RwLockWriteGuard::downgrade`, `Box/Rc/Arc::new_zeroed*`, `NonZero::div_ceil`, `btree_map::Entry::insert_entry`, unwind tables with `panic=abort` |
| 1.93 | 2026-01-22 | `[MaybeUninit<T>]` helpers, `as_array`, `Vec/String::into_raw_parts`, `VecDeque::pop_*_if`, `fmt::from_fn`, `unchecked_shl/shr`, musl 1.2.5, global allocator may use TLS |
| 1.94 | 2026-03-05 | `array_windows`, `element_offset`, `LazyLock/LazyCell::get*`, `Peekable::next_if_map`, Cargo config `include`, TOML 1.1 |
| 1.95 | 2026-04-16 | `if let` guards, `cfg_select!`, `Vec::push_mut/insert_mut`, `Atomic*::update/try_update`, `hint::cold_path`, `core::range::RangeInclusive`, `Layout::repeat/extend_packed/dangling_ptr`, `bool: TryFrom<int>` |
| 1.96 | 2026-05-28 | `core::range::{Range, RangeFrom, RangeToInclusive}`, `assert_matches!`, `debug_assert_matches!`, `NonZero` ranges |
| 1.97 | 2026-07-09 | Cargo `build.warnings`, v0 symbol mangling default, linker output shown, `highest_one/lowest_one/bit_width/isolate_*` |
| 1.98 | 2026-08-20 | Algebraic float ops, `NumBuffer` / `format_into`, `substr_range/subslice_range`, `strip_circumfix`, `Atomic<T>::from_mut*`, `bool::ok_or` |
| 1.99 | 2026-10-01 | C-variadic definitions, `Vec::into_parts/from_parts`, `Box::into_non_null/from_non_null`, `size_of_val_raw`/`align_of_val_raw`/`Layout::for_value_raw`, `fs::set_times`, `VecDeque::retain_back`, `UnsafeCell` access guarantee |

Patch releases fixed miscompilations (1.96.1, 1.97.1, 1.98.1). **Always use the latest patch release**, never `x.y.0`, when an `x.y.1` exists.

---

## 1. Edition 2024 (prerequisite for several features)

Several features (notably **let chains**) are only available on edition 2024. Migrate a crate with `cargo fix --edition`, then set `edition = "2024"`, then review the result manually. Do the migration as an isolated commit, and do the rustfmt style-edition change as a separate commit.

Key changes and what to review:

- **`if let` temporary scope.** Temporaries in an `if let` scrutinee are now dropped before the `else` block. This can fix latent deadlocks such as `if let Some(x) = lock.read().get(k) { … } else { lock.write() … }`, but it can also change behaviour. **Audit every lock guard, `RefCell` borrow, and other drop-significant temporary in `if let`/`while let`/`match` scrutinees.**
- **Tail-expression temporary scope.** Temporaries in a block's tail expression are dropped before the block's locals. This affects guards that are returned or borrowed at the end of a block.
- **RPIT lifetime capture.** `impl Trait` in return position captures all in-scope generic parameters and lifetimes by default. Use `use<'a, T>` to narrow the capture. Traits support this since 1.87.
- **`unsafe extern { … }`** blocks are required, and items inside can be marked `safe` or `unsafe`.
- **Unsafe attributes** are now written `#[unsafe(no_mangle)]`, `#[unsafe(export_name = …)]`, and `#[unsafe(link_section = …)]`.
- **`unsafe_op_in_unsafe_fn`** warns by default. Unsafe operations inside an `unsafe fn` need their own `unsafe {}` block. Use this to make unsafe scopes explicit.
- **`static_mut_refs`** denies by default. Replace `&STATIC_MUT` with `&raw const`/`&raw mut`, atomics, `OnceLock`, or `Mutex`.
- **`std::env::set_var` / `remove_var` are `unsafe`.** These calls are common in tests and setup code. Restructure to pass configuration explicitly where possible.
- **Never-type fallback** changed from `()` to `!`. Watch for inference changes in code using `panic!`/`todo!` in generic positions.
- **`gen` is a reserved keyword.**
- **`Box<[T]>: IntoIterator`** iterates by value.
- **Macro `expr` fragment** now also matches `const { … }` and `_`. Use `expr_2021` to keep the old behaviour.
- **Prelude** adds `Future` and `IntoFuture`.
- **Cargo resolver v3** is rust-version aware: it prefers dependency versions compatible with your `rust-version`.
- **rustfmt style edition 2024** applies when formatting.
- **Rustdoc combined doctests** are faster. Doctests that rely on process-global state may need `standalone_crate`.
- **(1.99, Cargo)** On edition ≥ 2024, a workspace member can override an inherited dependency's `default-features = false`. Older editions ignore this with a warning.

---

## 2. Control flow and pattern matching

### Let chains (1.88, edition 2024)
Chain `let` patterns and boolean conditions with `&&` in `if` and `while` conditions.
```rust
if let Some(node) = graph.get(id)
    && let Some(props) = node.properties()
    && !props.is_empty()
{ … }
```
- **Use for:** flattening nested `if let` / `match` pyramids, or `if let … { if cond { … } }`.
- **Avoid:** chains longer than about 4 clauses, where a helper function or early return reads better. Also avoid them where the `else` branch needs to know *which* clause failed.

### `if let` guards in `match` (1.95)
```rust
match token {
    Token::Number(s) if let Ok(n) = s.parse::<u64>() => Expr::Lit(n),
    Token::Number(s) => return Err(Error::BadNumber(s)),
    …
}
```
- **Use for:** arms that currently contain a nested `if let` with a fallthrough arm.
- **Avoid:** guards with side effects or expensive work. Guards can run for multiple arms.

### `assert_matches!` / `debug_assert_matches!` (1.96)
```rust
assert_matches!(result, Err(Error::Corrupt { .. }));
assert_matches!(page.kind(), PageKind::Leaf | PageKind::Internal);
```
- **Use for:** replacing `assert!(matches!(…))`. Failure messages now show the actual value. This lets you drop crates like `assert_matches`.
- **Note:** `debug_assert_matches!` is a debug-only invariant check, useful for internal state machines.

### `cfg(true)` / `cfg(false)` (1.88)
Use these for temporarily disabling code or in macro-generated cfgs. Prefer them over `cfg(any())` hacks.

---

## 3. Collections and slices

### `get_disjoint_mut` (1.86): slices and `HashMap`
Get several mutable references at once.
```rust
// slices: Result<[&mut T; N], GetDisjointMutError>
let [src, dst] = nodes.get_disjoint_mut([a, b])?;
src.out_edges.push(e);
dst.in_edges.push(e);

// HashMap: [Option<&mut V>; N]; panics if keys overlap
let [a, b] = map.get_disjoint_mut([&k1, &k2]);
```
- **Use for:** mutating two graph nodes or records at once (edge insertion, merges, swaps). It replaces `split_at_mut` gymnastics, remove-and-reinsert, `RefCell` used only for this purpose, and unsafe aliasing.
- **Avoid:** using the HashMap variant where keys may be equal unless that case is guarded beforehand, because it panics. The slice variant returns an error instead. `get_disjoint_unchecked_mut` is `unsafe`, so prefer the checked versions.

### `extract_if`: Vec/LinkedList (1.87), HashMap/HashSet (1.88), BTreeMap/BTreeSet (1.91)
Remove matching elements and yield them, in a single pass.
```rust
let removed: Vec<Edge> = edges.extract_if(.., |e| e.target == dead_node).collect();
for (k, v) in index.extract_if(|_, v| v.is_tombstone()) { … }
```
- `Vec::extract_if` takes a **range** argument first (use `..` for all elements).
- The iterator is **lazy**: only elements that are actually iterated get removed. Dropping the iterator early keeps the rest.
- **Use for:** cascade deletes, GC or tombstone sweeps, and "retain + collect removed" patterns.
- **Avoid:** cases where you do not need the removed items. Plain `retain` is clearer there.

### `Vec::push_mut` / `insert_mut`, plus `VecDeque` and `LinkedList` equivalents (1.95)
```rust
let node = arena.push_mut(Node::default());
node.id = id;
```
- **Use for:** replacing `push(x); let r = v.last_mut().unwrap();` and index-then-lookup patterns in arenas.
- **Avoid:** cases where the existing code already constructs the value fully before pushing it.

### `array_windows` (1.94)
```rust
for [prev, next] in sorted_ids.array_windows() {
    debug_assert!(prev < next);
}
```
- **Use for:** fixed-size adjacent windows, such as sorted-invariant checks, run detection, and delta encoding.
- **Avoid:** cases needing dynamic window sizes. Keep `windows(n)` for those.

### `as_chunks` / `as_rchunks` and `_mut` variants (1.88)
```rust
let (records, tail): (&[[u8; 16]], &[u8]) = page.as_chunks::<16>();
```
- **Use for:** fixed-size record arrays in pages, and SIMD-width batching. It replaces `chunks_exact` + `try_into().unwrap()`.

### `as_array` / `as_mut_array` (1.93)
Returns `Option<&[T; N]>` **only if `len == N` exactly**.
```rust
let header: &[u8; 8] = buf[..8].as_array().ok_or(Error::Truncated)?;
```
- **Use for:** replacing `<&[u8; N]>::try_from(slice).unwrap()` and `slice.try_into().unwrap()`.

### `split_off`, `split_off_first`, `split_off_last` and `_mut` variants (1.87)
These operate on `&mut &[T]`, which makes them a natural cursor for decoding.
```rust
let mut rest: &[u8] = buf;
let tag = *rest.split_off_first().ok_or(Error::Eof)?;
let len_bytes = rest.split_off(..4).ok_or(Error::Eof)?;
```
- **Use for:** hand-rolled binary decoders that currently track offsets manually.

### `element_offset` (1.94)
Returns the index of an element reference within a slice. Use it instead of pointer arithmetic for "which index is this `&T`?".

### Other collection APIs
- `Vec::pop_if` (1.86)
- `VecDeque::pop_front_if` / `pop_back_if` (1.93): buffer pools, LRU eviction, queues.
- `VecDeque::retain_back` (1.99)
- `btree_map::Entry::insert_entry` (1.92): insert and keep an `OccupiedEntry`.
- `core::array::repeat` (1.91)
- `IntoIterator for Box<[T; N]>` (1.99)
- `NonZero` integer ranges are iterable (1.96).

---

## 4. Ranges

### New `core::range` / `std::range` types (`RangeInclusive` 1.95; `Range`, `RangeFrom`, `RangeToInclusive` 1.96)
The new range types are **`Copy`** and are not themselves iterators. Call `.iter()` (or `IntoIterator`) to iterate. The legacy types live at `std::range::legacy` (1.98), and conversions exist between new and legacy types.
- **Use for:** internal span types (byte spans, page ranges, token spans) that are stored in structs and copied around. With the new types you no longer need `.clone()` or start/end pairs.
- **Avoid:** public API churn, and mass migration of ordinary `a..b` loops.
- **1.99 note:** iteration on legacy `RangeInclusive` was optimized. The `start()`/`end()` values of an **exhausted** `RangeInclusive`, and its use as a slice index, may differ from before. These behaviours were never guaranteed, so do not rely on them.

---

## 5. Integers and arithmetic

### `strict_*` arithmetic (1.91)
`strict_add`, `strict_sub`, `strict_mul`, `strict_div`, `strict_rem`, `strict_neg`, `strict_shl`, `strict_shr`, `strict_pow`, `strict_abs`, plus signed/unsigned mixes. These **always panic** on overflow, regardless of `overflow-checks`.
- **Use for:** offsets, sizes, and IDs where overflow can only mean a bug.
- **Avoid:** values derived from **untrusted or possibly corrupted input** (on-disk data, network data). Use `checked_*` with a proper error there, since panicking during recovery or parsing is worse than returning an error. Keep intentional `wrapping_*`/`saturating_*` code unchanged.

### Bit operations (1.97)
`highest_one`, `lowest_one` (return `Option<u32>` bit index), `isolate_highest_one`, `isolate_lowest_one`, and `bit_width`. They are available on all integers and `NonZero`.
- **Use for:** bitmaps and free-lists, allocator size classes, and "next power-of-two bucket" calculations. They replace `BITS - leading_zeros()` arithmetic and `x & x.wrapping_neg()` tricks.

### Other integer APIs
- `is_multiple_of` (1.87): alignment and page-size checks. It returns false instead of panicking when the divisor is 0.
- `unbounded_shl` / `unbounded_shr` (1.87): shifts ≥ BITS yield 0 instead of panicking or being UB. Good for bitmask generation.
- `cast_signed` / `cast_unsigned` (1.87): an explicit same-width reinterpretation, clearer than `as`.
- `midpoint` for signed integers (1.87): overflow-free binary search midpoints.
- `u{n}::checked/overflowing/saturating/wrapping_sub_signed` (1.90), and `checked_signed_diff` (1.91).
- `carrying_add`, `borrowing_sub`, `carrying_mul`, `carrying_mul_add` (1.91): bignums, checksums, 128-bit math. These may replace small bignum helpers.
- `NonZero::div_ceil` (1.92): page counts as in `len.div_ceil(PAGE)`, with no divide-by-zero possible.
- `unchecked_shl` / `unchecked_shr` / `unchecked_neg` (1.93): `unsafe`, only for proven-hot paths.
- `bool: TryFrom<{integer}>` (1.95): **strict decoding of on-disk bool bytes**, where anything other than 0 or 1 is an error.
- `bool::ok_or` / `ok_or_else` (1.98): `cond.ok_or(Error::X)?`.
- `NonZero::from_str_radix` (1.98)
- `#[repr(u128)]` / `#[repr(i128)]` enums (1.89)

---

## 6. Concurrency and synchronization

### `Atomic*::update` / `try_update` (1.95)
These replace `fetch_update` closure loops with a clearer API.
```rust
// monotonically advance a watermark / LSN
let prev = lsn.update(Ordering::AcqRel, Ordering::Acquire, |cur| cur.max(new));
```
- **Use for:** high-water marks, reference-count style logic, and flag transitions.

### `RwLockWriteGuard::downgrade` (1.92)
```rust
let mut w = state.write().unwrap();
w.commit(batch);
let r = RwLockWriteGuard::downgrade(w); // no window for another writer
```
- **Use for:** write-then-read sequences (commit then publish, rebuild then serve) without a gap.

### Other concurrency APIs
- `Once::wait` / `OnceLock::wait` (1.86): block until another thread finishes initialization.
- `LazyLock::get` / `get_mut` / `force_mut` and `LazyCell` equivalents (1.94): peek at a lazy value without forcing it.
- `From<T> for LazyLock` / `LazyCell` (1.96)
- `AtomicPtr::fetch_ptr_add/sub`, `fetch_byte_add/sub`, `fetch_or/and/xor` (1.91): tagged pointers and lock-free structures.
- `Atomic<T>::from_mut`, `from_mut_slice`, `get_mut_slice` (1.98): view a `&mut [u64]` as atomics, or the reverse, without unsafe.
- `cfg(target_has_atomic_primitive_alignment)` (1.97)
- `Cell::update` (1.88)
- **Dependency removal:** `once_cell` and `lazy_static` are fully replaceable by `OnceLock` and `LazyLock` (older APIs plus the 1.94 additions).

---

## 7. Files, I/O, OS

### File locking (1.89)
`File::lock`, `lock_shared`, `try_lock`, `try_lock_shared`, and `unlock`. `try_*` returns `Err(TryLockError::WouldBlock)` when the lock is contended.
- The lock is released when the handle is closed, including on process crash.
- **Unix:** advisory (`flock`-like). It only coordinates with processes that also lock, and its behaviour on NFS and other network filesystems is unreliable.
- **Windows:** `LockFileEx`, which blocks I/O from other handles on locked regions.
- **Solaris:** returns "unsupported" since 1.98, because the earlier implementation had wrong semantics.
- **Use for:** single-writer enforcement, lock files, and cross-process coordination. It replaces `fs2`, `fs4`, `fd-lock`, and hand-written `libc::flock`.
- **Avoid:** code that relies on byte-range locks or `fcntl` POSIX-lock semantics. These std APIs lock whole files.

### Other I/O APIs
- `fs::set_times` / `set_times_nofollow` (1.99): set file timestamps without the `filetime` crate.
- `io::pipe` (1.87): anonymous pipes, replacing `os_pipe`.
- `Path::file_prefix` (1.91), `PathBuf::add_extension` / `with_added_extension` (1.91)
- `OsStr::display` (1.87)
- `TcpStreamExt::quickack` on Linux (1.89)
- **1.90 behaviour change:** `UnixStream` sets `MSG_NOSIGNAL`. Writes to a closed peer return `EPIPE` instead of raising SIGPIPE.

---

## 8. Memory, pointers, unsafe code

Use these APIs **only to improve existing unsafe code**. Never introduce raw pointers into safe code.

- **`Box/Rc/Arc::new_zeroed`, `new_zeroed_slice`** (1.92): zeroed page and I/O buffers that come from the allocator already zeroed (`calloc`), not zeroed by a memset after allocation. Use them with `assume_init` for types where all-zero is valid.
- **`[MaybeUninit<T>]` helpers** (1.93): `write_copy_of_slice`, `write_clone_of_slice`, `assume_init_ref/mut/drop`. They replace manual pointer loops over uninitialized buffers.
- **`MaybeUninit<[T; N]>` ↔ `[MaybeUninit<T>; N]` conversions** (1.95)
- **`Box<MaybeUninit<T>>::write`** (1.87)
- **`Vec::into_raw_parts`, `String::into_raw_parts`** (1.93), returning `(*mut T, len, cap)`.
- **`Vec::into_parts` / `from_parts`** (1.99), returning `(NonNull<T>, len, cap)`. They replace `ManuallyDrop` + `as_mut_ptr` juggling.
- **`Box::into_non_null` / `from_non_null`** (1.99). The 1.99 docs now **advise against "unleaking"** (calling `Box::leak` and later reconstructing and freeing). Migrate such code to `into_non_null`/`from_non_null`.
- **`size_of_val_raw`, `align_of_val_raw`, `Layout::for_value_raw`** (1.99): get the layout of unsized pointees without manufacturing a reference.
- **`Layout::repeat`, `repeat_packed`, `extend_packed`, `dangling_ptr`** (1.95): custom arenas and DST layout computation.
- **`<*const/*mut T>::as_ref_unchecked`, `as_mut_unchecked`** (1.95)
- **`offset_from_unsigned` / `byte_offset_from_unsigned`** (1.87) on pointers and `NonNull`.
- **`NonNull::from_ref/from_mut`, `without_provenance`, `with_exposed_provenance`, `expose_provenance`** (1.89)
- **`UnsafeCell` guarantee** (1.99): its contents may be accessed without going through `get()`, for example via a pointer cast of the cell. This guarantee is now official.
- **1.98:** the `ManuallyDrop<Box<T>>` aliasing issue is documented as fixed and is now a stable guarantee.
- **1.93:** a global allocator may use `thread_local!` and `thread::current()`.
- **1.86:** debug builds insert null-pointer dereference checks.

**Lints to heed:** `dangerous_implicit_autorefs` (deny, 1.89), `dangling_pointers_from_locals` (1.91), `integer_to_ptr_transmutes` (1.91), `function_casts_as_integer` (1.93), `deref_nullptr` (deny, 1.93), and `raw_borrows_via_references` (allow-by-default, 1.99; worth enabling in unsafe-heavy crates).

**Every remaining `unsafe` block needs an accurate `// SAFETY:` comment.** Run Miri on the in-memory parts.

---

## 9. SIMD, CPU features, codegen hints

### Safe `#[target_feature]` functions (1.86) and safe intrinsics (1.87)
`#[target_feature(enable = "avx2")]` may be put on **safe** functions. Inside such a function, most `std::arch` intrinsics that **do not take pointer arguments** are safe to call. Calling the annotated function from a context without that feature still needs `unsafe` (or runtime detection plus `unsafe`).
```rust
#[target_feature(enable = "avx2")]
fn popcount_block(v: __m256i) -> u32 { /* intrinsics without unsafe */ }
```
- **Use for:** shrinking `unsafe` scopes in existing SIMD code to the dispatch point only.
- **Avoid:** adding new SIMD paths without benchmarks.

### New target features and intrinsics
- AVX-512 family, `sha512`, `sm3`, `sm4`, `kl`/`widekl` (1.89); `sse4a`, `tbm` (1.91).
- `avx512fp16` and AArch64 NEON fp16 intrinsics, excluding those that need `f16` (1.94).
- Many RISC-V and LoongArch features across releases.

### Codegen hints
- `hint::select_unpredictable(cond, a, b)` (1.88): requests a branchless select. Useful in binary search over B-tree nodes or sorted arrays, where the branch is truly random. **Benchmark** before keeping it.
- `hint::cold_path()` (1.95): marks an unlikely branch, such as error, corruption, or slow-path handling. It is more ergonomic than `#[cold]` helper functions.

### Other low-level features
- `asm_goto` (1.87), `asm!` with `cfg` attributes (1.93), naked functions (1.88).
- `-Cjump-tables=bool` (1.93).
- Frame pointers default on aarch64 Linux (1.89). This improves profiling.

---

## 10. Floating point

### Algebraic operations (1.98)
`algebraic_add`, `algebraic_sub`, `algebraic_mul`, `algebraic_div`, `algebraic_rem` on `f32`/`f64`. These allow reassociation and similar fast-math optimizations, which enables vectorized reductions. Results can be **non-deterministic**, but never UB.
- **Use for:** dot products, distances, and scoring loops where small numeric drift is acceptable.
- **Avoid:** anything persisted, hashed, compared for equality, used as an index key, or required to be reproducible across runs or builds.

### Other float notes
- `{float}::NAN` is guaranteed to be a quiet NaN (1.88).
- Many float methods became `const` (1.90, `floor`/`ceil`/`round`…; 1.94, `mul_add`).
- `f32/f64::consts::EULER_GAMMA` and `GOLDEN_RATIO` (1.94).

---

## 11. Strings, formatting, parsing, diagnostics

- **`NumBuffer` + `{integer}::format_into`** (1.98; `NumBuffer` re-exported in `std` in 1.99): allocation-free integer-to-text conversion. Its performance is comparable to `itoa`.
  ```rust
  let mut buf = std::fmt::NumBuffer::new();
  let s: &str = n.format_into(&mut buf);
  ```
  Use it to remove `itoa` and `to_string()` from hot serialization paths. **Benchmark first.**
- **`str::substr_range` / `[T]::subslice_range`** (1.98): recover a `Range<usize>` from a subslice. Use it for parser error spans instead of pointer arithmetic.
- **`str::strip_circumfix` / `[T]::strip_circumfix`** (1.98): strip a matching prefix and suffix, for example quoted literals or `[ … ]`.
- **`Peekable::next_if_map` / `next_if_map_mut`** (1.94): lexers that peek, test, consume, and transform in one call.
- **`fmt::from_fn`** (1.93; `const` in 1.95): ad-hoc `Display`/`Debug` without a wrapper struct.
  ```rust
  write!(f, "{}", fmt::from_fn(|f| plan.render(f, indent)))
  ```
- **`str::floor_char_boundary` / `ceil_char_boundary`** (1.91): truncate UTF-8 strings safely, for example in logs or index key prefixes.
- **`String::extend_from_within`** (1.87)
- **`String::from_utf8_lossy_owned`, `FromUtf8Error::into_utf8_lossy`** (1.99): lossy conversion without an extra copy.
- **`String::from_utf16le/be` (+ `_lossy`)** (1.98)
- **`impl TryFrom<Vec<u8>> for String`** (1.87), and `str::from_utf8` as an inherent method (1.87).
- **`PanicHookInfo::payload_as_str`** (1.91). Panic messages include the thread ID (1.91).
- **`Result::flatten`** (1.89)
- `format_args!()` can be stored in a variable (1.89).

---

## 12. Traits and the type system

- **Trait-object upcasting** (1.86): `&dyn Sub` coerces to `&dyn Super` (also for `Box`/`Arc`). It removes `as_super()` bridge methods and wrapper types. Do not redesign hierarchies just to use it.
- **`use<..>` precise capturing in traits** (1.87): control what an RPITIT captures. This pairs with the edition 2024 capture rules.
- **`_` for inferred const generic arguments** (1.89), for example `as_chunks::<_>()` where `N` is inferable.
- **`Default for Pin<Box<T>>` / `Pin<Rc<T>>` / `Pin<Arc<T>>`** (1.91)
- **`#[diagnostic::do_not_recommend]`** (1.85): improves error messages for library traits.
- **`#[my_macro] mod foo;`** (1.99): attribute macros on outlined modules.
- **1.98:** a `derive(PartialOrd)` fast path when deriving `Ord`. **This breaks code whose `PartialOrd` and `Ord` impls disagree.** Audit hand-written ordering impls.

---

## 13. Testing

- `assert_matches!` / `debug_assert_matches!` (1.96). See section 2.
- `#[test]` in invalid positions is now an error (1.93).
- Doctests run when cross-compiling (Cargo 1.89), and can be ignored per target (`ignore-<target>`, 1.88).
- `#[bench]` is fully removed from stable (1.88). Use `criterion`/`divan` or similar instead.
- libtest: `--nocapture` is deprecated in favour of `--no-capture` (1.88).
- `CARGO_BIN_EXE_<name>` is available at runtime (1.94), which helps integration tests that spawn the binary.
- `env::set_var` is `unsafe` in edition 2024. Avoid process-global environment mutation in tests.

---

## 14. Cargo, build, and CI

- **`build.warnings`** (1.97): enforce a warning-free build **without busting caches**. It replaces `RUSTFLAGS="-Dwarnings"`.
  ```toml
  # .cargo/config.toml (or env CARGO_BUILD_WARNINGS=deny in CI)
  [build]
  warnings = "deny"
  ```
- **Config `include`** (1.94): share config fragments between dev, CI, bench, and platform builds. Only split files when that removes real duplication.
- **TOML 1.1** in manifests and config (1.94). Note that using it raises the *development* MSRV.
- **`cargo publish --workspace`** (1.90): publishes multiple packages in dependency order.
- **`build.build-dir`** (1.91): separates intermediate artifacts from final outputs.
- **`resolver.lockfile-path`** (1.97), for read-only source trees.
- **`--target host-tuple`** (1.91)
- **`target.'cfg(..)'.rustdocflags`** (1.96)
- **Automatic cache garbage collection** of `~/.cargo` (1.88).
- **`cargo clean --workspace`** (1.93)
- **`-m` shorthand for `--manifest-path`** (1.97)
- **New `debug` profile** (1.99), currently identical to `dev`. It prepares for a future faster `dev` default. Do not depend on profile differences yet.
- **Incremental compilation is disabled by default in CI** (1.99, detected via the `CI` env var). Remove manual `CARGO_INCREMENTAL=0` settings.
- **LLD is the default linker on `x86_64-unknown-linux-gnu`** (1.90), which gives faster links. Remove custom `lld`/`mold` configuration unless benchmarks show a benefit.
- **v0 symbol mangling is the default** (1.97). Old debuggers and profilers may fail to demangle, so update `perf`, `gdb`, and `samply`. Backtrace text formatting changes.
- **Linker output is shown by default** (1.97). Linker warnings that were previously hidden will appear.
- **Rustdoc `--remap-path-prefix`** and `--emit` (1.97); `--remap-path-scope` for rustc (1.95).
- **Unwind tables are generated with `panic=abort`** (1.92) for usable backtraces. Opt out with `-C force-unwind-tables=no`.
- **musl 1.2.5** (1.93) brings better DNS behaviour in static binaries.
- **Static PIE on all gnu and musl targets** (1.99).

---

## 15. Lints worth knowing (new since 1.85)

| Lint | Level | Notes |
|---|---|---|
| `mismatched_lifetime_syntaxes` | warn (1.89) | Elided vs named lifetime inconsistencies, often noisy after upgrades. Fix the real cases. |
| `dangerous_implicit_autorefs` | deny (1.89) | Implicit `&` through raw-pointer deref. |
| `dangling_pointers_from_locals` | warn (1.91) | |
| `integer_to_ptr_transmutes` | warn (1.91) | Use `with_exposed_provenance` / `without_provenance`. |
| `semicolon_in_expressions_from_macros` | deny (1.91); non-local variant (1.99) | Report the problem upstream if it comes from a dependency's macro. |
| `const_item_interior_mutations` | warn (1.93) | Catches mutation of `const` items with interior mutability (a bug: each use creates a copy). |
| `function_casts_as_integer` | warn (1.93) | |
| `deref_nullptr` | deny (1.93) | |
| `unused_visibilities` | warn (1.94) | |
| `dead_code_pub_in_binary` | allow (1.97) | **Worth enabling in binary/application crates** to find dead `pub` items. |
| `c_void_returns` | warn (1.98) | |
| `invalid_runtime_symbol_definitions` | deny (1.98) | Defining `memcmp`, `memset`, and similar symbols. |
| `raw_borrows_via_references` | allow (1.99) | Worth enabling in unsafe-heavy crates. |
| `unreachable_cfg_select_predicates` | warn via `unused` (1.99) | |
| never-type fallback lints | deny (1.92) | Preparation for stabilizing `!`. |

Run `cargo clippy` with the new toolchain and review new lints **by category**. Never apply `--fix` repository-wide without reviewing the changes.

---

## 16. Upgrade gotchas (compatibility notes that bite)

Check these when moving from 1.85 to current:

- **Use the latest patch release.** 1.96.1, 1.97.1, and 1.98.1 each fixed a miscompilation (MIR optimization, LLVM, and vtable generation respectively).
- **1.93:** std stopped using `Copy` specialization internally (it was unsound). Some APIs may now call `Clone::clone` instead of `memcpy`, which can cause **possible perf regressions** in hot paths over `Copy` types. Benchmark.
- **1.93:** `BTreeMap::append` no longer updates existing keys.
- **1.96:** an optimized `BTreeMap::append` **may panic for types with incorrect `Ord` impls**.
- **1.98:** `derive(PartialOrd)` fast path. **Inconsistent `PartialOrd`/`Ord` impls now behave differently.**
- **1.98:** `repr(transparent)` is stricter about "trivial" fields (`repr(C)` types, private-field types, and `#[non_exhaustive]` types no longer count as trivial).
- **1.98:** `transmute` size checking is corrected for some `repr` combinations.
- **1.98:** Windows thread-local destructors use Fiber Local Storage.
- **1.98:** `File::lock` is unsupported on Solaris.
- **1.97:** enum layout changed for enums without layout guarantees. **Code that assumes enum layout (persisted via transmute or byte casts) is wrong and may now break.** Persisted types must be `repr(C)`/`repr(Int)` or explicitly serialized.
- **1.97:** `pin!` no longer performs deref coercion.
- **1.96:** `#[repr(Int)]` enum layout fixed in edge cases involving uninhabited ZST fields.
- **1.95:** matching on a `#[non_exhaustive]` enum always reads the discriminant, so closures may capture more.
- **1.94:** closure capture behaviour around patterns changed. **`Drop` may run at a different point.**
- **1.94:** std macros come via the prelude. A glob-imported custom macro with a std name (for example `matches`) becomes ambiguous.
- **1.92:** `iter::Repeat::last`/`count` now panic instead of looping forever.
- **1.91:** pattern bindings are lowered in written order, and drop order follows the primary bindings.
- **1.90:** `x86_64-apple-darwin` is demoted to Tier 2.
- **1.90:** `UnixStream` uses `MSG_NOSIGNAL`.
- **1.99:** the behaviour of an exhausted `RangeInclusive` changed (see section 4).
- **1.99:** legacy integer modules (`std::i32::MAX`) are fully deprecated. Use `i32::MAX`.
- **1.99:** `no_mangle_generic_items` is a hard error.
- **1.99:** attributes in doctests that apply to nothing are an error.
- **Any version:** new deny-by-default lints may break `-Dwarnings` builds. Budget time for that.

---

## 17. Not yet stable / upcoming

- **1.100 (expected 2026-11-12):** the community expects the **allocator API** and the **never type `!`**. Verify against the actual release notes. **Do not build workarounds that these will make obsolete**; record them as follow-ups instead.
- **Cargo dependency cooldowns** (time-based resolution) are in progress. The registry `pubtime` field was stabilized in 1.94 as groundwork.
- **`f16` / `f128`** are still unstable. The `f16`-dependent intrinsics are excluded from the 1.94 stabilization.
- **`gen` blocks/generators:** the keyword is reserved in 2024, but the feature is not stable.
- **`build-std`** is not stable. JSON target specs were re-gated in 1.95.

---

## 18. Dependencies that may now be replaceable

| Crate | std replacement | Since | Caveat |
|---|---|---|---|
| `fs2`, `fs4`, `fd-lock` | `File::lock*` | 1.89 | Whole-file only, advisory on Unix, unsupported on Solaris |
| `cfg-if` | `cfg_select!` | 1.95 | Keep simple `#[cfg]` attributes as they are |
| `itoa` | `NumBuffer` / `format_into` | 1.98 | Benchmark first |
| `once_cell`, `lazy_static` | `OnceLock`, `LazyLock` (+1.94 getters) | 1.80 / 1.94 | — |
| `assert_matches` | `assert_matches!` | 1.96 | — |
| `filetime` | `fs::set_times` | 1.99 | Check precision and platform needs |
| `os_pipe` | `io::pipe` | 1.87 | — |
| `static_assertions` (partially) | `const { assert!(…) }` | 1.79 | — |
| `memoffset` | `core::mem::offset_of!` | 1.77 | Nested fields need a newer version |
| small bignum helpers | `carrying_add/mul` etc. | 1.91 | — |

Remove a crate only if std provides the **same semantics**, reduces complexity, does not regress performance, and does not hurt portability.

---

## 19. Decision rule

> **If the new Rust version does not make the code safer, simpler, faster, easier to maintain, or remove an unnecessary dependency, leave the existing code alone.**

When adopting a feature, record **where** it was used, **what** it replaced, and the **concrete benefit**. When a feature was considered and rejected, record a one-line reason.