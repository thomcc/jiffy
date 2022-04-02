use crate::utils::CachePadded;
use std::{
    cell::{Cell, UnsafeCell},
    hint::spin_loop,
    marker::{PhantomData, PhantomPinned},
    mem::{drop, MaybeUninit},
    num::NonZeroUsize,
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    thread,
};

#[derive(Default)]
struct SpinWait {
    counter: usize,
}

impl SpinWait {
    fn try_yield_now(&mut self) -> bool {
        // spin_loop();
        false
    }
    fn yield_now(&mut self) {
        self.counter += 1;
        if self.counter <= 3 {
            for _ in 0..(1 << self.counter) {
                spin_loop();
            }
        } else {
            std::thread::yield_now();
        }
    }
    #[cfg(any())]
    fn try_yield_now(&mut self) -> bool {
        if !Self::should_spin() {
            return false;
        }

        if self.counter >= 100 {
            return false;
        }

        self.counter += 1;
        spin_loop();
        true
    }
    #[cfg(any())]
    fn yield_now(&mut self) {
        if !Self::should_spin() {
            return;
        }

        self.counter = (self.counter + 1).min(5);
        for _ in 0..(1 << self.counter) {
            spin_loop();
        }
    }

    fn should_spin() -> bool {
        static NUM_CPUS: AtomicUsize = AtomicUsize::new(0);

        let num_cpus = NonZeroUsize::new(NUM_CPUS.load(Ordering::Relaxed)).unwrap_or_else(|| {
            let num_cpus = thread::available_parallelism()
                .ok()
                .or(NonZeroUsize::new(1))
                .unwrap();

            NUM_CPUS.store(num_cpus.get(), Ordering::Relaxed);
            num_cpus
        });

        num_cpus.get() > 1
    }
}

#[derive(Default)]
struct Event {
    thread: Cell<Option<thread::Thread>>,
    is_set: AtomicBool,
    _pinned: PhantomPinned,
}

impl Event {
    fn with<F>(f: impl FnOnce(Pin<&Self>) -> F) -> F {
        let event = Self::default();
        event.thread.set(Some(thread::current()));
        f(unsafe { Pin::new_unchecked(&event) })
    }

    fn wait(&self) {
        while !self.is_set.load(Ordering::Acquire) {
            thread::park();
        }
    }

    fn set(&self) {
        let is_set_ptr = NonNull::from(&self.is_set).as_ptr();
        let thread = self.thread.take();
        drop(self);

        unsafe { (*is_set_ptr).store(true, Ordering::Release) };
        let thread = thread.expect("Event without a thread");
        thread.unpark()
    }
}

#[derive(Default)]
struct Parker {
    event: AtomicPtr<Event>,
}

impl Parker {
    #[inline]
    fn park(&self) {
        let mut ev = self.event.load(Ordering::Acquire);
        let notified = NonNull::dangling().as_ptr();

        // let mut spin = SpinWait::default();
        // while !ptr::eq(ev, notified) && spin.try_yield_now() {
        //     ev = self.event.load(Ordering::Acquire);
        // }

        if !ptr::eq(ev, notified) {
            ev = self.park_slow();
        }

        assert!(ptr::eq(ev, notified));
        self.event.store(ptr::null_mut(), Ordering::Relaxed);
    }

    #[cold]
    fn park_slow(&self) -> *mut Event {
        Event::with(|event| {
            let event_ptr = NonNull::from(&*event).as_ptr();
            let notified = NonNull::dangling().as_ptr();
            assert!(!ptr::eq(event_ptr, notified));

            match self.event.compare_exchange(
                ptr::null_mut(),
                event_ptr,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Err(ev) => ev,
                Ok(_) => {
                    event.wait();
                    self.event.load(Ordering::Acquire)
                }
            }
        })
    }

    unsafe fn unpark(&self) {
        let event_ptr = NonNull::from(&self.event).as_ptr();
        let notified = NonNull::dangling().as_ptr();
        drop(self);

        let ev = (*event_ptr).swap(notified, Ordering::AcqRel);
        assert!(!ptr::eq(ev, notified), "multiple threads unparked Parker");

        if !ev.is_null() {
            (*ev).set();
        }
    }
}

#[derive(Default)]
struct Waiter {
    next: Cell<Option<NonNull<Self>>>,
    parker: Parker,
    _pinned: PhantomPinned,
}

#[derive(Default)]
struct WaitList {
    top: AtomicPtr<Waiter>,
}

impl WaitList {
    // #[cold]
    #[inline(never)]
    fn wait_while(&self, mut should_wait: impl FnMut() -> bool) {
        let waiter = Waiter::default();
        let waiter = unsafe { Pin::new_unchecked(&waiter) };

        let mut spin = SpinWait::default();
        let mut top = self.top.load(Ordering::Relaxed);

        while should_wait() {
            if top.is_null() && spin.try_yield_now() {
                top = self.top.load(Ordering::Relaxed);
                continue;
            }

            waiter.next.set(NonNull::new(top));
            if let Err(e) = self.top.compare_exchange_weak(
                top,
                &*waiter as *const Waiter as *mut Waiter,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                top = e;
                continue;
            }

            if !should_wait() {
                self.wake_all();
            }

            waiter.parker.park();
            top = self.top.load(Ordering::Relaxed);
        }
    }

    // #[cold]
    #[inline(never)]
    fn wake_all(&self) {
        let mut top = self.top.swap(ptr::null_mut(), Ordering::AcqRel);
        // while !top.is_null() {
        //     match self.top.compare_exchange_weak(
        //         top,
        //         ptr::null_mut(),
        //         Ordering::AcqRel,
        //         Ordering::Relaxed,
        //     ) {
        //         Ok(_) => break,
        //         Err(e) => top = e,
        //     }
        // }

        while !top.is_null() {
            unsafe {
                let parker = NonNull::from(&(*top).parker).as_ptr();
                let next = (*top).next.get().map(|ptr| ptr.as_ptr());

                (*parker).unpark();
                top = next.unwrap_or(ptr::null_mut());
            }
        }
    }
}

struct Value<T>(UnsafeCell<MaybeUninit<T>>);

impl<T> Value<T> {
    const INIT: Self = Self(UnsafeCell::new(MaybeUninit::uninit()));

    unsafe fn write(&self, value: T) {
        self.0.get().write(MaybeUninit::new(value))
    }

    unsafe fn read(&self) -> T {
        self.0.get().read().assume_init()
    }
}

const LAP: usize = 32;
const BLOCK_CAP: usize = LAP - 1;

struct Block<T> {
    values: [Value<T>; BLOCK_CAP],
    stored: [AtomicBool; BLOCK_CAP],
    next: AtomicPtr<Self>,
}

impl<T> Block<T> {
    const fn new() -> Self {
        const UNSTORED: AtomicBool = AtomicBool::new(false);
        Self {
            values: [Value::<T>::INIT; BLOCK_CAP],
            stored: [UNSTORED; BLOCK_CAP],
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

#[derive(Default)]
struct Producer<T> {
    position: AtomicUsize,
    block: AtomicPtr<Block<T>>,
    waiters: WaitList,
}

#[derive(Default)]
struct Consumer<T> {
    block: AtomicPtr<Block<T>>,
    index: Cell<usize>,
}

#[derive(Default)]
pub struct MpscQueue<T> {
    producer: CachePadded<Producer<T>>,
    consumer: CachePadded<Consumer<T>>,
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for MpscQueue<T> {}
unsafe impl<T: Send> Sync for MpscQueue<T> {}

impl<T> Drop for MpscQueue<T> {
    fn drop(&mut self) {
        unsafe {
            while let Some(value) = self.pop() {
                drop(value);
            }

            let block = self.consumer.block.load(Ordering::Relaxed);
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}

impl<T> MpscQueue<T> {
    pub fn push(&self, item: T) {
        let mut next_block = None;
        let mut spin = SpinWait::default();

        loop {
            let position = self.producer.position.load(Ordering::Acquire);
            let index = position % LAP;

            if index == BLOCK_CAP {
                let should_wait = || {
                    let current_pos = self.producer.position.load(Ordering::Relaxed);
                    // current_pos % LAP == BLOCK_CAP
                    current_pos == position
                };

                self.producer.waiters.wait_while(should_wait);
                continue;
            }

            if index + 1 == BLOCK_CAP && next_block.is_none() {
                next_block = Some(Box::new(Block::new()));
            }

            let mut block = self.producer.block.load(Ordering::Acquire);
            if block.is_null() {
                let new_block = Box::into_raw(Box::new(Block::new()));

                if let Err(_) = self.producer.block.compare_exchange(
                    ptr::null_mut(),
                    new_block,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    drop(unsafe { Box::from_raw(new_block) });
                    continue;
                }

                block = new_block;
                self.consumer.block.store(new_block, Ordering::Release);
            }

            let new_position = position.wrapping_add(1);
            if let Err(_) = self.producer.position.compare_exchange_weak(
                position,
                new_position,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                spin.yield_now();
                continue;
            }

            unsafe {
                if index + 1 == BLOCK_CAP {
                    let next_block = Box::into_raw(next_block.unwrap());
                    self.producer.block.store(next_block, Ordering::Release);
                    (*block).next.store(next_block, Ordering::Release);

                    let next_position = new_position.wrapping_add(1);
                    self.producer
                        .position
                        .store(next_position, Ordering::Release);
                    self.producer.waiters.wake_all();
                }

                let value = NonNull::from((*block).values.get_unchecked(index)).as_ptr();
                let stored = NonNull::from((*block).stored.get_unchecked(index)).as_ptr();

                (*value).write(item);
                (*stored).store(true, Ordering::Release);
                return;
            }
        }
    }

    pub unsafe fn pop(&self) -> Option<T> {
        let mut block = self.consumer.block.load(Ordering::Acquire);
        if block.is_null() {
            return None;
        }

        let mut index = self.consumer.index.get();
        if index == BLOCK_CAP {
            let next = (*block).next.load(Ordering::Acquire);
            if next.is_null() {
                return None;
            }

            drop(Box::from_raw(block));
            block = next;
            index = 0;

            self.consumer.index.set(index);
            self.consumer.block.store(block, Ordering::Relaxed);
        }

        let value = (*block).values.get_unchecked(index);
        let stored = (*block).stored.get_unchecked(index);

        if stored.load(Ordering::Acquire) {
            self.consumer.index.set(index + 1);
            return Some(value.read());
        }

        None
    }
}
