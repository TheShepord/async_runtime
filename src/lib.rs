// Module providing safe bindings around unsafe epoll ffi.
mod epoll {
    use std::io;

    pub const EPOLL_CTL_ADD: i32 = 1;
    pub const EPOLL_CTL_DEL: i32 = 2;
    pub const EPOLLIN: i32 = 0x1;
    pub const EPOLLONESHOT: i32 = 0x40000000;

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
mod blockers {
    use crate::epoll;
    use std::io;
    use std::os::unix::io::{AsRawFd, RawFd};

    pub struct IoBlocker {
        fd: RawFd,
    }

    impl IoBlocker {
        pub fn new() -> io::Result<Self> {
            match epoll::create() {
                Ok(fd) => Ok(IoBlocker { fd }),
                Err(e) => Err(e),
            }
        }

        pub fn registrator(&self) -> Registrator {
            Registrator { fd: self.fd }
        }

        // Block until an event is available.
        pub fn block(&self, events: &mut Vec<epoll::Event>) -> io::Result<i32> {
            const TIMEOUT: i32 = -1; // Wait forever.
            const MAX_EVENTS: i32 = 1024;
            events.clear();

            let n_events = epoll::wait(self.fd, events, MAX_EVENTS, TIMEOUT)?;

            // Rust has no way of knowing events vec changed size, since
            // this happened in unsafe FFI world. Forcefully change size here.
            // Note this should never introduce problems, unless the OS
            // did something wrong.
            unsafe { events.set_len(n_events as usize) };

            Ok(n_events)
        }
    }

    // Ensure that we close held file descriptor.
    impl Drop for IoBlocker {
        fn drop(&mut self) {
            epoll::close(self.fd).unwrap();
        }
    }

    // Struct to get around Rust ownership issues
    #[derive(Debug, Clone, Copy)] // Helpful default traits.
    pub struct Registrator {
        fd: RawFd,
    }

    impl Registrator {
        // Register a file descriptor of interest.
        pub fn register(&self, interest: &impl AsRawFd, token: usize) -> io::Result<()> {
            let fd = interest.as_raw_fd();
            let mut event = epoll::Event::new(epoll::EPOLLIN | epoll::EPOLLONESHOT, token);
            epoll::ctl(self.fd, epoll::EPOLL_CTL_ADD, fd, &mut event)?;

            Ok(())
        }
    }
}

// Provides future API for awaitable services. For use by external crates.
// In this case, the only provided service is a TcpStream.
mod services {
    use crate::blockers;
    use futures::io::AsyncRead;
    use std::io::{self, Error, Read, Write};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::{
        net,
        pin::Pin,
        task::{Context, Poll},
    };

    pub struct TcpStream {
        // We use the non-async TcpStream as a starting point for our implementation.
        inner: net::TcpStream,
        registrator: blockers::Registrator,
    }

    impl TcpStream {
        pub fn connect(
            addr: impl net::ToSocketAddrs,
            registrator: blockers::Registrator,
        ) -> io::Result<Self> {
            let stream = net::TcpStream::connect(addr)?;
            stream.set_nonblocking(true)?;

            Ok(TcpStream {
                inner: stream,
                registrator,
            })
        }
    }

    impl AsyncRead for TcpStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            let stream = self.get_mut();
            match stream.inner.read(buf) {
                Ok(n) => Poll::Ready(Ok(n)),
                // Since we set this TcpStream to non-blocking, check if
                // still not ready and pend.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stream.registrator.register(stream, 1).unwrap();
                    Poll::Pending
                }
                // Another error occurred, done.
                Err(e) => Poll::Ready(Err(e)),
            }
        }
    }

    impl Write for TcpStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    // Allows us to retrieve the file descriptor associated with this stream.
    impl AsRawFd for TcpStream {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }

    // impl Future for TcpStream {
    //     type Output = ();
    //     fn poll(self: Pin<&mut Self>, wake: &mut Context<'_>) -> Poll<Self::Output> {
    //     }
    // }
}

// Implements reactor/executor design pattern. For use by external crates.
mod runtime {
    use crate::{blockers, epoll};
    use std::{
        collections::HashMap,
        future::Future,
        pin::Pin,
        sync::{mpsc, Arc, Mutex, RwLock},
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
        // Tasks are sent through this ready queue to be scheduled.
        // We use a `SyncSender` as opposed to a regular `Sender` to allow for sending
        // `MyTask` between threads.
        ready_queue: mpsc::SyncSender<Arc<Task>>,
    }

    // Reactor struct, responsible for receiving tasks and sending them to the
    // Executor.
    pub struct Reactor {
        sender: mpsc::SyncSender<Arc<Task>>,
        registrator: blockers::Registrator,
        event_map: Arc<RwLock<HashMap<usize, Arc<Task>>>>,
    }

    // Executor struct, responsible for running pending tasks.
    pub struct Executor {
        receiver: mpsc::Receiver<Arc<Task>>,
    }

    impl Executor {
        // Create a new Executor instance.
        pub fn new(receiver: mpsc::Receiver<Arc<Task>>) -> Executor {
            Executor { receiver }
        }
        // Run all tasks to completion.
        pub fn run(&self) {
            while let Ok(task) = self.receiver.recv() {
                let fut = &mut *task.future.lock().unwrap();

                let waker = Waker::from(Arc::clone(&task));
                let cx = &mut Context::from_waker(&waker);

                if fut.as_mut().poll(cx).is_pending() {}
            }
        }
    }

    impl Reactor {
        // Create a new Reactor instance.
        pub fn new(sender: mpsc::SyncSender<Arc<Task>>, blocker: blockers::IoBlocker) -> Reactor {
            let event_map: Arc<RwLock<HashMap<usize, Arc<Task>>>> =
                Arc::new(RwLock::new(HashMap::new()));
            let cloned_sender = sender.clone();
            let cloned_map = Arc::clone(&event_map);
            let registrator = blocker.registrator();
            // Spawn a new thread that blocks until an event is ready.
            let handle = thread::spawn(move || {
                let mut events: Vec<epoll::Event> = Vec::with_capacity(1024);
                loop {
                    // Blocks until an event is ready.
                    match &blocker.block(&mut events) {
                        Ok(_) => (),
                        // Err(ref e) if e.kind() == io::ErrorKind::Interrupted => break,
                        Err(e) => panic!("Poll error: {:?}, {}", e.kind(), e),
                    };
                    for event in &events {
                        let event_token = event.token();
                        match cloned_map.read().unwrap().get(&event_token) {
                            Some(task) => cloned_sender
                                .send(Arc::clone(task))
                                .expect("send event_token err."),
                            None => (),
                        }
                    }
                }
            });
            Reactor {
                sender,
                registrator,
                event_map,
            }
        }
        // Add a future to the task list.
        pub fn subscribe(&self, future: impl Future<Output = ()> + Send + 'static, token: usize) {
            let task = Arc::new(Task {
                future: Mutex::new(Box::pin(future)),
                ready_queue: self.sender.clone(),
            });
            self.sender
                .send(task)
                .expect("Failed to send task to executor.");
        }
    }

    impl Wake for Task {
        fn wake(self: Arc<Self>) {
            let clone = Arc::clone(&self);
            self.ready_queue
                .send(clone)
                .expect("Failed to send task to `ready_queue`");
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let clone = Arc::clone(&self);
            self.ready_queue
                .send(clone)
                .expect("Failed to send task to `ready_queue`");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::io::AsyncReadExt;
    use std::io::Write;
    use std::sync::mpsc;

    #[test]
    fn it_works() {
        const MAX_CHANNELS: usize = 1000;

        let (sender, receiver) = mpsc::sync_channel(MAX_CHANNELS);

        let blocker = blockers::IoBlocker::new().unwrap();
        let registrator = blocker.registrator();
        let reactor = runtime::Reactor::new(sender, blocker);
        let executor = runtime::Executor::new(receiver);

        // This site simulates slow server responses.
        let addr = "flash.siwalik.in:80";

        for i in 1..6 {
            let mut stream = services::TcpStream::connect(addr, registrator).unwrap();

            let delay = i * 1000;
            // The desired delay is passed in the GET request in miliseconds.
            let request = format!(
                "GET /delay/{}/url/http://www.google.com HTTP/1.1\r\n\
                Host: flash.siwalik.in\r\n\
                Connection: close\r\n\
                \r\n",
                delay
            );

            stream.write_all(request.as_bytes()).unwrap();
            // streams.push(stream);

            let future = async move {
                let mut buf = String::new();
                stream.read_to_string(&mut buf).await.unwrap();
                println!("contents: {}", buf);
            };
            reactor.subscribe(future, i);
        }

        executor.run();
    }
}
