# Async Runtime

## 1 Introduction

### 1.1 Rust

Rust is a general-purpose programming language that is exceptional for developing safe, parallelized, system-level code. At compile-time, it can identify many memory and concurrency bugs that would otherwise plague programs written in other languages. This is possible thanks to Rust’s *borrow-checker*. Every value has an “owning scope,” which starts and ends around curly braces. Passing or returning values between scopes means transferring ownership. Values can also be *borrowed* by another scope, and returned to their owner at a later point. These happen via either mutable or immutable borrows. If a value is mutably borrowed, no other scope is allowed to borrow that value, effectively allowing for the detection and prevention of data races at compile time. This is just one example of the constructs provided by Rust that enable the language’s self-proclaimed *fearless concurrency,* and motivate Rust as a choice of language for this project.

### 1.2 Asynchronous Runtimes

An asynchronous runtime is a library that executes async applications. Rather than running these on different threads, async runtimes allow for a single thread to advance tasks, pending each upon encountering slow asynchronous I/O operations. Pended tasks can then be picked back up once the I/O operation has completed, maximizing efficiency. One common way of implementing an async runtime is by combining a reactor with one or more executors. The *Reactor* is a design pattern that provides a subscription mechanism for asynchronous events received from the operating system. These events are delivered concurrently, demultiplexed, and dispatched to the appropriate handler. The *Executor* is a design pattern responsible for handling the scheduling and execution of tasks.

Rust does not provide an async runtime in its standard library. Instead, users can pick and choose from a swathe of open-source libraries (dubbed “crates” in Rust) based on their desired application. The goal of this project is to write an async runtime environment in Rust. This involves developing a task model, a reactor that can accept task subscriptions, an executor for handling tasks dispatched by the reactor, and the language constructs required for this runtime to operate seamlessly in an async Rust application. A sample Rust application is provided that spawns client threads to communicate with a server via TCP.

## 2 Rust Futures

This section will explore the basics of Rust semantics like structs, traits, borrows, and concurrency primitives, in the context of the most important async primitive: the Future.

### 2.1 Towards Rust Futures

Rather than the traditional inheritance model employed in languages like C++, Rust opts for a Composition over Inheritance model. Rust ******traits****** define functionality that a type can share with other types.  The `Future` trait defines a type that can eventually produce a value. The easiest way of writing a future is with an `async` block. Inside this block, we can `await` other futures, as follows:

```rust
let future = async {
    some_asynchronous_operation.await();
};
```

To build towards an understanding of how Rust implements this trait, let’s attempt to implement this ourselves. First of all, we need this trait to provide functionality that advances the future as far towards completion as possible. If the future successfully completes, we can return the value it produced. Otherwise, we should put the future to sleep and wake it up when more progress can be made. The signature could look something like this:

```rust
trait MyFuture {
    type Output;
		// Mutably borrow the object with `&mut self` so polling can modify it.
		// Provide a `wake` function that can be registered as a callback.
		// Either return a value wrapped inside `Poll::Ready`, or a pending status.
    // `poll` should be invoked from a ready queue of tasks.
    fn poll(&mut self, wake: fn()) -> Poll<Self::Output>;
}

enum Poll<T> {
    Ready(T),
    Pending,
}
```

Remember that Rust seeks to avoid many of the. This means additional constructs need to be added to `MyFuture` in order to make it bulletproof against potential mishandling. These will be explored in the following sections.

### 2.2 Wakers and Channels

One problem with this signature is that the `wake` function has no idea which future actually called it. Indeed, notice `wake` is a function taking no arguments, with no return value. What we really need is for `wake`to be some kind of `Waker` object that retains information about the specific task we’re running. For a task to become a `Waker`, let’s define a `MyWake` trait it should implement.
		

```rust
trait MyWake {
		fn wake(self: Self);
}
```

Our task struct should hold a future instance that can be sent between threads, a unique task identifier, as well as a mechanism for re-adding itself to a ready queue once it’s been woken up. To accomplish this, let’s use Rust’s message-passing primitive, a Multiple-Producer Single-Consumer channel. This task struct can be implemented as follows:

```rust
use std::sync::mpsc;

// Task struct, holding a Future instance and a ready_queue enqueuer.
struct MyTask {
    // Since futures are a trait, we need to store an object implementing this
    // trait via dynamic dispatch (using the `dyn` keyword) on the heap (using `Box`).
    // We want this future to be sent between threads, so we must require it
    // implements the `Send` trait.
    future: Box<dyn MyFuture<Output = ()> + Send>,
    // Unique identifier for this task.
    token: usize
		// Multiple-Producer Single-Consumer channel.
		// Task IDs are sent through this ready queue to be scheduled.
    // We use a `SyncSender` as opposed to a regular `Sender` to allow for sharing
    // the sender between threads.
    ready_queue: mpsc::SyncSender<usize>
}
```

We can implement the `MyWake` trait for our task as follows:

```rust
// Implement the MyWake trait for MyTask.
impl MyWake for MyTask {
    fn wake(self: Self) {
        // We can't use `self.ready_queue` and pass `self` to the channel
        // at once due to Rust's single-ownership, so let's create a copy
        // of the channel.
        let queue = self.ready_queue.clone();
        queue.send(self).expect("Failed to send task to `ready_queue`");
    }
}
```

### 2.3 Shared-State Concurrency

This implementation works just fine, but has a significant drawback: only one reference to `MyTask` can exist at a time. To understand this, we need to talk a bit more about Rust’s ownership system. Specifically, since `MyTask` holds a `Box` instance, i.e. a heap-allocated pointer, only one copy of this pointer is allowed at a time. Otherwise, there’s the possibility of race conditions due to a shared pointer. This fact is explicit in the language’s syntax: `Box` doesn’t implement the `Copy` trait, and therefore `MyTask` doesn’t implement the `Copy` trait, and therefore a task instance can’t be duplicated. Instead, when we’re sending this task to `ready_queue`, we actually send an owning instance. This is different from our `mpsc::SyncSender` instance: since we’re working with a Multiple-Producer, Single-Consumer channel, it’s completely fine to duplicate sender instances, as shown via the `.clone()` method

However, notice we’ll likely be working with multiple instances of a single `MyTask`. Remember, Rust does not normally allow shared mutable references to a value. Therefore, if we’re passing an instance of this task to a worker thread, it’d be cumbersome if this same worker thread was also responsible for spuriously waking itself up when more progress can be made towards completing the `future`. Therefore, it’d be helpful to work with thread-safe shared references to `MyTask`, as opposed to an owning instance. We can start by enabling mutual exclusion on our boxed future, as follows:

```rust
use std::sync::Mutex;

struct MyTask {
    // Wrap our boxed future in a Mutex, enabling mutually-exclusive access.
    future: Mutex<Box<dyn MyFuture<Output = ()>>>,
		// ...
}
```

Now, let’s allow shared references using Rust’s atomic reference counter, `Arc`:

```rust
use std::sync::{mpsc, Arc};

trait MyWake {
		fn wake(self: Arc<Self>);
}

impl MyWake for MyTask {
    fn wake(self: Arc<Self>) {
        // Create a shared reference to `MyTask`.
        let clone = Arc::clone(&self);
        self.ready_queue.send(clone).expect("Failed to send task to `ready_queue`");
    }
}
```

So our new definition of Future becomes:

```rust
trait MyFuture {
    type Output;
    // `waker` now takes in a struct implementing the `Wake` trait.
    fn poll(&mut self, waker: impl Wake) -> Poll<Self::Output>;
}
```

### 2.4 Pinning

Note that the `MyTask` struct holds an owning instance to a mutually-exclusive pointer to a struct implementing the `MyFuture` trait. As long as the value remains boxed at the same memory location, it’ll be valid. So what happens if we move this memory location? Here’s an example:

```rust
// Create a mutually-exclusive shared reference to an option holding a future
let future_ref = Arc::new(Mutex::new(Box::new(Some(async {println!("hello world")}))));
// Create a new reference to this future
let cloned_ref = Arc::clone(future);

// Spawn a new thread, and give it ownership of cloned_ref.
let handle = thread::spawn(move || {
    let future = cloned_ref
        .lock().unwrap()    // Retrieve a mutex guard to the future option.
        .take().unwrap();   // Take ownership of future, leaving behind a None.
    // `future` goes out of scope and gets dropped.
});

handle.join();    // Wait for the new thread to finish running.
let future = future_ref
    .lock().unwrap()     // Retrieve a mutex guard.
    .take().unwrap();    // The future was taken from the option, so this panics!
```

It’d be problematic if threads sharing a reference to a future could arbitrarily decide to take ownership of that future. Rust provides a special type called `Pin<T>` that pins a value to memory, preventing it from being moved around. In order to fix the above code, we could replace the call to `Box::new` with `Box::pin`. This would return a `Pin<Box<T>>` , causing the above code to fail compiling. We’ve now successfully caught the move at compile-time, preventing potential runtime panics.

Similarly, we’d like the `Box` holding `MyFuture` to be pinned in memory, as follows:

```rust
struct MyTask {
    future: Mutex<Pin<Box<dyn MyFuture<Output = ()> + Send>>>,
    // ...
}
```

And we’d like `MyFuture` to ensure we can only call `poll`when it is indeed pinned:

```rust
trait MyFuture {
    type Output;
    fn poll(self: Pin<&mut self>, wake: impl Wake) -> Poll<Self::Output>;
}
```

### 2.5 Finishing Touches

This is almost a complete view of Rust Futures, and the properties that enable their thread-safety at compile time. The actual signature is as follows:

```rust
trait Future {
    type Output;
    fn poll(self: Pin<&mut self>, wake: &mut Context<'_>) -> Poll<Self::Output>;
}
```

Here, `Context` is simply a wrapper around a `Waker` object. A `Waker` is created from a type implementing the `Wake` trait, such as the `MyTask` object from earlier. The idea here is to only provide the information necessary to wake the object, rather than the entire `MyTask` struct. We now have a complete understanding of how Rust implements its futures in order to be bulletproof against potential runtime errors!

## 3 I/O Events

As outlined in Section 2.1, in order to implement a future, we need some way of pending a running future if `poll` returns `Poll::Pending` , and picking it back up once progress can be made. One way of doing this would be to continually poll until we receive a `Poll::Ready<T>`, but this would lock our thread until the future is ready — not an ideal situation. Instead, we can use OS-level I/O primitives that allow a thread to block on I/O events and return once they’ve completed.

### 3.1 Epoll

Linux uses the `epoll` family of system calls to monitor I/O events. These are provided by the kernel with the following signatures:

```c
int epoll_create(int size);
int epoll_ctl(int epfd, int op, int fd, struct epoll_event *event);
int epoll_wait(int epfd, struct epoll_event *events, int maxevents, int timeout);
int close(int fd);
```

- `epoll_create` initializes an epoll queue, and returns it as a file descriptor. This should be called once by our program.
- `epoll_ctl` is used to add, modify, and remove entries from our epoll’s interest list. This means performing an add/modify/remove operation `op` on a file descriptor `fd` of interest, in our epoll queue `epfd`.
- `epoll_wait` suspends the current thread until a file descriptor in our epoll queue `epfd` triggers an event.
- `close` is not an epoll system call, but rather the general-purpose system call for closing a file descriptor. This should be called at the end of the program on the descriptor returned by `epoll_create`.

The `struct epoll_event` is defined as follows:

```c
#define EPOLL_PACKED __attribute__((packed))

typedef union epoll_data {
   void    *ptr;
   int      fd;
   uint32_t u32;
   uint64_t u64;
} epoll_data_t;

 struct epoll_event {
     uint32_t     events;    /* Epoll events */
     epoll_data_t data;      /* User data variable */
 } EPOLL_PACKED;
```

- `events` is a bit mask indicating which event we’re interested in observing of a monitored file descriptor, as well as additional properties such as when we’d like the event to trigger. Events include when the file is ready to be read or written. Trigger conditions can be level-triggered (i.e. notify as long as the event is ready) edge-triggered (i.e. notify once the event switches states from pending to ready), or one-shot (i.e. only notify the first time the event triggers).
- `data` specifies what the kernel should return once an event is triggered in `epoll_wait`. This is helpful to store, for example, the ID of the task corresponding to this event.

In-kernel, epoll boils down to a set of file descriptors that are monitored for events. When a program creates an epoll instance using the **`epoll_create`** system call, the kernel allocates an epoll instance data structure and returns a file descriptor that represents the epoll instance. The program can then use the file descriptor to manipulate the epoll instance using the **`epoll_ctl`** and **`epoll_wait`** system calls.When an event occurs on one of the file descriptors, the kernel adds an entry to the list of events for that file descriptor. 

When the program calls **`epoll_wait`**, the kernel checks the list of events for each file descriptor in the epoll instance. If there are any events that match the events the program is interested in, the kernel adds them to a list of events that is returned to the program. The kernel uses a red-black tree to store the file descriptors in the epoll instance. This allows the kernel to efficiently add and remove file descriptors from the epoll instance, as well as quickly search for events on specific file descriptors.

### 3.2 Using Epoll in Rust

Notice these system calls are provided by the Linux kernel in C. Therefore, in order to invoke these system calls in Rust, we have to provide a Foreign-Function Interface (FFI) layer. Notice the `epoll_data` union has a maximum size of 64 bits (the largest data members are `uint64_t` and `void *`). Therefore, we can get away with a `usize` rather than a union in our FFI. 

Once we’ve implemented our FFI, it’s helpful to provide `safe` bindings around these modules. Rust can operate under two modes: `safe` and `unsafe`. `unsafe` loses many of the memory-safety guarantees imposed by the language, but is required for operations the compiler isn’t aware of (notably, FFI). Therefore, it’s also helpful to provide `safe` bindings for our `unsafe` FFI to facilitate usage.

That’s about the extent of decision-making involved for realizing this FFI. The rest is plenty of boilerplate, so for the sake of brevity I’ll omit further explanations. See the `epoll` and `epoll::ffi` modules under `async_runtime/src/lib.rs` for more information.

### 3.3 TCP I/O Events

TCP is a transport-layer protocol that provides end-to-end packet delivery between two hosts. In Linux, this is implemented by reserving a file descriptor. TCP communication is initiated via a handshake, at which point the file descriptor representing this connection is marked as readable and writeable. If an event listener was set up to wait for the connection, it’s at this point that  `epoll_wait` would return. 

When network packets arrive they trigger an interrupt that traps into the kernel. The kernel reads in the data that arrived on the network card and writes it to the file descriptor. The file descriptor is then marked as readable. If this event has been placed in epoll's interest list,  `epoll_wait` is now ready to return. Writing out network packets happens in similar fashion. The user can write out data into the file descriptor. Once this data gets copied out by the kernel to the network card, the file descriptor is marked as readable and data is forwarded by the network card to the appropriate address.

## 4 Development

In Section 2.1, we saw that futures can only be awaited inside `async` blocks. This means that, in order to invoke our future from synchronous code, we need to implement a special intermediary layer. This is the so-called asynchronous runtime discussed in Section 1.2. The following bullets provide a high-level summary of the four modules provided under `async_runtime/src/lib.rs`, that are used to realize our asynchronous runtime.

- `epoll`: provides the FFI and safe bindings for invoking the Linux epoll system calls.
- `blockers`: provides an `IoBlocker` class which interacts with `epoll` module. It also provides a `Registrator`, which is used to register new events to `epoll` .
- `services`: this implements a `TcpStream` with asynchronous connecting, reading, and writing.
- `runtime`: this implements our `Task`, `Executor` and `Reactor`.

The following sections expand on the above modules in greater detail.

### 4.1 I/O Blocker and Registrators

In order to build up to a Reactor/Executor design pattern, we’ll need a few lower-level primitives. It’d be helpful to encapsulate the behavior for creating, waiting for, and destroying an `epoll` instance into a single object. This is the purpose of the `IoBlocker` struct, provided in the `blockers` module under `async_runtime/src/lib`. But there should only be a single `IoBlocker` for a given `epoll` instance, since `IoBlocker` is responsible for creating and destroying its file descriptor. However, we need some way for multiple different I/O events to add themselves to an `epoll` instance if they need to be pended. For this reason, we need a class that can be easily duplicated such that each task can own its own copy. This is the `Registrator`, and its implementation can be found together with the `IoBlocker` but deserves some additional deliberation.

The `Registrator` provides a `register()` function, which takes in a file descriptor of interest and registers an event that should trigger when this file descriptor becomes readable. This should clearly call `epoll_ctl` with an add operation (`libc::EPOLL_CTL_ADD`), and we want to listen for this file to become readable (`libc::EPOLLIN`).  Note the usage of the `libc` crate to access these enum values. However, it’s unclear when exactly we want this event to trigger. 

Thinking back to the description of the `events` field in Section 3.1, level-triggered would mean continuously receiving events until we’ve appropriately handled the event and the file descriptor is no longer readable. This doesn’t seem ideal, as we would benefit from a one-to-one correspondence between an event being triggered and our task performing its pended operation. Edge-triggered would mean receiving events both from when the file becomes readable and becomes unreadable. Since we don’t care about the latter event, we can instead use a one-shot policy. Once the file descriptor becomes readable, the event listener automatically disarms. We can manually rearm the listener with the `libc::EPOLL_CTL_MOD` operation if there’s still more work to be done — otherwise, the event is automatically disabled for us.

### 4.2 Task, Reactor and Executor

Our task model is identical to that described in Section 2.2. The next step involves implementing the actual ready queue employed by this task model. This is done inside the `Executor`, which provides methods for adding tasks to its queue, and actually running readied tasks. Initially, tasks should be added directly to an `Executor` instance, seeing as how initially every future can make progress. This saves the mapping from task ID to task. When the `Executor` instance begins running, task IDs are consumed from the ready queue, and the corresponding task is retrieved from our mapping. If this task finishes running, we can get rid of it from our mapping. If the task hasn’t finished, there’s nothing the executor can do.

In the background, if a task is still pending, this means it should’ve automatically registered itself to our `epoll` instance through a `Registrator` instance. This is where our `Reactor` comes in: it spawns a thread that sleeps until our `IoBlocker` instance finishes blocking. At this point, events are ready to be processed and sent to the executor. Note we employed the `data` field of our event to write this task’s token ID. This is why we send task IDs through our ready queue, as opposed actual instances of the `Task` struct: this is the only information our `Reactor` receives from epoll.

### 4.3 TCP Stream

To test this asynchronous runtime, I’ve chosen to develop a client that creates multiple concurrent TCP connections to communicate with an external server. Rust provides a `TcpStream` that allows for connecting, reading and writing to a remote TCP host. However, this implementation is entirely synchronous, so we need to implement our own connection, reading, and writing functionality.

Linux provides the `socket` family of system calls to initialize an endpoint for TCP communication. Much like `epoll_create`, the returned socket is a file descriptor. This means it can be added to epoll’s interest list using `epoll_ctl`. In order to manipulate our socket, let’s use some abstractions. Rather than implementing our own FFI bindings, let’s leverage the existing crate `socket2` . This mimics much of the work we did under the `epoll` module of `async_runtime/src/lib.rs`, but in the context of a socket. 

In order to implement an asynchronous `TcpStream`, the first is to write asynchronous functionality for connecting to a remote host. We can create a non-blocking socket instance by passing the relevant flags to the `socket` system call, and then connect it to a remote host using the `connect` system call. Since we marked the socket as non-blocking, this call returns instantly, either indicating that it’s ready or pending. The connection then resumes as an I/O operation in the background. Therefore, we shouldn’t invoke `connect` directly, but rather `await` from a type that implements `Future` , in order to invoke `connect` from the Future’s `poll` function. 

Note that to connect to a socket, we need a reference to the socket and the address we’re connecting to. Additionally, to pend our execution, we need to register it to an epoll instance, and indicate which specific task is being pended via a unique token identifier. From this, the struct signature should be fairly straightforward:

```rust
pub struct ConnectStreamFuture<'a> {
		socket: &'a Socket,
		addr: SockAddr,
		registrator: Registrator,
		token: usize,
}
```

From this, we can implement our future much alike described in Section 2:

```rust
impl Future for ConnectStreamFuture<'_> {
    // Specify that we either receive no result (i.e. success), or
    // we received an error.
    type Output = Result<(), io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.socket.connect(&self.addr) {
		        // Finished connecting, done polling.
		        Ok(_) => Poll::Ready(Ok(())),
		        // Since we set this TcpStream to non-blocking, check if
		        // return value still not ready.
		        Err(e)
		            if e.raw_os_error() == Some(libc::EINPROGRESS)
		                || e.kind() == io::ErrorKind::WouldBlock =>
		        {
                // If not ready, register the socket's file descriptor to epoll.
		            self.registrator.register(self.socket, self.token).unwrap();
		            Poll::Pending
		        }
		        // Error occurred, done polling.
		        Err(e) => Poll::Ready(Err(e)),
        }
    }
}
```

Implementing asynchronous reading and writing follows a very similar procedure to above. Therefore, to save time on implementing two more futures, I’ve instead opted to use the `AsyncRead` and `AsyncWrite` traits of the `futures` crate. These can be implemented for our `TcpStream` struct by providing a `poll_read` and `poll_write` methods respectively. The traits then automatically generate awaitable futures we can use in our code for reading and writing.  

### 4.4 Client-Server Model

The above data structures allows us to fully realize a client in Rust that can connect to a remote endpoint, and generate multiple futures that can be asynchronously advanced using only a single thread. A sample client is implemented in `client/src/main.rs`, and it can be invoked by typing the following command inside the `client` directory:

```rust
cargo run -- <number_of_clients>
```

This spawns `number_of_clients` clients that perform asynchronous connecting, reading, and writing to a local server. Note this requires Rust to be installed on your system ([instructions here](https://www.rust-lang.org/tools/install)). To circumvent potential networking issues, we spawn server instances on the local machine using Bash’s `nc` . This is provided in the Bash script `server.bash` , which can be invoked as follows:

```rust
./server.bash <number_of_servers>
```

Note that `nc` is fairly limited, and only allows for one connection per instance. Therefore, multiple `nc` instances are spawned using ports `3000` through `3000 + number_of_servers`. Equivalently, each `TcpStream` instance in our client also connects to a different port. This is not generally necessary, and our implementation does allow for multiple connections to the same port, but proved necessary for demonstration purposes using `nc` . 

Note that the server should be running by the time clients are spawned and attempt to connect. However, servers spawned with `nc` only sends a reply when they’re closed, so the Bash script provides a small delay to receive data from the clients and then kills the spawned servers. Therefore, there’s a small window of time during which a server and clients can be run, otherwise the program won’t work. To get around this, a convenience script `spawn_client_server.bash` is provided, which performs all of the above operations and can be invoked as follows:

```rust
./spawn_client_server.bash <number_of_client-server_pairs>
```

Sample runs with 1, 10 and 100 client-server pairs are provided in the `out` directory. Lines starting with `<task_id>:` are printed by the Rust client. They print out the time taken to connect to, send information to, and receive information from the server. On the next line, they print the message received from the server. Lines without `<task_id>:` are printed by the server, and print the message received by a client. Notice the time taken to receive information from the server is around 4 seconds, which is proportional to the time of the small delay described above.

### 4.5 Reflection

This project proved significantly more challenging than I’d initially envisioned. Understanding why certain things are implemented the way they are (particularly `Future` and `epoll`) took a lot of time, so hopefully the above explanations provide a good description of what I’ve learned. The final outcome is a single-threaded asynchronous server that coexists alongside many Rust libraries such as `tokio`, `async_std` and `async_io`, and provides the structures necessary to asynchronously connect, read, and write to a TCP endpoint.

## 5 References

[https://rust-lang.github.io/async-book/](https://rust-lang.github.io/async-book/)

[https://cfsamson.github.io/book-exploring-async-basics/](https://cfsamson.github.io/book-exploring-async-basics/)

[https://cfsamsonbooks.gitbook.io/epoll-kqueue-iocp-explained/](https://cfsamsonbooks.gitbook.io/epoll-kqueue-iocp-explained/)

[https://os.phil-opp.com/async-await/](https://os.phil-opp.com/async-await/)

[https://docs.rs/async-std/latest/async_std/index.html](https://docs.rs/async-std/latest/async_std/index.html)

[https://docs.rs/async-io/latest/async_io/](https://docs.rs/async-io/latest/async_io/)

[https://tokio.rs/](https://tokio.rs/)
