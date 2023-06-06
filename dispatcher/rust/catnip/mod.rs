// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

mod config;
mod interop;
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
    OperationResult,
    demikernel::config::Config,
    inetstack::InetStack,
    runtime::{
        fail::Fail,
        libdpdk::{
            rte_mbuf,
            load_mlx_driver,
        },
        memory::{
            MemoryRuntime,
            DemiBuffer,
        },
        timer::{
            Timer,
            TimerRc,
        },
        types::{
            demi_opcode_t,
            demi_qresult_t,
            demi_sgarray_t,
        },
        QDesc,
        QToken,
    },
    scheduler::{
        Scheduler,
        SchedulerHandle,
    },
    collections::ring::RingBuffer,
    pal::linux::shm::SharedMemory,
};
use ::std::{
    net::SocketAddrV4,
    ops::{
        Deref,
        DerefMut,
    },
    rc::Rc,
    sync::{
        Arc,
        atomic::{
            Ordering,
            AtomicBool,
        },
    },
    time::Instant,
};
use arrayvec::ArrayVec;
use dpdk_rs::rte_eth_tx_burst;

#[cfg(feature = "profiler")]
use crate::timer;

//==============================================================================
// Constants
//==============================================================================

const MAX_WORKERS: usize = 16;
const SCRATCH_SIZE: usize = 128;
const RING_BUFFER_CAPACITY: usize = 2;
const MAX_QUEUED_WORK_SIZE: usize = 1024;

//==============================================================================
// Structures
//==============================================================================

/// Test-and-Set Lock
pub struct TASLock {
    busy: AtomicBool,
}

/// Catnip LibOS
pub struct CatnipLibOS {
    scheduler: Scheduler,
    inetstack: InetStack,
    rt: Rc<DPDKRuntime>,
    scratches: Vec<SharedMemory>,
}

/// Catnip message
pub struct CatnipMessage {
    pub ring_tx: *mut RingBuffer::<(QDesc, *mut rte_mbuf)>,
    pub ring_rx: *mut RingBuffer::<(QDesc, *mut rte_mbuf)>,
}

//==============================================================================
// Associate Functions
//==============================================================================

/// Associate Functions for Test-and-Set Lock
impl TASLock {
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
        }
    }
    pub fn set(&self) {
        self.busy.store(true, Ordering::SeqCst);
    }
    pub fn lock(&self) {
        self.busy.store(true, Ordering::SeqCst);
        while self.busy.load(Ordering::SeqCst) {}
    }
    pub fn check(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    pub fn unlock(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

/// Associate Functions for Catnip LibOS
impl CatnipLibOS {
    pub fn start(config: &Config, nr_queues: u16) -> Result<MemoryManager, Fail> {
        load_mlx_driver();

        // Initializes the DPDKRuntime
        let (memory_manager, port_id) = DPDKRuntime::init_dpdk(
            &config.eal_init_args(),
            config.use_jumbo_frames(),
        ).unwrap();

        // Initializes the DPDK port and queues
        DPDKRuntime::init_dpdk_port(
            port_id, 
            &memory_manager, 
            config.use_jumbo_frames(), 
            config.mtu(), 
            config.tcp_checksum_offload(),
            config.udp_checksum_offload(),
            nr_queues
        ).unwrap();

        Ok(memory_manager)
    }

    pub fn new(config: &Config, queue_id: u16, nr_scratches_per_libos: u16, mm: Arc<MemoryManager>) -> Self {
        let rt: Rc<DPDKRuntime> = Rc::new(DPDKRuntime::new(
            config.local_ipv4_addr(),
            config.arp_table(),
            config.disable_arp(),
            config.mss(),
            config.tcp_checksum_offload(),
            config.udp_checksum_offload(),
            0u16,
            queue_id,
            mm,
        ));
        let now: Instant = Instant::now();
        let clock: TimerRc = TimerRc(Rc::new(Timer::new(now)));
        let scheduler: Scheduler = Scheduler::default();
        let rng_seed: [u8; 32] = [0; 32];
        
        let mut scratches: Vec<SharedMemory> = Vec::<SharedMemory>::with_capacity(nr_scratches_per_libos.try_into().unwrap());

        for i in 0..nr_scratches_per_libos {
            let scratch_name: String = "scratch".to_owned() + queue_id.to_string().as_str() + i.to_string().as_str();
            let shm: SharedMemory = match SharedMemory::create(&scratch_name, SCRATCH_SIZE) {
                Ok(shm) => shm,
                Err(_) => panic!("creating a shared memory region with valis size should be possible"),
            };

            unsafe {
                let mut msg: *mut CatnipMessage = shm.as_ptr() as *mut u8 as *mut CatnipMessage;
                (*msg).ring_rx = Box::into_raw(Box::new(RingBuffer::<(QDesc, *mut rte_mbuf)>::new(RING_BUFFER_CAPACITY).unwrap()));
                (*msg).ring_tx = Box::into_raw(Box::new(RingBuffer::<(QDesc, *mut rte_mbuf)>::new(RING_BUFFER_CAPACITY).unwrap()));
            }

            scratches.push(shm);
        }

        let inetstack: InetStack = InetStack::new(
            rt.clone(),
            scheduler.clone(),
            clock,
            rt.link_addr,
            rt.ipv4_addr,
            rt.udp_options.clone(),
            rt.tcp_options.clone(),
            rng_seed,
            rt.arp_options.clone(),
        )
        .unwrap();

        CatnipLibOS {
            inetstack,
            scheduler,
            rt,
            scratches,
        }
    }

    pub fn run(&mut self, local: SocketAddrV4) -> ! {
        // Setup peer.
        let sockqd: QDesc = match self.socket(libc::AF_INET, libc::SOCK_STREAM, 0) {
            Ok(qd) => qd,
            Err(e) => panic!("failed to create socket: {:?}", e.cause),
        };
        
        match self.bind(sockqd, local) {
            Ok(()) => (),
            Err(e) => panic!("bind failed: {:?}", e.cause),
        };

        // Mark as a passive one.
        match self.listen(sockqd, 256) {
            Ok(()) => (),
            Err(e) => panic!("listen failed: {:?}", e.cause),
        };
        
        let mut nr_pending: u64 = 0;
        let n: usize = self.scratches.len();

        assert!(n <= MAX_WORKERS);

        let mut idle_list: ArrayVec<bool, MAX_WORKERS> = ArrayVec::<bool, MAX_WORKERS>::new();
        let mut queued_workers: ArrayVec<usize, MAX_WORKERS> = ArrayVec::<usize, MAX_WORKERS>::new();
        let mut queued_work: ArrayVec<(QDesc, *mut rte_mbuf), MAX_QUEUED_WORK_SIZE> = ArrayVec::<(QDesc, *mut rte_mbuf), MAX_QUEUED_WORK_SIZE>::new();

        for i in 0..n {
            idle_list.push(true);
            queued_workers.push(i);
        }

        let port_id: u16 = self.rt.port_id;
        let queue_id: u16 = self.rt.queue_id;
        let mut qds: [QDesc; MAX_WORKERS] = unsafe { std::mem::zeroed() };
        let mut sgas: [demi_sgarray_t; MAX_WORKERS] = unsafe { std::mem::zeroed() };
        let mut packets: [*mut rte_mbuf; MAX_WORKERS] = unsafe { std::mem::zeroed() };

        loop {
            let mut idx: u16 = 0;
            for worker_idx in 0..n {
                let msg: *mut CatnipMessage = self.scratches[worker_idx].as_ptr() as *mut u8 as *mut CatnipMessage;

                unsafe {
                    if idle_list[worker_idx] == true || (*(*msg).ring_tx).is_empty() {
                        continue;
                    }

                    let (qd, buf_ptr): (QDesc, *mut rte_mbuf) = (*(*msg).ring_tx).try_dequeue().unwrap();

                    let buf: DemiBuffer = DemiBuffer::from_mbuf(buf_ptr);
                    let sga: demi_sgarray_t = self.rt.into_sgarray(buf).unwrap();
                    // self.push(qd, &sga).unwrap();
                    // self.sgafree(sga).unwrap();

                    match self.push2(qd, &sga) {
                        Ok(pkt) => {
                            packets[idx as usize] = pkt;
                            sgas[idx as usize] = sga;
                            qds[idx as usize] = qd;
                            idx += 1;
                        },
                        Err(_) => {}
                    };
                    
                    if ! queued_work.is_empty() {
                        let (qd, buf_ptr): (QDesc, *mut rte_mbuf) = queued_work.remove(0);
                        (*(*msg).ring_rx).try_enqueue((qd, buf_ptr)).ok();
                        idle_list[worker_idx] = false;
                    } else {
                        queued_workers.push(worker_idx);
                        idle_list[worker_idx] = true;
                    }
                }
            }

            if idx != 0 {
                unsafe { rte_eth_tx_burst(port_id, queue_id, packets.as_mut_ptr(), idx) };
                for j in 0..idx {
                    self.sgafree(sgas[j as usize]).unwrap();
                    self.pop(qds[j as usize], None).unwrap();
                }
            }

            if nr_pending < 1 {
                // Accept incoming connections.
                self.accept(sockqd).unwrap();
                nr_pending += 1;
            }

            let completed: Vec<u64> = self.poll_bg_work();

            for key in completed {   
                let handle: SchedulerHandle = self.scheduler.from_raw_handle(key).unwrap();
                let (qd, r): (QDesc, OperationResult) = self.take_operation(handle);
                let qr: demi_qresult_t = pack_result(self.rt.clone(), r, qd, key);

                match qr.qr_opcode {
                    demi_opcode_t::DEMI_OPC_POP => {
                        let qd: QDesc = qr.qr_qd.into();
                        let sga: demi_sgarray_t = unsafe { qr.qr_value.sga };

                        let buf: DemiBuffer = self.rt.clone_sgarray(&sga).unwrap();
                        let buf_ptr: *mut rte_mbuf = buf.into_mbuf().unwrap();

                        if ! queued_work.is_empty() || queued_workers.is_empty() {
                            queued_work.push((qd, buf_ptr));
                        } else {
                            let worker_idx: usize = queued_workers.remove(0);
                            let msg: *mut CatnipMessage = self.scratches[worker_idx].as_ptr() as *mut u8 as *mut CatnipMessage;
                            unsafe { (*(*msg).ring_rx).try_enqueue((qd, buf_ptr)).ok() };
                            idle_list[worker_idx] = false;
                        }

                        self.sgafree(sga).unwrap();
                    },
                    demi_opcode_t::DEMI_OPC_ACCEPT => {
                        // Pop first packet.
                        let qd: QDesc = unsafe { qr.qr_value.ares.qd.into() };
                        self.pop(qd, None).unwrap();
                        nr_pending -= 1;
                    },
                    demi_opcode_t::DEMI_OPC_PUSH => {
                        // Pop another packet.
                        let qd: QDesc = qr.qr_qd.into();
                        self.pop(qd, None).unwrap();
                    },
                    _ => panic!("unexpected result"),
                }
            }
        }
    }

    /// Create a push request for Demikernel to asynchronously write data from `sga` to the
    /// IO connection represented by `qd`. This operation returns immediately with a `QToken`.
    /// The data has been written when [`wait`ing](Self::wait) on the QToken returns.
    pub fn push(&mut self, qd: QDesc, sga: &demi_sgarray_t) -> Result<QToken, Fail> {
        #[cfg(feature = "profiler")]
        timer!("catnip::push");
        trace!("push(): qd={:?}", qd);
        match self.rt.clone_sgarray(sga) {
            Ok(buf) => {
                if buf.len() == 0 {
                    return Err(Fail::new(libc::EINVAL, "zero-length buffer"));
                }
                let future = self.do_push(qd, buf)?;
                let handle: SchedulerHandle = match self.scheduler.insert(future) {
                    Some(handle) => handle,
                    None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                };
                let qt: QToken = handle.into_raw().into();
                Ok(qt)
            },
            Err(e) => Err(e),
        }
    }

    pub fn push2(&mut self, qd: QDesc, sga: &demi_sgarray_t) -> Result<*mut rte_mbuf, Fail> {
        #[cfg(feature = "profiler")]
        timer!("catnip::push");
        trace!("push(): qd={:?}", qd);
        match self.rt.clone_sgarray(sga) {
            Ok(buf) => {
                if buf.len() == 0 {
                    return Err(Fail::new(libc::EINVAL, "zero-length buffer"));
                }

                self.do_push2(qd, buf)
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
                let handle: SchedulerHandle = match self.scheduler.insert(future) {
                    Some(handle) => handle,
                    None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                };
                let qt: QToken = handle.into_raw().into();
                Ok(qt)
            },
            Err(e) => Err(e),
        }
    }

    pub fn schedule(&mut self, qt: QToken) -> Result<SchedulerHandle, Fail> {
        match self.scheduler.from_raw_handle(qt.into()) {
            Some(handle) => Ok(handle),
            None => return Err(Fail::new(libc::EINVAL, "invalid queue token")),
        }
    }

    pub fn pack_result(&mut self, handle: SchedulerHandle, qt: QToken) -> Result<demi_qresult_t, Fail> {
        let (qd, r): (QDesc, OperationResult) = self.take_operation(handle);
        Ok(pack_result(self.rt.clone(), r, qd, qt.into()))
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
    type Target = InetStack;

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
