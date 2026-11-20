# Blackboard

Blackboard is a `no_std`, allocation-free parameter bus for embedded tasks that share memory. It stores the latest value for each parameter and uses Embassy signals to wake subscribed tasks. Separate operating-system processes would need a shared-memory or transport layer.

The goal of the crate is to build a safe message passing system that facilitates application configuration at power-up, while allowing tasks to pass data from one thread to another.  Blackboard was intended to be the *glue* for sending data between isolated processes.

The proposed design and communication contract are documented in [RFC 0001](doc/rfc-0001-blackboard.md).

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

