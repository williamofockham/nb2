/*
* Copyright 2019 Comcast Cable Communications Management, LLC
*
* Licensed under the Apache License, Version 2.0 (the "License");
* you may not use this file except in compliance with the License.
* You may obtain a copy of the License at
*
* http://www.apache.org/licenses/LICENSE-2.0
*
* Unless required by applicable law or agreed to in writing, software
* distributed under the License is distributed on an "AS IS" BASIS,
* WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
* See the License for the specific language governing permissions and
* limitations under the License.
*
* SPDX-License-Identifier: Apache-2.0
*/

mod core_map;
mod mempool_map;

pub use self::core_map::*;
pub use self::mempool_map::*;

use crate::dpdk::{eal_cleanup, eal_init, CoreId, Port, PortBuilder, PortError, PortQueue};
use crate::settings::RuntimeSettings;
use crate::{debug, ensure, info, Result};
use futures::{future, stream, Future, StreamExt};
use libc;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_executor::current_thread;
use tokio_net::driver;
use tokio_net::signal::unix::{self, SignalKind};
use tokio_timer::{timer, Interval};

/// Supported Unix signals.
#[derive(Copy, Clone, Debug)]
pub enum UnixSignal {
    SIGHUP = libc::SIGHUP as isize,
    SIGINT = libc::SIGINT as isize,
    SIGTERM = libc::SIGTERM as isize,
}

pub struct Runtime {
    ports: Vec<Port>,
    mempools: MempoolMap,
    core_map: CoreMap,
    on_signal: Arc<dyn Fn(UnixSignal) -> bool>,
    config: RuntimeSettings,
}

impl Runtime {
    /// Builds a runtime from config settings.
    #[allow(clippy::cognitive_complexity)]
    pub fn build(config: RuntimeSettings) -> Result<Self> {
        info!("initializing EAL...");
        eal_init(config.to_eal_args())?;

        let cores = config.all_cores();

        info!("initializing mempools...");
        let mut sockets = cores.iter().map(CoreId::socket_id).collect::<HashSet<_>>();
        let sockets = sockets.drain().collect::<Vec<_>>();
        let mut mempools =
            MempoolMap::new(config.mempool.capacity, config.mempool.cache_size, &sockets)?;

        info!("intializing cores...");
        let core_map = CoreMapBuilder::new()
            .cores(&cores)
            .master_core(config.master_core)
            .mempools(mempools.borrow_mut())
            .finish()?;

        info!("initializing ports...");
        let mut ports = vec![];
        for conf in config.ports.iter() {
            let port = PortBuilder::new(conf.name.clone(), conf.device.clone())?
                .cores(&conf.cores)?
                .mempools(mempools.borrow_mut())
                .rx_tx_queue_capacity(conf.rxd, conf.txd)?
                .finish()?;

            debug!(?port);
            ports.push(port);
        }

        info!("runtime ready.");

        Ok(Runtime {
            ports,
            mempools,
            core_map,
            on_signal: Arc::new(|_| true),
            config,
        })
    }

    /// Sets the Unix signal handler.
    ///
    /// `SIGHUP`, `SIGINT` and `SIGTERM` are the supported Unix signals.
    /// The return of the handler determines whether to terminate the
    /// process. `true` indicates the signal is received and the process
    /// should be terminated. `false` indicates to discard the signal and
    /// keep the process running.
    ///
    /// # Example
    ///
    /// ```
    /// Runtime::build(&config)?;
    ///     .set_on_signal(|signal| match signal {
    ///         SIGHUP => {
    ///             reload_config();
    ///             false
    ///         }
    ///         _ => true,
    ///     })
    ///     .execute();
    /// ```
    pub fn set_on_signal<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(UnixSignal) -> bool + 'static,
    {
        self.on_signal = Arc::new(f);
        self
    }

    /// Installs a pipeline to a port. The pipeline will run on all the
    /// cores assigned to the port.
    ///
    /// `port` is the logical name that identifies the port. The `installer`
    /// is a closure that takes in a `PortQueue` and returns a `Pipeline`
    /// that will be spawned onto the thread executor.
    pub fn add_pipeline_to_port<T: Future<Output = ()> + 'static, F>(
        &mut self,
        port: &str,
        installer: F,
    ) -> Result<&mut Self>
    where
        F: Fn(PortQueue) -> T + Send + Sync + 'static,
    {
        let port = &self
            .ports
            .iter()
            .find(|p| p.name() == port)
            .ok_or_else(|| PortError::NotFound(port.to_owned()))?;

        let f = Arc::new(installer);

        for (core_id, port_q) in port.queues() {
            let f = f.clone();
            let port_q = *port_q;
            let thread = &self.core_map.cores[core_id].thread;

            // spawns the bootstrap. we want the bootstrapping to execute on the
            // target core instead of the master core. that way the actual task
            // is spawned locally and the type bounds are less restricting.
            thread.spawn(future::lazy(move |_| {
                let task = f(port_q);
                current_thread::spawn(task);
            }))?;

            debug!("installed pipeline on port_q for {:?}.", core_id);
        }

        info!("installed pipeline for port {}.", port.name());

        Ok(self)
    }

    /// Installs a pipeline to a core. All the ports the core is assigned
    /// to will be available to the pipeline.
    ///
    /// `core` is the logical id that identifies the core. The `installer`
    /// is a closure that takes in a hashmap of `PortQueue`s and returns a
    /// `Pipeline` that will be spawned onto the thread executor of the core.
    pub fn add_pipeline_to_core<T: Future<Output = ()> + 'static, F>(
        &mut self,
        core: usize,
        installer: F,
    ) -> Result<&mut Self>
    where
        F: FnOnce(HashMap<String, PortQueue>) -> T + Send + Sync + 'static,
    {
        let core_id = CoreId::new(core);

        let thread = &self
            .core_map
            .cores
            .get(&core_id)
            .ok_or_else(|| CoreError::NotFound(core))?
            .thread;

        let port_qs = self
            .ports
            .iter()
            .filter_map(|p| p.queues().get(&core_id).map(|q| (p.name().to_owned(), *q)))
            .collect::<HashMap<_, _>>();

        ensure!(!port_qs.is_empty(), CoreError::NotAssigned(core));

        // spawns the bootstrap. we want the bootstrapping to execute on the
        // target core instead of the master core.
        thread.spawn(future::lazy(move |_| {
            let task = installer(port_qs);
            current_thread::spawn(task);
        }))?;

        info!("installed pipeline for core {:?}.", core_id);

        Ok(self)
    }

    /// Installs a periodic task to a core.
    ///
    /// `core` is the logical id that identifies the core. `task` is the
    /// closure to execute. The task will rerun every `dur` interval.
    pub fn add_periodic_task_to_core<T>(
        &mut self,
        core: usize,
        task: T,
        dur: Duration,
    ) -> Result<&mut Self>
    where
        T: Fn() -> () + Send + Sync + 'static,
    {
        let core_id = CoreId::new(core);

        let thread = &self
            .core_map
            .cores
            .get(&core_id)
            .ok_or_else(|| CoreError::NotFound(core))?
            .thread;

        // spawns the bootstrap. we want the bootstrapping to execute on the
        // target core instead of the master core so the periodic task is
        // associated with the correct timer instance.
        thread.spawn(future::lazy(move |_| {
            #[allow(clippy::unit_arg)]
            let task = Interval::new_interval(dur).for_each(move |_| future::ready(task()));
            current_thread::spawn(task);
        }))?;

        Ok(self)
    }

    /// Blocks the main thread until a timeout expires.
    ///
    /// This mode is useful for running integration tests. The timeout
    /// duration can be set in `RuntimeSettings`.
    fn wait_for_timeout(&mut self, timeout: u64) -> Result<()> {
        let MasterExecutor {
            ref timer,
            ref mut thread,
            ..
        } = self.core_map.master_core;

        let when = Instant::now() + Duration::from_secs(timeout);
        let delay = timer.delay(when);

        debug!("waiting for {} seconds...", timeout);
        let _timer = timer::set_default(&timer);
        thread.block_on(delay);
        info!("timed out after {} seconds.", timeout);

        Ok(())
    }

    /// Blocks the main thread until receives a signal to terminate.
    fn wait_for_signal(&mut self) -> Result<()> {
        let sighup = unix::signal(SignalKind::hangup())?.map(|_| UnixSignal::SIGHUP);
        let sigint = unix::signal(SignalKind::interrupt())?.map(|_| UnixSignal::SIGINT);
        let sigterm = unix::signal(SignalKind::terminate())?.map(|_| UnixSignal::SIGTERM);

        // combines the streams together
        let stream = stream::select(stream::select(sighup, sigint), sigterm);

        // passes each signal through the `on_signal` closure, and discard
        // any that shouldn't stop the execution.
        let f = self.on_signal.clone();
        let mut stream = stream.filter(|&signal| future::ready(f(signal)));

        let MasterExecutor {
            ref reactor,
            ref timer,
            ref mut thread,
            ..
        } = self.core_map.master_core;

        // sets the reactor so we receive the signals and runs the future
        // on the master core. the execution stops on the first signal that
        // wasn't filtered out.
        debug!("waiting for a Unix signal...");
        let _guard = driver::set_default(&reactor);
        let _timer = timer::set_default(&timer);
        let _ = thread.block_on(stream.next());
        info!("signaled to stop.");

        Ok(())
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn execute(&mut self) -> Result<()> {
        // starts all the ports so we can receive packets.
        for port in self.ports.iter_mut() {
            port.start()?;
        }

        // unparks all the cores to start task execution.
        for core in self.core_map.cores.values() {
            if let Some(unpark) = &core.unpark {
                unpark.unpark();
            }
        }

        // runs the app until main loop finishes.
        match self.config.duration {
            None | Some(0) => self.wait_for_signal(),
            Some(d) => self.wait_for_timeout(d),
        }?;

        // shuts down all the cores.
        for (core_id, core) in &mut self.core_map.cores {
            if let Some(trigger) = core.shutdown.take() {
                debug!("shutting down {:?}.", core_id);
                trigger.shutdown();
                debug!("sent {:?} shutdown trigger.", core_id);
                let handle = core.join.take().unwrap();
                let _ = handle.join();
                info!("terminated {:?}.", core_id);
            }
        }

        // stops all the ports.
        for port in self.ports.iter_mut() {
            port.stop();
        }

        Ok(())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        debug!("freeing EAL.");
        eal_cleanup().unwrap();
    }
}
