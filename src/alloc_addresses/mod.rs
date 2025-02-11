//! This module is responsible for managing the absolute addresses that allocations are located at,
//! and for casting between pointers and integers based on those addresses.

mod reuse_pool;
pub mod page_table;

use std::alloc::Layout;
use std::cell::RefCell;
use std::cmp::max;

use page_table::{PageTable, KERNEL_CODE_BASE_VADDR};
use physical_mem::{create_allocation_at, PageState, BASE_BEGIN, CPU_LOCAL_BEGIN, CPU_LOCAL_END, CPU_LOCAL_SIZE, KERNEL_MEM, PAGE_SIZE, PAGE_STATES, STACK_BEGIN};
use rand::Rng;
use rustc_abi::{Align, Size};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_span::Span;

use self::reuse_pool::ReusePool;
use crate::concurrency::VClock;
use crate::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProvenanceMode {
    /// We support `expose_provenance`/`with_exposed_provenance` via "wildcard" provenance.
    /// However, we warn on `with_exposed_provenance` to alert the user of the precision loss.
    Default,
    /// Like `Default`, but without the warning.
    Permissive,
    /// We error on `with_exposed_provenance`, ensuring no precision loss.
    Strict,
}

pub type GlobalState = RefCell<GlobalStateInner>;

#[derive(Debug)]
pub struct GlobalStateInner {
    /// This is used as a map between the address of each allocation and its `AllocId`. It is always
    /// sorted by address. We cannot use a `HashMap` since we can be given an address that is offset
    /// from the base address, and we need to find the `AllocId` it belongs to. This is not the
    /// *full* inverse of `base_addr`; dead allocations have been removed.
    pub int_to_ptr_map: Vec<(u64, AllocId)>,
    /// The base address for each allocation.  We cannot put that into
    /// `AllocExtra` because function pointers also have a base address, and
    /// they do not have an `AllocExtra`.
    /// This is the inverse of `int_to_ptr_map`.
    pub base_addr: FxHashMap<AllocId, u64>,
    /// Temporarily store prepared memory space for global allocations the first time their memory
    /// address is required. This is used to ensure that the memory is allocated before Miri assigns
    /// it an internal address, which is important for matching the internal address to the machine
    /// address so FFI can read from pointers.
    prepared_alloc_bytes: FxHashMap<AllocId, MiriAllocBytes>,
    /// A pool of addresses we can reuse for future allocations.
    reuse: ReusePool,
    /// Whether an allocation has been exposed or not. This cannot be put
    /// into `AllocExtra` for the same reason as `base_addr`.
    pub exposed: FxHashSet<AllocId>,
    /// This is used as a memory address when a new pointer is casted to an integer. It
    /// is always larger than any address that was previously made part of a block.
    next_base_addr: u64,

    next_cpu_local_addr: u64,

    pub next_stack_addr: u64,

    pub stack: Vec<u64>,
    /// The provenance to use for int2ptr casts
    provenance_mode: ProvenanceMode,

    pub page_table: Option<PageTable>,
}

impl VisitProvenance for GlobalStateInner {
    fn visit_provenance(&self, _visit: &mut VisitWith<'_>) {
        let GlobalStateInner {
            int_to_ptr_map: _,
            base_addr: _,
            prepared_alloc_bytes: _,
            reuse: _,
            exposed: _,
            next_base_addr: _,
            next_stack_addr: _,
            next_cpu_local_addr: _,
            stack: _,
            provenance_mode: _,
            page_table: _,
        } = self;
        // Though base_addr, int_to_ptr_map, and exposed contain AllocIds, we do not want to visit them.
        // int_to_ptr_map and exposed must contain only live allocations, and those
        // are never garbage collected.
        // base_addr is only relevant if we have a pointer to an AllocId and need to look up its
        // base address; so if an AllocId is not reachable from somewhere else we can remove it
        // here.
    }
}

impl GlobalStateInner {
    pub fn new(config: &MiriConfig, stack_addr: u64) -> Self {
        GlobalStateInner {
            int_to_ptr_map: Vec::default(),
            base_addr: FxHashMap::default(),
            prepared_alloc_bytes: FxHashMap::default(),
            reuse: ReusePool::new(config),
            exposed: FxHashSet::default(),
            next_base_addr: stack_addr,
            next_stack_addr: (CPU_LOCAL_BEGIN as usize + KERNEL_CODE_BASE_VADDR) as u64,
            next_cpu_local_addr: (CPU_LOCAL_BEGIN as usize + KERNEL_CODE_BASE_VADDR) as u64,
            stack: Vec::new(),
            provenance_mode: config.provenance_mode,
            page_table: None,
        }
    }

    pub fn remove_unreachable_allocs(&mut self, allocs: &LiveAllocs<'_, '_>) {
        // `exposed` and `int_to_ptr_map` are cleared immediately when an allocation
        // is freed, so `base_addr` is the only one we have to clean up based on the GC.
        self.base_addr.retain(|id, _| allocs.is_live(*id));
    }

    pub fn set_page_table(&mut self, page_table: PageTable) {
        self.page_table = Some(page_table);
    }

    pub fn set_address(&mut self, alloc_id: AllocId, paddr: usize) {
        let paddr = paddr as u64;
        let pos = if self
            .int_to_ptr_map
            .last()
            .is_some_and(|(last_addr, _)| *last_addr < paddr)
        {
            self.int_to_ptr_map.len()
        } else {
            self
                .int_to_ptr_map
                .binary_search_by_key(&paddr, |(addr, _)| *addr)
                .unwrap_err()
        };
        
        self.exposed.insert(alloc_id);
        self.int_to_ptr_map.insert(pos, (paddr, alloc_id));
        self.base_addr.insert(alloc_id, paddr);
    }
}

/// Shifts `addr` to make it aligned with `align` by rounding `addr` to the smallest multiple
/// of `align` that is larger or equal to `addr`
fn align_addr(addr: u64, align: u64) -> u64 {
    match addr % align {
        0 => addr,
        rem => addr.strict_add(align) - rem,
    }
}

impl<'tcx> EvalContextExtPriv<'tcx> for crate::MiriInterpCx<'tcx> {}

#[allow(invalid_reference_casting)]
pub trait EvalContextExtPriv<'tcx>: crate::MiriInterpCxExt<'tcx> {
    // Returns the exposed `AllocId` that corresponds to the specified addr,
    // or `None` if the addr is out of bounds
    fn alloc_id_from_addr(&self, vaddr: u64, size: i64) -> Option<AllocId> {
        let ecx = self.eval_context_ref();
        let global_state = ecx.machine.alloc_addresses.borrow();
        assert!(global_state.provenance_mode != ProvenanceMode::Strict);
        
        let addr = if let Some(page_table) = &global_state.page_table {
            page_table.page_walk(vaddr as usize)? as u64
        } else {
            vaddr
        };

        // We always search the allocation to the right of this address. So if the size is structly
        // negative, we have to search for `addr-1` instead.
        let addr = if size >= 0 { addr } else { addr.saturating_sub(1) };
        let pos = global_state.int_to_ptr_map.binary_search_by_key(&addr, |(addr, _)| *addr);

        // Determine the in-bounds provenance for this pointer.
        let alloc_id = match pos {
            Ok(pos) => Some(global_state.int_to_ptr_map[pos].1),
            Err(0) => {
                //None
                let addr = addr as usize;
                let page_num = addr / PAGE_SIZE;
                let page_info = unsafe {
                    PAGE_STATES[page_num]
                };

                if let PageState::Typed { page_type, type_size } = &page_info {
                    let mut alloc_map = ecx.memory.alloc_map().0.borrow_mut();
                    
                    let alloc_id = ecx.tcx.reserve_alloc_id();
                    let actual_addr = addr - addr % *type_size;
                    let kind = rustc_const_eval::interpret::MemoryKind::Machine(MiriMemoryKind::Kernel);
                    let allocation = {
                        let allocation = create_allocation_at(actual_addr, Layout::from_size_align(*type_size, *type_size).unwrap());
                        let extra = MiriMachine::init_alloc_extra(ecx, alloc_id, kind, allocation.size(), allocation.align).unwrap();
                        allocation.with_extra(extra)
                    };

                    alloc_map.insert(alloc_id, Box::new((kind, allocation)));
                    drop(global_state);
                    let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
                    global_state.set_address(alloc_id, actual_addr);
                    return Some(alloc_id);
                }

                let current_cpu_local_base = ecx.machine.threads.current_cpu_local_base();
                if (current_cpu_local_base..current_cpu_local_base + CPU_LOCAL_SIZE as usize).contains(&(vaddr as usize)) {
                    let original_vaddr = ecx.machine.threads.cpu_local_base[0] + vaddr as usize - current_cpu_local_base;
                    let original_addr = if let Some(page_table) = &global_state.page_table {
                        page_table.page_walk(original_vaddr as usize)? as u64
                    } else {
                        original_vaddr as u64
                    };
                    
                    let original_pos = global_state.int_to_ptr_map.binary_search_by_key(&original_addr, |(original_addr, _)| *original_addr);
                    let (original_alloc_id, offset) = match original_pos {
                        Ok(original_pos) => Some((global_state.int_to_ptr_map[original_pos].1, 0)),
                        Err(0) => {
                            None
                        },
                        Err(original_pos) => {
                            let (glb, alloc_id) = global_state.int_to_ptr_map[original_pos - 1];
                            let offset = original_addr - glb;
                            let size = ecx.get_alloc_info(alloc_id).0;

                            if offset < size.bytes() { Some((alloc_id, offset)) } else {
                                panic!("nonononono");
                            }
                        }
                    }.unwrap();

                    let original_alloc_info = ecx.get_alloc_info(original_alloc_id);
                    let (kind, original_alloc) = &ecx.memory.alloc_map().get(original_alloc_id).unwrap();
                    let kind = *kind;
                    let new_alloc_id = ecx.tcx.reserve_alloc_id();
                    let allocation = {
                        let mut new_allocation = create_allocation_at(addr - offset as usize, Layout::from_size_align(original_alloc_info.0.bytes_usize(), original_alloc_info.1.bytes_usize()).unwrap());
                        let extra = MiriMachine::init_alloc_extra(ecx, new_alloc_id, kind, original_alloc_info.0, original_alloc_info.1).unwrap();
                        
                        let alloc_range = rustc_middle::mir::interpret::alloc_range(Size::ZERO, original_alloc.size());
                        let init_mask = original_alloc.init_mask();

                        if !init_mask.is_range_initialized(alloc_range).is_err_and(|range| range.start == alloc_range.start && range.size == alloc_range.size) {
                            let alloc_size_usize = original_alloc.size().bytes_usize();
                            let src_ptr = original_alloc.get_bytes_unchecked_raw();
                            let mut dst_ptr = new_allocation.get_bytes_unchecked_raw_mut();
                            unsafe {
                                core::ptr::copy(src_ptr, dst_ptr, alloc_size_usize);
                            }
            
                            // Copy mask
                            let init_copy = init_mask.prepare_copy((0..alloc_size_usize).into());
                            new_allocation.init_mask_apply_copy(init_copy, alloc_range, 1);
            
                            // Copy provenance
                            let provenance_copy = original_alloc.provenance().prepare_copy(alloc_range, Size::ZERO, 1, ecx).unwrap();
                            new_allocation.provenance_apply_copy(provenance_copy);
                        }
                        
                        new_allocation.with_extra(extra)
                    };
                    drop(original_alloc);
                    ecx.machine.cpu_alloc_set.borrow_mut().insert(new_alloc_id);
                    ecx.memory.alloc_map().0.borrow_mut().insert(new_alloc_id, Box::new((kind, allocation)));
                    drop(global_state);
                    let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
                    global_state.set_address(new_alloc_id, addr - offset as usize);
                    return Some(new_alloc_id);
                }

                // if let PageState::Untyped = page_info {
                //     let mut alloc_map = ecx.memory.alloc_map().0.borrow_mut();
                    
                //     let alloc_id = ecx.tcx.reserve_alloc_id();
                //     let actual_addr = addr - addr % 4096;
                //     let kind = rustc_const_eval::interpret::MemoryKind::Machine(MiriMemoryKind::Kernel);
                //     let allocation = {
                //         let allocation = create_allocation_at(actual_addr, Layout::from_size_align(4096, 4096).unwrap());
                //         let extra = MiriMachine::init_alloc_extra(ecx, alloc_id, kind, allocation.size(), allocation.align).unwrap();
                //         allocation.with_extra(extra)
                //     };

                //     alloc_map.insert(alloc_id, Box::new((kind, allocation)));
                //     drop(global_state);
                //     let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
                //     global_state.set_address(alloc_id, actual_addr);
                //     return Some(alloc_id);
                // }
                
                return None;
            },
            Err(pos) => {
                // This is the largest of the addresses smaller than `int`,
                // i.e. the greatest lower bound (glb)
                let (glb, alloc_id) = global_state.int_to_ptr_map[pos - 1];
                // This never overflows because `addr >= glb`
                let offset = addr - glb;
                // We require this to be strict in-bounds of the allocation. This arm is only
                // entered for addresses that are not the base address, so even zero-sized
                // allocations will get recognized at their base address -- but all other
                // allocations will *not* be recognized at their "end" address.
                let size = ecx.get_alloc_info(alloc_id).0;

                if offset < size.bytes() { Some(alloc_id) } else { 
                    let addr = addr as usize;
                    let page_num = addr / PAGE_SIZE;
                    let page_info = unsafe {
                        PAGE_STATES[page_num]
                    };

                    if let PageState::Typed { page_type, type_size } = page_info {
                        let mut alloc_map = ecx.memory.alloc_map().0.borrow_mut();
                        
                        let alloc_id = ecx.tcx.reserve_alloc_id();
                        let actual_addr = addr - addr % type_size;
                        let kind = rustc_const_eval::interpret::MemoryKind::Machine(MiriMemoryKind::Kernel);
                        let allocation = {
                            let allocation = create_allocation_at(actual_addr, Layout::from_size_align(type_size, type_size).unwrap());
                            let extra = MiriMachine::init_alloc_extra(ecx, alloc_id, kind, allocation.size(), allocation.align).unwrap();
                            allocation.with_extra(extra)
                        };

                        alloc_map.insert(alloc_id, Box::new((kind, allocation)));
                        drop(global_state);
                        let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
                        global_state.set_address(alloc_id, actual_addr);
                        return Some(alloc_id);
                    }

                    let current_cpu_local_base = ecx.machine.threads.current_cpu_local_base();
                    if (current_cpu_local_base..current_cpu_local_base + CPU_LOCAL_SIZE as usize).contains(&(vaddr as usize)) {
                        let original_vaddr = ecx.machine.threads.cpu_local_base[0] + vaddr as usize - current_cpu_local_base;
                        let original_addr = if let Some(page_table) = &global_state.page_table {
                            page_table.page_walk(original_vaddr as usize)? as u64
                        } else {
                            original_vaddr as u64
                        };
                        
                        let original_pos = global_state.int_to_ptr_map.binary_search_by_key(&original_addr, |(original_addr, _)| *original_addr);
                        let (original_alloc_id, offset) = match original_pos {
                            Ok(original_pos) => Some((global_state.int_to_ptr_map[original_pos].1, 0)),
                            Err(0) => {
                                None
                            },
                            Err(original_pos) => {
                                let (glb, alloc_id) = global_state.int_to_ptr_map[original_pos - 1];
                                let offset = original_addr - glb;
                                let size = ecx.get_alloc_info(alloc_id).0;
    
                                if offset < size.bytes() { Some((alloc_id, offset)) } else {
                                    panic!();
                                }
                            }
                        }.unwrap();
    
                        let original_alloc_info = ecx.get_alloc_info(original_alloc_id);
                        
                        let new_alloc_id = ecx.tcx.reserve_alloc_id();
                        
                        let (kind, original_alloc) = 
                            &ecx.memory.alloc_map().get(original_alloc_id).unwrap();
                        let kind = *kind;
                        let allocation = {
                            let mut new_allocation = create_allocation_at(addr - offset as usize, Layout::from_size_align(original_alloc_info.0.bytes_usize(), original_alloc_info.1.bytes_usize()).unwrap());
                            let extra = MiriMachine::init_alloc_extra(ecx, new_alloc_id, kind, original_alloc_info.0, original_alloc_info.1).unwrap();
                            
                            
                            let alloc_range = rustc_middle::mir::interpret::alloc_range(Size::ZERO, original_alloc.size());
                            let init_mask = original_alloc.init_mask();
    
                            if !init_mask.is_range_initialized(alloc_range).is_err_and(|range| range.start == alloc_range.start && range.size == alloc_range.size) {
                                let alloc_size_usize = original_alloc.size().bytes_usize();
                                let src_ptr = original_alloc.get_bytes_unchecked_raw();
                                let mut dst_ptr = new_allocation.get_bytes_unchecked_raw_mut();
                                unsafe {
                                    core::ptr::copy(src_ptr, dst_ptr, alloc_size_usize);
                                }
                
                                // Copy mask
                                let init_copy = init_mask.prepare_copy((0..alloc_size_usize).into());
                                new_allocation.init_mask_apply_copy(init_copy, alloc_range, 1);
                
                                // Copy provenance
                                let provenance_copy = original_alloc.provenance().prepare_copy(alloc_range, Size::ZERO, 1, ecx).unwrap();
                                new_allocation.provenance_apply_copy(provenance_copy);
                            }
                            
                            new_allocation.with_extra(extra)
                        };
                        
                        ecx.memory.alloc_map().0.borrow_mut().insert(new_alloc_id, Box::new((kind, allocation)));
                        drop(original_alloc);
                        drop(global_state);
                        ecx.machine.cpu_alloc_set.borrow_mut().insert(new_alloc_id);
                        let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
                        global_state.set_address(new_alloc_id, addr - offset as usize);
                        
                        return Some(new_alloc_id);
                    }

                    return None;
                }
            }
        }?;

        // We only use this provenance if it has been exposed.
        if global_state.exposed.contains(&alloc_id) {
            // This must still be live, since we remove allocations from `int_to_ptr_map` when they get freed.
            debug_assert!(ecx.is_alloc_live(alloc_id));
            Some(alloc_id)
        } else {
            None
        }
    }

    fn addr_from_alloc_id_uncached(
        &self,
        global_state: &mut GlobalStateInner,
        alloc_id: AllocId,
        memory_kind: MemoryKind,
    ) -> InterpResult<'tcx, u64> {
        let ecx = self.eval_context_ref();
        let mut rng = ecx.machine.rng.borrow_mut();
        let (size, align, kind) = ecx.get_alloc_info(alloc_id);
        // This is either called immediately after allocation (and then cached), or when
        // adjusting `tcx` pointers (which never get freed). So assert that we are looking
        // at a live allocation. This also ensures that we never re-assign an address to an
        // allocation that previously had an address, but then was freed and the address
        // information was removed.
        assert!(!matches!(kind, AllocKind::Dead));

        // This allocation does not have a base address yet, pick or reuse one.
        if ecx.machine.native_lib.is_some() {
            // In native lib mode, we use the "real" address of the bytes for this allocation.
            // This ensures the interpreted program and native code have the same view of memory.
            let base_ptr = match kind {
                AllocKind::LiveData => {
                    if ecx.tcx.try_get_global_alloc(alloc_id).is_some() {
                        // For new global allocations, we always pre-allocate the memory to be able use the machine address directly.
                        let prepared_bytes = MiriAllocBytes::zeroed(size, align)
                            .unwrap_or_else(|| {
                                panic!("Miri ran out of memory: cannot create allocation of {size:?} bytes")
                            });
                        let ptr = prepared_bytes.as_ptr();
                        // Store prepared allocation space to be picked up for use later.
                        global_state
                            .prepared_alloc_bytes
                            .try_insert(alloc_id, prepared_bytes)
                            .unwrap();
                        ptr
                    } else {
                        ecx.get_alloc_bytes_unchecked_raw(alloc_id)?
                    }
                }
                AllocKind::Function | AllocKind::VTable => {
                    // Allocate some dummy memory to get a unique address for this function/vtable.
                    let alloc_bytes =
                        MiriAllocBytes::from_bytes(&[0u8; 1], Align::from_bytes(1).unwrap());
                    let ptr = alloc_bytes.as_ptr();
                    // Leak the underlying memory to ensure it remains unique.
                    std::mem::forget(alloc_bytes);
                    ptr
                }
                AllocKind::Dead => unreachable!(),
            };
            // Ensure this pointer's provenance is exposed, so that it can be used by FFI code.
            return interp_ok(base_ptr.expose_provenance().try_into().unwrap());
        }
        // We are not in native lib mode, so we control the addresses ourselves.
        if let Some((reuse_addr, clock)) =
            global_state.reuse.take_addr(&mut *rng, size, align, memory_kind, ecx.active_thread())
        {   
            if let Some(clock) = clock {
                ecx.acquire_clock(&clock);
            }
            interp_ok(reuse_addr)
        } else {
            let base_addr = if memory_kind == MemoryKind::Stack {
                let thread = ecx.machine.threads.active_thread_ref();
                let mut next_stack_addr = thread.next_stack_addr.borrow_mut();
                let base_addr = *next_stack_addr - max(size.bytes(), 1);
                let base_addr = base_addr - base_addr % align.bytes();
                
                if base_addr < thread.stack_bottom as u64 {
                    throw_exhaust!(AddressSpaceFull);
                }
                *next_stack_addr = base_addr;
                
                base_addr
            } else {
                let (mut next_address, limit) = if ecx.machine.cpu_alloc_set.borrow().contains(&alloc_id) {
                    (&mut global_state.next_cpu_local_addr, CPU_LOCAL_END + KERNEL_CODE_BASE_VADDR as u64)
                } else {
                    (&mut global_state.next_base_addr, STACK_BEGIN + KERNEL_CODE_BASE_VADDR as u64)
                };

                // We have to pick a fresh address.
                // Leave some space to the previous allocation, to give it some chance to be less aligned.
                // We ensure that `(global_state.next_base_addr + slack) % 16` is uniformly distributed.
                let slack = rng.gen_range(0..16);
                // From next_base_addr + slack, round up to adjust for alignment.
                let base_addr = next_address
                    .checked_add(slack)
                    .ok_or_else(|| err_exhaust!(AddressSpaceFull))?;
                let base_addr = align_addr(base_addr, align.bytes());
                if base_addr >= limit {
                    throw_exhaust!(AddressSpaceFull);
                }

                // Remember next base address.  If this allocation is zero-sized, leave a gap of at
                // least 1 to avoid two allocations having the same base address. (The logic in
                // `alloc_id_from_addr` assumes unique addresses, and different function/vtable pointers
                // need to be distinguishable!)
                *next_address = base_addr
                    .checked_add(max(size.bytes(), 1))
                    .ok_or_else(|| err_exhaust!(AddressSpaceFull))?;
                // Even if `Size` didn't overflow, we might still have filled up the address space.
                if *next_address > ecx.target_usize_max() {
                    throw_exhaust!(AddressSpaceFull);
                }
                base_addr
            };

            interp_ok(base_addr)
        }
    }

    fn addr_from_alloc_id(
        &self,
        alloc_id: AllocId,
        memory_kind: MemoryKind,
    ) -> InterpResult<'tcx, u64> {
        let ecx = self.eval_context_ref();
        let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
        let global_state = &mut *global_state;

        let addr = match global_state.base_addr.get(&alloc_id) {
            Some(&addr) => addr + KERNEL_CODE_BASE_VADDR as u64,
            None => {
                // First time we're looking for the absolute address of this allocation.
                let base_vaddr =
                    self.addr_from_alloc_id_uncached(global_state, alloc_id, memory_kind)?;
                trace!("Assigning base address {:#x} to allocation {:?}", base_vaddr, alloc_id);

                let base_addr = if let Some(page_table) = &global_state.page_table {
                    page_table.page_walk(base_vaddr as usize).unwrap() as u64
                } else {
                    base_vaddr - KERNEL_CODE_BASE_VADDR as u64
                };
                // Store address in cache.
                global_state.base_addr.try_insert(alloc_id, base_addr).unwrap();

                // Also maintain the opposite mapping in `int_to_ptr_map`, ensuring we keep it sorted.
                // We have a fast-path for the common case that this address is bigger than all previous ones.
                let pos = if global_state
                    .int_to_ptr_map
                    .last()
                    .is_some_and(|(last_addr, _)| *last_addr < base_addr)
                {
                    global_state.int_to_ptr_map.len()
                } else {
                    let res = global_state
                        .int_to_ptr_map
                        .binary_search_by_key(&base_addr, |(addr, _)| *addr);
                    res.unwrap_err()
                };
                global_state.int_to_ptr_map.insert(pos, (base_addr, alloc_id));

                base_vaddr
            }
        };
        
        interp_ok(addr)
    }
}

impl<'tcx> EvalContextExt<'tcx> for crate::MiriInterpCx<'tcx> {}
pub trait EvalContextExt<'tcx>: crate::MiriInterpCxExt<'tcx> {
    fn expose_ptr(&mut self, alloc_id: AllocId, tag: BorTag) -> InterpResult<'tcx> {
        let ecx = self.eval_context_mut();
        let global_state = ecx.machine.alloc_addresses.get_mut();
        // In strict mode, we don't need this, so we can save some cycles by not tracking it.
        if global_state.provenance_mode == ProvenanceMode::Strict {
            return interp_ok(());
        }
        // Exposing a dead alloc is a no-op, because it's not possible to get a dead allocation
        // via int2ptr.
        if !ecx.is_alloc_live(alloc_id) {
            return interp_ok(());
        }
        trace!("Exposing allocation id {alloc_id:?}");
        let global_state = ecx.machine.alloc_addresses.get_mut();
        global_state.exposed.insert(alloc_id);
        if ecx.machine.borrow_tracker.is_some() {
            ecx.expose_tag(alloc_id, tag)?;
        }
        interp_ok(())
    }

    fn ptr_from_addr_cast(&self, addr: u64) -> InterpResult<'tcx, Pointer> {
        trace!("Casting {:#x} to a pointer", addr);

        let ecx = self.eval_context_ref();
        let global_state = ecx.machine.alloc_addresses.borrow();

        // Potentially emit a warning.
        match global_state.provenance_mode {
            ProvenanceMode::Default => {
                // The first time this happens at a particular location, print a warning.
                thread_local! {
                    // `Span` is non-`Send`, so we use a thread-local instead.
                    static PAST_WARNINGS: RefCell<FxHashSet<Span>> = RefCell::default();
                }
                PAST_WARNINGS.with_borrow_mut(|past_warnings| {
                    let first = past_warnings.is_empty();
                    if past_warnings.insert(ecx.cur_span()) {
                        // Newly inserted, so first time we see this span.
                        ecx.emit_diagnostic(NonHaltingDiagnostic::Int2Ptr { details: first });
                    }
                });
            }
            ProvenanceMode::Strict => {
                throw_machine_stop!(TerminationInfo::Int2PtrWithStrictProvenance);
            }
            ProvenanceMode::Permissive => {}
        }

        // We do *not* look up the `AllocId` here! This is a `ptr as usize` cast, and it is
        // completely legal to do a cast and then `wrapping_offset` to another allocation and only
        // *then* do a memory access. So the allocation that the pointer happens to point to on a
        // cast is fairly irrelevant. Instead we generate this as a "wildcard" pointer, such that
        // *every time the pointer is used*, we do an `AllocId` lookup to find the (exposed)
        // allocation it might be referencing.
        interp_ok(Pointer::new(Some(Provenance::Wildcard), Size::from_bytes(addr)))
    }

    /// Convert a relative (tcx) pointer to a Miri pointer.
    fn adjust_alloc_root_pointer(
        &self,
        ptr: interpret::Pointer<CtfeProvenance>,
        tag: BorTag,
        kind: MemoryKind,
    ) -> InterpResult<'tcx, interpret::Pointer<Provenance>> {
        let ecx = self.eval_context_ref();

        let (prov, offset) = ptr.into_parts(); // offset is relative (AllocId provenance)
        let alloc_id = prov.alloc_id();

        let base_addr = ecx.addr_from_alloc_id(alloc_id, kind)?;

        let base_paddr = {
            let global_state = ecx.machine.alloc_addresses.borrow();
            *global_state.base_addr.get(&alloc_id).unwrap()
        };
        let alloc_map = &ecx.memory.alloc_map();

        if base_paddr >= BASE_BEGIN && kind == MemoryKind::Stack.into() {
            let (kind, old_allocation) = &alloc_map.get(alloc_id).unwrap();
            let alloc_size_usize = old_allocation.size().bytes_usize();
            if alloc_size_usize > 0 {
                let (new_allocation, kind) = {
                    
                    let mut allocation = create_allocation_at(base_paddr as usize, Layout::from_size_align(old_allocation.size().bytes_usize(), old_allocation.align.bytes_usize()).unwrap());
                    let extra = MiriMachine::init_alloc_extra(ecx, alloc_id, *kind, old_allocation.size(), old_allocation.align)?;
                    
                    let alloc_range = rustc_middle::mir::interpret::alloc_range(Size::ZERO, old_allocation.size());
                    let init_mask = old_allocation.init_mask();
                    
                    if unsafe {*(old_allocation.get_bytes_unchecked_raw() as *const u8)} == 0xAA {
                        println!("AA size: {:?}", alloc_size_usize);
                    }
                    if !init_mask.is_range_initialized(alloc_range).is_err_and(|range| range.start == alloc_range.start && range.size == alloc_range.size) {
                        // Copy context
                        let src_ptr = old_allocation.get_bytes_unchecked_raw();
                        let mut dst_ptr = allocation.get_bytes_unchecked_raw_mut();
                        unsafe {
                            core::ptr::copy(src_ptr, dst_ptr, alloc_size_usize);
                        }
        
                        // Copy mask
                        let init_copy = init_mask.prepare_copy((0..alloc_size_usize).into());
                        allocation.init_mask_apply_copy(init_copy, alloc_range, 1);
        
                        // Copy provenance
                        let provenance_copy = old_allocation.provenance().prepare_copy(alloc_range, Size::ZERO, 1, ecx).unwrap();
                        allocation.provenance_apply_copy(provenance_copy);
                    }
                    (allocation.with_extra(extra), *kind)
                }; 

                alloc_map.0.borrow_mut().insert(alloc_id, Box::new((kind,new_allocation)));
            }
        }
        // Get a pointer to the beginning of this allocation.
        
        let base_ptr = interpret::Pointer::new(
            Provenance::Concrete { alloc_id, tag },
            Size::from_bytes(base_addr),
        );
        // Add offset with the right kind of pointer-overflowing arithmetic.
        interp_ok(base_ptr.wrapping_offset(offset, ecx))
    }

    // This returns some prepared `MiriAllocBytes`, either because `addr_from_alloc_id` reserved
    // memory space in the past, or by doing the pre-allocation right upon being called.
    fn get_global_alloc_bytes(
        &self,
        id: AllocId,
        kind: MemoryKind,
        bytes: &[u8],
        align: Align,
    ) -> InterpResult<'tcx, MiriAllocBytes> {
        let ecx = self.eval_context_ref();
        if ecx.machine.native_lib.is_some() {
            // In native lib mode, MiriAllocBytes for global allocations are handled via `prepared_alloc_bytes`.
            // This additional call ensures that some `MiriAllocBytes` are always prepared, just in case
            // this function gets called before the first time `addr_from_alloc_id` gets called.
            ecx.addr_from_alloc_id(id, kind)?;
            // The memory we need here will have already been allocated during an earlier call to
            // `addr_from_alloc_id` for this allocation. So don't create a new `MiriAllocBytes` here, instead
            // fetch the previously prepared bytes from `prepared_alloc_bytes`.
            let mut global_state = ecx.machine.alloc_addresses.borrow_mut();
            let mut prepared_alloc_bytes = global_state
                .prepared_alloc_bytes
                .remove(&id)
                .unwrap_or_else(|| panic!("alloc bytes for {id:?} have not been prepared"));
            // Sanity-check that the prepared allocation has the right size and alignment.
            assert!(prepared_alloc_bytes.as_ptr().is_aligned_to(align.bytes_usize()));
            assert_eq!(prepared_alloc_bytes.len(), bytes.len());
            // Copy allocation contents into prepared memory.
            prepared_alloc_bytes.copy_from_slice(bytes);
            interp_ok(prepared_alloc_bytes)
        } else {
            interp_ok(MiriAllocBytes::from_bytes(std::borrow::Cow::Borrowed(bytes), align))
        }
    }

    /// When a pointer is used for a memory access, this computes where in which allocation the
    /// access is going.
    fn ptr_get_alloc(
        &self,
        ptr: interpret::Pointer<Provenance>,
        size: i64,
    ) -> Option<(AllocId, Size)> {
        let ecx = self.eval_context_ref();
        let (tag, addr) = ptr.into_parts(); // addr is absolute (Tag provenance)

        let alloc_id = if let Provenance::Concrete { alloc_id, .. } = tag {
            alloc_id
        } else {
            // A wildcard pointer.
            ecx.alloc_id_from_addr(addr.bytes(), size)?
        };

        let global_state = ecx.machine.alloc_addresses.borrow();

        // This cannot fail: since we already have a pointer with that provenance, adjust_alloc_root_pointer
        // must have been called in the past, so we can just look up the address in the map.
        let mut base_addr = *global_state.base_addr.get(&alloc_id).unwrap();

        let actual_addr = if let Some(page_table) = &global_state.page_table {
            page_table.page_walk(addr.bytes() as usize)? as u64
        } else {
            addr.bytes() - KERNEL_CODE_BASE_VADDR as u64
        };

        let offset = actual_addr.wrapping_sub(base_addr);
        // let offset = if addr.bytes() >= KERNEL_CODE_BASE_VADDR as u64 {
        //     (addr.bytes() - KERNEL_CODE_BASE_VADDR as u64).wrapping_sub(base_addr)
        // } else {
        //     let actual_addr = if let Some(page_table) = &global_state.page_table {
        //         page_table.page_walk(addr.bytes() as usize)? as u64
        //     } else {
        //         addr.bytes()
        //     };
        //     actual_addr.wrapping_sub(base_addr)
        // };

        // Wrapping "addr - base_addr"
        let rel_offset = ecx.truncate_to_target_usize(offset);
        Some((alloc_id, Size::from_bytes(rel_offset)))
    }
}

impl<'tcx> MiriMachine<'tcx> {
    pub fn free_alloc_id(&mut self, dead_id: AllocId, size: Size, align: Align, kind: MemoryKind) {
        let global_state = self.alloc_addresses.get_mut();
        let rng = self.rng.get_mut();

        // We can *not* remove this from `base_addr`, since the interpreter design requires that we
        // be able to retrieve an AllocId + offset for any memory access *before* we check if the
        // access is valid. Specifically, `ptr_get_alloc` is called on each attempt at a memory
        // access to determine the allocation ID and offset -- and there can still be pointers with
        // `dead_id` that one can attempt to use for a memory access. `ptr_get_alloc` may return
        // `None` only if the pointer truly has no provenance (this ensures consistent error
        // messages).
        // However, we *can* remove it from `int_to_ptr_map`, since any wildcard pointers that exist
        // can no longer actually be accessing that address. This ensures `alloc_id_from_addr` never
        // returns a dead allocation.
        // To avoid a linear scan we first look up the address in `base_addr`, and then find it in
        // `int_to_ptr_map`.
        let addr = *global_state.base_addr.get(&dead_id).unwrap();
        let pos =
            global_state.int_to_ptr_map.binary_search_by_key(&addr, |(addr, _)| *addr).unwrap();
        let removed = global_state.int_to_ptr_map.remove(pos);
        assert_eq!(removed, (addr, dead_id)); // double-check that we removed the right thing
        // We can also remove it from `exposed`, since this allocation can anyway not be returned by
        // `alloc_id_from_addr` any more.
        global_state.exposed.remove(&dead_id);
        // Also remember this address for future reuse.
        let thread = self.threads.active_thread();
        
        //println!("free: 0x{:x}, 0x{:x}, {:?}, {:?}", global_state.next_stack_addr, addr, size, kind);

        global_state.reuse.add_addr(rng, addr, size, align, kind, thread, || {
            if let Some(data_race) = &self.data_race {
                data_race.release_clock(&self.threads, |clock| clock.clone())
            } else {
                VClock::default()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_addr() {
        assert_eq!(align_addr(37, 4), 40);
        assert_eq!(align_addr(44, 4), 44);
    }
}
