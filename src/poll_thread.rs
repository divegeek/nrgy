use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Duration,
};

use log::error;

use crate::NrgyResult;

pub trait Pollable: Send + 'static {
    fn name(&self) -> &'static str;
    fn init(&mut self) -> NrgyResult<()>;
    fn poll(&mut self) -> NrgyResult<()>;
    fn default_interval(&self) -> Duration;
}

pub struct PollThread<T: Pollable> {
    state: Arc<Mutex<Inner<T>>>,
    _handle: JoinHandle<()>,
}

pub struct Inner<T> {
    poll_interval: Duration,
    task: T,
}

impl<T: Pollable> PollThread<T> {
    pub fn start(mut task: T) -> NrgyResult<Self> {
        task.init()?;
        let state = Arc::new(Mutex::new(Inner {
            poll_interval: task.default_interval(),
            task,
        }));
        let thread_state = state.clone();
        let _handle = thread::spawn(move || poll_loop(thread_state));
        Ok(Self { state, _handle })
    }

    pub fn lock(&'_ self) -> PollGuard<'_, T> {
        PollGuard(self.state.lock().unwrap())
    }

    pub fn interval(&self) -> Duration {
        self.state.lock().unwrap().poll_interval
    }

    pub fn set_interval(&self, interval: Duration) {
        self.state.lock().unwrap().poll_interval = interval;
    }
}

pub struct PollGuard<'a, T>(MutexGuard<'a, Inner<T>>);

impl<T> Deref for PollGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.task
    }
}

impl<T> DerefMut for PollGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0.task
    }
}

fn poll_loop<T: Pollable>(thread_state: Arc<Mutex<Inner<T>>>) -> ! {
    let name = thread_state.lock().unwrap().task.name();
    loop {
        let interval = thread_state.lock().unwrap().poll_interval;
        thread::sleep(interval);
        if let Err(e) = thread_state.lock().unwrap().task.poll() {
            error!("Poll {} error: {e}", name)
        }
    }
}
