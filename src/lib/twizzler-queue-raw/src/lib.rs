//! A raw queue interface for Twizzler, making no assumptions about where the underlying headers and
//! circular buffers are located. This means you probably don't want to use this --- instead, I
//! suggest you use the wrapped version of this library, twizzler-queue, since that actually
//! interacts with the object system.
//!
//! This library exists to provide an underlying implementation of the concurrent data structure for
//! each individual raw queue so that this complex code can be reused in both userspace and the kernel.
//!
//! The basic design of a raw queue is two parts:
//!
//!   1. A header, which contains things like head pointers, tail pointers, etc.
//!   2. A buffer, which contains the items that are enqueued.
//!
//! The queue is an MPSC lock-free blocking data structure. Any thread may submit to a queue, but
//! only one thread may receive on that queue at a time. The queue is implemented with a head
//! pointer, a tail pointer, a doorbell, and a waiters counter. Additionally, the queue is
//! maintained in terms of "turns", that indicate which "go around" of the queue we are on (mod 2).
//!
//! # Let's look at an insert
//! Here's what the queue looks like to start with. The 0_ indicates that it's empty, and turn is
//! set to 0.
//! ```
//!  b
//!  t
//!  h
//! [0_, 0_, 0_]
//! ```
//! When inserting, the thread first reserves space:
//! ```
//!  b
//!  t
//!      h
//! [0_, 0_, 0_]
//! ```
//! Then it fills out the data:
//! ```
//!  b
//!  t
//!      h
//! [0X, 0_, 0_]
//! ```
//! Then it toggles the turn bit:
//! ```
//!  b
//!  t
//!      h
//! [1X, 0_, 0_]
//! ```
//! Next, it bumps the doorbell (and maybe wakes up a waiting consumer):
//! ```
//!      b
//!  t
//!      h
//! [1X, 0_, 0_]
//! ```
//!
//! Now, let's say the consumer comes along and dequeues. First, it checks if it's empty by
//! comparing tail and bell, and finds it's not empty. Then it checks if it's the correct turn. This
//! turn is 1, so yes. Next, it remove the data from the queue:
//! ```
//!      b
//!  t
//!      h
//! [1_, 0_, 0_]
//! ```
//! And then finally it increments the tail counter:
//! ```
//!      b
//!      t
//!      h
//! [1_, 0_, 0_]
//! ```

#![cfg_attr(test, feature(termination_trait_lib))]
#![cfg_attr(test, feature(test))]
#![cfg_attr(not(any(feature = "std", test)), no_std)]

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};
#[derive(Clone, Copy, Default, Debug)]
#[repr(C)]
/// A queue entry. All queues must be formed of these, as the queue algorithm uses data inside this
/// struct as part of its operation. The cmd_slot is used internally to track turn, and the info is
/// used by the full queue structure to manage completion. The data T is user data passed around the queue.
pub struct QueueEntry<T> {
    cmd_slot: u32,
    info: u32,
    data: T,
}

impl<T> QueueEntry<T> {
    #[inline]
    fn get_cmd_slot(&self) -> u32 {
        unsafe { core::mem::transmute::<&u32, &AtomicU32>(&self.cmd_slot).load(Ordering::SeqCst) }
    }

    #[inline]
    fn set_cmd_slot(&self, v: u32) {
        unsafe {
            core::mem::transmute::<&u32, &AtomicU32>(&self.cmd_slot).store(v, Ordering::SeqCst);
        }
    }

    #[inline]
    /// Get the data item of a QueueEntry.
    pub fn item(self) -> T {
        self.data
    }

    #[inline]
    /// Get the info tag of a QueueEntry.
    pub fn info(&self) -> u32 {
        self.info
    }

    /// Construct a new QueueEntry. The `info` tag should be used to inform completion events in the
    /// full queue.
    pub fn new(info: u32, item: T) -> Self {
        Self {
            cmd_slot: 0,
            info,
            data: item,
        }
    }
}

#[repr(C)]
/// A raw queue header. This contains all the necessary counters and info to run the queue algorithm.
pub struct RawQueueHdr {
    l2len: usize,
    stride: usize,
    head: AtomicU32,
    waiters: AtomicU32,
    bell: AtomicU64,
    tail: AtomicU64,
}

impl RawQueueHdr {
    /// Construct a new raw queue header.
    pub fn new(l2len: usize, stride: usize) -> Self {
        Self {
            l2len,
            stride,
            head: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
            bell: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        1 << self.l2len
    }

    #[inline]
    fn is_full(&self, h: u32, t: u64) -> bool {
        (h & 0x7fffffff) as u64 - (t & 0x7fffffff) >= self.len() as u64
    }

    #[inline]
    fn is_empty(&self, bell: u64, tail: u64) -> bool {
        (bell & 0x7fffffff) == (tail & 0x7fffffff)
    }

    #[inline]
    fn is_turn<T>(&self, t: u64, item: *const QueueEntry<T>) -> bool {
        let turn = (t / (self.len() as u64)) % 2;
        let val = unsafe { &*item }.get_cmd_slot() >> 31;
        (val == 0) == (turn == 1)
    }

    #[inline]
    fn consumer_waiting(&self) -> bool {
        (self.tail.load(Ordering::SeqCst) & (1 << 31)) != 0
    }

    #[inline]
    fn submitter_waiting(&self) -> bool {
        self.waiters.load(Ordering::SeqCst) > 0
    }

    #[inline]
    fn consumer_set_waiting(&self, waiting: bool) {
        if waiting {
            self.tail.fetch_or(1 << 31, Ordering::SeqCst);
        } else {
            self.tail.fetch_and(!(1 << 31), Ordering::SeqCst);
        }
    }

    #[inline]
    fn inc_submit_waiting(&self) {
        self.waiters.fetch_add(1, Ordering::SeqCst);
    }

    #[inline]
    fn dec_submit_waiting(&self) {
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }

    #[inline]
    fn reserve_slot<W: Fn(&AtomicU64, u64)>(
        &self,
        flags: SubmissionFlags,
        wait: W,
    ) -> Result<u32, SubmissionError> {
        let h = self.head.fetch_add(1, Ordering::SeqCst);
        let mut waiter = false;
        let mut attempts = 1000;
        loop {
            let t = self.tail.load(Ordering::SeqCst);
            if !self.is_full(h, t) {
                break;
            }

            if flags.contains(SubmissionFlags::NON_BLOCK) {
                return Err(SubmissionError::WouldBlock);
            }

            if attempts != 0 {
                attempts -= 1;
                core::hint::spin_loop();
                continue;
            }

            if !waiter {
                waiter = true;
                self.inc_submit_waiting();
            }

            let t = self.tail.load(Ordering::SeqCst);
            if self.is_full(h, t) {
                wait(&self.tail, t);
            }
        }

        if waiter {
            self.dec_submit_waiting();
        }

        Ok(h & 0x7fffffff)
    }

    #[inline]
    fn get_turn(&self, h: u32) -> bool {
        (h / self.len() as u32) % 2 == 0
    }

    #[inline]
    fn ring<R: Fn(&AtomicU64)>(&self, ring: R) {
        self.bell.fetch_add(1, Ordering::SeqCst);
        if self.consumer_waiting() {
            ring(&self.bell)
        }
    }

    #[inline]
    fn get_next_ready<W: Fn(&AtomicU64, u64), T>(
        &self,
        wait: W,
        flags: ReceiveFlags,
        raw_buf: *const QueueEntry<T>,
    ) -> Result<u64, ReceiveError> {
        let mut attempts = 1000;
        let t = self.tail.load(Ordering::SeqCst) & 0x7fffffff;
        loop {
            let b = self.bell.load(Ordering::SeqCst);
            let item = unsafe { raw_buf.add((t as usize) & (self.len() - 1)) };

            if !self.is_empty(b, t) && self.is_turn(t, item) {
                break;
            }

            if flags.contains(ReceiveFlags::NON_BLOCK) {
                return Err(ReceiveError::WouldBlock);
            }

            if attempts != 0 {
                attempts -= 1;
                core::hint::spin_loop();
                continue;
            }

            self.consumer_set_waiting(true);
            let b = self.bell.load(Ordering::SeqCst);
            if self.is_empty(b, t) || !self.is_turn(t, item) {
                wait(&self.bell, b);
            }
        }

        if attempts == 0 {
            self.consumer_set_waiting(false);
        }
        Ok(t)
    }

    #[inline]
    fn advance_tail<R: Fn(&AtomicU64)>(&self, ring: R) {
        let t = self.tail.load(Ordering::SeqCst);
        self.tail.store((t + 1) & 0x7fffffff, Ordering::SeqCst);
        if self.submitter_waiting() {
            ring(&self.tail);
        }
    }
}

/// A raw queue, comprising of a header to track the algorithm and a buffer to hold queue entries.
pub struct RawQueue<'a, T> {
    hdr: &'a RawQueueHdr,
    buf: UnsafeCell<*mut QueueEntry<T>>,
}

bitflags::bitflags! {
    /// Flags to control how queue submission works.
    pub struct SubmissionFlags: u32 {
        /// If the request would block, return Err([SubmissionError::WouldBlock]) instead.
        const NON_BLOCK = 1;
    }

    /// Flags to control how queue receive works.
    pub struct ReceiveFlags: u32 {
        /// If the request would block, return Err([ReceiveError::WouldBlock]) instead.
        const NON_BLOCK = 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// Possible errors for submitting to a queue.
pub enum SubmissionError {
    /// An unknown error.
    Unknown,
    /// The operation would have blocked, and non-blocking operation was specified.
    WouldBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// Possible errors for receiving from a queue.
pub enum ReceiveError {
    /// An unknown error.
    Unknown,
    /// The operation would have blocked, and non-blocking operation was specified.
    WouldBlock,
}

impl<'a, T: Copy> RawQueue<'a, T> {
    /// Construct a new raw queue out of a header reference and a buffer pointer.
    pub fn new(hdr: &'a RawQueueHdr, buf: *mut QueueEntry<T>) -> Self {
        Self {
            hdr,
            buf: UnsafeCell::new(buf),
        }
    }

    // This is a bit unsafe, but it's because we're managing concurrency ourselves.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    fn get_buf(&self, off: usize) -> &mut QueueEntry<T> {
        unsafe {
            (*self.buf.get())
                .add(off & (self.hdr.len() - 1))
                .as_mut()
                .unwrap()
        }
    }

    /// Submit a data item of type T, wrapped in a QueueEntry, to the queue. The two callbacks,
    /// wait, and ring, are for implementing a rudimentary condvar, wherein if the queue needs to
    /// block, we'll call wait(x, y), where we are supposed to wait until *x != y. Once we are done
    /// inserting, if we need to wake up a consumer, we will call ring, which should wake up anyone
    /// waiting on that word of memory.
    pub fn submit<W: Fn(&AtomicU64, u64), R: Fn(&AtomicU64)>(
        &self,
        item: QueueEntry<T>,
        wait: W,
        ring: R,
        flags: SubmissionFlags,
    ) -> Result<(), SubmissionError> {
        let h = self.hdr.reserve_slot(flags, wait)?;
        let buf_item = self.get_buf(h as usize);
        *buf_item = item;
        let turn = self.hdr.get_turn(h);
        buf_item.set_cmd_slot(h | if turn { 1u32 << 31 } else { 0 });

        self.hdr.ring(ring);
        Ok(())
    }

    /// Receive data from the queue, returning either that data or an error. The wait and ring
    /// callbacks work similar to [RawQueue::submit].
    pub fn receive<W: Fn(&AtomicU64, u64), R: Fn(&AtomicU64)>(
        &self,
        wait: W,
        ring: R,
        flags: ReceiveFlags,
    ) -> Result<QueueEntry<T>, ReceiveError> {
        let t = self
            .hdr
            .get_next_ready(wait, flags, unsafe { *self.buf.get() })?;
        let buf_item = self.get_buf(t as usize);
        let item = *buf_item;
        self.hdr.advance_tail(ring);
        Ok(item)
    }
}

unsafe impl<'a, T: Send> Send for RawQueue<'a, T> {}
unsafe impl<'a, T: Send> Sync for RawQueue<'a, T> {}

#[cfg(test)]
mod tests {
    #![allow(soft_unstable)]
    use std::process::Termination;
    use std::sync::atomic::AtomicU64;

    use syscalls::SyscallArgs;

    use crate::ReceiveError;
    use crate::SubmissionError;
    use crate::{QueueEntry, RawQueue, RawQueueHdr, ReceiveFlags, SubmissionFlags};

    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }

    fn wait(x: &AtomicU64, v: u64) {
        // println!("wait");
        unsafe {
            let _ = syscalls::syscall(
                syscalls::SYS_futex,
                &SyscallArgs::new(x as *const AtomicU64 as usize, 0, v as usize, 0, 0, 0),
            );
        }
        /*
        while x.load(Ordering::SeqCst) == v {
            core::hint::spin_loop();
        }
        */
    }

    fn wake(x: &AtomicU64) {
        //   println!("wake");
        unsafe {
            let _ = syscalls::syscall(
                syscalls::SYS_futex,
                &SyscallArgs::new(x as *const AtomicU64 as usize, 1, !0, 0, 0, 0),
            );
        }
    }

    #[test]
    fn it_transmits() {
        let qh = RawQueueHdr::new(4, std::mem::size_of::<QueueEntry<u32>>());
        let mut buffer = [QueueEntry::<i32>::default(); 1 << 4];
        let q = RawQueue::new(&qh, buffer.as_mut_ptr());

        for i in 0..100 {
            let res = q.submit(
                QueueEntry::new(i as u32, i * 10),
                wait,
                wake,
                SubmissionFlags::empty(),
            );
            assert_eq!(res, Ok(()));
            let res = q.receive(wait, wake, ReceiveFlags::empty());
            assert!(res.is_ok());
            assert_eq!(res.unwrap().info(), i as u32);
            assert_eq!(res.unwrap().item(), i * 10);
        }
    }

    #[test]
    fn it_fills() {
        let qh = RawQueueHdr::new(2, std::mem::size_of::<QueueEntry<u32>>());
        let mut buffer = [QueueEntry::<i32>::default(); 1 << 2];
        let q = RawQueue::new(&qh, buffer.as_mut_ptr());

        let res = q.submit(QueueEntry::new(1, 7), wait, wake, SubmissionFlags::empty());
        assert_eq!(res, Ok(()));
        let res = q.submit(QueueEntry::new(2, 7), wait, wake, SubmissionFlags::empty());
        assert_eq!(res, Ok(()));
        let res = q.submit(QueueEntry::new(3, 7), wait, wake, SubmissionFlags::empty());
        assert_eq!(res, Ok(()));
        let res = q.submit(QueueEntry::new(4, 7), wait, wake, SubmissionFlags::empty());
        assert_eq!(res, Ok(()));
        let res = q.submit(
            QueueEntry::new(1, 7),
            wait,
            wake,
            SubmissionFlags::NON_BLOCK,
        );
        assert_eq!(res, Err(SubmissionError::WouldBlock));
    }

    #[test]
    fn it_nonblock_receives() {
        let qh = RawQueueHdr::new(4, std::mem::size_of::<QueueEntry<u32>>());
        let mut buffer = [QueueEntry::<i32>::default(); 1 << 4];
        let q = RawQueue::new(&qh, buffer.as_mut_ptr());

        let res = q.submit(QueueEntry::new(1, 7), wait, wake, SubmissionFlags::empty());
        assert_eq!(res, Ok(()));
        let res = q.receive(wait, wake, ReceiveFlags::empty());
        assert!(res.is_ok());
        assert_eq!(res.unwrap().info(), 1);
        assert_eq!(res.unwrap().item(), 7);
        let res = q.receive(wait, wake, ReceiveFlags::NON_BLOCK);
        assert_eq!(res.unwrap_err(), ReceiveError::WouldBlock);
    }

    extern crate crossbeam;
    extern crate test;
    #[bench]
    fn two_threads(b: &mut test::Bencher) -> impl Termination {
        let qh = RawQueueHdr::new(4, std::mem::size_of::<QueueEntry<u32>>());
        let mut buffer = [QueueEntry::<i32>::default(); 1 << 4];
        let q = RawQueue::new(
            unsafe { std::mem::transmute::<&RawQueueHdr, &'static RawQueueHdr>(&qh) },
            buffer.as_mut_ptr(),
        );

        //let count = AtomicU64::new(0);
        let x = crossbeam::scope(|s| {
            s.spawn(|_| loop {
                let res = q.receive(wait, wake, ReceiveFlags::empty());
                assert!(res.is_ok());
                if res.unwrap().info() == 2 {
                    break;
                }
                //count.fetch_add(1, Ordering::SeqCst);
            });

            b.iter(|| {
                let res = q.submit(QueueEntry::new(1, 2), wait, wake, SubmissionFlags::empty());
                assert_eq!(res, Ok(()));
            });
            let res = q.submit(QueueEntry::new(2, 2), wait, wake, SubmissionFlags::empty());
            assert_eq!(res, Ok(()));
        });

        x.unwrap();
    }
}
