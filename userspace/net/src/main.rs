//! NovaX virtio-net server (legacy virtio 0.9, transitional PCI device).
//!
//! The kernel hands us two capabilities:
//!   cap 1 = IoPort { BAR0 window }  (CAP_READ | CAP_WRITE)
//!   cap 2 = Dma { 64 KiB }          (CAP_READ | CAP_WRITE)
//!
//! Everything below runs in ring 3: virtqueue management, the Ethernet
//! framing, ARP and a minimal ICMP echo client used to prove that packets
//! really travel through QEMU's user-mode network.

#![no_std]
#![no_main]

use core::sync::atomic::{fence, Ordering};

#[path = "../../common/abi.rs"]
mod abi;

use abi::{write, yield_now};

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

const CAP_IO: u64 = 1;
const CAP_DMA: u64 = 2;

// --- Legacy virtio PCI register offsets (BAR0) ------------------------------

const REG_HOST_FEATURES: u64 = 0x00;
const REG_GUEST_FEATURES: u64 = 0x04;
const REG_QUEUE_PFN: u64 = 0x08;
const REG_QUEUE_SIZE: u64 = 0x0C;
const REG_QUEUE_SELECT: u64 = 0x0E;
const REG_QUEUE_NOTIFY: u64 = 0x10;
const REG_STATUS: u64 = 0x12;
const REG_DEVICE_CFG: u64 = 0x14;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// Device feature bit: the config space carries the MAC address.
const FEATURE_MAC: u32 = 1 << 5;

// --- Virtqueue --------------------------------------------------------------

const VRING_DESC_F_WRITE: u16 = 2;

const PAGE: u64 = 4096;
const DMA_SIZE: u32 = 64 * 1024;
const RX_BUFFERS: usize = 8;
const BUFFER_SIZE: usize = 2048;
const MAX_QSIZE: usize = 256;

// --- Network ----------------------------------------------------------------

const ETH_ALEN: usize = 6;
const ETH_HDR_LEN: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const IP_PROTO_ICMP: u8 = 1;
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const ICMP_ID: u16 = 0x4E58; // "NX"

const RX_WAIT_SPINS: u32 = 2_000_000;
const YIELD_INTERVAL: u32 = 100_000;

// --- Capability-checked port I/O -------------------------------------------

fn inb(offset: u64) -> u8 {
    abi::inb(CAP_IO, offset) as u8
}
fn outb(offset: u64, value: u8) {
    abi::outb(CAP_IO, offset, value as u64);
}
fn inw(offset: u64) -> u16 {
    abi::inw(CAP_IO, offset) as u16
}
fn outw(offset: u64, value: u16) {
    abi::outw(CAP_IO, offset, value as u64);
}
fn inl(offset: u64) -> u32 {
    abi::inl(CAP_IO, offset) as u32
}
fn outl(offset: u64, value: u32) {
    abi::outl(CAP_IO, offset, value as u64);
}

fn device_status() -> u8 {
    inb(REG_STATUS)
}
fn set_status(value: u8) {
    outb(REG_STATUS, value);
}

// --- Virtqueue memory layout ------------------------------------------------

#[derive(Clone, Copy)]
struct Layout {
    dma_virt: u64,
    dma_phys: u64,
    qsize: usize,
    rx_desc: u64,
    rx_avail: u64,
    rx_used: u64,
    tx_desc: u64,
    tx_avail: u64,
    tx_used: u64,
    rx_buf: u64,
    tx_buf: u64,
}

fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

impl Layout {
    fn new(dma_virt: u64, dma_phys: u64, qsize: usize) -> Option<Self> {
        if qsize < RX_BUFFERS || qsize > MAX_QSIZE {
            return None;
        }

        let desc_bytes = 16 * qsize as u64;
        let avail_bytes = 4 + 2 * qsize as u64;
        let used_bytes = 4 + 8 * qsize as u64;

        let rx_desc = dma_virt;
        let rx_avail = rx_desc + desc_bytes;
        let rx_used = rx_desc + align_up(desc_bytes + avail_bytes, PAGE);

        let tx_desc = rx_desc + align_up((rx_used + used_bytes) - dma_virt, PAGE);
        let tx_avail = tx_desc + desc_bytes;
        let tx_used = tx_desc + align_up(desc_bytes + avail_bytes, PAGE);

        let rx_buf = rx_desc + align_up((tx_used + used_bytes) - dma_virt, PAGE);
        let tx_buf = rx_buf + (RX_BUFFERS * BUFFER_SIZE) as u64;

        if tx_buf + BUFFER_SIZE as u64 > dma_virt + DMA_SIZE as u64 {
            return None;
        }

        Some(Self {
            dma_virt,
            dma_phys,
            qsize,
            rx_desc,
            rx_avail,
            rx_used,
            tx_desc,
            tx_avail,
            tx_used,
            rx_buf,
            tx_buf,
        })
    }

    fn phys(&self, virt: u64) -> u64 {
        self.dma_phys + (virt - self.dma_virt)
    }
}

// WHY unsafe (all ring helpers): the device writes the used ring while we
// read it. Volatile accesses plus the fences at publish/observe points are
// the contract; addresses come from our own DMA window.

fn desc_set(base: u64, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
    let p = base + (index * 16) as u64;
    // WHY unsafe: `p..p+16` is one descriptor inside the DMA window.
    unsafe {
        core::ptr::write_volatile(p as *mut u64, addr);
        core::ptr::write_volatile((p + 8) as *mut u32, len);
        core::ptr::write_volatile((p + 12) as *mut u16, flags);
        core::ptr::write_volatile((p + 14) as *mut u16, next);
    }
}

fn avail_idx(base: u64) -> u16 {
    // WHY unsafe: ring header inside our DMA window.
    unsafe { core::ptr::read_volatile((base + 2) as *const u16) }
}

fn avail_push(base: u64, qsize: usize, desc: u16) {
    let idx = avail_idx(base);
    let slot = (idx as usize) % qsize;
    // WHY unsafe: ring slot inside our DMA window.
    unsafe {
        core::ptr::write_volatile((base + 4 + (slot * 2) as u64) as *mut u16, desc);
    }
    fence(Ordering::SeqCst);
    // WHY unsafe: same as above, publishing the new index.
    unsafe {
        core::ptr::write_volatile((base + 2) as *mut u16, idx.wrapping_add(1));
    }
    fence(Ordering::SeqCst);
}

fn used_idx(base: u64) -> u16 {
    // WHY unsafe: ring header inside our DMA window.
    unsafe { core::ptr::read_volatile((base + 2) as *const u16) }
}

fn used_elem(base: u64, qsize: usize, idx: u16) -> (u16, u32) {
    let slot = (idx as usize) % qsize;
    let p = base + 4 + (slot * 8) as u64;
    // WHY unsafe: used-ring element written by the device.
    unsafe {
        let id = core::ptr::read_volatile(p as *const u32) as u16;
        let len = core::ptr::read_volatile((p + 4) as *const u32);
        (id, len)
    }
}

// --- Driver state -----------------------------------------------------------

struct Net {
    layout: Layout,
    mac: [u8; ETH_ALEN],
    last_rx: u16,
    last_tx: u16,
}

impl Net {
    /// Initialize the device and publish the RX ring. Returns `None` on
    /// any protocol violation (the caller reports it and exits).
    fn init(dma_virt: u64, dma_phys: u64) -> Option<Self> {
        set_status(0);
        let mut spins = 0u32;
        while device_status() != 0 && spins < 10_000 {
            spins += 1;
        }
        if device_status() != 0 {
            return None;
        }

        set_status(STATUS_ACK);
        set_status(STATUS_ACK | STATUS_DRIVER);

        let host = inl(REG_HOST_FEATURES);
        let guest = if host & FEATURE_MAC != 0 {
            FEATURE_MAC
        } else {
            0
        };
        outl(REG_GUEST_FEATURES, guest);

        set_status(STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
        if device_status() & STATUS_FEATURES_OK == 0 {
            return None;
        }

        outw(REG_QUEUE_SELECT, 0);
        let rx_qsize = inw(REG_QUEUE_SIZE) as usize;
        outw(REG_QUEUE_SELECT, 1);
        let tx_qsize = inw(REG_QUEUE_SIZE) as usize;
        if rx_qsize != tx_qsize {
            return None;
        }

        let layout = Layout::new(dma_virt, dma_phys, rx_qsize)?;

        outw(REG_QUEUE_SELECT, 0);
        outl(REG_QUEUE_PFN, (layout.phys(layout.rx_desc) >> 12) as u32);
        outw(REG_QUEUE_SELECT, 1);
        outl(REG_QUEUE_PFN, (layout.phys(layout.tx_desc) >> 12) as u32);

        let mut mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        if guest & FEATURE_MAC != 0 {
            for (i, byte) in mac.iter_mut().enumerate() {
                *byte = inb(REG_DEVICE_CFG + i as u64);
            }
        }

        set_status(STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

        let net = Self {
            layout,
            mac,
            last_rx: 0,
            last_tx: 0,
        };

        // Publish every RX buffer before telling the device we are ready.
        for i in 0..RX_BUFFERS {
            desc_set(
                net.layout.rx_desc,
                i,
                net.layout.phys(net.layout.rx_buf) + (i * BUFFER_SIZE) as u64,
                BUFFER_SIZE as u32,
                VRING_DESC_F_WRITE,
                0,
            );
            avail_push(net.layout.rx_avail, net.layout.qsize, i as u16);
        }
        outw(REG_QUEUE_NOTIFY, 0);
        Some(net)
    }

    /// Transmit one Ethernet frame (without FCS). Blocks until the device
    /// returns the descriptor to the used ring.
    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() + 10 > BUFFER_SIZE {
            return false;
        }

        let buf = self.layout.tx_buf as *mut u8;
        // WHY unsafe: TX buffer lives inside our DMA window and is sized
        // for a 10-byte virtio header plus one maximum-size frame.
        unsafe {
            core::ptr::write_bytes(buf, 0, 10);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf.add(10), frame.len());
        }
        fence(Ordering::SeqCst);

        desc_set(
            self.layout.tx_desc,
            0,
            self.layout.phys(self.layout.tx_buf),
            (10 + frame.len()) as u32,
            0,
            0,
        );
        avail_push(self.layout.tx_avail, self.layout.qsize, 0);
        outw(REG_QUEUE_NOTIFY, 1);

        let start = self.last_tx;
        for spin in 0..RX_WAIT_SPINS {
            if used_idx(self.layout.tx_used) != start {
                fence(Ordering::SeqCst);
                self.last_tx = self.last_tx.wrapping_add(1);
                return true;
            }
            if spin % YIELD_INTERVAL == YIELD_INTERVAL - 1 {
                yield_now();
            }
            core::hint::spin_loop();
        }
        write(b"net: tx timeout\n");
        false
    }

    /// Consume one completed RX buffer, if any, into `out`.
    fn poll_rx(&mut self, out: &mut [u8]) -> Option<usize> {
        let head = used_idx(self.layout.rx_used);
        if head == self.last_rx {
            return None;
        }
        fence(Ordering::SeqCst);

        let (desc_id, len) = used_elem(self.layout.rx_used, self.layout.qsize, self.last_rx);
        self.last_rx = self.last_rx.wrapping_add(1);

        let id = desc_id as usize;
        if id >= RX_BUFFERS {
            return None;
        }

        // The device prefixes every frame with `struct virtio_net_hdr`
        // (10 bytes without MRG_RXBUF); Ethernet starts right after it.
        const VIRTIO_NET_HDR_LEN: usize = 10;
        if (len as usize) <= VIRTIO_NET_HDR_LEN {
            return None;
        }

        let payload_len = len as usize - VIRTIO_NET_HDR_LEN;
        let n = core::cmp::min(payload_len, out.len());
        let src = (self.layout.rx_buf + (id * BUFFER_SIZE) as u64 + VIRTIO_NET_HDR_LEN as u64)
            as *const u8;
        // WHY unsafe: `src` is a device-filled RX buffer inside our DMA
        // window; `n` is bounded by its size.
        unsafe {
            core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), n);
        }

        // Give the buffer back to the device.
        avail_push(self.layout.rx_avail, self.layout.qsize, desc_id);
        outw(REG_QUEUE_NOTIFY, 0);
        Some(n)
    }

    fn wait_frame(&mut self, out: &mut [u8]) -> Option<usize> {
        for spin in 0..RX_WAIT_SPINS {
            if let Some(n) = self.poll_rx(out) {
                return Some(n);
            }
            if spin % YIELD_INTERVAL == YIELD_INTERVAL - 1 {
                yield_now();
            }
            core::hint::spin_loop();
        }
        None
    }

    fn print_mac(&self) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        write(b"net: mac ");
        for (i, byte) in self.mac.iter().enumerate() {
            let pair = [HEX[(byte >> 4) as usize], HEX[(byte & 0xF) as usize]];
            write(&pair);
            write(if i + 1 == ETH_ALEN { b"\n" } else { b":" });
        }
    }
}

// --- Frame builders and parsers --------------------------------------------

fn put_u16_be(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset] = (value >> 8) as u8;
    buf[offset + 1] = value as u8;
}

/// Internet checksum (RFC 1071) over an even/odd byte slice.
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn write_eth_header(buf: &mut [u8], dst: &[u8; ETH_ALEN], src: &[u8; ETH_ALEN], ethertype: u16) {
    buf[0..6].copy_from_slice(dst);
    buf[6..12].copy_from_slice(src);
    put_u16_be(buf, 12, ethertype);
}

fn build_arp_request(buf: &mut [u8], mac: &[u8; ETH_ALEN], spa: [u8; 4], tpa: [u8; 4]) -> usize {
    let broadcast = [0xFFu8; ETH_ALEN];
    write_eth_header(buf, &broadcast, mac, ETHERTYPE_ARP);
    let a = 14;
    put_u16_be(buf, a, 1); // htype: Ethernet
    put_u16_be(buf, a + 2, 0x0800); // ptype: IPv4
    buf[a + 4] = 6; // hlen
    buf[a + 5] = 4; // plen
    put_u16_be(buf, a + 6, 1); // request
    buf[a + 8..a + 14].copy_from_slice(mac);
    buf[a + 14..a + 18].copy_from_slice(&spa);
    buf[a + 18..a + 24].fill(0);
    buf[a + 24..a + 28].copy_from_slice(&tpa);

    // Pad to the Ethernet minimum payload size.
    buf[42..60].fill(0);
    60
}

fn parse_arp_reply(frame: &[u8], our_ip: [u8; 4], peer_ip: [u8; 4]) -> Option<[u8; ETH_ALEN]> {
    if frame.len() < 42 {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_ARP {
        return None;
    }
    let a = 14;
    if u16::from_be_bytes([frame[a + 6], frame[a + 7]]) != 2 {
        return None;
    } // reply
    if frame[a + 14..a + 18] != peer_ip {
        return None;
    }
    if frame[a + 24..a + 28] != our_ip {
        return None;
    }

    let mut mac = [0u8; ETH_ALEN];
    mac.copy_from_slice(&frame[a + 8..a + 14]);
    Some(mac)
}

fn build_icmp_echo(
    buf: &mut [u8],
    src_mac: &[u8; ETH_ALEN],
    dst_mac: &[u8; ETH_ALEN],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    seq: u16,
) -> usize {
    const PAYLOAD: &[u8] = b"NovaX virtio-net ping";

    write_eth_header(buf, dst_mac, src_mac, ETHERTYPE_IPV4);

    let ip = ETH_HDR_LEN;
    let total_len = 20 + 8 + PAYLOAD.len();
    buf[ip] = 0x45;
    buf[ip + 1] = 0;
    put_u16_be(buf, ip + 2, total_len as u16);
    put_u16_be(buf, ip + 4, 0x1234); // identification
    put_u16_be(buf, ip + 6, 0x4000); // don't fragment
    buf[ip + 8] = 64; // TTL
    buf[ip + 9] = IP_PROTO_ICMP;
    buf[ip + 10] = 0;
    buf[ip + 11] = 0;
    buf[ip + 12..ip + 16].copy_from_slice(&src_ip);
    buf[ip + 16..ip + 20].copy_from_slice(&dst_ip);
    let ip_sum = checksum(&buf[ip..ip + 20]);
    put_u16_be(buf, ip + 10, ip_sum);

    let icmp = ip + 20;
    buf[icmp] = ICMP_ECHO_REQUEST;
    buf[icmp + 1] = 0;
    buf[icmp + 2] = 0;
    buf[icmp + 3] = 0;
    put_u16_be(buf, icmp + 4, ICMP_ID);
    put_u16_be(buf, icmp + 6, seq);
    buf[icmp + 8..icmp + 8 + PAYLOAD.len()].copy_from_slice(PAYLOAD);
    let icmp_sum = checksum(&buf[icmp..icmp + 8 + PAYLOAD.len()]);
    put_u16_be(buf, icmp + 2, icmp_sum);

    ETH_HDR_LEN + total_len
}

fn parse_icmp_reply(frame: &[u8], our_ip: [u8; 4], peer_ip: [u8; 4], seq: u16) -> bool {
    if frame.len() < ETH_HDR_LEN + 20 + 8 {
        return false;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_IPV4 {
        return false;
    }

    let ip = ETH_HDR_LEN;
    if frame[ip] >> 4 != 4 {
        return false;
    }
    let ihl = ((frame[ip] & 0x0F) as usize) * 4;
    if ihl < 20 || frame.len() < ip + ihl + 8 {
        return false;
    }
    if frame[ip + 9] != IP_PROTO_ICMP {
        return false;
    }
    if frame[ip + 12..ip + 16] != peer_ip {
        return false;
    }
    if frame[ip + 16..ip + 20] != our_ip {
        return false;
    }

    let icmp = ip + ihl;
    if frame[icmp] != ICMP_ECHO_REPLY || frame[icmp + 1] != 0 {
        return false;
    }
    if u16::from_be_bytes([frame[icmp + 4], frame[icmp + 5]]) != ICMP_ID {
        return false;
    }
    u16::from_be_bytes([frame[icmp + 6], frame[icmp + 7]]) == seq
}

// --- Console helpers --------------------------------------------------------

fn write_num(prefix: &[u8], value: u16, suffix: &[u8]) {
    write(prefix);
    let mut digits = [0u8; 5];
    let mut n = value;
    let mut i = digits.len();
    if n == 0 {
        i -= 1;
        digits[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(&digits[i..]);
    write(suffix);
}

fn fail(message: &[u8]) -> ! {
    write(message);
    abi::exit(1)
}

// --- Entry point ------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write(b"net: virtio-net driver starting\n");

    let mut result = [0u8; 16];
    let rc = abi::dma_alloc(CAP_DMA, &mut result);
    if rc != 0 {
        fail(b"net: dma alloc failed\n");
    }

    let mut virt_bytes = [0u8; 8];
    let mut phys_bytes = [0u8; 8];
    virt_bytes.copy_from_slice(&result[0..8]);
    phys_bytes.copy_from_slice(&result[8..16]);
    let dma_virt = u64::from_le_bytes(virt_bytes);
    let dma_phys = u64::from_le_bytes(phys_bytes);

    let mut net = match Net::init(dma_virt, dma_phys) {
        Some(net) => net,
        None => fail(b"net: virtio init failed\n"),
    };
    net.print_mac();
    write(b"net: link up\n");

    // Resolve the gateway MAC; slirp answers ARP for 10.0.2.2.
    let mut gateway_mac = [0u8; ETH_ALEN];
    let mut resolved = false;
    let mut frame = [0u8; 1600];
    for _ in 0..8 {
        let len = build_arp_request(&mut frame, &net.mac, OUR_IP, GATEWAY_IP);
        if !net.transmit(&frame[..len]) {
            continue;
        }

        for _ in 0..4 {
            let mut rx = [0u8; 1600];
            let Some(n) = net.wait_frame(&mut rx) else {
                break;
            };
            if let Some(mac) = parse_arp_reply(&rx[..n], OUR_IP, GATEWAY_IP) {
                gateway_mac = mac;
                resolved = true;
                break;
            }
        }
        if resolved {
            break;
        }
    }
    if !resolved {
        fail(b"net: arp timeout\n");
    }
    write(b"net: arp reply from 10.0.2.2\n");

    let mut replies = 0u32;
    for seq in 1..=3u16 {
        let len = build_icmp_echo(&mut frame, &net.mac, &gateway_mac, OUR_IP, GATEWAY_IP, seq);
        if !net.transmit(&frame[..len]) {
            write_num(b"net: ping seq=", seq, b" tx timeout\n");
            continue;
        }

        let mut got_reply = false;
        for _ in 0..4 {
            let mut rx = [0u8; 1600];
            let Some(n) = net.wait_frame(&mut rx) else {
                break;
            };
            if parse_icmp_reply(&rx[..n], OUR_IP, GATEWAY_IP, seq) {
                got_reply = true;
                break;
            }
        }

        if got_reply {
            replies += 1;
            write_num(b"net: ping 10.0.2.2 seq=", seq, b" ok\n");
        } else {
            write_num(b"net: ping 10.0.2.2 seq=", seq, b" timeout\n");
        }
    }

    if replies == 0 {
        fail(b"net: ping failed\n");
    }
    write(b"net: ping 10.0.2.2 ok\n");
    abi::exit(0);
}
