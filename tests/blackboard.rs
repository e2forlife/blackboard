use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use blackboard::{Blackboard, DataType, ParamCatalog, ParamMeta, PublisherId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key;

static META: [ParamMeta; 1] = [ParamMeta {
    ext_id: 0x10001,
    offset: 0,
    max_len: 4,
    dtype: DataType::U32,
    persistent: false,
    sub_mask: 0b011,
}];

impl ParamCatalog for Key {
    const COUNT: usize = 1;
    const BUF_SIZE: usize = 4;

    fn index(self) -> usize {
        0
    }

    fn from_index(idx: usize) -> Option<Self> {
        (idx == 0).then_some(Self)
    }

    fn meta(self) -> &'static ParamMeta {
        &META[0]
    }

    fn all_meta() -> &'static [ParamMeta] {
        &META
    }

    fn from_ext_id(ext_id: u32) -> Option<Self> {
        (ext_id == META[0].ext_id).then_some(Self)
    }
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn changes_coalesce_per_subscriber_and_unchanged_values_stay_quiet() {
    let board = Blackboard::<Key, 4, 1, 3>::new();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    board.finish_startup();
    for subscriber in 0..2 {
        assert_eq!(
            std::pin::pin!(board.next_change(subscriber)).poll(&mut cx),
            Poll::Ready(Key)
        );
    }
    let mut first = std::pin::pin!(board.next_change(0));
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Pending);

    let source = PublisherId::Internal;
    assert_eq!(board.read::<u32>(Key), Some(0));
    assert!(!board.publish(Key, true, source));
    assert!(board.publish(Key, 1u32, source));
    assert!(board.publish(Key, 2u32, source));

    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Ready(Key));
    assert_eq!(board.read::<u32>(Key), Some(2));
    let mut second = std::pin::pin!(board.next_change(1));
    assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(Key));

    let mut first = std::pin::pin!(board.next_change(0));
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Pending);
    assert!(board.publish(Key, 2u32, source));
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(
        std::pin::pin!(board.next_change(1)).poll(&mut cx),
        Poll::Pending
    );
    assert_eq!(
        std::pin::pin!(board.next_change(2)).poll(&mut cx),
        Poll::Pending
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StartupKey(usize);

const STARTUP_COUNT: usize = 35;
const STARTUP_META: [ParamMeta; STARTUP_COUNT] = {
    let mut metadata = [ParamMeta {
        ext_id: 0,
        offset: 0,
        max_len: 4,
        dtype: DataType::U32,
        persistent: true,
        sub_mask: 0b011,
    }; STARTUP_COUNT];
    let mut i = 0;
    while i < STARTUP_COUNT {
        metadata[i].ext_id = 0x20000 + i as u32;
        metadata[i].offset = i * 4;
        i += 1;
    }
    metadata[0].sub_mask = 0b001;
    metadata[1].sub_mask = 0b010;
    metadata[34].sub_mask = 0;
    metadata
};

impl ParamCatalog for StartupKey {
    const COUNT: usize = STARTUP_COUNT;
    const BUF_SIZE: usize = STARTUP_COUNT * 4;
    const DEFAULT_BYTES: &'static [u8] = &{
        let mut bytes = [0; Self::BUF_SIZE];
        let mut i = 0;
        while i < STARTUP_COUNT {
            bytes[i * 4] = i as u8 + 1;
            i += 1;
        }
        bytes[32 * 4] = 0;
        bytes
    };

    fn index(self) -> usize {
        self.0
    }
    fn from_index(idx: usize) -> Option<Self> {
        (idx < Self::COUNT).then_some(Self(idx))
    }
    fn meta(self) -> &'static ParamMeta {
        &STARTUP_META[self.0]
    }
    fn all_meta() -> &'static [ParamMeta] {
        &STARTUP_META
    }
    fn from_ext_id(ext_id: u32) -> Option<Self> {
        Self::from_index(ext_id.checked_sub(0x20000)? as usize)
    }
}

#[test]
fn startup_is_silent_until_restore_finishes_then_delivers_final_values_once() {
    // Construction still works in a static/const context.
    static INITIAL: Blackboard<StartupKey, 140, 2, 3> = Blackboard::new();
    let board = &INITIAL;
    assert_eq!(board.read::<u32>(StartupKey(0)), Some(1));
    assert_eq!(board.read::<u32>(StartupKey(33)), Some(34));

    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut first = std::pin::pin!(board.next_change(0));
    let mut second = std::pin::pin!(board.next_change(1));
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(second.as_mut().poll(&mut cx), Poll::Pending);

    assert!(board.publish(StartupKey(0), 900u32, PublisherId::NvmRestore));
    assert!(board.publish(StartupKey(0), 1200u32, PublisherId::NvmRestore));
    assert!(board.publish_raw(StartupKey(1), &2u32.to_le_bytes(), PublisherId::NvmRestore));
    // All sources stay silent during initialization, including accidental publishes.
    assert!(board.publish(StartupKey(2), 42u32, PublisherId::Internal));
    assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(second.as_mut().poll(&mut cx), Poll::Pending);

    board.finish_startup();
    assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
    assert_eq!(first.as_mut().poll(&mut cx), Poll::Ready(StartupKey(0)));
    assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(StartupKey(1)));
    assert_eq!(board.read::<u32>(StartupKey(0)), Some(1200));
    assert_eq!(board.read::<u32>(StartupKey(1)), Some(2));
    assert_eq!(board.read::<u32>(StartupKey(2)), Some(42));
    assert_eq!(board.read::<u32>(StartupKey(32)), Some(0));

    for subscriber in 0..2 {
        for idx in 2..34 {
            assert_eq!(
                std::pin::pin!(board.next_change(subscriber)).poll(&mut cx),
                Poll::Ready(StartupKey(idx))
            );
        }
    }
    board.finish_startup();
    for subscriber in 0..3 {
        assert_eq!(
            std::pin::pin!(board.next_change(subscriber)).poll(&mut cx),
            Poll::Pending
        );
    }
    assert_eq!(board.read::<u32>(StartupKey(0)), Some(1200));
}
