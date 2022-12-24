// Module providing safe bindings around unsafe epoll ffi.
mod epoll {
    use std::io;

    // Module providing unsafe ffi for Linux's epoll and epoll-adjacent syscalls.
    mod ffi {
        #[link(name = "c")]
        extern "C" {
            // https://man7.org/linux/man-pages/man2/epoll_create.2.html
            pub fn epoll_create(size: i32) -> i32;
            // https://man7.org/linux/man-pages/man2/close.2.html
            pub fn close(fd: i32) -> i32;
            // https://man7.org/linux/man-pages/man2/epoll_ctl.2.html
            pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut super::Event) -> i32;
            // https://man7.org/linux/man-pages/man2/epoll_wait.2.html
            pub fn epoll_wait(
                epfd: i32,
                events: *mut super::Event,
                maxevents: i32,
                timeout: i32,
            ) -> i32;
        }
    }

    pub fn create() -> io::Result<i32> {
        // As of Linux 2.6.8, the size argument is ignored but has to be >= 0.
        let res = unsafe { ffi::epoll_create(1) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }

    pub fn close(fd: i32) -> io::Result<()> {
        let res = unsafe { ffi::close(fd) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn ctl(epfd: i32, op: i32, fd: i32, event: &mut Event) -> io::Result<()> {
        let res = unsafe { ffi::epoll_ctl(epfd, op, fd, event) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn wait(epfd: i32, events: &mut [Event], maxevents: i32, timeout: i32) -> io::Result<i32> {
        let res = unsafe { ffi::epoll_wait(epfd, events.as_mut_ptr(), maxevents, timeout) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }

    #[derive(Debug, Clone, Copy)] // Helpful default traits.
    #[repr(C, packed)] // packed struct used in C, so make sure to specify.
    pub struct Event {
        events: u32,
        token: usize,
    }

    impl Event {
        pub fn new(events: i32, token: usize) -> Self {
            Event {
                events: events as u32,
                token,
            }
        }
        pub fn token(&self) -> usize {
            self.token
        }
    }
}

// Interfaces with epoll to provide OS-level I/O polling.
pub mod blockers {
    use crate::epoll;
    use std::io;
    use std::os::unix::io::{AsRawFd, RawFd};

    pub struct IoBlocker {
        fd: RawFd,
    }

    // Abstraction around `epoll`. Allows for blocking until an I/O event is ready.
    impl IoBlocker {
        // Initialize an `epoll` instance.
        pub fn new() -> io::Result<Self> {
            match epoll::create() {
                Ok(fd) => Ok(IoBlocker { fd }),
                Err(e) => Err(e),
            }
        }

        // Generate a registrator that can register events to this IoBlocker's epoll.
        pub fn registrator(&self) -> Registrator {
            Registrator { fd: self.fd }
        }

        // Block until an event is available.
        pub fn block(&self, events: &mut Vec<epoll::Event>) -> io::Result<i32> {
            const TIMEOUT: i32 = -1; // Wait forever.
            const MAX_EVENTS: i32 = 1024;
            events.clear();

            // println!("IoBlocker::block --> begin blocking");
            let n_events = epoll::wait(self.fd, events, MAX_EVENTS, TIMEOUT)?;
            // Rust has no way of knowing events vec changed size, since
            // this happened in unsafe FFI world. Forcefully change size here.
            // Note this should never introduce problems, unless the OS
            // did something wrong.
            unsafe { events.set_len(n_events as usize) };
            // println!("IoBlocker::block --> done waiting for events {:?}", events);
            Ok(n_events)
        }
    }

    // Ensure that we close held `epoll` file descriptor.
    impl Drop for IoBlocker {
        fn drop(&mut self) {
            // println!("IoBlocker::drop --> dropping");
            epoll::close(self.fd).unwrap();
        }
    }

    // There should only be a single IoBlocker for a given `epoll` instance,
    // since IoBlocker is responsible for creating and destroying its file descriptor.
    // However, we need some way for multiple different I/O events to register
    // themselves to an `epoll`. For this reason, we need a class that can be
    // duplicated such that each task can own its own copy. This is the Registrator.
    #[derive(Debug, Clone, Copy)] // Helpful default traits.
    pub struct Registrator {
        fd: RawFd,
    }

    impl Registrator {
        // Register a file descriptor of interest.
        pub fn register(&self, interest: &impl AsRawFd, token: usize) -> io::Result<()> {
            let fd = interest.as_raw_fd();

            // Listen for file to become readable.
            let mut event = epoll::Event::new(libc::EPOLLIN | libc::EPOLLONESHOT, token);
            match epoll::ctl(self.fd, libc::EPOLL_CTL_ADD, fd, &mut event) {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // one-shot epoll consumed, so needs to be re-armed.
                    epoll::ctl(self.fd, libc::EPOLL_CTL_MOD, fd, &mut event)
                }
                Err(e) => Err(e),
            }
        }

        pub fn unregister(&self, interest: &impl AsRawFd, token: usize) -> io::Result<()> {
            let fd = interest.as_raw_fd();
            let mut event = epoll::Event::new(libc::EPOLLIN | libc::EPOLLET, token);
            match epoll::ctl(self.fd, libc::EPOLL_CTL_DEL, fd, &mut event) {
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            }
        }
    }
}

// Provides future API for awaitable services. For use by external crates.
// In this case, the only provided service is a TcpStream.
pub mod services {
    use crate::blockers::{self, Registrator};
    use futures::io::{AsyncRead, AsyncWrite};
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    use std::io::{self, Error, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::{
        future::Future,
        net,
        pin::Pin,
        task::{Context, Poll},
    };

    pub struct TcpStream {
        // We use the non-async TcpStream as a starting point for our implementation.
        inner: net::TcpStream,
        registrator: blockers::Registrator,
        token: usize,
    }

    pub struct ConnectStreamFuture<'a> {
        socket: &'a Socket,
        addr: SockAddr,
        registrator: Registrator,
        token: usize,
    }

    impl Future for ConnectStreamFuture<'_> {
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
                    self.registrator.register(self.socket, self.token).unwrap();
                    Poll::Pending
                }
                // Error occurred, done polling.
                Err(e) => Poll::Ready(Err(e)),
            }
        }
    }

    impl TcpStream {
        // Asynchronously connect this stream to a socket address.
        // Reference: https://docs.rs/async-io/latest/src/async_io/lib.rs.html#1867
        pub async fn connect(
            addr_generator: impl net::ToSocketAddrs,
            registrator: blockers::Registrator,
            token: usize,
        ) -> io::Result<Self> {
            let sock_type = Type::STREAM.nonblocking();
            let protocol = Protocol::TCP;
            let socket_addr = addr_generator.to_socket_addrs().unwrap().next().unwrap();
            let domain = Domain::for_address(socket_addr);

            let socket = Socket::new(domain, sock_type, Some(protocol))
                .unwrap_or_else(|_| panic!("Failed to create socket at address {}", socket_addr));

            // wait for the socket to become writeable.
            TcpStream::sock_connect(
                &socket,
                SockAddr::from(socket_addr),
                registrator.clone(),
                token,
            )
            .await?;

            let stream = net::TcpStream::from(socket);

            Ok(TcpStream {
                inner: stream,
                registrator,
                token,
            })
        }

        fn sock_connect(
            socket: &Socket,
            addr: SockAddr,
            registrator: Registrator,
            token: usize,
        ) -> ConnectStreamFuture {
            return ConnectStreamFuture {
                socket,
                addr,
                registrator,
                token,
            };
        }
    }

    // Allow calling async read operations on socket.
    impl AsyncRead for TcpStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            let stream = self.get_mut();
            match stream.inner.read(buf) {
                // Finished reading, done polling.
                Ok(n) => Poll::Ready(Ok(n)),
                // Since we set this TcpStream to non-blocking, check if
                // return value still not ready.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Not ready, so pend.
                    stream.registrator.register(stream, stream.token).unwrap();
                    Poll::Pending
                }
                // Error occurred, done polling.
                Err(e) => Poll::Ready(Err(e)),
            }
        }
    }

    // Allow calling async write operations on socket.
    impl AsyncWrite for TcpStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &[u8],
        ) -> Poll<Result<usize, Error>> {
            let stream = self.get_mut();
            match stream.inner.write(buf) {
                // Finished writing, done polling.
                Ok(n) => Poll::Ready(Ok(n)),
                // Since we set this TcpStream to non-blocking, check if
                // return value still not ready.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Not ready, so pend.
                    stream.registrator.register(stream, stream.token).unwrap();
                    Poll::Pending
                }
                // Error occurred, done polling.
                Err(e) => Poll::Ready(Err(e)),
            }
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
            let stream = self.get_mut();
            match stream.inner.flush() {
                Ok(n) => Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.registrator.register(stream, stream.token).unwrap();
                    Poll::Pending
                }
                Err(e) => Poll::Ready(Err(e)),
            }
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
            let stream = self.get_mut();
            // Attempt to close write end of stream.
            match stream.inner.shutdown(Shutdown::Write) {
                Ok(n) => Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.registrator.register(stream, stream.token).unwrap();
                    Poll::Pending
                }
                Err(e) => Poll::Ready(Err(e)),
            }
        }
    }

    // Allows us to retrieve the file descriptor associated with this stream.
    impl AsRawFd for TcpStream {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }
}

// Implements reactor/executor design pattern. For use by external crates.
pub mod runtime {
    use crate::{blockers, epoll};
    use std::{
        collections::HashMap,
        future::Future,
        pin::Pin,
        sync::{mpsc, Arc, Mutex},
        task::{Context, Wake, Waker},
        thread,
    };

    // Task struct, holding a Future instance and a ready_queue enqueuer.
    pub struct Task {
        // Since futures are a trait, we need to store an object implementing this
        // trait via dynamic dispatch (using the `dyn` keyword) on the heap (using `Box`).
        // We want this future to be sent between threads, so we must require it
        // implements the `Send` trait.
        future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
        // Multiple-Producer Single-Consumer channel.
        // Task IDs are sent through this ready queue to be scheduled.
        // We use a `SyncSender` as opposed to a regular `Sender` to allow for sharing
        // the sender between threads.
        ready_queue: mpsc::SyncSender<usize>,
        // Unique identifier for this task.
        token: usize,
    }

    // Reactor struct, responsible for receiving tasks and sending them to the
    // Executor.
    pub struct Reactor {
        registrator: blockers::Registrator,
        // Hold onto join handle for
        handle: std::thread::JoinHandle<()>,
    }

    // Executor struct, responsible for running pending tasks.
    pub struct Executor {
        receiver: mpsc::Receiver<usize>,
        // Map from task token to a suspended task.
        task_map: HashMap<usize, Arc<Task>>,
    }

    impl Task {
        pub fn new(
            future: impl Future<Output = ()> + Send + 'static,
            ready_queue: mpsc::SyncSender<usize>,
            token: usize,
        ) -> Arc<Task> {
            Arc::new(Task {
                future: Mutex::new(Box::pin(future)),
                ready_queue,
                token,
            })
        }
    }

    impl Wake for Task {
        fn wake(self: Arc<Self>) {
            self.ready_queue
                .send(self.token)
                .expect("Failed to send task to `ready_queue`");
        }
    }

    impl Reactor {
        // Create a new Reactor instance.
        pub fn new(sender: mpsc::SyncSender<usize>) -> Reactor {
            let blocker = blockers::IoBlocker::new().unwrap();
            let registrator = blocker.registrator();

            // Spawn a new thread that blocks until an event is ready.
            let handle = thread::spawn(move || {
                let mut events: Vec<epoll::Event> = Vec::with_capacity(1024);
                loop {
                    // Blocks until an event is ready.
                    match &blocker.block(&mut events) {
                        Ok(_) => (),
                        Err(e) => panic!("Poll error: {:?}, {}", e.kind(), e),
                    };
                    for event in &events {
                        let event_token = event.token();
                        // println!("Reactor::new --> got event {}", event_token);
                        sender.send(event_token).unwrap();
                    }
                }
            });
            Reactor {
                registrator,
                handle,
            }
        }

        pub fn registrator(&self) -> blockers::Registrator {
            self.registrator.clone()
        }
    }

    impl Executor {
        // Create a new Executor instance.
        pub fn new(receiver: mpsc::Receiver<usize>) -> Executor {
            let task_map: HashMap<usize, Arc<Task>> = HashMap::new();
            Executor { receiver, task_map }
        }
        // Run all tasks to completion.
        pub fn run(&mut self) {
            // println!("Executor::run --> begin");
            for (_, task) in self.task_map.iter() {
                let waker = Waker::from(Arc::clone(task));
                waker.wake();
            }

            while let Ok(token) = self.receiver.recv() {
                let mut finished = false;
                {
                    // println!("Executor::run --> received {}", token);
                    let task = self
                        .task_map
                        .get(&token)
                        .unwrap_or_else(|| panic!("Invalid task token {}", token));
                    let future = &mut *task.future.lock().unwrap();

                    let waker = Waker::from(Arc::clone(&task));
                    let cx = &mut Context::from_waker(&waker);

                    if future.as_mut().poll(cx).is_ready() {
                        finished = true;
                    }
                }
                // Used to circumvent the borrow-checker. Otherwise, `future` could
                // be pointing to garbage upon calling remove.
                if finished {
                    self.task_map.remove(&token).unwrap();

                    // Done processing all suspended events.
                    if self.task_map.is_empty() {
                        break;
                    }
                }
            }
            // println!("Executor::run --> Finished running");
        }
        pub fn suspend(&mut self, task: Arc<Task>) {
            self.task_map.insert(task.token, task);
        }
    }
}
