//! Event coalescing policies for typed event sources (issue #329).
//!
//! This is the pure building block of the event-source contract: a queue that
//! applies a coalescing policy to events a producer emits faster than a consumer
//! takes them. It starts no process and touches no bus or file.

use std::collections::VecDeque;

/// How a queue of pending events is bounded when producers outpace the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalescePolicy {
    /// Keep only the most recent pending event.
    LatestOnly,
    /// Keep at most `capacity` pending events; when full, the oldest is dropped.
    /// A capacity of zero behaves as one.
    DropOldest { capacity: usize },
}

/// A queue of pending events that applies a [`CoalescePolicy`] on every push.
#[derive(Debug, Clone)]
pub struct Coalescer<T> {
    policy: CoalescePolicy,
    queue: VecDeque<T>,
    dropped: usize,
}

impl<T> Coalescer<T> {
    /// Creates an empty queue governed by `policy`.
    pub fn new(policy: CoalescePolicy) -> Self {
        Self {
            policy,
            queue: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Appends `event`, dropping the oldest pending events the policy requires.
    /// Returns how many older events were dropped by this push.
    pub fn push(&mut self, event: T) -> usize {
        let limit = match self.policy {
            CoalescePolicy::LatestOnly => 1,
            CoalescePolicy::DropOldest { capacity } => capacity.max(1),
        };
        let mut dropped_now = 0;
        while self.queue.len() >= limit {
            self.queue.pop_front();
            dropped_now += 1;
        }
        self.queue.push_back(event);
        self.dropped += dropped_now;
        dropped_now
    }

    /// Removes and returns the oldest pending event, if any.
    pub fn pop(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    /// Number of pending events.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// True when no event is pending.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Total number of events dropped by the policy since creation.
    pub fn dropped(&self) -> usize {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::{CoalescePolicy, Coalescer};

    #[test]
    fn latest_only_burst_of_100_yields_one_event() {
        let mut queue = Coalescer::new(CoalescePolicy::LatestOnly);
        for n in 0..100 {
            queue.push(n);
        }
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.dropped(), 99);
        assert_eq!(queue.pop(), Some(99));
        assert!(queue.is_empty());
    }

    #[test]
    fn drop_oldest_keeps_newest_capacity_events() {
        let mut queue = Coalescer::new(CoalescePolicy::DropOldest { capacity: 3 });
        assert_eq!(queue.push(1), 0);
        assert_eq!(queue.push(2), 0);
        assert_eq!(queue.push(3), 0);
        assert_eq!(queue.push(4), 1);
        assert_eq!(queue.push(5), 1);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.dropped(), 2);
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), Some(4));
        assert_eq!(queue.pop(), Some(5));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn zero_capacity_behaves_as_one() {
        let mut queue = Coalescer::new(CoalescePolicy::DropOldest { capacity: 0 });
        queue.push("a");
        queue.push("b");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some("b"));
    }
}
