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
