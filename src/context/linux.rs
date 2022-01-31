// nih-plug: plugins, but rewritten in Rust
// Copyright (C) 2022 Robbert van der Helm
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! An event loop impelemntation for Linux. APIs on Linux are generally thread safe, so the context
//! of a main thread does not exist there. Because of that, this mostly just serves as a way to
//! delegate expensive processing to another thread.

use crossbeam::channel;
use std::sync::Arc;
use std::thread::{self, JoinHandle, ThreadId};

use crate::nih_log;

use super::{EventLoop, MainThreadExecutor};

/// See [super::EventLoop].
pub(crate) struct LinuxEventLoop<T, E> {
    /// The thing that ends up executing these tasks. The tasks are usually executed from the worker
    /// thread, but if the current thread is the main thread then the task cna also be executed
    /// directly.
    executor: Arc<E>,

    /// The ID of the main thread. In practice this is the ID of the thread that created this task
    /// queue.
    main_thread_id: ThreadId,

    /// A thread that act as our worker thread. When [do_maybe_async] is called, this thread will be
    /// woken up to execute the task on the executor. This is wrapped in an `Option` so the thread
    /// can be taken out of it and joined when this struct gets dropped.
    worker_thread: Option<JoinHandle<()>>,
    /// A channel for waking up the worker thread and having it perform one of the tasks from
    /// [Message].
    worker_thread_channel: channel::Sender<Message<T>>,
}

/// A message for communicating with the worker thread.
enum Message<T> {
    /// A new task for the event loop to execute.
    Task(T),
    /// Shut down the worker thread.
    Shutdown,
}

impl<T, E> EventLoop<T, E> for LinuxEventLoop<T, E>
where
    T: Send,
    E: MainThreadExecutor<T>,
{
    fn new_and_spawn(executor: Arc<E>) -> Self {
        let (sender, receiver) = channel::bounded(super::TASK_QUEUE_CAPACITY);

        Self {
            executor: executor.clone(),
            main_thread_id: thread::current().id(),
            // With our drop implementation we guarentee that this thread never outlives this struct
            worker_thread: Some(unsafe {
                thread::Builder::new()
                    .name(String::from("worker"))
                    // FIXME: Find another way to bind a thread lifetime to this struct without a
                    //        nightly-only fature
                    .spawn_unchecked(move || worker_thread(receiver, executor))
                    .expect("Could not spawn worker thread")
            }),
            worker_thread_channel: sender,
        }
    }

    fn do_maybe_async(&self, task: T) -> bool {
        if self.is_main_thread() {
            self.executor.execute(task);
            true
        } else {
            self.worker_thread_channel
                .try_send(Message::Task(task))
                .is_ok()
        }
    }

    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }
}

impl<T, E> Drop for LinuxEventLoop<T, E> {
    fn drop(&mut self) {
        self.worker_thread_channel
            .send(Message::Shutdown)
            .expect("Failed while sending worker thread shutdown request");
        if let Some(join_handle) = self.worker_thread.take() {
            join_handle.join().expect("Worker thread panicked");
        }
    }
}

/// The worker thread used in [EventLoop] that executes incmoing tasks on the event loop's executor.
fn worker_thread<T, E>(receiver: channel::Receiver<Message<T>>, executor: Arc<E>)
where
    T: Send,
    E: MainThreadExecutor<T>,
{
    loop {
        match receiver.recv() {
            Ok(Message::Task(task)) => executor.execute(task),
            Ok(Message::Shutdown) => return,
            Err(err) => {
                nih_log!(
                    "Worker thread got disconnected unexpectedly, shutting down: {}",
                    err
                );
                return;
            }
        }
    }
}