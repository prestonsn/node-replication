// Copyright © VMware, Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use core::cell::RefCell;
use core::hash::{Hash, Hasher};
use core::mem::MaybeUninit;
use core::sync::atomic::{spin_loop_hint, AtomicUsize, Ordering};
use std::collections::hash_map::DefaultHasher;

use alloc::sync::Arc;
use alloc::vec::Vec;

use arr_macro::arr;
use crossbeam_utils::CachePadded;

use super::context::Context;
use super::log::Log;
use super::rwlock::RwLock;
use super::Dispatch;

/// A token handed out to threads registered with replicas.
///
/// # Note
/// Ideally this would be an affine type and returned again by
/// `execute` and `execute_ro`. However it feels like this would
/// hurt API ergonomics a lot.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ReplicaToken(usize);

/// To make it harder to use the same ReplicaToken on multiple threads.
impl !Send for ReplicaToken {}

impl ReplicaToken {
    /// Creates a new ReplicaToken
    ///
    /// # Safety
    /// This should only ever be used for the benchmark harness to create
    /// additional fake replica implementations.
    /// If we had a means to declare this not-pub we should do that instead.
    #[doc(hidden)]
    pub unsafe fn new(ident: usize) -> Self {
        ReplicaToken(ident)
    }
}

/// The maximum number of threads that can be registered with a replica. If more than
/// this number of threads try to register, the register() function will return None.
///
/// # Important
/// If this number is adjusted due to the use of the `arr_macro::arr` macro we
/// have to adjust the `256` literals in the `new` constructor of `Replica`.
pub const MAX_THREADS_PER_REPLICA: usize = 256;
const_assert!(
    MAX_THREADS_PER_REPLICA >= 1 && (MAX_THREADS_PER_REPLICA & (MAX_THREADS_PER_REPLICA - 1) == 0)
);

/// An instance of a replicated data structure. Uses a shared log to scale
/// operations on the data structure across cores and processors.
///
/// Takes in one type argument: `D` represents the underlying sequential data
/// structure `D` must implement the `Dispatch` trait.
///
/// A thread can be registered against the replica by calling `register()`. A
/// mutable operation can be issued by calling `execute_mut()` (immutable uses
/// `execute`). A mutable operation will be eventually executed against the replica
/// along with any operations that were received on other replicas that share
/// the same underlying log.
pub struct Replica<'a, D>
where
    D: Sized + Default + Dispatch + Sync,
{
    /// Idx that will be handed out to the next thread that registers with the replica.
    next: CachePadded<AtomicUsize>,

    /// Reference to the shared log that operations will be appended to and the
    /// data structure will be updated from.
    slog: Vec<Arc<Log<'a, <D as Dispatch>::WriteOperation>>>,

    /// The underlying replicated data structure. Shared between threads registered
    /// with this replica. Each replica maintains its own.
    // TODO(nr2): Don't need RwLock anymore
    data: CachePadded<RwLock<D>>,

    //
    // Per-"ReplicaFlatCombiner":
    //
    /// A replica-identifier received when the replica is registered against
    /// the shared-log. Required when consuming operations from the log.
    idx: Vec<usize>,

    /// Thread idx of the thread currently responsible for flat combining. Zero
    /// if there isn't any thread actively performing flat combining on the log.
    /// This also doubles up as the combiner lock.
    combiners: Vec<CachePadded<AtomicUsize>>,

    /// List of per-thread contexts. Threads buffer write operations in here when they
    /// cannot perform flat combining (because another thread might be doing so).
    ///
    /// The vector is initialized with `MAX_THREADS_PER_REPLICA` elements.
    contexts: Vec<Context<<D as Dispatch>::WriteOperation, <D as Dispatch>::Response>>,

    /// A buffer of operations for flat combining. The combiner stages operations in
    /// here and then batch appends them into the shared log. This helps amortize
    /// the cost of the compare_and_swap() on the tail of the log.
    buffer: Vec<RefCell<Vec<<D as Dispatch>::WriteOperation>>>,

    /// Number of operations collected by the combiner from each thread at any
    /// given point of time. Index `i` holds the number of operations collected from
    /// thread with identifier `i + 1`.
    inflight: RefCell<[usize; MAX_THREADS_PER_REPLICA]>,

    /// A buffer of results collected after flat combining. With the help of `inflight`,
    /// the combiner enqueues these results into the appropriate thread context.
    result: RefCell<Vec<<D as Dispatch>::Response>>,
}

/// The Replica is Sync. Member variables are protected by a CAS on `combiner`.
/// Contexts are thread-safe.
unsafe impl<'a, D> Sync for Replica<'a, D> where D: Sized + Default + Sync + Dispatch {}

impl<'a, D> core::fmt::Debug for Replica<'a, D>
where
    D: Sized + Default + Sync + Dispatch,
{
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "Replica")
    }
}

impl<'a, D> Replica<'a, D>
where
    D: Sized + Default + Dispatch + Sync,
{
    /// Constructs an instance of a replicated data structure.
    ///
    /// Takes a reference to the shared log as an argument. The Log is assumed to
    /// outlive the replica. The replica is bound to the log's lifetime.
    ///
    /// # Example
    ///
    /// ```
    /// use node_replication::Dispatch;
    /// use node_replication::Log;
    /// use node_replication::Replica;
    ///
    /// use std::sync::Arc;
    ///
    /// // The data structure we want replicated.
    /// #[derive(Default)]
    /// struct Data {
    ///     junk: u64,
    /// }
    ///
    /// // This trait allows the `Data` to be used with node-replication.
    /// impl Dispatch for Data {
    ///     type ReadOperation = ();
    ///     type WriteOperation = u64;
    ///     type Response = Option<u64>;
    ///
    ///     // A read returns the underlying u64.
    ///     fn dispatch(
    ///         &self,
    ///         _op: Self::ReadOperation,
    ///     ) -> Self::Response {
    ///         Some(self.junk)
    ///     }
    ///
    ///     // A write updates the underlying u64.
    ///     fn dispatch_mut(
    ///         &mut self,
    ///         op: Self::WriteOperation,
    ///     ) -> Self::Response {
    ///         self.junk = op;
    ///         None
    ///     }
    /// }
    ///
    /// // First create a shared log.
    /// let log = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
    ///
    /// // Create a replica that uses the above log.
    /// let replica = Replica::<Data>::new(vec![log]);
    /// ```
    pub fn new<'b>(
        logs: Vec<Arc<Log<'b, <D as Dispatch>::WriteOperation>>>,
    ) -> Arc<Replica<'b, D>> {
        let mut uninit_replica: Arc<MaybeUninit<Replica<D>>> = Arc::new_zeroed();

        // This is the preferred but unsafe mode of initialization as it avoids
        // putting the (often big) Replica object on the stack first.
        unsafe {
            let uninit_ptr = Arc::get_mut_unchecked(&mut uninit_replica).as_mut_ptr();
            let nlogs = logs.len();
            // Register the replica with all the logs.
            let mut idx = Vec::with_capacity(nlogs);
            for log in logs.iter() {
                idx.push(log.register().unwrap());
            }
            // Allocate a combiner for each log.
            let mut combiners = Vec::with_capacity(nlogs);
            for _i in 0..nlogs {
                combiners.push(CachePadded::new(AtomicUsize::new(0)))
            }

            uninit_ptr.write(Replica {
                idx,
                combiners,
                next: CachePadded::new(AtomicUsize::new(1)),
                contexts: Vec::with_capacity(MAX_THREADS_PER_REPLICA),
                buffer: vec![
                    RefCell::new(
                        Vec::with_capacity(
                            MAX_THREADS_PER_REPLICA
                                * Context::<
                                    <D as Dispatch>::WriteOperation,
                                    <D as Dispatch>::Response,
                                >::batch_size(),
                        ),
                    );
                    nlogs
                ],
                inflight: RefCell::new(arr![Default::default(); 256]),
                result:
                    RefCell::new(
                        Vec::with_capacity(
                            MAX_THREADS_PER_REPLICA
                                * Context::<
                                    <D as Dispatch>::WriteOperation,
                                    <D as Dispatch>::Response,
                                >::batch_size(),
                        ),
                    ),
                slog: logs.clone(),
                data: CachePadded::new(RwLock::<D>::default()),
            });

            let mut replica = uninit_replica.assume_init();
            // Add `MAX_THREADS_PER_REPLICA` contexts
            for _idx in 0..MAX_THREADS_PER_REPLICA {
                Arc::get_mut(&mut replica)
                    .unwrap()
                    .contexts
                    .push(Default::default());
            }

            replica
        }
    }

    /// Registers a thread with this replica. Returns an idx inside an Option if the registration
    /// was successfull. None if the registration failed.
    ///
    /// # Example
    ///
    /// ```
    /// use node_replication::Dispatch;
    /// use node_replication::Log;
    /// use node_replication::Replica;
    ///
    /// use std::sync::Arc;
    ///
    /// #[derive(Default)]
    /// struct Data {
    ///     junk: u64,
    /// }
    ///
    /// impl Dispatch for Data {
    ///     type ReadOperation = ();
    ///     type WriteOperation = u64;
    ///     type Response = Option<u64>;
    ///
    ///     fn dispatch(
    ///         &self,
    ///         _op: Self::ReadOperation,
    ///     ) -> Self::Response {
    ///         Some(self.junk)
    ///     }
    ///
    ///     fn dispatch_mut(
    ///         &mut self,
    ///         op: Self::WriteOperation,
    ///     ) -> Self::Response {
    ///         self.junk = op;
    ///         None
    ///     }
    /// }
    ///
    /// let log = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
    /// let replica = Replica::<Data>::new(vec![log]);
    ///
    /// // Calling register() returns an idx that can be used to execute
    /// // operations against the replica.
    /// let idx = replica.register().expect("Failed to register with replica.");
    /// ```
    pub fn register(&self) -> Option<ReplicaToken> {
        // Loop until we either run out of identifiers or we manage to increment `next`.
        loop {
            let idx = self.next.load(Ordering::SeqCst);

            if idx > MAX_THREADS_PER_REPLICA {
                return None;
            };

            if self.next.compare_and_swap(idx, idx + 1, Ordering::SeqCst) != idx {
                continue;
            };

            return Some(ReplicaToken(idx));
        }
    }

    /// Executes an mutable operation against this replica and returns a response.
    /// `idx` is an identifier for the thread performing the execute operation.
    ///
    /// # Example
    ///
    /// ```
    /// use node_replication::Dispatch;
    /// use node_replication::Log;
    /// use node_replication::Replica;
    ///
    /// use std::sync::Arc;
    ///
    /// #[derive(Default)]
    /// struct Data {
    ///     junk: u64,
    /// }
    ///
    /// impl Dispatch for Data {
    ///     type ReadOperation = ();
    ///     type WriteOperation = u64;
    ///     type Response = Option<u64>;
    ///
    ///     fn dispatch(
    ///         &self,
    ///         _op: Self::ReadOperation,
    ///     ) -> Self::Response {
    ///         Some(self.junk)
    ///     }
    ///
    ///     fn dispatch_mut(
    ///         &mut self,
    ///         op: Self::WriteOperation,
    ///     ) -> Self::Response {
    ///         self.junk = op;
    ///         None
    ///     }
    /// }
    ///
    /// let log = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
    /// let replica = Replica::<Data>::new(vec![log]);
    /// let idx = replica.register().expect("Failed to register with replica.");
    ///
    /// // execute_mut() can be used to write to the replicated data structure.
    /// let res = replica.execute_mut(100, idx);
    /// assert_eq!(None, res);
    pub fn execute_mut(
        &self,
        op: <D as Dispatch>::WriteOperation,
        idx: ReplicaToken,
    ) -> <D as Dispatch>::Response {
        let mut hasher = DefaultHasher::new();
        op.hash(&mut hasher);
        let hash = hasher.finish() as usize;

        // Enqueue the operation onto the thread local batch and then try to flat combine.
        while !self.make_pending(op.clone(), idx.0, hash) {}

        // A thread becomes combiner for operations with hash same as its own operation.
        self.try_combine(idx.0, hash);

        // Return the response to the caller function.
        self.get_response(idx.0, op)
    }

    /// Executes a read-only operation against this replica and returns a response.
    /// `idx` is an identifier for the thread performing the execute operation.
    ///
    /// # Example
    ///
    /// ```
    /// use node_replication::Dispatch;
    /// use node_replication::Log;
    /// use node_replication::Replica;
    ///
    /// use std::sync::Arc;
    ///
    /// #[derive(Default)]
    /// struct Data {
    ///     junk: u64,
    /// }
    ///
    /// impl Dispatch for Data {
    ///     type ReadOperation = ();
    ///     type WriteOperation = u64;
    ///     type Response = Option<u64>;
    ///
    ///     fn dispatch(
    ///         &self,
    ///         _op: Self::ReadOperation,
    ///     ) -> Self::Response {
    ///         Some(self.junk)
    ///     }
    ///
    ///     fn dispatch_mut(
    ///         &mut self,
    ///         op: Self::WriteOperation,
    ///     ) -> Self::Response {
    ///         self.junk = op;
    ///         None
    ///     }
    /// }
    ///
    /// let log = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
    /// let replica = Replica::<Data>::new(vec![log]);
    /// let idx = replica.register().expect("Failed to register with replica.");
    /// let _wr = replica.execute_mut(100, idx);
    ///
    /// // execute() can be used to read from the replicated data structure.
    /// let res = replica.execute((), idx);
    /// assert_eq!(Some(100), res);
    pub fn execute(
        &self,
        op: <D as Dispatch>::ReadOperation,
        idx: ReplicaToken,
    ) -> <D as Dispatch>::Response {
        self.read_only(op, idx.0)
    }

    /// Busy waits until a response is available within the thread's context.
    /// `idx` identifies this thread.
    fn get_response(
        &self,
        idx: usize,
        op: <D as Dispatch>::WriteOperation,
    ) -> <D as Dispatch>::Response {
        let mut iter = 0;
        let interval = 1 << 29;

        // Keep trying to retrieve a response from the thread context. After trying `interval`
        // times with no luck, try to perform flat combining to make some progress.
        loop {
            let r = self.contexts[idx - 1].res();
            if r.is_some() {
                return r.unwrap();
            }

            iter += 1;

            if iter == interval {
                let mut hasher = DefaultHasher::new();
                op.hash(&mut hasher);
                let hash = hasher.finish() as usize;
                self.try_combine(idx, hash);
                iter = 0;
            }
        }
    }

    /// Executes a passed in closure against the replica's underlying data
    /// structure. Useful for unit testing; can be used to verify certain
    /// properties of the data structure after issuing a bunch of operations
    /// against it.
    ///
    /// # Note
    /// There is probably no need for a regular client to ever call this function.
    /// TODO: find a way to pass hashidx here.
    #[doc(hidden)]
    pub fn verify<F: FnMut(&D)>(&self, mut v: F) {
        // Acquire the combiner lock before attempting anything on the data structure.
        // Use an idx greater than the maximum that can be allocated.
        while self.combiners[0].compare_and_swap(0, MAX_THREADS_PER_REPLICA + 2, Ordering::Acquire)
            != 0
        {}

        let data = self.data.write(self.next.load(Ordering::Relaxed));

        let mut f = |o: <D as Dispatch>::WriteOperation, _i: usize| {
            data.dispatch_mut(o);
        };

        self.slog[0].exec(self.idx[0], &mut f);

        v(&data);

        self.combiners[0].store(0, Ordering::Release);
    }

    /// This method is useful when a replica stops making progress and some threads
    /// on another replica are still active. The active replica will use all the entries
    /// in the log and won't be able perform garbage collection because of the inactive
    /// replica. So, this method syncs up the replica against the underlying log.
    /*pub fn sync(&self, idx: ReplicaToken) {
        let ctail = self.slog[hash_idx].get_ctail();
        while !self.slog[hash_idx].is_replica_synced_for_reads(self.idx, ctail) {
            self.try_combine(idx.0);
            spin_loop_hint();
        }
    }*/

    /// Issues a read-only operation against the replica and returns a response.
    /// Makes sure the replica is synced up against the log before doing so.
    fn read_only(
        &self,
        op: <D as Dispatch>::ReadOperation,
        tid: usize,
    ) -> <D as Dispatch>::Response {
        let mut hasher = DefaultHasher::new();
        op.hash(&mut hasher);
        let hash = hasher.finish() as usize;
        let hash_idx = hash % self.slog.len();

        // We can perform the read only if our replica is synced up against
        // the shared log. If it isn't, then try to combine until it is synced up.
        let ctail = self.slog[hash_idx].get_ctail();
        while !self.slog[hash_idx].is_replica_synced_for_reads(self.idx[hash_idx], ctail) {
            self.try_combine(tid, hash);
            spin_loop_hint();
        }

        self.data.read(tid - 1).dispatch(op)
    }

    /// Enqueues an operation inside a thread local context. Returns a boolean
    /// indicating whether the operation was enqueued (true) or not (false).
    #[inline(always)]
    fn make_pending(&self, op: <D as Dispatch>::WriteOperation, tid: usize, hash: usize) -> bool {
        self.contexts[tid - 1].enqueue(op, hash)
    }

    /// Appends an operation to the log and attempts to perform flat combining.
    /// Accepts a thread `tid` as an argument. Required to acquire the combiner lock.
    fn try_combine(&self, tid: usize, hash: usize) {
        let hashidx = hash % self.slog.len();
        // First, check if there already is a flat combiner. If there is no active flat combiner
        // then try to acquire the combiner lock. If there is, then just return.
        for _i in 0..4 {
            if unsafe {
                core::ptr::read_volatile(
                    &self.combiners[hashidx]
                        as *const crossbeam_utils::CachePadded<core::sync::atomic::AtomicUsize>
                        as *const usize,
                )
            } != 0
            {
                return;
            };
        }

        // Try to become the combiner here. If this fails, then simply return.
        if self.combiners[hashidx].compare_and_swap(0, tid, Ordering::Acquire) != 0 {
            return;
        }

        // Successfully became the combiner; perform one round of flat combining.
        self.combine(hash);

        // Allow other threads to perform flat combining once we have finished all our work.
        // At this point, we've dropped all mutable references to thread contexts and to
        // the staging buffer as well.
        self.combiners[hashidx].store(0, Ordering::Release);
    }

    /// Performs one round of flat combining. Collects, appends and executes operations.
    #[inline(always)]
    fn combine(&self, hash: usize) {
        let hashidx = hash % self.slog.len();

        //  TODO: may need to be in a per-log state context
        let mut buffer = self.buffer[hashidx].borrow_mut();
        let mut operations = self.inflight.borrow_mut();
        let mut results = self.result.borrow_mut();

        buffer.clear();
        results.clear();

        let next = self.next.load(Ordering::Relaxed);

        // Collect operations from each thread registered with this replica.
        for i in 1..next {
            // pass hash of current op to contexts, only get ops from context that have the same hash/log id
            operations[i - 1] = self.contexts[i - 1].ops(&mut buffer, hash);
        }

        // Append all collected operations into the shared log. We pass a closure
        // in here because operations on the log might need to be consumed for GC.
        {
            let f = |o: <D as Dispatch>::WriteOperation, i: usize| {
                let resp = self.data.write(next).dispatch_mut(o);
                if i == self.idx[hashidx] {
                    results.push(resp);
                }
            };
            self.slog[hashidx].append(&buffer, self.idx[hashidx], f);
        }

        // Execute any operations on the shared log against this replica.
        {
            let data = self.data.write(next);
            let mut f = |o: <D as Dispatch>::WriteOperation, i: usize| {
                let resp = data.dispatch_mut(o);
                if i == self.idx[hashidx] {
                    results.push(resp)
                };
            };
            self.slog[hashidx].exec(self.idx[hashidx], &mut f);
        }

        // Return/Enqueue responses back into the appropriate thread context(s).
        let (mut s, mut f) = (0, 0);
        // TODO: hashing makes this non-linear, need to take into account which operations
        // belong to our current combiner round...
        for i in 1..next {
            if operations[i - 1] == 0 {
                continue;
            };

            f += operations[i - 1];
            self.contexts[i - 1].enqueue_resps(&results[s..f]);
            s += operations[i - 1];
            operations[i - 1] = 0;
        }
    }
}

#[cfg(test)]
mod test {
    extern crate std;

    use super::*;
    use std::vec;

    // Really dumb data structure to test against the Replica and shared log.
    #[derive(Default)]
    struct Data {
        junk: AtomicUsize,
    }

    impl Dispatch for Data {
        type ReadOperation = usize;
        type WriteOperation = usize;
        type Response = Result<usize, ()>;

        fn dispatch(&self, _op: Self::ReadOperation) -> Self::Response {
            Ok(self.junk.load(Ordering::Relaxed))
        }

        fn dispatch_mut(&self, _op: Self::WriteOperation) -> Self::Response {
            self.junk.fetch_add(1, Ordering::Relaxed);
            return Ok(107);
        }
    }

    // Tests whether we can construct a Replica given a log.
    #[test]
    fn test_replica_create() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::new(1024));
        let repl = Replica::<Data>::new(vec![slog]);
        assert_eq!(repl.idx[0], 1);
        assert_eq!(repl.combiners[0].load(Ordering::SeqCst), 0);
        assert_eq!(repl.next.load(Ordering::SeqCst), 1);
        assert_eq!(repl.contexts.len(), MAX_THREADS_PER_REPLICA);
        assert_eq!(
            repl.buffer[0].borrow().capacity(),
            MAX_THREADS_PER_REPLICA * Context::<u64, Result<u64, ()>>::batch_size()
        );
        assert_eq!(repl.inflight.borrow().len(), MAX_THREADS_PER_REPLICA);
        assert_eq!(
            repl.result.borrow().capacity(),
            MAX_THREADS_PER_REPLICA * Context::<u64, Result<u64, ()>>::batch_size()
        );
        assert_eq!(repl.data.read(0).junk.load(Ordering::Relaxed), 0);
    }

    // Tests whether we can register with this replica and receive an idx.
    #[test]
    fn test_replica_register() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::new(1024));
        let repl = Replica::<Data>::new(vec![slog]);
        assert_eq!(repl.register(), Some(ReplicaToken(1)));
        assert_eq!(repl.next.load(Ordering::SeqCst), 2);
        repl.next.store(17, Ordering::SeqCst);
        assert_eq!(repl.register(), Some(ReplicaToken(17)));
        assert_eq!(repl.next.load(Ordering::SeqCst), 18);
    }

    // Tests whether registering more than the maximum limit of threads per replica is disallowed.
    #[test]
    fn test_replica_register_none() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::new(1024));
        let repl = Replica::<Data>::new(vec![slog]);
        repl.next
            .store(MAX_THREADS_PER_REPLICA + 1, Ordering::SeqCst);
        assert!(repl.register().is_none());
    }

    // Tests that we can successfully allow operations to go pending on this replica.
    #[test]
    fn test_replica_make_pending() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::new(1024));
        let repl = Replica::<Data>::new(vec![slog]);
        let mut o = vec![];

        assert!(repl.make_pending(121, 8, 0));
        assert_eq!(repl.contexts[7].ops(&mut o, 0), 1);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0], 121);
    }

    // Tests that we can't pend operations on a context that is already full of operations.
    #[test]
    fn test_replica_make_pending_false() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::new(1024));
        let repl = Replica::<Data>::new(vec![slog]);
        for _i in 0..Context::<u64, Result<u64, ()>>::batch_size() {
            assert!(repl.make_pending(121, 1, 0))
        }

        assert!(!repl.make_pending(11, 1, 0));
    }

    // Tests that we can append and execute operations using try_combine().
    #[test]
    fn test_replica_try_combine() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
        let repl = Replica::<Data>::new(vec![slog]);
        let _idx = repl.register();

        repl.make_pending(121, 1, 0);
        repl.try_combine(1, 0);

        assert_eq!(repl.combiners[0].load(Ordering::SeqCst), 0);
        assert_eq!(repl.data.read(0).junk.load(Ordering::Relaxed), 1);
        assert_eq!(repl.contexts[0].res(), Some(Ok(107)));
    }

    // Tests whether try_combine() also applies pending operations on other threads to the log.
    #[test]
    fn test_replica_try_combine_pending() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
        let repl = Replica::<Data>::new(vec![slog]);

        repl.next.store(9, Ordering::SeqCst);
        repl.make_pending(121, 8, 0);
        repl.try_combine(1, 0);

        assert_eq!(repl.data.read(0).junk.load(Ordering::Relaxed), 1);
        assert_eq!(repl.contexts[7].res(), Some(Ok(107)));
    }

    // Tests whether try_combine() fails if someone else is currently flat combining.
    #[test]
    fn test_replica_try_combine_fail() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::new(1024));
        let repl = Replica::<Data>::new(vec![slog]);

        repl.next.store(9, Ordering::SeqCst);
        repl.combiners[0].store(8, Ordering::SeqCst);
        repl.make_pending(121, 1, 0);
        repl.try_combine(1, 0);

        assert_eq!(repl.data.read(0).junk.load(Ordering::Relaxed), 0);
        assert_eq!(repl.contexts[0].res(), None);
    }

    // Tests whether we can execute an operation against the log using execute_mut().
    #[test]
    fn test_replica_execute_combine() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
        let repl = Replica::<Data>::new(vec![slog]);
        let idx = repl.register().unwrap();

        assert_eq!(Ok(107), repl.execute_mut(121, idx));
        assert_eq!(1, repl.data.read(0).junk.load(Ordering::Relaxed));
    }

    // Tests whether get_response() retrieves a response to an operation that was executed
    // against a replica.
    #[test]
    fn test_replica_get_response() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
        let repl = Replica::<Data>::new(vec![slog]);
        let _idx = repl.register();

        let op = 121;
        let mut hasher = DefaultHasher::new();
        op.hash(&mut hasher);
        let hash = hasher.finish() as usize;
        repl.make_pending(op, 1, hash);

        assert_eq!(repl.get_response(1, op), Ok(107));
    }

    // Tests whether we can issue a read-only operation against the replica.
    #[test]
    fn test_replica_execute() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
        let repl = Replica::<Data>::new(vec![slog]);
        let idx = repl.register().expect("Failed to register with replica.");

        assert_eq!(Ok(107), repl.execute_mut(121, idx));
        assert_eq!(Ok(1), repl.execute(11, idx));
    }

    // Tests that execute() syncs up the replica with the log before
    // executing the read against the data structure.
    #[test]
    fn test_replica_execute_not_synced() {
        let slog = Arc::new(Log::<<Data as Dispatch>::WriteOperation>::default());
        let repl = Replica::<Data>::new(vec![slog.clone()]);

        // Add in operations to the log off the side, not through the replica.
        let o = [121, 212];
        slog.append(&o, 2, |_o: usize, _i: usize| {});
        slog.exec(2, &mut |_o: usize, _i: usize| {});

        let t1 = repl.register().expect("Failed to register with replica.");
        assert_eq!(Ok(2), repl.execute(11, t1));
    }
}
