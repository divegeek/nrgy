use std::{
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    thread::{self, JoinHandle},
    time::Duration,
};

use log::error;

use crate::NrgyResult;

pub trait Pollable: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn init(&self) -> NrgyResult<()>;
    fn poll(&self) -> NrgyResult<()>;
    fn default_interval(&self) -> Duration;
}

pub struct PollThread {
    poll_interval_nanos: Arc<AtomicU64>,
    _handle: JoinHandle<()>,
}

impl PollThread {
    pub fn start<T: Pollable>(task: Arc<T>) -> NrgyResult<Self> {
        let (default_interval, name) = (task.default_interval(), task.name());
        task.init()?;

        let poll_interval_nanos = Arc::new(AtomicU64::new(default_interval.as_nanos() as u64));
        let thread_task = task.clone();
        let thread_interval = poll_interval_nanos.clone();

        let _handle = thread::spawn(move || loop {
            let nanos = thread_interval.load(Ordering::Relaxed);
            thread::sleep(Duration::from_nanos(nanos));
            if let Err(e) = thread_task.poll() {
                error!("Poll {name} error: {e}");
            }
        });

        Ok(Self { poll_interval_nanos, _handle })
    }

    pub fn interval(&self) -> Duration {
        Duration::from_nanos(self.poll_interval_nanos.load(Ordering::Relaxed))
    }

    pub fn set_interval(&self, interval: Duration) {
        self.poll_interval_nanos.store(interval.as_nanos() as u64, Ordering::Relaxed);
    }
}
