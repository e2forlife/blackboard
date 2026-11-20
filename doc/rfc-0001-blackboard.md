# RFC 0001: Blackboard configuration and message passing

- Status: Draft
- Date: 2026-11-20
- Target crate: `blackboard`
- Implementation baseline: repository version `0.1.0`
- Related documents: [README](../readme.md), [original design specification](specs.md)

## Summary

Blackboard provides safe, allocation-free communication between embedded tasks that share memory. It acts as the *glue* between otherwise independent application components: configuration loaders, hardware drivers, control tasks, user interfaces, diagnostics, and persistence services.

The same parameter catalog serves two purposes. At power-up, it supplies defaults and receives restored settings before consumers are notified. During normal operation, it retains the latest value of each parameter and notifies interested tasks when that value changes.

This RFC proposes a retained-state communication contract. A notification means "this parameter needs your attention"; consumers read its current value. Intermediate values may be coalesced. Commands and events that must each be delivered require a queue or another transport.

The core operates within one shared address space. Here, isolated components are tasks or threads with separate responsibilities that communicate through Blackboard's API. Communication between operating-system processes, processors, or devices requires an adapter. The crate does not itself create process isolation or provide an IPC transport.

## Motivation

Embedded applications commonly need to combine compiled defaults, saved settings, and runtime changes. Without a common mechanism, each subsystem develops its own shared variables, startup ordering, locks, callbacks, and storage integration. Producers become coupled to consumers, and consumers can observe partially restored configuration.

Blackboard centralizes parameter storage and change routing while leaving application policy with the application. A control task can use the same parameter whether its value came from a default, saved configuration, a user interface, or another task. Producers do not need references to their consumers, and consumers do not need to know how values were obtained.

For example, a motor controller starts with a default speed of 1500. Startup restores a saved speed of 2500 without waking the controller. After initialization completes, the controller receives a notification and reads 2500. Later, a user interface publishes 3000, and the controller receives another notification through the same API.

## Goals

- Provide deterministic configuration initialization: defaults, restore, application overrides, then notification of the resulting state.
- Allow multiple producers and independent subscribers to exchange retained parameter values safely across tasks or threads.
- Support `no_std` targets with statically bounded storage and no runtime heap allocation in the core.
- Provide typed access without exposing references into mutable shared storage.
- Keep notification storage bounded even when producers outpace consumers.
- Separate application parameter definitions from the reusable communication mechanism.
- Use stable external identifiers for persistence and protocol adapters while keeping internal access direct.
- Keep storage I/O and application work outside Blackboard's critical sections.

## Non-goals

The initial design does not provide an event log, delivery of every intermediate value, chronological ordering, subscriber acknowledgements, multi-parameter transactions, or durable storage by itself. It does not define a network protocol, automatic schema migration, authentication, or a scheduler.

It also does not validate application-specific rules such as a motor's permitted speed range. Applications must validate such rules before publishing or through a dedicated configuration service. Type correctness alone does not establish that a value is safe for a device.

## Design status

This RFC documents the existing foundation and proposes requirements for completing it. Acceptance of this document would establish the intended contract; it would not mean every proposed feature is already implemented.

| Area | Repository baseline | Proposal or remaining work |
| --- | --- | --- |
| Storage | Packed, fixed-size retained slots | Preserve this model |
| Typed values | `u8`, `i8`, `u16`, `i16`, `u32`, `i32`, `f32`, `bool`, fixed-size byte arrays | Consider additional scalar types and validated UTF-8 separately |
| Startup | Defaults, silent restore, `finish_startup()` | Preserve and document the ordering contract |
| Notifications | Static masks, up to 16 subscriber IDs, Embassy signals | Preserve coalescing; evaluate fair draining |
| Persistence | Internal dirty bits and a wake signal | Public claim/snapshot/retry interface and storage adapter |
| External access | External ID lookup and raw writes | Raw reads into caller-owned buffers and encoding validation |
| Source identity | Fixed `PublisherId`; `NvmRestore` skips dirty tracking | Optional echo suppression with explicit subscriber mapping |
| Access policy | No read-only enforcement | Consider owner/read-only policy in a later RFC |

## Parameter catalog and storage

The application owns a catalog implementing `ParamCatalog`. The preferred path generates this catalog from an application CSV during the build. The reusable crate must not contain application-specific parameter names or defaults.

Each parameter has the following metadata:

| Field | Meaning |
| --- | --- |
| Dense index | Internal slot identity in `0..COUNT`; also identifies its dirty bit |
| `ext_id: u32` | Stable external identity, encoded as `(category << 16) \| param_id` |
| `offset: usize` | Byte offset into the retained buffer |
| `max_len: usize` | Allocated slot width |
| `dtype: DataType` | Encoding and typed-access discriminator |
| `persistent: bool` | Whether normal runtime changes require eventual storage |
| `sub_mask: u16` | Static set of subscribers interested in the parameter |

The category occupies bits 16 through 23, the parameter ID occupies bits 0 through 15, and bits 24 through 31 are zero. Generated external lookup uses a sorted table and binary search. Dense indices and offsets are build-local details; adapters must not persist or transmit them as stable identities. Reordering CSV rows may change both while leaving external IDs unchanged.

The generator produces the key enum, metadata, packed defaults, external lookup, and buffer/bitset size constants. Defaults use little-endian scalar encoding and fixed-width, zero-padded byte slots. Boolean values use `0` and `1`. A byte slot does not carry a logical length or imply valid UTF-8; applications needing either must define an encoding or wait for a dedicated API.

The current CSV format requires this header:

```csv
name,category,param_id,dtype,max_len,persistent,sub_mask,default
MotorSpeed,1,1,U32,4,true,0x0001,1500
Enabled,1,2,Bool,1,true,0x0001,true
Label,1,3,Bytes,8,false,0x0002,hex:4D 6F 74 6F 72
```

The parser accepts comma-separated fields without quoting or embedded commas. Names must compile as Rust enum variants. Scalar defaults must be explicit and fit the declared type; scalar widths must match their encoding. Generated `F32` defaults must be finite. Byte defaults must fit their nonempty slot. The existing generator rejects duplicate names and external IDs, invalid numeric fields, and empty catalogs.

Catalog validation must also establish nonoverlapping, in-bounds slots, a consistent dense-index mapping, complete metadata, and valid default encodings. Generated catalogs supply the intended layout; manual catalogs currently carry these obligations themselves. The constructor checks overall buffer size, bitset word count, subscriber capacity, and default-buffer length, but does not validate every manual metadata entry. Stronger validation and clearer diagnostics are proposed release work.

Metadata and defaults are immutable static data. Whether they occupy Flash without a RAM copy depends on the target and linker configuration.

## Power-up lifecycle

Startup has one coordinator and follows this sequence:

1. Construct the board, usually as a `static` using `Blackboard::new()`. Generated defaults populate the retained buffer. A manual catalog with an empty `DEFAULT_BYTES` uses zeros.
2. Load saved configuration. Validate record identity, encoding, schema compatibility, and application constraints before applying it with `PublisherId::NvmRestore`.
3. Apply any application-specific startup overrides. The application defines precedence; a typical order is defaults, saved values, then boot-mode overrides.
4. Call `finish_startup()` once all accepted startup values are present.
5. Enable ordinary producers and consumers that require finalized configuration.

During startup, reads are available and may observe defaults or partially restored values. All accepted writes remain silent, regardless of source: they update storage without notifying subscribers or marking persistence dirty. The application must gate configuration-dependent activity until the coordinator finishes. Blackboard is not a barrier that automatically starts other tasks.

`finish_startup()` queues every parameter for each assigned subscriber, including zero and unchanged values. It exposes those notifications only after catalog queuing completes, then wakes the subscribers. Repeated calls do nothing. Defaults and restore do not cause an immediate write-back to NVM. Saving defaults on first boot, if desired, is an explicit application action.

Normal producers must not run concurrently with startup finalization. The method uses per-parameter critical sections rather than one transaction over the entire catalog. An early subscriber may wait for notifications, but must not assume direct reads already contain finalized configuration.

Unknown, corrupt, missing, or incompatible saved records must not overwrite valid defaults. The application decides whether to continue with defaults or stop startup when restore fails; Blackboard cannot determine whether operation is safe with incomplete configuration.

## Runtime operations

### Publish and read

Typed `publish` checks the requested value type against catalog metadata and encodes the value into bytes. The commit compares the full normalized slot representation with the retained bytes while holding the board lock. If unchanged, it returns success without setting new dirty bits or waking tasks. Otherwise, it copies the value and sets routing and persistence bits within the same lock acquisition.

The commit is atomic for one parameter relative to other board reads and writes. Concurrent writers are serialized; the last committed write becomes the retained value. There is no priority between producers. A sequence of reads across different parameters is not a consistent snapshot, and a read-modify-write sequence is not atomic. Applications requiring coupled updates must publish a single encoded aggregate or provide coordination outside the board.

Typed `read` copies and decodes one value under the lock. It returns an owned value, so callers cannot retain an alias into mutable storage. The current API returns `Option<T>` for reads and `bool` for writes: a type/encoding failure yields `None`, a rejected typed write yields `false`, and an accepted unchanged write yields `true`. Those return types do not report subscriber processing or storage durability.

Raw writes currently accept any byte sequence that fits, zero-fill the remainder, and bypass semantic encoding validation. Consequently, they can store an invalid boolean representation that a later typed read rejects. Adapters must validate input before using this API. A proposed checked raw interface would require exact scalar widths and valid boolean encodings while retaining explicit padding semantics for byte slots. Its names, errors, and compatibility strategy remain open.

Bytewise comparison defines a change, including for floating-point values. For example, positive and negative zero have different encodings. Runtime `f32` access does not inherit the generator's finite-default restriction.

### Subscribe and consume

Each subscriber ID has its own pending bitset and signal. An application assigns one waiting task to each ID and repeatedly calls `next_change(sub_id).await`, then reads and processes the returned key. Multiple tasks waiting on one ID are unsupported. Out-of-range IDs currently panic.

The pending bitset is the record of outstanding work; the signal is a wake hint. Waiting must check pending bits before sleeping and recheck after waking so a publish between the check and the wait remains observable. The current implementation uses a retained Embassy signal for this handoff.

Multiple writes to a pending parameter collapse into one pending bit. Different subscribers drain independently, and a subscriber that starts late can drain its pending startup state. A write racing with a consumer's read may cause that consumer to read a newer value than the write that first set the bit. A subsequent pending notification may therefore yield the same value again.

The guarantee is that a subscribed parameter remains pending until claimed, and a later changed write can make it pending again. It is not an exactly-once value-delivery guarantee. Scheduling and continued consumer polling are required for progress. The current scan chooses the lowest pending dense index; sustained changes to lower indices can delay higher ones. A rotating scan cursor is a possible fairness improvement.

### Source identity and feedback loops

The baseline defines `Internal`, `UserInterface`, `Diagnostics`, and `NvmRestore`. Only the persistence exemption of `NvmRestore` changes current routing behavior; the source is not retained or returned with notifications. Publishers may receive their own changes if subscribed.

Unchanged-write suppression prevents simple write-back of identical bytes from repeatedly waking tasks. It does not prevent feedback loops where components transform values. Optional source-based routing should use an explicit publisher-to-subscriber mapping rather than assuming numeric IDs are interchangeable. Its interaction with startup delivery and persistence requires a separate design decision. A publisher enum is routing metadata, not an authorization credential.

## Persistence integration

Changed runtime writes to persistent parameters set a separate NVM dirty bit, except for writes from `NvmRestore`. These bits are independent of subscriber delivery. The baseline tracks and signals changes internally but has no public persistence drain API and performs no storage I/O.

The proposed interface must let one persistence worker atomically claim a dirty key and copy its current bytes into caller-owned storage. The worker then releases the lock and writes a record identified by external ID. It must not borrow the board's buffer across I/O.

The required race and failure behavior is:

1. Claiming clears the selected dirty bit and captures the corresponding value within the same critical section.
2. A later publish sets that bit again, even while the older snapshot is being stored.
3. Successful completion of the older write must not clear a newer dirty bit.
4. A failed write requeues the key without rolling back newer RAM state. Retrying may store the latest value rather than the failed snapshot.
5. One worker serializes record writes so an older operation cannot finish after a newer operation and overwrite it.

The adapter owns debounce, retry backoff, wear management, versioning, record integrity, and power-loss recovery. Until storage confirms a record, a successful publish means only that RAM accepted the value. Multi-record atomic durability requires a storage-level transaction scheme and is outside the core contract.

## Concurrency, safety, and resource use

The baseline uses `CriticalSectionRawMutex` and Embassy signals. The firmware must provide a critical-section implementation appropriate to its execution model. Multicore and threaded targets require mutual exclusion across all participating cores or threads; masking interrupts on one core alone is insufficient for that case. The board must not be placed directly into inter-process shared memory and assumed to work as IPC.

The implementation uses byte copies and explicit decoding rather than casting packed bytes to aligned typed references. Safety depends on a correct synchronization backend, valid catalog metadata, and respecting the subscriber/startup contracts. Malformed manual catalogs can currently cause panics or logical slot aliasing even though the core does not expose an unsafe value-access API.

The lock protects slot mutation, dirty-bit changes, and subscriber claims. The implementation must not await, perform I/O, or invoke application callbacks while holding it. Wake signals are sent after releasing the board lock. Critical-section duration depends on slot width, subscriber count, and bitset scanning; this RFC makes no universal microsecond latency promise.

For `P` parameters, `B` retained bytes, `S` subscribers, and `W = ceil(P / 32)`, the main mutable payload is:

```text
B + 4 * W * S + 4 * W bytes
```

This excludes signals, mutex/state overhead, alignment, and any future persistence scratch buffer. For 2,048 parameters, each subscriber bitset uses 256 bytes and the NVM bitset uses another 256 bytes. Adding subscribers increases RAM even if their subscriptions are sparse.

Internal key lookup is direct. A publish compares/copies one slot and visits its subscriber mask. A subscriber claim may scan `W` words while holding the lock. Startup visits the whole catalog. Slot limits, fairness, and target measurements therefore matter when selecting the catalog size and latency budget.

## Application example

The [README's catalog-generation instructions](../readme.md#generate-an-application-parameter-catalog) describe the build setup. With the example catalog above generated into a `params` module, the runtime flow uses existing APIs:

```rust
use blackboard::{Blackboard, PublisherId};
use params::{PARAM_BUF_SIZE, PARAM_WORDS, ParamId};

static BOARD: Blackboard<ParamId, PARAM_BUF_SIZE, PARAM_WORDS, 2> =
    Blackboard::new();

// Invoke once from the startup coordinator, before ordinary producers run.
fn initialize(saved_speed: Option<u32>) {
    // The caller has already validated the saved record and speed range.
    if let Some(speed) = saved_speed {
        assert!(BOARD.publish(ParamId::MotorSpeed, speed, PublisherId::NvmRestore));
    }
    BOARD.finish_startup();
}

// Assign subscriber 0 exclusively to this task.
async fn control_task() {
    loop {
        let key = BOARD.next_change(0).await;
        match key {
            ParamId::MotorSpeed => {
                let speed = BOARD.read::<u32>(key).expect("valid catalog and encoding");
                // Apply speed to the controller.
                let _ = speed;
            }
            ParamId::Enabled => {
                let enabled = BOARD.read::<bool>(key).expect("valid catalog and encoding");
                // Apply enabled to the controller.
                let _ = enabled;
            }
            _ => {}
        }
    }
}
```

## Alternatives and tradeoffs

| Alternative | Benefit | Reason for the proposed approach |
| --- | --- | --- |
| Queue every message | Preserves events and ordering within the queue | Retained configuration needs bounded storage and the current value; queues remain appropriate for commands |
| Shared variables with individual locks | Simple for a few values | A catalog provides common initialization, discovery, persistence identity, and routing |
| Direct callbacks | Immediate delivery | Callbacks couple execution and can introduce reentrancy or slow producer paths |
| One global changed flag | Minimal notification state | Per-subscriber bitsets identify relevant pending keys without scanning every value |
| Heap-backed dynamic registry | Flexible runtime registration | Static catalogs make memory use predictable and support small `no_std` targets |

The main costs are bitset RAM per subscriber, a single shared lock, runtime type checks, and loss of intermediate values. The design also requires application discipline around startup and parameter ownership.

## Compatibility and rollout

Document the existing API contract first without changing runtime behavior. Applications using generated catalogs must include explicit CSV defaults and call `finish_startup()` before enabling normal producers. Existing manual catalogs can retain zero initialization, subject to valid encodings and layout.

Complete catalog validation and persistence draining as separate implementation changes. Any stricter raw-write behavior or replacement of `bool`/`Option` with structured errors must have an explicit compatibility plan. Do not present those APIs as available until implemented and tested.

External IDs must remain stable for a parameter's meaning. Changing its type, width, or encoding requires an adapter migration policy or a new external ID; preserving the ID alone does not make old stored bytes compatible.

## Validation and acceptance criteria

The core contract should be accepted against the following evidence:

- Generated defaults round-trip for every supported type; invalid definitions fail with useful diagnostics.
- Startup restore stays silent, retains defaults for missing values, sends the final subscribed state including zeros, and never dirties NVM. Repeated finalization is harmless.
- Typed mismatches reject writes without mutation; malformed raw encodings have documented outcomes.
- Independent subscribers receive their own pending keys; repeated writes coalesce and unchanged writes stay quiet.
- Publish/wait and publish/read interleavings preserve pending work and never expose a torn slot, using a supported synchronization backend.
- Proposed persistence claims preserve a concurrent update, and failed I/O requeues work without clearing newer state.
- Target builds demonstrate `no_std` use and feature separation from host-only `build-tools`; representative firmware measures RAM and worst observed critical-section duration.

Existing tests cover defaults, generation, startup, basic coalescing, typed mismatch, and internal NVM dirty tracking. Persistence failure/race tests and target concurrency/latency evidence remain necessary as those integrations are completed. A documentation change alone does not establish hardware timing or multicore correctness.

## Open questions

1. Should structured errors distinguish invalid catalog, type mismatch, invalid encoding, and insufficient buffer capacity while preserving the simple typed API?
2. Should manual catalogs be fully validated at construction, through a separate validation step, or through a more restrictive catalog interface?
3. Is a rotating subscriber scan required for fairness in the initial supported release?
4. What public claim/snapshot/retry API best supports bounded persistence buffers and application-selected storage backends?
5. Should publisher identity become application-defined, and should consumers receive origin metadata or use optional echo suppression?
6. Do applications require an explicit readiness query or wait API beyond coordinator-controlled task startup?
7. Which additional scalar types, string encodings, and access policies warrant separate extensions?

## Future work

Potential extensions include checked raw access, category iteration, bounded UTF-8 values, additional scalars, typed parameter handles, owner/read-only policies, and alternative wake backends. Process or device adapters can resolve external IDs and validate encodings at their boundaries while keeping this retained-state core small. Each extension must preserve explicit memory bounds and clearly state any additional delivery or durability guarantees.
