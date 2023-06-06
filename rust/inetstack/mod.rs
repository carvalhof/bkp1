// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//==============================================================================
// Imports
//==============================================================================

use crate::{
    inetstack::protocols::{
        ethernet2::{
            Ethernet2Header,
        },
        tcp::{
            operations::{
                PushFuture, 
                PopFuture,
            },
            established::ControlBlock,
        },
        Peer,
    },
    runtime::{
        fail::Fail,
        memory::DemiBuffer,
        network::{
            config::{
                TcpConfig,
            },
            types::MacAddress,
            NetworkRuntime, 
            consts::RECEIVE_BATCH_SIZE,
        },
        queue::{
            Operation,
            OperationResult,
            OperationTask,
            QDesc,
            QToken,
        },
        timer::TimerRc,
    },
    scheduler::{
        Scheduler,
        TaskHandle,
    },
};
use ::libc::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use ::std::{
    net::{
        Ipv4Addr,
        SocketAddrV4,
    },
    pin::Pin,
    sync::Arc,
    time::Instant
};

#[cfg(feature = "profiler")]
use crate::timer;

//==============================================================================
// Exports
//==============================================================================

pub mod collections;
pub mod futures;
pub mod options;
pub mod protocols;

//======================================================================================================================
// Constants
//======================================================================================================================

const TIMER_RESOLUTION: usize = 64;

//======================================================================================================================
// Structures
//======================================================================================================================

pub struct TASLock {
    busy: AtomicBool,
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
        while self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst) != Ok(false) {}
    }
    pub fn test(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    pub fn unlock(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

pub struct InetStack<const N: usize> {
    ipv4: Peer<N>,
    rt: Arc<dyn NetworkRuntime<RECEIVE_BATCH_SIZE>>,
    local_link_addr: MacAddress,
    scheduler: Arc<Scheduler>,
    clock: TimerRc,
    ts_iters: usize,
}

impl<const N: usize> InetStack<N> {
    pub fn new(
        rt: Arc<dyn NetworkRuntime<RECEIVE_BATCH_SIZE>>,
        scheduler: Arc<Scheduler>,
        clock: TimerRc,
        local_link_addr: MacAddress,
        local_ipv4_addr: Ipv4Addr,
        tcp_config: TcpConfig,
    ) -> Result<Self, Fail> {
        let ipv4: Peer<N> = Peer::new(
            local_ipv4_addr,
            tcp_config,
        )?;
        Ok(Self {
            ipv4,
            rt,
            local_link_addr,
            scheduler,
            clock,
            ts_iters: 0,
        })
    }

    //======================================================================================================================
    // Associated Functions
    //======================================================================================================================

    // ///
    // /// **Brief**
    // ///
    // /// Looks up queue type based on queue descriptor
    // ///
    // fn lookup_qtype(&self, &qd: &QDesc) -> Option<QType> {
    //     None
    // }

    ///
    /// **Brief**
    ///
    /// Creates an endpoint for communication and returns a file descriptor that
    /// refers to that endpoint. The file descriptor returned by a successful
    /// call will be the lowest numbered file descriptor not currently open for
    /// the process.
    ///
    /// The domain argument specifies a communication domain; this selects the
    /// protocol family which will be used for communication. These families are
    /// defined in the libc crate. Currently, the following families are supported:
    ///
    /// - AF_INET Internet Protocol Version 4 (IPv4)
    ///
    /// **Return Vale**
    ///
    /// Upon successful completion, a file descriptor for the newly created
    /// socket is returned. Upon failure, `Fail` is returned instead.
    ///
    pub fn socket(&mut self, _domain: c_int, _socket_type: c_int, _protocol: c_int) -> Result<QDesc, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    ///
    /// **Brief**
    ///
    /// Binds the socket referred to by `qd` to the local endpoint specified by
    /// `local`.
    ///
    /// **Return Value**
    ///
    /// Upon successful completion, `Ok(())` is returned. Upon failure, `Fail` is
    /// returned instead.
    ///
    pub fn bind(&mut self, _qd: QDesc, _local: SocketAddrV4) -> Result<(), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    ///
    /// **Brief**
    ///
    /// Marks the socket referred to by `qd` as a socket that will be used to
    /// accept incoming connection requests using [accept](Self::accept). The `qd` should
    /// refer to a socket of type `SOCK_STREAM`. The `backlog` argument defines
    /// the maximum length to which the queue of pending connections for `qd`
    /// may grow. If a connection request arrives when the queue is full, the
    /// client may receive an error with an indication that the connection was
    /// refused.
    ///
    /// **Return Value**
    ///
    /// Upon successful completion, `Ok(())` is returned. Upon failure, `Fail` is
    /// returned instead.
    ///
    pub fn listen(&mut self, _qd: QDesc, _backlog: usize) -> Result<(), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    ///
    /// **Brief**
    ///
    /// Accepts an incoming connection request on the queue of pending
    /// connections for the listening socket referred to by `qd`.
    ///
    /// **Return Value**
    ///
    /// Upon successful completion, a queue token is returned. This token can be
    /// used to wait for a connection request to arrive. Upon failure, `Fail` is
    /// returned instead.
    ///
    pub fn accept(&mut self, _qd: QDesc) -> Result<QToken, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    ///
    /// **Brief**
    ///
    /// Connects the socket referred to by `qd` to the remote endpoint specified by `remote`.
    ///
    /// **Return Value**
    ///
    /// Upon successful completion, a queue token is returned. This token can be
    /// used to push and pop data to/from the queue that connects the local and
    /// remote endpoints. Upon failure, `Fail` is
    /// returned instead.
    ///
    pub fn connect(&mut self, _qd: QDesc, _remote: SocketAddrV4) -> Result<QToken, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    ///
    /// **Brief**
    ///
    /// Closes a connection referred to by `qd`.
    ///
    /// **Return Value**
    ///
    /// Upon successful completion, `Ok(())` is returned. Upon failure, `Fail` is
    /// returned instead.
    ///
    pub fn close(&mut self, _qd: QDesc) -> Result<(), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    ///
    /// **Brief**
    ///
    /// Asynchronously closes a connection referred to by `qd`.
    ///
    /// **Return Value**
    ///
    /// Upon successful completion, `Ok(())` is returned. This qtoken can be used to wait until the close
    /// completes shutting down the connection. Upon failure, `Fail` is returned instead.
    ///
    pub fn async_close(&mut self, _qd: QDesc) -> Result<QToken, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Pushes a buffer to a TCP socket.
    /// TODO: Rename this function to push() once we have a common representation across all libOSes.
    pub fn do_push(&mut self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, buf: DemiBuffer) -> Result<OperationTask, Fail> {
        unsafe { debug!("do_push(): qd={:?}", (*cb).qd) };

        let future: PushFuture<RECEIVE_BATCH_SIZE> = self.ipv4.tcp.push(cb, buf);
        let coroutine: Pin<Box<Operation>> = Box::pin(async move {
            // Wait for push to complete.
            let result: Result<(), Fail> = future.await;
            // Handle result.
            match result {
                Ok(()) => (cb, OperationResult::Push),
                Err(e) => (cb, OperationResult::Failed(e)),
            }
        });
        let task_id: String = unsafe { format!("Inetstack::TCP::push for qd={:?}", (*cb).qd) };
        Ok(OperationTask::new(task_id, coroutine))
    }

    /// Pushes raw data to a TCP socket.
    /// TODO: Move this function to demikernel repo once we have a common buffer representation across all libOSes.
    pub fn push2(&mut self, _qd: QDesc, _data: &[u8]) -> Result<QToken, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Pushes a buffer to a UDP socket.
    /// TODO: Rename this function to pushto() once we have a common buffer representation across all libOSes.
    pub fn do_pushto(&mut self, _qd: QDesc, _buf: DemiBuffer, _to: SocketAddrV4) -> Result<OperationTask, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Pushes raw data to a UDP socket.
    /// TODO: Move this function to demikernel repo once we have a common buffer representation across all libOSes.
    pub fn pushto2(&mut self, _qd: QDesc, _data: &[u8], _remote: SocketAddrV4) -> Result<QToken, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Create a pop request to write data from IO connection represented by `qd` into a buffer
    /// allocated by the application.
    pub fn pop(&mut self, _qd: QDesc, _size: Option<usize>) -> Result<QToken, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Waits for an operation to complete.
    #[deprecated]
    pub fn wait2(&mut self, _qt: QToken) -> Result<(QDesc, OperationResult), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Waits for any operation to complete.
    #[deprecated]
    pub fn wait_any2(&mut self, _qts: &[QToken]) -> Result<(usize, QDesc, OperationResult), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    /// Given a handle representing a task in our scheduler. Return the results of this future
    /// and the file descriptor for this connection.
    ///
    /// This function will panic if the specified future had not completed or is _background_ future.
    pub fn take_operation(&mut self, handle: TaskHandle) -> (*mut ControlBlock<RECEIVE_BATCH_SIZE>, OperationResult) {
        let task: OperationTask = OperationTask::from(self.scheduler.remove(handle).as_any());

        task.get_result().expect("Coroutine not finished")
    }

    /// New incoming data has arrived. Route it to the correct parse out the Ethernet header and
    /// allow the correct protocol to handle it. The underlying protocol will futher parse the data
    /// and inform the correct task that its data has arrived.
    fn do_receive(&mut self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, bytes: DemiBuffer) -> Result<(), Fail> {
        #[cfg(feature = "profiler")]
        timer!("inetstack::engine::receive");
        let (header, payload) = Ethernet2Header::parse(bytes)?;
        debug!("Engine received {:?}", header);
        if self.local_link_addr != header.dst_addr()
            && !header.dst_addr().is_broadcast()
            && !header.dst_addr().is_multicast()
        {
            return Err(Fail::new(libc::EINVAL, "physical destination address mismatch"));
        }
        self.ipv4.receive(cb, payload)
    }

    /// Scheduler will poll all futures that are ready to make progress.
    /// Then ask the runtime to receive new data which we will forward to the engine to parse and
    /// route to the correct protocol.
    pub fn poll_bg_work(&mut self) -> Vec<(*mut ControlBlock<RECEIVE_BATCH_SIZE>, QToken)> {
        let mut output = Vec::new();

        #[cfg(feature = "profiler")]
        timer!("inetstack::poll_bg_work");
        {
            #[cfg(feature = "profiler")]
            timer!("inetstack::poll_bg_work::poll");

            self.scheduler.poll();
        }
        {
            #[cfg(feature = "profiler")]
            timer!("inetstack::poll_bg_work::for");

            let batch = {
                #[cfg(feature = "profiler")]
                timer!("inetstack::poll_bg_work::for::receive");

                self.rt.receive()
            };

            {
                #[cfg(feature = "profiler")]
                timer!("inetstack::poll_bg_work::for::for");

                for (pkt, cb) in batch {
                    unsafe {
                        // (*cb).spinlock.lock();
                        // --------------------------------------
                        (*cb).set_clock(self.clock.clone());
                        (*cb).set_rt(self.rt.clone());
                        (*cb).set_scheduler(self.scheduler.clone());

                        let task_id: String = format!("Inetstack::TCP::pop for qd={:?}", (*cb).qd);
                        let future: PopFuture<RECEIVE_BATCH_SIZE> = self.ipv4.tcp.pop(cb, None);
                        let coroutine: Pin<Box<Operation>> = Box::pin(async move {
                            // Wait for pop to complete.
                            let result: Result<DemiBuffer, Fail> = future.await;
                            // Handle result.
                            match result {
                                Ok(buf) => (cb, OperationResult::Pop(None, buf)),
                                Err(e) => (cb, OperationResult::Failed(e)),
                            }
                        });

                        let handle: TaskHandle = match self.scheduler.insert(OperationTask::new(task_id, coroutine)) {
                            Some(handle) => handle,
                            None => panic!("ERROR"),
                        };
                        let qt: QToken = handle.get_task_id().into();
                        trace!("pop() qt={:?}", qt);
                        output.push((cb, qt));
                    }   

                    self.scheduler.poll();

                    let buf: DemiBuffer = unsafe { DemiBuffer::from_mbuf(pkt) };

                    if let Err(e) = self.do_receive(cb, buf) {
                        warn!("Dropped packet: {:?}", e);
                    }
                    // // TODO: This is a workaround for https://github.com/demikernel/inetstack/issues/149.
                    self.scheduler.poll();
                }
            }
        }

        if self.ts_iters == 0 {
            self.clock.advance_clock(Instant::now());
        }
        self.ts_iters = (self.ts_iters + 1) % TIMER_RESOLUTION;

        output
    }

    pub fn get_clock(&self) -> TimerRc {
        self.clock.clone()
    }
}
