# SuperQueue

A tiny, lock-light, **type-routed message bus** for Rust.

Primary use cases: fast, ergonomic state/event dispatch for **game development** (systems & actors exchanging events and sampling shared game state), background workers, UI event routing, and modular plugin systems.

> ⚠️ **Blocking caution:** The blocking send/receive variants can deadlock if you create cyclic waits or receivers do not drain. Prefer the non-blocking `try_*` calls where appropriate.

---

## Highlights

* **Type-based routing** – you work with concrete `T`; messages are erased internally as `Arc<dyn Any + Send + Sync>` and downcast on read.
* **Two complementary primitives, one API surface:**

  * **Event streams** – classic per-subscriber queues (broadcast or single-consumer), optionally **bounded** for backpressure.
  * **Latest-value topics** – a **single, always-overwritable slot per type** that every actor can sample independently (**one observation per update**).
* **Backpressure (streams)** – per-subscription bounded (including rendezvous `Some(0)`) or unbounded queues.
* **Simple ownership** – `SuperQueueActor` unsubscribes itself in `Drop`.
* **Cheap cloning** – the bus is shared and `Clone`.

This crate is not `no_std`.

---

## Install

```toml
# Cargo.toml
[dependencies]
superqueue = "0.1"
```

---

## Quick starts

### 1) Event stream (broadcast)

```rust
use superqueue::SuperQueue;

let bus = SuperQueue::new();
let mut recv = bus.create_actor();
let send = bus.create_actor();

recv.subscribe::<String>(None)?;           // unbounded queue
send.send("Hello".to_string())?;           // broadcast (may block if a bounded queue is full)

let msg = recv.read::<String>()?;          // blocking read
assert_eq!(&*msg, "Hello");
```

### 2) Latest-value topic (snapshot)

```rust
use superqueue::SuperQueue;

let bus = SuperQueue::new();
let publisher = bus.create_actor();
let mut reader = bus.create_actor();

// No subscription needed for latest-value topics.
assert!(reader.read_latest::<u32>().is_none()); // nothing published yet

publisher.update_latest::<u32>(1);
assert_eq!(*reader.read_latest::<u32>().unwrap(), 1); // sees the new value once
assert!(reader.read_latest::<u32>().is_none());       // at most once per update

publisher.update_latest::<u32>(2);
assert_eq!(*reader.read_latest::<u32>().unwrap(), 2);
```

### 3) Mixing both

```rust
# use superqueue::SuperQueue;
let bus = SuperQueue::new();
let mut physics = bus.create_actor();
let ai = bus.create_actor();

// Physics consumes events...
physics.subscribe::<(u32, u32)>(Some(256))?; // position updates as events

// ...and also samples a latest snapshot AI publishes opportunistically.
ai.update_latest::<f32>(0.016); // delta time in seconds
```

---

## Core concepts

* A **queue** (`SuperQueue`) is shared and cheap to clone.
* An **actor** (`SuperQueueActor`) can:

  * **Streams:** subscribe to `T` and send/read events of `T`.
  * **Latest:** publish with `update_latest<T>(value)` and sample with `read_latest::<T>() -> Option<Arc<T>>` (no subscription required).

**Keying:**

* Stream subscriptions are keyed by `(TypeId, ActorId)` and create a **private channel** per subscriber.
* Latest-value topics are keyed by `TypeId` **only** and hold **one slot** for the entire bus. Each actor maintains its own **cursor** to observe each update **at most once**.

---

## Choosing an API

### Sending (streams)

| Method               | Delivery                                | Blocking                                                                               | Returns / Errors                                                                                         |
| -------------------- | --------------------------------------- | -------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `send(T)`            | Broadcast to **all** subscribers of `T` | May block per receiver if that receiver’s queue is bounded and full                    | `Ok` or `SendError::NoSubscribers`                                                                       |
| `try_send(T)`        | Broadcast, **non-blocking**             | Never blocks; per-receiver enqueue attempts may be dropped if full                     | `Ok` or `TrySendError::{NoSubscribers, NoSpaceAvailable}` (*NoSpaceAvailable* only if **none** accepted) |
| `send_single(T)`     | **Exactly one** subscriber              | Prefers a subscriber with capacity; if all are full, **blocks on a random subscriber** | `Ok` or `SendError::NoSubscribers`                                                                       |
| `try_send_single(T)` | Exactly one, **non-blocking**           | Never blocks; drops the message if everyone is full                                    | `Ok` or `TrySendError::{NoSubscribers, NoSpaceAvailable}`                                                |

> Tip: `Some(0)` creates a **rendezvous** channel. `try_send*` will always return `NoSpaceAvailable` unless a receiver is waiting; `send*` will rendezvous (and may block).

### Latest-value topics

| Method                                 | Semantics                                                                                      | Blocking |
| -------------------------------------- | ---------------------------------------------------------------------------------------------- | -------- |
| `update_latest(T)`                     | Overwrite the single slot for type `T`. **Last-writer-wins.**                                  | Never    |
| `read_latest::<T>() -> Option<Arc<T>>` | Return the **newest value once per actor per update**; `None` if unchanged or never published. | Never    |

---

## Behavior & guarantees

* **Per-type FIFO (streams):** For a given actor and type `T`, message order matches send order.
* **Arc cloning:** Broadcast is cheap; the bus clones `Arc<T>` per subscriber.
* **Drop safety:** Dropping an actor unsubscribes all of its stream subscriptions; subsequent sends won’t panic due to closed channels.
* **No replay (streams):** Messages sent while an actor is **unsubscribed** are not queued for it.
* **Latest-value topics:**

  * **One slot per `TypeId`** across the bus; **no history** is kept.
  * Updates **coalesce**; intermediate values may be skipped by readers.
  * **Per-actor cursors** ensure each actor observes at most one value per update.
  * **No subscription required** to publish or read latest values.
  * **Never blocks**.

---

## Patterns

### Broadcast events to many systems

```rust
#[derive(Clone)]
struct PlayerMoved { id: u32, x: f32, y: f32 }

let bus = SuperQueue::new();
let mut physics = bus.create_actor();
let mut audio   = bus.create_actor();

physics.subscribe::<PlayerMoved>(None)?;
audio.subscribe::<PlayerMoved>(None)?;

let tx = bus.create_actor();
tx.try_send(PlayerMoved { id: 1, x: 4.0, y: 2.0 })?; // non-blocking
```

### Single-consumer worker pool

```rust
#[derive(Clone)]
struct PathJob { start: (i32,i32), goal: (i32,i32) }

let bus = SuperQueue::new();
// two workers
let mut w1 = bus.create_actor(); w1.subscribe::<PathJob>(Some(128))?;
let mut w2 = bus.create_actor(); w2.subscribe::<PathJob>(Some(128))?;

let client = bus.create_actor();

// deliver each job to exactly one available worker (non-blocking)
for job in jobs() {
    let _ = client.try_send_single(job); // drop if both queues are full
}
```

### Backpressure with bounded queues

```rust
let mut ui = bus.create_actor();
ui.subscribe::<String>(Some(64))?;   // bounded
producer.try_send("notification".to_string()).ok(); // avoid stalling producers
```

### Global snapshots with latest-value topics

```rust
// Publish "frame delta" as a coalescing snapshot:
let publisher = bus.create_actor();
publisher.update_latest::<f32>(0.016);

// Any system can sample once per update without subscribing:
let mut consumer = bus.create_actor();
if let Some(dt) = consumer.read_latest::<f32>() {
    // use *dt
}
```

---

## Error types (reference)

* `SuperQueueError::{ NotSubscribed, AlreadySubscribed, EmptyQueue }`
* `SendError::NoSubscribers`
* `TrySendError::{ NoSubscribers, NoSpaceAvailable }`

---

## License

MIT © alexdesander
