// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

pub mod memory;
mod network;

//==============================================================================
// Imports
//==============================================================================

use self::memory::{
    consts::DEFAULT_MAX_BODY_SIZE,
    MemoryManager,
};
use crate::runtime::{
    libdpdk::{
        rte_eal_init,
        rte_eth_conf,
        rte_socket_id,
        rte_eth_dev_configure,
        rte_eth_dev_count_avail,
        rte_eth_dev_get_mtu,
        rte_eth_dev_info_get,
        rte_eth_dev_set_mtu,
        rte_eth_dev_start,
        rte_eth_macaddr_get,
        rte_eth_find_next_owned_by,
        rte_eth_promiscuous_enable,
        rte_eth_rx_mq_mode_RTE_ETH_MQ_RX_RSS as RTE_ETH_MQ_RX_RSS,
        rte_eth_rx_mq_mode_RTE_ETH_MQ_RX_NONE as RTE_ETH_MQ_RX_NONE,
        rte_eth_tx_mq_mode_RTE_ETH_MQ_TX_NONE as RTE_ETH_MQ_TX_NONE,
        rte_eth_rx_queue_setup,
        rte_eth_rxconf,
        rte_eth_txconf,
        rte_eth_tx_queue_setup,
        rte_ether_addr,
        rte_eth_rss_ip,
        rte_eth_rss_tcp,
        rte_eth_rss_udp,
        RTE_ETHER_MAX_LEN,
        RTE_ETH_DEV_NO_OWNER,
        RTE_PKTMBUF_HEADROOM,
        RTE_ETHER_MAX_JUMBO_FRAME_LEN,
        rte_eth_rx_offload_ip_cksum,
        rte_eth_tx_offload_ip_cksum,
        rte_eth_rx_offload_tcp_cksum,
        rte_eth_tx_offload_tcp_cksum,
        rte_eth_rx_offload_udp_cksum,
        rte_eth_tx_offload_udp_cksum,
    },
    network::{
        config::{
            ArpConfig,
            TcpConfig,
            UdpConfig,
        },
        types::MacAddress,
    },
    Runtime,
};
use ::anyhow::{
    bail,
    format_err,
    Error,
};
use ::std::{
    sync::Arc,
    collections::HashMap,
    ffi::CString,
    mem::MaybeUninit,
    net::Ipv4Addr,
    time::Duration,
};

//==============================================================================
// Macros
//==============================================================================

macro_rules! expect_zero {
    ($name:ident ( $($arg: expr),* $(,)* )) => {{
        let ret = $name($($arg),*);
        if ret == 0 {
            Ok(0)
        } else {
            Err(format_err!("{} failed with {:?}", stringify!($name), ret))
        }
    }};
}

//==============================================================================
// Structures
//==============================================================================

/// DPDK Runtime
#[derive(Clone)]
pub struct DPDKRuntime {
    mm: Arc<MemoryManager>,
    pub port_id: u16,
    pub queue_id: u16,
    pub link_addr: MacAddress,
    pub ipv4_addr: Ipv4Addr,
    pub arp_options: ArpConfig,
    pub tcp_options: TcpConfig,
    pub udp_options: UdpConfig,
}

//==============================================================================
// Associate Functions
//==============================================================================

/// Associate Functions for DPDK Runtime
impl DPDKRuntime {
    // Initializes DPDK environment
    pub fn init_dpdk(
        eal_init_args: &[CString],
        use_jumbo_frames: bool,
    ) -> Result<(MemoryManager, u16), Error> {
        let eal_init_refs = eal_init_args.iter().map(|s| s.as_ptr() as *mut u8).collect::<Vec<_>>();
        let ret: libc::c_int = unsafe { rte_eal_init(eal_init_refs.len() as i32, eal_init_refs.as_ptr() as *mut _) };
        if ret < 0 {
            let rte_errno: libc::c_int = unsafe { dpdk_rs::rte_errno() };
            bail!("EAL initialization failed (rte_errno={:?})", rte_errno);
        }
        let nb_ports: u16 = unsafe { rte_eth_dev_count_avail() };
        if nb_ports == 0 {
            bail!("No ethernet ports available");
        }
        eprintln!("DPDK reports that {} ports (interfaces) are available.", nb_ports);

        let max_body_size: usize = if use_jumbo_frames {
            (RTE_ETHER_MAX_JUMBO_FRAME_LEN + RTE_PKTMBUF_HEADROOM) as usize
        } else {
            DEFAULT_MAX_BODY_SIZE
        };

        let memory_manager: MemoryManager = MemoryManager::new(max_body_size).unwrap();

        let owner: u64 = RTE_ETH_DEV_NO_OWNER as u64;
        let port_id: u16 = unsafe { rte_eth_find_next_owned_by(0, owner) as u16 };

        Ok((memory_manager, port_id))
    }

    pub fn init_dpdk_port(
        port_id: u16,
        memory_manager: &MemoryManager,
        use_jumbo_frames: bool,
        mtu: u16,
        tcp_checksum_offload: bool,
        udp_checksum_offload: bool,
        nr_queues: u16,
    ) -> Result<(), Error> {
        let rx_ring_size: u16 = 4096;
        let tx_ring_size: u16 = 4096;

        let dev_info: dpdk_rs::rte_eth_dev_info = unsafe {
            let mut d: MaybeUninit<dpdk_rs::rte_eth_dev_info> = MaybeUninit::zeroed();
            rte_eth_dev_info_get(port_id, d.as_mut_ptr());
            d.assume_init()
        };

        // println!("dev_info: {:?}", dev_info);
        let mut port_conf: rte_eth_conf = unsafe { MaybeUninit::zeroed().assume_init() };

        port_conf.rxmode.mq_mode = if nr_queues > 1 {
            RTE_ETH_MQ_RX_RSS
        } else {
            RTE_ETH_MQ_RX_NONE
        };
        port_conf.txmode.mq_mode = RTE_ETH_MQ_TX_NONE;

        port_conf.rxmode.max_lro_pkt_size = if use_jumbo_frames {
            RTE_ETHER_MAX_JUMBO_FRAME_LEN
        } else {
            RTE_ETHER_MAX_LEN
        };
        port_conf.rx_adv_conf.rss_conf.rss_key = 0 as *mut _;
        port_conf.rx_adv_conf.rss_conf.rss_hf = unsafe { (rte_eth_rss_ip() | rte_eth_rss_udp() | rte_eth_rss_tcp()) as u64 } & dev_info.flow_type_rss_offloads;

        if tcp_checksum_offload {
            port_conf.rxmode.offloads |= unsafe { (rte_eth_rx_offload_ip_cksum() | rte_eth_rx_offload_tcp_cksum()) as u64 };
            port_conf.txmode.offloads |= unsafe { (rte_eth_tx_offload_ip_cksum() | rte_eth_tx_offload_tcp_cksum()) as u64 };
        }
        if udp_checksum_offload {
            port_conf.rxmode.offloads |= unsafe { (rte_eth_rx_offload_ip_cksum() | rte_eth_rx_offload_udp_cksum()) as u64 };
            port_conf.txmode.offloads |= unsafe { (rte_eth_tx_offload_ip_cksum() | rte_eth_tx_offload_udp_cksum()) as u64 };
        }

        unsafe {
            expect_zero!(rte_eth_dev_configure(
                port_id,
                nr_queues,
                nr_queues,
                &port_conf as *const _,
            ))?;
        }

        unsafe {
            expect_zero!(rte_eth_dev_set_mtu(port_id, mtu))?;
            let mut dpdk_mtu: u16 = 0u16;
            expect_zero!(rte_eth_dev_get_mtu(port_id, &mut dpdk_mtu as *mut _))?;
            if dpdk_mtu != mtu {
                bail!("Failed to set MTU to {}, got back {}", mtu, dpdk_mtu);
            }
        }

        let socket_id: u32 = unsafe { rte_socket_id() };

        let mut rx_conf: rte_eth_rxconf = dev_info.default_rxconf;
        rx_conf.offloads = port_conf.rxmode.offloads;
        rx_conf.rx_drop_en = 1;

        let mut tx_conf: rte_eth_txconf = dev_info.default_txconf;
        tx_conf.offloads = port_conf.txmode.offloads;

        unsafe {
            for i in 0..nr_queues {
                expect_zero!(rte_eth_rx_queue_setup(
                    port_id,
                    i,
                    rx_ring_size,
                    socket_id,
                    &rx_conf as *const _,
                    memory_manager.body_pool(),
                ))?;
            }
            for i in 0..nr_queues {
                expect_zero!(rte_eth_tx_queue_setup(
                    port_id,
                    i,
                    tx_ring_size,
                    socket_id,
                    &tx_conf as *const _,
                ))?;
            }
            expect_zero!(rte_eth_dev_start(port_id))?;
            rte_eth_promiscuous_enable(port_id);
        }

        Ok(())
    }

    pub fn new(
        ipv4_addr: Ipv4Addr,
        arp_table: HashMap<Ipv4Addr, MacAddress>,
        disable_arp: bool,
        mss: usize,
        tcp_checksum_offload: bool,
        udp_checksum_offload: bool,
        port_id: u16,
        queue_id: u16,
        mm: Arc<MemoryManager>,
    ) -> DPDKRuntime {
        let arp_options = ArpConfig::new(
            Some(Duration::from_secs(15)),
            Some(Duration::from_secs(20)),
            Some(5),
            Some(arp_table),
            Some(disable_arp),
        );

        let tcp_options = TcpConfig::new(
            Some(mss),
            None,
            None,
            Some(0xffff),
            Some(0),
            None,
            Some(tcp_checksum_offload),
            Some(tcp_checksum_offload),
        );

        let udp_options = UdpConfig::new(Some(udp_checksum_offload), Some(udp_checksum_offload));

        let link_addr: MacAddress = unsafe {
            let mut m: MaybeUninit<rte_ether_addr> = MaybeUninit::zeroed();
            // TODO: Why does bindgen say this function doesn't return an int?
            rte_eth_macaddr_get(port_id, m.as_mut_ptr());
            MacAddress::new(m.assume_init().addr_bytes)
        };

        Self {
            mm,
            port_id,
            queue_id,
            link_addr,
            ipv4_addr,
            arp_options,
            tcp_options,
            udp_options,
        }
    }
}

//==============================================================================
// Trait Implementations
//==============================================================================

impl Runtime for DPDKRuntime {}