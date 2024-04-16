//! Global machine state as well as implementation of the interpreter engine
//! `Machine` trait.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::fmt;
use std::path::Path;
use std::process;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

use rustc_data_structures::fx::{FxHashMap, FxHashSet};
#[allow(unused)]
use rustc_data_structures::static_assert_size;
use rustc_middle::{
    mir,
    query::TyCtxtAt,
    ty::{
        self,
        layout::{LayoutCx, LayoutError, LayoutOf, TyAndLayout},
        Instance, Ty, TyCtxt,
    },
};
use rustc_span::def_id::{CrateNum, DefId};
use rustc_span::{Span, SpanData, Symbol};
use rustc_target::abi::{Align, Size};
use rustc_target::spec::abi::Abi;

use crate::{
    concurrency::{data_race, weak_memory},
    shims::unix::FdTable,
    *,
};

use self::concurrency::data_race::NaReadType;
use self::concurrency::data_race::NaWriteType;

/// First real-time signal.
/// `signal(7)` says this must be between 32 and 64 and specifies 34 or 35
/// as typical values.
pub const SIGRTMIN: i32 = 34;

/// Last real-time signal.
/// `signal(7)` says it must be between 32 and 64 and specifies
/// `SIGRTMAX` - `SIGRTMIN` >= 8 (which is the value of `_POSIX_RTSIG_MAX`)
pub const SIGRTMAX: i32 = 42;

/// Each const has multiple addresses, but only this many. Since const allocations are never
/// deallocated, choosing a new [`AllocId`] and thus base address for each evaluation would
/// produce unbounded memory usage.
const ADDRS_PER_CONST: usize = 16;

/// Extra data stored with each stack frame
pub struct FrameExtra<'tcx> {
    /// Extra data for the Borrow Tracker.
    pub borrow_tracker: Option<borrow_tracker::FrameState>,

    /// If this is Some(), then this is a special "catch unwind" frame (the frame of `try_fn`
    /// called by `try`). When this frame is popped during unwinding a panic,
    /// we stop unwinding, use the `CatchUnwindData` to handle catching.
    pub catch_unwind: Option<CatchUnwindData<'tcx>>,

    /// If `measureme` profiling is enabled, holds timing information
    /// for the start of this frame. When we finish executing this frame,
    /// we use this to register a completed event with `measureme`.
    pub timing: Option<measureme::DetachedTiming>,

    /// Indicates whether a `Frame` is part of a workspace-local crate and is also not
    /// `#[track_caller]`. We compute this once on creation and store the result, as an
    /// optimization.
    /// This is used by `MiriMachine::current_span` and `MiriMachine::caller_span`
    pub is_user_relevant: bool,

    /// We have a cache for the mapping from [`mir::Const`] to resulting [`AllocId`].
    /// However, we don't want all frames to always get the same result, so we insert
    /// an additional bit of "salt" into the cache key. This salt is fixed per-frame
    /// so that within a call, a const will have a stable address.
    salt: usize,
}

impl<'tcx> std::fmt::Debug for FrameExtra<'tcx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Omitting `timing`, it does not support `Debug`.
        let FrameExtra { borrow_tracker, catch_unwind, timing: _, is_user_relevant: _, salt: _ } =
            self;
        f.debug_struct("FrameData")
            .field("borrow_tracker", borrow_tracker)
            .field("catch_unwind", catch_unwind)
            .finish()
    }
}

impl VisitProvenance for FrameExtra<'_> {
    fn visit_provenance(&self, visit: &mut VisitWith<'_>) {
        let FrameExtra { catch_unwind, borrow_tracker, timing: _, is_user_relevant: _, salt: _ } =
            self;

        catch_unwind.visit_provenance(visit);
        borrow_tracker.visit_provenance(visit);
    }
}

/// Extra memory kinds
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MiriMemoryKind {
    /// `__rust_alloc` memory.
    Rust,
    /// `miri_alloc` memory.
    Miri,
    /// `malloc` memory.
    C,
    /// Windows `HeapAlloc` memory.
    WinHeap,
    /// Windows "local" memory (to be freed with `LocalFree`)
    WinLocal,
    /// Memory for args, errno, and other parts of the machine-managed environment.
    /// This memory may leak.
    Machine,
    /// Memory allocated by the runtime (e.g. env vars). Separate from `Machine`
    /// because we clean it up and leak-check it.
    Runtime,
    /// Globals copied from `tcx`.
    /// This memory may leak.
    Global,
    /// Memory for extern statics.
    /// This memory may leak.
    ExternStatic,
    /// Memory for thread-local statics.
    /// This memory may leak.
    Tls,
    /// Memory mapped directly by the program
    Mmap,
}

impl From<MiriMemoryKind> for MemoryKind {
    #[inline(always)]
    fn from(kind: MiriMemoryKind) -> MemoryKind {
        MemoryKind::Machine(kind)
    }
}

impl MayLeak for MiriMemoryKind {
    #[inline(always)]
    fn may_leak(self) -> bool {
        use self::MiriMemoryKind::*;
        match self {
            Rust | Miri | C | WinHeap | WinLocal | Runtime => false,
            Machine | Global | ExternStatic | Tls | Mmap => true,
        }
    }
}

impl MiriMemoryKind {
    /// Whether we have a useful allocation span for an allocation of this kind.
    fn should_save_allocation_span(self) -> bool {
        use self::MiriMemoryKind::*;
        match self {
            // Heap allocations are fine since the `Allocation` is created immediately.
            Rust | Miri | C | WinHeap | WinLocal | Mmap => true,
            // Everything else is unclear, let's not show potentially confusing spans.
            Machine | Global | ExternStatic | Tls | Runtime => false,
        }
    }
}

impl fmt::Display for MiriMemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use self::MiriMemoryKind::*;
        match self {
            Rust => write!(f, "Rust heap"),
            Miri => write!(f, "Miri bare-metal heap"),
            C => write!(f, "C heap"),
            WinHeap => write!(f, "Windows heap"),
            WinLocal => write!(f, "Windows local memory"),
            Machine => write!(f, "machine-managed memory"),
            Runtime => write!(f, "language runtime memory"),
            Global => write!(f, "global (static or const)"),
            ExternStatic => write!(f, "extern static"),
            Tls => write!(f, "thread-local static"),
            Mmap => write!(f, "mmap"),
        }
    }
}

pub type MemoryKind = interpret::MemoryKind<MiriMemoryKind>;

/// Pointer provenance.
#[derive(Clone, Copy)]
pub enum Provenance {
    /// For pointers with concrete provenance. we exactly know which allocation they are attached to
    /// and what their borrow tag is.
    Concrete {
        alloc_id: AllocId,
        /// Borrow Tracker tag.
        tag: BorTag,
    },
    /// Pointers with wildcard provenance are created on int-to-ptr casts. According to the
    /// specification, we should at that point angelically "guess" a provenance that will make all
    /// future uses of this pointer work, if at all possible. Of course such a semantics cannot be
    /// actually implemented in Miri. So instead, we approximate this, erroring on the side of
    /// accepting too much code rather than rejecting correct code: a pointer with wildcard
    /// provenance "acts like" any previously exposed pointer. Each time it is used, we check
    /// whether *some* exposed pointer could have done what we want to do, and if the answer is yes
    /// then we allow the access. This allows too much code in two ways:
    /// - The same wildcard pointer can "take the role" of multiple different exposed pointers on
    ///   subsequenct memory accesses.
    /// - In the aliasing model, we don't just have to know the borrow tag of the pointer used for
    ///   the access, we also have to update the aliasing state -- and that update can be very
    ///   different depending on which borrow tag we pick! Stacked Borrows has support for this by
    ///   switching to a stack that is only approximately known, i.e. we overapproximate the effect
    ///   of using *any* exposed pointer for this access, and only keep information about the borrow
    ///   stack that would be true with all possible choices.
    Wildcard,
}

// This needs to be `Eq`+`Hash` because the `Machine` trait needs that because validity checking
// *might* be recursive and then it has to track which places have already been visited.
// However, comparing provenance is meaningless, since `Wildcard` might be any provenance -- and of
// course we don't actually do recursive checking.
// We could change `RefTracking` to strip provenance for its `seen` set but that type is generic so that is quite annoying.
// Instead owe add the required instances but make them panic.
impl PartialEq for Provenance {
    fn eq(&self, _other: &Self) -> bool {
        panic!("Provenance must not be compared")
    }
}
impl Eq for Provenance {}
impl std::hash::Hash for Provenance {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {
        panic!("Provenance must not be hashed")
    }
}

/// The "extra" information a pointer has over a regular AllocId.
#[derive(Copy, Clone, PartialEq)]
pub enum ProvenanceExtra {
    Concrete(BorTag),
    Wildcard,
}

#[cfg(target_pointer_width = "64")]
static_assert_size!(Pointer<Provenance>, 24);
// FIXME: this would with in 24bytes but layout optimizations are not smart enough
// #[cfg(target_pointer_width = "64")]
//static_assert_size!(Pointer<Option<Provenance>>, 24);
#[cfg(target_pointer_width = "64")]
static_assert_size!(Scalar<Provenance>, 32);

impl fmt::Debug for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provenance::Concrete { alloc_id, tag } => {
                // Forward `alternate` flag to `alloc_id` printing.
                if f.alternate() {
                    write!(f, "[{alloc_id:#?}]")?;
                } else {
                    write!(f, "[{alloc_id:?}]")?;
                }
                // Print Borrow Tracker tag.
                write!(f, "{tag:?}")?;
            }
            Provenance::Wildcard => {
                write!(f, "[wildcard]")?;
            }
        }
        Ok(())
    }
}

impl interpret::Provenance for Provenance {
    /// We use absolute addresses in the `offset` of a `Pointer<Provenance>`.
    const OFFSET_IS_ADDR: bool = true;

    fn get_alloc_id(self) -> Option<AllocId> {
        match self {
            Provenance::Concrete { alloc_id, .. } => Some(alloc_id),
            Provenance::Wildcard => None,
        }
    }

    fn fmt(ptr: &Pointer<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prov, addr) = ptr.into_parts(); // address is absolute
        write!(f, "{:#x}", addr.bytes())?;
        if f.alternate() {
            write!(f, "{prov:#?}")?;
        } else {
            write!(f, "{prov:?}")?;
        }
        Ok(())
    }

    fn join(left: Option<Self>, right: Option<Self>) -> Option<Self> {
        match (left, right) {
            // If both are the *same* concrete tag, that is the result.
            (
                Some(Provenance::Concrete { alloc_id: left_alloc, tag: left_tag }),
                Some(Provenance::Concrete { alloc_id: right_alloc, tag: right_tag }),
            ) if left_alloc == right_alloc && left_tag == right_tag => left,
            // If one side is a wildcard, the best possible outcome is that it is equal to the other
            // one, and we use that.
            (Some(Provenance::Wildcard), o) | (o, Some(Provenance::Wildcard)) => o,
            // Otherwise, fall back to `None`.
            _ => None,
        }
    }
}

impl fmt::Debug for ProvenanceExtra {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProvenanceExtra::Concrete(pid) => write!(f, "{pid:?}"),
            ProvenanceExtra::Wildcard => write!(f, "<wildcard>"),
        }
    }
}

impl ProvenanceExtra {
    pub fn and_then<T>(self, f: impl FnOnce(BorTag) -> Option<T>) -> Option<T> {
        match self {
            ProvenanceExtra::Concrete(pid) => f(pid),
            ProvenanceExtra::Wildcard => None,
        }
    }
}

/// Extra per-allocation data
#[derive(Debug, Clone)]
pub struct AllocExtra<'tcx> {
    /// Global state of the borrow tracker, if enabled.
    pub borrow_tracker: Option<borrow_tracker::AllocState>,
    /// Data race detection via the use of a vector-clock.
    /// This is only added if it is enabled.
    pub data_race: Option<data_race::AllocState>,
    /// Weak memory emulation via the use of store buffers.
    /// This is only added if it is enabled.
    pub weak_memory: Option<weak_memory::AllocState>,
    /// A backtrace to where this allocation was allocated.
    /// As this is recorded for leak reports, it only exists
    /// if this allocation is leakable. The backtrace is not
    /// pruned yet; that should be done before printing it.
    pub backtrace: Option<Vec<FrameInfo<'tcx>>>,
}

impl VisitProvenance for AllocExtra<'_> {
    fn visit_provenance(&self, visit: &mut VisitWith<'_>) {
        let AllocExtra { borrow_tracker, data_race, weak_memory, backtrace: _ } = self;

        borrow_tracker.visit_provenance(visit);
        data_race.visit_provenance(visit);
        weak_memory.visit_provenance(visit);
    }
}

/// Precomputed layouts of primitive types
pub struct PrimitiveLayouts<'tcx> {
    pub unit: TyAndLayout<'tcx>,
    pub i8: TyAndLayout<'tcx>,
    pub i16: TyAndLayout<'tcx>,
    pub i32: TyAndLayout<'tcx>,
    pub i64: TyAndLayout<'tcx>,
    pub i128: TyAndLayout<'tcx>,
    pub isize: TyAndLayout<'tcx>,
    pub u8: TyAndLayout<'tcx>,
    pub u16: TyAndLayout<'tcx>,
    pub u32: TyAndLayout<'tcx>,
    pub u64: TyAndLayout<'tcx>,
    pub u128: TyAndLayout<'tcx>,
    pub usize: TyAndLayout<'tcx>,
    pub bool: TyAndLayout<'tcx>,
    pub mut_raw_ptr: TyAndLayout<'tcx>,   // *mut ()
    pub const_raw_ptr: TyAndLayout<'tcx>, // *const ()
}

impl<'mir, 'tcx: 'mir> PrimitiveLayouts<'tcx> {
    fn new(layout_cx: LayoutCx<'tcx, TyCtxt<'tcx>>) -> Result<Self, &'tcx LayoutError<'tcx>> {
        let tcx = layout_cx.tcx;
        let mut_raw_ptr = Ty::new_mut_ptr(tcx, tcx.types.unit);
        let const_raw_ptr = Ty::new_imm_ptr(tcx, tcx.types.unit);
        Ok(Self {
            unit: layout_cx.layout_of(Ty::new_unit(tcx))?,
            i8: layout_cx.layout_of(tcx.types.i8)?,
            i16: layout_cx.layout_of(tcx.types.i16)?,
            i32: layout_cx.layout_of(tcx.types.i32)?,
            i64: layout_cx.layout_of(tcx.types.i64)?,
            i128: layout_cx.layout_of(tcx.types.i128)?,
            isize: layout_cx.layout_of(tcx.types.isize)?,
            u8: layout_cx.layout_of(tcx.types.u8)?,
            u16: layout_cx.layout_of(tcx.types.u16)?,
            u32: layout_cx.layout_of(tcx.types.u32)?,
            u64: layout_cx.layout_of(tcx.types.u64)?,
            u128: layout_cx.layout_of(tcx.types.u128)?,
            usize: layout_cx.layout_of(tcx.types.usize)?,
            bool: layout_cx.layout_of(tcx.types.bool)?,
            mut_raw_ptr: layout_cx.layout_of(mut_raw_ptr)?,
            const_raw_ptr: layout_cx.layout_of(const_raw_ptr)?,
        })
    }

    pub fn uint(&self, size: Size) -> Option<TyAndLayout<'tcx>> {
        match size.bits() {
            8 => Some(self.u8),
            16 => Some(self.u16),
            32 => Some(self.u32),
            64 => Some(self.u64),
            128 => Some(self.u128),
            _ => None,
        }
    }

    pub fn int(&self, size: Size) -> Option<TyAndLayout<'tcx>> {
        match size.bits() {
            8 => Some(self.i8),
            16 => Some(self.i16),
            32 => Some(self.i32),
            64 => Some(self.i64),
            128 => Some(self.i128),
            _ => None,
        }
    }
}

/// The machine itself.
///
/// If you add anything here that stores machine values, remember to update
/// `visit_all_machine_values`!
pub struct MiriMachine<'mir, 'tcx> {
    // We carry a copy of the global `TyCtxt` for convenience, so methods taking just `&Evaluator` have `tcx` access.
    pub tcx: TyCtxt<'tcx>,

    /// Global data for borrow tracking.
    pub borrow_tracker: Option<borrow_tracker::GlobalState>,

    /// Data race detector global data.
    pub data_race: Option<data_race::GlobalState>,

    /// Ptr-int-cast module global data.
    pub alloc_addresses: alloc_addresses::GlobalState,

    /// Environment variables set by `setenv`.
    /// Miri does not expose env vars from the host to the emulated program.
    pub(crate) env_vars: EnvVars<'tcx>,

    /// Return place of the main function.
    pub(crate) main_fn_ret_place: Option<MPlaceTy<'tcx, Provenance>>,

    /// Program arguments (`Option` because we can only initialize them after creating the ecx).
    /// These are *pointers* to argc/argv because macOS.
    /// We also need the full command line as one string because of Windows.
    pub(crate) argc: Option<Pointer<Option<Provenance>>>,
    pub(crate) argv: Option<Pointer<Option<Provenance>>>,
    pub(crate) cmd_line: Option<Pointer<Option<Provenance>>>,

    /// TLS state.
    pub(crate) tls: TlsData<'tcx>,

    /// What should Miri do when an op requires communicating with the host,
    /// such as accessing host env vars, random number generation, and
    /// file system access.
    pub(crate) isolated_op: IsolatedOp,

    /// Whether to enforce the validity invariant.
    pub(crate) validate: bool,

    /// The table of file descriptors.
    pub(crate) fds: shims::unix::FdTable,
    /// The table of directory descriptors.
    pub(crate) dirs: shims::unix::DirTable,

    /// This machine's monotone clock.
    pub(crate) clock: Clock,

    /// The set of threads.
    pub(crate) threads: ThreadManager<'mir, 'tcx>,

    /// Precomputed `TyLayout`s for primitive data types that are commonly used inside Miri.
    pub(crate) layouts: PrimitiveLayouts<'tcx>,

    /// Allocations that are considered roots of static memory (that may leak).
    pub(crate) static_roots: Vec<AllocId>,

    /// The `measureme` profiler used to record timing information about
    /// the emulated program.
    profiler: Option<measureme::Profiler>,
    /// Used with `profiler` to cache the `StringId`s for event names
    /// used with `measureme`.
    string_cache: FxHashMap<String, measureme::StringId>,

    /// Cache of `Instance` exported under the given `Symbol` name.
    /// `None` means no `Instance` exported under the given name is found.
    pub(crate) exported_symbols_cache: FxHashMap<Symbol, Option<Instance<'tcx>>>,

    /// Whether to raise a panic in the context of the evaluated process when unsupported
    /// functionality is encountered. If `false`, an error is propagated in the Miri application context
    /// instead (default behavior)
    pub(crate) panic_on_unsupported: bool,

    /// Equivalent setting as RUST_BACKTRACE on encountering an error.
    pub(crate) backtrace_style: BacktraceStyle,

    /// Crates which are considered local for the purposes of error reporting.
    pub(crate) local_crates: Vec<CrateNum>,

    /// Mapping extern static names to their base pointer.
    extern_statics: FxHashMap<Symbol, Pointer<Provenance>>,

    /// The random number generator used for resolving non-determinism.
    /// Needs to be queried by ptr_to_int, hence needs interior mutability.
    pub(crate) rng: RefCell<StdRng>,

    /// The allocation IDs to report when they are being allocated
    /// (helps for debugging memory leaks and use after free bugs).
    tracked_alloc_ids: FxHashSet<AllocId>,
    /// For the tracked alloc ids, also report read/write accesses.
    track_alloc_accesses: bool,

    /// Controls whether alignment of memory accesses is being checked.
    pub(crate) check_alignment: AlignmentCheck,

    /// Failure rate of compare_exchange_weak, between 0.0 and 1.0
    pub(crate) cmpxchg_weak_failure_rate: f64,

    /// Corresponds to -Zmiri-mute-stdout-stderr and doesn't write the output but acts as if it succeeded.
    pub(crate) mute_stdout_stderr: bool,

    /// Whether weak memory emulation is enabled
    pub(crate) weak_memory: bool,

    /// The probability of the active thread being preempted at the end of each basic block.
    pub(crate) preemption_rate: f64,

    /// If `Some`, we will report the current stack every N basic blocks.
    pub(crate) report_progress: Option<u32>,
    // The total number of blocks that have been executed.
    pub(crate) basic_block_count: u64,

    /// Handle of the optional shared object file for external functions.
    #[cfg(target_os = "linux")]
    pub external_so_lib: Option<(libloading::Library, std::path::PathBuf)>,
    #[cfg(not(target_os = "linux"))]
    pub external_so_lib: Option<!>,

    /// Run a garbage collector for BorTags every N basic blocks.
    pub(crate) gc_interval: u32,
    /// The number of blocks that passed since the last BorTag GC pass.
    pub(crate) since_gc: u32,

    /// The number of CPUs to be reported by miri.
    pub(crate) num_cpus: u32,

    /// Determines Miri's page size and associated values
    pub(crate) page_size: u64,
    pub(crate) stack_addr: u64,
    pub(crate) stack_size: u64,

    /// Whether to collect a backtrace when each allocation is created, just in case it leaks.
    pub(crate) collect_leak_backtraces: bool,

    /// The spans we will use to report where an allocation was created and deallocated in
    /// diagnostics.
    pub(crate) allocation_spans: RefCell<FxHashMap<AllocId, (Span, Option<Span>)>>,

    /// Maps MIR consts to their evaluated result. We combine the const with a "salt" (`usize`)
    /// that is fixed per stack frame; this lets us have sometimes different results for the
    /// same const while ensuring consistent results within a single call.
    const_cache: RefCell<FxHashMap<(mir::Const<'tcx>, usize), OpTy<'tcx, Provenance>>>,

    /// For each allocation, an offset inside that allocation that was deemed aligned even for
    /// symbolic alignment checks. This cannot be stored in `AllocExtra` since it needs to be
    /// tracked for vtables and function allocations as well as regular allocations.
    ///
    /// Invariant: the promised alignment will never be less than the native alignment of the
    /// allocation.
    pub(crate) symbolic_alignment: RefCell<FxHashMap<AllocId, (Size, Align)>>,
}

impl<'mir, 'tcx> MiriMachine<'mir, 'tcx> {
    pub(crate) fn new(config: &MiriConfig, layout_cx: LayoutCx<'tcx, TyCtxt<'tcx>>) -> Self {
        let tcx = layout_cx.tcx;
        let local_crates = helpers::get_local_crates(tcx);
        let layouts =
            PrimitiveLayouts::new(layout_cx).expect("Couldn't get layouts of primitive types");
        let profiler = config.measureme_out.as_ref().map(|out| {
            let crate_name = layout_cx
                .tcx
                .sess
                .opts
                .crate_name
                .clone()
                .unwrap_or_else(|| "unknown-crate".to_string());
            let pid = process::id();
            // We adopt the same naming scheme for the profiler output that rustc uses. In rustc,
            // the PID is padded so that the nondeterministic value of the PID does not spread
            // nondeterminism to the allocator. In Miri we are not aiming for such performance
            // control, we just pad for consistency with rustc.
            let filename = format!("{crate_name}-{pid:07}");
            let path = Path::new(out).join(filename);
            measureme::Profiler::new(path).expect("Couldn't create `measureme` profiler")
        });
        let rng = StdRng::seed_from_u64(config.seed.unwrap_or(0));
        let borrow_tracker = config.borrow_tracker.map(|bt| bt.instantiate_global_state(config));
        let data_race = config.data_race_detector.then(|| data_race::GlobalState::new(config));
        // Determine page size, stack address, and stack size.
        // These values are mostly meaningless, but the stack address is also where we start
        // allocating physical integer addresses for all allocations.
        let page_size = if let Some(page_size) = config.page_size {
            page_size
        } else {
            let target = &tcx.sess.target;
            match target.arch.as_ref() {
                "wasm32" | "wasm64" => 64 * 1024, // https://webassembly.github.io/spec/core/exec/runtime.html#memory-instances
                "aarch64" => {
                    if target.options.vendor.as_ref() == "apple" {
                        // No "definitive" source, but see:
                        // https://www.wwdcnotes.com/notes/wwdc20/10214/
                        // https://github.com/ziglang/zig/issues/11308 etc.
                        16 * 1024
                    } else {
                        4 * 1024
                    }
                }
                _ => 4 * 1024,
            }
        };
        // On 16bit targets, 32 pages is more than the entire address space!
        let stack_addr = if tcx.pointer_size().bits() < 32 { page_size } else { page_size * 32 };
        let stack_size =
            if tcx.pointer_size().bits() < 32 { page_size * 4 } else { page_size * 16 };
        MiriMachine {
            tcx,
            borrow_tracker,
            data_race,
            alloc_addresses: RefCell::new(alloc_addresses::GlobalStateInner::new(config, stack_addr)),
            // `env_vars` depends on a full interpreter so we cannot properly initialize it yet.
            env_vars: EnvVars::default(),
            main_fn_ret_place: None,
            argc: None,
            argv: None,
            cmd_line: None,
            tls: TlsData::default(),
            isolated_op: config.isolated_op,
            validate: config.validate,
            fds: FdTable::new(config.mute_stdout_stderr),
            dirs: Default::default(),
            layouts,
            threads: ThreadManager::default(),
            static_roots: Vec::new(),
            profiler,
            string_cache: Default::default(),
            exported_symbols_cache: FxHashMap::default(),
            panic_on_unsupported: config.panic_on_unsupported,
            backtrace_style: config.backtrace_style,
            local_crates,
            extern_statics: FxHashMap::default(),
            rng: RefCell::new(rng),
            tracked_alloc_ids: config.tracked_alloc_ids.clone(),
            track_alloc_accesses: config.track_alloc_accesses,
            check_alignment: config.check_alignment,
            cmpxchg_weak_failure_rate: config.cmpxchg_weak_failure_rate,
            mute_stdout_stderr: config.mute_stdout_stderr,
            weak_memory: config.weak_memory_emulation,
            preemption_rate: config.preemption_rate,
            report_progress: config.report_progress,
            basic_block_count: 0,
            clock: Clock::new(config.isolated_op == IsolatedOp::Allow),
            #[cfg(target_os = "linux")]
            external_so_lib: config.external_so_file.as_ref().map(|lib_file_path| {
                let target_triple = layout_cx.tcx.sess.opts.target_triple.triple();
                // Check if host target == the session target.
                if env!("TARGET") != target_triple {
                    panic!(
                        "calling external C functions in linked .so file requires host and target to be the same: host={}, target={}",
                        env!("TARGET"),
                        target_triple,
                    );
                }
                // Note: it is the user's responsibility to provide a correct SO file.
                // WATCH OUT: If an invalid/incorrect SO file is specified, this can cause
                // undefined behaviour in Miri itself!
                (
                    unsafe {
                        libloading::Library::new(lib_file_path)
                            .expect("failed to read specified extern shared object file")
                    },
                    lib_file_path.clone(),
                )
            }),
            #[cfg(not(target_os = "linux"))]
            external_so_lib: config.external_so_file.as_ref().map(|_| {
                panic!("loading external .so files is only supported on Linux")
            }),
            gc_interval: config.gc_interval,
            since_gc: 0,
            num_cpus: config.num_cpus,
            page_size,
            stack_addr,
            stack_size,
            collect_leak_backtraces: config.collect_leak_backtraces,
            allocation_spans: RefCell::new(FxHashMap::default()),
            const_cache: RefCell::new(FxHashMap::default()),
            symbolic_alignment: RefCell::new(FxHashMap::default()),
        }
    }

    pub(crate) fn late_init(
        this: &mut MiriInterpCx<'mir, 'tcx>,
        config: &MiriConfig,
        on_main_stack_empty: StackEmptyCallback<'mir, 'tcx>,
    ) -> InterpResult<'tcx> {
        EnvVars::init(this, config)?;
        MiriMachine::init_extern_statics(this)?;
        ThreadManager::init(this, on_main_stack_empty);
        Ok(())
    }

    pub(crate) fn add_extern_static(
        this: &mut MiriInterpCx<'mir, 'tcx>,
        name: &str,
        ptr: Pointer<Option<Provenance>>,
    ) {
        // This got just allocated, so there definitely is a pointer here.
        let ptr = ptr.into_pointer_or_addr().unwrap();
        this.machine.extern_statics.try_insert(Symbol::intern(name), ptr).unwrap();
    }

    pub(crate) fn communicate(&self) -> bool {
        self.isolated_op == IsolatedOp::Allow
    }

    /// Check whether the stack frame that this `FrameInfo` refers to is part of a local crate.
    pub(crate) fn is_local(&self, frame: &FrameInfo<'_>) -> bool {
        let def_id = frame.instance.def_id();
        def_id.is_local() || self.local_crates.contains(&def_id.krate)
    }

    /// Called when the interpreter is going to shut down abnormally, such as due to a Ctrl-C.
    pub(crate) fn handle_abnormal_termination(&mut self) {
        // All strings in the profile data are stored in a single string table which is not
        // written to disk until the profiler is dropped. If the interpreter exits without dropping
        // the profiler, it is not possible to interpret the profile data and all measureme tools
        // will panic when given the file.
        drop(self.profiler.take());
    }

    pub(crate) fn page_align(&self) -> Align {
        Align::from_bytes(self.page_size).unwrap()
    }

    pub(crate) fn allocated_span(&self, alloc_id: AllocId) -> Option<SpanData> {
        self.allocation_spans
            .borrow()
            .get(&alloc_id)
            .map(|(allocated, _deallocated)| allocated.data())
    }

    pub(crate) fn deallocated_span(&self, alloc_id: AllocId) -> Option<SpanData> {
        self.allocation_spans
            .borrow()
            .get(&alloc_id)
            .and_then(|(_allocated, deallocated)| *deallocated)
            .map(Span::data)
    }
}

impl VisitProvenance for MiriMachine<'_, '_> {
    fn visit_provenance(&self, visit: &mut VisitWith<'_>) {
        #[rustfmt::skip]
        let MiriMachine {
            threads,
            tls,
            env_vars,
            main_fn_ret_place,
            argc,
            argv,
            cmd_line,
            extern_statics,
            dirs,
            borrow_tracker,
            data_race,
            alloc_addresses,
            fds,
            tcx: _,
            isolated_op: _,
            validate: _,
            clock: _,
            layouts: _,
            static_roots: _,
            profiler: _,
            string_cache: _,
            exported_symbols_cache: _,
            panic_on_unsupported: _,
            backtrace_style: _,
            local_crates: _,
            rng: _,
            tracked_alloc_ids: _,
            track_alloc_accesses: _,
            check_alignment: _,
            cmpxchg_weak_failure_rate: _,
            mute_stdout_stderr: _,
            weak_memory: _,
            preemption_rate: _,
            report_progress: _,
            basic_block_count: _,
            external_so_lib: _,
            gc_interval: _,
            since_gc: _,
            num_cpus: _,
            page_size: _,
            stack_addr: _,
            stack_size: _,
            collect_leak_backtraces: _,
            allocation_spans: _,
            const_cache: _,
            symbolic_alignment: _,
        } = self;

        threads.visit_provenance(visit);
        tls.visit_provenance(visit);
        env_vars.visit_provenance(visit);
        dirs.visit_provenance(visit);
        fds.visit_provenance(visit);
        data_race.visit_provenance(visit);
        borrow_tracker.visit_provenance(visit);
        alloc_addresses.visit_provenance(visit);
        main_fn_ret_place.visit_provenance(visit);
        argc.visit_provenance(visit);
        argv.visit_provenance(visit);
        cmd_line.visit_provenance(visit);
        for ptr in extern_statics.values() {
            ptr.visit_provenance(visit);
        }
    }
}

/// A rustc InterpCx for Miri.
pub type MiriInterpCx<'mir, 'tcx> = InterpCx<'mir, 'tcx, MiriMachine<'mir, 'tcx>>;

/// A little trait that's useful to be inherited by extension traits.
pub trait MiriInterpCxExt<'mir, 'tcx> {
    fn eval_context_ref<'a>(&'a self) -> &'a MiriInterpCx<'mir, 'tcx>;
    fn eval_context_mut<'a>(&'a mut self) -> &'a mut MiriInterpCx<'mir, 'tcx>;
}
impl<'mir, 'tcx> MiriInterpCxExt<'mir, 'tcx> for MiriInterpCx<'mir, 'tcx> {
    #[inline(always)]
    fn eval_context_ref(&self) -> &MiriInterpCx<'mir, 'tcx> {
        self
    }
    #[inline(always)]
    fn eval_context_mut(&mut self) -> &mut MiriInterpCx<'mir, 'tcx> {
        self
    }
}

/// Machine hook implementations.
impl<'mir, 'tcx> Machine<'mir, 'tcx> for MiriMachine<'mir, 'tcx> {
    type MemoryKind = MiriMemoryKind;
    type ExtraFnVal = DynSym;

    type FrameExtra = FrameExtra<'tcx>;
    type AllocExtra = AllocExtra<'tcx>;

    type Provenance = Provenance;
    type ProvenanceExtra = ProvenanceExtra;
    type Bytes = Box<[u8]>;

    type MemoryMap =
        MonoHashMap<AllocId, (MemoryKind, Allocation<Provenance, Self::AllocExtra, Self::Bytes>)>;

    const GLOBAL_KIND: Option<MiriMemoryKind> = Some(MiriMemoryKind::Global);

    const PANIC_ON_ALLOC_FAIL: bool = false;

    #[inline(always)]
    fn enforce_alignment(ecx: &MiriInterpCx<'mir, 'tcx>) -> bool {
        ecx.machine.check_alignment != AlignmentCheck::None
    }

    #[inline(always)]
    fn alignment_check(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        alloc_id: AllocId,
        alloc_align: Align,
        alloc_kind: AllocKind,
        offset: Size,
        align: Align,
    ) -> Option<Misalignment> {
        if ecx.machine.check_alignment != AlignmentCheck::Symbolic {
            // Just use the built-in check.
            return None;
        }
        if alloc_kind != AllocKind::LiveData {
            // Can't have any extra info here.
            return None;
        }
        // Let's see which alignment we have been promised for this allocation.
        let (promised_offset, promised_align) = ecx
            .machine
            .symbolic_alignment
            .borrow()
            .get(&alloc_id)
            .copied()
            .unwrap_or((Size::ZERO, alloc_align));
        if promised_align < align {
            // Definitely not enough.
            Some(Misalignment { has: promised_align, required: align })
        } else {
            // What's the offset between us and the promised alignment?
            let distance = offset.bytes().wrapping_sub(promised_offset.bytes());
            // That must also be aligned.
            if distance % align.bytes() == 0 {
                // All looking good!
                None
            } else {
                // The biggest power of two through which `distance` is divisible.
                let distance_pow2 = 1 << distance.trailing_zeros();
                Some(Misalignment {
                    has: Align::from_bytes(distance_pow2).unwrap(),
                    required: align,
                })
            }
        }
    }

    #[inline(always)]
    fn enforce_validity(ecx: &MiriInterpCx<'mir, 'tcx>, _layout: TyAndLayout<'tcx>) -> bool {
        ecx.machine.validate
    }

    #[inline(always)]
    fn enforce_abi(_ecx: &MiriInterpCx<'mir, 'tcx>) -> bool {
        true
    }

    #[inline(always)]
    fn ignore_optional_overflow_checks(ecx: &MiriInterpCx<'mir, 'tcx>) -> bool {
        !ecx.tcx.sess.overflow_checks()
    }

    #[inline(always)]
    fn find_mir_or_eval_fn(
        ecx: &mut MiriInterpCx<'mir, 'tcx>,
        instance: ty::Instance<'tcx>,
        abi: Abi,
        args: &[FnArg<'tcx, Provenance>],
        dest: &MPlaceTy<'tcx, Provenance>,
        ret: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx, Option<(&'mir mir::Body<'tcx>, ty::Instance<'tcx>)>> {
        // For foreign items, try to see if we can emulate them.
        if ecx.tcx.is_foreign_item(instance.def_id()) {
            // An external function call that does not have a MIR body. We either find MIR elsewhere
            // or emulate its effect.
            // This will be Ok(None) if we're emulating the intrinsic entirely within Miri (no need
            // to run extra MIR), and Ok(Some(body)) if we found MIR to run for the
            // foreign function
            // Any needed call to `goto_block` will be performed by `emulate_foreign_item`.
            let args = ecx.copy_fn_args(args); // FIXME: Should `InPlace` arguments be reset to uninit?
            let link_name = ecx.item_link_name(instance.def_id());
            return ecx.emulate_foreign_item(link_name, abi, &args, dest, ret, unwind);
        }

        // Otherwise, load the MIR.
        Ok(Some((ecx.load_mir(instance.def, None)?, instance)))
    }

    #[inline(always)]
    fn call_extra_fn(
        ecx: &mut MiriInterpCx<'mir, 'tcx>,
        fn_val: DynSym,
        abi: Abi,
        args: &[FnArg<'tcx, Provenance>],
        dest: &MPlaceTy<'tcx, Provenance>,
        ret: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        let args = ecx.copy_fn_args(args); // FIXME: Should `InPlace` arguments be reset to uninit?
        ecx.emulate_dyn_sym(fn_val, abi, &args, dest, ret, unwind)
    }

    #[inline(always)]
    fn call_intrinsic(
        ecx: &mut MiriInterpCx<'mir, 'tcx>,
        instance: ty::Instance<'tcx>,
        args: &[OpTy<'tcx, Provenance>],
        dest: &MPlaceTy<'tcx, Provenance>,
        ret: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        ecx.call_intrinsic(instance, args, dest, ret, unwind)
    }

    #[inline(always)]
    fn assert_panic(
        ecx: &mut MiriInterpCx<'mir, 'tcx>,
        msg: &mir::AssertMessage<'tcx>,
        unwind: mir::UnwindAction,
    ) -> InterpResult<'tcx> {
        ecx.assert_panic(msg, unwind)
    }

    fn panic_nounwind(ecx: &mut InterpCx<'mir, 'tcx, Self>, msg: &str) -> InterpResult<'tcx> {
        ecx.start_panic_nounwind(msg)
    }

    fn unwind_terminate(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        reason: mir::UnwindTerminateReason,
    ) -> InterpResult<'tcx> {
        // Call the lang item.
        let panic = ecx.tcx.lang_items().get(reason.lang_item()).unwrap();
        let panic = ty::Instance::mono(ecx.tcx.tcx, panic);
        ecx.call_function(
            panic,
            Abi::Rust,
            &[],
            None,
            StackPopCleanup::Goto { ret: None, unwind: mir::UnwindAction::Unreachable },
        )?;
        Ok(())
    }

    #[inline(always)]
    fn binary_ptr_op(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        bin_op: mir::BinOp,
        left: &ImmTy<'tcx, Provenance>,
        right: &ImmTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, (ImmTy<'tcx, Provenance>, bool)> {
        ecx.binary_ptr_op(bin_op, left, right)
    }

    #[inline(always)]
    fn generate_nan<
        F1: rustc_apfloat::Float + rustc_apfloat::FloatConvert<F2>,
        F2: rustc_apfloat::Float,
    >(
        ecx: &InterpCx<'mir, 'tcx, Self>,
        inputs: &[F1],
    ) -> F2 {
        ecx.generate_nan(inputs)
    }

    fn thread_local_static_base_pointer(
        ecx: &mut MiriInterpCx<'mir, 'tcx>,
        def_id: DefId,
    ) -> InterpResult<'tcx, Pointer<Provenance>> {
        ecx.get_or_create_thread_local_alloc(def_id)
    }

    fn extern_static_base_pointer(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        def_id: DefId,
    ) -> InterpResult<'tcx, Pointer<Provenance>> {
        let link_name = ecx.item_link_name(def_id);
        if let Some(&ptr) = ecx.machine.extern_statics.get(&link_name) {
            // Various parts of the engine rely on `get_alloc_info` for size and alignment
            // information. That uses the type information of this static.
            // Make sure it matches the Miri allocation for this.
            let Provenance::Concrete { alloc_id, .. } = ptr.provenance else {
                panic!("extern_statics cannot contain wildcards")
            };
            let (shim_size, shim_align, _kind) = ecx.get_alloc_info(alloc_id);
            let def_ty = ecx.tcx.type_of(def_id).instantiate_identity();
            let extern_decl_layout = ecx.tcx.layout_of(ty::ParamEnv::empty().and(def_ty)).unwrap();
            if extern_decl_layout.size != shim_size || extern_decl_layout.align.abi != shim_align {
                throw_unsup_format!(
                    "extern static `{link_name}` has been declared as `{krate}::{name}` \
                    with a size of {decl_size} bytes and alignment of {decl_align} bytes, \
                    but Miri emulates it via an extern static shim \
                    with a size of {shim_size} bytes and alignment of {shim_align} bytes",
                    name = ecx.tcx.def_path_str(def_id),
                    krate = ecx.tcx.crate_name(def_id.krate),
                    decl_size = extern_decl_layout.size.bytes(),
                    decl_align = extern_decl_layout.align.abi.bytes(),
                    shim_size = shim_size.bytes(),
                    shim_align = shim_align.bytes(),
                )
            }
            Ok(ptr)
        } else {
            throw_unsup_format!("extern static `{link_name}` is not supported by Miri",)
        }
    }

    fn adjust_allocation<'b>(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        id: AllocId,
        alloc: Cow<'b, Allocation>,
        kind: Option<MemoryKind>,
    ) -> InterpResult<'tcx, Cow<'b, Allocation<Self::Provenance, Self::AllocExtra>>> {
        let kind = kind.expect("we set our STATIC_KIND so this cannot be None");
        if ecx.machine.tracked_alloc_ids.contains(&id) {
            ecx.emit_diagnostic(NonHaltingDiagnostic::CreatedAlloc(
                id,
                alloc.size(),
                alloc.align,
                kind,
            ));
        }

        let alloc = alloc.into_owned();
        let borrow_tracker = ecx
            .machine
            .borrow_tracker
            .as_ref()
            .map(|bt| bt.borrow_mut().new_allocation(id, alloc.size(), kind, &ecx.machine));

        let race_alloc = ecx.machine.data_race.as_ref().map(|data_race| {
            data_race::AllocState::new_allocation(
                data_race,
                &ecx.machine.threads,
                alloc.size(),
                kind,
                ecx.machine.current_span(),
            )
        });
        let buffer_alloc = ecx.machine.weak_memory.then(weak_memory::AllocState::new_allocation);

        // If an allocation is leaked, we want to report a backtrace to indicate where it was
        // allocated. We don't need to record a backtrace for allocations which are allowed to
        // leak.
        let backtrace = if kind.may_leak() || !ecx.machine.collect_leak_backtraces {
            None
        } else {
            Some(ecx.generate_stacktrace())
        };

        let alloc: Allocation<Provenance, Self::AllocExtra> = alloc.adjust_from_tcx(
            &ecx.tcx,
            AllocExtra {
                borrow_tracker,
                data_race: race_alloc,
                weak_memory: buffer_alloc,
                backtrace,
            },
            |ptr| ecx.global_base_pointer(ptr),
        )?;

        if matches!(kind, MemoryKind::Machine(kind) if kind.should_save_allocation_span()) {
            ecx.machine
                .allocation_spans
                .borrow_mut()
                .insert(id, (ecx.machine.current_span(), None));
        }

        Ok(Cow::Owned(alloc))
    }

    fn adjust_alloc_base_pointer(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        ptr: Pointer<CtfeProvenance>,
    ) -> InterpResult<'tcx, Pointer<Provenance>> {
        let alloc_id = ptr.provenance.alloc_id();
        if cfg!(debug_assertions) {
            // The machine promises to never call us on thread-local or extern statics.
            match ecx.tcx.try_get_global_alloc(alloc_id) {
                Some(GlobalAlloc::Static(def_id)) if ecx.tcx.is_thread_local_static(def_id) => {
                    panic!("adjust_alloc_base_pointer called on thread-local static")
                }
                Some(GlobalAlloc::Static(def_id)) if ecx.tcx.is_foreign_item(def_id) => {
                    panic!("adjust_alloc_base_pointer called on extern static")
                }
                _ => {}
            }
        }
        // FIXME: can we somehow preserve the immutability of `ptr`?
        let tag = if let Some(borrow_tracker) = &ecx.machine.borrow_tracker {
            borrow_tracker.borrow_mut().base_ptr_tag(alloc_id, &ecx.machine)
        } else {
            // Value does not matter, SB is disabled
            BorTag::default()
        };
        ecx.ptr_from_rel_ptr(ptr, tag)
    }

    /// Called on `usize as ptr` casts.
    #[inline(always)]
    fn ptr_from_addr_cast(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        addr: u64,
    ) -> InterpResult<'tcx, Pointer<Option<Self::Provenance>>> {
        ecx.ptr_from_addr_cast(addr)
    }

    /// Called on `ptr as usize` casts.
    /// (Actually computing the resulting `usize` doesn't need machine help,
    /// that's just `Scalar::try_to_int`.)
    fn expose_ptr(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        ptr: Pointer<Self::Provenance>,
    ) -> InterpResult<'tcx> {
        match ptr.provenance {
            Provenance::Concrete { alloc_id, tag } => ecx.expose_ptr(alloc_id, tag),
            Provenance::Wildcard => {
                // No need to do anything for wildcard pointers as
                // their provenances have already been previously exposed.
                Ok(())
            }
        }
    }

    /// Convert a pointer with provenance into an allocation-offset pair,
    /// or a `None` with an absolute address if that conversion is not possible.
    ///
    /// This is called when a pointer is about to be used for memory access,
    /// an in-bounds check, or anything else that requires knowing which allocation it points to.
    /// The resulting `AllocId` will just be used for that one step and the forgotten again
    /// (i.e., we'll never turn the data returned here back into a `Pointer` that might be
    /// stored in machine state).
    fn ptr_get_alloc(
        ecx: &MiriInterpCx<'mir, 'tcx>,
        ptr: Pointer<Self::Provenance>,
    ) -> Option<(AllocId, Size, Self::ProvenanceExtra)> {
        let rel = ecx.ptr_get_alloc(ptr);

        rel.map(|(alloc_id, size)| {
            let tag = match ptr.provenance {
                Provenance::Concrete { tag, .. } => ProvenanceExtra::Concrete(tag),
                Provenance::Wildcard => ProvenanceExtra::Wildcard,
            };
            (alloc_id, size, tag)
        })
    }

    #[inline(always)]
    fn before_memory_read(
        _tcx: TyCtxtAt<'tcx>,
        machine: &Self,
        alloc_extra: &AllocExtra<'tcx>,
        (alloc_id, prov_extra): (AllocId, Self::ProvenanceExtra),
        range: AllocRange,
    ) -> InterpResult<'tcx> {
        if machine.track_alloc_accesses && machine.tracked_alloc_ids.contains(&alloc_id) {
            machine
                .emit_diagnostic(NonHaltingDiagnostic::AccessedAlloc(alloc_id, AccessKind::Read));
        }
        if let Some(data_race) = &alloc_extra.data_race {
            data_race.read(alloc_id, range, NaReadType::Read, None, machine)?;
        }
        if let Some(borrow_tracker) = &alloc_extra.borrow_tracker {
            borrow_tracker.before_memory_read(alloc_id, prov_extra, range, machine)?;
        }
        if let Some(weak_memory) = &alloc_extra.weak_memory {
            weak_memory.memory_accessed(range, machine.data_race.as_ref().unwrap());
        }
        Ok(())
    }

    #[inline(always)]
    fn before_memory_write(
        _tcx: TyCtxtAt<'tcx>,
        machine: &mut Self,
        alloc_extra: &mut AllocExtra<'tcx>,
        (alloc_id, prov_extra): (AllocId, Self::ProvenanceExtra),
        range: AllocRange,
    ) -> InterpResult<'tcx> {
        if machine.track_alloc_accesses && machine.tracked_alloc_ids.contains(&alloc_id) {
            machine
                .emit_diagnostic(NonHaltingDiagnostic::AccessedAlloc(alloc_id, AccessKind::Write));
        }
        if let Some(data_race) = &mut alloc_extra.data_race {
            data_race.write(alloc_id, range, NaWriteType::Write, None, machine)?;
        }
        if let Some(borrow_tracker) = &mut alloc_extra.borrow_tracker {
            borrow_tracker.before_memory_write(alloc_id, prov_extra, range, machine)?;
        }
        if let Some(weak_memory) = &alloc_extra.weak_memory {
            weak_memory.memory_accessed(range, machine.data_race.as_ref().unwrap());
        }
        Ok(())
    }

    #[inline(always)]
    fn before_memory_deallocation(
        _tcx: TyCtxtAt<'tcx>,
        machine: &mut Self,
        alloc_extra: &mut AllocExtra<'tcx>,
        (alloc_id, prove_extra): (AllocId, Self::ProvenanceExtra),
        size: Size,
        align: Align,
        _kind: MemoryKind,
    ) -> InterpResult<'tcx> {
        if machine.tracked_alloc_ids.contains(&alloc_id) {
            machine.emit_diagnostic(NonHaltingDiagnostic::FreedAlloc(alloc_id));
        }
        if let Some(data_race) = &mut alloc_extra.data_race {
            data_race.write(
                alloc_id,
                alloc_range(Size::ZERO, size),
                NaWriteType::Deallocate,
                None,
                machine,
            )?;
        }
        if let Some(borrow_tracker) = &mut alloc_extra.borrow_tracker {
            borrow_tracker.before_memory_deallocation(alloc_id, prove_extra, size, machine)?;
        }
        if let Some((_, deallocated_at)) = machine.allocation_spans.borrow_mut().get_mut(&alloc_id)
        {
            *deallocated_at = Some(machine.current_span());
        }
        machine.alloc_addresses.get_mut().free_alloc_id(
            machine.rng.get_mut(),
            alloc_id,
            size,
            align,
        );
        Ok(())
    }

    #[inline(always)]
    fn retag_ptr_value(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        kind: mir::RetagKind,
        val: &ImmTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, ImmTy<'tcx, Provenance>> {
        if ecx.machine.borrow_tracker.is_some() {
            ecx.retag_ptr_value(kind, val)
        } else {
            Ok(val.clone())
        }
    }

    #[inline(always)]
    fn retag_place_contents(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        kind: mir::RetagKind,
        place: &PlaceTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx> {
        if ecx.machine.borrow_tracker.is_some() {
            ecx.retag_place_contents(kind, place)?;
        }
        Ok(())
    }

    fn protect_in_place_function_argument(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        place: &MPlaceTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx> {
        // If we have a borrow tracker, we also have it set up protection so that all reads *and
        // writes* during this call are insta-UB.
        let protected_place = if ecx.machine.borrow_tracker.is_some() {
            ecx.protect_place(place)?
        } else {
            // No borrow tracker.
            place.clone()
        };
        // We do need to write `uninit` so that even after the call ends, the former contents of
        // this place cannot be observed any more. We do the write after retagging so that for
        // Tree Borrows, this is considered to activate the new tag.
        // Conveniently this also ensures that the place actually points to suitable memory.
        ecx.write_uninit(&protected_place)?;
        // Now we throw away the protected place, ensuring its tag is never used again.
        Ok(())
    }

    #[inline(always)]
    fn init_frame_extra(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        frame: Frame<'mir, 'tcx, Provenance>,
    ) -> InterpResult<'tcx, Frame<'mir, 'tcx, Provenance, FrameExtra<'tcx>>> {
        // Start recording our event before doing anything else
        let timing = if let Some(profiler) = ecx.machine.profiler.as_ref() {
            let fn_name = frame.instance.to_string();
            let entry = ecx.machine.string_cache.entry(fn_name.clone());
            let name = entry.or_insert_with(|| profiler.alloc_string(&*fn_name));

            Some(profiler.start_recording_interval_event_detached(
                *name,
                measureme::EventId::from_label(*name),
                ecx.get_active_thread().to_u32(),
            ))
        } else {
            None
        };

        let borrow_tracker = ecx.machine.borrow_tracker.as_ref();

        let extra = FrameExtra {
            borrow_tracker: borrow_tracker.map(|bt| bt.borrow_mut().new_frame(&ecx.machine)),
            catch_unwind: None,
            timing,
            is_user_relevant: ecx.machine.is_user_relevant(&frame),
            salt: ecx.machine.rng.borrow_mut().gen::<usize>() % ADDRS_PER_CONST,
        };

        Ok(frame.with_extra(extra))
    }

    fn stack<'a>(
        ecx: &'a InterpCx<'mir, 'tcx, Self>,
    ) -> &'a [Frame<'mir, 'tcx, Self::Provenance, Self::FrameExtra>] {
        ecx.active_thread_stack()
    }

    fn stack_mut<'a>(
        ecx: &'a mut InterpCx<'mir, 'tcx, Self>,
    ) -> &'a mut Vec<Frame<'mir, 'tcx, Self::Provenance, Self::FrameExtra>> {
        ecx.active_thread_stack_mut()
    }

    fn before_terminator(ecx: &mut InterpCx<'mir, 'tcx, Self>) -> InterpResult<'tcx> {
        ecx.machine.basic_block_count += 1u64; // a u64 that is only incremented by 1 will "never" overflow
        ecx.machine.since_gc += 1;
        // Possibly report our progress.
        if let Some(report_progress) = ecx.machine.report_progress {
            if ecx.machine.basic_block_count % u64::from(report_progress) == 0 {
                ecx.emit_diagnostic(NonHaltingDiagnostic::ProgressReport {
                    block_count: ecx.machine.basic_block_count,
                });
            }
        }

        // Search for BorTags to find all live pointers, then remove all other tags from borrow
        // stacks.
        // When debug assertions are enabled, run the GC as often as possible so that any cases
        // where it mistakenly removes an important tag become visible.
        if ecx.machine.gc_interval > 0 && ecx.machine.since_gc >= ecx.machine.gc_interval {
            ecx.machine.since_gc = 0;
            ecx.run_provenance_gc();
        }

        // These are our preemption points.
        ecx.maybe_preempt_active_thread();

        // Make sure some time passes.
        ecx.machine.clock.tick();

        Ok(())
    }

    #[inline(always)]
    fn after_stack_push(ecx: &mut InterpCx<'mir, 'tcx, Self>) -> InterpResult<'tcx> {
        if ecx.frame().extra.is_user_relevant {
            // We just pushed a local frame, so we know that the topmost local frame is the topmost
            // frame. If we push a non-local frame, there's no need to do anything.
            let stack_len = ecx.active_thread_stack().len();
            ecx.active_thread_mut().set_top_user_relevant_frame(stack_len - 1);
        }
        Ok(())
    }

    fn before_stack_pop(
        ecx: &InterpCx<'mir, 'tcx, Self>,
        frame: &Frame<'mir, 'tcx, Self::Provenance, Self::FrameExtra>,
    ) -> InterpResult<'tcx> {
        // We want this *before* the return value copy, because the return place itself is protected
        // until we do `end_call` here.
        if ecx.machine.borrow_tracker.is_some() {
            ecx.on_stack_pop(frame)?;
        }
        // tracing-tree can autoamtically annotate scope changes, but it gets very confused by our
        // concurrency and what it prints is just plain wrong. So we print our own information
        // instead. (Cc https://github.com/rust-lang/miri/issues/2266)
        info!("Leaving {}", ecx.frame().instance);
        Ok(())
    }

    #[inline(always)]
    fn after_stack_pop(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        frame: Frame<'mir, 'tcx, Provenance, FrameExtra<'tcx>>,
        unwinding: bool,
    ) -> InterpResult<'tcx, StackPopJump> {
        if frame.extra.is_user_relevant {
            // All that we store is whether or not the frame we just removed is local, so now we
            // have no idea where the next topmost local frame is. So we recompute it.
            // (If this ever becomes a bottleneck, we could have `push` store the previous
            // user-relevant frame and restore that here.)
            ecx.active_thread_mut().recompute_top_user_relevant_frame();
        }
        let res = {
            // Move `frame`` into a sub-scope so we control when it will be dropped.
            let mut frame = frame;
            let timing = frame.extra.timing.take();
            let res = ecx.handle_stack_pop_unwind(frame.extra, unwinding);
            if let Some(profiler) = ecx.machine.profiler.as_ref() {
                profiler.finish_recording_interval_event(timing.unwrap());
            }
            res
        };
        // Needs to be done after dropping frame to show up on the right nesting level.
        // (Cc https://github.com/rust-lang/miri/issues/2266)
        if !ecx.active_thread_stack().is_empty() {
            info!("Continuing in {}", ecx.frame().instance);
        }
        res
    }

    fn after_local_allocated(
        ecx: &mut InterpCx<'mir, 'tcx, Self>,
        local: mir::Local,
        mplace: &MPlaceTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx> {
        let Some(Provenance::Concrete { alloc_id, .. }) = mplace.ptr().provenance else {
            panic!("after_local_allocated should only be called on fresh allocations");
        };
        let local_decl = &ecx.frame().body.local_decls[local];
        let span = local_decl.source_info.span;
        ecx.machine.allocation_spans.borrow_mut().insert(alloc_id, (span, None));
        Ok(())
    }

    fn eval_mir_constant<F>(
        ecx: &InterpCx<'mir, 'tcx, Self>,
        val: mir::Const<'tcx>,
        span: Span,
        layout: Option<TyAndLayout<'tcx>>,
        eval: F,
    ) -> InterpResult<'tcx, OpTy<'tcx, Self::Provenance>>
    where
        F: Fn(
            &InterpCx<'mir, 'tcx, Self>,
            mir::Const<'tcx>,
            Span,
            Option<TyAndLayout<'tcx>>,
        ) -> InterpResult<'tcx, OpTy<'tcx, Self::Provenance>>,
    {
        let frame = ecx.active_thread_stack().last().unwrap();
        let mut cache = ecx.machine.const_cache.borrow_mut();
        match cache.entry((val, frame.extra.salt)) {
            Entry::Vacant(ve) => {
                let op = eval(ecx, val, span, layout)?;
                ve.insert(op.clone());
                Ok(op)
            }
            Entry::Occupied(oe) => Ok(oe.get().clone()),
        }
    }
}
