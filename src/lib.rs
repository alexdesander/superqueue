use crossbeam_channel::{Receiver, Sender, TryRecvError};
use rustc_hash::{FxHashMap, FxHashSet};
use std::{
    any::{Any, TypeId},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

type Msg = Arc<dyn Any + Send + Sync + 'static>;
type ActorId = u64;

#[derive(Clone)]
pub struct SuperQueue {
    inner: Arc<SuperQueueInner>,
}

impl SuperQueue {
    pub fn new() -> Self {
        let state = SuperQueueInnerState {
            subscribers_present: FxHashSet::default(),
            subscriber_channels: FxHashMap::default(),
        };
        let inner = SuperQueueInner {
            next_actor_id: AtomicU64::new(0),
            state: RwLock::new(state),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn create_actor(&self) -> SuperQueueActor {
        let actor_id = self.inner.next_actor_id.fetch_add(1, Ordering::Relaxed);
        SuperQueueActor {
            actor_id,
            channels: FxHashMap::default(),
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
    state: RwLock<SuperQueueInnerState>,
}

struct SuperQueueInnerState {
    subscribers_present: FxHashSet<(TypeId, ActorId)>,
    subscriber_channels: FxHashMap<TypeId, Vec<Subscriber>>,
}

struct Subscriber {
    id: ActorId,
    sender: Sender<Msg>,
}

impl SuperQueue {
    fn send(&self, type_id: TypeId, data: Msg) {
        let state = self.inner.state.read().unwrap();
        if let Some(subscriber) = state.subscriber_channels.get(&type_id) {
            for sub in subscriber {
                sub.sender.send(Arc::clone(&data)).unwrap();
            }
        }
    }

    fn try_send(&self, type_id: TypeId, data: Msg) {
        let state = self.inner.state.read().unwrap();
        if let Some(subscriber) = state.subscriber_channels.get(&type_id) {
            for sub in subscriber {
                let _ = sub.sender.try_send(Arc::clone(&data));
            }
        }
    }

    fn add_subscriber(
        &self,
        type_id: TypeId,
        actor_id: ActorId,
        bounds: Option<usize>,
    ) -> Result<Receiver<Arc<dyn Any + Send + Sync + 'static>>, SuperQueueError> {
        let mut state = self.inner.state.write().unwrap();
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

    fn remove_subscriber(&self, type_id: TypeId, actor_id: ActorId) -> Result<(), SuperQueueError> {
        let mut state = self.inner.state.write().unwrap();
        if state.subscribers_present.contains(&(type_id, actor_id)) {
            state.subscribers_present.remove(&(type_id, actor_id));
            let subscriber_channels = state.subscriber_channels.get_mut(&type_id).unwrap();
            for i in 0..subscriber_channels.len() {
                let subscriber = &subscriber_channels[i];
                if subscriber.id == actor_id {
                    subscriber_channels.remove(i);
                    return Ok(());
                }
            }
        }
        Err(SuperQueueError::NotSubscribed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Error)]
pub enum SuperQueueError {
    #[error("This SuperQueueActor is not subscribed to read messages of the specified type.")]
    NotSubscribed,
    #[error("This SuperQueueActor is already subscribed to read messages of the specified type.")]
    AlreadySubscribed,
    #[error("The queue is empty.")]
    EmptyQueue,
}

pub struct SuperQueueActor {
    actor_id: ActorId,
    channels: FxHashMap<TypeId, Receiver<Msg>>,
    queue: SuperQueue,
}

impl Drop for SuperQueueActor {
    fn drop(&mut self) {
        for (type_id, _) in self.channels.drain() {
            let _ = self.queue.remove_subscriber(type_id, self.actor_id);
        }
    }
}

impl SuperQueueActor {
    pub fn send<T>(&self, data: T)
    where
        T: Any + Send + Sync + 'static,
    {
        self.queue.send(TypeId::of::<T>(), Arc::new(data) as Msg);
    }

    pub fn try_send<T>(&self, data: T)
    where
        T: Any + Send + Sync + 'static,
    {
        self.queue
            .try_send(TypeId::of::<T>(), Arc::new(data) as Msg);
    }

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

    pub fn subscribe<T>(&mut self, bounds: Option<usize>) -> Result<(), SuperQueueError>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        let rx = self.queue.add_subscriber(type_id, self.actor_id, bounds)?;
        self.channels.insert(type_id, rx);
        Ok(())
    }

    pub fn unsubscribe<T>(&mut self) -> Result<(), SuperQueueError>
    where
        T: Any + Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        self.queue.remove_subscriber(type_id, self.actor_id)?;
        self.channels.remove(&TypeId::of::<T>());
        Ok(())
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
        actor.send(42);

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

        actors[0].send(1);
        actors[100].send(2);
        actors[2000].send(3);

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
                actor.send(42);
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
        actor.send(10);
        assert_eq!(*actor.read::<i32>().unwrap(), 10);

        // Unsubscribe: reading should immediately error (no channel).
        actor.unsubscribe::<i32>().unwrap();
        assert!(matches!(
            actor.read::<i32>(),
            Err(SuperQueueError::NotSubscribed)
        ));

        // Further sends for this type must not be deliverable to this actor.
        let sender = queue.create_actor();
        sender.send(11);
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
        sender.send(7_i32);
        sender.send::<String>("hi".to_string());
        sender.send(8_i32);
        sender.send::<String>("ho".to_string());

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
        actor.send(1);
        assert_eq!(*actor.read::<i32>().unwrap(), 1);

        // Unsubscribe, messages sent now should not be queued for this actor.
        actor.unsubscribe::<i32>().unwrap();
        let other = queue.create_actor();
        other.send(2);

        // Resubscribe and send again; we should only see the new message (3), not the missed (2).
        actor.subscribe::<i32>(None).unwrap();
        other.send(3);

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
                    actor.send(TestMsg { from: s, seq });
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
        sender.send(123_i32);
        sender.send::<String>("hello".to_string());

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
        actor.send(42_i32);
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
        s.try_send(10_i32);
        s.try_send(20_i32);

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
        s.try_send(1_i32); // fills the single slot
        s.try_send(2_i32); // dropped (non-blocking)

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
        s.try_send(123_i32); // must not block

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
        sender.send(1_i32); // delivered only to r1 for now (r2 not subscribed yet)

        // Second receiver: has capacity and subscribes after the pre-fill.
        let mut r2 = queue.create_actor();
        r2.subscribe::<i32>(Some(1)).unwrap();

        // This broadcast should reach r2 but be dropped for r1 (which is still full).
        sender.try_send(2_i32);

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
        s.try_send(999_i32);
    }
}
