# Blackboard

Blackboard is a `no_std`, allocation-free parameter bus for embedded tasks that share memory. It stores the latest value for each parameter and uses Embassy signals to wake subscribed tasks. Separate operating-system processes would need a shared-memory or transport layer.

## Implemented in 0.1.0

- Typed access for `u8`, `i8`, `u16`, `i16`, `u32`, `i32`, `f32`, `bool`, and fixed-size byte arrays. Slots start at the defaults defined in the application CSV. Raw writes zero-fill the unused portion of a slot.
- Silent startup: load defaults, restore saved NVM values, then call `finish_startup()` to notify subscribers of the final state. Startup does not mark parameters dirty for NVM storage.
- Per-subscriber change notifications for up to 16 subscribers. Use one waiting task per subscriber ID. Multiple writes to the same parameter can coalesce. Unchanged writes return success without notifying subscribers.
- Application-owned parameter catalogs with dense keys and lookup by external ID.

The firmware must supply a critical-section implementation for its target.

## Release readiness

The original design specification below includes planned functionality. Persistence tracks changes internally and still needs a public drain API and storage integration. Raw reads, source-based echo suppression, read-only flags, and the additional scalar and UTF-8 APIs are also pending.

The generator expects the documented header and application-controlled rows. It validates default types, scalar widths, byte-slot capacity, numeric fields, duplicate names/external IDs, and empty catalogs. Names must be valid Rust enum variant identifiers. The CSV parser splits fields on commas; quoted fields and embedded commas are not supported. A manual `ParamCatalog` must provide valid offsets, slot widths, and dense indices. The board checks that its const sizes match the catalog.

Before release, select a license and declare it in `Cargo.toml`, confirm ownership or availability of the package name on crates.io, and add the public repository URL when available. Test the intended firmware target and executor. From a clean checkout, run `cargo test --locked --all-features`, `cargo package --list`, and `cargo publish --dry-run`. Run `cargo publish` when those checks pass and the release is ready.

## Generate an application parameter catalog

Add Blackboard as a runtime dependency and enable `build-tools` on the build dependency. Replace the example path with the path to your Blackboard checkout.

```toml
[dependencies]
blackboard = { path = "../blackboard" }

[build-dependencies]
blackboard = { path = "../blackboard", features = ["build-tools"] }
```

In the application's `build.rs`:

```rust
fn main() -> std::io::Result<()> {
    blackboard::codegen::generate_params("params.csv")
}
```

Put `params.csv` next to the application's `Cargo.toml`:

```csv
name,category,param_id,dtype,max_len,persistent,sub_mask,default
MotorSpeed,1,1,U32,4,false,0x0001,1500
Enabled,1,2,Bool,1,true,0x0001,true
Label,1,3,Bytes,8,false,0x0001,hex:4D 6F 74 6F 72
```

Include the catalog in the application, where it can define a board using the generated sizes:

```rust
mod params {
    include!(concat!(env!("OUT_DIR"), "/params_generated.rs"));
}

use params::{PARAM_BUF_SIZE, PARAM_WORDS, ParamId};

static BOARD: blackboard::Blackboard<ParamId, PARAM_BUF_SIZE, PARAM_WORDS, 1> =
    blackboard::Blackboard::new();
```

Cargo reruns the generator when the application CSV changes. For a workspace using edition 2024, use resolver 3 (resolver 2 also keeps build dependency features separate from runtime features).

The `default` column is required and is the source of startup values. Integer defaults use decimal notation, `F32` accepts finite decimal/scientific notation, and `Bool` accepts `true` or `false`. Scalar `max_len` must match the type's byte width. `Bytes` accepts `hex:` followed by whitespace-separated two-digit hex bytes, padded with zeros to `max_len`; an empty field supplies an all-zero byte slot. Empty scalar defaults are errors, so write `0`, `0.0`, or `false` explicitly where appropriate. Defaults are encoded at build time as packed little-endian bytes, with no runtime parsing or allocation.

### Startup and NVM restore

`Blackboard::new()` remains a const constructor suitable for `static` boards. It loads the generated defaults and starts in a silent startup phase. Reads work immediately. All writes during this phase, including `publish_raw`, update storage without notifying subscribers or marking NVM dirty.

Restore saved parameters after constructing the board, then finish startup:

```rust
use blackboard::PublisherId;

// In application startup, after reading saved settings from NVM:
if let Some(saved_enabled) = saved_enabled {
    assert!(BOARD.publish(ParamId::Enabled, saved_enabled, PublisherId::NvmRestore));
}

// Call once every saved parameter has been restored, or immediately if no NVM
// settings exist. Only then enable normal producer tasks.
BOARD.finish_startup();
```

Parameters missing from NVM retain their CSV defaults. `finish_startup()` queues every parameter for its assigned subscribers, including unchanged and zero values, so subscribers can initialize from the final state. Repeated calls do nothing. Defaults and startup restores never mark persistent parameters dirty; later changed publishes use the normal notification and persistence behavior. `NvmRestore` writes continue to skip NVM dirty tracking after startup.

Existing applications must add the `default` column to their CSV and call `finish_startup()` before enabling normal producers. Manually implemented catalogs may supply packed defaults through `ParamCatalog::DEFAULT_BYTES`; the trait's empty default retains all-zero initialization for those catalogs.

## Original design specification

### Overview & Goals

state_bus is a no_std, zero-heap, data-centric Blackboard communication library for embedded Rust applications. It decouples parameter producers (hardware drivers, control loops), consumers (subscribing tasks), persistence (NVM/Flash), and external interfaces (UI, diagnostics, fieldbus/telemetry) through a single shared state store.

#### Core Requirements

• Zero Heap (no_std): All RAM structures are statically sized at compile time.
• Heterogeneous Data Types: Supports primitives (u8–u64, i8–i64, f32, f64, bool) and bounded byte arrays/UTF-8 strings without enum padding bloat.
• Dual Addressing:
	• Internal O(1) Slot Index (usize / Enum): Used by firmware tasks for zero-overhead reads/writes.
	• External Sparse ID (u32 = [category: u8, param_id: u16]): Used for NVM storage keys, UI grouping/sorting, and external diagnostic protocols.
• Retained Latest State: Every parameter retains its latest value in RAM for immediate on-demand inspection.
• 1-to-Many Change Signaling: Writing a changed value notifies all subscribed tasks without dropping updates or overflowing queues.
• Echo Prevention: Tracks the PublisherId (origin) of each write so tasks do not trigger self-updates.

### System Architecture

```text
┌─────────────────────────────────────────────────────────────────────────┐
│                     APPLICATION CRATE (Project-Specific)                │
│                                                                         │
│  [build.rs / Macro] ──▶ Generates `ParamId` Enum + `PARAM_META` (Flash) │
│                                                                         │
│  ┌──────────────┐      ┌──────────────┐      ┌───────────────────────┐  │
│  │  Process A   │      │  Process B   │      │ Process C (UI / Diag) │  │
│  │ (Producer)   │      │ (Subscriber) │      │ (Poller & Controller) │  │
│  └──────┬───────┘      └──────▲───────┘      └───────────▲───────────┘  │
└─────────┼─────────────────────┼──────────────────────────┼──────────────┘
          │ publish()           │ next_dirty() / read()    │ read_raw() / publish_raw()
          ▼                     │                          ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                     REUSABLE CRATE: `state_bus`                         │
│                                                                         │
│  ┌─────────────────────┐  ┌─────────────────────┐  ┌─────────────────┐  │
│  │  Packed RAM Buffer  │  │ Subscriber Bitsets  │  │  NVM Dirty Map  │  │
│  │   [u8; BUF_SIZE]    │  │ [[u32; WORDS]; SUBS]│  │  [u32; WORDS]   │  │
│  └─────────────────────┘  └─────────────────────┘  └─────────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
```

### Data Model


#### Flash Metadata (ParamMeta)

Stored in .rodata (Flash) with 0 bytes of RAM overhead per parameter:

Field	Type	Description
ext_id	u32	Packed (category << 16) | param_id for NVM & external protocols
offset	u16	Byte offset into the packed RAM buffer
max_len	u16	Allocated byte width (1, 2, 4, 8, or N for strings/blobs)
dtype	DataType	Type tag (U32, I32, F32, Bool, Utf8, Bytes)
flags	ParamFlags	Bitflags: PERSISTENT, READ_ONLY, DIAGNOSTIC_VISIBLE
sub_mask	u16	Bitmask of static subscriber IDs (0..15) notified on change


#### RAM Storage (Blackboard)

+ buffer: `[u8; BUF_SIZE]`: Contiguous, tightly packed raw values (~8–16 KB for 2,000 mixed items).
+ sub_dirty: `[[u32; WORDS]; MAX_SUBS]`: One dirty bit per parameter per subscriber (WORDS = ceil(COUNT / 32)). For 2,048 parameters, each subscriber uses 256 bytes of RAM.
+ nvm_dirty: `[u32; WORDS]`: Dedicated dirty bitset tracking persistent parameters modified since the last NVM flush.
+ last_source: `[u8; COUNT]` *(Optional)*: Stores the PublisherId that last mutated each slot.

### Core Operations & Semantics


#### publish<T>(key: K, value: T, source: PublisherId)

1. Validate: Verify T matches key.meta().dtype (and value.len() <= max_len for strings/blobs). Reject writes to READ_ONLY keys unless source == PublisherId::InternalOwner.
2. Compare-Before-Write: Compare serialized bytes against buffer[offset..offset + max_len]. If identical, abort early (no dirty bits set, no tasks woken).
3. Commit: Copy new bytes into buffer inside a short critical section.
4. Route Notifications:
	+ If `flags.contains(PERSISTENT)` and `source != PublisherId::NvmRestore`, set bit `key.index()` in `nvm_dirty`.
	+ For every set bit sub_id in `key.meta().sub_mask` (excluding source if mapped to a subscriber), set bit `key.index()` in `sub_dirty[sub_id]`.
5. Signal Runtime: Outside the lock, trigger the platform wake primitive (e.g., `embassy_sync::signal::Signal`, `RTIC` software task pend, or RTOS event flag) for each notified `sub_id`.

#### Subscriber::next_change() -> Option<K>

1. Awaits/blocks on the subscriber's wake signal.
2. Enters a short critical section, scans sub_dirty[sub_id] using trailing_zeros(), clears the lowest set bit, and returns Some(K::from_index(idx)).
3. Loops until all bits in sub_dirty[sub_id] are cleared before sleeping again.
• Coalescing Guarantee: Rapid bursts of updates to the same parameter keep its bit set to 1; the subscriber wakes once and reads the latest value cleanly without queue overflow.

#### read<T>(key: K) -> Result<T, BusError>

• Enters a short critical section (or uses atomic load for aligned primitives), copies buffer[offset..offset + max_len], and deserializes T.

#### External / Diagnostic & NVM Interface

• Sparse ID Resolution: Uses a compile-time phf::Map<u32, u16> (or sorted Flash slice with binary search) to translate ext_id -> slot_index.
• Category Iteration: Each category exposes a sorted &'static [K] slice in Flash so UI/diagnostic tasks can stream or paginate parameters by group without scanning unrelated entries.
• NVM Sync Loop: Background task periodically claims set bits from nvm_dirty, reads (meta.ext_id, &buffer[offset..offset + max_len]), and writes them to Flash/EEPROM.

## Concurrency & Locking Rules

• Bounded Critical Sections: Locks are held only for memcpy of a single parameter and bitwise operations (< 2 µs), never across Flash writes, I/O, or user callbacks.
• No Lock Inversion: Task wakeups (Signal::signal) are always invoked after releasing the Blackboard mutex.

`finish_startup()` queues the catalog using per-parameter critical sections, then makes the queued startup notifications available and signals subscribers. Call it from a single startup task before enabling normal producers.
