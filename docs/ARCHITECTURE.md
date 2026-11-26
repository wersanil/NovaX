# NovaX — Architecture & Project Summary (v0.2.0, Milestones 0–11)

Microkernel OS for x86_64. `Heart` keeps only what cannot live in userspace:
memory, scheduling, IPC, syscalls, IRQ routing. Drivers and services are
ring-3 servers using capability-checked IPC. This describes the real tree.

## 1. Repository layout

```
Cargo.toml                     workspace: kernel/heart (kernel/memory is a
                               standalone no_std crate, path dependency)
kernel/heart/                  microkernel binary (no_std)
  Cargo.toml                   limine, spin, linked_list_allocator, x86_64
  build.rs                     compiles every userspace program with rustc
  src/main.rs                  boot flow, device wiring, SMP demo
  src/arch/x86_64.rs           GDT/TSS/IDT, IOAPIC, LAPIC, syscall, IRQ, switch
  src/arch/smp.rs              PerCpu (GS-based), MP discovery, AP boot
  src/bootinfo.rs              Limine: revision, HHDM, memmap, MP, framebuffer
  src/console.rs               boot-log ring + best-effort fb mirror
  src/mmio.rs                  capability-checked physical MMIO mapping
  src/heap.rs                  1 MiB heap @ 0xffff_ffff_9000_0000
  src/sched.rs                 TCB, per-CPU queues, affinity, switching
  src/ipc.rs                   endpoints, messages, caps, IRQ routes
  src/cdt.rs                   capability derivation tree (mint/revoke)
  src/shm.rs                   read-only shared-buffer mappings
  src/syscall.rs               dispatch + user-memory copy helpers
  src/user.rs                  ELF64 loader, address spaces, ring-3 entry
  src/dma.rs                   contiguous DMA window for drivers
  src/pci.rs                   legacy 0xCF8/0xCFC scan (virtio-net)
  src/serial.rs panic.rs       early console, panic/double-fault reporting
  src/tests.rs                 kernel self-tests (`--features test`)
kernel/memory/                 novax_memory (own workspace)
  src/frame.rs                 bitmap frame allocator, HHDM access
  src/paging.rs                4-level paging on a given PML4
userspace/                     single-file programs, one linker.ld each
  common/abi.rs                shared syscall numbers + typed safe wrappers
  hello/                       M4 smoke test (not spawned by default)
  server/ client/              M5 IPC ping over one endpoint
  serial/                      COM1 server, IRQ4, feeds shell input
  kbd/                         PS/2 server, IRQ1
  init/                        command server ("echo ..." -> reply)
  shell/                       line editor, forwards commands to init
  fb/                          ring-3 framebuffer text console
  net/                         virtio-net server (ARP + ICMP)
  supervisor/                  v0.2 capability-lifecycle supervisor
  v02_server/ v02_client/      v0.2 crash/revoke/respawn stand
tools/                         fetch_limine, mkiso, qemu_run, qemu_gui,
                               test, m6/m7/m8/m9 test + check, git_commit
```

Every userspace program pulls in `userspace/common/abi.rs` with
`#[path = "../../common/abi.rs"] mod abi;`. No separate crate or cargo
project is needed, so `build.rs` still invokes plain `rustc` per program.
`boot/limine/`, `target/`, `build/` are excluded from this document.

## 2. Kernel architecture
### Boot and memory

Limine (base revision 3) provides HHDM, memory map, CPU list and, when a
display is present, a linear framebuffer. The frame allocator keeps a
bitmap in the first large enough USABLE region. Init order: per-CPU
GDT/TSS → IDT → mask PIC → LAPIC (map + id) → IOAPIC → syscalls → MP
discovery; APs start after all userspace threads are spawned.
- 4 KiB frames; all physical access via the HHDM. 4-level paging with
  `PRESENT`/`WRITABLE`/`USER`/`NO_EXEC`/`PWT`/`PCD`; intermediate tables
  are present+writable+user.
- Kernel heap: 1 MiB of mapped frames via `linked_list_allocator`.
- Each process gets a private PML4 (user half cleared, kernel half cloned);
  ELF `PT_LOAD` segments and a 2-page stack at `0x7fff_ffff_0000`.
  `load_elf` validates the header table, every `p_offset/p_filesz` and
  `p_vaddr/p_memsz` pair, requires segments to stay in the user half and
  outside the stack/DMA/MMIO windows, and rejects the image before any
  frame is allocated if validation fails. A failed load frees every frame
  and page table it created.
- `dma.rs` maps contiguous zeroed frames at user VA `0x3000_0000` and
  returns `{virt, phys}` to the driver.
- `bootinfo.rs` requests a framebuffer and stores `{paddr, len, width,
  height, pitch, bpp, RGB masks}`. Limine reports a virtual HHDM address;
  the kernel subtracts the HHDM offset. If no framebuffer exists (for
  example `-nographic -vga none`), the field is `None` and boot continues
  without a graphical console.
- `mmio.rs` maps a capability-granted physical window into the calling
  process's PML4 at user VA `0x4000_0000`, using `PRESENT | USER |
  WRITABLE | NO_EXEC | PCD | PWT` (uncached). The kernel never reads or
  writes the window itself.
### Scheduler and SMP

- `Tcb`: id, state (Ready/Running/Blocked/Dead), saved `rsp`, 16 KiB kernel
  stack, `cpu` affinity, `is_idle`, `wake_pending`, userspace fields
  (`pml4_phys`, entry, stack, cspace).
- One `Mutex<Scheduler>` holds TCBs; each CPU has its own `current` and
  `idle_slot`. `pick_next` considers only threads pinned to that CPU and
  skips idle threads (idle is the fallback).
- `switch_context` (naked asm) saves rbp/rbx/r12–r15/rflags, swaps `rsp`.
  Address space and `TSS.rsp0` are updated before the switch. New threads
  start with `IF=0`; a thread that needs timer preemption enables
  interrupts itself. AP idle runs `cli; schedule; sti; hlt`, so the
  scheduler lock is never taken with interrupts enabled.
- `wake_pending` closes the lost-wakeup race between `ipc::recv` and
  `block_current`; cross-CPU wakeups send a reschedule IPI.
- `PerCpu` (GS-based) holds kernel/user rsp, current index, LAPIC id, idle
  slot, TSS, GDT, boot/DF stacks. `CR4.FSGSBASE` stays clear: ring 3 cannot
  read it. AP boot: Limine `MpRequest` → `bootstrap(ap_entry, slot)`; the
  naked entry loads its stack, installs per-CPU GDT/TSS/IDT, enables the
  LAPIC, creates an idle thread and joins the scheduler. Max 8 CPUs.
- Vectors: timer `0x20`, reschedule IPI `0x30`, TLB shootdown IPI `0x31`,
  IRQ1 `0x21`, IRQ4 `0x24`.
  IOAPIC ISA lines are edge/active-high, destination = BSP LAPIC id.
### Syscall ABI

`syscall`/`sysretq`; `EFER.SCE=1`, `STAR` = kernel CS 0x08 / user CS 0x23,
`LSTAR=syscall_entry`, `FMASK=0x700` (clears IF, TF, DF).
- Input: `rax` = number, `rdi`, `rsi`, `rdx`, `r10`, `r8` = arguments.
- Output: `rax`; `rcx`/`r11` are clobbered by the instruction itself.
- Other GPRs preserved; per-CPU `kernel_rsp`/`user_rsp` via `gs:`.
- Every user pointer is walked page by page against the current PML4
  before it is dereferenced: the pages must be `USER` and, for output,
  `WRITABLE`. Invalid pointers return `u64::MAX` instead of faulting in
  ring 0. A ring-3 `#PF/#GP/#UD/#DE/#SS/#AC` terminates the faulting
  thread; ring-0 faults still panic.
| # | Syscall | Arguments | Result |
|---|---------|-----------|--------|
| 0 | EXIT | code | — |
| 1 | WRITE | fd(1), ptr, len | bytes |
| 2 | SEND | cap, ptr, len | len (≤64) |
| 3 | RECV | cap, ptr, max | len / `u64::MAX` |
| 4 | YIELD | — | 0 |
| 5 | IRQ_REGISTER | irq_cap, ep_cap | 0 |
| 6 | IRQ_ACK | irq_cap | 0 |
| 7/8 | INB/OUTB | io_cap, offset[, value] | byte / 0 |
| 9 | DMA_ALLOC | dma_cap, out_ptr | 0 |
| 10–13 | INW/OUTW/INL/OUTL | io_cap, offset[, value] | word/dword / 0 |
| 14 | MAP_MMIO | mmio_cap, out_ptr | 0; writes `{virt, paddr, len}` |
| 15 | CAP_MINT | cap, new_rights, out_slot | 0; writes `u64` slot |
| 16 | CAP_REVOKE | cap | 0; root stays, descendants die |
| 17 | SEND_CAP | ep_cap, ptr, len | payload len; header carries one cap |
| 18 | RECV_CAP | ep_cap, ptr, max, out_meta | len; `{cap_slot, reply_slot}` |
| 19 | CALL | ep, req, req_len, reply, reply_max | reply len |
| 20 | REPLY | reply_cap, ptr, len | 0; one-shot, slot burns |
| 21 | NOTIFY_SIGNAL | notify_cap, badge | 0 |
| 22 | NOTIFY_WAIT | notify_cap | consumed mask |
| 23 | NOTIFY_POLL | notify_cap | mask (`swap(0)`) |
| 24 | NOTIFY_BIND | notify, source, src_cap, badge | 0 |
| 25 | SPAWN | prog_id, grants, grants_len, out_meta | 0; `{proc_slot}` |
| 26 | MAP_SHARED | shm_cap, out_info | 0; writes `{virt, phys, len}` |
Errors return `u64::MAX`; kernel internals use `Result<T, E>`.
### IPC and capabilities

- Endpoints have a FIFO (messages ≤64 bytes) and at most one blocked
  receiver. `send` wakes it (or sets `wake_pending` if it is running);
  `recv` returns a queued message, blocks, or reports that another
  receiver is already blocked. The FIFO is capped at 1024 messages for
  both senders; a full queue makes `SYS_SEND` return `u64::MAX`.
- cspace = `[Option<Capability>; 16]`; `Capability = { kind, rights }`,
  rights = `CAP_READ`, `CAP_WRITE`, `CAP_NOTIFY`.
- Kinds: `Endpoint(id)`, `Irq(n)`, `IoPort { base, len }`, `Dma { len }`,
  `Mmio { paddr, len }`. IRQ model: the handler masks the line, enqueues
  one byte "IRQ n" on the registered endpoint and EOIs the LAPIC; the
  driver drains the device and calls `SYS_IRQ_ACK`, which unmasks the
  IOAPIC entry.
- `ipc::try_send` is the non-blocking console path: it uses `try_lock`,
  caps an endpoint queue at 1024 messages and drops on overflow. The
  serial console therefore never waits for a stalled fb server.

### Capability lifecycle and notifications (v0.2)

- `Capability = { kind, rights, node }`; `node` indexes the static CDT
  (`cdt.rs`, 256 nodes, no heap). Nodes form a parent + doubly-linked
  child list. Kinds add `Notification(id)`, `Reply(id)`, `Process(tid)`,
  `DmaMapped { phys, len }` and `Shm { phys, len }`.
- `SYS_CAP_MINT` derives a child in the first free cspace slot with
  `new_rights ⊆ old_rights`; widening is rejected. Minting an allocated
  `DmaMapped` yields a read-only `Shm`.
- `SYS_CAP_REVOKE` destroys every descendant of the root node, clears the
  owners' cspace slots and unmaps MMIO (`0x4000_0000`), DMA
  (`0x3000_0000`, frames freed) and shared (`0x5000_0000`) windows. A
  targeted TLB shootdown IPI (vector `0x31`) reloads CR3 on the owner CPU.
- Process death (`SYS_EXIT` or a ring-3 fault) runs `ipc::process_died`:
  signal bound notifications, fail pending calls, revoke every capability
  owned by the thread and release its DMA window. `SYS_CAP_REVOKE` is
  idempotent and still removes descendants held by live processes.
- Capability transfer uses a 12-byte `IpcHeader` (`NVCH`, `cap_slot`,
  `cap_rights`) at the start of a `SEND_CAP`/`CALL` payload; the kernel
  strips it and creates the CDT child only when `RECV_CAP` installs it.
- `SYS_CALL` allocates a one-shot `Reply` capability and installs it in
  the server's cspace when the message is received; `SYS_REPLY` copies
  the answer into the blocked client's address space (translated through
  its PML4), burns the slot and wakes the client.
- `Notification` is an `AtomicU64` mask plus an 8-entry wait queue.
  `SIGNAL` does `fetch_or(badge)` and wakes waiters; `WAIT` consumes with
  `swap(0)` and blocks; `POLL` is the non-blocking `swap(0)` for ring-3
  event loops. `NOTIFY_BIND` attaches an IRQ line or a child process
  exit/fault to a notification with a badge.
- `SYS_SPAWN` loads an embedded program by id (`1 = v02_server`,
  `2 = v02_client`) into a fresh PML4, derives its cspace from
  `CapGrant[]`, returns a `Process` capability and only then makes the
  thread runnable.

## 3. Userspace servers

| Program | Caps | Role |
|---------|------|------|
| `server` | ep(0) R/W | M5 demo receiver |
| `client` | ep(0) R/W | M5 demo sender (`ping`) |
| `serial` | Irq(4), irq_ep, IoPort 0x3F8+8, shell_in W | COM1 echo + shell input |
| `kbd` | Irq(1), irq_ep, IoPort 0x60+5 | PS/2 scancodes |
| `init` | init_ep R, reply_ep W | `echo <text>` / `unknown command` |
| `shell` | init_ep W, shell_in R, reply_ep R | line buffer + printing |
| `fb` | Mmio, fb_info R, fb_console R | 8x16 text console on the framebuffer |
| `net` | IoPort(BAR0), Dma(64 KiB) | virtio-net, ARP, ICMP |
| `supervisor` | ep R/W, notification, Dma, go/done ep | v0.2 mint/revoke/notify stand |
| `v02_server` | ep READ | call + shared map + deliberate fault |
| `v02_client` | ep WRITE, Dma, Shm, go READ, done WRITE | two calls around respawn |
Path: keyboard/COM1 → `serial` → `shell_in_ep` → `shell` → `init_ep` →
`init` → `reply_ep` → `shell` → console via `SYS_WRITE` (COM1 and,
when present, the fb console).
### Framebuffer console (since v0.1.1)

The kernel requests a Limine framebuffer and grants the `fb` server:

1. `CapKind::Mmio { paddr, len }` for the linear framebuffer;
2. `Endpoint(fb_info)` carrying one 48-byte `FbInfo` message
   (`magic`, version, width, height, pitch, bpp, RGB masks, paddr, len);
3. `Endpoint(fb_console)` carrying mirrored console bytes.

`fb` calls `SYS_MAP_MMIO`, receives `{virt, paddr, len}`, then renders a
text console with an embedded 8x16 ASCII font. The style is NovaX
cybercore: `#0A0A0C` background, a red `#E60000` banner
`NOVAX v0.2.0 // HEART MICROKERNEL`, and `#E5E5E5` text. The banner and a
separator stay pinned to the top; the log area below scrolls when full.
Supported formats are 32/24/16 bpp with RGB masks from `FbInfo`; any
other format makes `fb` log an error and exit instead of panicking.

`console.rs` mirrors every `serial_print*!` byte. Until `fb` attaches it
keeps a 32 KiB boot-log ring; `attach` replays it so the screen shows the
boot from `NovaX booting...`. After that, bytes are offered with
`ipc::try_send` and dropped if the fb queue is full. The serial path
remains the primary console and is never blocked by the graphical one.
If no framebuffer exists, `fb` is not spawned and the kernel logs
`fb: no framebuffer (headless mode)`.
### virtio-net

Kernel finds PCI `1af4:1000` on the root bus (legacy device; QEMU uses
`disable-modern=on`), sizes BAR0, enables I/O + bus mastering, and grants
`IoPort` + `Dma`. The driver: resets the device, negotiates
`VIRTIO_NET_F_MAC`, reads the MAC, sets up RX/TX legacy split virtqueues
(256 entries, 8 RX buffers of 2 KiB), publishes descriptor PFNs, transmits
with the 10-byte `virtio_net_hdr`, strips that header on RX, polls the used
ring (`yield` between batches), resolves `10.0.2.2` by ARP and sends three
ICMP echoes from `10.0.2.15`. Success line: `net: ping 10.0.2.2 ok`.

## 4. Build, run and test

Dependencies: nightly Rust (`abi_x86_interrupt`, `naked_asm!`),
`x86_64-unknown-none`, `xorriso`, `qemu-system-x86_64`, Limine binaries
from `./tools/fetch_limine.sh`.

```bash
cd ~/novax/novax
cargo build --release -p novax_heart     # kernel + userspace ELFs
./tools/mkiso.sh                         # build/novax.iso
./tools/qemu_run.sh                      # q35, -smp 2, netdev user, serial
./tools/qemu_gui.sh                      # graphical window, -vga std
# direct flags: -M q35 -smp 2 -m 512M -nographic -no-reboot \
#   -boot d -cdrom build/novax.iso -netdev user,id=n0 \
#   -device virtio-net-pci,netdev=n0,disable-modern=on
```

`qemu_gui.sh` keeps the serial log in the terminal (`-serial mon:stdio`)
and opens QEMU's default display with `-vga std`. Set `QEMU_DISPLAY=sdl`
or `QEMU_DISPLAY=none` to override the backend. The ISO also boots
headless: without a framebuffer the kernel skips `fb` and continues on
COM1 exactly as before.

Lint and format gates (release tree is clean):

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p novax_heart --features test -- -D warnings
cargo clippy --manifest-path kernel/memory/Cargo.toml --lib -- -D warnings
```

Checks (each boots QEMU and inspects the serial stream):

```bash
./tools/m6_test.sh   # IRQ4 serial + IRQ1 keyboard via IPC
./tools/m7_test.sh   # shell -> init -> shell, echo hello
./tools/m8_test.sh   # 2 CPUs, workers on cpu0/cpu1 + M5–M7
./tools/m9_test.sh   # virtio-net ARP + ICMP + M5–M8
./tools/v02_test.sh  # v0.2 mint/revoke, reply-cap, notification, respawn
./tools/test.sh      # kernel self-tests, success = QEMU exit 33
```

Reference M9 log (trimmed):

```
NovaX booting...
hhdm offset: 0xffff800000000000
fb: 1280x800 pitch=5120 bpp=32 paddr=0xfd000000 len=0x3e8000
heap:  1024 KiB @ 0xffffffff90000000
ioapic: id=0, version=0x20, entries=24
lapic: bsp id=0, virt=0xffffffffb0000000
smp: 2 cpu(s) reported by limine
Milestone 5: OK
Milestone 6: userspace drivers spawned
fb thread id = 11
fb: graphical console attached
fb: ready
Milestone 7: init + shell spawned
pci: virtio-net 1af4:1000 io=0xc040+0x20 irq=11
Milestone 9: virtio-net driver spawned
smp: cpu1 online
Milestone 8: smp online (2 of 2 cpu(s))
net: mac 52:54:00:12:34:56
net: link up
net: arp reply from 10.0.2.2
net: ping 10.0.2.2 seq=1 ok
net: ping 10.0.2.2 ok
```

### Release polish (v0.1.2)

- Hardened ELF loader: all program headers and ranges are validated before
  allocation; segments are restricted to the user half and kept out of the
  stack/DMA/MMIO windows; failed loads free their frames and tables.
- Checked user pointers at every syscall boundary (`USER`/`WRITABLE` page
  walk); invalid pointers return `u64::MAX` instead of faulting in ring 0.
- Ring-3 `#PF/#GP/#UD/#DE/#SS/#AC` terminate the faulting thread; ring-0
  faults still panic.
- Endpoint FIFOs are capped at 1024 messages; a second blocked receiver is
  reported as an error instead of overwriting the first.
- New threads start with `IF=0`, AP idle runs `cli; schedule; sti; hlt`,
  and the IOAPIC uses the real BSP LAPIC id.
- Huge-page entries are rejected during map/unmap; port-I/O width and
  DMA/MMIO result pointers are validated.

### Capability lifecycle (v0.2.0)

- Static CDT with transitive `SYS_CAP_REVOKE`; revoked MMIO, DMA and
  shared windows are unmapped, DMA frames freed, and the owner CPU gets a
  targeted TLB shootdown IPI.
- IPC messages can carry exactly one capability (`SEND_CAP`/`RECV_CAP`);
  `SYS_CALL`/`SYS_REPLY` provide one-shot reply capabilities without
  exposing thread ids.
- `Notification` objects with `AtomicU64` badges, blocking wait,
  non-blocking poll, and IRQ/process-exit/fault bindings.
- `SYS_SPAWN` with an embedded-program registry and `CapGrant[]`; the
  `supervisor` stand exercises mint → transfer → fault → revoke → respawn
  end to end in `tools/v02_test.sh`.

## 5. Limitations (v0.2.0)

- **One linear framebuffer**: no multi-monitor, no mode setting, no
  double buffering; `fb` writes directly to the MMIO window.
- **Uncached MMIO only**: `SYS_MAP_MMIO` uses `PCD | PWT`; write-combining
  would need PAT setup and is not implemented.
- **ASCII console**: 8x16 glyphs, no Unicode, font scaling, cursor
  blinking or ANSI escape parsing. Output beyond the fb queue is dropped.
- **No graphical input**: keyboard input still travels through PS/2 →
  `kbd` → `serial` → shell; `fb` is output-only.
- **Legacy virtio only**: no modern virtio 1.0, no MSI-X; the net driver
  polls the used ring instead of using IRQ 11.
- **Networking is a demo**: static IP, ARP + ICMP only; no DHCP, IPv6,
  UDP/TCP, checksum offload or scatter-gather.
- **Scheduler**: static affinity, no load balancing or priorities; the
  busy-spin SMP workers exist only as proof of parallel scheduling.
- **No TLB shootdown broadcast**: v0.2 adds a targeted IPI for the owner
  CPU; address spaces are still pinned to one CPU.
- **Minimal process model**: `SYS_SPAWN` loads embedded programs by id;
  no fork/exec, file descriptors or `wait`. No VFS/ramfs, devmgr or ACPI;
  PCI is root bus + legacy config only.
- **No SMAP/SMEP**, no global W^X; user copies validate page mapping and
  rights, but the hardware still permits a ring-0 access to a user page.
- **Fixed limits**: 16 caps/thread, 64-byte messages, 8 CPUs, one DMA
  window, one 256 MiB MMIO window, 256 CDT nodes, 32 notifications,
  16 in-flight replies, one shared window per process.
- **No `libnovax`**: `userspace/common/abi.rs` removes the duplicated
  syscall stubs, but there is no reusable userspace library crate yet;
  logging is direct `SYS_WRITE`, not a serial-server protocol.
- **No CI yet**: the tree is formatted and clippy-clean, but the gates
  above are manual (`kernel/memory` is linted separately because it is a
  standalone no_std workspace).
- **Test targets are disabled in Cargo**: `x86_64-unknown-none` has no
  libtest, so kernel behaviour is verified by the in-QEMU
  `--features test` suite instead of `cargo test`.
