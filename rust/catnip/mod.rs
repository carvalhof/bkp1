// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

mod config;
pub mod interop;
pub mod runtime;

//==============================================================================
// Imports
//==============================================================================

use self::{
    interop::pack_result,
    runtime::{
        DPDKRuntime,
        memory::MemoryManager
    },
};
use crate::{
    demikernel::config::Config,
    collections::ring::RingBuffer,
    inetstack::{
        InetStack,
        protocols::tcp::established::ControlBlock
    },
    runtime::{
        fail::Fail,
        libdpdk::{
            rte_mbuf,
            load_mlx_driver,
        },
        memory::MemoryRuntime,
        network::consts::RECEIVE_BATCH_SIZE,
        types::{
            demi_qresult_t,
            demi_sgarray_t,
        },
        OperationResult,
        QDesc,
        QToken, timer::{TimerRc, Timer},
    },
    scheduler::{
        Scheduler,
        TaskHandle,
    },
};
use std::{time::Instant, rc::Rc};
use ::std::{
    net::SocketAddrV4,
    ops::{
        Deref,
        DerefMut,
    },
    sync::Arc,
};

#[cfg(feature = "profiler")]
use crate::timer;

//==============================================================================
// Structures
//==============================================================================

/// Catnip LibOS
pub struct CatnipLibOS {
    scheduler: Arc<Scheduler>,
    inetstack: InetStack<RECEIVE_BATCH_SIZE>,
    rt: Arc<DPDKRuntime>,
}

//==============================================================================
// Associate Functions
//==============================================================================

/// Associate Functions for Catnip LibOS
impl CatnipLibOS {
    pub fn start(tx_queues: u16) -> Result<MemoryManager, Fail> {
        load_mlx_driver();

        // Read in configuration file.
        let config_path: String = match std::env::var("CONFIG_PATH") {
            Ok(config_path) => config_path,
            Err(_) => {
                return Err(Fail::new(
                    libc::EINVAL,
                    "missing value for CONFIG_PATH environment variable",
                ))
            },
        };

        let config: Config = Config::new(config_path);

        // Initializes the DPDKRuntime
        let (mm, port_id) = DPDKRuntime::init_dpdk(
            &config.eal_init_args(),
            config.use_jumbo_frames(),
        ).unwrap();

        // Initializes the DPDK port and queues
        DPDKRuntime::init_dpdk_port(
            port_id, 
            &mm, 
            config.use_jumbo_frames(), 
            config.mtu(), 
            config.tcp_checksum_offload(),
            config.udp_checksum_offload(),
            tx_queues
        ).unwrap();

        Ok(mm)
    }

    pub fn new(config: &Config, queue_id: u16, mm: Arc<MemoryManager>, ring: *mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>) -> Self {
        load_mlx_driver();
        let rt: Arc<DPDKRuntime> = Arc::new(DPDKRuntime::new(
            config.local_ipv4_addr(),
            config.arp_table(),
            config.disable_arp(),
            config.mss(),
            config.tcp_checksum_offload(),
            config.udp_checksum_offload(),
            0u16, 
            queue_id, 
            mm,
            ring,
        ));
        let now: Instant = Instant::now();
        let clock: TimerRc = TimerRc(Rc::new(Timer::new(now)));
        let scheduler: Arc<Scheduler> = Arc::new(Scheduler::default());
        let inetstack: InetStack<RECEIVE_BATCH_SIZE> = InetStack::new(
            rt.clone(),
            scheduler.clone(),
            clock,
            rt.link_addr,
            rt.ipv4_addr,
            rt.tcp_options.clone(),
        )
        .unwrap();
        CatnipLibOS {
            inetstack,
            scheduler,
            rt,
        }
    }

    pub fn get_rt(&self) -> Arc<DPDKRuntime> {
        self.rt.clone()
    }

    pub fn get_clock(&self) -> TimerRc {
        self.inetstack.get_clock()
    }

    pub fn get_scheduler(&self) -> Arc<Scheduler> {
        self.scheduler.clone()
    }

    pub fn get_ring(&self) -> *mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)> {
        self.rt.ring
    }

    /// Create a push request for Demikernel to asynchronously write data from `sga` to the
    /// IO connection represented by `qd`. This operation returns immediately with a `QToken`.
    /// The data has been written when [`wait`ing](Self::wait) on the QToken returns.
    pub fn push(&mut self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, sga: &demi_sgarray_t) -> Result<QToken, Fail> {
        #[cfg(feature = "profiler")]
        timer!("catnip::push");
        unsafe { trace!("push(): qd={:?}", (*cb).qd) };
        match self.rt.clone_sgarray(sga) {
            Ok(buf) => {
                if buf.len() == 0 {
                    return Err(Fail::new(libc::EINVAL, "zero-length buffer"));
                }
                let future = self.do_push(cb, buf)?;
                let handle: TaskHandle = match self.scheduler.insert(future) {
                    Some(handle) => handle,
                    None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                };
                let qt: QToken = handle.get_task_id().into();
                unsafe {
                    trace!("push(): qd={:?} -- releasing the lock", (*cb).qd);
                    (*cb).spinlock.unlock();
                };
                Ok(qt)
            },
            Err(e) => Err(e),
        }
    }

    pub fn pushto(&mut self, qd: QDesc, sga: &demi_sgarray_t, to: SocketAddrV4) -> Result<QToken, Fail> {
        #[cfg(feature = "profiler")]
        timer!("catnip::pushto");
        trace!("pushto2(): qd={:?}", qd);
        match self.rt.clone_sgarray(sga) {
            Ok(buf) => {
                if buf.len() == 0 {
                    return Err(Fail::new(libc::EINVAL, "zero-length buffer"));
                }
                let future = self.do_pushto(qd, buf, to)?;
                let handle: TaskHandle = match self.scheduler.insert(future) {
                    Some(handle) => handle,
                    None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                };
                let qt: QToken = handle.get_task_id().into();
                Ok(qt)
            },
            Err(e) => Err(e),
        }
    }

    pub fn schedule(&mut self, qt: QToken) -> Result<TaskHandle, Fail> {
        match self.scheduler.from_task_id(qt.into()) {
            Some(handle) => Ok(handle),
            None => return Err(Fail::new(libc::EINVAL, "invalid queue token")),
        }
    }

    pub fn pack_result(&mut self, handle: TaskHandle, qt: QToken) -> Result<demi_qresult_t, Fail> {
        let (cb, r): (*mut ControlBlock<RECEIVE_BATCH_SIZE>, OperationResult) = self.take_operation(handle);
        Ok(unsafe { pack_result(self.rt.clone(), r, (*cb).qd, qt.into()) })
    }

    /// Allocates a scatter-gather array.
    pub fn sgaalloc(&self, size: usize) -> Result<demi_sgarray_t, Fail> {
        self.rt.alloc_sgarray(size)
    }

    /// Releases a scatter-gather array.
    pub fn sgafree(&self, sga: demi_sgarray_t) -> Result<(), Fail> {
        self.rt.free_sgarray(sga)
    }
}

//==============================================================================
// Trait Implementations
//==============================================================================

/// De-Reference Trait Implementation for Catnip LibOS
impl Deref for CatnipLibOS {
    type Target = InetStack<RECEIVE_BATCH_SIZE>;

    fn deref(&self) -> &Self::Target {
        &self.inetstack
    }
}

/// Mutable De-Reference Trait Implementation for Catnip LibOS
impl DerefMut for CatnipLibOS {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inetstack
    }
}
