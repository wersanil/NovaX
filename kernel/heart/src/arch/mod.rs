//! Architecture-specific code. Only x86_64 for now; ARM64 comes later
//! if NovaX is ported.

/// SMP bring-up, per-CPU state and AP bootstrap.
pub mod smp;
/// x86_64 CPU support: GDT/TSS/IDT, APIC, syscalls, context switch.
pub mod x86_64;
