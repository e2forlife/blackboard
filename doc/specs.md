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
