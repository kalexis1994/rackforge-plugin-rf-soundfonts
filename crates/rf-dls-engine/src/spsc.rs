//! A lock-free ring shared by exactly one writer and one reader.
//!
//! The audio thread runs at `SCHED_FIFO` and the disk thread does not. If the
//! two shared a mutex, the scheduler could stop the disk thread while it held
//! that mutex and then hand the CPU to the audio thread, which would block on
//! a lock its own priority prevents from ever being released. Priority
//! inversion of exactly that shape is the classic way a real-time audio path
//! dies, and no amount of buffering hides it.
//!
//! So the two threads share only atomics. Ordering does the rest: the writer
//! publishes data with a release store of its index, and the reader observes
//! it with an acquire load, which makes every byte written before that store
//! visible to the reader afterwards.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A single-producer single-consumer ring of `Copy` values.
///
/// One slot is always left empty so a full ring is distinguishable from an
/// empty one without a third counter that both threads would have to agree on.
pub struct SpscRing<T: Copy> {
    slots: Box<[UnsafeCell<T>]>,
    /// Next index the reader will take.
    read: AtomicUsize,
    /// Next index the writer will fill.
    write: AtomicUsize,
}

// SAFETY: the reader only ever touches slots in `read..write` and the writer
// only ever touches slots outside that span. The atomic indices are what
// separate the two, and they are published with release/acquire ordering, so
// no slot is ever accessed by both threads at once.
unsafe impl<T: Copy + Send> Send for SpscRing<T> {}
unsafe impl<T: Copy + Send> Sync for SpscRing<T> {}

impl<T: Copy> std::fmt::Debug for SpscRing<T> {
    /// Reports occupancy only. Printing the slots would read memory the
    /// producer may be writing, which is precisely what the index discipline
    /// exists to prevent.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpscRing")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl<T: Copy + Default> SpscRing<T> {
    /// Allocates a ring able to hold `capacity` values.
    ///
    /// Allocation happens here, once, away from the audio thread. Nothing in
    /// [`SpscRing::push`] or [`SpscRing::pop`] touches the allocator.
    pub fn new(capacity: usize) -> Self {
        let slots = (0..capacity + 1)
            .map(|_| UnsafeCell::new(T::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }
}

impl<T: Copy> SpscRing<T> {
    /// Values the ring can hold at once.
    pub fn capacity(&self) -> usize {
        self.slots.len() - 1
    }

    /// Values available to the reader right now.
    pub fn len(&self) -> usize {
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        Self::distance(read, write, self.slots.len())
    }

    /// Forward distance from `read` to `write` around a ring of `len` slots.
    ///
    /// The span is closed by adding `len` before the remainder rather than by
    /// subtracting and wrapping. A wrapping subtraction happens to work when
    /// the ring is a power of two and is silently wrong otherwise: with five
    /// slots it reports an empty ring that is in fact full, and the reader
    /// starves while the writer believes it delivered.
    fn distance(read: usize, write: usize, len: usize) -> usize {
        (write + len - read) % len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Room the writer has before the ring is full.
    pub fn vacancy(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Writes one value. Only the producer may call this.
    ///
    /// Returns `false` when the ring is full; the caller decides whether that
    /// is a dropped command or a stream that has run ahead of the disk.
    pub fn push(&self, value: T) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        let next = self.advance(write);
        if next == self.read.load(Ordering::Acquire) {
            return false;
        }
        // SAFETY: `write` is outside the reader's span, so this slot is not
        // observed by the reader until the release store below publishes it.
        unsafe { *self.slots[write].get() = value };
        self.write.store(next, Ordering::Release);
        true
    }

    /// Reads one value. Only the consumer may call this.
    pub fn pop(&self) -> Option<T> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the acquire load above proves the writer finished this slot.
        let value = unsafe { *self.slots[read].get() };
        self.read.store(self.advance(read), Ordering::Release);
        Some(value)
    }

    /// Writes as many values as fit, returning how many were taken.
    ///
    /// Bulk transfer matters on the disk side: refilling a buffer one value at
    /// a time would pay an atomic store per sample.
    pub fn push_slice(&self, values: &[T]) -> usize {
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let vacancy = self.capacity() - Self::distance(read, write, self.slots.len());
        let count = values.len().min(vacancy);
        let mut cursor = write;
        for value in &values[..count] {
            // SAFETY: every index walked here lies in the vacancy computed
            // from the reader's published position, so none is being read.
            unsafe { *self.slots[cursor].get() = *value };
            cursor = self.advance(cursor);
        }
        self.write.store(cursor, Ordering::Release);
        count
    }

    /// Reads into `out`, returning how many values were taken.
    pub fn pop_slice(&self, out: &mut [T]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let available = Self::distance(read, write, self.slots.len());
        let count = out.len().min(available);
        let mut cursor = read;
        for slot in &mut out[..count] {
            // SAFETY: bounded by the writer's published position, so every
            // slot read here was fully written before that store.
            *slot = unsafe { *self.slots[cursor].get() };
            cursor = self.advance(cursor);
        }
        self.read.store(cursor, Ordering::Release);
        count
    }

    /// Discards everything the reader has not taken. Consumer side only.
    pub fn clear(&self) {
        self.read
            .store(self.write.load(Ordering::Acquire), Ordering::Release);
    }

    fn advance(&self, index: usize) -> usize {
        let next = index + 1;
        if next == self.slots.len() { 0 } else { next }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn a_value_comes_back_out() {
        let ring = SpscRing::<u32>::new(4);
        assert!(ring.push(7));
        assert_eq!(ring.pop(), Some(7));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn a_full_ring_refuses_rather_than_overwriting() {
        let ring = SpscRing::<u32>::new(2);
        assert!(ring.push(1));
        assert!(ring.push(2));
        assert!(!ring.push(3), "an overwrite would corrupt unread audio");
        assert_eq!(ring.pop(), Some(1));
        assert!(ring.push(3));
    }

    #[test]
    fn capacity_and_occupancy_agree() {
        let ring = SpscRing::<u32>::new(8);
        assert_eq!(ring.capacity(), 8);
        assert_eq!(ring.vacancy(), 8);
        ring.push(1);
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.vacancy(), 7);
    }

    #[test]
    fn values_wrap_without_reordering() {
        let ring = SpscRing::<u32>::new(3);
        for round in 0..10 {
            assert!(ring.push(round));
            assert_eq!(ring.pop(), Some(round));
        }
    }

    #[test]
    fn bulk_transfer_preserves_order() {
        let ring = SpscRing::<f32>::new(8);
        assert_eq!(ring.push_slice(&[1.0, 2.0, 3.0]), 3);
        let mut out = [0.0; 3];
        assert_eq!(ring.pop_slice(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn bulk_transfer_takes_only_what_fits() {
        let ring = SpscRing::<f32>::new(4);
        assert_eq!(ring.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
        let mut out = [0.0; 8];
        assert_eq!(ring.pop_slice(&mut out), 4);
    }

    #[test]
    fn bulk_transfer_wraps_correctly() {
        let ring = SpscRing::<f32>::new(4);
        ring.push_slice(&[1.0, 2.0, 3.0]);
        let mut drain = [0.0; 2];
        ring.pop_slice(&mut drain);
        // Writer is now ahead of the reader and must wrap around the end.
        assert_eq!(ring.push_slice(&[4.0, 5.0, 6.0]), 3);
        let mut out = [0.0; 4];
        assert_eq!(ring.pop_slice(&mut out), 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn clearing_drops_only_unread_values() {
        let ring = SpscRing::<u32>::new(4);
        ring.push(1);
        ring.push(2);
        ring.clear();
        assert!(ring.is_empty());
        assert!(ring.push(3));
        assert_eq!(ring.pop(), Some(3));
    }

    #[test]
    fn a_producer_and_consumer_on_two_threads_lose_nothing() {
        // The property that matters: under real contention every value
        // arrives, exactly once, in order.
        const COUNT: u32 = 200_000;
        let ring = Arc::new(SpscRing::<u32>::new(64));
        let writer = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            let mut sent = 0;
            while sent < COUNT {
                if writer.push(sent) {
                    sent += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let mut expected = 0;
        while expected < COUNT {
            match ring.pop() {
                Some(value) => {
                    assert_eq!(value, expected, "a value arrived out of order");
                    expected += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
        producer.join().unwrap();
        assert!(ring.is_empty());
    }

    #[test]
    fn bulk_transfer_across_threads_loses_nothing() {
        const COUNT: usize = 100_000;
        let ring = Arc::new(SpscRing::<f32>::new(256));
        let writer = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            let mut sent = 0;
            while sent < COUNT {
                let block: Vec<f32> = (sent..(sent + 64).min(COUNT))
                    .map(|value| value as f32)
                    .collect();
                let taken = writer.push_slice(&block);
                sent += taken;
                if taken == 0 {
                    std::hint::spin_loop();
                }
            }
        });
        let mut received = 0;
        let mut out = [0.0_f32; 37];
        while received < COUNT {
            let taken = ring.pop_slice(&mut out);
            for (offset, value) in out[..taken].iter().enumerate() {
                assert_eq!(*value, (received + offset) as f32);
            }
            received += taken;
            if taken == 0 {
                std::hint::spin_loop();
            }
        }
        producer.join().unwrap();
    }
}
