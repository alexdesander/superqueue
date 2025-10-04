# SuperQueue

A tiny, lock-light, **type-routed** message bus built on [`crossbeam_channel`].
Primary use-case: fast, ergonomic state/event dispatch for **game development** (systems & actors exchanging events without tight coupling). It also fits background workers, UI routing, or modular plugin systems.

[`crossbeam_channel`]: https://docs.rs/crossbeam-channel

---

## Highlights

* **Type-based routing** – subscribe to concrete `T`; messages are erased internally (`Arc<dyn Any + Send + Sync>`) and downcast on read.
* **Broadcast or single-consumer** delivery – choose per send.
* **Backpressure** – per-subscription bounded or unbounded queues.
* **Simple ownership** – `SuperQueueActor` unsubscribes itself in `Drop`.
* **Cheap cloning** – `SuperQueue` is a small, shared handle; messages are `Arc<T>`.

> ⚠️ **Blocking caveat:** the blocking send variants can deadlock your code if receivers never drain their queues. Prefer the `try_*` APIs where appropriate.

---

## Install

```toml
# Cargo.toml
[dependencies]
superqueue = "0.1"
```

This crate uses `std` and `crossbeam_channel` (not `no_std`).

---

## Quick start

```rust
use superqueue::SuperQueue;

let bus = SuperQueue::new();
let mut receiver = bus.create_actor();
let sender = bus.create_actor();

// Subscribe the receiver to String messages (unbounded)
receiver.subscribe::<String>(None)?;

// Send a message
sender.send("Hello, world".to_string())?;

// Read it (blocking)
let msg = receiver.read::<String>()?;
assert_eq!(&*msg, "Hello, world");
# Ok::<_, Box<dyn std::error::Error>>(())
```

---

## Core concepts

* **Queue (`SuperQueue`)** – the shared bus; cheap to clone.
* **Actor (`SuperQueueActor`)** – a participant that owns its receive channels and can send any `T: Any + Send + Sync + 'static`.
* **Subscriptions** – keyed by `TypeId`. Each `(TypeId, ActorId)` pair gets its **own** channel.

---

## Choosing a send API

| Method               | Delivery                                | Blocking                                                                                | Errors                                                                                                     |
| -------------------- | --------------------------------------- | --------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `send(T)`            | Broadcast to **all** subscribers of `T` | Blocks per receiver if their queue is full                                              | `SendError::NoSubscribers`                                                                                 |
| `try_send(T)`        | Broadcast, **non-blocking**             | Never blocks; may drop per-receiver if full                                             | `TrySendError::{NoSubscribers, NoSpaceAvailable}` (*NoSpaceAvailable* only if **no** subscriber accepted it) |
| `send_single(T)`     | **Exactly one** subscriber              | Prefers a subscriber with space; if everyone is full, **blocks on a random** subscriber | `SendError::NoSubscribers`                                                                                 |
| `try_send_single(T)` | Exactly one, **non-blocking**           | Never blocks; drops if everyone is full                                                 | `TrySendError::{NoSubscribers, NoSpaceAvailable}`                                                           |

### Reading

* `read::<T>() -> Result<Arc<T>, SuperQueueError>` – blocking.
* `try_read::<T>() -> Result<Arc<T>, SuperQueueError>` – non-blocking (`EmptyQueue` if none).

### Subscribing

* `subscribe::<T>(Some(cap))` – bounded queue (backpressure).
* `subscribe::<T>(None)` – unbounded queue.
* `unsubscribe::<T>()` – remove the subscription.

> Tip: `Some(0)` creates a **rendezvous** channel. `try_send*` will always return `NoSpaceAvailable` unless a receiver is waiting; `send*` will rendezvous (and may block).

---

## Guarantees & behavior

* **Per-type FIFO:** For a given actor and type `T`, message order matches send order.
* **Arc cloning:** Broadcast is cheap (`Arc<T>` clones).
* **Drop safety:** Dropping an actor removes its subscriptions; subsequent sends won’t panic due to closed channels.
* **No replay:** Messages sent while an actor is **unsubscribed** are not queued for it.

---

## Patterns

### 1) Broadcast events to many systems

```rust
#[derive(Clone)]
struct PlayerMoved { id: u32, x: f32, y: f32 }

let bus = SuperQueue::new();
let mut physics = bus.create_actor();
let mut audio   = bus.create_actor();

physics.subscribe::<PlayerMoved>(None)?;
audio.subscribe::<PlayerMoved>(None)?;

let tx = bus.create_actor();
tx.try_send(PlayerMoved { id: 1, x: 4.0, y: 2.0 })?;
```

### 2) Single-consumer worker pool

```rust
#[derive(Clone)]
struct PathJob { start: (i32,i32), goal: (i32,i32) }

let bus = SuperQueue::new();
// two workers
let mut w1 = bus.create_actor(); w1.subscribe::<PathJob>(Some(128))?;
let mut w2 = bus.create_actor(); w2.subscribe::<PathJob>(Some(128))?;

let client = bus.create_actor();

// deliver each job to exactly one available worker (non-blocking)
for j in jobs() {
    client.try_send_single(j).ok(); // drop if both queues are full
}
```

### 3) Backpressure with bounded queues

```rust
let mut ui = bus.create_actor();
ui.subscribe::<String>(Some(64))?;   // bounded
// Prefer try_send so producers don't stall the whole app:
producer.try_send("notification".to_string()).ok();
```

---

## Error types (quick reference)

* `SuperQueueError::{NotSubscribed, AlreadySubscribed, EmptyQueue}`
* `SendError::NoSubscribers`
* `TrySendError::{NoSubscribers, NoSpaceAvailable}`

---

## API overview (selected)

```rust
impl SuperQueue {
    pub fn new() -> Self;
    pub fn create_actor(&self) -> SuperQueueActor;
}

impl SuperQueueActor {
    // subscriptions
    pub fn subscribe<T>(&mut self, bounds: Option<usize>) -> Result<(), SuperQueueError>;
    pub fn unsubscribe<T>(&mut self) -> Result<(), SuperQueueError>;

    // send
    pub fn send<T>(&self, data: T) -> Result<(), SendError>;
    pub fn try_send<T>(&self, data: T) -> Result<(), TrySendError>;
    pub fn send_single<T>(&self, data: T) -> Result<(), SendError>;
    pub fn try_send_single<T>(&self, data: T) -> Result<(), TrySendError>;

    // read
    pub fn read<T>(&self) -> Result<std::sync::Arc<T>, SuperQueueError>;
    pub fn try_read<T>(&self) -> Result<std::sync::Arc<T>, SuperQueueError>;
}
```

---

## License

MIT © alexdesander

---

## Credits

Built on top of the excellent `crossbeam_channel`.
