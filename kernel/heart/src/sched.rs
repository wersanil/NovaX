//! CPU scheduler with per-CPU run queues.
//!
//! Every thread has a home CPU. Each CPU only ever picks threads assigned
//! to it, which lets the scheduler switch a thread out without another
//! CPU observing a half-saved stack pointer. Cross-CPU wakeups are
//! delivered with a reschedule IPI.

use alloc::boxed::Box;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::arch::smp;
use crate::arch::x86_64::switch_context;
use crate::ipc::Capability;

/// Stable thread identifier. Never reused within a boot.
pub type ThreadId = u64;

/// Lifecycle state of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Runnable; eligible for `pick_next`.
    Ready,
    /// Currently executing on its home CPU.
    Running,
    /// Waiting for an IPC message or a wakeup.
    Blocked,
    /// Exited; kept in the table so ids stay stable.
    #[allow(dead_code)]
    Dead,
}

/// Size of the kernel stack allocated for every thread.
pub const STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct Stack([u8; STACK_SIZE]);

/// Entry point of a kernel thread.
#[allow(dead_code)]
pub type EntryFn = extern "C" fn();

/// Thread control block.
pub struct Tcb {
    /// Unique id (also used for wakeups).
    #[allow(dead_code)]
    pub id: ThreadId,
    /// Current lifecycle state.
    pub state: State,
    /// Saved kernel stack pointer used by `switch_context`.
    pub rsp: u64,
    _stack: Option<Box<Stack>>,
    /// Human-readable name for logs.
    #[allow(dead_code)]
    pub name: &'static str,

    /// CPU that is allowed to run this thread.
    pub cpu: usize,
    /// Idle threads are only ever used as the per-CPU fallback; the
    /// round-robin scan skips them so real work is never starved.
    pub is_idle: bool,
    /// Set when a wakeup raced with the thread entering the Blocked
    /// state; `block_current` consumes it instead of descheduling.
    pub wake_pending: bool,

    /// Non-zero for user threads: address space to load on switch.
    pub is_user: bool,
    /// Top of the kernel stack (also written to TSS.rsp0).
    pub kernel_stack_top: u64,
    /// Physical PML4 of the process (0 for kernel threads).
    pub pml4_phys: u64,
    /// Ring-3 entry point (user threads only).
    pub user_entry: u64,
    /// Initial ring-3 stack pointer (user threads only).
    pub user_stack: u64,
    /// Capability space; index is the number used by syscalls.
    pub cspace: [Option<Capability>; 16],
}

/// The global thread table plus id allocation.
pub struct Scheduler {
    // Boxed on purpose: `switch_context` stores `&mut tcb.rsp` and the
    // saved stack pointer must stay valid even when the Vec reallocates
    // on a later `spawn`. `Vec<Tcb>` would invalidate those pointers.
    #[allow(clippy::vec_box)]
    threads: Vec<Box<Tcb>>,
    next_id: ThreadId,
}

impl Scheduler {
    fn new() -> Self {
        Self {
            threads: Vec::new(),
            next_id: 0,
        }
    }

    fn push(&mut self, tcb: Tcb) -> usize {
        let slot = self.threads.len();
        self.threads.push(Box::new(tcb));
        slot
    }
}

static SCHEDULER: spin::Once<Mutex<Scheduler>> = spin::Once::new();

/// Create the scheduler and register the BSP boot thread as slot 0.
pub fn init() {
    SCHEDULER.call_once(|| {
        let mut s = Scheduler::new();
        // Slot 0 is the BSP's boot thread. It only becomes CPU0's idle
        // once it has finished booting (`mark_self_idle`).
        let slot = s.push(Tcb {
            id: 0,
            state: State::Running,
            rsp: 0,
            _stack: None,
            name: "main",
            cpu: 0,
            is_idle: false,
            wake_pending: false,
            is_user: false,
            kernel_stack_top: 0,
            pml4_phys: 0,
            user_entry: 0,
            user_stack: 0,
            cspace: [None; 16],
        });
        s.next_id = 1;
        smp::percpu_at(0)
            .current
            .store(slot as u64, core::sync::atomic::Ordering::Release);
        smp::percpu_at(0)
            .idle_slot
            .store(slot as u64, core::sync::atomic::Ordering::Release);
        Mutex::new(s)
    });
}

/// Register the idle thread for an AP. Called from `ap_main` before the
/// AP enables interrupts.
#[cfg(not(feature = "test"))]
pub fn init_cpu_idle(cpu: usize) -> ThreadId {
    let mut s = SCHEDULER.get().expect("sched: call init first").lock();
    let id = s.next_id;
    s.next_id += 1;
    let pc = smp::percpu_at(cpu);
    let slot = s.push(Tcb {
        id,
        state: State::Running,
        rsp: 0,
        _stack: None,
        name: "cpu-idle",
        cpu,
        is_idle: true,
        wake_pending: false,
        is_user: false,
        kernel_stack_top: pc.boot_stack_top(),
        pml4_phys: 0,
        user_entry: 0,
        user_stack: 0,
        cspace: [None; 16],
    });
    pc.current
        .store(slot as u64, core::sync::atomic::Ordering::Release);
    pc.idle_slot
        .store(slot as u64, core::sync::atomic::Ordering::Release);
    id
}

/// Spawn a kernel thread pinned to CPU 0.
#[allow(dead_code)]
pub fn spawn(name: &'static str, entry: EntryFn) -> ThreadId {
    spawn_on(name, entry, 0)
}

/// Spawn a kernel thread pinned to `cpu`.
pub fn spawn_on(name: &'static str, entry: EntryFn, cpu: usize) -> ThreadId {
    interrupts::without_interrupts(|| {
        let mut s = SCHEDULER.get().expect("sched: call init first").lock();
        let id = s.next_id;
        s.next_id += 1;

        let mut stack = Box::new(Stack([0; STACK_SIZE]));
        let base = stack.0.as_mut_ptr() as u64;
        let top = (base + STACK_SIZE as u64) & !15u64;
        let initial_rsp = top - 72;

        // SAFETY: `initial_rsp` points into the freshly allocated 16 KiB
        // stack; the eight slots lie inside it and stay 16-byte aligned.
        unsafe {
            let p = initial_rsp as *mut u64;
            p.add(0).write(0);
            p.add(1).write(0);
            p.add(2).write(0);
            p.add(3).write(0);
            p.add(4).write(0);
            p.add(5).write(0);
            p.add(6).write(0x202);
            p.add(7).write(entry as usize as u64);
        }

        s.push(Tcb {
            id,
            state: State::Ready,
            rsp: initial_rsp,
            _stack: Some(stack),
            name,
            cpu: cpu.min(smp::cpu_count().saturating_sub(1)),
            is_idle: false,
            wake_pending: false,
            is_user: false,
            kernel_stack_top: top,
            pml4_phys: 0,
            user_entry: 0,
            user_stack: 0,
            cspace: [None; 16],
        });
        id
    })
}

/// Spawn a user thread with its own address space and capability space.
pub fn spawn_user(
    name: &'static str,
    pml4_phys: u64,
    entry: u64,
    user_stack: u64,
    cspace: [Option<Capability>; 16],
) -> ThreadId {
    interrupts::without_interrupts(|| {
        let mut s = SCHEDULER.get().expect("sched: call init first").lock();
        let id = s.next_id;
        s.next_id += 1;
        // Spread user threads across the CPUs that will come online.
        let cpu = (id as usize) % smp::cpu_count().max(1);

        let mut stack = Box::new(Stack([0; STACK_SIZE]));
        let base = stack.0.as_mut_ptr() as u64;
        let top = (base + STACK_SIZE as u64) & !15u64;
        let initial_rsp = top - 72;

        // SAFETY: `initial_rsp` points into the freshly allocated 16 KiB
        // stack; the eight slots lie inside it and stay 16-byte aligned.
        unsafe {
            let p = initial_rsp as *mut u64;
            p.add(0).write(0);
            p.add(1).write(0);
            p.add(2).write(0);
            p.add(3).write(0);
            p.add(4).write(0);
            p.add(5).write(0);
            p.add(6).write(0x202);
            p.add(7).write(crate::user::user_trampoline as usize as u64);
        }

        s.push(Tcb {
            id,
            state: State::Ready,
            rsp: initial_rsp,
            _stack: Some(stack),
            name,
            cpu,
            is_idle: false,
            wake_pending: false,
            is_user: true,
            kernel_stack_top: top,
            pml4_phys,
            user_entry: entry,
            user_stack,
            cspace,
        });
        id
    })
}

/// Yield the CPU: let the scheduler pick the next thread on this CPU.
pub fn yield_now() {
    interrupts::disable();
    schedule();
    interrupts::enable();
}

/// Pick and switch to the next runnable thread on the current CPU.
///
/// Must be called with interrupts disabled (timer handler, syscall paths
/// and explicit `yield_now` all guarantee this).
pub fn schedule() {
    let cpu = smp::cpu_index();
    let old_rsp_ptr: *mut u64;
    let new_rsp: u64;
    let new_pml4: u64;
    let new_ktop: u64;
    let to_user: bool;

    {
        let mut s = SCHEDULER.get().expect("sched").lock();
        let old = smp::percpu_at(cpu)
            .current
            .load(core::sync::atomic::Ordering::Acquire) as usize;
        let new = pick_next(&s, old, cpu);
        if new == old {
            return;
        }
        if s.threads[old].state == State::Running {
            s.threads[old].state = State::Ready;
        }
        s.threads[new].state = State::Running;
        s.threads[new].wake_pending = false;
        smp::percpu_at(cpu)
            .current
            .store(new as u64, core::sync::atomic::Ordering::Release);

        old_rsp_ptr = &mut s.threads[old].rsp as *mut u64;
        new_rsp = s.threads[new].rsp;
        new_pml4 = s.threads[new].pml4_phys;
        new_ktop = s.threads[new].kernel_stack_top;
        to_user = s.threads[new].is_user;
    }

    // SAFETY: `schedule` runs with IF=0; `new_pml4`/`new_ktop` come from a
    // TCB pinned to this CPU, and CR3/TSS writes are privileged ring-0 ops.
    unsafe {
        crate::arch::x86_64::switch_address_space(new_pml4, to_user);
        crate::arch::x86_64::set_kernel_stack(new_ktop);
    }

    // SAFETY: both stack pointers refer to frames saved by this scheduler;
    // `old_rsp_ptr` points into a boxed TCB, so it stays valid across the
    // switch, and `new_rsp` holds a frame with the same layout.
    unsafe {
        switch_context(old_rsp_ptr, new_rsp);
    }
}

/// Turn the calling thread into this CPU's idle fallback. Called by the
/// boot thread once all bring-up work is done.
#[cfg(not(feature = "test"))]
pub fn mark_self_idle() {
    interrupts::without_interrupts(|| {
        let mut s = SCHEDULER.get().expect("sched").lock();
        let cur = smp::current()
            .current
            .load(core::sync::atomic::Ordering::Acquire) as usize;
        s.threads[cur].is_idle = true;
        smp::current()
            .idle_slot
            .store(cur as u64, core::sync::atomic::Ordering::Release);
    });
}

/// Id of the thread currently running on this CPU.
pub fn current_id() -> ThreadId {
    let s = SCHEDULER.get().expect("sched").lock();
    let cur = smp::current()
        .current
        .load(core::sync::atomic::Ordering::Acquire) as usize;
    s.threads[cur].id
}

/// Number of threads in the table (including idle threads).
#[allow(dead_code)]
pub fn thread_count() -> usize {
    SCHEDULER.get().expect("sched").lock().threads.len()
}

/// Minimal copy of the user-mode fields of the current TCB.
pub struct TcbSnapshot {
    /// Ring-3 entry point.
    pub user_entry: u64,
    /// Ring-3 stack pointer.
    pub user_stack: u64,
}

/// Physical PML4 of the current thread (0 for kernel threads).
pub fn current_pml4_phys() -> u64 {
    let s = SCHEDULER.get().expect("sched").lock();
    let cur = smp::current()
        .current
        .load(core::sync::atomic::Ordering::Acquire) as usize;
    s.threads[cur].pml4_phys
}

/// Copy the current thread's user entry/stack for the ring-3 trampoline.
pub fn current_tcb_snapshot() -> TcbSnapshot {
    let s = SCHEDULER.get().expect("sched").lock();
    let cur = smp::current()
        .current
        .load(core::sync::atomic::Ordering::Acquire) as usize;
    let t = &s.threads[cur];
    TcbSnapshot {
        user_entry: t.user_entry,
        user_stack: t.user_stack,
    }
}

/// Mark the current thread dead and reschedule forever.
pub fn exit_current() -> ! {
    interrupts::disable();
    {
        let mut s = SCHEDULER.get().expect("sched").lock();
        let cur = smp::current()
            .current
            .load(core::sync::atomic::Ordering::Acquire) as usize;
        s.threads[cur].state = State::Dead;
    }
    loop {
        schedule();
    }
}

/// Block the current thread. If a wakeup arrived after `ipc::recv`
/// released its lock but before we got here, consume it and return
/// immediately instead of sleeping through the message.
pub fn block_current() {
    interrupts::disable();
    let should_block = {
        let mut s = SCHEDULER.get().expect("sched").lock();
        let cur = smp::current()
            .current
            .load(core::sync::atomic::Ordering::Acquire) as usize;
        if s.threads[cur].wake_pending {
            s.threads[cur].wake_pending = false;
            false
        } else {
            s.threads[cur].state = State::Blocked;
            true
        }
    };
    if should_block {
        schedule();
    }
    interrupts::enable();
}

/// Make `tid` runnable (or set `wake_pending` if it is already running).
/// Sends a reschedule IPI when the thread belongs to another CPU.
pub fn wake_thread(tid: ThreadId) {
    let home_cpu = {
        let mut s = SCHEDULER.get().expect("sched").lock();
        let Some(t) = s.threads.iter_mut().find(|t| t.id == tid) else {
            return;
        };
        match t.state {
            State::Blocked => {
                t.state = State::Ready;
            }
            State::Running => {
                t.wake_pending = true;
            }
            _ => {}
        }
        t.cpu
    };
    if home_cpu != smp::cpu_index() {
        crate::arch::x86_64::send_ipi(
            smp::percpu_at(home_cpu)
                .lapic_id
                .load(core::sync::atomic::Ordering::Acquire) as u8,
            crate::arch::x86_64::RESCHED_VECTOR,
        );
    }
}

/// Look up capability `idx` in the current thread's cspace.
pub fn get_current_cap(idx: u64) -> Option<Capability> {
    let s = SCHEDULER.get().expect("sched").lock();
    let cur = smp::current()
        .current
        .load(core::sync::atomic::Ordering::Acquire) as usize;
    s.threads[cur].cspace.get(idx as usize).copied().flatten()
}

fn pick_next(s: &Scheduler, from: usize, cpu: usize) -> usize {
    let n = s.threads.len();
    for i in 1..=n {
        let cand = (from + i) % n;
        if s.threads[cand].cpu != cpu {
            continue;
        }
        if s.threads[cand].is_idle {
            continue;
        }
        if s.threads[cand].state == State::Ready {
            return cand;
        }
    }
    let idle = smp::percpu_at(cpu)
        .idle_slot
        .load(core::sync::atomic::Ordering::Acquire) as usize;
    if idle < n {
        idle
    } else {
        from
    }
}
