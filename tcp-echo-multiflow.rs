// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//======================================================================================================================
// Imports
//======================================================================================================================

#![feature(test)]
extern crate test;

use ::anyhow::Result;
use arrayvec::ArrayVec;
use ::demikernel::{
    catnip::{
        CatnipLibOS,
        runtime::{
            DPDKRuntime,
            memory::MemoryManager,
        },
    },
    runtime::{
        libdpdk::{
            rte_mbuf,
            rte_eth_rx_burst,
            rte_eal_mp_wait_lcore,
            rte_eal_remote_launch,
        },
        memory::DemiBuffer,
        queue::IoQueueTable,
        network::consts::RECEIVE_BATCH_SIZE,
        network::config::TcpConfig,
    },
    demikernel::config::Config,
    demi_sgarray_t,
    runtime::types::demi_opcode_t,
    LibOS,
    LibOSName,
    QDesc,
    QToken,
    collections::ring::RingBuffer,
    inetstack::protocols::{
        queue::InetQueue,
        tcp::{
            queue::TcpQueue,
            peer::{
                Socket,
                SocketId
            },
            segment::TcpHeader,
            passive_open::PassiveSocket,
            established::{
                ControlBlock,
                EstablishedSocket
            },
        },
        ipv4::Ipv4Header,
        ethernet2::Ethernet2Header,
    },
};
use rand::{
    rngs::{
        self,
        StdRng
    },
    SeedableRng, 
    Rng
};
use std::{collections::HashMap, net::Ipv4Addr};
use ::std::{
    env,
    sync::{
        Arc,
        atomic::{
            Ordering,
            AtomicBool,
        },
    },
    net::SocketAddrV4,
    str::FromStr,
};
use log::trace;

#[cfg(target_os = "windows")]
pub const AF_INET: i32 = windows::Win32::Networking::WinSock::AF_INET.0 as i32;

#[cfg(target_os = "windows")]
pub const SOCK_STREAM: i32 = windows::Win32::Networking::WinSock::SOCK_STREAM as i32;

#[cfg(target_os = "linux")]
pub const AF_INET: i32 = libc::AF_INET;

#[cfg(target_os = "linux")]
pub const SOCK_STREAM: i32 = libc::SOCK_STREAM;

#[cfg(feature = "profiler")]
use ::demikernel::perftools::profiler;

//======================================================================================================================
// Constants
//======================================================================================================================

const MAX_WORKERS: usize = 32;
const MAX_WORK_QUEUE: usize = 1024;

//======================================================================================================================
// Structures
//======================================================================================================================

pub struct TASLock {
    busy: AtomicBool,
}

enum WorkerState {
    IDLE,
    ALREADYIDLE,
    BUSY,
}

pub enum FakeWorker {
    Sqrt,
    Multiplication,
    StridedMem(Vec<u8>, usize),
    PointerChase(Vec<usize>),
    RandomMem(Vec<u8>, Vec<usize>),
    StreamingMem(Vec<u8>),
}

pub struct DispatcherArg {
    addr: SocketAddrV4,
    nr_workers: u16,
    rings: *mut ArrayVec<*mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>, MAX_WORKERS>,
    idle_workers_ptr: *mut ArrayVec::<*mut WorkerState, MAX_WORKERS>,
    spinlock: *mut Arc<TASLock>,
    mm: Arc<MemoryManager>,
}

pub struct WorkerArg {
    worker_id: u16,
    idle_worker_ptr: *mut WorkerState,
    spec: Arc<String>,
    ring: *mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>,
    mm: Arc<MemoryManager>,
}


//======================================================================================================================
// TASLock
//======================================================================================================================
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
        while self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst) != Ok(true) {}
    }
    pub fn test(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    pub fn unlock(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

//======================================================================================================================
// FakeWorker
//======================================================================================================================
impl FakeWorker {
    pub fn create(spec: &str) -> Result<Self, &str> {
        let mut rng: StdRng = rngs::StdRng::from_seed([0 as u8; 32]);

        let tokens: Vec<&str> = spec.split(":").collect();
        assert!(tokens.len() > 0);

        match tokens[0] {
            "sqrt" => Ok(FakeWorker::Sqrt),
            "multiplication" => Ok(FakeWorker::Multiplication),
            "stridedmem" | "randmem" | "memstream" | "pointerchase" => {
                assert!(tokens.len() > 1);
                let size: usize = tokens[1].parse().unwrap();
                let buf = (0..size).map(|_| rng.gen()).collect();
                match tokens[0] {
                    "stridedmem" => {
                        assert!(tokens.len() > 2);
                        let stride: usize = tokens[2].parse().unwrap();
                        Ok(FakeWorker::StridedMem(buf, stride))
                    }
                    "pointerchase" => {
                        assert!(tokens.len() > 2);
                        let seed: u64 = tokens[2].parse().unwrap();
                        let mut rng: StdRng = rngs::StdRng::from_seed([seed as u8; 32]);
                        let nwords = size / 8;
                        let buf: Vec<usize> = (0..nwords).map(|_| rng.gen::<usize>() % nwords).collect();
                        Ok(FakeWorker::PointerChase(buf))
                    }
                    "randmem" => {
                        let sched = (0..size).map(|_| rng.gen::<usize>() % size).collect();
                        Ok(FakeWorker::RandomMem(buf, sched))
                    }
                    "memstream" => Ok(FakeWorker::StreamingMem(buf)),
                    _ => unreachable!(),
                }
            }
            _ => Err("bad fakework spec"),
        }
    }

    fn warmup_cache(&self) {
        match *self {
            FakeWorker::RandomMem(ref buf, ref sched) => {
                for i in 0..sched.len() {
                    test::black_box::<u8>(buf[sched[i]]);
                }
            }
            FakeWorker::StridedMem(ref buf, _stride) => {
                for i in 0..buf.len() {
                    test::black_box::<u8>(buf[i]);
                }
            }
            FakeWorker::PointerChase(ref buf) => {
                for i in 0..buf.len() {
                    test::black_box::<usize>(buf[i]);
                }
            }
            FakeWorker::StreamingMem(ref buf) => {
                for i in 0..buf.len() {
                    test::black_box::<u8>(buf[i]);
                }
            }
            _ => (),
        }
    }

    fn time(&self, iterations: u64, ticks_per_ns: f64) -> u64 {
        let rounds: usize = 50;
        let mut sum: f64 = 0.0;

        for _ in 0..rounds {
            let seed: u64 = rand::thread_rng().gen::<u64>();
            self.warmup_cache();
            let t0 = unsafe { x86::time::rdtsc() };
            self.work(iterations, seed);
            let t1 = unsafe { x86::time::rdtsc() };

            sum += ((t1 - t0) as f64)/ticks_per_ns;
        }

        (sum/(rounds as f64)) as u64
    }

    pub fn calibrate(&self, target_ns: u64, ticks_per_ns: f64) -> u64 {
        match *self {
            _ => {
                let mut iterations: u64 = 1;

                while self.time(iterations, ticks_per_ns) < target_ns {
                    iterations *= 2;
                }
                while self.time(iterations, ticks_per_ns) > target_ns {
                    iterations -= 1;
                }

                println!("{} ns: {} iterations", target_ns, iterations);

                iterations
            }
        }
    }

    pub fn work(&self, iters: u64, randomness: u64) {
        match *self {
            FakeWorker::Sqrt => {
                let k = 2350845.545;
                for i in 0..iters {
                    test::black_box(f64::sqrt(k * i as f64));
                }
            }
            FakeWorker::Multiplication => {
                let k = randomness;
                for i in 0..iters {
                    test::black_box(k * i);
                }
            }
            FakeWorker::StridedMem(ref buf, stride) => {
                let mut idx = randomness as usize % buf.len();
                let blen = buf.len();
                for _i in 0..iters as usize {
                    test::black_box::<u8>(buf[idx]);
                    idx += stride;
                    if idx >= blen {
                        idx -= blen;
                    }
                }
            }
            FakeWorker::RandomMem(ref buf, ref sched) => {
                for i in 0..iters as usize {
                    test::black_box::<u8>(buf[sched[i % sched.len()]]);
                }
            }
            FakeWorker::PointerChase(ref buf) => {
                let mut idx = randomness as usize % buf.len();
                for _i in 0..iters {
                    idx = buf[idx];
                    test::black_box::<usize>(idx);
                }
            }
            FakeWorker::StreamingMem(ref buf) => {
                for _ in 0..iters {
                    for i in (0..buf.len()).step_by(64) {
                        test::black_box::<u8>(buf[i]);
                    }
                }
            }
        }
    }
}

//======================================================================================================================
// wrappers
//======================================================================================================================

extern "C" fn dispatcher_wrapper(data: *mut std::os::raw::c_void) -> i32 {
    let arg: &mut DispatcherArg = unsafe { &mut *(data as *mut DispatcherArg) };

    let mut dispatcher = Dispatcher::new(arg).unwrap();

    dispatcher.run();

    #[allow(unreachable_code)]
    0
}

extern "C" fn worker_wrapper(data: *mut std::os::raw::c_void) -> i32 {
    let arg: &mut WorkerArg = unsafe { &mut *(data as *mut WorkerArg) };

    let mut worker = Worker::new(arg).unwrap();

    worker.run();

    #[allow(unreachable_code)]
    0
}

//======================================================================================================================
// Dispatcher
//======================================================================================================================

pub struct Dispatcher {
    addr: SocketAddrV4,
    nr_workers: u16,
    rings: *mut ArrayVec<*mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>, MAX_WORKERS>,
    idle_workers_ptr: *mut ArrayVec::<*mut WorkerState, MAX_WORKERS>,
    //
    rt: Arc<DPDKRuntime>,
    addresses: HashMap::<SocketId, QDesc>,
    qtable: IoQueueTable::<InetQueue<RECEIVE_BATCH_SIZE>>,
    queue_idle_worker: ArrayVec<usize, MAX_WORKERS>,
}   

impl Dispatcher {
    pub fn new(args: &mut DispatcherArg) -> Result<Self> {
        let mut queue_idle_worker: ArrayVec<usize, MAX_WORKERS> = ArrayVec::<usize, MAX_WORKERS>::new();

        let addr: SocketAddrV4 = args.addr;
        let nr_workers: u16 = args.nr_workers;

        for w in 0..nr_workers {
            queue_idle_worker.push(w as usize);
        } 

        let port_id: u16 = 0;
        let queue_id: u16 = 0;
        let mm: Arc<MemoryManager> = args.mm.clone();

        // Read in configuration file.
        let config_path: String = std::env::var("CONFIG_PATH").unwrap();
        let config: Config = Config::new(config_path);
        let rt: Arc<DPDKRuntime> = Arc::new(DPDKRuntime::new(
            config.local_ipv4_addr(),
            config.arp_table(),
            config.disable_arp(),
            config.mss(),
            config.tcp_checksum_offload(),
            config.udp_checksum_offload(),
            port_id, 
            queue_id, 
            mm,
            std::ptr::null_mut(),
        ));

        let addresses: HashMap::<SocketId, QDesc> = HashMap::<SocketId, QDesc>::new();
        let qtable: IoQueueTable::<InetQueue<RECEIVE_BATCH_SIZE>> = IoQueueTable::<InetQueue<RECEIVE_BATCH_SIZE>>::new();

        let mut dispatcher: Dispatcher = Self { 
            addr,
            nr_workers,
            rings: args.rings,
            idle_workers_ptr: args.idle_workers_ptr,
            rt,
            addresses,
            qtable,
            queue_idle_worker,
        };

        dispatcher.prepare();

        let spinlock: *mut Arc<TASLock> = args.spinlock;
        unsafe { (*spinlock).set() };

        Ok(dispatcher)
    }

    fn prepare(&mut self) {
        let nonce: u32 = 7;
        let backlog: usize = 128;

        let mut tcp_queue: TcpQueue<RECEIVE_BATCH_SIZE> = TcpQueue::new();
        let socket = PassiveSocket::new(
            self.addr,
            backlog,
            Some(self.rt.clone()),
            TcpConfig::default(),
            nonce,
        );
        tcp_queue.set_socket(Socket::Listening(socket));
        let qd: QDesc = self.qtable.alloc(InetQueue::Tcp(tcp_queue));
        self.addresses.insert(SocketId::Passive(self.addr), qd);
    }

    fn get_5tuple(pkt: *mut rte_mbuf) -> (SocketAddrV4, SocketAddrV4) {
        //Assuming Ethernet, IP and TCP
        let header_size = 14+20+20 as usize;
        let buf: DemiBuffer = unsafe { DemiBuffer::from_mbuf(pkt) };

        let ip_offset: usize = 14 as usize;
        let hdr_buf: &[u8] = &buf[ip_offset..header_size];
        // Source address.
        let src_addr: Ipv4Addr = Ipv4Addr::new(hdr_buf[12], hdr_buf[13], hdr_buf[14], hdr_buf[15]);
        // Destination address.
        let dst_addr: Ipv4Addr = Ipv4Addr::new(hdr_buf[16], hdr_buf[17], hdr_buf[18], hdr_buf[19]);

        let tcp_offset: usize = ip_offset+20 as usize;
        let hdr_buf: &[u8] = &buf[tcp_offset..header_size];
        // Source port.
        let src_port: u16 = u16::from_be_bytes([hdr_buf[0], hdr_buf[1]]);
        // Destination port.
        let dst_port: u16 = u16::from_be_bytes([hdr_buf[2], hdr_buf[3]]);

        let local: SocketAddrV4 = SocketAddrV4::new(dst_addr, dst_port);
        let remote: SocketAddrV4 = SocketAddrV4::new(src_addr, src_port);

        (local, remote)
    }

    fn get_qd(&mut self, local: SocketAddrV4, remote: SocketAddrV4) -> Option<QDesc> {
        match self.addresses.get(&SocketId::Active(local, remote)) {
            Some(qdesc) => Some(*qdesc),
            None => match self.addresses.get(&SocketId::Passive(local)) {
                Some(qdesc) => Some(*qdesc),
                None => None
            },
        }
    }

    fn run(&mut self) -> ! {
        let port_id: u16 = 0;
        let queue_id: u16 = 0;
        let mut rx_pkts: [*mut rte_mbuf; RECEIVE_BATCH_SIZE] = unsafe { std::mem::zeroed() };
        let mut queue_work: ArrayVec<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>), MAX_WORK_QUEUE> = ArrayVec::<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>), MAX_WORK_QUEUE>::new();

        loop {
            if !self.queue_idle_worker.is_empty() {
                for (pkt, cb) in queue_work.iter_mut() {
                    let w: usize = self.queue_idle_worker.remove(0);
                    log::debug!("Enqueuing from queue_work to worker {:?}", w);
    
                    unsafe {
                        let ring_ptr: *mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)> = (*self.rings)[w as usize];
                        (*(*self.idle_workers_ptr)[w]) = WorkerState::BUSY;
                        (*ring_ptr).try_enqueue((*pkt, *cb)).ok();
                    } 
    
                    if self.queue_idle_worker.is_empty() {
                        break;
                    }
                }
            }

            let nr_rx_pkts = unsafe { rte_eth_rx_burst(port_id, queue_id, rx_pkts.as_mut_ptr(), RECEIVE_BATCH_SIZE as u16) };
            if nr_rx_pkts != 0 {
                log::trace!("Received {:?} packets", nr_rx_pkts);
            
                for i in 0..nr_rx_pkts {
                    let pkt = rx_pkts[i as usize];
                    let (local, remote) = Dispatcher::get_5tuple(pkt);
                    let buf: DemiBuffer = unsafe { DemiBuffer::from_mbuf(pkt) };
    
                    let qd: QDesc = match self.get_qd(local, remote) {
                        Some(qd) => qd,
                        None => panic!("Not be here")
                    };
    
                    match self.qtable.get_mut(&qd) {
                        Some(InetQueue::Tcp(queue)) => {
                            match queue.get_mut_socket() {
                                Socket::Established(socket) => {
                                    log::debug!("Routing to established connection: {:?}", socket.endpoints());
    
                                    if self.queue_idle_worker.is_empty() == true {
                                        log::debug!("Enqueue to QUEUE_WORK");
                                        queue_work.push((pkt, socket.cb));
                                    } else {
                                        let w = self.queue_idle_worker.remove(0);
                                        log::debug!("Enqueue to worker {:?}", w);
    
                                        unsafe {
                                            let ring_ptr = (*self.rings)[w as usize];
                                            (*(*self.idle_workers_ptr)[w]) = WorkerState::BUSY;
                                            (*ring_ptr).try_enqueue((pkt, socket.cb)).ok();
                                        } 
                                        log::debug!("Enqueue DONE");
                                    }
                                },
                                Socket::Listening(socket) => {
                                    log::debug!("Routing to passive connection: {:?}", local);
                                    log::trace!("Dispatcher receives a SYN or ACK on LISTEN state");

                                    let (eth_hdr, payload) = Ethernet2Header::parse(buf).unwrap();
                                    let (ip_hdr, payload) = Ipv4Header::parse(payload).unwrap();
                                    let (tcp_hdr, _) = TcpHeader::parse(&ip_hdr, payload, true).unwrap();
    
                                    match socket.receive(qd, &eth_hdr, &ip_hdr, &tcp_hdr) {
                                        Ok(opt_cb) => {
                                            match opt_cb {
                                                Some(cb) => {
                                                    //Received ACK
                                                    // let cb = unsafe { *Box::from_raw(cb) };
                                                    let new_qd: QDesc = self.qtable.alloc(InetQueue::Tcp(TcpQueue::new()));
                                                    let established: EstablishedSocket<RECEIVE_BATCH_SIZE> = EstablishedSocket::new(cb, new_qd);
                                                    
                                                    match self.qtable.get_mut(&new_qd) {
                                                        Some(InetQueue::Tcp(queue)) => queue.set_socket(Socket::Established(established)),
                                                        _ => todo!(),
                                                    };
                                                    log::trace!("{:?} {:?} -- {:?}", local, remote, new_qd);
    
                                                    if self.addresses.insert(SocketId::Active(local, remote), new_qd).is_some() {
                                                        panic!("duplicate queue descriptor in established sockets table");
                                                    }
                                                },
                                                None => (),
                                            }
                                        }
                                        Err(_) => todo!(),
                                    }
                                }
                                Socket::Inactive(_) => todo!(),
                                Socket::Closing(_) => todo!(),
                            }
                        }
                        _ => todo!(),
                    }
                }
            }

            for w in 0..self.nr_workers {
                unsafe {
                    match *(*self.idle_workers_ptr)[w as usize] {
                        WorkerState::IDLE => {
                            self.queue_idle_worker.push(w as usize);
                            log::debug!("Inserting the worker {:?} to idle list", w);
                            (*(*self.idle_workers_ptr)[w as usize]) = WorkerState::ALREADYIDLE
                        },
                        _ => {},
                    }
                }
            }
        }
    }
}
//======================================================================================================================
// Worker
//======================================================================================================================

pub struct Worker {
    worker_id: u16,
    libos: LibOS,
    fakework: FakeWorker,
    idle_ptr: *mut WorkerState,
}

impl Worker {
    pub fn new(args: &mut WorkerArg) -> Result<Self> {
        let worker_id: u16 = args.worker_id;
        let fakework: FakeWorker = FakeWorker::create(args.spec.as_str()).unwrap();
        let libos: LibOS = LibOS::new(LibOSName::Catnip, worker_id, args.mm.clone(), args.ring).unwrap();

        Ok(Self {
            worker_id,
            libos,
            fakework,
            idle_ptr: args.idle_worker_ptr,
        })
    }

    fn run(&mut self) -> ! {
        let mut pushed: Option<QToken> = None;
        
        loop {
            let results = self.libos.wait_push_pop(pushed);
            pushed = None;
            for (cb, qr) in results {
                match qr.qr_opcode {
                    // Pop completed
                    demi_opcode_t::DEMI_OPC_POP => {
                        trace!("Worker {:?}: pop completed qd={:?}", self.worker_id, qr.qr_qd);
                        let mut sga: demi_sgarray_t = unsafe { qr.qr_value.sga };
                        
                        unsafe {
                            let iterations: u64 = *(((sga.sga_segs[0].sgaseg_buf as *mut u8).offset(32)) as *mut u64);
                            let randomness: u64 = *(((sga.sga_segs[0].sgaseg_buf as *mut u8).offset(40)) as *mut u64);
                            self.fakework.work(iterations, randomness);
                        }
                        
                        let qt: QToken = self.libos.push(cb, &mut sga).unwrap();
                        pushed = Some(qt);
                        self.libos.sgafree(sga).unwrap();

                        unsafe { (*self.idle_ptr) = WorkerState::IDLE };
                    },
                    // Push completed
                    demi_opcode_t::DEMI_OPC_PUSH => trace!("Worker {:?}: push completed", self.worker_id),
                    // Any other
                    _ => panic!("Worker {:?}: Not be here", self.worker_id)
                }
            }
        }
    }
}

//======================================================================================================================
// main()
//======================================================================================================================

pub fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() >= 5 {
        if args[1] == "--server" {
            let addr: SocketAddrV4 = SocketAddrV4::from_str(&args[2])?;
            let lcores: Vec<&str> = args[3].split(":").collect();
            let spec: Arc<String> = Arc::new(args[5].clone());
            let nr_workers: u16 = u16::from_str(&args[4])?;

            let mm: Arc<MemoryManager> = Arc::new(CatnipLibOS::start(nr_workers).unwrap());
            let mut idle_workers = ArrayVec::<*mut WorkerState, MAX_WORKERS>::new();
            let mut rings: ArrayVec<*mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>, MAX_WORKERS> = ArrayVec::<*mut RingBuffer<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>, MAX_WORKERS>::new();

            for _ in 0..nr_workers {
                let ring = Box::new(RingBuffer::<(*mut rte_mbuf, *mut ControlBlock<RECEIVE_BATCH_SIZE>)>::new(2).unwrap());
                let ring_ptr = Box::into_raw(ring);

                rings.push(ring_ptr);
                idle_workers.push(Box::into_raw(Box::new(WorkerState::ALREADYIDLE)));
            }

            let rings_ptr = Box::into_raw(Box::new(rings));
            let idle_workers_ptr = Box::into_raw(Box::new(idle_workers));

            // Creating the Dispatcher
            let mut lcore_idx: usize = 1;
            let spinlock: Box<Arc<TASLock>> = Box::new(Arc::new(TASLock::new()));
            let mut arg: DispatcherArg = DispatcherArg {
                addr,
                nr_workers,
                rings: rings_ptr,
                idle_workers_ptr,
                spinlock: Box::into_raw(spinlock.clone()),
                mm: mm.clone(),
            };
            let lcore_id: u32 = u32::from_str(lcores[lcore_idx])?;
            lcore_idx += 1;
            let arg_ptr: *mut std::os::raw::c_void = &mut arg as *mut _ as *mut std::os::raw::c_void;
            unsafe { rte_eal_remote_launch(Some(dispatcher_wrapper), arg_ptr, lcore_id) };
            while !spinlock.test() { }

            // Creating the Worker
            for i in 0..nr_workers {
                let mut arg: WorkerArg = WorkerArg {
                    worker_id: i,
                    idle_worker_ptr: unsafe { (*idle_workers_ptr)[i as usize] },
                    spec: Arc::clone(&spec),
                    mm: mm.clone(),
                    ring: unsafe { (*rings_ptr)[i as usize] },
                };

                let lcore_id = u32::from_str(lcores[lcore_idx])?;
                lcore_idx += 1;
                let arg_ptr: *mut std::os::raw::c_void = &mut arg as *mut _ as *mut std::os::raw::c_void;
                unsafe { rte_eal_remote_launch(Some(worker_wrapper), arg_ptr, lcore_id) };
            }

            // Wait for all
            unsafe { rte_eal_mp_wait_lcore() };
        }
        
    }

    Ok(())
}