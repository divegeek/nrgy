use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use log::error;

use crate::NrgyResult;

/// For types that manage their own internal synchronisation (e.g. `TeslaVehicle`).
/// `PollThread` holds an `Arc<T>` and calls through `&self`.
pub trait Pollable: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn init(&self) -> NrgyResult<()>;
    fn poll(&self) -> NrgyResult<()>;
    fn default_interval(&self) -> Duration;
}

/// For types that require exclusive access on each call (e.g. `SolarEdge`).
/// Wrap in `Arc<Mutex<T>>` to obtain a `Pollable`.
pub trait PollableMut: Send + 'static {
    fn name(&self) -> &'static str;
    fn init(&mut self) -> NrgyResult<()>;
    fn poll(&mut self) -> NrgyResult<()>;
    fn default_interval(&self) -> Duration;
}

impl<T: PollableMut + Send> Pollable for Mutex<T> {
    fn name(&self) -> &'static str {
        self.lock().unwrap().name()
    }
    fn init(&self) -> NrgyResult<()> {
        self.lock().unwrap().init()
    }
    fn poll(&self) -> NrgyResult<()> {
        self.lock().unwrap().poll()
    }
    fn default_interval(&self) -> Duration {
        self.lock().unwrap().default_interval()
    }
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

        let _handle = thread::spawn(move || {
            let mut last = Instant::now();
            loop {
                let nanos = thread_interval.load(Ordering::Relaxed);
                thread::sleep(Duration::from_secs(10).min(Duration::from_nanos(nanos)));
                if last.elapsed().as_nanos() >= nanos as u128 {
                    if let Err(e) = thread_task.poll() {
                        error!("Poll {name} error: {e}");
                    }
                    last = Instant::now();
                }
            }
        });

        Ok(Self {
            poll_interval_nanos,
            _handle,
        })
    }

    pub fn set_interval(&self, interval: Duration) {
        self.poll_interval_nanos
            .store(interval.as_nanos() as u64, Ordering::Relaxed);
    }
}
