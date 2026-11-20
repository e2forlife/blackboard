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
    fn index(self) -> usize;
    fn from_index(idx: usize) -> Option<Self>;
    fn meta(self) -> &'static ParamMeta;
    fn all_meta() -> &'static [ParamMeta];
    fn from_ext_id(ext_id: u32) -> Option<Self>;
}

struct InnerState<const BUF_SIZE: usize, const WORDS: usize, const MAX_SUBS: usize> {
    buffer: [u8; BUF_SIZE],
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
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(core::cell::RefCell::new(InnerState {
                buffer: [0u8; BUF_SIZE],
                sub_dirty: [[0u32; WORDS]; MAX_SUBS],
                nvm_dirty: [0u32; WORDS],
            })),
            signals: [const { Signal::new() }; MAX_SUBS],
            nvm_signal: Signal::new(),
            _marker: PhantomData,
        }
    }

    /// Publish raw bytes, zero-filling the remainder of the slot.
    ///
    /// Returns true for accepted writes, including unchanged values. Only
    /// changes wake subscribers. Raw writes accept any encoding that fits.
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
        let mut mask = wake_mask;
        while mask != 0 {
            let sub_id = mask.trailing_zeros() as usize;
            if sub_id < MAX_SUBS {
                self.signals[sub_id].signal(());
            }
            mask &= mask - 1;
        }
        if wake_nvm {
            self.nvm_signal.signal(());
        }
        true
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
