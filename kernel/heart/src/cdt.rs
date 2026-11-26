//! Capability derivation tree (CDT).
//!
//! Every capability that participates in mint/transfer/revoke has a node
//! in a fixed static pool. Nodes form a doubly-linked tree: `parent` plus
//! a `first_child`/`next_sibling`/`prev_sibling` child list. No heap
//! allocation happens here; the pool is bounded by [`MAX_NODES`].
//!
//! `SYS_CAP_REVOKE` walks the children of a node and destroys their whole
//! subtrees, clearing the owning cspace slot and unmapping MMIO/DMA/shared
//! ranges as it goes. The root itself is never removed.

use crate::ipc::{CapKind, Capability, NO_NODE};
use crate::sched::ThreadId;
use spin::Mutex;

/// Maximum number of tracked capabilities (16 slots × many processes).
pub const MAX_NODES: usize = 256;

/// Node flag: capability owns an MMIO mapping.
pub const F_MMIO: u8 = 1;
/// Node flag: capability owns a DMA mapping/allocation.
pub const F_DMA: u8 = 2;
/// Node flag: capability owns a shared-memory mapping.
pub const F_SHM: u8 = 4;

#[derive(Clone, Copy, Debug)]
struct Node {
    parent: u16,
    first_child: u16,
    next_sibling: u16,
    prev_sibling: u16,
    owner: ThreadId,
    slot: u8,
    kind: CapKind,
    #[allow(dead_code)] // retained for future capability introspection
    rights: u32,
    flags: u8,
}

struct Pool {
    nodes: [Option<Node>; MAX_NODES],
}

static CDT: Mutex<Pool> = Mutex::new(Pool {
    nodes: [None; MAX_NODES],
});

fn flags_for(kind: CapKind) -> u8 {
    match kind {
        CapKind::Mmio { .. } => F_MMIO,
        CapKind::Dma { .. } | CapKind::DmaMapped { .. } => F_DMA,
        CapKind::Shm { .. } => F_SHM,
        _ => 0,
    }
}

fn alloc_node(pool: &mut Pool, node: Node) -> Option<u16> {
    for (i, slot) in pool.nodes.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(node);
            return Some(i as u16);
        }
    }
    None
}

fn link_child(pool: &mut Pool, parent: u16, child: u16) {
    let Some(parent_node) = pool.nodes.get(parent as usize).copied().flatten() else {
        return;
    };
    let first = parent_node.first_child;
    if let Some(child_node) = pool.nodes.get_mut(child as usize).and_then(|n| n.as_mut()) {
        child_node.parent = parent;
        child_node.prev_sibling = NO_NODE;
        child_node.next_sibling = first;
    }
    if first != NO_NODE {
        if let Some(first_node) = pool.nodes.get_mut(first as usize).and_then(|n| n.as_mut()) {
            first_node.prev_sibling = child;
        }
    }
    if let Some(parent_node) = pool.nodes.get_mut(parent as usize).and_then(|n| n.as_mut()) {
        parent_node.first_child = child;
    }
}

fn unlink(pool: &mut Pool, idx: u16) {
    let Some(node) = pool.nodes.get(idx as usize).copied().flatten() else {
        return;
    };
    if node.prev_sibling != NO_NODE {
        if let Some(prev) = pool
            .nodes
            .get_mut(node.prev_sibling as usize)
            .and_then(|n| n.as_mut())
        {
            prev.next_sibling = node.next_sibling;
        }
    } else if node.parent != NO_NODE {
        if let Some(parent) = pool
            .nodes
            .get_mut(node.parent as usize)
            .and_then(|n| n.as_mut())
        {
            parent.first_child = node.next_sibling;
        }
    }
    if node.next_sibling != NO_NODE {
        if let Some(next) = pool
            .nodes
            .get_mut(node.next_sibling as usize)
            .and_then(|n| n.as_mut())
        {
            next.prev_sibling = node.prev_sibling;
        }
    }
}

/// Allocate a root node for a capability that is about to be minted from.
pub fn alloc_root(owner: ThreadId, slot: u8, kind: CapKind, rights: u32) -> Option<u16> {
    let mut pool = CDT.lock();
    alloc_node(
        &mut pool,
        Node {
            parent: NO_NODE,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
            prev_sibling: NO_NODE,
            owner,
            slot,
            kind,
            rights,
            flags: flags_for(kind),
        },
    )
}

/// Allocate a child node under `parent` and link it into the child list.
pub fn alloc_child(
    parent: u16,
    owner: ThreadId,
    slot: u8,
    kind: CapKind,
    rights: u32,
) -> Option<u16> {
    let mut pool = CDT.lock();
    if pool.nodes.get(parent as usize)?.is_none() {
        return None;
    }
    let idx = alloc_node(
        &mut pool,
        Node {
            parent: NO_NODE,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
            prev_sibling: NO_NODE,
            owner,
            slot,
            kind,
            rights,
            flags: flags_for(kind),
        },
    )?;
    link_child(&mut pool, parent, idx);
    Some(idx)
}

/// True when `idx` names a live node.
pub fn exists(idx: u16) -> bool {
    idx != NO_NODE
        && CDT
            .lock()
            .nodes
            .get(idx as usize)
            .map(|n| n.is_some())
            .unwrap_or(false)
}

/// Ensure that `cap` has a CDT root node; create and store one if needed.
/// Called before minting or transferring a capability.
pub fn ensure_current_cap_node(cap_idx: u64, cap: Capability) -> Option<u16> {
    if cap.node != NO_NODE && exists(cap.node) {
        return Some(cap.node);
    }
    let owner = crate::sched::current_id();
    let node = alloc_root(owner, cap_idx as u8, cap.kind, cap.rights)?;
    if crate::sched::set_current_cap_node(cap_idx, node) {
        Some(node)
    } else {
        free_node(node);
        None
    }
}

/// Free one node without touching its children. Used only for rollback.
pub fn free_node(idx: u16) {
    let mut pool = CDT.lock();
    if idx != NO_NODE {
        unlink(&mut pool, idx);
        if let Some(slot) = pool.nodes.get_mut(idx as usize) {
            *slot = None;
        }
    }
}

/// Release the resources described by one node. Runs while the CDT lock is
/// held; the scheduler and memory locks are always taken after it, never
/// before, so the ordering is safe.
fn release_node(pool: &mut Pool, idx: u16) {
    let Some(node) = pool.nodes.get(idx as usize).copied().flatten() else {
        return;
    };
    crate::sched::clear_cap_if(node.owner, node.slot, idx);
    let Some(pml4) = crate::sched::pml4_of(node.owner) else {
        return;
    };
    if pml4 == 0 {
        return;
    }
    if node.flags & F_MMIO != 0 {
        if let CapKind::Mmio { paddr, len } = node.kind {
            crate::mmio::unmap_for(node.owner, pml4, paddr, len);
        }
    }
    if node.flags & F_DMA != 0 {
        crate::dma::free_for(node.owner);
    }
    if node.flags & F_SHM != 0 {
        crate::shm::unmap_for(node.owner, pml4);
    }
}

/// Destroy `root` and every descendant. Used for `revoke_owned` and for
/// tearing down a transferred capability that was never installed.
fn kill_subtree(pool: &mut Pool, root: u16) {
    let mut stack = [NO_NODE; MAX_NODES];
    let mut sp = 0usize;
    stack[sp] = root;
    sp += 1;
    while sp > 0 {
        sp -= 1;
        let idx = stack[sp];
        let Some(node) = pool.nodes.get(idx as usize).copied().flatten() else {
            continue;
        };
        // Push all children before detaching this node.
        let mut child = node.first_child;
        while child != NO_NODE {
            if sp < stack.len() {
                stack[sp] = child;
                sp += 1;
            }
            let next = pool
                .nodes
                .get(child as usize)
                .and_then(|n| n.as_ref())
                .map(|n| n.next_sibling)
                .unwrap_or(NO_NODE);
            child = next;
        }
        unlink(pool, idx);
        release_node(pool, idx);
        if let Some(slot) = pool.nodes.get_mut(idx as usize) {
            *slot = None;
        }
    }
}

/// Revoke every descendant of `parent`, leaving the root capability in
/// place. Returns the number of capabilities removed.
pub fn revoke_children(parent: u16) -> u32 {
    let mut pool = CDT.lock();
    if pool.nodes.get(parent as usize).copied().flatten().is_none() {
        return 0;
    }
    let mut removed = 0u32;
    loop {
        let child = pool
            .nodes
            .get(parent as usize)
            .and_then(|n| n.as_ref())
            .map(|n| n.first_child)
            .unwrap_or(NO_NODE);
        if child == NO_NODE {
            break;
        }
        let before = count_subtree(&pool, child);
        kill_subtree(&mut pool, child);
        removed += before;
    }
    removed
}

fn count_subtree(pool: &Pool, root: u16) -> u32 {
    let mut count = 0u32;
    let mut stack = [NO_NODE; MAX_NODES];
    let mut sp = 0usize;
    stack[sp] = root;
    sp += 1;
    while sp > 0 {
        sp -= 1;
        let idx = stack[sp];
        let Some(node) = pool.nodes.get(idx as usize).copied().flatten() else {
            continue;
        };
        count += 1;
        let mut child = node.first_child;
        while child != NO_NODE {
            if sp < stack.len() {
                stack[sp] = child;
                sp += 1;
            }
            child = pool
                .nodes
                .get(child as usize)
                .and_then(|n| n.as_ref())
                .map(|n| n.next_sibling)
                .unwrap_or(NO_NODE);
        }
    }
    count
}

/// Revoke every node owned by `tid`, including the roots. Descendants owned
/// by other processes are revoked too: a capability derived from a dead
/// process must not outlive it.
pub fn revoke_owned(tid: ThreadId) -> u32 {
    let mut pool = CDT.lock();
    let mut removed = 0u32;
    for idx in 0..MAX_NODES {
        let owned = pool
            .nodes
            .get(idx)
            .and_then(|n| n.as_ref())
            .map(|n| n.owner == tid)
            .unwrap_or(false);
        if owned {
            let count = count_subtree(&pool, idx as u16);
            kill_subtree(&mut pool, idx as u16);
            removed += count;
        }
    }
    removed
}
