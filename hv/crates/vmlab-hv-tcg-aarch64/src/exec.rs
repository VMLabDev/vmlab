//! The interpreter loop: translate a block at the PC, run it on the slot
//! executor of `vmlab-hv-tcg-ir`, write the registers back, repeat.
//!
//! The addresses of a guest are virtual. Every fetch, load and store goes
//! through the softmmu TLB of the CPU (`mmu.rs`, DESIGN.md R12) to a
//! physical address on a `Memory`, also while the MMU of the guest is off.
//! The loop takes the exceptions and the interrupts of the guest itself
//! (`exception.rs`), among them the data and instruction aborts, and runs
//! the system instructions that end a block (`Trap`). It stops for what
//! only its owner can serve: a hypercall, a `WFI` with no interrupt line
//! high, the end of a budget, and an instruction that has no lifter yet.
//!
//! A block is lowered once into its pre-decoded form (`block`, the gate of
//! card C5) and kept under its PC. That form is pure scalar code with memory
//! events between it: loads, stores and guards, in the order of the
//! instructions. Each event names an exit
//! that holds the registers as they were before its instruction, so a failed
//! access leaves the registers of that instruction untouched.
//!
//! The exclusive monitor of a CPU is the address of its last exclusive
//! load, the value that the load read, and the write generation of the
//! memory's reservation granules at that moment (`Memory::load_reserved`).
//! What it does on one CPU follows what the lockstep saw QEMU do (S6,
//! DESIGN.md U1): an exclusive store writes when its address is the marked
//! one and the memory, read at the size of the store, still holds the value.
//! The sizes of the load and of the store need not be the same, which the
//! architecture leaves open. A store that finds another address, or no
//! mark, reads no memory and fails. Every exclusive store that completes
//! ends the mark. A store of this CPU between the two keeps the mark, and
//! an exception keeps it. An exception return and `CLREX` end it.
//!
//! With more CPUs the store also asks that no other writer reached the
//! granules of the load (`Memory::store_conditional`), so it fails more
//! often than QEMU's: a write of another CPU fails it also when it left the
//! value as it was.
//!
//! A block is kept under its virtual PC, its context and the physical
//! address of its code, so a new mapping of the same PC finds no old block.
//! It also keeps the code stamp of the memory for its bytes
//! (`Memory::code_stamp`). A store of this CPU into a page that holds
//! translated code, and a write of anything else to the memory, start a new
//! epoch: a block of an older epoch is used again only when its stamp still
//! holds. A write of another CPU is seen at the start of `run` and `step`
//! and at every exception and exception return, which are the points where
//! the architecture lets a CPU see new code.
//!
//! Sources read: S6 for what a system instruction does at each level (the
//! lockstep of the cards C6 and C8 and the probe of the system registers).
//! This file holds no other instruction semantics.

use crate::exception::{self, Cause, Exception};
use crate::generated::decode::{Record, decode};
use crate::lift::{A64Exit, A64Next, Builder, ExceptionClass, Lifted, PstateField, Trap, lift};
use crate::mmu::{self, Kind, Miss, Mmu, Regime};
use crate::shootdown::Invalidation;
use crate::state::{Context, State, slot};
use crate::sysreg::{self, Denied, Environment, reg, sctlr};
use crate::timer::{self, Clock};
use vmlab_hv_tcg::{CodeStamp, Error, Memory};
use vmlab_hv_tcg_ir::block::{self, Code, FIRST_EFFECT, Generic, Lowering, Op, Place, Slot};
use vmlab_hv_tcg_ir::native::block::{BlockOf, Event};
use vmlab_hv_tcg_ir::native::fasthash::{FastMap, FastSet};
use vmlab_hv_tcg_ir::native::{
    Graph, HelperFn, Operation, ScalarId, ScalarType, dce, mask, schedule,
};
use vmlab_hv_tcg_jit as jit;

/// The most instructions in one block.
pub const BLOCK_INSTRUCTIONS: u32 = 64;

/// A machine CPU whose timer is armed reads the clock after this many
/// blocks, to leave its loop when the timer is due (`Cpu::run`).
const CLOCK_BLOCKS: u32 = 64;

/// The block cache is direct-mapped by the PC.
const CACHE_ENTRIES: usize = 1 << 16;
/// The heap bytes of the young generation of the store of a cache: the
/// blocks that a block of another PC took the entry of (`Cache`). The old
/// generation holds as much again at most.
const SPARE_BYTES: usize = 256 << 20;

/// The pages that hold translated code are a bit set over the low bits of
/// the page number. Two pages with the same low bits share a bit, which
/// costs a slower store and nothing else.
const CODE_PAGE_BITS: usize = 1 << 16;

/// How a guest calls what is above EL1 (DESIGN.md G4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conduit {
    Hvc,
    Smc,
}

/// Why the loop stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The instruction budget of `run` is used up.
    Budget,
    /// The record is decoded and has no lifter yet. The PC of the state is
    /// the instruction, and no part of it has changed the state.
    Unimplemented { pc: u64, record: Record },
    /// `HVC` or `SMC` at EL1, at the address `pc`. The PC of the state is
    /// the instruction after it. The machine serves the call (PSCI on
    /// `virt`) and runs on, or refuses it with `Cpu::refuse_hypercall`.
    Hypercall {
        conduit: Conduit,
        immediate: u16,
        pc: u64,
    },
    /// `WFI` with no interrupt line high. The PC of the state is the
    /// instruction after it. The machine waits for a line and runs on.
    Halt,
    /// An error of the host, not of the guest.
    Host(String),
}

/// An exception or an interrupt that the CPU took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    pub cause: Cause,
    /// The instruction that took it, or that an interrupt came before.
    pub from: u64,
    /// The vector.
    pub to: u64,
}

/// What the guest asked for that this CPU does not have (DESIGN.md G3f). The
/// guest got the Undefined Instruction exception, as the architecture says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unknown {
    /// A word that no selected record matches.
    Encoding(u32),
    /// `MRS` or `MSR` of a register without a row, or one that the CPU
    /// model leaves out.
    Register { encoding: u16, write: bool },
    /// `SYS` or `SYSL` of an operation that the CPU does not have, by
    /// op1:CRn:CRm:op2.
    Operation { encoding: u16, read: bool },
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encoding(word) => write!(f, "undefined instruction {word:08x}"),
            Self::Register { encoding, write } => write!(
                f,
                "{} of the system register {}, which this CPU does not have",
                if *write { "write" } else { "read" },
                sysreg::name(*encoding)
            ),
            Self::Operation { encoding, read } => write!(
                f,
                "{} of the system operation {}, which this CPU does not have",
                if *read { "SYSL" } else { "SYS" },
                operation_name(*encoding)
            ),
        }
    }
}

/// The name of a `SYS` operation for a log line: the names of the LLVM
/// tables, or the generic form.
pub fn operation_name(encoding: u16) -> String {
    use crate::generated::sysreg::{AT_OPS, DC_OPS, IC_OPS, TLBI_OPS};
    let named = |kind: &str, name: &str| format!("{kind} {name}");
    if let Some(op) = DC_OPS.iter().find(|op| op.encoding == encoding) {
        return named("DC", op.name);
    }
    if let Some(op) = AT_OPS.iter().find(|op| op.encoding == encoding) {
        return named("AT", op.name);
    }
    if let Some(op) = IC_OPS.iter().find(|op| op.encoding == encoding) {
        return named("IC", op.name);
    }
    let (op1, crn, crm, op2) = (
        encoding >> 11 & 7,
        encoding >> 7 & 15,
        encoding >> 3 & 15,
        encoding & 7,
    );
    if crn == 8
        && let Some(op) = TLBI_OPS.iter().find(|op| {
            u16::from(op.op1) == op1 && u16::from(op.crm) == crm && u16::from(op.op2) == op2
        })
    {
        return named("TLBI", op.name);
    }
    format!("S1_{op1}_C{crn}_C{crm}_{op2}")
}

/// The hook for a register without a row. `write`: the value of an `MSR`.
/// `Some`: the machine has the register, and the value is what an `MRS`
/// reads.
pub type RegisterHook = fn(state: &mut State, encoding: u16, write: Option<u64>) -> Option<u64>;

/// The hook for a word that no selected record matches. `true`: the machine
/// ran it, and the CPU goes on at the next instruction.
pub type InstructionHook = fn(state: &mut State, word: u32) -> bool;

/// What a machine adds to the core (DESIGN.md G3e, A5): registers of the
/// IMPLEMENTATION DEFINED space and instructions of its own. Each hook is
/// asked before the Undefined Instruction exception is taken.
#[derive(Clone, Copy, Default)]
pub struct Hooks {
    pub register: Option<RegisterHook>,
    pub instruction: Option<InstructionHook>,
}

/// Why no block starts at the PC.
enum Refused {
    Stop(Stop),
    Exception(Exception),
}

/// One access that the CPU made, for a harness that compares them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub address: u64,
    /// 1, 2, 4, 8 or 16.
    pub bytes: u8,
    /// The bytes, little-endian.
    pub value: u128,
    pub write: bool,
}

/// The mark that an exclusive load leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reservation {
    /// The virtual address of the load, which a store is compared with.
    address: u64,
    /// The physical address, which the memory keeps the generation of.
    phys: u64,
    /// The size of the load.
    bytes: u8,
    generation: u64,
}

/// Why an access did not complete.
enum Fault {
    /// A fault of the guest, with the address for FAR_EL1.
    Abort(mmu::Fault, u64),
    Host(String),
}

impl Fault {
    /// What a failed translation of `address` is.
    fn of(miss: Miss, address: u64) -> Self {
        match miss {
            Miss::Fault(fault) => Self::Abort(fault, address),
            Miss::Host(message) => Self::Host(message),
        }
    }

    /// What an error of the memory at the translation of `address` is: the
    /// memory holds nothing there, which is an external abort.
    fn memory(error: Error, address: u64) -> Self {
        match error {
            Error::Memory(_) => Self::Abort(mmu::Fault::EXTERNAL, address),
            other => Self::Host(other.to_string()),
        }
    }
}

/// Why a store did not complete.
enum Unstored {
    /// It goes into a page of translated code, in a block of more than one
    /// instruction.
    Retry,
    Fault(Fault),
    /// A fault that says "read": the alignment fault of an exclusive store,
    /// which QEMU reports that way (the lockstep of card C8).
    ReadFault(Fault),
}

impl From<Fault> for Unstored {
    fn from(fault: Fault) -> Self {
        Self::Fault(fault)
    }
}

/// Where the bytes of an access are: the physical address of the first, how
/// many of them the first page holds, and the physical address of the rest
/// when the access crosses into another page.
#[derive(Clone, Copy)]
struct Spot {
    first: u64,
    in_first: usize,
    second: u64,
}

/// What a block needs beside the registers and the memory.
struct Side {
    exclusive: Option<Reservation>,
    /// The value of the last exclusive load, with zeros above its size. A
    /// load of 16 bytes leaves its lower half here and its upper half in
    /// `exclusive_high`, which no other load changes.
    exclusive_value: u64,
    exclusive_high: u64,
    /// The accesses since the caller last took them, when it asked for them.
    log: Option<Vec<Access>>,
    /// A filter in front of `code_set`: a bit for the low bits of each
    /// physical page that holds translated code.
    code_pages: Vec<u64>,
    /// The physical pages that hold translated code.
    code_set: FastSet<u64>,
    /// Writes of this CPU to the memory since the caches were last checked.
    own_writes: u64,
    /// One of them went into a page that holds translated code.
    code_written: bool,
    /// The translations of the CPU.
    mmu: Mmu,
    /// The block that runs is at EL0.
    user: bool,
}

impl Side {
    fn new(state: &State) -> Self {
        Self {
            exclusive: None,
            exclusive_value: 0,
            exclusive_high: 0,
            log: None,
            code_pages: vec![0; CODE_PAGE_BITS / 64],
            code_set: FastSet::default(),
            own_writes: 0,
            code_written: false,
            mmu: Mmu::new(state),
            user: false,
        }
    }

    fn page_bit(address: u64) -> (usize, u64) {
        let page = (address >> 12) as usize & (CODE_PAGE_BITS - 1);
        (page / 64, 1 << (page % 64))
    }

    /// The page of the physical address holds translated code from now on.
    fn mark_code(&mut self, address: u64) {
        let (word, bit) = Self::page_bit(address);
        self.code_pages[word] |= bit;
        self.code_set.insert(address >> 12);
    }

    /// Whether the page of the physical address can hold translated code.
    /// `false` is exact, and `true` asks `code_set`.
    #[inline(always)]
    fn maybe_code(&self, address: u64) -> bool {
        let (word, bit) = Self::page_bit(address);
        self.code_pages[word] & bit != 0
    }

    /// Whether the bytes at `spot` are in a page that holds translated code.
    fn is_code(&self, spot: Spot, bytes: u8) -> bool {
        let holds =
            |address: u64| self.maybe_code(address) && self.code_set.contains(&(address >> 12));
        holds(spot.first) || (spot.in_first < usize::from(bytes) && holds(spot.second))
    }

    /// A store of this CPU reached the memory, in `writes` parts. `code`: it
    /// went into a page of translated code. The page is one no more: its
    /// blocks do not pass their stamp, and translating one marks it again.
    fn wrote(&mut self, address: u64, spot: Spot, bytes: u8, value: u128, code: bool) {
        let crossed = spot.in_first < usize::from(bytes);
        self.own_writes += 1 + u64::from(crossed);
        if code {
            self.code_written = true;
            self.code_set.remove(&(spot.first >> 12));
            if crossed {
                self.code_set.remove(&(spot.second >> 12));
            }
        }
        if let Some(log) = &mut self.log {
            log.push(Access {
                address,
                bytes,
                value,
                write: true,
            });
        }
    }

    /// Where the `bytes` bytes at the virtual address `va` are, for an
    /// access of `kind` made as EL0 when `user`.
    #[inline(always)]
    fn resolve<M: Memory>(
        &mut self,
        memory: &mut M,
        va: u64,
        bytes: u8,
        kind: Kind,
        user: bool,
    ) -> Result<Spot, Fault> {
        if let Some((first, _)) = self.mmu.hit(va, bytes.into(), mmu::needed(kind, user)) {
            return Ok(Spot {
                first,
                in_first: bytes.into(),
                second: 0,
            });
        }
        self.resolve_slow(memory, va, bytes, kind, user)
    }

    #[cold]
    fn resolve_slow<M: Memory>(
        &mut self,
        memory: &mut M,
        va: u64,
        bytes: u8,
        kind: Kind,
        user: bool,
    ) -> Result<Spot, Fault> {
        let (first, _) = self
            .mmu
            .translate(memory, va, kind, user)
            .map_err(|miss| Fault::of(miss, va))?;
        let in_first = (0x1000 - (va & 0xfff)).min(u64::from(bytes)) as usize;
        let mut spot = Spot {
            first,
            in_first,
            second: 0,
        };
        if in_first < usize::from(bytes) {
            // The rest is in the next page, which has a translation of its
            // own. A fault there names the first address of that page.
            let next = va.wrapping_add(in_first as u64);
            spot.second = self
                .mmu
                .translate(memory, next, kind, user)
                .map_err(|miss| Fault::of(miss, next))?
                .0;
        }
        Ok(spot)
    }

    /// Whether an access at `va` that is not aligned to its size takes an
    /// alignment fault: with SCTLR_EL1.A, and in Device memory. The order
    /// of the faults is what the lockstep found on QEMU:
    ///
    /// - While the MMU is off all memory is Device memory, and the fault
    ///   comes before every other one, also for an address that is wider
    ///   than a physical address. With SCTLR_EL1.A it comes first too.
    /// - In Device memory it comes after the faults of the walk for the
    ///   page (translation, address size, access flag) and before the
    ///   permission fault of the page.
    /// - The page of the first byte decides. An access that crosses from
    ///   Normal memory into Device memory does not fault.
    ///
    /// `false` where another fault comes first: the access itself reports
    /// that one.
    /// `device`: the alignment in bytes that Device memory asks of the
    /// access, which is none for a structure of vector registers.
    #[cold]
    fn unaligned_faults<M: Memory>(
        &mut self,
        memory: &mut M,
        va: u64,
        kind: Kind,
        device: u8,
    ) -> bool {
        let controls = *self.mmu.controls();
        if !controls.enabled || controls.strict_alignment {
            return true;
        }
        va & (u64::from(device.max(1)) - 1) != 0
            && self
                .mmu
                .page(memory, va, kind)
                .is_ok_and(|page| page.flags & mmu::flag::DEVICE != 0)
    }

    fn read_at<M: Memory>(
        memory: &mut M,
        va: u64,
        spot: Spot,
        part: &mut [u8],
    ) -> Result<(), Fault> {
        let (low, high) = part.split_at_mut(spot.in_first);
        memory
            .read(spot.first, low)
            .map_err(|error| Fault::memory(error, va))?;
        if !high.is_empty() {
            memory
                .read(spot.second, high)
                .map_err(|error| Fault::memory(error, va.wrapping_add(spot.in_first as u64)))?;
        }
        Ok(())
    }

    fn write_at<M: Memory>(memory: &mut M, va: u64, spot: Spot, part: &[u8]) -> Result<(), Fault> {
        let (low, high) = part.split_at(spot.in_first);
        memory
            .write(spot.first, low)
            .map_err(|error| Fault::memory(error, va))?;
        if !high.is_empty() {
            memory
                .write(spot.second, high)
                .map_err(|error| Fault::memory(error, va.wrapping_add(spot.in_first as u64)))?;
        }
        Ok(())
    }

    fn load<M: Memory>(
        &mut self,
        memory: &mut M,
        address: u64,
        bytes: u8,
        exclusive: bool,
        user: bool,
    ) -> Result<u128, Fault> {
        let spot = self.resolve(memory, address, bytes, Kind::Read, user)?;
        let mut buffer = [0u8; 16];
        let part = &mut buffer[..usize::from(bytes)];
        if exclusive {
            // A failed load leaves the monitor as it was. An exclusive load
            // is aligned, so it is in one page.
            let generation = memory
                .load_reserved(spot.first, part)
                .map_err(|error| Fault::memory(error, address))?;
            let value = u128::from_le_bytes(buffer);
            self.exclusive = Some(Reservation {
                address,
                phys: spot.first,
                bytes,
                generation,
            });
            self.exclusive_value = value as u64;
            if bytes == 16 {
                self.exclusive_high = (value >> 64) as u64;
            }
        } else {
            Self::read_at(memory, address, spot, part)?;
        }
        let value = u128::from_le_bytes(buffer);
        if let Some(log) = &mut self.log {
            log.push(Access {
                address,
                bytes,
                value,
                write: false,
            });
        }
        Ok(value)
    }

    /// `single`: the block is one instruction, which makes its stores also
    /// into a page of translated code.
    fn store<M: Memory>(
        &mut self,
        memory: &mut M,
        address: u64,
        bytes: u8,
        value: u128,
        user: bool,
        single: bool,
    ) -> Result<(), Unstored> {
        let spot = self.resolve(memory, address, bytes, Kind::Write, user)?;
        let code = self.is_code(spot, bytes);
        if code && !single {
            return Err(Unstored::Retry);
        }
        let buffer = value.to_le_bytes();
        let part = &buffer[..usize::from(bytes)];
        match self.exclusive {
            None => Self::write_at(memory, address, spot, part)?,
            Some(reservation) => {
                // The store moves the write generation of its granules, and
                // those can be the reserved ones. A reservation that was
                // intact takes the new generation. The lock keeps the store
                // of another CPU from landing between the check and the new
                // generation, where it would be taken for our own.
                let lock = memory.transaction_lock();
                let _atomic = lock.as_ref().map(|lock| lock.lock());
                let len = usize::from(reservation.bytes);
                let intact =
                    memory.reservation_generation(reservation.phys, len) == reservation.generation;
                Self::write_at(memory, address, spot, part)?;
                if intact {
                    self.exclusive = Some(Reservation {
                        generation: memory.reservation_generation(reservation.phys, len),
                        ..reservation
                    });
                }
            }
        }
        self.wrote(address, spot, bytes, value, code);
        Ok(())
    }

    /// `DC ZVA`: zeros in the block of `ZERO_BLOCK` bytes that holds
    /// `address`.
    fn zero<M: Memory>(&mut self, memory: &mut M, address: u64, user: bool) -> Result<(), Fault> {
        let base = address & !(ZERO_BLOCK as u64 - 1);
        // The block is aligned to its size, so it is in one page. A fault
        // names the address that the instruction gave, as QEMU's does.
        let spot =
            self.resolve(memory, base, 1, Kind::Write, user)
                .map_err(|fault| match fault {
                    Fault::Abort(fault, _) => Fault::Abort(fault, address),
                    host => host,
                })?;
        let code = self.is_code(spot, 1);
        // An external abort names the first byte of the block.
        memory
            .write(spot.first, &[0u8; ZERO_BLOCK])
            .map_err(|error| Fault::memory(error, base))?;
        self.own_writes += 1;
        if code {
            self.code_written = true;
            self.code_set.remove(&(spot.first >> 12));
        }
        if let Some(log) = &mut self.log {
            for half in 0..ZERO_BLOCK / 16 {
                log.push(Access {
                    address: base + 16 * half as u64,
                    bytes: 16,
                    value: 0,
                    write: true,
                });
            }
        }
        Ok(())
    }

    /// Whether the store wrote, and whether it reports success. The two are
    /// the same thing unless the store is smaller than the value of its
    /// load: QEMU then writes where the low bytes agree and reports failure
    /// unless the whole value agrees, and so does this.
    fn store_exclusive<M: Memory>(
        &mut self,
        memory: &mut M,
        address: u64,
        bytes: u8,
        value: u128,
        user: bool,
        single: bool,
    ) -> Result<(bool, bool), Unstored> {
        // QEMU keeps "no mark" as the address 2^64 - 1, so for it a store
        // there is at the marked address also without a mark, and is an
        // access that can fault. The architecture leaves open whether an
        // exclusive store that fails takes the fault of its access, so the
        // same is done here. Such a store never writes.
        let marked = self.exclusive.map_or(u64::MAX, |mark| mark.address);
        if marked != address {
            // Another address: no memory is looked at.
            self.exclusive = None;
            return Ok((false, false));
        }
        // From here the store is an access. One that faults leaves the
        // mark, as its instruction did not complete.
        if address & (u64::from(bytes) - 1) != 0 {
            return Err(Unstored::ReadFault(Fault::Abort(
                mmu::Fault::ALIGNMENT,
                address,
            )));
        }
        // The store is aligned, so it is in one page. It is an access that
        // writes, also when it then finds another value and writes nothing.
        let spot = self.resolve(memory, address, bytes, Kind::Write, user)?;
        let code = self.is_code(spot, bytes);
        if code && !single {
            return Err(Unstored::Retry);
        }
        let phys = spot.first;
        let buffer = value.to_le_bytes();
        let part = &buffer[..usize::from(bytes)];
        let lock = memory.transaction_lock();
        let _atomic = lock.as_ref().map(|lock| lock.lock());
        let mut now = [0u8; 16];
        memory
            .read(phys, &mut now[..usize::from(bytes)])
            .map_err(|error| Fault::memory(error, address))?;
        let now = u128::from_le_bytes(now);
        let Some(reservation) = self.exclusive.take() else {
            return Ok((false, false));
        };
        let expected = u128::from(self.exclusive_value) | u128::from(self.exclusive_high) << 64;
        // The memory holds the value, as far as the store reaches.
        let same = now == expected & bits_mask(bytes);
        // The memory holds the whole value: 64 bits, or 128 for a pair of
        // 64-bit registers.
        let whole = same && (bytes == 16 || now == u128::from(self.exclusive_value));
        if !same {
            return Ok((false, false));
        }
        let wrote = if bytes == reservation.bytes {
            memory
                .store_conditional(phys, part, reservation.generation)
                .map_err(|error| Fault::memory(error, address))?
        } else if memory.reservation_generation(reservation.phys, usize::from(reservation.bytes))
            == reservation.generation
        {
            // A store of another size than its load: the granules of the
            // load are untouched, and the lock is held.
            memory
                .write(phys, part)
                .map_err(|error| Fault::memory(error, address))?;
            true
        } else {
            false
        };
        if wrote {
            self.wrote(address, spot, bytes, value, code);
        }
        Ok((wrote, wrote && whole))
    }
}

/// A load or a store that has no code of its own in the block form: one of
/// 16 bytes, or an exclusive one.
#[derive(Clone, Copy, Debug)]
enum Access16 {
    Read {
        address: Slot,
        result: Place,
        bytes: u8,
        exclusive: bool,
        /// The access is made as EL0, whatever the level.
        unprivileged: bool,
        exit: u32,
    },
    Write {
        address: Slot,
        value: Place,
        bytes: u8,
        /// The word of the result of an exclusive store.
        exclusive: Option<Slot>,
        unprivileged: bool,
        exit: u32,
    },
}

/// The bit of `Op::aux` of a load or a store that is made as EL0.
const UNPRIVILEGED: u8 = 0x80;

/// Where a block can leave before its end.
#[derive(Clone, Debug)]
struct Leave {
    /// Register slot, words of the frame: copied out when the block leaves
    /// here. The PC is one of them.
    outputs: Vec<(u32, Place)>,
    /// The instructions of the block that are complete here.
    completed: u32,
    /// The word of the address of an alignment fault.
    address: Option<Slot>,
    class: ExceptionClass,
}

/// Why an effect did not complete.
enum Failed {
    /// The condition of a guard did not hold, and the guard names no
    /// exception.
    Guard,
    /// A load or a store, and whether it is a store.
    Access(Fault, bool),
    /// A store into a page of translated code, in a block of more than one
    /// instruction.
    Retry,
}

impl Failed {
    fn store(refused: Unstored) -> Self {
        match refused {
            Unstored::Retry => Self::Retry,
            Unstored::Fault(fault) => Self::Access(fault, true),
            Unstored::ReadFault(fault) => Self::Access(fault, false),
        }
    }
}

/// Why a block left before its end.
enum Halt {
    Stop(Stop),
    /// The CPU took an exception or an interrupt without an instruction of
    /// the block. It costs one instruction of a budget, so that a guest
    /// with a broken vector does not hold the loop for ever.
    Taken,
    /// An instruction of the block takes this exception. The registers are
    /// those before it.
    Raise(Exception),
    /// A store into a page that holds translated code, in a block of more
    /// than one instruction. Nothing of the instruction is lost: the loop
    /// runs it alone and then drops the translated blocks.
    Retry,
}

/// A block as the loop runs it: the pre-decoded form of `block`.
struct Compiled {
    /// The physical address of the first instruction. No block holds its
    /// virtual address: it computes its addresses from the PC at its entry
    /// (`Builder::code_address`).
    phys: u64,
    context: Context,
    /// The stamp of the memory for the bytes of the code, and the epoch of
    /// the CPU in which the stamp was last seen to hold.
    stamp: Option<CodeStamp>,
    epoch: u64,
    instructions: u32,
    /// The operations in the order in which they run: the pure ones, and
    /// between them the loads, the stores, the guards and the helper calls.
    ops: Vec<Op>,
    generics: Vec<Generic>,
    accesses: Vec<Access16>,
    leaves: Vec<Leave>,
    /// The words of the scalars. The constants are in place.
    frame: Vec<u64>,
    /// The helper calls (R10).
    calls: Vec<Call>,
    /// Words of the frame, register slot: copied in before the block runs.
    inputs: Vec<(Place, u32)>,
    /// Register slot, words of the frame: copied out after it.
    outputs: Vec<(u32, Place)>,
    /// The last instruction, when the loop runs it: its distance in bytes
    /// from the first, and what it does.
    terminal: Option<(u64, Trap)>,
    /// The compiled code of the block, when the JIT is on (3e DESIGN.md
    /// J1). Code of an older generation of the region is none.
    code: Option<jit::Entry>,
    /// The emitter has no form for an operation of the block on this host:
    /// it runs in the interpreter (H2b).
    refused: bool,
}

/// The most arguments of a helper.
const CALL_ARGUMENTS: usize = 8;

/// One helper call of a block. Its place among the operations is that of
/// its `Code::Call`.
struct Call {
    function: HelperFn,
    /// The words of the arguments.
    args: Vec<Place>,
    /// The words of each result, with the mask of its type.
    results: Vec<(Place, u128)>,
}

fn host(message: &str) -> Stop {
    Stop::Host(message.to_string())
}

fn narrow(value: usize) -> Result<u32, Stop> {
    u32::try_from(value).map_err(|_| host("a block with too many slots"))
}

/// The mask of the type of a scalar that a helper fills.
fn result_mask(graph: &Graph, id: ScalarId) -> u128 {
    match graph.scalars[id].ty {
        ScalarType::Bits(width) => mask(width),
        ScalarType::Bool => 1,
        ScalarType::Integer => u128::MAX,
    }
}

/// What `compile` gathers beside the operations of the block.
#[derive(Default)]
struct Parts {
    accesses: Vec<Access16>,
    leaves: Vec<Leave>,
    calls: Vec<Call>,
    inputs: Vec<(Place, u32)>,
    op_counts: Option<Vec<usize>>,
}

impl Parts {
    /// Put a helper call after the operations that the block has so far.
    fn call(&mut self, lowering: &mut Lowering, call: Call) -> Result<(), Stop> {
        let index = narrow(self.calls.len())?;
        self.calls.push(call);
        lowering.effect(Code::Call, 0, 0, 0, 0, index.into());
        Ok(())
    }

    /// Append the pure scalars that `roots` need and the block has not
    /// computed yet: constants and inputs are bound, a pure helper call
    /// becomes a `Call`, and the rest is lowered.
    fn emit(
        &mut self,
        lowering: &mut Lowering,
        graph: &Graph,
        seen: &mut [bool],
        roots: &[ScalarId],
    ) -> Result<(), Stop> {
        for id in schedule::scalars(graph, roots, seen) {
            let before = lowering.ops.len();
            match &graph.scalars[id].operation {
                Operation::Constant(value) => lowering.constant(id, *value),
                Operation::Input(register) => {
                    self.inputs.push((lowering.place(id), narrow(*register)?));
                }
                // The event of the effect writes its words.
                Operation::Effect(_) => {}
                // A pure helper call runs at its place among the operations.
                Operation::Call { helper, args } => {
                    if args.len() > CALL_ARGUMENTS {
                        return Err(host("a helper with too many arguments"));
                    }
                    let call = Call {
                        function: helper.0.function,
                        args: args.iter().map(|arg| lowering.place(*arg)).collect(),
                        results: vec![(lowering.place(id), result_mask(graph, id))],
                    };
                    self.call(lowering, call)?;
                }
                _ => lowering.scalar(graph, id),
            }
            if let Some(counts) = &mut self.op_counts {
                counts[id] = lowering.ops.len() - before;
            }
        }
        Ok(())
    }
}

fn exit_roots(exit: &A64Exit) -> Vec<ScalarId> {
    let mut roots: Vec<ScalarId> = exit
        .writeback
        .writes
        .iter()
        .map(|(_, scalar)| *scalar)
        .collect();
    match &exit.next {
        A64Next::Pc(next) => roots.push(*next),
        A64Next::Exception { syndrome, .. } => roots.extend(syndrome),
        A64Next::Trap { .. } => {}
    }
    roots
}

/// Static operation counts summed over translations, including retranslations.
#[derive(Debug, Default)]
pub struct BlockCounts {
    pub blocks: u64,
    pub operations: u64,
    pub alignment_operations: u64,
    pub exit_writeback_operations: u64,
    pub ldp_x: u64,
    pub stp_x: u64,
}

impl BlockCounts {
    fn translated(&mut self, block: &BlockOf<A64Exit>, counts: &[usize], operations: usize) {
        let graph = &block.graph;
        let mut normal = vec![false; counts.len()];
        let mut alignment = Vec::new();
        let mut writeback = Vec::new();
        for event in &block.events {
            let (exit, roots) = match event {
                Event::Guard {
                    condition, exit, ..
                } if matches!(
                    block.exits[*exit].next,
                    A64Next::Exception {
                        class: ExceptionClass::Alignment { .. },
                        ..
                    }
                ) =>
                {
                    alignment.push(*condition);
                    self.alignment_operations += 1;
                    (Some(*exit), Vec::new())
                }
                Event::Guard {
                    condition, exit, ..
                } => (Some(*exit), vec![*condition]),
                Event::ReadMemory { address, error, .. } => (Some(*error), vec![*address]),
                Event::WriteMemory {
                    address,
                    value,
                    error,
                    ..
                } => (Some(*error), vec![*address, *value]),
                Event::Call { args, .. } => (None, args.clone()),
                Event::Exit(exit) => (None, exit_roots(&block.exits[*exit])),
                Event::Checkpoint(_) => continue,
            };
            schedule::scalars(graph, &roots, &mut normal);
            if let Some(exit) = exit {
                let payload = &block.exits[exit];
                writeback.extend(payload.writeback.writes.iter().map(|&(_, id)| id));
                if let A64Next::Exception { syndrome, .. } = &payload.next {
                    schedule::scalars(graph, syndrome, &mut normal);
                }
            }
        }
        // Shared dependencies belong to neither exclusive category.
        let alignment_only = schedule::scalars(graph, &alignment, &mut normal.clone());
        let writeback_only = schedule::scalars(graph, &writeback, &mut normal);
        let mut shared = vec![false; counts.len()];
        for &id in &writeback_only {
            shared[id] = true;
        }
        self.alignment_operations += alignment_only
            .iter()
            .filter(|&&id| !shared[id])
            .map(|&id| counts[id] as u64)
            .sum::<u64>();
        for id in alignment_only {
            shared[id] = false;
        }
        self.exit_writeback_operations += writeback_only
            .into_iter()
            .filter(|&id| shared[id])
            .map(|id| counts[id] as u64)
            .sum::<u64>();
        self.blocks += 1;
        self.operations += operations as u64;
    }
}

fn compile(
    phys: u64,
    context: Context,
    instructions: u32,
    mut block: BlockOf<A64Exit>,
    counts: Option<&mut BlockCounts>,
) -> Result<Compiled, Stop> {
    dce::prune_scalars(&mut block);
    let graph = &block.graph;
    let mut lowering = Lowering::new(graph).ok_or_else(|| host("a block with too many slots"))?;
    let mut seen = vec![false; graph.scalars.len()];
    let mut parts = Parts {
        op_counts: counts.as_ref().map(|_| vec![0; graph.scalars.len()]),
        ..Parts::default()
    };
    let mut block_outputs = None;
    let mut terminal = None;
    // The exits that events leave through, by their number in the block.
    let mut leave_of = vec![None; block.exits.len()];
    for event in &block.events {
        if block_outputs.is_some() {
            return Err(host("an event after the exit of a block"));
        }
        let (exit, mut roots) = match event {
            Event::Guard {
                condition, exit, ..
            } => (*exit, vec![*condition]),
            Event::ReadMemory { address, error, .. } => (*error, vec![*address]),
            Event::WriteMemory {
                address,
                value,
                error,
                ..
            } => (*error, vec![*address, *value]),
            // An ordered helper call, after what its arguments need. It
            // leaves through no exit.
            Event::Call {
                helper,
                args,
                results,
            } => {
                if !matches!(helper.0.function, HelperFn::Ordered(_))
                    || args.len() > CALL_ARGUMENTS
                    || results.len() > CALL_ARGUMENTS
                {
                    return Err(host("an ordered call of a helper that is not one"));
                }
                parts.emit(&mut lowering, graph, &mut seen, args)?;
                for result in results {
                    seen[*result] = true;
                }
                let call = Call {
                    function: helper.0.function,
                    args: args.iter().map(|arg| lowering.place(*arg)).collect(),
                    results: results
                        .iter()
                        .map(|result| (lowering.place(*result), result_mask(graph, *result)))
                        .collect(),
                };
                parts.call(&mut lowering, call)?;
                continue;
            }
            Event::Exit(exit) => (*exit, Vec::new()),
            Event::Checkpoint(_) => return Err(host("an aarch64 block has no checkpoint")),
        };
        let payload = &block.exits[exit];
        roots.extend(exit_roots(payload));
        parts.emit(&mut lowering, graph, &mut seen, &roots)?;
        let mut outputs = Vec::new();
        for (register, scalar) in &payload.writeback.writes {
            outputs.push((narrow(*register)?, lowering.place(*scalar)));
        }
        if let Event::Exit(_) = event {
            match payload.next {
                A64Next::Pc(next) => outputs.push((slot::PC as u32, lowering.place(next))),
                // The loop runs the last instruction, with the PC at it.
                A64Next::Trap { pc, trap } => terminal = Some((pc, trap)),
                A64Next::Exception { .. } => {
                    return Err(host("a block that ends in the exit of an event"));
                }
            }
            block_outputs = Some(outputs);
            continue;
        }
        let leave = match leave_of[exit] {
            Some(leave) => leave,
            None => {
                let A64Next::Exception { class, syndrome } = &payload.next else {
                    return Err(host("an event that leaves a block without an exception"));
                };
                let leave = narrow(parts.leaves.len())?;
                parts.leaves.push(Leave {
                    outputs,
                    completed: payload.completed,
                    address: syndrome.first().map(|scalar| lowering.word(*scalar)),
                    class: *class,
                });
                leave_of[exit] = Some(leave);
                leave
            }
        };
        let unprivileged = matches!(
            parts.leaves[leave as usize].class,
            ExceptionClass::DataAbort { unprivileged: true }
        );
        let mode = if unprivileged { UNPRIVILEGED } else { 0 };
        match event {
            Event::Guard {
                condition,
                expected,
                ..
            } => {
                let condition = lowering.word(*condition);
                lowering.effect(
                    Code::Guard,
                    u8::from(*expected),
                    0,
                    condition,
                    0,
                    leave.into(),
                );
            }
            Event::ReadMemory {
                address,
                bytes,
                exclusive,
                result,
                ..
            } => {
                let place = lowering.place(*result);
                let code = match (*bytes, *exclusive || place.wide) {
                    (1, false) => Code::Load1,
                    (2, false) => Code::Load2,
                    (4, false) => Code::Load4,
                    (8, false) => Code::Load8,
                    _ => Code::Access,
                };
                let address = lowering.word(*address);
                if code == Code::Access {
                    let index = narrow(parts.accesses.len())?;
                    parts.accesses.push(Access16::Read {
                        address,
                        result: place,
                        bytes: *bytes,
                        exclusive: *exclusive,
                        unprivileged,
                        exit: leave,
                    });
                    lowering.effect(code, 0, 0, 0, 0, index.into());
                } else {
                    lowering.effect(code, *bytes | mode, place.slot, address, 0, leave.into());
                }
            }
            Event::WriteMemory {
                address,
                value,
                bytes,
                exclusive,
                result,
                ..
            } => {
                let code = match (*bytes, *exclusive) {
                    (1, false) => Code::Store1,
                    (2, false) => Code::Store2,
                    (4, false) => Code::Store4,
                    (8, false) => Code::Store8,
                    _ => Code::Access,
                };
                let at = lowering.word(*address);
                if code == Code::Access {
                    let index = narrow(parts.accesses.len())?;
                    parts.accesses.push(Access16::Write {
                        address: at,
                        value: lowering.place(*value),
                        bytes: *bytes,
                        exclusive: match (exclusive, result) {
                            (false, _) => None,
                            (true, Some(result)) => Some(lowering.word(*result)),
                            (true, None) => {
                                return Err(host("an exclusive store without a result"));
                            }
                        },
                        unprivileged,
                        exit: leave,
                    });
                    lowering.effect(code, 0, 0, 0, 0, index.into());
                } else {
                    let value = lowering.word(*value);
                    lowering.effect(code, *bytes | mode, 0, at, value, leave.into());
                }
            }
            Event::Call { .. } | Event::Exit(_) | Event::Checkpoint(_) => {}
        }
    }
    let Some(outputs) = block_outputs else {
        return Err(host("a block without an exit"));
    };
    if let (Some(counts), Some(op_counts)) = (counts, &parts.op_counts) {
        counts.translated(&block, op_counts, lowering.ops.len());
    }
    // The loop reads the words of an operation without a check.
    let words = lowering.frame.len();
    let generics = lowering.generics.len();
    if !lowering
        .ops
        .iter()
        .all(|op| block::op_in_frame(op, words, generics))
    {
        return Err(host(
            "an operation of a block names a word outside its frame",
        ));
    }
    Ok(Compiled {
        phys,
        context,
        stamp: None,
        epoch: 0,
        instructions,
        ops: lowering.ops,
        generics: lowering.generics,
        accesses: parts.accesses,
        leaves: parts.leaves,
        frame: lowering.frame,
        calls: parts.calls,
        inputs: parts.inputs,
        outputs,
        terminal,
        code: None,
        refused: false,
    })
}

fn bits_mask(bytes: u8) -> u128 {
    if bytes >= 16 {
        u128::MAX
    } else {
        (1u128 << (u32::from(bytes) * 8)) - 1
    }
}

/// A load of `N` bytes, 1 to 8, into one word.
#[inline(always)]
fn load<M: Memory, const N: usize>(
    op: &Op,
    frame: &mut [u64],
    memory: &mut M,
    side: &mut Side,
) -> Result<(), (u32, Failed)> {
    // SAFETY: `compile` checked the words of every operation.
    let at = unsafe { block::word(frame, usize::from(op.a)) };
    let user = side.user || op.aux & UNPRIVILEGED != 0;
    // The common case: the TLB holds the page with the permission, nothing
    // keeps the accesses, and the memory holds the address. A read that
    // fails has changed nothing, and the general path reads again to say
    // why.
    if side.log.is_none()
        && let Some((phys, _)) = side.mmu.hit(at, N as u64, mmu::needed(Kind::Read, user))
    {
        let mut bytes = [0u8; 8];
        if memory.read(phys, &mut bytes[..N]).is_ok() {
            // SAFETY: as above.
            unsafe { block::set_word(frame, usize::from(op.dst), u64::from_le_bytes(bytes)) };
            return Ok(());
        }
    }
    match side.load(memory, at, N as u8, false, user) {
        Ok(value) => {
            frame[usize::from(op.dst)] = value as u64;
            Ok(())
        }
        Err(fault) => Err((op.imm as u32, Failed::Access(fault, false))),
    }
}

/// A store of the low `N` bytes, 1 to 8, of one word.
#[inline(always)]
fn store<M: Memory, const N: usize>(
    op: &Op,
    frame: &[u64],
    memory: &mut M,
    side: &mut Side,
    single: bool,
) -> Result<(), (u32, Failed)> {
    // SAFETY: `compile` checked the words of every operation.
    let (at, value) = unsafe {
        (
            block::word(frame, usize::from(op.a)),
            block::word(frame, usize::from(op.b)),
        )
    };
    let exit = op.imm as u32;
    let user = side.user || op.aux & UNPRIVILEGED != 0;
    // The common case: the TLB holds the page with the permission, the page
    // holds no translated code, there is no exclusive mark to carry over
    // the store, and nothing keeps the accesses. A write that fails has
    // changed nothing, and the general path writes again to say why.
    if side.exclusive.is_none()
        && side.log.is_none()
        && let Some((phys, _)) = side.mmu.hit(at, N as u64, mmu::needed(Kind::Write, user))
        && !side.maybe_code(phys)
        && memory.write(phys, &value.to_le_bytes()[..N]).is_ok()
    {
        side.own_writes += 1;
        return Ok(());
    }
    let value = u128::from(value) & bits_mask(N as u8);
    side.store(memory, at, N as u8, value, user, single)
        .map_err(|refused| (exit, Failed::store(refused)))
}

impl Compiled {
    /// The bytes that the block holds, itself and on the heap, for the bound
    /// of the store of its cache.
    fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        size_of::<Self>()
            + self.ops.capacity() * size_of::<Op>()
            + self.generics.capacity() * size_of::<Generic>()
            + self.accesses.capacity() * size_of::<Access16>()
            + self
                .leaves
                .iter()
                .map(|leave| {
                    size_of::<Leave>() + leave.outputs.capacity() * size_of::<(u32, Place)>()
                })
                .sum::<usize>()
            + self.frame.capacity() * size_of::<u64>()
            + self
                .calls
                .iter()
                .map(|call| {
                    size_of::<Call>()
                        + call.args.capacity() * size_of::<Place>()
                        + call.results.capacity() * size_of::<(Place, u128)>()
                })
                .sum::<usize>()
            + self.inputs.capacity() * size_of::<(Place, u32)>()
            + self.outputs.capacity() * size_of::<(u32, Place)>()
    }

    /// Make one effect. The error: the exit to leave through, and why.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn effect<M: Memory>(
        op: &Op,
        frame: &mut [u64],
        accesses: &[Access16],
        calls: &[Call],
        leaves: &[Leave],
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
        single: bool,
    ) -> Result<(), (u32, Failed)> {
        match op.code {
            Code::Guard => {
                // SAFETY: `compile` checked the words of every operation.
                if (unsafe { block::word(frame, usize::from(op.a)) } != 0) != (op.aux != 0) {
                    return Self::guard(op.imm as u32, frame, leaves, memory, side);
                }
                Ok(())
            }
            Code::Load1 => load::<M, 1>(op, frame, memory, side),
            Code::Load2 => load::<M, 2>(op, frame, memory, side),
            Code::Load4 => load::<M, 4>(op, frame, memory, side),
            Code::Load8 => load::<M, 8>(op, frame, memory, side),
            Code::Store1 => store::<M, 1>(op, frame, memory, side, single),
            Code::Store2 => store::<M, 2>(op, frame, memory, side, single),
            Code::Store4 => store::<M, 4>(op, frame, memory, side, single),
            Code::Store8 => store::<M, 8>(op, frame, memory, side, single),
            Code::Access => Self::access(accesses[op.imm as usize], frame, memory, side, single),
            Code::Call => {
                Self::call(&calls[op.imm as usize], frame, state);
                Ok(())
            }
            // `block::step` runs every other code.
            _ => Ok(()),
        }
    }

    /// A guard whose condition does not hold. An alignment guard leaves the
    /// block only where the access faults for its alignment.
    #[cold]
    fn guard<M: Memory>(
        exit: u32,
        frame: &[u64],
        leaves: &[Leave],
        memory: &mut M,
        side: &mut Side,
    ) -> Result<(), (u32, Failed)> {
        let leave = &leaves[exit as usize];
        let ExceptionClass::Alignment {
            write,
            always,
            device,
            ..
        } = leave.class
        else {
            return Err((exit, Failed::Guard));
        };
        let address = leave.address.map_or(0, |slot| frame[usize::from(slot)]);
        let kind = if write { Kind::Write } else { Kind::Read };
        if always || side.unaligned_faults(memory, address, kind, device) {
            let fault = Fault::Abort(mmu::Fault::ALIGNMENT, address);
            return Err((exit, Failed::Access(fault, write)));
        }
        Ok(())
    }

    #[inline(never)]
    fn call(call: &Call, frame: &mut [u64], state: &mut State) {
        let mut args = [0u128; CALL_ARGUMENTS];
        for (arg, place) in args.iter_mut().zip(&call.args) {
            *arg = place.read(frame);
        }
        let args = &args[..call.args.len()];
        match call.function {
            HelperFn::Pure(function) => {
                let (place, mask) = call.results[0];
                place.write(frame, function(args) & mask);
            }
            // An ordered helper sees the state as it was at the entry of the
            // block for every register slot: the slots are written back at
            // the exit. What it may touch is the state that is not in a slot.
            HelperFn::Ordered(function) => {
                let mut results = [0u128; CALL_ARGUMENTS];
                let count = call.results.len().min(CALL_ARGUMENTS);
                function(&mut *state, args, &mut results[..count]);
                for ((place, mask), result) in call.results.iter().zip(results) {
                    place.write(frame, result & mask);
                }
            }
        }
    }

    /// A load or a store of 16 bytes, or an exclusive one.
    #[inline(never)]
    fn access<M: Memory>(
        access: Access16,
        frame: &mut [u64],
        memory: &mut M,
        side: &mut Side,
        single: bool,
    ) -> Result<(), (u32, Failed)> {
        match access {
            Access16::Read {
                address,
                result,
                bytes,
                exclusive,
                unprivileged,
                exit,
            } => {
                let at = frame[usize::from(address)];
                let user = side.user || unprivileged;
                match side.load(memory, at, bytes, exclusive, user) {
                    Ok(value) => result.write(frame, value),
                    Err(fault) => return Err((exit, Failed::Access(fault, false))),
                }
            }
            Access16::Write {
                address,
                value,
                bytes,
                exclusive,
                unprivileged,
                exit,
            } => {
                let at = frame[usize::from(address)];
                let value = value.read(frame) & bits_mask(bytes);
                let user = side.user || unprivileged;
                match exclusive {
                    None => side
                        .store(memory, at, bytes, value, user, single)
                        .map_err(|refused| (exit, Failed::store(refused)))?,
                    Some(result) => {
                        let (_, succeeded) = side
                            .store_exclusive(memory, at, bytes, value, user, single)
                            .map_err(|refused| (exit, Failed::store(refused)))?;
                        frame[usize::from(result)] = u64::from(!succeeded);
                    }
                }
            }
        }
        Ok(())
    }

    /// Leave through `exit`, which `failed` asked for: the registers as they
    /// were before the instruction that could not complete.
    #[cold]
    fn leave(&self, state: &mut State, exit: u32, failed: Failed) -> (u32, Option<Halt>) {
        let leave = &self.leaves[exit as usize];
        for (register, place) in &leave.outputs {
            state.slots[*register as usize] = place.read(&self.frame);
        }
        let halt = match failed {
            Failed::Retry => Halt::Retry,
            Failed::Guard => Halt::Stop(host("a guard that names no exception")),
            Failed::Access(Fault::Abort(fault, address), write) => {
                Halt::Raise(Exception::DataAbort {
                    status: fault.status(),
                    write,
                    cache: false,
                    address,
                })
            }
            Failed::Access(Fault::Host(message), _) => Halt::Stop(Stop::Host(message)),
        };
        (leave.completed, Some(halt))
    }

    /// Run the block. The count of instructions that completed, and why the
    /// block left before its end when it did.
    #[inline]
    fn run<M: Memory>(
        &mut self,
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
    ) -> (u32, Option<Halt>) {
        let entry = state.pc();
        for (place, register) in &self.inputs {
            place.write(&mut self.frame, state.slots[*register as usize]);
        }
        // A block of one instruction makes its stores also into a page of
        // translated code, and the loop starts a new epoch after it.
        let single = self.instructions == 1;
        side.user = self.context.el == 0;
        for op in &self.ops {
            if (op.code as u8) < FIRST_EFFECT {
                // SAFETY: `compile` checked every operation against the
                // frame and the generic operations of the block, and
                // neither changes size after it.
                if !unsafe { block::step_unchecked(op, &mut self.frame, &self.generics) } {
                    return (
                        0,
                        Some(Halt::Stop(host(
                            "an arithmetic operation outside its domain",
                        ))),
                    );
                }
                continue;
            }
            let done = Self::effect(
                op,
                &mut self.frame,
                &self.accesses,
                &self.calls,
                &self.leaves,
                state,
                memory,
                side,
                single,
            );
            if let Err((exit, failed)) = done {
                return self.leave(state, exit, failed);
            }
        }
        for (register, place) in &self.outputs {
            state.slots[*register as usize] = place.read(&self.frame);
        }
        // The loop runs the last instruction with the PC at it.
        if let Some((distance, _)) = self.terminal {
            state.set_pc(entry.wrapping_add(distance));
        }
        (self.instructions, None)
    }
}

/// What the thunk of compiled code reaches: the parts of the block that it
/// runs an effect with, and the CPU around it.
struct Env<'a, M> {
    ops: &'a [Op],
    generics: &'a [Generic],
    accesses: &'a [Access16],
    calls: &'a [Call],
    leaves: &'a [Leave],
    frame: *mut u64,
    words: usize,
    state: *mut State,
    memory: &'a mut M,
    side: &'a mut Side,
    single: bool,
    /// Why the block left, when an effect did not complete.
    failed: Option<(u32, Failed)>,
    stop: Option<Stop>,
}

/// What the thunk returns when the block leaves.
const THUNK_LEFT: u64 = 2;

/// The thunk of compiled code (3e DESIGN.md J1): it runs the operation
/// `index` of the block as the interpreter does. The emitter put every word
/// that the operation reads into the frame, and reads what it writes from
/// there.
///
/// # Safety
/// `env` is the `Env<M>` of the block that runs, and `index` one of its
/// operations, which `compile` checked against the frame.
unsafe extern "sysv64" fn thunk<M: Memory>(env: *mut std::ffi::c_void, index: u64) -> u64 {
    // SAFETY: the caller's promise. Compiled code holds no reference into
    // the environment while it calls.
    let env = unsafe { &mut *env.cast::<Env<M>>() };
    let op = &env.ops[index as usize];
    // SAFETY: the frame of the block, which nothing else touches while the
    // thunk runs.
    let frame = unsafe { std::slice::from_raw_parts_mut(env.frame, env.words) };
    if (op.code as u8) < FIRST_EFFECT {
        // SAFETY: `compile` checked every operation against the frame and
        // the generic operations of the block.
        if unsafe { block::step_unchecked(op, frame, env.generics) } {
            return 0;
        }
        env.stop = Some(host("an arithmetic operation outside its domain"));
        return THUNK_LEFT;
    }
    // SAFETY: the state of the CPU, which compiled code does not touch
    // while it calls.
    let state = unsafe { &mut *env.state };
    match Compiled::effect(
        op,
        frame,
        env.accesses,
        env.calls,
        env.leaves,
        state,
        env.memory,
        env.side,
        env.single,
    ) {
        Ok(()) => 0,
        Err(failed) => {
            env.failed = Some(failed);
            THUNK_LEFT
        }
    }
}

/// The words of a place.
fn place_words(place: Place) -> impl Iterator<Item = Slot> {
    std::iter::once(place.slot).chain(place.wide.then_some(place.slot + 1))
}

impl Compiled {
    /// Compile the block, or mark it as one that the emitter refuses.
    fn compile_code(&mut self, jit: &mut jit::Jit) {
        let leaves: Vec<Vec<Slot>> = self
            .leaves
            .iter()
            .map(|leave| {
                leave
                    .outputs
                    .iter()
                    .flat_map(|(_, place)| place_words(*place))
                    .chain(leave.address)
                    .collect()
            })
            .collect();
        let accesses: Vec<jit::Effect> = self
            .accesses
            .iter()
            .map(|access| match *access {
                Access16::Read {
                    address,
                    result,
                    exit,
                    ..
                } => jit::Effect {
                    reads: vec![address],
                    writes: place_words(result).collect(),
                    leave: Some(exit),
                },
                Access16::Write {
                    address,
                    value,
                    exclusive,
                    exit,
                    ..
                } => jit::Effect {
                    reads: std::iter::once(address).chain(place_words(value)).collect(),
                    writes: exclusive.into_iter().collect(),
                    leave: Some(exit),
                },
            })
            .collect();
        let calls: Vec<jit::Effect> = self
            .calls
            .iter()
            .map(|call| jit::Effect {
                reads: call.args.iter().copied().flat_map(place_words).collect(),
                writes: call
                    .results
                    .iter()
                    .flat_map(|(place, _)| place_words(*place))
                    .collect(),
                leave: None,
            })
            .collect();
        let shape = jit::Shape {
            ops: &self.ops,
            generics: &self.generics,
            frame: &self.frame,
            accesses: &accesses,
            calls: &calls,
            leaves: &leaves,
            inputs: &self.inputs,
            outputs: &self.outputs,
            instructions: self.instructions,
        };
        match jit.compile(&shape) {
            Ok(entry) => self.code = Some(entry),
            Err(_) => {
                self.code = None;
                self.refused = true;
            }
        }
    }

    /// Run the compiled code of the block. `None`: the budget did not hold
    /// the block, and nothing ran.
    fn run_code<M: Memory>(
        &mut self,
        entry: &jit::Entry,
        jit: &jit::Jit,
        state: &mut State,
        memory: &mut M,
        side: &mut Side,
        budget: u64,
    ) -> Option<(u32, Option<Halt>)> {
        let pc = state.pc();
        side.user = self.context.el == 0;
        let state_pointer: *mut State = state;
        let frame = self.frame.as_mut_ptr();
        let mut env = Env {
            ops: &self.ops,
            generics: &self.generics,
            accesses: &self.accesses,
            calls: &self.calls,
            leaves: &self.leaves,
            frame,
            words: self.frame.len(),
            state: state_pointer,
            memory,
            side,
            single: self.instructions == 1,
            failed: None,
            stop: None,
        };
        let mut context = jit::Context {
            budget: budget.min(i64::MAX as u64) as i64,
            thunk: thunk::<M>,
            env: (&raw mut env).cast(),
        };
        // SAFETY: `entry` is current and was compiled from this block, whose
        // frame and register slots are as large as its shape names. While
        // the code runs, only it and the thunk touch the frame and the
        // state, and the thunk reaches them through `env` alone.
        let code = unsafe {
            jit.enter(
                entry,
                &mut context,
                (&raw mut (*state_pointer).slots).cast(),
                frame,
            )
        };
        let failed = env.failed.take();
        let stop = env.stop.take();
        match code {
            jit::DONE => {
                // The loop runs the last instruction with the PC at it.
                if let Some((distance, _)) = self.terminal {
                    state.set_pc(pc.wrapping_add(distance));
                }
                Some((self.instructions, None))
            }
            jit::BUDGET => None,
            _ => Some(match (failed, stop) {
                (Some((exit, failed)), _) => self.leave(state, exit, failed),
                (None, stop) => (
                    0,
                    Some(Halt::Stop(stop.unwrap_or_else(|| {
                        host("compiled code left the block without a reason")
                    }))),
                ),
            }),
        }
    }
}

/// What the JIT of a CPU did, for the bench rows and the debug log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitCounts {
    /// Runs of compiled blocks, and their instructions.
    pub compiled_runs: u64,
    pub compiled_instructions: u64,
    /// Runs of blocks in the interpreter while the JIT is on, and their
    /// instructions: blocks that the emitter refused (H2b), and blocks that
    /// the budget did not hold at their head.
    pub interpreted_runs: u64,
    pub interpreted_instructions: u64,
    /// Blocks compiled and refused, the bytes of code, the resets of the
    /// region and the host time of the compiles.
    pub compiled: u64,
    pub refused: u64,
    pub bytes: u64,
    pub resets: u64,
    pub compile_ns: u64,
}

/// Run `block` on its compiled code when the JIT is on and takes it, else
/// in the interpreter.
fn execute<M: Memory>(
    block: &mut Compiled,
    state: &mut State,
    memory: &mut M,
    side: &mut Side,
    jit: &mut Option<jit::Jit>,
    counts: &mut JitCounts,
    budget: u64,
) -> (u32, Option<Halt>) {
    let Some(jit) = jit.as_mut() else {
        return block.run(state, memory, side);
    };
    if !block.refused && !block.code.is_some_and(|entry| jit.current(&entry)) {
        block.compile_code(jit);
    }
    if let Some(entry) = block.code
        && let Some(done) = block.run_code(&entry, jit, state, memory, side, budget)
    {
        counts.compiled_runs += 1;
        counts.compiled_instructions += u64::from(done.0);
        return done;
    }
    let done = block.run(state, memory, side);
    counts.interpreted_runs += 1;
    counts.interpreted_instructions += u64::from(done.0);
    done
}

/// Whether `VMLAB_HV_JIT` turns the JIT on: `1`, `on` or `true`. It is off
/// by default (3e DESIGN.md J1, H2e).
pub fn jit_from_environment() -> bool {
    matches!(
        std::env::var("VMLAB_HV_JIT").as_deref(),
        Ok("1" | "on" | "true")
    )
}

/// The translated blocks of a CPU: an entry for each code address, picked
/// by the low bits of the physical address, and a store of the blocks that
/// lost their entry to other code. A kernel and its programs run more code than the entries
/// cover, so without the store the same blocks were translated again and
/// again (card C13).
///
/// The store has two generations. A block that loses its entry goes into
/// the young one. When the young one holds `SPARE_BYTES` it becomes the old
/// one, and the old one is dropped. A block that is taken back from either
/// returns to its entry. So the blocks that were used since the last turn
/// stay, and those that were not go: emptying the whole store when it was
/// full threw away the code of the running programs with the rest, and a
/// boot of Debian translated three times out of four what it had
/// translated before.
struct Cache {
    entries: Vec<Option<Box<Compiled>>>,
    young: FastMap<BlockKey, Box<Compiled>>,
    old: FastMap<BlockKey, Box<Compiled>>,
    /// The heap bytes of the blocks in `young`.
    young_bytes: usize,
}

/// A block of the store: the physical address of its code and the context
/// it was lifted for. A block computes every address from the PC at its
/// entry (`Builder::code_address`), so it serves its code at any virtual
/// address: a library that two programs map at two places is translated
/// once.
type BlockKey = (u64, Context);

impl Cache {
    fn new() -> Self {
        let mut entries = Vec::new();
        entries.resize_with(CACHE_ENTRIES, || None);
        Self {
            entries,
            young: FastMap::default(),
            old: FastMap::default(),
            young_bytes: 0,
        }
    }

    /// The entry of the code at the physical address `phys`. One block
    /// serves the code at every virtual address that maps it, so the entry
    /// is picked by where the code is, not by the PC. The bits above the
    /// low ones fold in, so that code at the same offsets of distant pages
    /// meets less.
    #[inline(always)]
    fn index(phys: u64) -> usize {
        ((phys >> 2) ^ (phys >> 22) ^ (phys >> 44)) as usize & (CACHE_ENTRIES - 1)
    }

    /// Put `block` in the entry at `index`, and keep the block that was
    /// there in the store.
    fn put(&mut self, index: usize, block: Box<Compiled>) {
        if let Some(old) = self.entries[index].replace(block) {
            self.keep(old);
        }
    }

    fn keep(&mut self, block: Box<Compiled>) {
        let bytes = block.heap_bytes();
        if self.young_bytes + bytes > SPARE_BYTES {
            self.old = std::mem::take(&mut self.young);
            self.young_bytes = 0;
        }
        self.young_bytes += bytes;
        let key = (block.phys, block.context);
        if let Some(replaced) = self.young.insert(key, block) {
            self.young_bytes = self.young_bytes.saturating_sub(replaced.heap_bytes());
        }
    }

    /// Bring the block of the code at `phys` in `context` back from the
    /// store into the entry at `index`, when the store has it. Whether it
    /// still holds the code of the memory is for the caller to check.
    fn revive(&mut self, index: usize, phys: u64, context: Context) -> bool {
        let key = (phys, context);
        let block = match self.young.remove(&key) {
            Some(block) => {
                self.young_bytes = self.young_bytes.saturating_sub(block.heap_bytes());
                block
            }
            None => match self.old.remove(&key) {
                Some(block) => block,
                None => return false,
            },
        };
        self.put(index, block);
        true
    }
}

/// The interrupt lines of a CPU as levels that other threads set: the
/// interrupt controller of a machine, from the thread of a device or from
/// an access of this CPU to the controller. A CPU that has them reads them
/// before every block, so that a line that falls (the guest acknowledged
/// the interrupt) is not taken again.
#[derive(Debug, Default)]
pub struct Lines {
    pub irq: std::sync::atomic::AtomicBool,
    pub fiq: std::sync::atomic::AtomicBool,
}

/// One CPU on a memory.
pub struct Cpu<M> {
    pub state: State,
    pub memory: M,
    /// The two interrupt lines of the core (DESIGN.md F3, A6). A machine
    /// drives them from its interrupt controller. They are levels.
    pub irq: bool,
    pub fiq: bool,
    /// The lines of a machine, which `irq` and `fiq` follow before every
    /// block. With them, `run` also returns after a block that wrote a
    /// register of a timer, so that the machine moves the output lines of
    /// the timers at once.
    pub lines: Option<std::sync::Arc<Lines>>,
    /// A register of a timer was written in the block that ran last.
    timer_written: bool,
    /// Where the system counter comes from.
    pub clock: Clock,
    pub hooks: Hooks,
    /// The last exception or interrupt that `step` or `run` took. `step`
    /// clears it first.
    pub taken: Option<Taken>,
    /// Exceptions and interrupts taken since the CPU was made.
    pub exceptions: u64,
    /// Instructions that have run since the CPU was made. An instruction
    /// that takes an exception counts.
    pub instructions: u64,
    /// Blocks translated since the CPU was made.
    pub translations: u64,
    /// The host time that the translations took, in nanoseconds.
    pub translation_ns: u64,
    /// Enabled by the owner for an instrumented boot.
    pub block_counts: Option<BlockCounts>,
    /// What the guest asked for that the CPU does not have, each once. The
    /// owner of the CPU writes it to its log.
    pub unknown: std::collections::BTreeSet<Unknown>,
    /// The mailboxes of the machine's CPUs and the index of this one: a
    /// TLB maintenance operation of the Inner Shareable domain reaches the
    /// others through them. `None`: the CPU is alone.
    pub shootdown: Option<(std::sync::Arc<crate::shootdown::Shootdown>, usize)>,
    blocks: Cache,
    /// Blocks of one instruction, for `step` and for the end of a budget.
    singles: Cache,
    /// The write generation of the memory at the last check of the caches.
    generation: u64,
    /// A block whose epoch is this one is known to hold the code that the
    /// memory holds.
    epoch: u64,
    side: Side,
    /// The JIT, when it is on (3e DESIGN.md J1).
    jit: Option<jit::Jit>,
    jit_counts: JitCounts,
    /// Why the JIT that `VMLAB_HV_JIT` asked for could not start. The owner
    /// of the CPU writes it to its log.
    pub jit_error: Option<String>,
}

/// The bytes that `DC ZVA` zeroes: DCZID_EL0.BS is 4, which is 16 words.
pub const ZERO_BLOCK: usize = 64;

/// The most entries of `Cpu::unknown`. A guest that walks an encoding space
/// must not fill the memory of the host.
const UNKNOWN_ENTRIES: usize = 4096;

impl<M: Memory> Cpu<M> {
    pub fn new(memory: M, state: State) -> Self {
        let generation = memory.generation();
        let side = Side::new(&state);
        let mut cpu = Self {
            state,
            memory,
            irq: false,
            fiq: false,
            lines: None,
            timer_written: false,
            clock: Clock::Manual(0),
            hooks: Hooks::default(),
            taken: None,
            exceptions: 0,
            instructions: 0,
            translations: 0,
            translation_ns: 0,
            block_counts: None,
            unknown: std::collections::BTreeSet::new(),
            shootdown: None,
            blocks: Cache::new(),
            singles: Cache::new(),
            generation,
            epoch: 1,
            side,
            jit: None,
            jit_counts: JitCounts::default(),
            jit_error: None,
        };
        if jit_from_environment()
            && let Err(error) = cpu.set_jit(true)
        {
            cpu.jit_error = Some(error.to_string());
        }
        cpu
    }

    /// Turn the JIT on or off. On: every block runs as compiled code where
    /// the emitter takes it. Off: the code region is dropped.
    pub fn set_jit(&mut self, on: bool) -> std::io::Result<()> {
        if on == self.jit.is_some() {
            return Ok(());
        }
        self.jit = if on { Some(jit::Jit::new()?) } else { None };
        Ok(())
    }

    pub fn jit_enabled(&self) -> bool {
        self.jit.is_some()
    }

    /// What the JIT did since the CPU was made.
    pub fn jit_counts(&self) -> JitCounts {
        let mut counts = self.jit_counts;
        if let Some(jit) = &self.jit {
            counts.compiled = jit.stats.compiled;
            counts.refused = jit.stats.refused;
            counts.bytes = jit.stats.bytes;
            counts.resets = jit.stats.resets;
            counts.compile_ns = jit.stats.compile_ns;
        }
        counts
    }

    /// The physical address of the virtual address `va`, when it has one,
    /// whatever its permissions. Nothing is changed.
    pub fn physical_address(&mut self, va: u64) -> Option<u64> {
        self.walk_trace(va).1
    }

    /// Walk the translation tables for `va` without the TLB: the
    /// descriptors that the walk reads, as physical address and value, and
    /// the physical address when `va` has one. Nothing is changed.
    pub fn walk_trace(&mut self, va: u64) -> (Vec<(u64, u64)>, Option<u64>) {
        self.side.mmu.sync(&self.state);
        self.side.mmu.trace(&mut self.memory, va)
    }

    /// The count of translations that the TLB of the CPU holds.
    pub fn tlb_entries(&self) -> usize {
        self.side.mmu.entries()
    }

    /// Keep the loads and stores of the CPU from now on, or stop keeping
    /// them. `take_accesses` empties the list.
    pub fn log_accesses(&mut self, on: bool) {
        self.side.log = on.then(Vec::new);
    }

    /// The loads and stores since the last call, in their order.
    pub fn take_accesses(&mut self) -> Vec<Access> {
        self.side
            .log
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// End the exclusive reservation of the CPU, as `CLREX` and an exception
    /// return do.
    pub fn clear_exclusive(&mut self) {
        self.side.exclusive = None;
    }

    /// Put the CPU into `state` from outside the guest: a reset, PSCI
    /// `CPU_ON`, a snapshot restore or a write of the debug port. What the
    /// CPU derived from the state and the memory before is dropped: the TLB
    /// (its entries are tagged and survive a new table base, and a restore
    /// can bring other tables under the same base), the translated blocks,
    /// the code pages and the exclusive reservation. The next `run` builds
    /// them again. The interrupt inputs, the counters and the log of
    /// accesses stay.
    pub fn replace_state(&mut self, state: State) {
        self.state = state;
        let log = self.side.log.take();
        self.side = Side::new(&self.state);
        self.side.log = log;
        self.blocks = Cache::new();
        self.singles = Cache::new();
        self.generation = self.memory.generation();
        self.epoch += 1;
        self.taken = None;
    }

    /// What the system registers read that is not in the state.
    pub fn environment(&self) -> Environment {
        Environment {
            count: self.clock.count(),
            irq: self.irq,
            fiq: self.fiq,
        }
    }

    /// The output lines of the generic timer now.
    pub fn timer_lines(&self) -> timer::Lines {
        timer::lines(&self.state.system.timers, self.clock.count())
    }

    /// The count of the system counter at which a timer line rises next.
    pub fn timer_deadline(&self) -> Option<u64> {
        timer::deadline(&self.state.system.timers, self.clock.count())
    }

    fn take(&mut self, cause: Cause, pc: u64) {
        // An exception is a point where the CPU sees new code.
        self.settle();
        exception::enter(&mut self.state, cause, pc);
        self.exceptions += 1;
        self.taken = Some(Taken {
            cause,
            from: pc,
            to: self.state.pc(),
        });
    }

    fn raise(&mut self, exception: Exception, pc: u64) {
        self.take(Cause::Exception(exception), pc);
    }

    fn note(&mut self, unknown: Unknown) {
        if self.unknown.len() < UNKNOWN_ENTRIES {
            self.unknown.insert(unknown);
        }
    }

    /// The machine does not serve the hypercall that stopped the loop at
    /// `pc`: the guest takes the Undefined Instruction exception there, as
    /// on a CPU without EL2 and EL3.
    pub fn refuse_hypercall(&mut self, pc: u64) {
        self.raise(Exception::Undefined, pc);
    }

    /// `MRS` or `MSR` at `pc`. `Err`: the exception that it takes.
    fn register(&mut self, encoding: u16, rt: u8, write: bool) -> Result<(), Exception> {
        let value = match rt {
            31 => 0,
            n => self.state.x(usize::from(n)),
        };
        let Some(row) = sysreg::find(encoding) else {
            if let Some(hook) = self.hooks.register
                && let Some(read) = hook(&mut self.state, encoding, write.then_some(value))
            {
                if !write && rt != 31 {
                    self.state.set_x(usize::from(rt), read);
                }
                return Ok(());
            }
            self.note(Unknown::Register { encoding, write });
            return Err(Exception::Undefined);
        };
        match sysreg::check(row, write, &self.state) {
            Ok(()) => {}
            Err(Denied::Undefined) => {
                if matches!(sysreg::ROWS[row].kind, sysreg::Kind::Absent(_)) {
                    self.note(Unknown::Register { encoding, write });
                }
                return Err(Exception::Undefined);
            }
            Err(Denied::Trapped) => {
                let iss = sysreg::trap_iss(encoding, rt.into(), write);
                return Err(Exception::SystemTrap(iss));
            }
            Err(Denied::Fp) => return Err(Exception::FpAccess),
        }
        let environment = self.environment();
        if write {
            if matches!(
                sysreg::ROWS[row].kind,
                sysreg::Kind::Special(
                    sysreg::Special::TimerControl(_)
                        | sysreg::Special::TimerCompare(_)
                        | sysreg::Special::TimerValue(_)
                )
            ) {
                self.timer_written = true;
            }
            sysreg::write(row, &mut self.state, &environment, value);
            self.state.refresh();
            self.side.mmu.sync(&self.state);
        } else if rt != 31 {
            let value = sysreg::read(row, &self.state, &environment);
            self.state.set_x(usize::from(rt), value);
        }
        Ok(())
    }

    /// `MSR` of a PSTATE field with an immediate.
    fn pstate(&mut self, field: PstateField, immediate: u8) -> Result<(), Exception> {
        let el0 = self.state.context.el == 0;
        match field {
            PstateField::SpSel if el0 => return Err(Exception::Undefined),
            PstateField::SpSel => self.state.context.sp_sel = immediate & 1 != 0,
            PstateField::DaifSet | PstateField::DaifClr => {
                if el0 && self.state.system.regs[reg::SCTLR_EL1] & sctlr::UMA == 0 {
                    // op0 0, op1 3, CRn 4, CRm the immediate, Rt 31, a write.
                    // QEMU reports op1 in the place of op2 and op2 in the
                    // place of op1 for this form (the lockstep of card C6
                    // found ESR_EL1 0x620793f6 for `MSR DAIFSet, #11`). We
                    // follow it (DESIGN.md P2, U1).
                    let op2 = if field == PstateField::DaifSet { 6 } else { 7 };
                    let iss = 3 << 17 | op2 << 14 | 4 << 10 | 31 << 5 | u32::from(immediate) << 1;
                    return Err(Exception::SystemTrap(iss));
                }
                if field == PstateField::DaifSet {
                    self.state.daif |= immediate;
                } else {
                    self.state.daif &= !immediate;
                }
            }
        }
        Ok(())
    }

    /// `SYS` (or `SYSL` when `read`) of the operation op1:CRn:CRm:op2 with
    /// the register `rt`: a cache, TLB or address translation operation.
    /// The inner `Err`: the exception that it takes.
    fn operation(
        &mut self,
        encoding: u16,
        rt: u8,
        read: bool,
    ) -> Result<Result<(), Exception>, Stop> {
        let el0 = self.state.context.el == 0;
        let sctlr = self.state.system.regs[reg::SCTLR_EL1];
        let value = match rt {
            31 => 0,
            n => self.state.x(usize::from(n)),
        };
        // op0 of a `SYS` is 1.
        let trapped = Exception::SystemTrap(sysreg::trap_iss(0x4000 | encoding, rt.into(), !read));
        let split = (
            encoding >> 11 & 7,
            encoding >> 7 & 15,
            encoding >> 3 & 15,
            encoding & 7,
        );
        let done = match split {
            _ if read => None,
            // IC IALLUIS and IALLU, DC IVAC, ISW, CSW and CISW: the caches
            // of this CPU hold nothing that an operation could drop. The
            // blocks follow the memory by their code stamps.
            (0, 7, 1 | 5, 0) | (0, 7, 6, 1 | 2) | (0, 7, 10 | 14, 2) if !el0 => Some(Ok(())),
            // IC IVAU, DC CVAC, CVAU and CIVAC: EL0 runs them with
            // SCTLR_EL1.UCI.
            (3, 7, 5, 1) | (3, 7, 10 | 11 | 14, 1) => Some(if el0 && sctlr & sctlr::UCI == 0 {
                Err(trapped)
            } else {
                Ok(())
            }),
            // DC ZVA: EL0 runs it with SCTLR_EL1.DZE.
            (3, 7, 4, 1) => Some(if el0 && sctlr & sctlr::DZE == 0 {
                Err(trapped)
            } else {
                match self.side.zero(&mut self.memory, value, el0) {
                    Ok(()) => Ok(()),
                    Err(Fault::Abort(fault, address)) => Err(Exception::DataAbort {
                        status: fault.status(),
                        write: true,
                        cache: false,
                        address,
                    }),
                    Err(Fault::Host(message)) => return Err(Stop::Host(message)),
                }
            }),
            // AT S1E1R, S1E1W, S1E0R and S1E0W: the result is in PAR_EL1.
            (0, 7, 8, op2 @ 0..=3) if !el0 => {
                let kind = if op2 & 1 == 0 {
                    Kind::Read
                } else {
                    Kind::Write
                };
                let par = match self.side.mmu.walk(&mut self.memory, value, false) {
                    Ok(page) if page.flags & mmu::needed(kind, op2 & 2 != 0) != 0 => {
                        mmu::par_of(&page)
                    }
                    Ok(page) => mmu::par_fault(mmu::Fault {
                        kind: mmu::FaultKind::Permission,
                        level: page.level,
                    }),
                    // A descriptor that the memory does not hold is an
                    // exception and no result: QEMU takes the abort.
                    Err(Miss::Fault(fault)) if fault.kind == mmu::FaultKind::ExternalOnWalk => {
                        return Ok(Err(Exception::DataAbort {
                            status: fault.status(),
                            // The abort of an address translation
                            // instruction says "write", also for a read.
                            write: true,
                            cache: true,
                            address: value,
                        }));
                    }
                    Err(Miss::Fault(fault)) => mmu::par_fault(fault),
                    Err(Miss::Host(message)) => return Err(Stop::Host(message)),
                };
                self.state.system.regs[reg::PAR_EL1] = par;
                Some(Ok(()))
            }
            // TLBI of the EL1&0 regime, on this CPU (CRm 7) or on all of
            // them (CRm 3, the Inner Shareable domain): the second kind goes
            // to the other CPUs of the machine too.
            (0, 8, crm @ (3 | 7), op2) if !el0 => {
                let asid = (value >> 48) as u16;
                let invalidation = match op2 {
                    // VMALLE1.
                    0 => Invalidation::All,
                    // VAE1 and VALE1: one address of one ASID.
                    1 | 5 => Invalidation::Page {
                        argument: value,
                        asid: Some(asid),
                    },
                    // ASIDE1.
                    2 => Invalidation::Asid(asid),
                    // VAAE1 and VAALE1: one address of every ASID.
                    3 | 7 => Invalidation::Page {
                        argument: value,
                        asid: None,
                    },
                    _ => return self.unknown_operation(encoding, read),
                };
                self.invalidate(invalidation);
                if crm == 3 {
                    self.broadcast(invalidation);
                }
                Some(Ok(()))
            }
            _ => None,
        };
        match done {
            Some(done) => Ok(done),
            None => self.unknown_operation(encoding, read),
        }
    }

    fn invalidate(&mut self, invalidation: Invalidation) {
        match invalidation {
            Invalidation::All => self.side.mmu.flush(),
            Invalidation::Page { argument, asid } => {
                self.side.mmu.flush_page(Regime::El10, argument, asid)
            }
            Invalidation::Asid(asid) => self.side.mmu.flush_asid(Regime::El10, asid),
        }
    }

    /// Hand `invalidation` to the other CPUs, and return when every one of
    /// them that runs has applied it (`shootdown.rs`).
    fn broadcast(&mut self, invalidation: Invalidation) {
        let Some((shootdown, id)) = self.shootdown.clone() else {
            return;
        };
        shootdown.post(id, invalidation);
        let mut spins = 0u32;
        while shootdown.outstanding(id) {
            // Mail of a CPU that waits on this one is taken here.
            self.take_mail();
            spins = spins.wrapping_add(1);
            if spins.is_multiple_of(64) {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
    }

    #[inline(always)]
    fn read_lines(&mut self) {
        if let Some(lines) = &self.lines {
            use std::sync::atomic::Ordering;
            self.irq = lines.irq.load(Ordering::Acquire);
            self.fiq = lines.fiq.load(Ordering::Acquire);
        }
    }

    /// Apply what the other CPUs invalidated.
    fn take_mail(&mut self) {
        let mail = match &self.shootdown {
            Some((shootdown, id)) => shootdown.take(*id),
            None => return,
        };
        for invalidation in mail.into_iter().flatten() {
            self.invalidate(invalidation);
        }
    }

    fn unknown_operation(
        &mut self,
        encoding: u16,
        read: bool,
    ) -> Result<Result<(), Exception>, Stop> {
        self.note(Unknown::Operation { encoding, read });
        Ok(Err(Exception::Undefined))
    }

    /// Run the last instruction of a block, at `pc`.
    fn trap(&mut self, pc: u64, trap: Trap) -> Result<(), Stop> {
        let el0 = self.state.context.el == 0;
        let next = pc.wrapping_add(4);
        let sctlr = self.state.system.regs[reg::SCTLR_EL1];
        let done = match trap {
            Trap::Exception(exception) => Err(exception),
            Trap::Unknown(word) => {
                if let Some(hook) = self.hooks.instruction
                    && hook(&mut self.state, word)
                {
                    Ok(())
                } else {
                    self.note(Unknown::Encoding(word));
                    Err(Exception::Undefined)
                }
            }
            // A call from EL0 to what is above EL1 does not exist.
            Trap::Hvc(_) | Trap::Smc(_) if el0 => Err(Exception::Undefined),
            Trap::Hvc(immediate) | Trap::Smc(immediate) => {
                self.state.set_pc(next);
                return Err(Stop::Hypercall {
                    conduit: if matches!(trap, Trap::Hvc(_)) {
                        Conduit::Hvc
                    } else {
                        Conduit::Smc
                    },
                    immediate,
                    pc,
                });
            }
            Trap::Eret if el0 => Err(Exception::Undefined),
            Trap::Eret => {
                exception::exception_return(&mut self.state);
                self.clear_exclusive();
                // A return is a point where the CPU sees new code.
                self.settle();
                return Ok(());
            }
            Trap::ClearExclusive => {
                self.clear_exclusive();
                Ok(())
            }
            Trap::Wfi if el0 && sctlr & sctlr::NTWI == 0 => Err(Exception::WaitTrap(false)),
            Trap::Wfi => {
                self.state.set_pc(next);
                // A line that is high wakes the CPU, masked or not.
                return if self.irq || self.fiq {
                    Ok(())
                } else {
                    Err(Stop::Halt)
                };
            }
            Trap::Wfe if el0 && sctlr & sctlr::NTWE == 0 => Err(Exception::WaitTrap(true)),
            Trap::Wfe => Ok(()),
            Trap::Mrs { encoding, rt } => self.register(encoding, rt, false),
            Trap::Msr { encoding, rt } => self.register(encoding, rt, true),
            Trap::MsrPstate { field, immediate } => self.pstate(field, immediate),
            Trap::Sys { encoding, rt, read } => self.operation(encoding, rt, read)?,
        };
        match done {
            Ok(()) => self.state.set_pc(next),
            Err(exception) => self.raise(exception, pc),
        }
        Ok(())
    }

    /// The physical address of the instruction at the PC of the state, for
    /// its execution at the level of the state.
    #[inline(always)]
    fn code_address(&mut self) -> Result<u64, Refused> {
        let pc = self.state.pc();
        let need = mmu::needed(Kind::Execute, self.state.context.el == 0);
        // An entry of the TLB is kept under the address with its tag, and
        // a PC with a tag may have another translation than a data address
        // with that tag: the slow path decides.
        if pc & 3 == 0
            && mmu::canonical(pc)
            && let Some((phys, _)) = self.side.mmu.hit(pc, 4, need)
        {
            return Ok(phys);
        }
        self.code_address_slow()
    }

    #[cold]
    fn code_address_slow(&mut self) -> Result<u64, Refused> {
        let pc = self.state.pc();
        if pc & 3 != 0 {
            return Err(Refused::Exception(Exception::PcAlignment));
        }
        let user = self.state.context.el == 0;
        match self
            .side
            .mmu
            .translate(&mut self.memory, pc, Kind::Execute, user)
        {
            Ok((phys, _)) => Ok(phys),
            Err(Miss::Fault(fault)) => Err(Refused::Exception(Exception::InstructionAbort(
                fault.status(),
            ))),
            Err(Miss::Host(message)) => Err(Refused::Stop(Stop::Host(message))),
        }
    }

    /// Translate the block at the PC of the state, whose code is at the
    /// physical address `phys`, of at most `limit` instructions.
    fn translate(&mut self, phys: u64, limit: u32) -> Result<Compiled, Refused> {
        let pc = self.state.pc();
        let context = self.state.context;
        let mut builder = Builder::new(context, pc);
        let mut at = pc;
        let mut count = 0;
        let mut ldp_x = 0;
        let mut stp_x = 0;
        loop {
            let mut bytes = [0u8; 4];
            // A block stays inside one page, so one translation serves it.
            match self
                .memory
                .read(phys.wrapping_add(at.wrapping_sub(pc)), &mut bytes)
            {
                Ok(()) => {}
                Err(Error::Memory(_)) if count == 0 => {
                    // The memory of the machine holds nothing there.
                    return Err(Refused::Exception(Exception::InstructionAbort(
                        exception::EXTERNAL,
                    )));
                }
                // The block ends before the instruction. It is looked at
                // again as the first of the next block.
                Err(Error::Memory(_)) => break,
                Err(error) => return Err(Refused::Stop(Stop::Host(error.to_string()))),
            }
            let word = u32::from_le_bytes(bytes);
            builder.begin(at);
            let lifted = match decode(word) {
                Some(decoded) => {
                    let lifted = lift(&mut builder, &decoded);
                    if self.block_counts.is_some() && !matches!(lifted, Lifted::Unsupported) {
                        match decoded.record {
                            Record::LDPXi | Record::LDPXpost | Record::LDPXpre => ldp_x += 1,
                            Record::STPXi | Record::STPXpost | Record::STPXpre => stp_x += 1,
                            _ => {}
                        }
                    }
                    lifted
                }
                // `SMC` needs EL3 in the LLVM tables, so no selected record
                // matches it. The machine serves it as it serves `HVC`.
                None if word & 0xffe0_001f == 0xd400_0003 => {
                    builder.trap(Trap::Smc((word >> 5) as u16));
                    Lifted::Branch
                }
                None => {
                    builder.trap(Trap::Unknown(word));
                    Lifted::Branch
                }
            };
            match lifted {
                Lifted::Next => {
                    count += 1;
                    at = at.wrapping_add(4);
                }
                Lifted::Branch => {
                    count += 1;
                    at = at.wrapping_add(4);
                    break;
                }
                Lifted::Unsupported if count == 0 => {
                    let record = decode(word).map(|decoded| decoded.record);
                    return Err(Refused::Stop(match record {
                        Some(record) => Stop::Unimplemented { pc, record },
                        None => host("a word without a record has no lifter"),
                    }));
                }
                // The block ends before the instruction.
                Lifted::Unsupported => {
                    builder.unbegin();
                    break;
                }
            }
            // A block stays inside one page, so that the page of its code
            // names it for a later invalidation.
            if count >= limit || at & 0xfff == 0 {
                break;
            }
        }
        self.translations += 1;
        self.side.mark_code(phys);
        let mut compiled = compile(
            phys,
            context,
            count,
            builder.finish(at),
            self.block_counts.as_mut(),
        )
        .map_err(Refused::Stop)?;
        if let Some(counts) = &mut self.block_counts {
            counts.ldp_x += ldp_x;
            counts.stp_x += stp_x;
        }
        compiled.stamp = self.memory.code_stamp(phys, count.max(1) as usize * 4);
        compiled.epoch = self.epoch;
        Ok(compiled)
    }

    /// Bring the caches up to date with the memory and with the system
    /// registers, at the start of `run` and `step`: the owner of the CPU can
    /// have changed either.
    fn check_generation(&mut self) {
        self.side.mmu.sync(&self.state);
        self.settle();
    }

    /// Account for the writes to the memory since the last check. The
    /// stores of this CPU moved the write generation of the memory by their
    /// count. When one of them went into a page of translated code, or the
    /// generation moved by more than that count, a new epoch starts: a block
    /// of an older epoch is used again only when the code stamp of the
    /// memory for its bytes still holds. A memory whose generation does not
    /// count writes one by one starts an epoch here every time, which is
    /// slow and correct.
    fn settle(&mut self) {
        let expected = self.generation.wrapping_add(self.side.own_writes);
        let generation = self.memory.generation();
        if self.side.code_written || generation != expected {
            self.epoch += 1;
        }
        self.generation = generation;
        self.side.own_writes = 0;
        self.side.code_written = false;
    }

    /// Whether the block of the cache at `index` is the one for the code at
    /// `phys` in `context`, and holds the code that the memory holds.
    #[inline(always)]
    fn usable(&mut self, single: bool, index: usize, phys: u64, context: Context) -> bool {
        let cache = if single {
            &mut self.singles
        } else {
            &mut self.blocks
        };
        let Some(block) = cache.entries[index].as_mut() else {
            return false;
        };
        if block.phys != phys || block.context != context {
            return false;
        }
        if block.epoch == self.epoch {
            return true;
        }
        // A write reached the memory since the block was last looked at.
        let stamp = self
            .memory
            .code_stamp(block.phys, block.instructions.max(1) as usize * 4);
        if stamp.is_some() && stamp == block.stamp {
            block.epoch = self.epoch;
            true
        } else {
            false
        }
    }

    /// Take what does not wait for an instruction: an interrupt whose line
    /// is high and not masked, then the illegal state. `true`: the CPU is at
    /// a vector now.
    fn asynchronous(&mut self) -> bool {
        let pc = self.state.pc();
        if let Some(interrupt) = exception::pending(&self.state, self.irq, self.fiq) {
            self.take(Cause::Interrupt(interrupt), pc);
            return true;
        }
        if self.state.il {
            self.raise(Exception::IllegalState, pc);
            return true;
        }
        false
    }

    /// Run the block at the PC: one instruction when `single`, else a whole
    /// block, with the last instruction that the loop runs itself. An
    /// interrupt, the illegal state and a fetch that fails come before the
    /// block. `Err`: no block could be made.
    fn run_block(&mut self, single: bool) -> Result<(u32, Option<Halt>), Stop> {
        if (self.irq || self.fiq || self.state.il) && self.asynchronous() {
            return Ok((0, Some(Halt::Taken)));
        }
        let pc = self.state.pc();
        let context = self.state.context;
        let phys = match self.code_address() {
            Ok(phys) => phys,
            Err(Refused::Stop(stop)) => return Err(stop),
            Err(Refused::Exception(exception)) => {
                self.raise(exception, pc);
                return Ok((0, Some(Halt::Taken)));
            }
        };
        let index = Cache::index(phys);
        let mut ready = self.usable(single, index, phys, context);
        if !ready {
            let cache = if single {
                &mut self.singles
            } else {
                &mut self.blocks
            };
            ready = cache.revive(index, phys, context) && self.usable(single, index, phys, context);
        }
        if !ready {
            let limit = if single { 1 } else { BLOCK_INSTRUCTIONS };
            let started = std::time::Instant::now();
            let translated = self.translate(phys, limit);
            self.translation_ns += started.elapsed().as_nanos() as u64;
            let compiled = match translated {
                Ok(compiled) => Box::new(compiled),
                Err(Refused::Stop(stop)) => return Err(stop),
                Err(Refused::Exception(exception)) => {
                    self.raise(exception, pc);
                    return Ok((0, Some(Halt::Taken)));
                }
            };
            let cache = if single {
                &mut self.singles
            } else {
                &mut self.blocks
            };
            cache.put(index, compiled);
        }
        let cache = if single {
            &mut self.singles
        } else {
            &mut self.blocks
        };
        let Some(block) = cache.entries[index].as_mut() else {
            return Err(host("the block cache lost a block"));
        };
        let (count, halt) = execute(
            block,
            &mut self.state,
            &mut self.memory,
            &mut self.side,
            &mut self.jit,
            &mut self.jit_counts,
            u64::MAX,
        );
        // The loop runs the last instruction at the PC that the block left.
        let terminal = block.terminal.map(|(_, trap)| (self.state.pc(), trap));
        Ok((count, self.finish(halt, terminal)))
    }

    /// Run the last instruction of a block that ran to its end.
    fn finish(&mut self, halt: Option<Halt>, terminal: Option<(u64, Trap)>) -> Option<Halt> {
        match (halt, terminal) {
            (None, Some((pc, trap))) => self.trap(pc, trap).err().map(Halt::Stop),
            (Some(Halt::Raise(exception)), _) => {
                let pc = self.state.pc();
                self.raise(exception, pc);
                Some(Halt::Taken)
            }
            (halt, _) => halt,
        }
    }

    /// Run exactly one instruction, or take the interrupt or the exception
    /// that comes before it. `taken` says which.
    pub fn step(&mut self) -> Result<(), Stop> {
        self.taken = None;
        self.take_mail();
        self.read_lines();
        self.check_generation();
        let result = self.run_block(true);
        self.settle();
        let (count, halt) = result?;
        self.instructions += u64::from(count);
        match halt {
            None | Some(Halt::Taken) => Ok(()),
            Some(Halt::Stop(stop)) => Err(stop),
            Some(Halt::Retry | Halt::Raise(_)) => Err(host(
                "a block of one instruction left in a way that it cannot",
            )),
        }
    }

    /// Run up to `budget` instructions and say why the loop stopped.
    pub fn run(&mut self, budget: u64) -> Stop {
        if let Some((shootdown, id)) = &self.shootdown {
            shootdown.set_running(*id, true);
        }
        self.check_generation();
        let stop = self.run_inner(budget);
        self.settle();
        if let Some((shootdown, id)) = &self.shootdown {
            shootdown.set_running(*id, false);
        }
        // Mail posted after this is taken before the next block.
        stop
    }

    fn run_inner(&mut self, budget: u64) -> Stop {
        let mut left = budget;
        // A machine CPU leaves the loop when a timer line is due to rise,
        // so that its owner raises the interrupt then and not at the end of
        // the budget: a guest that spins until the interrupt comes waited
        // a whole budget for each (kvm-unit-tests `timer`, card C13). The
        // clock is read every `CLOCK_BLOCKS` blocks, not before each.
        let deadline = if self.lines.is_some() {
            self.timer_deadline()
        } else {
            None
        };
        let mut blocks = 0u32;
        while left > 0 {
            if let Some(deadline) = deadline {
                blocks += 1;
                if blocks.is_multiple_of(CLOCK_BLOCKS) && self.clock.count() >= deadline {
                    break;
                }
            }
            // Another CPU invalidated translations: no block starts before
            // this CPU has dropped them too.
            self.take_mail();
            self.read_lines();
            let context = self.state.context;
            let quiet = !(self.irq || self.fiq || self.state.il);
            let fetched = if quiet {
                self.code_address().ok()
            } else {
                None
            };
            let index = fetched.map_or(0, Cache::index);
            // The common case: the TLB holds the page of the code, and the
            // block is translated and fits the budget.
            let outcome = if let Some(phys) = fetched
                && self.usable(false, index, phys, context)
                && let Some(block) = self.blocks.entries[index].as_mut()
                && u64::from(block.instructions) <= left
            {
                let (count, halt) = execute(
                    block,
                    &mut self.state,
                    &mut self.memory,
                    &mut self.side,
                    &mut self.jit,
                    &mut self.jit_counts,
                    left,
                );
                let terminal = block.terminal.map(|(_, trap)| (self.state.pc(), trap));
                (count, self.finish(halt, terminal))
            } else {
                // A whole block where the budget allows one, else one
                // instruction. A block that is not translated yet can be as
                // long as `BLOCK_INSTRUCTIONS`.
                let cached = self.blocks.entries[index]
                    .as_ref()
                    .filter(|block| Some(block.phys) == fetched && block.context == context)
                    .map(|block| u64::from(block.instructions));
                let single = match cached {
                    Some(length) => length > left,
                    None => left < u64::from(BLOCK_INSTRUCTIONS),
                };
                match self.run_block(single) {
                    Ok(outcome) => outcome,
                    Err(stop) => return stop,
                }
            };
            let (count, halt) = outcome;
            self.instructions += u64::from(count);
            left = left.saturating_sub(u64::from(count));
            match halt {
                None => {}
                Some(Halt::Taken) => left = left.saturating_sub(1),
                Some(Halt::Stop(stop)) => return stop,
                Some(Halt::Raise(_)) => return host("an exception that the loop did not take"),
                Some(Halt::Retry) => {
                    // The store goes into a page of translated code. The
                    // registers are those before its instruction: run it
                    // alone, and then drop the blocks.
                    if left == 0 {
                        break;
                    }
                    match self.run_block(true) {
                        Ok((count, halt)) => {
                            self.instructions += u64::from(count);
                            left = left.saturating_sub(u64::from(count));
                            match halt {
                                Some(Halt::Stop(stop)) => return stop,
                                Some(Halt::Taken) => left = left.saturating_sub(1),
                                _ => {}
                            }
                            // The store is made: the blocks of its page
                            // are looked at again.
                            self.settle();
                        }
                        Err(stop) => return stop,
                    }
                }
            }
            if self.side.code_written {
                self.settle();
            }
            if self.timer_written {
                self.timer_written = false;
                if self.lines.is_some() {
                    break;
                }
            }
        }
        Stop::Budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags;
    use vmlab_hv_tcg::FlatMemory;

    const BASE: u64 = 0x4000_0000;

    fn cpu(words: &[u32]) -> Cpu<FlatMemory> {
        let mut data = vec![0u8; 0x2000];
        for (index, word) in words.iter().enumerate() {
            data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        Cpu::new(FlatMemory::new(BASE, data), State::reset(BASE))
    }

    // add x0, x0, #1 ; subs x1, x1, #1 ; b.ne -8
    const LOOP: [u32; 3] = [0x9100_0400, 0xf100_0421, 0x54ff_ffc1];

    #[test]
    fn a_counted_loop_runs_to_its_end_with_the_flags_of_the_last_compare() {
        let mut cpu = cpu(&LOOP);
        cpu.state.set_x(1, 1000);
        assert_eq!(cpu.run(3000), Stop::Budget);
        assert_eq!(cpu.taken, None);
        assert_eq!(cpu.state.x(0), 1000);
        assert_eq!(cpu.state.x(1), 0);
        assert_eq!(cpu.state.pc(), BASE + 12);
        // 1 - 1: zero, and no borrow, so Z and C.
        assert_eq!(cpu.state.nzcv(), 0b0110);
        assert_eq!(cpu.instructions, 3000);
        // One block of three instructions, translated once.
        assert_eq!(cpu.translations, 1);
    }

    #[test]
    fn block_counts_measure_rollback_work_and_pairs_without_changing_execution() {
        // add x4, x4, #1 ; ldp x0, x1, [x2] ; stp x0, x1, [x3] ;
        // movz x4, #0 ; b .
        let words = [
            0x9100_0484,
            0xa940_0440,
            0xa900_0460,
            0xd280_0004,
            0x1400_0000,
        ];
        let mut plain = cpu(&words);
        let mut counted = cpu(&words);
        counted.block_counts = Some(BlockCounts::default());
        for cpu in [&mut plain, &mut counted] {
            cpu.state.set_x(2, BASE + 0x1000);
            cpu.state.set_x(3, BASE + 0x1010);
            cpu.memory.write(BASE + 0x1000, &[0x5a; 16]).unwrap();
            let (completed, halt) = cpu.run_block(false).unwrap();
            assert_eq!(completed, 5);
            assert!(halt.is_none());
            let mut copied = [0; 16];
            cpu.memory.read(BASE + 0x1010, &mut copied).unwrap();
            assert_eq!(copied, [0x5a; 16]);
        }
        assert_eq!(counted.state.slots, plain.state.slots);
        let counts = counted.block_counts.unwrap();
        assert_eq!(counts.blocks, 1);
        assert_eq!((counts.ldp_x, counts.stp_x), (1, 1));
        assert_eq!(counts.alignment_operations, 6);
        // The discarded add restores X4. Each access also has its own fault PC.
        assert_eq!(counts.exit_writeback_operations, 3);
    }

    #[test]
    fn a_machine_cpu_leaves_its_loop_when_its_timer_is_due() {
        // The virtual timer is due 20 us from now on a 1 GHz clock, and the
        // budget takes some milliseconds. A machine CPU leaves its loop
        // within `CLOCK_BLOCKS` blocks of the deadline, so that its owner
        // raises the line. A CPU without lines runs its budget.
        let budget = 3_000_000;
        for machine in [true, false] {
            let mut cpu = cpu(&LOOP);
            cpu.state.set_x(1, budget);
            cpu.clock = Clock::host(0, 1_000_000_000);
            let timer = &mut cpu.state.system.timers[1];
            timer.compare = cpu.clock.count() + 20_000;
            timer.set_control(1);
            if machine {
                cpu.lines = Some(std::sync::Arc::new(Lines::default()));
            }
            assert_eq!(cpu.run(budget), Stop::Budget);
            if machine {
                assert!(cpu.instructions < budget / 10, "{}", cpu.instructions);
            } else {
                assert_eq!(cpu.instructions, budget);
            }
        }
    }

    #[test]
    fn a_replaced_state_runs_on_caches_that_are_built_again() {
        let mut cpu = cpu(&LOOP);
        cpu.state.set_x(1, 1000);
        assert_eq!(cpu.run(300), Stop::Budget);
        assert!(
            cpu.blocks.entries.iter().any(Option::is_some)
                || cpu.singles.entries.iter().any(Option::is_some)
        );
        let translations = cpu.translations;
        let mut state = State::reset(BASE);
        state.set_x(1, 5);
        cpu.replace_state(state);
        // No block and no translation of the state before is kept.
        assert_eq!(cpu.tlb_entries(), 0);
        assert!(cpu.blocks.entries.iter().all(Option::is_none));
        assert!(cpu.singles.entries.iter().all(Option::is_none));
        assert_eq!(cpu.state.x(0), 0);
        assert_eq!(cpu.run(15), Stop::Budget);
        assert_eq!(cpu.state.x(0), 5);
        assert!(cpu.translations > translations);
    }

    #[test]
    fn a_budget_stops_between_instructions_and_stepping_agrees_with_running() {
        let mut run = cpu(&LOOP);
        run.state.set_x(1, 50);
        let mut step = cpu(&LOOP);
        step.state.set_x(1, 50);
        for budget in [1u64, 2, 3, 4, 7, 64, 1] {
            assert_eq!(run.run(budget), Stop::Budget);
            for _ in 0..budget {
                step.step().unwrap();
            }
            assert_eq!(run.state, step.state, "after a budget of {budget}");
            assert_eq!(run.instructions, step.instructions);
        }
    }

    /// The vector table of the tests: `VBAR_EL1` points at the second page.
    const VECTORS: u64 = BASE + 0x1000;

    fn with_vectors(words: &[u32]) -> Cpu<FlatMemory> {
        let mut cpu = cpu(words);
        cpu.state.system.regs[reg::VBAR_EL1] = VECTORS;
        cpu
    }

    fn sync(exception: Exception, from: u64, to: u64) -> Option<Taken> {
        Some(Taken {
            cause: Cause::Exception(exception),
            from,
            to,
        })
    }

    #[test]
    fn what_cannot_run_is_an_exception_of_the_guest() {
        // Outside the memory: an instruction abort, with the PC in FAR_EL1.
        let mut cpu = with_vectors(&[]);
        cpu.state.set_pc(0x1000);
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(
                Exception::InstructionAbort(exception::EXTERNAL),
                0x1000,
                VECTORS + 0x200
            )
        );
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x8600_0010);
        assert_eq!(cpu.state.system.regs[reg::FAR_EL1], 0x1000);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], 0x1000);
        // A PC above the 40 bits of a physical address: an address size fault.
        cpu.state.set_pc(1 << 40);
        cpu.step().unwrap();
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x8600_0000);
        assert_eq!(cpu.state.system.regs[reg::FAR_EL1], 1 << 40);
        // Not a multiple of four.
        cpu.state.set_pc(BASE + 2);
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::PcAlignment, BASE + 2, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x8a00_0000);
        assert_eq!(cpu.state.system.regs[reg::FAR_EL1], BASE + 2);
        // An encoding that no record of the selected sets matches: an add
        // with the immediate shift 2, which tables/constraints.toml refuses.
        // The block ends in it, and the log of the CPU names it once.
        let mut cpu = with_vectors(&[0x9100_0400, 0x9180_0400]);
        assert_eq!(cpu.run(2), Stop::Budget);
        assert_eq!(
            cpu.taken,
            sync(Exception::Undefined, BASE + 4, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.x(0), 1);
        assert_eq!(cpu.instructions, 2);
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x0200_0000);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE + 4);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x3c5);
        assert_eq!(
            cpu.unknown.iter().copied().collect::<Vec<_>>(),
            vec![Unknown::Encoding(0x9180_0400)]
        );
        assert_eq!(
            Unknown::Encoding(0x9180_0400).to_string(),
            "undefined instruction 91800400"
        );
        // UDF is undefined and is no unknown encoding.
        let mut cpu = with_vectors(&[0]);
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x200));
        assert!(cpu.unknown.is_empty());
        // A guest whose vector is not in the memory takes one abort after the
        // other, and the budget still ends the run.
        let mut cpu = self::cpu(&[0]);
        assert_eq!(cpu.run(10), Stop::Budget);
        assert_eq!(cpu.exceptions, 10);
        assert_eq!(cpu.state.pc(), 0x200);
    }

    // The words of the system tests.
    const SVC_7: u32 = 0xd400_00e1;
    const HVC_0: u32 = 0xd400_0002;
    const SMC_5: u32 = 0xd400_00a3;
    const ERET: u32 = 0xd69f_03e0;
    const WFI: u32 = 0xd503_207f;
    const WFE: u32 = 0xd503_205f;
    const NOP: u32 = 0xd503_201f;

    fn mrs(rt: u32, name: &str) -> u32 {
        0xd520_0000 | u32::from(sysreg::ROWS[sysreg::index(name)].encoding) << 5 | rt
    }

    fn msr(name: &str, rt: u32) -> u32 {
        0xd500_0000 | u32::from(sysreg::ROWS[sysreg::index(name)].encoding) << 5 | rt
    }

    /// Put `words` at the vector that EL0 takes its synchronous exceptions
    /// to, and at the one of EL1 with SP_EL1.
    fn handler(cpu: &mut Cpu<FlatMemory>, words: &[u32]) {
        for offset in [0x200u64, 0x400] {
            for (index, word) in words.iter().enumerate() {
                cpu.memory
                    .write(VECTORS + offset + 4 * index as u64, &word.to_le_bytes())
                    .unwrap();
            }
        }
    }

    #[test]
    fn a_system_call_from_el0_and_its_return() {
        // svc #7 ; add x0, x0, #1. The handler: add x1, x1, #1 ; eret.
        let mut cpu = with_vectors(&[SVC_7, 0x9100_0400]);
        handler(&mut cpu, &[0x9100_0421, ERET]);
        assert!(cpu.state.set_cpsr(0x2000_0000));
        cpu.state.slots[slot::SP_EL0] = 0x1000;
        cpu.state.slots[slot::SP_EL1] = 0x2000;
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::SupervisorCall(7), BASE, VECTORS + 0x400)
        );
        assert_eq!(cpu.state.cpsr(), 0x3c5);
        assert_eq!(cpu.state.sp(), 0x2000);
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x5600_0007);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE + 4);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x2000_0000);
        assert_eq!(cpu.run(3), Stop::Budget);
        assert_eq!((cpu.state.x(0), cpu.state.x(1)), (1, 1));
        assert_eq!(cpu.state.cpsr(), 0x2000_0000);
        assert_eq!(cpu.state.sp(), 0x1000);
        assert_eq!(cpu.state.pc(), BASE + 8);
        // ERET at EL0 is undefined.
        let mut cpu = with_vectors(&[ERET]);
        assert!(cpu.state.set_cpsr(0));
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x400));
        // A return to a state that does not exist leaves the level, and the
        // next instruction takes the Illegal Execution State exception.
        let mut cpu = with_vectors(&[ERET, NOP]);
        cpu.state.system.regs[reg::SPSR_EL1] = 0x3c9;
        cpu.state.system.regs[reg::ELR_EL1] = BASE + 4;
        cpu.step().unwrap();
        assert_eq!(cpu.taken, None);
        assert_eq!(cpu.state.cpsr(), 0x0010_03c5);
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::IllegalState, BASE + 4, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x3a00_0000);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x0010_03c5);
        assert_eq!(cpu.state.cpsr(), 0x3c5);
    }

    #[test]
    fn hypercalls_leave_the_core_with_their_conduit() {
        let mut cpu = with_vectors(&[HVC_0, SMC_5, NOP]);
        assert_eq!(
            cpu.run(10),
            Stop::Hypercall {
                conduit: Conduit::Hvc,
                immediate: 0,
                pc: BASE
            }
        );
        assert_eq!(cpu.state.pc(), BASE + 4);
        assert_eq!(cpu.taken, None);
        assert_eq!(
            cpu.step(),
            Err(Stop::Hypercall {
                conduit: Conduit::Smc,
                immediate: 5,
                pc: BASE + 4
            })
        );
        assert_eq!(cpu.state.pc(), BASE + 8);
        // A machine that does not serve the call refuses it.
        cpu.refuse_hypercall(BASE + 4);
        assert_eq!(
            cpu.taken,
            sync(Exception::Undefined, BASE + 4, VECTORS + 0x200)
        );
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE + 4);
        // From EL0 neither call exists.
        for word in [HVC_0, SMC_5] {
            let mut cpu = with_vectors(&[word]);
            assert!(cpu.state.set_cpsr(0));
            cpu.step().unwrap();
            assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x400));
        }
        // SMC is no unknown encoding.
        assert!(cpu.unknown.is_empty());
    }

    #[test]
    fn an_interrupt_line_is_taken_between_blocks_when_its_mask_is_clear() {
        let mut cpu = with_vectors(&LOOP);
        cpu.state.set_x(1, 1000);
        cpu.irq = true;
        // Masked at the reset: the loop runs.
        assert_eq!(cpu.run(30), Stop::Budget);
        assert_eq!(cpu.taken, None);
        // msr daifclr, #2 in the loop would unmask. Here the owner does.
        cpu.state.daif = 0b1101;
        let before = cpu.state.pc();
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            Some(Taken {
                cause: Cause::Interrupt(exception::Interrupt::Irq),
                from: before,
                to: VECTORS + 0x280
            })
        );
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], before);
        assert_eq!(cpu.state.daif, 0xf);
        // FIQ has its own mask and its own vector, from EL0 too.
        let mut cpu = with_vectors(&LOOP);
        assert!(cpu.state.set_cpsr(0x80));
        cpu.fiq = true;
        cpu.irq = true;
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(cpu.state.pc(), VECTORS + 0x500);
        assert_eq!(cpu.state.system.regs[reg::SPSR_EL1], 0x80);
        assert_eq!(cpu.instructions, 0);
    }

    #[test]
    fn wfi_halts_until_a_line_is_high_and_traps_from_el0() {
        let mut cpu = with_vectors(&[WFI, NOP, WFI, WFE, NOP]);
        assert_eq!(cpu.run(10), Stop::Halt);
        assert_eq!(cpu.state.pc(), BASE + 4);
        // A masked line still wakes the CPU: the second WFI runs through.
        cpu.irq = true;
        assert_eq!(cpu.run(4), Stop::Budget);
        assert_eq!(cpu.state.pc(), BASE + 20);
        assert_eq!(cpu.taken, None);
        // At EL0 with SCTLR_EL1.nTWI clear, WFI is trapped. WFE likewise.
        for (word, wfe, bit) in [(WFI, false, sctlr::NTWI), (WFE, true, sctlr::NTWE)] {
            let mut cpu = with_vectors(&[word, NOP]);
            assert!(cpu.state.set_cpsr(0x3c0));
            cpu.irq = true;
            cpu.state.system.regs[reg::SCTLR_EL1] &= !bit;
            cpu.step().unwrap();
            assert_eq!(
                cpu.taken,
                sync(Exception::WaitTrap(wfe), BASE, VECTORS + 0x400)
            );
            assert_eq!(
                cpu.state.system.regs[reg::ESR_EL1],
                0x07e0_0000 | u64::from(wfe)
            );
            // With the bit set, as it is after the reset, it runs.
            let mut cpu = with_vectors(&[word, NOP]);
            assert!(cpu.state.set_cpsr(0x3c0));
            cpu.irq = true;
            assert_ne!(cpu.state.system.regs[reg::SCTLR_EL1] & bit, 0);
            cpu.step().unwrap();
            assert_eq!(cpu.taken, None);
            assert_eq!(cpu.state.pc(), BASE + 4);
        }
    }

    #[test]
    fn the_timer_raises_its_line_and_the_interrupt_wakes_wfi() {
        // msr cntv_cval_el0, x0 ; msr cntv_ctl_el0, x1 ; mrs x2, cntv_ctl_el0 ;
        // mrs x3, cntvct_el0 ; wfi ; nop
        let mut cpu = with_vectors(&[
            msr("CNTV_CVAL_EL0", 0),
            msr("CNTV_CTL_EL0", 1),
            mrs(2, "CNTV_CTL_EL0"),
            mrs(3, "CNTVCT_EL0"),
            WFI,
            NOP,
        ]);
        cpu.clock = Clock::Manual(100);
        cpu.state.set_x(0, 500);
        cpu.state.set_x(1, 1);
        assert_eq!(cpu.run(10), Stop::Halt);
        assert_eq!((cpu.state.x(2), cpu.state.x(3)), (1, 100));
        assert_eq!(cpu.timer_lines(), timer::Lines::default());
        assert_eq!(cpu.timer_deadline(), Some(500));
        // The machine waits until the deadline and wires the line to IRQ.
        cpu.clock = Clock::Manual(500);
        assert!(cpu.timer_lines().virtual_);
        assert_eq!(cpu.timer_deadline(), None);
        cpu.irq = cpu.timer_lines().virtual_;
        cpu.state.daif = 0;
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken.map(|taken| (taken.from, taken.to)),
            Some((BASE + 20, VECTORS + 0x280))
        );
        // The handler masks the timer, and the line falls.
        cpu.state.system.timers[1].set_control(3);
        assert!(!cpu.timer_lines().virtual_);
    }

    #[test]
    fn an_fp_instruction_traps_while_cpacr_disables_fp() {
        // fadd d0, d1, d2 ; msr cpacr_el1, x0 ; fadd d0, d1, d2
        let mut cpu = with_vectors(&[0x1e62_2820, msr("CPACR_EL1", 0), 0x1e62_2820]);
        handler(
            &mut cpu,
            &[0x9100_0421, 0xd538_4031, 0x9100_1231, 0xd518_4031, ERET],
        );
        cpu.state.set_x(0, 3 << 20);
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::FpAccess, BASE, VECTORS + 0x200));
        assert_eq!(cpu.state.system.regs[reg::ESR_EL1], 0x1fe0_0000);
        assert_eq!(cpu.state.system.regs[reg::ELR_EL1], BASE);
        // The handler skips the instruction (mrs x17, elr_el1 ; add x17,
        // x17, #4 ; msr elr_el1, x17), the guest enables FP, and the block
        // that follows is lifted for the new context.
        cpu.state.set_v(1, u128::from(1.5f64.to_bits()));
        cpu.state.set_v(2, u128::from(2.25f64.to_bits()));
        assert_eq!(cpu.run(7), Stop::Budget);
        assert!(cpu.state.context.fp);
        assert_eq!(cpu.state.v(0), u128::from(3.75f64.to_bits()));
        assert_eq!(cpu.exceptions, 1);
    }

    #[test]
    fn a_read_of_a_stored_register_stays_in_its_block() {
        // mrs x0, tpidr_el1 ; add x0, x0, #1 ; mrs x1, tpidr_el0 ;
        // mrs x2, midr_el1 ; mrs xzr, tpidr_el1 ; mrs x3, cntvct_el0 ; b .
        let words = [
            mrs(0, "TPIDR_EL1"),
            0x9100_0400,
            mrs(1, "TPIDR_EL0"),
            mrs(2, "MIDR_EL1"),
            mrs(31, "TPIDR_EL1"),
            mrs(3, "CNTVCT_EL0"),
            0x1400_0000,
        ];
        let mut cpu = with_vectors(&words);
        cpu.state.system.regs[sysreg::index("TPIDR_EL1")] = 0x1000;
        cpu.state.system.regs[sysreg::index("TPIDR_EL0")] = 0x2000;
        cpu.clock = Clock::Manual(77);
        assert_eq!(cpu.run(64), Stop::Budget);
        // The counter is computed, so it ends the block. The five
        // instructions before it are one block with it, and the branch to
        // itself is the second block.
        assert_eq!(cpu.translations, 2);
        assert_eq!(
            (
                cpu.state.x(0),
                cpu.state.x(1),
                cpu.state.x(2),
                cpu.state.x(3)
            ),
            (0x1001, 0x2000, 0x000f_3d00, 77)
        );
        // Stepping gives the same, and a second run of the cached block
        // reads the words as they are then.
        let mut step = with_vectors(&words);
        step.state.system.regs[sysreg::index("TPIDR_EL1")] = 0x1000;
        step.state.system.regs[sysreg::index("TPIDR_EL0")] = 0x2000;
        step.clock = Clock::Manual(77);
        for _ in 0..7 {
            step.step().unwrap();
        }
        assert_eq!(step.state.slots[..31], cpu.state.slots[..31]);
        cpu.state.system.regs[sysreg::index("TPIDR_EL1")] = 0x5000;
        cpu.state.set_pc(BASE);
        assert_eq!(cpu.run(7), Stop::Budget);
        assert_eq!(cpu.state.x(0), 0x5001);
        assert_eq!(cpu.translations, 2);
        // At EL0 only what EL0 may read without a control bit stays inside:
        // TPIDR_EL1 is undefined there, and CTR_EL0 asks SCTLR_EL1.
        assert!(sysreg::reads_stored(sysreg::index("TPIDR_EL0"), 0));
        assert!(!sysreg::reads_stored(sysreg::index("TPIDR_EL1"), 0));
        assert!(!sysreg::reads_stored(sysreg::index("CTR_EL0"), 0));
        assert!(sysreg::reads_stored(sysreg::index("CTR_EL0"), 1));
        assert!(!sysreg::reads_stored(sysreg::index("OSLAR_EL1"), 1));
        let mut user = with_vectors(&[mrs(0, "TPIDR_EL1")]);
        assert!(user.state.set_cpsr(0x3c0));
        user.step().unwrap();
        assert_eq!(
            user.taken,
            sync(Exception::Undefined, BASE, VECTORS + 0x400)
        );
    }

    #[test]
    fn a_machine_adds_registers_and_instructions_through_the_hooks() {
        // S3_0_C15_C0_0: IMPLEMENTATION DEFINED, no row.
        const ENCODING: u16 = sysreg::encoding(3, 0, 15, 0, 0);
        fn register(state: &mut State, encoding: u16, write: Option<u64>) -> Option<u64> {
            if encoding != ENCODING {
                return None;
            }
            if let Some(value) = write {
                state.set_x(9, value + 1);
            }
            Some(0x1234)
        }
        fn instruction(state: &mut State, word: u32) -> bool {
            state.set_x(10, word.into());
            word == 0x0020_1234
        }
        let read = 0xd520_0000 | u32::from(ENCODING) << 5 | 3;
        let write = 0xd500_0000 | u32::from(ENCODING) << 5 | 4;
        let other = read + (1 << 5);
        let words = [read, write, 0x0020_1234, other, 0x0020_9999];
        let mut cpu = with_vectors(&words);
        cpu.hooks.register = Some(register);
        cpu.hooks.instruction = Some(instruction);
        cpu.state.set_x(4, 41);
        assert_eq!(cpu.run(3), Stop::Budget);
        assert_eq!(cpu.taken, None);
        assert_eq!((cpu.state.x(3), cpu.state.x(9)), (0x1234, 42));
        assert_eq!(cpu.state.x(10), 0x0020_1234);
        assert_eq!(cpu.state.pc(), BASE + 12);
        // What the machine does not claim is undefined and is in the log.
        cpu.step().unwrap();
        assert_eq!(
            cpu.taken,
            sync(Exception::Undefined, BASE + 12, VECTORS + 0x200)
        );
        cpu.state.set_pc(BASE + 16);
        cpu.step().unwrap();
        assert_eq!(
            cpu.unknown
                .iter()
                .map(Unknown::to_string)
                .collect::<Vec<_>>(),
            vec![
                "undefined instruction 00209999".to_string(),
                "read of the system register S3_0_C15_C0_1, which this CPU does not have"
                    .to_string(),
            ]
        );
        // Without hooks the first word is undefined too.
        let mut cpu = with_vectors(&words);
        cpu.step().unwrap();
        assert_eq!(cpu.taken, sync(Exception::Undefined, BASE, VECTORS + 0x200));
    }

    #[test]
    fn a_write_to_the_memory_drops_the_translated_blocks() {
        let mut cpu = cpu(&[0x9100_0400, 0x1400_0000]);
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(cpu.state.x(0), 1);
        // add x0, x0, #2 in place of add x0, x0, #1.
        cpu.memory
            .write(BASE, &0x9100_0800u32.to_le_bytes())
            .unwrap();
        cpu.state.set_pc(BASE);
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(cpu.state.x(0), 3);
    }

    #[test]
    fn scalar_fp_runs_through_the_helpers_and_collects_its_exceptions() {
        use crate::helpers::fp::fpsr;
        // fadd d0, d1, d2 ; fdiv d3, d1, d4 ; fcvtzs w5, d0 ; fcmp d1, d2
        let mut cpu = cpu(&[0x1e62_2820, 0x1e64_1823, 0x1e78_0005, 0x1e62_2020]);
        cpu.state.set_v(0, u128::MAX);
        cpu.state
            .set_v(1, u128::from(1.5f64.to_bits()) | 0xdead << 64);
        cpu.state.set_v(2, u128::from(2.25f64.to_bits()));
        cpu.state.set_v(3, u128::MAX);
        cpu.state.set_x(5, u64::MAX);
        cpu.state.enable_fp();
        assert_eq!(cpu.run(4), Stop::Budget);
        // A scalar result clears the register above it.
        assert_eq!(cpu.state.v(0), u128::from(3.75f64.to_bits()));
        assert_eq!(cpu.state.v(3), u128::from(f64::INFINITY.to_bits()));
        assert_eq!(cpu.state.x(5), 3);
        // 1.5 < 2.25: N.
        assert_eq!(cpu.state.nzcv(), 0b1000);
        // A division by zero, then an inexact conversion. The bits stay.
        assert_eq!(cpu.state.fpsr(), u32::from(fpsr::DZC | fpsr::IXC));
        // Stepping gives the same.
        let mut step = self::cpu(&[0x1e62_2820, 0x1e64_1823, 0x1e78_0005, 0x1e62_2020]);
        step.state.set_v(0, u128::MAX);
        step.state
            .set_v(1, u128::from(1.5f64.to_bits()) | 0xdead << 64);
        step.state.set_v(2, u128::from(2.25f64.to_bits()));
        step.state.set_v(3, u128::MAX);
        step.state.set_x(5, u64::MAX);
        step.state.enable_fp();
        for _ in 0..4 {
            step.step().unwrap();
        }
        assert_eq!(step.state, cpu.state);
    }

    #[test]
    fn the_rounding_mode_of_fpcr_reaches_the_helpers() {
        // fdiv s0, s1, s2: 1 / 3 in each of the four modes.
        for (mode, expected) in [
            (0u32, 0x3eaa_aaabu32),
            (1, 0x3eaa_aaab),
            (2, 0x3eaa_aaaa),
            (3, 0x3eaa_aaaa),
        ] {
            let mut cpu = cpu(&[0x1e22_1820]);
            cpu.state.set_v(1, u128::from(1.0f32.to_bits()));
            cpu.state.set_v(2, u128::from(3.0f32.to_bits()));
            cpu.state.set_fpcr(mode << 22);
            cpu.state.enable_fp();
            cpu.step().unwrap();
            assert_eq!(cpu.state.v(0), u128::from(expected), "mode {mode}");
            assert_eq!(cpu.state.fpsr(), 0x10);
            assert_eq!(cpu.state.fpcr(), mode << 22);
        }
    }

    fn bump(context: &mut dyn std::any::Any, args: &[u128], results: &mut [u128]) {
        let state = context
            .downcast_mut::<State>()
            .expect("the context of an aarch64 helper is the state");
        state.daif ^= 0xf;
        results[0] = args[0] + args[1];
    }

    static BUMP: vmlab_hv_tcg_ir::native::Helper = vmlab_hv_tcg_ir::native::Helper {
        name: "bump",
        args: 2,
        results: 1,
        effects: vmlab_hv_tcg_ir::native::Effects::WRITES_STATE,
        function: HelperFn::Ordered(bump),
    };

    #[test]
    fn an_ordered_helper_runs_once_at_its_place_with_the_state() {
        use crate::state::REGISTER_FILE;
        use vmlab_hv_tcg_ir::native::regfile::Writeback;
        use vmlab_hv_tcg_ir::native::{Binary, HelperRef};
        let ty = ScalarType::Bits(64);
        let mut block: BlockOf<A64Exit> = BlockOf::default();
        let x0 = REGISTER_FILE.read(&mut block.graph, slot::X0);
        let x1 = REGISTER_FILE.read(&mut block.graph, slot::X0 + 1);
        // x2 = bump(x0 + x1, x1), and nothing reads the result of a second call.
        let sum = block.graph.binary(ty, Binary::Add, x0, x1).unwrap();
        let first = block.graph.effect(0, ty);
        let second = block.graph.effect(1, ty);
        for (args, result) in [(vec![sum, x1], first), (vec![x0, x0], second)] {
            block.events.push(Event::Call {
                helper: HelperRef(&BUMP),
                args,
                results: vec![result],
            });
        }
        let next = block.graph.constant(ty, 0x4000_0004);
        let exit = block.exit(A64Exit {
            writeback: Writeback {
                writes: vec![(slot::X0 + 2, first)],
            },
            next: A64Next::Pc(next),
            completed: 1,
        });
        block.events.push(Event::Exit(exit));
        let context = State::reset(0).context;
        let mut compiled = compile(0x4000_0000, context, 1, block, None).unwrap();
        let mut state = State::reset(0x4000_0000);
        state.set_x(0, 5);
        state.set_x(1, 7);
        let mut memory = FlatMemory::new(0x4000_0000, vec![0; 16]);
        let mut side = Side::new(&state);
        let (count, halt) = compiled.run(&mut state, &mut memory, &mut side);
        assert!(count == 1 && halt.is_none());
        assert_eq!(state.x(2), 19);
        assert_eq!(state.pc(), 0x4000_0004);
        // Both calls ran: the masks were flipped twice.
        assert_eq!(state.daif, 0xf);
        // An ordered call of a pure helper is refused.
        let mut block: BlockOf<A64Exit> = BlockOf::default();
        let x0 = REGISTER_FILE.read(&mut block.graph, slot::X0);
        let result = block.graph.effect(0, ty);
        block.events.push(Event::Call {
            helper: HelperRef(&crate::helpers::fp::FP_UNARY),
            args: vec![x0],
            results: vec![result],
        });
        let next = block.graph.constant(ty, 0);
        let exit = block.exit(A64Exit {
            writeback: Writeback::default(),
            next: A64Next::Pc(next),
            completed: 1,
        });
        block.events.push(Event::Exit(exit));
        assert!(matches!(
            compile(0, context, 1, block, None),
            Err(Stop::Host(_))
        ));
    }

    #[test]
    fn a_block_is_one_page_at_most_and_sixty_four_instructions() {
        // Fill two pages with add x0, x0, #1.
        let words = vec![0x9100_0400u32; 0x800];
        let mut cpu = cpu(&words);
        assert_eq!(cpu.run(0x800), Stop::Budget);
        assert_eq!(cpu.state.x(0), 0x800);
        assert_eq!(cpu.translations, u64::from(0x800 / BLOCK_INSTRUCTIONS));
        assert_eq!(cpu.state.pc(), BASE + 0x2000);
        // The memory ends there: an instruction abort.
        assert_eq!(cpu.run(1), Stop::Budget);
        assert_eq!(
            cpu.taken.map(|taken| (taken.cause, taken.from)),
            Some((
                Cause::Exception(Exception::InstructionAbort(exception::EXTERNAL)),
                BASE + 0x2000
            ))
        );
    }

    /// The flags of `a + b + carry` in `width` bits, from the definitions of
    /// N, Z, C and V, as the bits 3 to 0.
    fn sum_flags(width: u32, a: u64, b: u64, carry: bool) -> u8 {
        let mask = if width == 64 {
            u64::MAX
        } else {
            u64::from(u32::MAX)
        };
        let (a, b) = (a & mask, b & mask);
        let result = a.wrapping_add(b).wrapping_add(u64::from(carry)) & mask;
        let sign = |value: u64| -> i128 {
            if width == 64 {
                i128::from(value as i64)
            } else {
                i128::from(value as u32 as i32)
            }
        };
        let n = result >> (width - 1) & 1 == 1;
        let c = u128::from(a) + u128::from(b) + u128::from(carry) != u128::from(result);
        let v = sign(a) + sign(b) + i128::from(carry) != sign(result);
        u8::from(n) << 3 | u8::from(result == 0) << 2 | u8::from(c) << 1 | u8::from(v)
    }

    #[test]
    fn every_condition_reads_the_lazy_flags_inside_and_after_their_block() {
        // msr nzcv, x12 ; <setter> ; csinc x2, xzr, xzr, cond ; b +4 ;
        // csinc x3, xzr, xzr, cond ; b . The first csinc reads the flags of the
        // block, the second the flag slots.
        const VALUES: [u64; 8] = [
            0,
            1,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_ffff,
            0x7fff_ffff_ffff_ffff,
            0x8000_0000_0000_0000,
            u64::MAX,
        ];
        let ccmp =
            |sf: u32, cond: u32, nzcv: u32| sf << 31 | 0x7a40_0000 | 1 << 16 | cond << 12 | nzcv;
        let mut case = 0u32;
        for width in [32u32, 64] {
            let sf = u32::from(width == 64);
            // (encoding with rd 9, rn 0 and rm 1, its flags).
            type Setter = (u32, fn(u32, u64, u64, u8) -> u8);
            let setters: [Setter; 6] = [
                (sf << 31 | 0x6b00_0009 | 1 << 16, |w, a, b, _| {
                    sum_flags(w, a, !b, true)
                }),
                (sf << 31 | 0x2b00_0009 | 1 << 16, |w, a, b, _| {
                    sum_flags(w, a, b, false)
                }),
                (sf << 31 | 0x6a00_0009 | 1 << 16, |w, a, b, _| {
                    let mask = if w == 64 {
                        u64::MAX
                    } else {
                        u64::from(u32::MAX)
                    };
                    let result = a & b & mask;
                    u8::from(result >> (w - 1) & 1 == 1) << 3 | u8::from(result == 0) << 2
                }),
                (sf << 31 | 0x3a00_0009 | 1 << 16, |w, a, b, before| {
                    sum_flags(w, a, b, before & 2 != 0)
                }),
                (sf << 31 | 0x7a00_0009 | 1 << 16, |w, a, b, before| {
                    sum_flags(w, a, !b, before & 2 != 0)
                }),
                (0, |_, _, _, _| 0),
            ];
            for (index, (encoding, flags)) in setters.iter().enumerate() {
                for &a in &VALUES {
                    for &b in &VALUES {
                        for cond in 0..16u32 {
                            case += 1;
                            let before = (case % 16) as u8;
                            // The last setter is a ccmp on a condition of
                            // the flags before it.
                            let (encoding, expected) = if index == 5 {
                                let on = case / 16 % 16;
                                let given = case / 7 % 16;
                                let expected = if flags::holds(on as u8, before) {
                                    sum_flags(width, a, !b, true)
                                } else {
                                    given as u8
                                };
                                (ccmp(sf, on, given), expected)
                            } else {
                                (*encoding, flags(width, a, b, before))
                            };
                            let csinc = |rd: u32| 0x9a9f_07e0 | cond << 12 | rd;
                            let mut cpu = cpu(&[
                                0xd51b_420c,
                                encoding,
                                csinc(2),
                                0x1400_0001,
                                csinc(3),
                                0x1400_0000,
                            ]);
                            cpu.state.set_x(0, a);
                            cpu.state.set_x(1, b);
                            cpu.state.set_x(12, u64::from(before) << 28);
                            assert_eq!(cpu.run(6), Stop::Budget);
                            let what = format!(
                                "{encoding:#010x} {a:#x} {b:#x} cond {cond} flags {before:04b}"
                            );
                            assert_eq!(cpu.state.nzcv(), expected, "{what}");
                            let holds = u64::from(!flags::holds(cond as u8, expected));
                            assert_eq!(cpu.state.x(2), holds, "in the block: {what}");
                            assert_eq!(cpu.state.x(3), holds, "after the block: {what}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn mrs_nzcv_reads_the_lazy_flags() {
        // subs x9, x0, x1 ; mrs x2, nzcv ; b +4 ; mrs x3, nzcv
        let mut cpu = cpu(&[0xeb01_0009, 0xd53b_4202, 0x1400_0001, 0xd53b_4203]);
        cpu.state.set_x(0, 1);
        cpu.state.set_x(1, 2);
        assert_eq!(cpu.run(4), Stop::Budget);
        assert_eq!(cpu.state.x(2), 0x8000_0000);
        assert_eq!(cpu.state.x(3), 0x8000_0000);
    }
}
