//! Issue #329: the event-source coalescing policies, through the public API.

use flow::event_source::{CoalescePolicy, Coalescer};

#[test]
fn burst_of_100_events_coalesces_to_latest() {
    let mut queue = Coalescer::new(CoalescePolicy::LatestOnly);
    for n in 0..100 {
        queue.push(n);
    }
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.pop(), Some(99));
}

#[test]
fn bounded_queue_drops_oldest_when_full() {
    let mut queue = Coalescer::new(CoalescePolicy::DropOldest { capacity: 2 });
    for n in 0..5 {
        queue.push(n);
    }
    assert_eq!(queue.dropped(), 3);
    assert_eq!(queue.pop(), Some(3));
    assert_eq!(queue.pop(), Some(4));
    assert_eq!(queue.pop(), None);
}
