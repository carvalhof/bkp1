// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//==============================================================================
// Imports
//==============================================================================

use super::DPDKRuntime;
use crate::{
    inetstack::protocols::ethernet2::MIN_PAYLOAD_SIZE,
    runtime::{
        libdpdk::{
            rte_eth_rx_burst,
            rte_eth_tx_burst,
            rte_mbuf,
            rte_pktmbuf_prepend,
            rte_pktmbuf_headroom,
        },
        memory::DemiBuffer,
        network::{
            consts::RECEIVE_BATCH_SIZE,
            NetworkRuntime,
            PacketBuf,
        },
    },
};
use ::arrayvec::ArrayVec;
use ::std::mem;

#[cfg(feature = "profiler")]
use crate::timer;

//==============================================================================
// Trait Implementations
//==============================================================================

/// Network Runtime Trait Implementation for DPDK Runtime
impl NetworkRuntime for DPDKRuntime {
    fn transmit(&self, buf: Box<dyn PacketBuf>) {
        if let Some(body) = buf.take_body() {
            // Get the body mbuf.
            let mut body_mbuf: *mut rte_mbuf = if body.is_dpdk_allocated() {
                // The body is already stored in an MBuf, just extract it from the DemiBuffer.
                body.into_mbuf().expect("'body' should be DPDK-allocated")
            } else {
                // The body is not dpdk-allocated, allocate a DPDKBuffer and copy the body into it.
                let mut mbuf: DemiBuffer = match self.mm.alloc_body_mbuf() {
                    Ok(mbuf) => mbuf,
                    Err(e) => panic!("failed to allocate body mbuf: {:?}", e.cause),
                };
                assert!(mbuf.len() >= body.len());
                mbuf[..body.len()].copy_from_slice(&body[..]);
                mbuf.trim(mbuf.len() - body.len()).unwrap();
                mbuf.into_mbuf().expect("mbuf should not be empty")
            };

            let header_size: usize = buf.header_size();
            let headroom_size: usize = unsafe { rte_pktmbuf_headroom(body_mbuf) as usize };
            assert!(header_size < headroom_size);

            let hdr_ptr: *mut u8 = unsafe { rte_pktmbuf_prepend(body_mbuf, header_size.try_into().unwrap()) as *mut u8};

            let headers: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(hdr_ptr, header_size) };
            buf.write_header(headers);

            let num_sent: u16 = unsafe { rte_eth_tx_burst(self.port_id, self.queue_id, &mut body_mbuf, 1) };
            assert_eq!(num_sent, 1);
        } else {
            let mut header_mbuf: DemiBuffer = match self.mm.alloc_header_mbuf() {
                Ok(mbuf) => mbuf,
                Err(e) => panic!("failed to allocate header mbuf: {:?}", e.cause),
            };

            let header_size = buf.header_size();
            assert!(header_size <= header_mbuf.len());
            buf.write_header(&mut header_mbuf[..header_size]);

            if header_size < MIN_PAYLOAD_SIZE {
                let padding_bytes = MIN_PAYLOAD_SIZE - header_size;
                let padding_buf = &mut header_mbuf[header_size..][..padding_bytes];
                for byte in padding_buf {
                    *byte = 0;
                }
            }
            let frame_size = std::cmp::max(header_size, MIN_PAYLOAD_SIZE);
            header_mbuf.trim(header_mbuf.len() - frame_size).unwrap();
            let mut header_mbuf_ptr: *mut rte_mbuf = header_mbuf.into_mbuf().expect("mbuf cannot be empty");
            
            let num_sent: u16 = unsafe { rte_eth_tx_burst(self.port_id, self.queue_id, &mut header_mbuf_ptr, 1) };
            assert_eq!(num_sent, 1);
        }
    }

    fn receive(&self) -> ArrayVec<DemiBuffer, RECEIVE_BATCH_SIZE> {
        let mut out = ArrayVec::new();

        let mut packets: [*mut rte_mbuf; RECEIVE_BATCH_SIZE] = unsafe { mem::zeroed() };
        let nb_rx = unsafe {
            #[cfg(feature = "profiler")]
            timer!("catnip_libos::receive::rte_eth_rx_burst");

            rte_eth_rx_burst(self.port_id, self.queue_id, packets.as_mut_ptr(), RECEIVE_BATCH_SIZE as u16)
        };
        assert!(nb_rx as usize <= RECEIVE_BATCH_SIZE);

        {
            #[cfg(feature = "profiler")]
            timer!("catnip_libos:receive::for");
            for &packet in &packets[..nb_rx as usize] {
                // Safety: `packet` is a valid pointer to a properly initialized `rte_mbuf` struct.
                let buf: DemiBuffer = unsafe { DemiBuffer::from_mbuf(packet) };
                out.push(buf);
            }
        }

        out
    }
}
