//! # SuperQueue
//!
//! A tiny, lock-light, **type-routed message bus**.
//!
//! SuperQueue gives you two complementary primitives, both keyed by Rust
//! `TypeId`:
//!
//! 1) **Event streams** — classic per-subscriber queues (broadcast or
//!    single-consumer), optionally bounded for backpressure.
//! 2) **Latest-value topics** — a single, always-overwritable slot per type
//!    that every actor can sample independently (one observation per update).
//!
//! Primary use cases: fast, ergonomic state/message dispatch for **game
//! development** (systems & actors exchanging events and sampling shared game
//! state), background workers, UI event routing, and modular plugin systems.
//!
//! **Blocking caution:** The blocking send/receive variants can deadlock if you
//! create cyclic waits. Prefer non-blocking calls where appropriate.
//!
//! ## Highlights
//!
//! - **Type-based routing:** subscribers declare `T`; messages are erased as
//!   `Arc<dyn Any + Send + Sync>` and downcast on receipt.
//! - **Two modes, one API surface:**
//!   - **Event streams:** broadcast to all, or deliver to exactly one.
//!   - **Latest-value topics:** publish snapshots; readers see each update at most once.
//! - **Backpressure control (streams):** per-subscription bounded or unbounded queues.
//! - **Simple ownership:** `SuperQueueActor` unsubscribes itself in `Drop`.
//! - **Cheap cloning:** the bus is shared and `Clone`.
//!
//! ## Quick starts
//!
//! ### Event stream (broadcast)
//! ```rust
//! use superqueue::SuperQueue;
//!
//! let bus = SuperQueue::new();
//! let mut recv = bus.create_actor();
//! let send = bus.create_actor();
//!
//! recv.subscribe::<String>(None).unwrap(); // unbounded queue
//! send.send("Hello".to_string()).unwrap(); // broadcast (blocking per receiver if full)
//!
//! let msg = recv.read::<String>().unwrap(); // blocking
//! assert_eq!(&*msg, "Hello");
//! ```
//!
//! ### Latest-value topic (snapshot)
//! ```rust
//! use superqueue::SuperQueue;
//!
//! let bus = SuperQueue::new();
//! let publisher = bus.create_actor();
//! let mut reader = bus.create_actor();
//!
//! // No subscription needed for latest-value topics.
//! assert!(reader.read_latest::<u32>().is_none()); // nothing published yet
//!
//! publisher.update_latest::<u32>(1);
//! assert_eq!(*reader.read_latest::<u32>().unwrap(), 1); // sees the new value
//! assert!(reader.read_latest::<u32>().is_none());       // at most once per update
//!
//! publisher.update_latest::<u32>(2);
//! assert_eq!(*reader.read_latest::<u32>().unwrap(), 2);
//! ```
//!
//! ### Mixing both
//! ```rust
//! # use superqueue::SuperQueue;
//! let bus = SuperQueue::new();
//! let mut physics = bus.create_actor();
//! let ai = bus.create_actor();
//!
//! // Physics consumes events...
//! physics.subscribe::<(u32, u32)>(Some(256)).unwrap(); // position updates as events
//!
//! // ...and also publishes a latest snapshot AI can poll opportunistically.
//! ai.update_latest::<f32>(0.016); // delta time in seconds
//! ```
//!
//! ## Concepts
//!
//! - A **queue** (`SuperQueue`) is shared and cheap to clone.
//! - An **actor** (`SuperQueueActor`) can:
//!   - **Streams:** subscribe to `T` and send/read events of `T`.
//!   - **Latest:** publish `update_latest<T>(value)` and sample with
//!     `read_latest<T>() -> Option<Arc<T>>` (no subscription required).
//! - Stream subscriptions are keyed by `(TypeId, ActorId)` and create a private
//!   channel. Latest-value topics are keyed only by `TypeId` and hold one slot.
//!   Each actor keeps its own cursor to know whether it has already observed
//!   the current latest value of a type.
//!
//! ## Choosing an API
//!
//! **Event streams (per-subscriber queues):**
//! - `send(T)` — **broadcast** to all subscribers of `T`. Blocks per receiver
//!   if that receiver’s queue is bounded and full.
//! - `try_send(T)` — broadcast **without blocking**; if *no* receiver had space,
//!   returns `TrySendError::NoSpaceAvailable`.
//! - `send_single(T)` — deliver to **exactly one** subscriber of `T`. Prefers a
//!   subscriber with capacity; otherwise **blocks on a random subscriber**.
//! - `try_send_single(T)` — like `send_single` but **never blocks**; drops if all are full.
//!
//! **Latest-value topics (single slot per `T`):**
//! - `update_latest(T)` — publish/overwrite the current value for type `T`.
//!   Readers will observe this update once.
//! - `read_latest::<T>() -> Option<Arc<T>>` — return the newest value **exactly once
//!   per actor per update**, or `None` if unchanged since the last call.
//!
//! ## Bounded vs unbounded (streams)
//!
//! - `subscribe::<T>(Some(cap))` creates a bounded channel. Bounded queues
//!   provide backpressure and can cause `send*` to block.
//! - `subscribe::<T>(None)` creates an unbounded channel.
//!
//! **Latest-value topics have no queue and never block.** Updates coalesce
//! (last-writer-wins); intermediate values may be skipped by readers.
//!
//! ## Notes & guarantees
//!
//! - All stream messages are stored as `Arc<T>`; broadcast clones the `Arc`.
//! - Unsubscribe/Drop are coordinated so `send*` does not race with removal
//!   inside a single call.
//! - Latest-value topics:
//!   - There is **one slot per `TypeId`** across the bus (not per actor).
//!   - `update_latest` overwrites the slot atomically; **no history** is kept.
//!   - Each actor sees **at most one** value per update; independent cursors.
//!   - No subscription is needed to publish or read latest values.
//!   - Non-blocking in both directions; suitable for “state snapshots”
//!     (e.g., delta-time, world tick, configuration, last camera pose).
//! - This crate is **not** `no_std`.
//!
//! ---

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use parking_lot::RwLock;
use rand::Rng;
use rustc_hash::{FxHashMap, FxHashSet};
use std::{
    any::{Any, TypeId},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

type Msg = Arc<dyn Any + Send + Sync + 'static>;
type ActorId = u64;

/// Common errors for queue usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Error)]
pub enum SuperQueueError {
    /// This actor is not subscribed to the requested message type.
    #[error("This SuperQueueActor is not subscribed to read messages of the specified type.")]
    NotSubscribed,
    /// This actor is already subscribed to the requested message type.
    #[error("This SuperQueueActor is already subscribed to read messages of the specified type.")]
    AlreadySubscribed,
    /// A non-blocking read found no message in the queue.
    #[error("The queue is empty.")]
    EmptyQueue,
}

/// Errors returned by blocking/broadcast send operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Error)]
pub enum SendError {
    /// No current subscribers exist for the target message type.
    #[error("No subscriber is currently subscribed to read messages of the specified type.")]
    NoSubscribers,
}

/// Errors returned by non-blocking send operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Error)]
pub enum TrySendError {
    /// No current subscribers exist for the target message type.
    #[error("No subscriber is currently subscribed to read messages of the specified type.")]
    NoSubscribers,
    /// All target subscriber queues were full; the message was not sent.
    #[error("All queues of the subscribers are full. The message has not been sent to anyone.")]
    NoSpaceAvailable,
}

/// Shared message bus. Cheap to clone.
///
/// Create actors with [`SuperQueue::create_actor`], then subscribe them to types.
///
/// ```rust
/// use superqueue::SuperQueue;
///
/// let bus = SuperQueue::new();
/// let mut a = bus.create_actor();
/// a.subscribe::<u32>(Some(128)).unwrap(); // bounded per-type queue
/// ```
#[derive(Clone)]
pub struct SuperQueue {
    inner: Arc<SuperQueueInner>,
}

impl SuperQueue {
    /// Create a new, empty bus.
    #[inline]
    pub fn new() -> Self {
        let state = SuperQueueInnerState {
            subscribers_present: FxHashSet::default(),
            subscriber_channels: FxHashMap::default(),
        };
        let latest_value_state = LatestValuesState {
            latest_values: FxHashMap::default(),
        };
        let inner = SuperQueueInner {
            next_actor_id: AtomicU64::new(0),
            queue_state: RwLock::new(state),
            latest_value_state: RwLock::new(latest_value_state),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Create a new actor bound to this bus.
    ///
    /// The actor initially has **no subscriptions**.
    ///
    /// ```rust
    /// use superqueue::SuperQueue;
    ///
    /// let bus = SuperQueue::new();
    /// let actor = bus.create_actor();
    /// ```
    #[inline]
    pub fn create_actor(&self) -> SuperQueueActor {
        let actor_id = self.inner.next_actor_id.fetch_add(1, Ordering::Relaxed);
        SuperQueueActor {
            actor_id,
            channels: FxHashMap::default(),
            latest_value_iterations: FxHashMap::default(),
            queue: self.clone(),
        }
    }
}

impl Default for SuperQueue {
    fn default() -> Self {
        Self::new()
    }
}

struct SuperQueueInner {
    next_actor_id: AtomicU64,
    queue_state: RwLock<SuperQueueInnerState>,
    latest_value_state: RwLock<LatestValuesState>,
}

struct SuperQueueInnerState {
    subscribers_present: FxHashSet<(TypeId, ActorId)>,
    subscriber_channels: FxHashMap<TypeId, Vec<Subscriber>>,
}

struct Subscriber {
    id: ActorId,
    sender: Sender<Msg>,
}

struct LatestValuesState {
    latest_values: FxHashMap<TypeId, ArcSwap<LatestValue>>,
}

struct LatestValue {
    iteration: u64,
    value: Msg,
}

impl SuperQueue {
    /// Broadcast a message to **all** subscribers of this type.
    ///
    /// Blocks per subscriber if that subscriber’s queue is bounded and full.
    ///
    /// Returns [`SendError::NoSubscribers`] if nobody is subscribed to `type_id`.
    #[inline]
    fn send(&self, type_id: TypeId, data: Msg) -> Result<(), SendError> {
        let state = self.inner.queue_state.read();
        if let Some(subscriber) = state.subscriber_channels.get(&type_id) {
            if subscriber.is_empty() {
                return Err(SendError::NoSubscribers);
            }
            for sub in subscriber {
                sub.sender.send(data.clone()).unwrap();
            }
            return Ok(());
        }
        Err(SendError::NoSubscribers)
    }

    /// Send to **exactly one** subscriber of this type.
    ///
    /// Prefers a subscriber with available space using a randomized starting
    /// offset. If all are full, **blocks** on a random subscriber.
    ///
    /// Returns [`SendError::NoSubscribers`] if nobody is subscribed to `type_id`.
    #[inline]
    fn send_single(&self, type_id: TypeId, data: Msg) -> Result<(), SendError> {
        let mut rng = rand::rng();
        let state = self.inner.queue_state.read();
        if let Some(subscribers) = state.subscriber_channels.get(&type_id) {
            if subscribers.is_empty() {
                return Err(SendError::NoSubscribers);
            }
            let index = rng.random_range(0..subscribers.len());
            for i in 0..subscribers.len() {
                let j = (i + index) % subscribers.len();
                let subscriber = &subscribers[j];
                if subscriber.sender.try_send(data.clone()).is_ok() {
                    return Ok(());
                }
            }
            // No subscriber is free to receive the message.
            // We just pick a random one and block on it.
            let subscriber = &subscribers[rng.random_range(0..subscribers.len())];
            subscriber.sender.send(data).unwrap();
            return Ok(());
        }
        Err(SendError::NoSubscribers)
    }

    /// Non-blocking broadcast.
    ///
    /// Attempts to enqueue into every subscriber’s queue without blocking.
    /// If **no** subscriber accepted the message, returns
    /// [`TrySendError::NoSpaceAvailable`].
    ///
    /// Returns [`TrySendError::NoSubscribers`] if nobody is subscribed to `type_id`.
    #[inline]
    fn try_send(&self, type_id: TypeId, data: Msg) -> Result<(), TrySendError> {
        let state = self.inner.queue_state.read();
        if let Some(subscriber) = state.subscriber_channels.get(&type_id) {
            if subscriber.is_empty() {
                return Err(TrySendError::NoSubscribers);
            }
            let mut message_not_sent = true;
            for sub in subscriber {
                if sub.sender.try_send(data.clone()).is_ok() {
                    message_not_sent = false;
                }
            }
            if message_not_sent {
                return Err(TrySendError::NoSpaceAvailable);
            } else {
                return Ok(());
            }
        }
        Err(TrySendError::NoSubscribers)
    }

    /// Non-blocking single-consumer send.
    ///
    /// Picks a randomized starting index and tries each subscriber’s queue
    /// once. If everyone is full, the message is **dropped** and
    /// [`TrySendError::NoSpaceAvailable`] is returned.
    ///
    /// Returns [`TrySendError::NoSubscribers`] if nobody is subscribed to `type_id`.
    #[inline]
    fn try_send_single(&self, type_id: TypeId, data: Msg) -> Result<(), TrySendError> {
        let mut rng = rand::rng();
        let state = self.inner.queue_state.read();
        if let Some(subscribers) = state.subscriber_channels.get(&type_id) {
            if subscribers.is_empty() {
                return Err(TrySendError::NoSubscribers);
            }
            let index = rng.random_range(0..subscribers.len());
            for i in 0..subscribers.len() {
                let j = (i + index) % subscribers.len();
                let subscriber = &subscribers[j];
                if subscriber.sender.try_send(data.clone()).is_ok() {
                    return Ok(());
                }
            }
            // No subscriber is free to receive the message.
            // Just drop the message.
            return Err(TrySendError::NoSpaceAvailable);
        }
        Err(TrySendError::NoSubscribers)
    }

    /// Add a subscriber for a concrete `type_id` with an optional channel bound.
    ///
    /// - `bounds = Some(cap)` creates a bounded channel with capacity `cap`.
    /// - `bounds = None` creates an unbounded channel.
    ///
    /// Returns a `Receiver<Msg>` that is owned by the subscribing actor.
    fn add_subscriber(
        &self,
        type_id: TypeId,
        actor_id: ActorId,
        bounds: Option<usize>,
    ) -> Result<Receiver<Arc<dyn Any + Send + Sync + 'static>>, SuperQueueError> {
        let mut state = self.inner.queue_state.write();
        if state.subscribers_present.contains(&(type_id, actor_id)) {
            return Err(SuperQueueError::AlreadySubscribed);
        }
        let (tx, rx) = match bounds {
            Some(size) => crossbeam_channel::bounded(size),
            None => crossbeam_channel::unbounded(),
        };
        state.subscribers_present.insert((type_id, actor_id));
        state
            .subscriber_channels
            .entry(type_id)
            .or_default()
            .push(Subscriber {
                id: actor_id,
                sender: tx,
            });

        Ok(rx)
    }

    /// Remove a subscriber for a concrete `type_id`.
    ///
    /// Returns [`SuperQueueError::NotSubscribed`] if this actor was not
    /// subscribed to that type.
    fn remove_subscriber(&self, type_id: TypeId, actor_id: ActorId) -> Result<(), SuperQueueError> {
        let mut state = self.inner.queue_state.write();
        if state.subscribers_present.contains(&(type_id, actor_id)) {
            state.subscribers_present.remove(&(type_id, actor_id));
            let subscriber_channels = state.subscriber_channels.get_mut(&type_id).unwrap();
            for i in 0..subscriber_channels.len() {
                let subscriber = &subscriber_channels[i];
                if subscriber.id == actor_id {
                    subscriber_channels.swap_remove(i);
                    if subscriber_channels.is_empty() {
                        state.subscriber_channels.remove(&type_id);
                    }
                    return Ok(());
                }
            }
        }
        Err(SuperQueueError::NotSubscribed)
    }

    fn update_latest(&self, type_id: TypeId, value: Msg) {
        // Try to do arc swap if the type is already present in the hashmap
        {
            let state = self.inner.latest_value_state.read();
            if let Some(latest_value) = state.latest_values.get(&type_id) {
                latest_value.rcu(|p| LatestValue {
                    value: value.clone(),
                    iteration: p.iteration + 1,
                });
                return;
            }
        }

        // If the type is not present, insert a new entry
        let mut state = self.inner.latest_value_state.write();

        // ---- Try again to do an arc swap, maybe someone else has already created the entry
        if let Some(latest_value) = state.latest_values.get(&type_id) {
            latest_value.rcu(|p| LatestValue {
                value: value.clone(),
                iteration: p.iteration + 1,
            });
            return;
        }

        let latest_value = ArcSwap::new(Arc::new(LatestValue {
            value,
            iteration: 1,
        }));
        state.latest_values.insert(type_id, latest_value);
    }

    fn read_latest(&self, type_id: TypeId, last_read: u64) -> Option<(u64, Msg)> {
        let state = self.inner.latest_value_state.read();
        if let Some(entry) = state.latest_values.get(&type_id) {
            let entry = entry.load();
            if entry.iteration <= last_read {
                None
            } else {
                Some((entry.iteration, entry.value.clone()))
            }
        } else {
            None
        }
    }
}

/// A participant in the bus. Owns its receiving channels (per message type)
/// and can send messages of any type.
///
/// Dropping an actor automatically unsubscribes all of its subscriptions.
///
/// ```rust
/// use superqueue::SuperQueue;
///
/// let bus = SuperQueue::new();
/// let mut a = bus.create_actor();
/// let b = bus.create_actor();
///
/// a.subscribe::<String>(None).unwrap();
/// b.send("ping".to_string()).unwrap();
/// assert_eq!(&*a.read::<String>().unwrap(), "ping");
/// ```
pub struct SuperQueueActor {
    actor_id: ActorId,
    channels: FxHashMap<TypeId, Receiver<Msg>>,
    latest_value_iterations: FxHashMap<TypeId, u64>,
    queue: SuperQueue,
}

impl Drop for SuperQueueActor {
    fn drop(&mut self) {
        for (type_id, channel) in self.channels.drain() {
            let _ = self.queue.remove_subscriber(type_id, self.actor_id);
            drop(channel);
        }
    }
}

impl SuperQueueActor {
    /// Broadcast a value of type `T` to all subscribers of `T`.
    ///
    /// Blocks per receiver if their queue is bounded and full.
    ///
    /// Returns [`SendError::NoSubscribers`] if nobody is subscribed to `T`.
    pub fn send<T>(&self, data: T) -> Result<(), SendError>
    where
        T: Any + Send + Sync + 'static,
    {
        self.queue.send(TypeId::of::<T>(), Arc::new(data) as Msg)
    }

    /// Non-blocking broadcast of a value of type `T`.
    ///
    /// If **no** receiver could accept it, returns
    /// [`TrySendError::NoSpaceAvailable`].
    pub fn try_send<T>(&self, data: T) -> Result<(), TrySendError>
    where
        T: Any + Send + Sync + 'static,
    {
        self.queue
            .try_send(TypeId::of::<T>(), Arc::new(data) as Msg)
    }

    /// Send to **one** subscriber of `T`.
    ///
    /// Tries non-blocking first; if all are full, **blocks** on a random
    /// subscriber.
    pub fn send_single<T>(&self, data: T) -> Result<(), SendError>
    where
        T: Any + Send + Sync + 'static,
    {
        self.queue
            .send_single(TypeId::of::<T>(), Arc::new(data) as Msg)
    }

    /// Non-blocking single-consumer send for type `T`.
    ///
    /// Drops the message if everyone is full.
    pub fn try_send_single<T>(&self, data: T) -> Result<(), TrySendError>
    where
        T: Any + Send + Sync + 'static,
    {
        self.queue
            .try_send_single(TypeId::of::<T>(), Arc::new(data) as Msg)
    }

    /// Blocking read for messages of type `T`.
    ///
    /// Returns [`SuperQueueError::NotSubscribed`] if this actor is not
    /// subscribed to `T`.
    pub fn read<T>(&self) -> Result<Arc<T>, SuperQueueError>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        let Some(rx) = self.channels.get(&type_id) else {
            return Err(SuperQueueError::NotSubscribed);
        };
        let erased: Msg = rx.recv().unwrap();
        let concrete: Arc<T> = erased.downcast::<T>().unwrap();
        Ok(concrete)
    }

    /// Non-blocking read for messages of type `T`.
    ///
    /// Returns:
    /// - [`SuperQueueError::NotSubscribed`] if not subscribed to `T`.
    /// - [`SuperQueueError::EmptyQueue`] if the queue currently has no message.
    pub fn try_read<T>(&self) -> Result<Arc<T>, SuperQueueError>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        let Some(rx) = self.channels.get(&type_id) else {
            return Err(SuperQueueError::NotSubscribed);
        };
        let erased = rx.try_recv().map_err(|err| match err {
            TryRecvError::Empty => SuperQueueError::EmptyQueue,
            _ => unreachable!(),
        })?;
        let concrete: Arc<T> = erased.downcast::<T>().unwrap();
        Ok(concrete)
    }

    /// Subscribe this actor to messages of type `T`.
    ///
    /// - `bounds = Some(cap)` for a bounded queue of capacity `cap`.
    /// - `bounds = None` for an unbounded queue.
    ///
    /// Returns [`SuperQueueError::AlreadySubscribed`] if already subscribed.
    pub fn subscribe<T>(&mut self, bounds: Option<usize>) -> Result<(), SuperQueueError>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        let rx = self.queue.add_subscriber(type_id, self.actor_id, bounds)?;
        self.channels.insert(type_id, rx);
        Ok(())
    }

    /// Unsubscribe this actor from messages of type `T`.
    ///
    /// Returns [`SuperQueueError::NotSubscribed`] if not subscribed.
    pub fn unsubscribe<T>(&mut self) -> Result<(), SuperQueueError>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        self.queue.remove_subscriber(type_id, self.actor_id)?;
        self.channels.remove(&TypeId::of::<T>());
        Ok(())
    }

    /// Publish (overwrite) the **latest value** for type `T`.
    ///
    /// This updates a single slot shared by all actors for the `TypeId` of `T`.
    /// Readers use [`read_latest`](Self::read_latest) to observe each update
    /// **at most once per actor**. Intermediate updates may be skipped if
    /// multiple `update_latest` calls happen before a reader samples.
    ///
    /// Characteristics:
    /// - **No subscription required.**
    /// - **Non-blocking.** No backpressure; last-writer-wins.
    /// - **No history.** Only the newest value is retained.
    ///
    /// Typical uses: world/frame state, configuration snapshots, last known
    /// transform, delta time, “most recent metrics”.
    ///
    /// # Examples
    /// ```
    /// # use superqueue::SuperQueue;
    /// let bus = SuperQueue::new();
    /// let publisher = bus.create_actor();
    /// let mut reader = bus.create_actor();
    ///
    /// publisher.update_latest::<u64>(42);
    /// assert_eq!(*reader.read_latest::<u64>().unwrap(), 42);
    /// assert!(reader.read_latest::<u64>().is_none()); // same update seen already
    /// ```
    pub fn update_latest<T>(&self, value: T)
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        self.queue.update_latest(type_id, Arc::new(value));
    }

    /// Read the **latest value** of type `T` **once per update**.
    ///
    /// Returns:
    /// - `Some(Arc<T>)` if a newer value than this actor last observed exists.
    /// - `None` if no value has ever been published for `T`, or if the actor
    ///   already consumed the current update.
    ///
    /// Characteristics:
    /// - **Non-blocking.** Never waits.
    /// - **No subscription required.**
    /// - **Per-actor cursor.** Each actor observes every update at most once.
    ///
    /// # Examples
    /// ```
    /// # use superqueue::SuperQueue;
    /// let bus = SuperQueue::new();
    /// let producer = bus.create_actor();
    /// let mut a = bus.create_actor();
    /// let mut b = bus.create_actor();
    ///
    /// assert!(a.read_latest::<i32>().is_none()); // nothing yet
    ///
    /// producer.update_latest::<i32>(7);
    /// assert_eq!(*a.read_latest::<i32>().unwrap(), 7);
    /// assert!(a.read_latest::<i32>().is_none()); // already seen
    ///
    /// // Another actor has an independent cursor and can also see "7" once.
    /// assert_eq!(*b.read_latest::<i32>().unwrap(), 7);
    ///
    /// // Subsequent update:
    /// producer.update_latest::<i32>(9);
    /// assert_eq!(*a.read_latest::<i32>().unwrap(), 9);
    /// assert_eq!(*b.read_latest::<i32>().unwrap(), 9);
    /// ```
    pub fn read_latest<T>(&mut self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        if let Some(latest_read) = self.latest_value_iterations.get_mut(&type_id) {
            let (iteration, msg) = self.queue.read_latest(type_id, *latest_read)?;
            *latest_read = iteration;
            let value = msg.downcast::<T>().unwrap();
            Some(value)
        } else {
            let (iteration, msg) = self.queue.read_latest(type_id, 0)?;
            let value = msg.downcast::<T>().unwrap();
            self.latest_value_iterations.insert(type_id, iteration);
            Some(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, thread};

    use super::*;

    #[test]
    fn simple() {
        let queue = SuperQueue::new();
        let mut actor = queue.create_actor();

        actor.subscribe::<i32>(None).unwrap();
        actor.send(42).unwrap();

        let result = actor.read::<i32>().unwrap();
        assert_eq!(*result, 42);
    }

    #[test]
    fn simple_multiple() {
        let queue = SuperQueue::new();
        let mut actors: Vec<SuperQueueActor> = Vec::new();

        for _ in 0..10000 {
            let mut actor = queue.create_actor();
            actor.subscribe::<i32>(None).unwrap();
            actor.unsubscribe::<i32>().unwrap();
            actor.subscribe::<i32>(None).unwrap();
            actors.push(actor);
        }

        actors[0].send(1).unwrap();
        actors[100].send(2).unwrap();
        actors[2000].send(3).unwrap();

        for actor in actors {
            assert_eq!(*actor.read::<i32>().unwrap(), 1);
            assert_eq!(*actor.read::<i32>().unwrap(), 2);
            assert_eq!(*actor.read::<i32>().unwrap(), 3);
        }
    }

    #[test]
    fn threaded_simple() {
        let queue = SuperQueue::new();
        let mut actor = queue.create_actor();
        actor.subscribe::<i32>(None).unwrap();

        std::thread::spawn({
            let queue = queue.clone();
            move || {
                let actor = queue.create_actor();
                actor.send(42).unwrap();
            }
        });

        let result = actor.read::<i32>().unwrap();
        assert_eq!(*result, 42);
    }

    #[test]
    fn unsubscribe_stops_delivery_and_future_reads_error() {
        let queue = SuperQueue::new();
        let mut actor = queue.create_actor();

        // Subscribe, send, and read once successfully.
        actor.subscribe::<i32>(None).unwrap();
        actor.send(10).unwrap();
        assert_eq!(*actor.read::<i32>().unwrap(), 10);

        // Unsubscribe: reading should immediately error (no channel).
        actor.unsubscribe::<i32>().unwrap();
        assert!(matches!(
            actor.read::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));

        // Further sends for this type must not be deliverable to this actor.
        let sender = queue.create_actor();
        assert!(matches!(sender.send(11_i32), Err(SendError::NoSubscribers)));
        assert!(matches!(
            actor.read::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));
    }

    #[test]
    fn multiple_message_types_and_ordering_per_type() {
        let queue = SuperQueue::new();

        let mut only_ints = queue.create_actor();
        only_ints.subscribe::<i32>(None).unwrap();

        let mut only_strings = queue.create_actor();
        only_strings.subscribe::<String>(None).unwrap();

        let mut both = queue.create_actor();
        both.subscribe::<i32>(None).unwrap();
        both.subscribe::<String>(None).unwrap();

        let sender = queue.create_actor();
        sender.send(7_i32).unwrap();
        sender.send::<String>("hi".to_string()).unwrap();
        sender.send(8_i32).unwrap();
        sender.send::<String>("ho".to_string()).unwrap();

        // Per-type FIFO is preserved.
        assert_eq!(*only_ints.read::<i32>().unwrap(), 7);
        assert_eq!(*only_ints.read::<i32>().unwrap(), 8);

        assert_eq!(&*only_strings.read::<String>().unwrap(), "hi");
        assert_eq!(&*only_strings.read::<String>().unwrap(), "ho");

        // The actor subscribed to both types should receive both streams, each ordered.
        assert_eq!(*both.read::<i32>().unwrap(), 7);
        assert_eq!(&*both.read::<String>().unwrap(), "hi");
        assert_eq!(*both.read::<i32>().unwrap(), 8);
        assert_eq!(&*both.read::<String>().unwrap(), "ho");
    }

    #[test]
    fn resubscribe_does_not_deliver_messages_sent_while_unsubscribed() {
        let queue = SuperQueue::new();
        let mut actor = queue.create_actor();

        actor.subscribe::<i32>(None).unwrap();
        actor.send(1).unwrap();
        assert_eq!(*actor.read::<i32>().unwrap(), 1);

        // Unsubscribe, messages sent now should not be queued for this actor.
        actor.unsubscribe::<i32>().unwrap();
        let other = queue.create_actor();
        assert!(matches!(other.send(2_i32), Err(SendError::NoSubscribers)));

        // Resubscribe and send again; we should only see the new message (3), not the missed (2).
        actor.subscribe::<i32>(None).unwrap();
        other.send(3).unwrap();

        assert_eq!(*actor.read::<i32>().unwrap(), 3);
    }

    #[test]
    fn misuse_errors_are_surface() {
        let queue = SuperQueue::new();

        // Double subscribe should error.
        let mut a = queue.create_actor();
        assert!(a.subscribe::<i32>(None).is_ok());
        assert!(matches!(
            a.subscribe::<i32>(None),
            Err(SuperQueueError::AlreadySubscribed)
        ));

        // Unsubscribe without being subscribed should error.
        let mut b = queue.create_actor();
        assert!(matches!(
            b.unsubscribe::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));

        // Read without subscription should error.
        let c = queue.create_actor();
        assert!(matches!(
            c.read::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));
    }

    #[test]
    fn concurrency_broadcast_stress_many_senders_many_receivers() {
        #[derive(Debug)]
        struct TestMsg {
            from: usize,
            seq: usize,
        }

        let queue = SuperQueue::new();

        // Prepare multiple receivers; broadcast semantics mean each will get all messages.
        const RECEIVERS: usize = 31;
        let mut receivers: Vec<SuperQueueActor> = (0..RECEIVERS)
            .map(|_| {
                let mut r = queue.create_actor();
                r.subscribe::<TestMsg>(None).unwrap();
                r
            })
            .collect();

        // Spawn several senders, each sending a sequence of messages.
        const SENDERS: usize = 71;
        const MSGS_PER_SENDER: usize = 200;

        let barrier = Arc::new(Barrier::new(SENDERS + 1));
        let mut handles = Vec::with_capacity(SENDERS);

        for s in 0..SENDERS {
            let queue_cloned = queue.clone();
            let barrier_cloned = barrier.clone();
            handles.push(thread::spawn(move || {
                let actor = queue_cloned.create_actor();
                // Start all senders together for better interleaving.
                barrier_cloned.wait();
                for seq in 0..MSGS_PER_SENDER {
                    actor.send(TestMsg { from: s, seq }).unwrap();
                }
            }));
        }

        // Release the senders and wait for completion.
        barrier.wait();
        for h in handles {
            h.join().expect("sender thread panicked");
        }

        // Each receiver must observe all messages from all senders.
        let expected = SENDERS * MSGS_PER_SENDER;
        for r in receivers.drain(..) {
            let mut seen: FxHashSet<(usize, usize)> = FxHashSet::default();
            for _ in 0..expected {
                let msg = r.read::<TestMsg>().unwrap();
                seen.insert((msg.from, msg.seq));
            }
            assert_eq!(seen.len(), expected, "receiver missed some messages");
        }
    }

    #[test]
    fn drop_removes_subscriptions_and_prevents_send_panics() {
        let queue = SuperQueue::new();

        // Actor subscribed to multiple types, then dropped without explicit unsubscribe.
        {
            let mut to_drop = queue.create_actor();
            to_drop.subscribe::<i32>(None).unwrap();
            to_drop.subscribe::<String>(None).unwrap();
            // Dropped here; Drop impl should remove both subscriptions.
        }

        // A surviving receiver proves broadcasts still work after the drop.
        let mut survivor = queue.create_actor();
        survivor.subscribe::<i32>(None).unwrap();
        survivor.subscribe::<String>(None).unwrap();

        // If drop hadn't removed the stale channels, these sends would panic (send to closed).
        let sender = queue.create_actor();
        sender.send(123_i32).unwrap();
        sender.send::<String>("hello".to_string()).unwrap();

        // Survivor should receive the messages normally.
        assert_eq!(*survivor.read::<i32>().unwrap(), 123);
        assert_eq!(&*survivor.read::<String>().unwrap(), "hello");
    }

    #[test]
    fn try_read_nonblocking_and_errors() {
        let queue = SuperQueue::new();
        let mut actor = queue.create_actor();

        // Not subscribed -> NotSubscribed
        assert!(matches!(
            actor.try_read::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));

        // Subscribe, but no messages yet -> EmptyQueue
        actor.subscribe::<i32>(None).unwrap();
        assert!(matches!(
            actor.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));

        // Send a message and read it non-blockingly -> Ok(42)
        actor.send(42_i32).unwrap();
        let msg = actor.try_read::<i32>().unwrap();
        assert_eq!(*msg, 42);

        // Queue now empty again -> EmptyQueue
        assert!(matches!(
            actor.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));

        // Unsubscribe -> NotSubscribed
        actor.unsubscribe::<i32>().unwrap();
        assert!(matches!(
            actor.try_read::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));
    }

    #[test]
    fn try_send_delivers_when_capacity_available() {
        let queue = SuperQueue::new();

        let mut r = queue.create_actor();
        // Provide some capacity to make delivery deterministic.
        r.subscribe::<i32>(Some(4)).unwrap();

        let s = queue.create_actor();
        s.try_send(10_i32).unwrap();
        s.try_send(20_i32).unwrap();

        assert_eq!(*r.read::<i32>().unwrap(), 10);
        assert_eq!(*r.read::<i32>().unwrap(), 20);
    }

    #[test]
    fn try_send_drops_when_bounded_channel_full() {
        let queue = SuperQueue::new();

        let mut r = queue.create_actor();
        // Capacity 1: second try_send should be dropped for this subscriber.
        r.subscribe::<i32>(Some(1)).unwrap();

        let s = queue.create_actor();
        s.try_send(1_i32).unwrap(); // fills the single slot
        assert!(matches!(
            s.try_send(2_i32), // dropped (non-blocking)
            Err(TrySendError::NoSpaceAvailable)
        ));

        assert_eq!(*r.read::<i32>().unwrap(), 1);
        assert!(matches!(
            r.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));
    }

    #[test]
    fn try_send_on_zero_capacity_is_nonblocking_and_drops() {
        let queue = SuperQueue::new();

        let mut r = queue.create_actor();
        // Rendezvous channel (capacity 0): try_send always fails immediately.
        r.subscribe::<i32>(Some(0)).unwrap();

        let s = queue.create_actor();
        assert!(matches!(
            s.try_send(123_i32), // must not block
            Err(TrySendError::NoSpaceAvailable)
        ));

        assert!(matches!(
            r.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));
    }

    #[test]
    fn try_send_is_delivered_to_available_subscribers_even_if_others_full() {
        let queue = SuperQueue::new();

        // First receiver: bounded(1) and pre-filled so it is full.
        let mut r1 = queue.create_actor();
        r1.subscribe::<i32>(Some(1)).unwrap();

        let sender = queue.create_actor();
        sender.send(1_i32).unwrap(); // delivered only to r1 for now (r2 not subscribed yet)

        // Second receiver: has capacity and subscribes after the pre-fill.
        let mut r2 = queue.create_actor();
        r2.subscribe::<i32>(Some(1)).unwrap();

        // This broadcast should reach r2 but be dropped for r1 (which is still full).
        sender.try_send(2_i32).unwrap();

        // r2 receives the new message
        assert_eq!(*r2.read::<i32>().unwrap(), 2);

        // r1 still only has the pre-filled message
        assert_eq!(*r1.read::<i32>().unwrap(), 1);
        assert!(matches!(
            r1.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));
    }

    #[test]
    fn try_send_with_no_subscribers_is_noop() {
        let queue = SuperQueue::new();
        let s = queue.create_actor();

        // Should not panic or block even if there are no subscribers.
        assert!(matches!(
            s.try_send(999_i32),
            Err(TrySendError::NoSubscribers)
        ));
    }

    #[test]
    fn send_single_without_subscribers_errors() {
        let queue = SuperQueue::new();
        let s = queue.create_actor();

        assert!(matches!(
            s.send_single(123_i32),
            Err(SendError::NoSubscribers)
        ));
    }

    #[test]
    fn send_single_delivers_to_exactly_one_subscriber() {
        let queue = SuperQueue::new();

        let mut r1 = queue.create_actor();
        let mut r2 = queue.create_actor();
        r1.subscribe::<i32>(Some(4)).unwrap();
        r2.subscribe::<i32>(Some(4)).unwrap();

        let s = queue.create_actor();
        s.send_single(99_i32).unwrap();

        let r1_msg = r1.try_read::<i32>();
        let r2_msg = r2.try_read::<i32>();

        match (r1_msg, r2_msg) {
            (Ok(v), Err(SuperQueueError::EmptyQueue)) => assert_eq!(*v, 99),
            (Err(SuperQueueError::EmptyQueue), Ok(v)) => assert_eq!(*v, 99),
            _ => panic!("expected exactly one receiver to get the message"),
        }
    }

    #[test]
    fn send_single_finds_free_subscriber_when_others_full() {
        let queue = SuperQueue::new();

        // r1 subscribes first and will be pre-filled to make it full.
        let mut r1 = queue.create_actor();
        r1.subscribe::<i32>(Some(1)).unwrap();

        // Send a prefill while only r1 is subscribed -> only r1 receives it and is now full.
        let s_prefill = queue.create_actor();
        s_prefill.send(1_i32).unwrap();

        // r2 subscribes after the prefill and has free capacity.
        let mut r2 = queue.create_actor();
        r2.subscribe::<i32>(Some(1)).unwrap();

        // Now send_single must deliver to r2 (the only subscriber with space) and must not block.
        let s = queue.create_actor();
        s.send_single(2_i32).unwrap();

        // r2 gets the new message; r1 still only has its prefill.
        assert_eq!(*r2.read::<i32>().unwrap(), 2);
        assert_eq!(*r1.read::<i32>().unwrap(), 1);
        assert!(matches!(
            r1.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));
    }

    #[test]
    fn send_single_blocks_when_all_full_until_capacity_freed() {
        let queue = SuperQueue::new();

        // Two bounded subscribers, both will be filled once.
        let mut r1 = queue.create_actor();
        let mut r2 = queue.create_actor();
        r1.subscribe::<i32>(Some(1)).unwrap();
        r2.subscribe::<i32>(Some(1)).unwrap();

        // Broadcast a prefill -> both queues now full.
        let s_prefill = queue.create_actor();
        s_prefill.send(1_i32).unwrap();

        // Spawn a sender that will have to block because all subscribers are full.
        let queue_cloned = queue.clone();
        let handle = std::thread::spawn(move || {
            let s = queue_cloned.create_actor();
            // This call blocks until some subscriber frees capacity.
            s.send_single(2_i32).unwrap();
        });

        std::thread::sleep(std::time::Duration::from_millis(500));

        // Free exactly one slot in each subscriber (consume the prefill).
        assert_eq!(*r1.read::<i32>().unwrap(), 1);
        assert_eq!(*r2.read::<i32>().unwrap(), 1);

        // Now the blocked sender can complete (regardless of which subscriber it picked).
        handle.join().expect("send_single thread panicked");

        // Exactly one of the subscribers must have received the new message.
        let a = r1.try_read::<i32>();
        let b = r2.try_read::<i32>();
        match (a, b) {
            (Ok(v), Err(SuperQueueError::EmptyQueue)) => assert_eq!(*v, 2),
            (Err(SuperQueueError::EmptyQueue), Ok(v)) => assert_eq!(*v, 2),
            _ => panic!("expected exactly one receiver to get the new message"),
        }
    }

    #[test]
    fn try_send_single_without_subscribers_errors() {
        let queue = SuperQueue::new();
        let s = queue.create_actor();

        // No one is subscribed -> must report NoSubscribers.
        assert!(matches!(
            s.try_send_single(123_i32),
            Err(TrySendError::NoSubscribers)
        ));
    }

    #[test]
    fn try_send_single_delivers_to_exactly_one_subscriber() {
        let queue = SuperQueue::new();

        let mut r1 = queue.create_actor();
        let mut r2 = queue.create_actor();
        // Give both some capacity; one (and only one) should receive.
        r1.subscribe::<i32>(Some(4)).unwrap();
        r2.subscribe::<i32>(Some(4)).unwrap();

        let s = queue.create_actor();
        s.try_send_single(77_i32).unwrap();

        let r1_msg = r1.try_read::<i32>();
        let r2_msg = r2.try_read::<i32>();

        match (r1_msg, r2_msg) {
            (Ok(v), Err(SuperQueueError::EmptyQueue)) => assert_eq!(*v, 77),
            (Err(SuperQueueError::EmptyQueue), Ok(v)) => assert_eq!(*v, 77),
            _ => panic!("expected exactly one receiver to get the message"),
        }
    }

    #[test]
    fn try_send_single_finds_free_subscriber_when_others_full_and_reports_no_space_when_all_full() {
        let queue = SuperQueue::new();

        // r1 subscribes first and will be pre-filled to make it full.
        let mut r1 = queue.create_actor();
        r1.subscribe::<i32>(Some(1)).unwrap();

        // Pre-fill r1 while it is the only subscriber -> r1 is now full.
        let s_prefill = queue.create_actor();
        s_prefill.send(1_i32).unwrap();

        // r2 has free capacity.
        let mut r2 = queue.create_actor();
        r2.subscribe::<i32>(Some(1)).unwrap();

        // Case A: at least one subscriber has space -> must deliver to that subscriber, non-blocking.
        let s = queue.create_actor();
        s.try_send_single(2_i32).unwrap();

        // r2 receives the message (r1 was full).
        assert_eq!(*r2.read::<i32>().unwrap(), 2);
        // r1 still only has the pre-fill.
        assert_eq!(*r1.read::<i32>().unwrap(), 1);
        assert!(matches!(
            r1.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));

        // Case B: all subscribers full -> the call is non-blocking and returns NoSpaceAvailable.
        // Refill both to be full again.
        let s_refill = queue.create_actor();
        s_refill.send(10_i32).unwrap(); // fills r1 and r2 (each capacity 1)

        // Now both are full; try_send_single should not block and should report no space.
        let s_drop = queue.create_actor();
        assert!(matches!(
            s_drop.try_send_single(999_i32),
            Err(TrySendError::NoSpaceAvailable)
        ));

        // Verify neither received the dropped message (they still only have the refill).
        assert_eq!(*r1.read::<i32>().unwrap(), 10);
        assert!(matches!(
            r1.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));
        assert_eq!(*r2.read::<i32>().unwrap(), 10);
        assert!(matches!(
            r2.try_read::<i32>(),
            Err(SuperQueueError::EmptyQueue)
        ));
    }

    #[test]
    fn latest_value_none_before_any_update() {
        let queue = SuperQueue::new();
        let mut r = queue.create_actor();

        // No value published yet => None.
        assert!(r.read_latest::<i32>().is_none());
        // Still none on repeated calls.
        assert!(r.read_latest::<i32>().is_none());
    }

    #[test]
    fn latest_value_once_per_update() {
        let queue = SuperQueue::new();
        let u = queue.create_actor();
        let mut r = queue.create_actor();

        u.update_latest::<i32>(1);
        assert_eq!(*r.read_latest::<i32>().unwrap(), 1);
        // No new update -> None (requires the <= fix).
        assert!(r.read_latest::<i32>().is_none());

        u.update_latest::<i32>(2);
        assert_eq!(*r.read_latest::<i32>().unwrap(), 2);
        assert!(r.read_latest::<i32>().is_none());
    }

    #[test]
    fn latest_value_returns_latest_not_intermediate() {
        let queue = SuperQueue::new();
        let u = queue.create_actor();
        let mut r = queue.create_actor();

        // Several quick updates; reader should only observe the newest one.
        u.update_latest::<i32>(10);
        u.update_latest::<i32>(20);
        u.update_latest::<i32>(30);

        assert_eq!(*r.read_latest::<i32>().unwrap(), 30);
        assert!(r.read_latest::<i32>().is_none());
    }

    #[test]
    fn latest_value_per_actor_cursors_are_independent() {
        let queue = SuperQueue::new();
        let u = queue.create_actor();
        let mut r1 = queue.create_actor();
        let mut r2 = queue.create_actor();

        u.update_latest::<String>("alpha".to_string());

        // Both actors can observe the same "latest" once.
        assert_eq!(&*r1.read_latest::<String>().unwrap(), "alpha");
        assert_eq!(&*r2.read_latest::<String>().unwrap(), "alpha");

        // Neither sees it again until a new update is published.
        assert!(r1.read_latest::<String>().is_none());
        assert!(r2.read_latest::<String>().is_none());

        u.update_latest::<String>("beta".to_string());
        assert_eq!(&*r1.read_latest::<String>().unwrap(), "beta");
        assert_eq!(&*r2.read_latest::<String>().unwrap(), "beta");
    }

    #[test]
    fn latest_value_does_not_require_subscription() {
        let queue = SuperQueue::new();
        let updater = queue.create_actor();
        let mut reader = queue.create_actor();

        // No subscribe() calls at all.
        updater.update_latest::<u64>(123);
        assert_eq!(*reader.read_latest::<u64>().unwrap(), 123);
        assert!(reader.read_latest::<u64>().is_none());
    }

    #[test]
    fn latest_value_concurrent_updates_yield_a_single_latest_snapshot() {
        use std::{
            sync::{Arc, Barrier},
            thread,
        };

        let queue = SuperQueue::new();
        let mut reader = queue.create_actor();

        const THREADS: usize = 16;
        let barrier = Arc::new(Barrier::new(THREADS + 1));
        let mut handles = Vec::new();

        for t in 0..THREADS {
            let q = queue.clone();
            let b = barrier.clone();
            handles.push(thread::spawn(move || {
                let a = q.create_actor();
                b.wait();
                // Each thread publishes a distinct value.
                a.update_latest::<usize>(1000 + t);
            }));
        }

        // Start all updaters near-simultaneously.
        barrier.wait();
        for h in handles {
            h.join().unwrap();
        }

        // Reader sees exactly one of the published values, then nothing more
        // until a subsequent update.
        let v = reader.read_latest::<usize>().unwrap();
        assert!(*v >= 1000 && *v < 1000 + THREADS);
        assert!(reader.read_latest::<usize>().is_none());
    }
}
