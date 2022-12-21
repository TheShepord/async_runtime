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
            pub fn epoll_wait(epfd: i32, events: *mut super::Event, maxevents: i32, timeout: i32) -> i32;
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

    fn wait(epfd: i32, events: &mut [Event], maxevents: i32, timeout: i32) -> io::Result<i32> {
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
        epoll_data: usize,
    }

    impl Event {
        pub fn new(events: u32, id: usize) -> Self {
            Event {
                events: events,
                epoll_data: id,
            }
        }
        pub fn data(&self) -> usize {
            self.epoll_data
        }
    }
}

mod queue {}

// Module implementing our Task, Reactor and Executor structs.
mod runtime {
    use std::{
        future::Future,
        pin::Pin,
        sync::{mpsc, Arc, Mutex},
        task::{Context, Wake, Waker},
    };

    use futures::task::ArcWake;
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
        pub fn new(sender: mpsc::SyncSender<Arc<Task>>) -> Reactor {
            Reactor { sender }
        }
        // Add a future to the task list.
        pub fn subscribe(&self, future: impl Future<Output = ()> + Send + 'static) {
            let task = Arc::new(Task {
                future: Mutex::new(Box::pin(future)),
                ready_queue: self.sender.clone(),
            });

            self.sender
                .send(task)
                .expect("Failed to send task to executor.");
        }
    }

    // Task struct, holding a future and implementing the Wake trait.

    // Implement the MyWake trait for MyTask.
    // impl ArcWake for MyTask {
    //     fn wake_by_ref(arc_self: &Arc<Self>) {
    //         let clone = Arc::clone(&arc_self);
    //         arc_self.ready_queue.send(clone).expect("Failed to send task to `ready_queue`");
    //     }
    // }

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

    use std::sync::mpsc;

    #[test]
    fn it_works() {
        const MAX_CHANNELS: usize = 1000;

        let (sender, receiver) = mpsc::sync_channel(MAX_CHANNELS);

        let reactor = runtime::Reactor::new(sender);
        let executor = runtime::Executor::new(receiver);

        reactor.subscribe(async {
            println!("hello");
        });
        executor.run();
    }
}
