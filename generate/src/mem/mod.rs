use std::{
    collections::{HashMap, BTreeSet},
    fmt, mem,
    ops::Range,
};

use abi::size::Size;
use index_vec::{define_index_type, IndexVec};
use mir::{
    syntax::{TyId, TyKind},
    tyctxt::TyCtxt,
};
use rangemap::RangeMap;
use smallvec::SmallVec;

use crate::ptable::ProjectionIndex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AbstractByte {
    /// An uninitialized byte.
    Uninit,
    /// An initialized byte, optionally with some provenance (if it is encoding a pointer).
    Init,
}

impl fmt::Debug for AbstractByte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uninit => write!(f, "UU"),
            Self::Init => write!(f, "II"),
        }
    }
}

impl AbstractByte {
    pub fn is_init(&self) -> bool {
        self == &AbstractByte::Init
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowType {
    Raw,
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Borrow {
    borrow_type: BorrowType,
    edge: ProjectionIndex,
}

/// A Run represents a contiguous region of memory free of padding
#[derive(Debug, Clone)]
pub struct Run {
    bytes: Box<[AbstractByte]>,
    ref_stack: RangeMap<Vec<Borrow>>,
}

impl Run {
    pub fn new_uninit(size: Size) -> Self {
        let bytes = vec![AbstractByte::Uninit; size.bytes() as usize].into_boxed_slice();
        let ref_stack = RangeMap::new(size, vec![]);
        Self { bytes, ref_stack }
    }

    pub fn size(&self) -> Size {
        Size::from_bytes(self.bytes.len())
    }

    pub fn add_borrow(
        &mut self,
        offset: Size,
        len: Size,
        borrow_type: BorrowType,
        edge: ProjectionIndex,
    ) {
        for (_, stack) in self.ref_stack.iter_mut(offset, len) {
            stack.push(Borrow { borrow_type, edge });
        }
    }

    pub fn remove_borrow(&mut self, offset: Size, len: Size, edge: ProjectionIndex) {
        for (_, stack) in self.ref_stack.iter_mut(offset, len) {
            if let Some(i) = stack.iter().position(|b| b.edge == edge) {
                stack.remove(i);
            }
        }
    }

    /// Gets all edges including and below edge (and therefore potentially borrowed from it)
    pub fn below(&self, offset: Size, len: Size, edge: ProjectionIndex) -> Vec<ProjectionIndex> {
        let mut edges = BTreeSet::new();
        for (_, stack) in self.ref_stack.iter(offset, len) {
            let index = stack.iter().position(|borrow| borrow.edge == edge);
            if let Some(index) = index {
                edges.extend(stack[index..].iter().map(|borrow| borrow.edge));
            }
        }
        edges.iter().copied().collect()
    }

    pub fn first_shared(&self, offset: Size, len: Size) -> Option<ProjectionIndex> {
        for (_, stack) in self.ref_stack.iter(offset, len) {
            let first_shared = stack
                .iter()
                .position(|borrow| borrow.borrow_type == BorrowType::Shared);
            return first_shared.map(|index| stack[index].edge);
        }
        None
    }

    pub fn below_first_shared(&self, offset: Size, len: Size) -> Vec<ProjectionIndex> {
        let mut edges = BTreeSet::new();
        for (_, stack) in self.ref_stack.iter(offset, len) {
            let first_shared = stack
                .iter()
                .position(|borrow| borrow.borrow_type == BorrowType::Shared);
            if let Some(first_shared) = first_shared {
                edges.extend(stack[first_shared..].iter().map(|borrow| borrow.edge));
            }
        }
        edges.iter().copied().collect()
    }

    pub fn above_first_shared(&self, offset: Size, len: Size) -> Vec<ProjectionIndex> {
        let mut edges = BTreeSet::new();
        for (_, stack) in self.ref_stack.iter(offset, len) {
            let first_shared = stack
                .iter()
                .position(|borrow| borrow.borrow_type == BorrowType::Shared);
            if let Some(first_shared) = first_shared {
                edges.extend(stack[..first_shared].iter().map(|borrow| borrow.edge));
            }
        }
        edges.iter().copied().collect()
    }

    pub fn remove_all_below(
        &mut self,
        offset: Size,
        len: Size,
        edge: ProjectionIndex,
    ) -> Vec<ProjectionIndex> {
        let mut edges = vec![];
        for (_, stack) in self.ref_stack.iter_mut(offset, len) {
            let index = stack.iter().position(|borrow| borrow.edge == edge);
            if let Some(index) = index {
                edges.extend(stack[index..].iter().map(|borrow| borrow.edge));
                stack.truncate(index);
            }
        }
        edges
    }

    pub fn can_read_through(&self, offset: Size, len: Size, edge: ProjectionIndex) -> bool {
        //FIXME: performance
        self.ref_stack
            .iter(offset, len)
            .all(|(_, stack)| stack.iter().find(|borrow| borrow.edge == edge).is_some())
    }

    pub fn can_write_through(&self, offset: Size, len: Size, edge: ProjectionIndex) -> bool {
        //FIXME: performance
        self.above_first_shared(offset, len).contains(&edge)
    }
}

define_index_type! {pub struct RunId = u32;}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunAndOffset(RunId, Size);

impl RunAndOffset {
    pub fn same_run(&self, other: &Self) -> bool {
        self.0 == other.0
    }

    pub fn offset(&self, offset: isize) -> Self {
        Self(self.0, Size::from_bytes(self.1.bytes() as isize + offset))
    }
}

#[derive(Clone)]
struct Allocation {
    /// The data stored in this allocation.
    runs: IndexVec<RunId, Run>,
    /// The alignment that was requested for this allocation.
    // align: Align,
    /// Whether this allocation is still live.
    live: bool,
}

impl Allocation {
    fn runs_and_sizes(&self) -> impl Iterator<Item = (RunId, Size)> + '_ {
        self.runs
            .iter_enumerated()
            .map(|(run_id, run)| (run_id, run.size()))
    }

    fn run(&self, run_and_offset: RunAndOffset) -> &Run {
        &self.runs[run_and_offset.0]
    }
}

pub struct AllocationBuilder {
    alloc_id: AllocId,
    runs: IndexVec<RunId, Run>,
}

impl AllocationBuilder {
    pub fn new_run(&mut self, size: Size) -> RunAndOffset {
        let run = Run::new_uninit(size);
        let run_id = self.runs.push(run);
        RunAndOffset(run_id, Size::ZERO)
    }

    pub fn alloc_id(&self) -> AllocId {
        self.alloc_id
    }

    fn build(self) -> Allocation {
        Allocation {
            runs: self.runs,
            live: true,
        }
    }
}

trait RangeExt: Sized {
    fn overlap(&self, other: &Self) -> bool;
    fn subtract(&self, other: &Self) -> [Option<Self>; 2];
}

impl RangeExt for Range<usize> {
    fn overlap(&self, other: &Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
    fn subtract(&self, other: &Self) -> [Option<Self>; 2] {
        assert!(self.overlap(other));
        let left = if self.start < other.start {
            Some(self.start..other.start)
        } else {
            None
        };
        let right = if other.end < self.end {
            Some(other.end..self.end)
        } else {
            None
        };
        [left, right]
    }
}

define_index_type! {pub struct AllocId = u32;}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunPointer {
    pub alloc_id: AllocId,
    pub run_and_offset: RunAndOffset,
    pub size: Size,
}

impl RunPointer {
    pub fn from_bytes_range(range: Range<usize>, alloc_id: AllocId, run: RunId) -> Self {
        RunPointer {
            alloc_id,
            run_and_offset: RunAndOffset(run, Size::from_bytes(range.start)),
            size: Size::from_bytes(range.count()),
        }
    }

    pub fn run(&self) -> RunId {
        self.run_and_offset.0
    }

    pub fn bytes_range(&self) -> Range<usize> {
        self.run_and_offset.1.bytes_usize()
            ..self.run_and_offset.1.bytes_usize() + self.size.bytes_usize()
    }

    pub fn overlap(&self, other: &Self) -> bool {
        if self.alloc_id != other.alloc_id {
            return false;
        }
        if !self.run_and_offset.same_run(&other.run_and_offset) {
            return false;
        }
        return self.bytes_range().overlap(&other.bytes_range());
    }
}

#[derive(Clone)]
pub struct BasicMemory {
    allocations: IndexVec<AllocId, Allocation>,

    // a lookup table to aid removal from borrow stacks
    // an edge may cover multiple runs, e.g. &(u32, u32),
    pointers: HashMap<ProjectionIndex, SmallVec<[RunPointer; 4]>>,
}

impl BasicMemory {
    const PTR_SIZE: Size = Size::from_bytes_const(mem::size_of::<*const ()>() as u64);

    pub fn new() -> Self {
        Self {
            allocations: IndexVec::new(),
            pointers: HashMap::new(),
        }
    }

    pub fn allocate_with_builder<F>(&mut self, build: F) -> AllocId
    where
        F: FnOnce(&mut AllocationBuilder),
    {
        let alloc_id = self.allocations.len_idx();
        let mut builder = AllocationBuilder {
            alloc_id,
            runs: IndexVec::new(),
        };
        build(&mut builder);
        self.allocations.push(builder.build())
    }

    pub fn deallocate(&mut self, alloc_id: AllocId) {
        self.allocations[alloc_id].live = false;
    }

    pub fn is_live(&self, alloc_id: AllocId) -> bool {
        self.allocations[alloc_id].live
    }

    pub fn bytes(&self, run_ptr: RunPointer) -> &[AbstractByte] {
        assert!(
            self.allocations[run_ptr.alloc_id].live,
            "can't access dead bytes"
        );
        &self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].bytes
            [run_ptr.bytes_range()]
    }

    pub fn fill(&mut self, run_ptr: RunPointer, val: AbstractByte) {
        self.bytes_mut(run_ptr).fill(val);
    }

    pub fn bytes_mut(&mut self, run_ptr: RunPointer) -> &mut [AbstractByte] {
        assert!(
            self.allocations[run_ptr.alloc_id].live,
            "can't access dead bytes"
        );
        &mut self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].bytes
            [run_ptr.bytes_range()]
    }

    pub fn copy(&mut self, dst: RunPointer, src: RunPointer) {
        assert_eq!(dst.size, src.size);
        let tmp = self.bytes(src).to_vec();
        self.bytes_mut(dst).copy_from_slice(&tmp)
    }

    /// Returns Size for types with guaranteed size.
    /// Composite types under the default layout has no guaranteed size,
    /// as the AM is free to insert arbitarily large paddings.
    pub fn ty_size(ty: TyId, tcx: &TyCtxt) -> Option<Size> {
        Some(match ty {
            TyCtxt::UNIT => Size::ZERO,
            TyCtxt::BOOL => Size::from_bytes(1),
            TyCtxt::CHAR => Size::from_bytes(4),
            TyCtxt::I8 | TyCtxt::U8 => Size::from_bits(8),
            TyCtxt::I16 | TyCtxt::U16 => Size::from_bits(16),
            TyCtxt::I32 | TyCtxt::U32 => Size::from_bits(32),
            TyCtxt::I64 | TyCtxt::U64 => Size::from_bits(64),
            TyCtxt::I128 | TyCtxt::U128 => Size::from_bits(128),
            TyCtxt::F32 => Size::from_bits(32),
            TyCtxt::F64 => Size::from_bits(64),
            TyCtxt::ISIZE | TyCtxt::USIZE => Self::PTR_SIZE,
            _ => match ty.kind(tcx) {
                TyKind::RawPtr(..) => Self::PTR_SIZE,
                TyKind::Ref(..) => Self::PTR_SIZE,
                TyKind::Array(ty, len) => {
                    return Self::ty_size(*ty, tcx)
                        .map(|elem| Size::from_bytes(elem.bytes_usize() * len))
                }
                _ => return None,
            },
        })
    }

    pub fn copy_ref(
        &mut self,
        new: ProjectionIndex,
        old: ProjectionIndex,
        // We should be able to get this information ourselves
        borrow_type: BorrowType,
    ) {
        assert_ne!(new, old);
        for run_ptr in &self.pointers[&old] {
            self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].add_borrow(
                run_ptr.run_and_offset.1,
                run_ptr.size,
                borrow_type,
                new,
            );
        }
        self.pointers.insert(new, self.pointers[&old].clone());
    }

    pub fn add_ref(&mut self, run_ptr: RunPointer, borrow_type: BorrowType, edge: ProjectionIndex) {
        self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].add_borrow(
            run_ptr.run_and_offset.1,
            run_ptr.size,
            borrow_type,
            edge,
        );
        self.pointers
            .entry(edge)
            .and_modify(|ptrs| ptrs.push(run_ptr))
            .or_insert(SmallVec::from([run_ptr].as_slice()));
    }

    /// Remove ref for all runs
    pub fn remove_ref(&mut self, edge: ProjectionIndex) {
        let run_ptrs = self.pointers.remove(&edge);
        if let Some(run_ptrs) = run_ptrs {
            for run_ptr in run_ptrs {
                self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].remove_borrow(
                    run_ptr.run_and_offset.1,
                    run_ptr.size,
                    edge,
                );
            }
        }
    }

    /// Remove a range (run_ptr) from the lookup table.
    fn derange(&mut self, edge: ProjectionIndex, run_ptr: RunPointer) {
        if let Some(all_run_ptrs) = self.pointers.get(&edge) {
            // Check if the run_ptr we removed overlaps with ones cached, then remove/split them as necessary
            let mut updated = SmallVec::new();
            for stored in all_run_ptrs {
                if stored.overlap(&run_ptr) {
                    let left_and_right = stored.bytes_range().subtract(&run_ptr.bytes_range());
                    for range in left_and_right {
                        if let Some(range) = range {
                            updated.push(RunPointer::from_bytes_range(
                                range,
                                stored.alloc_id,
                                stored.run(),
                            ));
                        }
                    }
                } else {
                    updated.push(*stored);
                }
            }

            let empty = updated.is_empty();
            if empty {
                self.pointers.remove(&edge);
            } else {
                self.pointers.insert(edge, updated);
            }
        }
    }

    /// Remove all refs including and below from a run. Returns a list of edges with no valid borrows
    /// left in any run after removal
    pub fn remove_ref_below(
        &mut self,
        edge: ProjectionIndex,
        run_ptr: RunPointer,
    ) -> Vec<ProjectionIndex> {
        let removed = self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0]
            .remove_all_below(run_ptr.run_and_offset.1, run_ptr.size, edge);

        let mut all_gone = vec![];
        for edge in removed {
            self.derange(edge, run_ptr);
            if !self.pointers.contains_key(&edge) {
                all_gone.push(edge);
            }
        }
        return all_gone;
    }

    /// Remove ref for a run ptr. Returns true if the ref is no longer present in any
    /// borrow stack
    pub fn remove_ref_run_ptr(&mut self, edge: ProjectionIndex, run_ptr: RunPointer) -> bool {
        self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].remove_borrow(
            run_ptr.run_and_offset.1,
            run_ptr.size,
            edge,
        );

        self.derange(edge, run_ptr);
        !self.pointers.contains_key(&edge)
    }

    pub fn first_shared(&self, run_ptr: RunPointer) -> Option<ProjectionIndex> {
        self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0]
            .first_shared(run_ptr.run_and_offset.1, run_ptr.size)
    }

    pub fn below_first_shared(&self, run_ptr: RunPointer) -> Vec<ProjectionIndex> {
        self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0]
            .below_first_shared(run_ptr.run_and_offset.1, run_ptr.size)
    }

    pub fn can_read_through(&self, run_ptr: RunPointer, edge: ProjectionIndex) -> bool {
        self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].can_read_through(
            run_ptr.run_and_offset.1,
            run_ptr.size,
            edge,
        )
    }

    pub fn can_write_through(&self, run_ptr: RunPointer, edge: ProjectionIndex) -> bool {
        self.allocations[run_ptr.alloc_id].runs[run_ptr.run_and_offset.0].can_write_through(
            run_ptr.run_and_offset.1,
            run_ptr.size,
            edge,
        )
    }
}
