// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//======================================================================================================================
// Imports
//======================================================================================================================

#![feature(test)]
extern crate test;

use ::anyhow::Result;
use demikernel::runtime::memory::DemiBuffer;
use ::demikernel::{
    QDesc,
    LibOS,
    LibOSName,
    pal::linux::shm::SharedMemory,
    catnip::{
        TASLock,
        CatnipMessage,
        runtime::memory::MemoryManager,
    },
    runtime::libdpdk::{
        rte_mbuf,
        rte_lcore_count,
        rte_get_timer_hz,
        rte_eal_mp_wait_lcore,
        rte_eal_remote_launch,
        rte_tcp_hdr,
        rte_flow_attr,
        rte_flow_error,
        rte_flow_item,
        rte_flow_action,
        rte_flow_validate,
        rte_flow_create,
        rte_flow_action_queue,
        rte_flow_item_type_RTE_FLOW_ITEM_TYPE_ETH,
        rte_flow_item_type_RTE_FLOW_ITEM_TYPE_IPV4,
        rte_flow_item_type_RTE_FLOW_ITEM_TYPE_TCP,
        rte_flow_item_type_RTE_FLOW_ITEM_TYPE_END,
        rte_flow_action_type_RTE_FLOW_ACTION_TYPE_END,
        rte_flow_action_type_RTE_FLOW_ACTION_TYPE_QUEUE,
    },
};
use ::std::{
    env,
    mem,
    net::SocketAddrV4,
    panic,
    sync::Arc,
    str::FromStr,
};
use rand::{
    Rng,
    rngs::{self, StdRng},
    SeedableRng,
};

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

const SCRATCH_SIZE: usize = 128;

//======================================================================================================================
// Flow affinity (DPDK)
//======================================================================================================================

fn flow_affinity(nr_queues: u16) {
    unsafe {
        let n = 128;
        for i in 0..n {
            let mut err: rte_flow_error = mem::zeroed();

            let mut attr: rte_flow_attr = mem::zeroed();
            attr.set_egress(0);
            attr.set_ingress(1);

            let mut pattern: Vec<rte_flow_item> = vec![mem::zeroed(); 4];
            pattern[0].type_ = rte_flow_item_type_RTE_FLOW_ITEM_TYPE_ETH;
            pattern[1].type_ = rte_flow_item_type_RTE_FLOW_ITEM_TYPE_IPV4;
            pattern[2].type_ = rte_flow_item_type_RTE_FLOW_ITEM_TYPE_TCP;
            let mut flow_tcp: rte_tcp_hdr = mem::zeroed();
            let mut flow_tcp_mask: rte_tcp_hdr = mem::zeroed();
            flow_tcp.src_port = u16::to_be(i + 1);
            flow_tcp_mask.src_port = u16::MAX;
            pattern[2].spec = &mut flow_tcp as *mut _ as *mut std::os::raw::c_void;
            pattern[2].mask = &mut flow_tcp_mask as *mut _ as *mut std::os::raw::c_void;
            pattern[3].type_ = rte_flow_item_type_RTE_FLOW_ITEM_TYPE_END;

            let mut action: Vec<rte_flow_action> = vec![mem::zeroed(); 2];
            action[0].type_ = rte_flow_action_type_RTE_FLOW_ACTION_TYPE_QUEUE;
            let mut queue_action: rte_flow_action_queue = mem::zeroed();
            queue_action.index = i % nr_queues;
            action[0].conf = &mut queue_action as *mut _ as *mut std::os::raw::c_void;
            action[1].type_ = rte_flow_action_type_RTE_FLOW_ACTION_TYPE_END;

            rte_flow_validate(0, &attr, pattern.as_ptr(), action.as_ptr(), &mut err);

            rte_flow_create(0, &attr, pattern.as_ptr(), action.as_ptr(), &mut err);
        }
    }
}

//======================================================================================================================
// Structures
//======================================================================================================================

struct EchoMultiflowLibOSArg {
    local: SocketAddrV4,
    queue_id: u16, 
    nr_scratches_per_libos: u16,
    spinlock: *mut Arc<TASLock>,
    memory_manager: Arc<MemoryManager>,
}

struct EchoMultiflowAppArg {
    libos_id: u16, 
    app_id: u16,
    spec: Arc<String>,
}

pub enum FakeWorker {
    Sqrt,
    Multiplication,
    StridedMem(Vec<u8>, usize),
    PointerChase(Vec<usize>),
    RandomMem(Vec<u8>, Vec<usize>),
    StreamingMem(Vec<u8>),
}

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
// server()
//======================================================================================================================

extern "C" fn app_wrapper(data: *mut std::os::raw::c_void) -> i32 {
    let arg: &mut EchoMultiflowAppArg = unsafe { &mut *(data as *mut EchoMultiflowAppArg) };

    app(arg);

    #[allow(unreachable_code)]
    0
}

extern "C" fn dispatcher_wrapper(data: *mut std::os::raw::c_void) -> i32 {
    let arg: &mut EchoMultiflowLibOSArg = unsafe { &mut *(data as *mut EchoMultiflowLibOSArg) };

    dispatcher(arg);

    #[allow(unreachable_code)]
    0
}

fn dispatcher(args: &mut EchoMultiflowLibOSArg) {
    let queue_id: u16 = args.queue_id;
    let nr_queues: u16 = args.nr_scratches_per_libos;
    let mm: Arc<MemoryManager> = args.memory_manager.clone();
    let mut libos: LibOS = LibOS::new(queue_id, nr_queues, mm).unwrap();

    let spinlock: Arc<TASLock> = unsafe { (*(args.spinlock)).clone() };
    spinlock.set();

    let local: SocketAddrV4 = args.local;
    libos.run(local);
}

fn app(args: &mut EchoMultiflowAppArg) -> ! {
    let libos_id: u16 = args.libos_id;
    let app_id: u16 = args.app_id;

    // Create the scratch area
    let scratch_name: String = "scratch".to_owned() + libos_id.to_string().as_str() + app_id.to_string().as_str();
    let shm: SharedMemory = match SharedMemory::open(&scratch_name, SCRATCH_SIZE) {
        Ok(shm) => shm,
        Err(_) => panic!("creating a shared memory region with valis size should be possible"),
    };
    let msg: *mut CatnipMessage = shm.as_ptr() as *mut u8 as *mut CatnipMessage;

    // Create the fake worker
    let fakework: FakeWorker = FakeWorker::create(args.spec.as_str()).unwrap();

    loop {
        unsafe {
            let (qd, buf_ptr): (QDesc, *mut rte_mbuf) = (*(*msg).ring_rx).dequeue();
            
            let buf = DemiBuffer::from_mbuf(buf_ptr);
            let raw = buf.as_ptr();

            let iterations: u64 = *((raw.offset(32)) as *mut u64);
            let randomness: u64 = *((raw.offset(40)) as *mut u64);
            fakework.work(iterations, randomness);

            std::mem::forget(buf);
            (*(*msg).ring_tx).enqueue((qd, buf_ptr));
        }
    }
}

//======================================================================================================================
// usage()
//======================================================================================================================

/// Prints program usage and exits.
fn usage(program_name: &String) {
    println!("Usage:");
    println!("{} FAKEWORK MEAN calibrate\n", program_name);
    println!("\n");
    println!("{} MODE address CORES nr_liboses nr_apps FAKEWORK\n", program_name);
    println!("Modes:");
    println!("  --client    Run program in client mode.");
    println!("  --server    Run program in server mode.\n");
    println!("Fakework:\n");
    println!("  sqrt");
    println!("  randmem:1024");
    println!("  stridedmem:1024:7");
    println!("  streamingmem:1024");
    println!("  pointerchase:1024:7\n");
}

//======================================================================================================================
// main()
//======================================================================================================================

pub fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() == 4 {
        if args[3] == "calibrate" {
            match LibOSName::from_env() {
                Ok(LibOSName::Catnip) => LibOS::start(1).unwrap(),
                _ => panic!("Should be Catnip LibOS.")
            };
            let ticks_per_ns: f64 = unsafe { (rte_get_timer_hz() as f64) / (1000000000.0 as f64) };

            let mean: u64 = u64::from_str(&args[2])?;
            let fakework: FakeWorker = FakeWorker::create(args[1].as_str()).unwrap();
            fakework.calibrate(mean, ticks_per_ns);
        } else {
            usage(&args[0]);
        }

        return Ok(());
    }

    if args.len() >= 6 {
        if args[1] == "--server" {
            let nr_liboses: u16 = u16::from_str(&args[4])?;
            let nr_apps: u16 = u16::from_str(&args[5])?;

            let mm: MemoryManager = match LibOSName::from_env() {
                Ok(LibOSName::Catnip) => LibOS::start(nr_liboses).unwrap(),
                _ => panic!("Should be Catnip LibOS.")
            };

            unsafe {
                if rte_lcore_count() < ((nr_apps + nr_liboses + 1) as u32) {
                    panic!("The number of DPDK lcores should be at least {:?}", nr_apps + nr_liboses + 1);
                }
            }

            flow_affinity(nr_liboses);

            let sockaddr: SocketAddrV4 = SocketAddrV4::from_str(&args[2])?;
            let lcores: Vec<&str> = args[3].split(":").collect();
            
            let mut lcore_idx: usize = 1;
            let memory_manager: Arc<MemoryManager> = Arc::new(mm);
            let spec: Arc<String> = Arc::new(args[6].clone());
            let nr_scratches_per_libos: u16 = (nr_apps/nr_liboses).try_into().unwrap();

            for queue_id in 0..nr_liboses {
                //Starting the LibOS
                let spinlock: Box<Arc<TASLock>> = Box::new(Arc::new(TASLock::new()));
                let mut arg: EchoMultiflowLibOSArg = EchoMultiflowLibOSArg {
                    local: sockaddr.clone(),
                    queue_id,
                    nr_scratches_per_libos,
                    spinlock: Box::into_raw(spinlock.clone()),
                    memory_manager: memory_manager.clone(),
                };

                let mut lcore_id: u32 = u32::from_str(lcores[lcore_idx])?;
                lcore_idx += 1;
                let arg_ptr: *mut std::os::raw::c_void = &mut arg as *mut _ as *mut std::os::raw::c_void;
                unsafe { rte_eal_remote_launch(Some(dispatcher_wrapper), arg_ptr, lcore_id) };

                while !spinlock.check() { }

                for app_id in 0..nr_scratches_per_libos {
                    // Starting the App
                    let mut arg: EchoMultiflowAppArg = EchoMultiflowAppArg {
                        app_id,
                        libos_id: queue_id,
                        spec: Arc::clone(&spec),
                    };

                    lcore_id = u32::from_str(lcores[lcore_idx])?;
                    lcore_idx += 1;
                    let arg_ptr: *mut std::os::raw::c_void = &mut arg as *mut _ as *mut std::os::raw::c_void;
                    unsafe { rte_eal_remote_launch(Some(app_wrapper), arg_ptr, lcore_id) };
                }
            }

            unsafe { rte_eal_mp_wait_lcore() };
        }
    }

    usage(&args[0]);

    Ok(())
}
