# NovaX — Architecture & Project Summary (v0.1.0, Milestones 0–9)

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
  src/bootinfo.rs              Limine: revision, HHDM, memmap, MP
  src/heap.rs                  1 MiB heap @ 0xffff_ffff_9000_0000
  src/sched.rs                 TCB, per-CPU queues, affinity, switching
  src/ipc.rs                   endpoints, messages, caps, IRQ routes
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
  net/                         virtio-net server (ARP + ICMP)
tools/                         fetch_limine, mkiso, qemu_run, test,
                               m6/m7/m8/m9 test + check, git_commit
```

Every userspace program pulls in `userspace/common/abi.rs` with
`#[path = "../../common/abi.rs"] mod abi;`. No separate crate or cargo
project is needed, so `build.rs` still invokes plain `rustc` per program.
`boot/limine/`, `target/`, `build/` are excluded from this document.

## 2. Kernel architecture
### Boot and memory

Limine (base revision 3) provides HHDM, memory map and CPU list. The frame
allocator keeps a bitmap in the first large enough USABLE region. Init order:
per-CPU GDT/TSS → IDT → mask PIC → IOAPIC → LAPIC → syscalls → MP discovery;
APs start after all userspace threads are spawned.
- 4 KiB frames; all physical access via the HHDM. 4-level paging with
  `PRESENT`/`WRITABLE`/`USER`/`NO_EXEC`/`PWT`/`PCD`; intermediate tables
  are present+writable+user.
- Kernel heap: 1 MiB of mapped frames via `linked_list_allocator`.
- Each process gets a private PML4 (user half cleared, kernel half cloned);
  ELF `PT_LOAD` segments and a 2-page stack at `0x7fff_ffff_0000`.
- `dma.rs` maps contiguous zeroed frames at user VA `0x3000_0000` and
  returns `{virt, phys}` to the driver.
### Scheduler and SMP

- `Tcb`: id, state (Ready/Running/Blocked/Dead), saved `rsp`, 16 KiB kernel
  stack, `cpu` affinity, `is_idle`, `wake_pending`, userspace fields
  (`pml4_phys`, entry, stack, cspace).
- One `Mutex<Scheduler>` holds TCBs; each CPU has its own `current` and
  `idle_slot`. `pick_next` considers only threads pinned to that CPU and
  skips idle threads (idle is the fallback).
- `switch_context` (naked asm) saves rbp/rbx/r12–r15/rflags, swaps `rsp`.
  Address space and `TSS.rsp0` are updated before the switch.
- `wake_pending` closes the lost-wakeup race between `ipc::recv` and
  `block_current`; cross-CPU wakeups send a reschedule IPI.
- `PerCpu` (GS-based) holds kernel/user rsp, current index, LAPIC id, idle
  slot, TSS, GDT, boot/DF stacks. `CR4.FSGSBASE` stays clear: ring 3 cannot
  read it. AP boot: Limine `MpRequest` → `bootstrap(ap_entry, slot)`; the
  naked entry loads its stack, installs per-CPU GDT/TSS/IDT, enables the
  LAPIC, creates an idle thread and joins the scheduler. Max 8 CPUs.
- Vectors: timer `0x20`, reschedule IPI `0x30`, IRQ1 `0x21`, IRQ4 `0x24`.
  IOAPIC ISA lines are edge/active-high, destination = BSP LAPIC id.
### Syscall ABI

`syscall`/`sysretq`; `EFER.SCE=1`, `STAR` = kernel CS 0x08 / user CS 0x23,
`LSTAR=syscall_entry`, `FMASK=0x700` (clears IF, TF, DF).
- Input: `rax` = number, `rdi`, `rsi`, `rdx` = arguments.
- Output: `rax`; `rcx`/`r11` are clobbered by the instruction itself.
- Other GPRs preserved; per-CPU `kernel_rsp`/`user_rsp` via `gs:`.
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
Errors return `u64::MAX`; kernel internals use `Result<T, E>`.
### IPC and capabilities

- Endpoints have a FIFO (messages ≤64 bytes) and at most one blocked
  receiver. `send` wakes it (or sets `wake_pending` if it is running);
  `recv` returns a queued message or blocks.
- cspace = `[Option<Capability>; 16]`; `Capability = { kind, rights }`,
  rights = `CAP_READ`, `CAP_WRITE`, `CAP_NOTIFY`.
- Kinds: `Endpoint(id)`, `Irq(n)`, `IoPort { base, len }`, `Dma { len }`.
  IRQ model: the handler masks the line, enqueues one byte "IRQ n" on the
  registered endpoint and EOIs the LAPIC; the driver drains the device and
  calls `SYS_IRQ_ACK`, which unmasks the IOAPIC entry.

## 3. Userspace servers

| Program | Caps | Role |
|---------|------|------|
| `server` | ep(0) R/W | M5 demo receiver |
| `client` | ep(0) R/W | M5 demo sender (`ping`) |
| `serial` | Irq(4), irq_ep, IoPort 0x3F8+8, shell_in W | COM1 echo + shell input |
| `kbd` | Irq(1), irq_ep, IoPort 0x60+5 | PS/2 scancodes |
| `init` | init_ep R, reply_ep W | `echo <text>` / `unknown command` |
| `shell` | init_ep W, shell_in R, reply_ep R | line buffer + printing |
| `net` | IoPort(BAR0), Dma(64 KiB) | virtio-net, ARP, ICMP |
Path: keyboard/COM1 → `serial` → `shell_in_ep` → `shell` → `init_ep` →
`init` → `reply_ep` → `shell` → console via `SYS_WRITE`.
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
./tools/qemu_run.sh                      # q35, -smp 2, netdev user
# direct flags: -M q35 -smp 2 -m 512M -nographic -no-reboot \
#   -boot d -cdrom build/novax.iso -netdev user,id=n0 \
#   -device virtio-net-pci,netdev=n0,disable-modern=on
```

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
./tools/test.sh      # kernel self-tests, success = QEMU exit 33
```

Reference M9 log (trimmed):

```
NovaX booting...
hhdm offset: 0xffff800000000000
heap:  1024 KiB @ 0xffffffff90000000
ioapic: id=0, version=0x20, entries=24
lapic: bsp id=0, virt=0xffffffffb0000000
smp: 2 cpu(s) reported by limine
Milestone 5: OK
Milestone 6: userspace drivers spawned
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

### Release polish (v0.1.0)

- Every `unsafe` block carries a `SAFETY`/`WHY` rationale; public kernel
  structures and the `novax_memory` crate carry rustdoc.
- `build.rs` validates its environment instead of `unwrap()`-ing paths;
  userspace programs share `common/abi.rs` instead of duplicating stubs.
- `cargo fmt --all` applied; clippy is clean with `-D warnings` in the
  release, `--features test` and standalone `novax_memory` configurations.
- All five QEMU checks pass: `test`, `m6`, `m7`, `m8`, `m9`.

## 5. Limitations (v0.1.0)

- **Legacy virtio only**: no modern virtio 1.0, no MSI-X; the net driver
  polls the used ring instead of using IRQ 11.
- **Networking is a demo**: static IP, ARP + ICMP only; no DHCP, IPv6,
  UDP/TCP, checksum offload or scatter-gather.
- **Scheduler**: static affinity, no load balancing or priorities; the
  busy-spin SMP workers exist only as proof of parallel scheduling.
- **No TLB shootdown**; safe only because each process runs on one CPU.
- **No process model**: no fork/exec, file descriptors or `wait`; no
  VFS/ramfs, no devmgr, no ACPI; PCI is root bus + legacy config only.
- **No SMAP/SMEP**, no global W^X, user copies only range-checked.
- **Fixed limits**: 16 caps/thread, 64-byte messages, 8 CPUs, one DMA window.
- **No `libnovax`**: `userspace/common/abi.rs` removes the duplicated
  syscall stubs, but there is no reusable userspace library crate yet;
  logging is direct `SYS_WRITE`, not a serial-server protocol.
- **No CI yet**: the tree is formatted and clippy-clean, but the gates
  above are manual (`kernel/memory` is linted separately because it is a
  standalone no_std workspace).
- **Test targets are disabled in Cargo**: `x86_64-unknown-none` has no
  libtest, so kernel behaviour is verified by the in-QEMU
  `--features test` suite instead of `cargo test`.
