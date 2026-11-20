#![no_std]
//! A retained parameter bus for tasks sharing memory on an embedded device.
//!
//! Use an application-defined [`ParamCatalog`] to map keys to storage slots.
//! The application must supply a critical-section implementation for its target.

#[cfg(feature = "build-tools")]
extern crate std;

#[cfg(feature = "build-tools")]
pub mod codegen;

use core::marker::PhantomData;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DataType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    Bool,
    Bytes, // Fixed-max-length UTF-8 string or raw blob
}

mod sealed {
    pub trait Sealed {}
}

/// A value that can be stored in a Blackboard slot.
///
/// Scalars use little-endian encoding. Byte arrays represent the entire slot.
pub trait BlackboardValue: sealed::Sealed + Sized {
    const DATA_TYPE: DataType;
    const WIDTH: usize;
    type Bytes: AsRef<[u8]>;

    fn encode(self) -> Self::Bytes;
    fn decode(bytes: &[u8]) -> Option<Self>;
}

macro_rules! impl_scalar {
    ($ty:ty, $dtype:ident) => {
        impl sealed::Sealed for $ty {}

        impl BlackboardValue for $ty {
            const DATA_TYPE: DataType = DataType::$dtype;
            const WIDTH: usize = core::mem::size_of::<Self>();
            type Bytes = [u8; Self::WIDTH];

            fn encode(self) -> Self::Bytes {
                self.to_le_bytes()
            }

            fn decode(bytes: &[u8]) -> Option<Self> {
                Some(Self::from_le_bytes(bytes.try_into().ok()?))
            }
        }
    };
}

impl_scalar!(u32, U32);
impl_scalar!(i32, I32);
impl_scalar!(f32, F32);
impl_scalar!(u8, U8);
impl_scalar!(i8, I8);
impl_scalar!(u16, U16);
impl_scalar!(i16, I16);

impl sealed::Sealed for bool {}

impl BlackboardValue for bool {
    const DATA_TYPE: DataType = DataType::Bool;
    const WIDTH: usize = 1;
    type Bytes = [u8; 1];

    fn encode(self) -> Self::Bytes {
        [u8::from(self)]
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }
}

impl<const N: usize> sealed::Sealed for [u8; N] {}

impl<const N: usize> BlackboardValue for [u8; N] {
    const DATA_TYPE: DataType = DataType::Bytes;
    const WIDTH: usize = N;
    type Bytes = Self;

    fn encode(self) -> Self::Bytes {
        self
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PublisherId {
    Internal = 0,
    UserInterface = 1,
    Diagnostics = 2,
    NvmRestore = 3,
}

/// Stored in Flash (.rodata) — 0 bytes of RAM per parameter
#[derive(Copy, Clone)]
pub struct ParamMeta {
    pub ext_id: u32,    // (category << 16) | param_id
    pub offset: usize,  // Byte offset in RAM buffer
    pub max_len: usize, // Exact byte size (e.g. 4 for u32/f32, 32 for strings)
    pub dtype: DataType,
    pub persistent: bool,
    pub sub_mask: u16, // Bitmask of up to 16 Embassy subscriber tasks
}

/// Implemented by the generated `ParamId` enum in each application
pub trait ParamCatalog: Copy + Sized + 'static {
    const COUNT: usize;
    const BUF_SIZE: usize;
    /// Packed, little-endian startup values, generated from the parameter CSV.
    /// An empty slice keeps manual catalogs zero-initialized.
    const DEFAULT_BYTES: &'static [u8] = &[];
    fn index(self) -> usize;
    fn from_index(idx: usize) -> Option<Self>;
    fn meta(self) -> &'static ParamMeta;
    fn all_meta() -> &'static [ParamMeta];
    fn from_ext_id(ext_id: u32) -> Option<Self>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupState {
    Restoring,
    Finishing,
    Complete,
}

struct InnerState<const BUF_SIZE: usize, const WORDS: usize, const MAX_SUBS: usize> {
    buffer: [u8; BUF_SIZE],
    startup: StartupState,
    sub_dirty: [[u32; WORDS]; MAX_SUBS],
    nvm_dirty: [u32; WORDS],
}

pub struct Blackboard<
    K: ParamCatalog,
    const BUF_SIZE: usize,
    const WORDS: usize,    // ceil(COUNT / 32)
    const MAX_SUBS: usize, // Number of subscriber tasks (<= 16)
> {
    inner:
        Mutex<CriticalSectionRawMutex, core::cell::RefCell<InnerState<BUF_SIZE, WORDS, MAX_SUBS>>>,
    signals: [Signal<CriticalSectionRawMutex, ()>; MAX_SUBS],
    nvm_signal: Signal<CriticalSectionRawMutex, ()>,
    _marker: PhantomData<K>,
}

impl<K: ParamCatalog, const BUF_SIZE: usize, const WORDS: usize, const MAX_SUBS: usize>
    Blackboard<K, BUF_SIZE, WORDS, MAX_SUBS>
{
    /// Load catalog defaults without notifying subscribers or dirtying NVM.
    ///
    /// Restore saved values with [`PublisherId::NvmRestore`], then call
    /// [`Self::finish_startup`] before starting normal producers. All writes
    /// remain silent until startup finishes, including ordinary publishes.
    pub const fn new() -> Self {
        assert!(BUF_SIZE == K::BUF_SIZE, "buffer size must match catalog");
        assert!(
            WORDS == K::COUNT.div_ceil(32),
            "word count must match catalog"
        );
        assert!(MAX_SUBS <= 16, "at most 16 subscribers are supported");
        assert!(
            K::DEFAULT_BYTES.is_empty() || K::DEFAULT_BYTES.len() == BUF_SIZE,
            "default bytes must fill the catalog buffer"
        );
        let mut buffer = [0u8; BUF_SIZE];
        let mut i = 0;
        while i < K::DEFAULT_BYTES.len() {
            buffer[i] = K::DEFAULT_BYTES[i];
            i += 1;
        }
        Self {
            inner: Mutex::new(core::cell::RefCell::new(InnerState {
                buffer,
                startup: StartupState::Restoring,
                sub_dirty: [[0u32; WORDS]; MAX_SUBS],
                nvm_dirty: [0u32; WORDS],
            })),
            signals: [const { Signal::new() }; MAX_SUBS],
            nvm_signal: Signal::new(),
            _marker: PhantomData,
        }
    }

    /// Publish the final startup state after all NVM restores have completed.
    ///
    /// Every parameter is queued once for each assigned subscriber, including
    /// zero or unchanged values. This does not mark persistent parameters dirty.
    /// Repeated calls do nothing. Call from the startup task before enabling
    /// normal producers; readers may inspect defaults before this call.
    pub fn finish_startup(&self) {
        let finishing = self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            if state.startup != StartupState::Restoring {
                return false;
            }
            state.startup = StartupState::Finishing;
            true
        });
        if !finishing {
            return;
        }
        let mut wake_mask = 0;
        for (idx, meta) in K::all_meta().iter().enumerate() {
            wake_mask |= meta.sub_mask;
            // Keep each critical section bounded to one parameter's routing.
            self.inner.lock(|cell| {
                let mut state = cell.borrow_mut();
                let mut mask = meta.sub_mask;
                while mask != 0 {
                    let sub_id = mask.trailing_zeros() as usize;
                    if sub_id < MAX_SUBS {
                        state.sub_dirty[sub_id][idx / 32] |= 1u32 << (idx % 32);
                    }
                    mask &= mask - 1;
                }
            });
        }
        self.inner.lock(|cell| {
            cell.borrow_mut().startup = StartupState::Complete;
        });
        self.signal_subscribers(wake_mask);
    }

    /// Publish raw bytes, zero-filling the remainder of the slot.
    ///
    /// Returns true for accepted writes, including unchanged values. Only
    /// changes wake subscribers after startup. Raw writes accept any encoding
    /// that fits. During startup, writes neither notify nor mark NVM dirty.
    pub fn publish_raw(&self, key: K, bytes: &[u8], source: PublisherId) -> bool {
        let m = key.meta();
        if bytes.len() > m.max_len {
            return false;
        }

        let (wake_mask, wake_nvm) = self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            let slot = &mut state.buffer[m.offset..m.offset + m.max_len];

            // Skip if unchanged (prevents unnecessary wakeups & Flash wear)
            if &slot[..bytes.len()] == bytes && slot[bytes.len()..].iter().all(|&b| b == 0) {
                return (0u16, false);
            }

            slot.fill(0);
            slot[..bytes.len()].copy_from_slice(bytes);

            if state.startup != StartupState::Complete {
                return (0u16, false);
            }

            let idx = key.index();
            let word = idx / 32;
            let bit = 1u32 << (idx % 32);

            let wake_nvm = m.persistent && source != PublisherId::NvmRestore;
            if wake_nvm {
                state.nvm_dirty[word] |= bit;
            }

            let mut mask = m.sub_mask;
            while mask != 0 {
                let sub_id = mask.trailing_zeros() as usize;
                if sub_id < MAX_SUBS {
                    state.sub_dirty[sub_id][word] |= bit;
                }
                mask &= mask - 1;
            }
            (m.sub_mask, wake_nvm)
        });

        // Signal tasks outside the critical section
        self.signal_subscribers(wake_mask);
        if wake_nvm {
            self.nvm_signal.signal(());
        }
        true
    }

    fn signal_subscribers(&self, mut mask: u16) {
        while mask != 0 {
            let sub_id = mask.trailing_zeros() as usize;
            if sub_id < MAX_SUBS {
                self.signals[sub_id].signal(());
            }
            mask &= mask - 1;
        }
    }

    /// Publish a typed value using the same change notifications as raw writes.
    /// Returns false if the value type or width does not fit the slot.
    pub fn publish<T: BlackboardValue>(&self, key: K, value: T, source: PublisherId) -> bool {
        Self::typed_offset::<T>(key).is_some()
            && self.publish_raw(key, value.encode().as_ref(), source)
    }

    /// Read a typed value, for example `board.read::<u32>(key)`.
    /// Returns None if the slot does not match the requested type or cannot be decoded.
    pub fn read<T: BlackboardValue>(&self, key: K) -> Option<T> {
        let offset = Self::typed_offset::<T>(key)?;
        self.inner.lock(|cell| {
            let state = cell.borrow();
            T::decode(&state.buffer[offset..offset + T::WIDTH])
        })
    }

    fn typed_offset<T: BlackboardValue>(key: K) -> Option<usize> {
        let meta = key.meta();
        if meta.dtype != T::DATA_TYPE
            || T::WIDTH > meta.max_len
            || (meta.dtype == DataType::Bytes && T::WIDTH != meta.max_len)
            || meta.offset.checked_add(meta.max_len)? > BUF_SIZE
        {
            return None;
        }
        Some(meta.offset)
    }

    pub fn publish_u32(&self, key: K, val: u32, src: PublisherId) -> bool {
        self.publish(key, val, src)
    }

    pub fn read_u32(&self, key: K) -> Option<u32> {
        self.read(key)
    }

    /// Wait for the next changed parameter assigned to `sub_id`.
    ///
    /// Use one waiting task per subscriber ID. Multiple updates to the same
    /// parameter can coalesce into one notification.
    ///
    /// # Panics
    ///
    /// Panics if `sub_id` is greater than or equal to `MAX_SUBS`.
    pub async fn next_change(&self, sub_id: usize) -> K {
        loop {
            if let Some(key) = self.pop_dirty(sub_id) {
                return key;
            }
            self.signals[sub_id].wait().await;
        }
    }

    fn pop_dirty(&self, sub_id: usize) -> Option<K> {
        self.inner.lock(|cell| {
            let mut state = cell.borrow_mut();
            // Finish queuing the full catalog before exposing startup changes.
            if state.startup != StartupState::Complete {
                return None;
            }
            for (w_idx, word) in state.sub_dirty[sub_id].iter_mut().enumerate() {
                if *word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    *word &= !(1u32 << bit);
                    return K::from_index(w_idx * 32 + bit);
                }
            }
            None
        })
    }
}

impl<K: ParamCatalog, const BUF_SIZE: usize, const WORDS: usize, const MAX_SUBS: usize> Default
    for Blackboard<K, BUF_SIZE, WORDS, MAX_SUBS>
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::future::Future;
    use core::task::{Context, Poll, Waker};

    #[derive(Clone, Copy)]
    struct Persistent;

    impl ParamCatalog for Persistent {
        const COUNT: usize = 1;
        const BUF_SIZE: usize = 4;
        const DEFAULT_BYTES: &'static [u8] = &1500u32.to_le_bytes();
        fn index(self) -> usize {
            0
        }
        fn from_index(idx: usize) -> Option<Self> {
            (idx == 0).then_some(Self)
        }
        fn meta(self) -> &'static ParamMeta {
            &Self::all_meta()[0]
        }
        fn all_meta() -> &'static [ParamMeta] {
            &[ParamMeta {
                ext_id: 1,
                offset: 0,
                max_len: 4,
                dtype: DataType::U32,
                persistent: true,
                sub_mask: 1,
            }]
        }
        fn from_ext_id(ext_id: u32) -> Option<Self> {
            (ext_id == 1).then_some(Self)
        }
    }

    #[test]
    fn defaults_and_startup_restores_never_dirty_or_signal_nvm() {
        let board = Blackboard::<Persistent, 4, 1, 1>::new();
        let mut cx = Context::from_waker(Waker::noop());
        let mut nvm = core::pin::pin!(board.nvm_signal.wait());
        assert_eq!(board.read::<u32>(Persistent), Some(1500));
        assert_eq!(nvm.as_mut().poll(&mut cx), Poll::Pending);
        assert!(board.publish(Persistent, 2000u32, PublisherId::NvmRestore));
        assert!(board.publish(Persistent, 3000u32, PublisherId::Internal));
        board.finish_startup();
        assert_eq!(nvm.as_mut().poll(&mut cx), Poll::Pending);
        board
            .inner
            .lock(|cell| assert_eq!(cell.borrow().nvm_dirty, [0]));

        // NVM restores remain exempt from write-back after startup as well.
        assert!(board.publish(Persistent, 4000u32, PublisherId::NvmRestore));
        board
            .inner
            .lock(|cell| assert_eq!(cell.borrow().nvm_dirty, [0]));
        assert_eq!(nvm.as_mut().poll(&mut cx), Poll::Pending);
        assert!(board.publish(Persistent, 5000u32, PublisherId::Internal));
        board
            .inner
            .lock(|cell| assert_eq!(cell.borrow().nvm_dirty, [1]));
        assert_eq!(nvm.as_mut().poll(&mut cx), Poll::Ready(()));
    }
}
